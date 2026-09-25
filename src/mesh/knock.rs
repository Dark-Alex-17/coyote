//! The knock flow: a known identity asking, from an instance nobody has trusted yet, to be
//! let in. Inbound, the dispatcher's `KnockSink` feeds a bounded queue that a drain task
//! hands to the `KnockGate`, which decides once more against the trust list, rate-limits
//! per identity, files the knock in the cache and surfaces one line per identity per
//! session. Outbound, a knock tries the peer over a link first and only falls back to an
//! LXMF propagation node when the peer cannot be reached; the fetch path routes such a
//! stored knock back into the same gate.

use crate::mesh::announce::MAX_DISPLAY_NAME_BYTES;
use crate::mesh::idle::{IdleNotify, Origin};
use crate::mesh::knocks::{KNOCK_INTRO_MAX_CHARS, KNOCK_RECORD_VERSION, KnockCache, KnockRecord};
use crate::mesh::notify::Source;
use crate::mesh::peers::PeerTable;
use crate::mesh::propagation::{OutboundMessage, PropagationError};
use crate::mesh::propagation_fetch::{InboundMessage, InboundSink};
use crate::mesh::r3::{
    DEFAULT_LINK_TIMEOUT, KnockEvent, KnockSink, NAME_HASH_LEN, OriginName, R3Error, describe_path,
    short,
};
use crate::mesh::trust::{Decision, IdentityStanding, Rule, TrustStore};
use crate::mesh::{destination_address, display_text, rfc3339_utc};

use lxmf_core::constants::{FIELD_CUSTOM_DATA, FIELD_CUSTOM_TYPE};
use parking_lot::Mutex;
use rmpv::Value;
use rns_transport::hash::AddressHash;
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

/// The LXMF custom type a stored knock carries in `FIELD_CUSTOM_TYPE`, so a fetch can tell
/// a knock from a message before it reads anything else. Versioned in the name: a later
/// layout gets a new type, and a node that does not know it treats the payload as a
/// message.
pub(crate) const KNOCK_TYPE: &str = "coyote.knock/1";
/// Ceiling on the direct attempt. The dispatcher answers a knock at once, so a peer that
/// is up replies well inside this; a longer wait only delays the fallback.
pub(crate) const KNOCK_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Ceiling on opening the link for the direct attempt, the r3 default.
pub(crate) const KNOCK_LINK_TIMEOUT: Duration = DEFAULT_LINK_TIMEOUT;
/// Depth of the queue between the dispatcher and the gate. The dispatcher must never wait
/// on the gate, so past this many unprocessed knocks the newest are counted and dropped.
pub(crate) const KNOCK_QUEUE_CAPACITY: usize = 64;
/// Identities the gate keeps per-identity state for. Known identities are the only ones
/// that get this far, so the cap is a backstop against a trust list of thousands, not
/// against an attacker minting identities. An identity evicted from the map is new again
/// when it returns, with a full bucket and one more line to spend; getting there takes 256
/// other trust-listed identities knocking in between, which is acceptable.
pub(crate) const KNOCK_GATE_MAX_IDENTITIES: usize = 256;
/// Knocks one identity may land back to back before the rest are dropped.
pub(crate) const KNOCK_BUCKET_BURST: u32 = 3;
/// One token returns to an identity's bucket per interval. Knocking is a human act asking
/// for a human decision; a few per hour is plenty.
pub(crate) const KNOCK_BUCKET_REFILL_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// The knocker's text, cleaned and within `KNOCK_INTRO_MAX_CHARS`. Empty is allowed: a
/// knock needs no words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KnockIntro(String);

// Reached by the REPL mesh commands once they land.
#[allow(dead_code)]
impl KnockIntro {
    /// Cleans `text` as the receiver will and refuses it when what is left is still over
    /// the cap: the sender is told rather than having its words cut silently.
    pub(crate) fn new(text: &str) -> Result<Self, KnockError> {
        let cleaned = display_text(text, usize::MAX).unwrap_or_default();
        let chars = cleaned.chars().count();
        if chars > KNOCK_INTRO_MAX_CHARS {
            return Err(KnockError::IntroTooLong {
                chars,
                max: KNOCK_INTRO_MAX_CHARS,
            });
        }
        Ok(Self(cleaned))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn to_r3_body(&self) -> Value {
        Value::Map(vec![(Value::from("intro"), Value::from(self.0.as_str()))])
    }
}

/// The intro out of a `/knock` body. Only a map with a UTF-8 string under `intro` yields
/// one; the receiver truncates rather than refuses, since the knock counts either way.
pub(crate) fn intro_from_r3_body(body: Option<&Value>) -> Option<String> {
    let entries = body?.as_map()?;
    let intro = entries
        .iter()
        .find(|(key, _)| key.as_str() == Some("intro"))
        .and_then(|(_, value)| value.as_str())?;
    display_text(intro, KNOCK_INTRO_MAX_CHARS)
}

/// A knock as a propagation node stores it: the intro as the content, the type and this
/// node's origin name in the custom fields. The recipient recomputes the knocking
/// destination from that name and the signer's identity, exactly as the dispatcher does
/// for a link request, so a knock can only ever name one of the knocker's own instances.
pub(crate) fn knock_message(intro: &KnockIntro, origin: &OriginName) -> OutboundMessage {
    OutboundMessage {
        title: None,
        content: intro.0.as_bytes().to_vec(),
        fields: Some(Value::Map(vec![
            (Value::from(FIELD_CUSTOM_TYPE), Value::from(KNOCK_TYPE)),
            (
                Value::from(FIELD_CUSTOM_DATA),
                Value::Map(vec![(
                    Value::from("name_hash"),
                    Value::Binary(origin.0.to_vec()),
                )]),
            ),
        ])),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KnockMessage {
    NotAKnock,
    /// Typed as a knock but not laid out as one; never a message either.
    Malformed(&'static str),
    Knock {
        name_hash: [u8; NAME_HASH_LEN],
        intro: Option<String>,
    },
}

/// Reads a fetched message as a knock. The custom type is accepted as a string or as its
/// UTF-8 bytes, since msgpack encoders differ on which they emit.
pub(crate) fn decode_knock_message(message: &InboundMessage) -> KnockMessage {
    let Some(Value::Map(fields)) = &message.fields else {
        return KnockMessage::NotAKnock;
    };
    let field = |key: u8| {
        fields
            .iter()
            .find(|(field, _)| field.as_u64() == Some(u64::from(key)))
            .map(|(_, value)| value)
    };
    let Some(kind) = field(FIELD_CUSTOM_TYPE) else {
        return KnockMessage::NotAKnock;
    };
    let is_knock = match kind {
        Value::String(text) => text.as_str() == Some(KNOCK_TYPE),
        Value::Binary(bytes) => bytes == KNOCK_TYPE.as_bytes(),
        _ => false,
    };
    if !is_knock {
        return KnockMessage::NotAKnock;
    }
    let Some(Value::Map(data)) = field(FIELD_CUSTOM_DATA) else {
        return KnockMessage::Malformed("custom data is missing or not a map");
    };
    let name_hash = data
        .iter()
        .find(|(key, _)| key.as_str() == Some("name_hash"))
        .map(|(_, value)| value);
    let Some(Value::Binary(bytes)) = name_hash else {
        return KnockMessage::Malformed("name_hash is missing or not binary");
    };
    let Ok(name_hash) = <[u8; NAME_HASH_LEN]>::try_from(bytes.as_slice()) else {
        return KnockMessage::Malformed("name_hash is not 10 bytes");
    };
    let intro = message
        .content
        .as_deref()
        .and_then(|bytes| display_text(&String::from_utf8_lossy(bytes), KNOCK_INTRO_MAX_CHARS));
    KnockMessage::Knock { name_hash, intro }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KnockVia {
    Direct,
    StoreAndForward,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KnockOutcome {
    pub via: KnockVia,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KnockError {
    IntroTooLong {
        chars: usize,
        max: usize,
    },
    NotRunning,
    /// The direct attempt failed for a reason that is not the peer being unreachable, so
    /// nothing was stored for it.
    Direct(R3Error),
    NoPropagationNode,
    Propagation(PropagationError),
}

impl fmt::Display for KnockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IntroTooLong { chars, max } => write!(
                f,
                "The knock intro is {chars} characters, above the {max}-character limit"
            ),
            Self::NotRunning => write!(
                f,
                "The mesh node has been stopped; run `.mesh on` to start it again"
            ),
            Self::Direct(err) => write!(f, "The knock could not be sent: {err}"),
            Self::NoPropagationNode => write!(
                f,
                "The peer is unreachable and no propagation node is known yet to hold the knock for it. Run `.mesh peers` to see which nodes this Coyote has heard from."
            ),
            Self::Propagation(err) => {
                write!(
                    f,
                    "The peer is unreachable and the knock could not be stored: {err}"
                )
            }
        }
    }
}

impl std::error::Error for KnockError {}

/// The dispatcher's knock sink: a bounded channel the drain task reads. `knock` runs on the
/// server's request path, so it never waits; a full queue counts the knock and drops it.
/// Each knock is noted in the debug log as it is queued, since the gate's verdict lands
/// later and off this path.
pub(crate) struct ChannelKnockSink {
    tx: Sender<KnockEvent>,
    overflow: AtomicU64,
}

impl ChannelKnockSink {
    pub(crate) fn new(capacity: usize) -> (Arc<Self>, Receiver<KnockEvent>) {
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);
        (
            Arc::new(Self {
                tx,
                overflow: AtomicU64::new(0),
            }),
            rx,
        )
    }

    /// Knocks dropped because the queue was full.
    pub(crate) fn overflow(&self) -> u64 {
        self.overflow.load(Ordering::Relaxed)
    }
}

impl KnockSink for ChannelKnockSink {
    fn knock(&self, knock: KnockEvent) {
        let id8 = short(&knock.identity_hash).to_string();
        let dest8 = short(&knock.destination_hash).to_string();
        let link = knock.link_id.to_hex_string();
        let path = describe_path(knock.path_hash);
        let intro = if knock.data.is_some() {
            " with an introduction"
        } else {
            ""
        };
        match self.tx.try_send(knock) {
            Ok(()) => {
                debug!(
                    "Mesh knock from {id8} for destination {dest8} on link {link} via {path}{intro}"
                );
            }
            Err(TrySendError::Full(_)) => {
                let dropped = self.overflow.fetch_add(1, Ordering::Relaxed) + 1;
                debug!(
                    "Mesh knock queue is full ({dropped} dropped so far); dropped the knock from {id8} for destination {dest8} on link {link} via {path}"
                );
            }
            Err(TrySendError::Closed(_)) => {
                debug!(
                    "Mesh knock from {id8} for destination {dest8} dropped: the gate has stopped"
                );
            }
        }
    }
}

/// Where the gate puts the one line a knock earns. Held weakly by the gate: the slot owns
/// the runtime that owns the gate. `surface` runs off the request path with no lock held
/// and must not block; `false` means the line was dropped, and the gate then offers the
/// identity's next knock instead.
pub(crate) trait KnockSurface: Send + Sync {
    fn surface(&self, note: IdleNotify) -> bool;
}

/// A knock as either transport hands it to the gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InboundKnock {
    pub identity_hash: String,
    pub destination_hash: String,
    pub via: KnockVia,
    pub intro: Option<String>,
}

/// What the gate did with a knock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    Blocked,
    Unknown,
    Denied,
    AlreadyTrusted,
    RateLimited,
    Admitted { surfaced: bool },
}

struct IdentityState {
    last_seen: Instant,
    surfaced: bool,
    tokens: u32,
    last_refill: Instant,
}

impl IdentityState {
    fn new(now: Instant) -> Self {
        Self {
            last_seen: now,
            surfaced: false,
            tokens: KNOCK_BUCKET_BURST,
            last_refill: now,
        }
    }

    /// Spends one token if the bucket has one, crediting whole refill intervals first.
    fn admit(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last_refill);
        let intervals = elapsed.as_nanos() / KNOCK_BUCKET_REFILL_INTERVAL.as_nanos();
        if intervals > 0 {
            let intervals = u32::try_from(intervals).unwrap_or(u32::MAX);
            self.tokens = self
                .tokens
                .saturating_add(intervals)
                .min(KNOCK_BUCKET_BURST);
            self.last_refill = self
                .last_refill
                .checked_add(KNOCK_BUCKET_REFILL_INTERVAL.saturating_mul(intervals))
                .unwrap_or(now);
        }
        self.last_seen = now;
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }
}

#[derive(Default)]
struct GateState {
    identities: HashMap<String, IdentityState>,
}

impl GateState {
    /// The state for `identity`, made fresh if absent; the least recently seen identity
    /// makes room when the map is at its cap.
    fn entry(&mut self, identity: &str, now: Instant) -> &mut IdentityState {
        if !self.identities.contains_key(identity)
            && self.identities.len() >= KNOCK_GATE_MAX_IDENTITIES
        {
            let stalest = self
                .identities
                .iter()
                .min_by_key(|(_, state)| state.last_seen)
                .map(|(key, _)| key.clone())
                .expect("a map at its cap is not empty");
            self.identities.remove(&stalest);
        }
        self.identities
            .entry(identity.to_string())
            .or_insert_with(|| IdentityState::new(now))
    }
}

/// The trust list's answer to a knock, applied once more where knocks land: the
/// dispatcher already refused the request, but a fetched knock arrives with no dispatcher
/// in front of it, and a block may have landed since. Then a per-identity bucket, the
/// cache, and one terminal line per identity per session.
pub(crate) struct KnockGate {
    trust: Arc<TrustStore>,
    peers: Arc<PeerTable>,
    cache: KnockCache,
    surface: Mutex<Option<Weak<dyn KnockSurface>>>,
    state: Mutex<GateState>,
}

impl KnockGate {
    pub(crate) fn new(trust: Arc<TrustStore>, peers: Arc<PeerTable>, cache: KnockCache) -> Self {
        Self {
            trust,
            peers,
            cache,
            surface: Mutex::new(None),
            state: Mutex::new(GateState::default()),
        }
    }

    /// A knock admitted before this is called is cached but not surfaced, and not marked
    /// as surfaced either, so the identity's next knock still earns its one line.
    pub(crate) fn attach(&self, surface: Weak<dyn KnockSurface>) {
        *self.surface.lock() = Some(surface);
    }

    #[cfg(test)]
    pub(crate) fn cache(&self) -> &KnockCache {
        &self.cache
    }

    /// Decides one knock. Every refusal returns before the gate's own state is touched, so
    /// an identity the list does not admit leaves no trace here.
    pub(crate) fn admit(
        &self,
        knock: InboundKnock,
        now: Instant,
        received_at: SystemTime,
    ) -> Admission {
        let id8 = short(&knock.identity_hash);
        let dest8 = short(&knock.destination_hash);
        if self.trust.is_blocked_identity(&knock.identity_hash) {
            return Admission::Blocked;
        }
        if self.trust.identity_standing(&knock.identity_hash) == IdentityStanding::Unknown {
            return Admission::Unknown;
        }
        let verdict = self
            .trust
            .authorize(&knock.identity_hash, &knock.destination_hash);
        match (verdict.decision, verdict.rule) {
            (Decision::Refuse, Rule::DefaultClosed) => {}
            (Decision::Allow, _) => {
                debug!(
                    "Mesh knock from {id8} via {:?} is not a knock: already trusted from {dest8}",
                    knock.via
                );
                return Admission::AlreadyTrusted;
            }
            (Decision::Refuse, Rule::IdentityBlocked) => return Admission::Blocked,
            (Decision::Refuse, _) => {
                debug!("Mesh knock from {id8} for destination {dest8} dropped: destination denied");
                return Admission::Denied;
            }
        }
        if !self
            .state
            .lock()
            .entry(&knock.identity_hash, now)
            .admit(now)
        {
            debug!("Mesh knock from {id8} for destination {dest8} dropped: rate limited");
            return Admission::RateLimited;
        }
        let (display_name, hops) = self.knocker_name_and_hops(&knock);
        let record = KnockRecord {
            version: KNOCK_RECORD_VERSION,
            received_at: rfc3339_utc(received_at),
            identity_hash: knock.identity_hash.clone(),
            destination_hash: knock.destination_hash.clone(),
            display_name: display_name.clone(),
            intro: knock.intro.clone(),
            hops,
        };
        if let Err(err) = self.cache.append(record, received_at) {
            warn!("Mesh knock from {id8} for destination {dest8} was not cached: {err:#}");
        }
        let Some(surface) = self.surface.lock().as_ref().and_then(Weak::upgrade) else {
            return Admission::Admitted { surfaced: false };
        };
        {
            let mut state = self.state.lock();
            let entry = state.entry(&knock.identity_hash, now);
            if entry.surfaced {
                return Admission::Admitted { surfaced: false };
            }
            entry.surfaced = true;
        }
        let shown = surface.surface(IdleNotify {
            source: Source::Knock,
            origin: Origin::Peer(id8.to_string()),
            text: knock_text(&knock, display_name.as_deref()),
            model_note: None,
        });
        if !shown {
            self.state.lock().entry(&knock.identity_hash, now).surfaced = false;
        }
        Admission::Admitted { surfaced: shown }
    }

    /// The knocking instance's own peer record names it and says how far it is; when only
    /// another instance of the identity has announced, that record lends its name and the
    /// distance is unknown. The name is cleaned and cut to the announce byte cap so the
    /// cache can never refuse it.
    fn knocker_name_and_hops(&self, knock: &InboundKnock) -> (Option<String>, u8) {
        let peers = self.peers.snapshot();
        let (name, hops) = match peers
            .iter()
            .find(|peer| peer.destination_hash == knock.destination_hash)
        {
            Some(peer) => (peer.display_name.as_deref(), peer.hops),
            None => (
                peers
                    .iter()
                    .filter(|peer| peer.identity_hash == knock.identity_hash)
                    .max_by_key(|peer| peer.last_seen)
                    .and_then(|peer| peer.display_name.as_deref()),
                0,
            ),
        };
        let name = name
            .and_then(|name| display_text(name, MAX_DISPLAY_NAME_BYTES))
            .map(|name| {
                let mut cut = MAX_DISPLAY_NAME_BYTES.min(name.len());
                while !name.is_char_boundary(cut) {
                    cut -= 1;
                }
                name[..cut].trim_end().to_string()
            });
        (name, hops)
    }

    #[cfg(test)]
    pub(crate) fn tracked_identities(&self) -> Vec<String> {
        self.state.lock().identities.keys().cloned().collect()
    }
}

/// Two lines: who knocks from where, then the three things the user can do about it.
/// `.mesh trust` and `.mesh info` take the destination, since trust is granted to an
/// instance and the peer table is keyed by one; `.mesh block` takes the identity, since a
/// block silences every instance of it. Full hashes, because the user pastes them. The
/// name and the intro are quoted and any quote inside them becomes an apostrophe, so peer
/// text cannot close its own quotes and pose as the frame.
fn knock_text(knock: &InboundKnock, display_name: Option<&str>) -> String {
    let who = display_name.map_or_else(
        || "A peer".to_string(),
        |name| format!("\"{}\"", name.replace('"', "'")),
    );
    let intro = knock
        .intro
        .as_deref()
        .map(|intro| format!(": \"{}\"", intro.replace('"', "'")))
        .unwrap_or_default();
    format!(
        "{who} (identity {identity}) knocks from instance {destination}{intro}\ntrust: .mesh trust {destination} | who: .mesh info {destination} | silence: .mesh block {identity}",
        identity = knock.identity_hash,
        destination = knock.destination_hash,
    )
}

/// Feeds the dispatcher's knocks to the gate until the node stops. `admit` rewrites the
/// knock cache under a file lock, so each call runs on a blocking thread rather than a
/// worker; the LXMF fetch path calls `admit` inline because `InboundSink::deliver` is
/// synchronous.
pub(crate) async fn drain_knocks(
    mut rx: Receiver<KnockEvent>,
    gate: Arc<KnockGate>,
    cancel: CancellationToken,
) {
    loop {
        let event = tokio::select! {
            () = cancel.cancelled() => return,
            event = rx.recv() => match event {
                Some(event) => event,
                None => return,
            },
        };
        let id8 = short(&event.identity_hash).to_string();
        let knock = InboundKnock {
            identity_hash: event.identity_hash,
            destination_hash: event.destination_hash,
            via: KnockVia::Direct,
            intro: intro_from_r3_body(event.data.as_ref()),
        };
        let gate = Arc::clone(&gate);
        let admitted = tokio::task::spawn_blocking(move || {
            gate.admit(knock, Instant::now(), SystemTime::now());
        })
        .await;
        if let Err(err) = admitted {
            warn!("Mesh knock gate task for {id8} did not finish: {err}");
        }
    }
}

/// An `InboundSink` in front of another: fetched knocks go to the gate, everything else to
/// `inner`. A payload typed as a knock is never a message, so a malformed one is dropped
/// rather than forwarded.
pub(crate) struct KnockRouting<'a> {
    pub gate: &'a KnockGate,
    pub inner: &'a dyn InboundSink,
}

impl InboundSink for KnockRouting<'_> {
    fn deliver(&self, message: InboundMessage) {
        let id8 = short(&message.source_identity_hash);
        match decode_knock_message(&message) {
            KnockMessage::NotAKnock => self.inner.deliver(message),
            KnockMessage::Malformed(why) => {
                debug!("Propagated knock from {id8} dropped: {why}");
            }
            KnockMessage::Knock { name_hash, intro } => {
                let Ok(identity) = AddressHash::new_from_hex_string(&message.source_identity_hash)
                else {
                    debug!("Propagated knock from {id8} dropped: the signer's hash is malformed");
                    return;
                };
                self.gate.admit(
                    InboundKnock {
                        identity_hash: message.source_identity_hash.clone(),
                        destination_hash: destination_address(&name_hash, &identity)
                            .to_hex_string(),
                        via: KnockVia::StoreAndForward,
                        intro,
                    },
                    Instant::now(),
                    SystemTime::now(),
                );
            }
        }
    }
}

/// Records every line the gate surfaces.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingSurface {
    notes: Mutex<Vec<IdleNotify>>,
}

#[cfg(test)]
impl RecordingSurface {
    pub(crate) fn texts(&self) -> Vec<String> {
        self.notes
            .lock()
            .iter()
            .map(|note| note.text.clone())
            .collect()
    }
}

#[cfg(test)]
impl KnockSurface for RecordingSurface {
    fn surface(&self, note: IdleNotify) -> bool {
        self.notes.lock().push(note);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::hex_lower;
    use crate::mesh::notify::{NOTIFICATION_LINE_MAX_CHARS, Notification};
    use crate::mesh::peers::PeerSighting;
    use crate::mesh::r3::{PathHash, STATUS_PATH};
    use crate::mesh::test_support::{TempDir, TrustList};
    use crate::testing::{debug_snapshot, install_log_collector};

    use rand_core::OsRng;
    use rns_transport::destination::link::LinkId;
    use std::io::Cursor;

    /// A gate over `list` with a fresh peer table and cache, surfacing into the returned
    /// recorder.
    struct Rig {
        gate: KnockGate,
        surface: Arc<RecordingSurface>,
        peers: Arc<PeerTable>,
        _tmp: TempDir,
    }

    impl Rig {
        fn new(tag: &str, list: TrustList) -> Self {
            let (trust, tmp) = list.open(tag);
            let peers =
                Arc::new(PeerTable::load(tmp.path.join("peers.json"), SystemTime::now()).unwrap());
            let gate = KnockGate::new(trust, peers.clone(), KnockCache::new(&tmp.path, 24));
            let surface = Arc::new(RecordingSurface::default());
            gate.attach(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
            Self {
                gate,
                surface,
                peers,
                _tmp: tmp,
            }
        }

        fn cached(&self) -> Vec<KnockRecord> {
            self.gate.cache().list(SystemTime::now()).unwrap()
        }
    }

    fn hash_of(seed: &str) -> String {
        let mut bytes = [0u8; 16];
        for (slot, byte) in bytes.iter_mut().zip(seed.bytes()) {
            *slot = byte;
        }
        hex_lower(&bytes)
    }

    fn knock_from(identity: &str, destination: &str, via: KnockVia) -> InboundKnock {
        InboundKnock {
            identity_hash: identity.to_string(),
            destination_hash: destination.to_string(),
            via,
            intro: Some("hello".to_string()),
        }
    }

    fn now() -> Instant {
        Instant::now()
    }

    fn inbound(fields: Option<Value>, content: Option<Vec<u8>>, source: &str) -> InboundMessage {
        InboundMessage {
            transient_id: [1u8; 32],
            message_id: [2u8; 32],
            source_identity_hash: source.to_string(),
            source_delivery_hash: hash_of("delivery"),
            timestamp: 1_700_000_000.0,
            title: None,
            content,
            fields,
            stamp_value: None,
        }
    }

    #[test]
    fn intro_is_refused_over_the_cap_and_cleaned_under_it() {
        let err = KnockIntro::new(&"x".repeat(KNOCK_INTRO_MAX_CHARS + 1)).unwrap_err();
        assert_eq!(
            err,
            KnockError::IntroTooLong {
                chars: 201,
                max: 200
            }
        );
        assert!(err.to_string().contains("201"), "{err}");

        let at_cap = KnockIntro::new(&"\u{e9}".repeat(KNOCK_INTRO_MAX_CHARS)).unwrap();
        assert_eq!(at_cap.as_str().chars().count(), KNOCK_INTRO_MAX_CHARS);
        assert_eq!(KnockIntro::new("  \u{1b}[31m  ").unwrap().as_str(), "");
        assert_eq!(
            KnockIntro::new(" hi\u{202E} there\r\n").unwrap().as_str(),
            "hi there"
        );
        let cleaned_under_cap = format!(
            "{}{}",
            "\u{200B}".repeat(50),
            "y".repeat(KNOCK_INTRO_MAX_CHARS)
        );
        assert_eq!(
            KnockIntro::new(&cleaned_under_cap).unwrap().as_str(),
            "y".repeat(KNOCK_INTRO_MAX_CHARS)
        );
    }

    #[test]
    fn r3_body_round_trips_and_the_receiver_truncates_on_a_char_boundary() {
        let intro = KnockIntro::new("bonjour").unwrap();
        assert_eq!(
            intro_from_r3_body(Some(&intro.to_r3_body())).as_deref(),
            Some("bonjour")
        );

        let long = Value::Map(vec![(
            Value::from("intro"),
            Value::from("\u{e9}\u{1f600}".repeat(150)),
        )]);
        let received = intro_from_r3_body(Some(&long)).unwrap();
        assert_eq!(received.chars().count(), KNOCK_INTRO_MAX_CHARS);
        assert!(received.starts_with("\u{e9}\u{1f600}"));

        for body in [
            None,
            Some(Value::Nil),
            Some(Value::from("intro")),
            Some(Value::Map(vec![(Value::from("intro"), Value::from(7))])),
            Some(Value::Map(vec![(Value::from("intro"), Value::from(""))])),
            Some(Value::Map(vec![(Value::from("other"), Value::from("x"))])),
        ] {
            assert_eq!(intro_from_r3_body(body.as_ref()), None, "{body:?}");
        }
    }

    #[test]
    fn lxmf_knock_round_trips_and_names_the_senders_own_instance() {
        let origin = OriginName([7u8; NAME_HASH_LEN]);
        let intro = KnockIntro::new("let me in").unwrap();
        let message = knock_message(&intro, &origin);
        assert_eq!(message.title, None);

        let decoded = decode_knock_message(&inbound(
            message.fields.clone(),
            Some(message.content.clone()),
            &hash_of("signer"),
        ));
        assert_eq!(
            decoded,
            KnockMessage::Knock {
                name_hash: origin.0,
                intro: Some("let me in".to_string()),
            }
        );

        let mut packed = Vec::new();
        rmpv::encode::write_value(&mut packed, message.fields.as_ref().unwrap()).unwrap();
        let unpacked = rmpv::decode::read_value(&mut Cursor::new(packed)).unwrap();
        let through_the_wire = decode_knock_message(&inbound(
            Some(unpacked),
            Some(message.content),
            &hash_of("signer"),
        ));
        assert_eq!(through_the_wire, decoded);
    }

    #[test]
    fn lxmf_decoding_tells_messages_from_malformed_knocks_and_caps_the_intro() {
        let typed = |kind: Value| Value::Map(vec![(Value::from(FIELD_CUSTOM_TYPE), kind)]);
        for fields in [
            None,
            Some(Value::Map(vec![])),
            Some(Value::from("fields")),
            Some(typed(Value::from("coyote.other/1"))),
            Some(typed(Value::from(3))),
            Some(Value::Map(vec![(
                Value::from(FIELD_CUSTOM_DATA),
                Value::Map(vec![]),
            )])),
        ] {
            assert_eq!(
                decode_knock_message(&inbound(fields.clone(), None, &hash_of("s"))),
                KnockMessage::NotAKnock,
                "{fields:?}"
            );
        }

        let with_data = |data: Value| {
            Value::Map(vec![
                (
                    Value::from(FIELD_CUSTOM_TYPE),
                    Value::Binary(KNOCK_TYPE.as_bytes().to_vec()),
                ),
                (Value::from(FIELD_CUSTOM_DATA), data),
            ])
        };
        for (data, why) in [
            (Value::Nil, "custom data is missing or not a map"),
            (Value::Map(vec![]), "name_hash is missing or not binary"),
            (
                Value::Map(vec![(Value::from("name_hash"), Value::from("text"))]),
                "name_hash is missing or not binary",
            ),
            (
                Value::Map(vec![(Value::from("name_hash"), Value::Binary(vec![1; 9]))]),
                "name_hash is not 10 bytes",
            ),
        ] {
            assert_eq!(
                decode_knock_message(&inbound(Some(with_data(data)), None, &hash_of("s"))),
                KnockMessage::Malformed(why)
            );
        }
        assert_eq!(
            decode_knock_message(&inbound(
                Some(typed(Value::from("coyote.knock/1"))),
                None,
                &hash_of("s")
            )),
            KnockMessage::Malformed("custom data is missing or not a map")
        );

        let good = with_data(Value::Map(vec![(
            Value::from("name_hash"),
            Value::Binary(vec![3; NAME_HASH_LEN]),
        )]));
        let long = "\u{4e2d}\u{1f600}".repeat(150);
        let KnockMessage::Knock { name_hash, intro } = decode_knock_message(&inbound(
            Some(good.clone()),
            Some(long.into_bytes()),
            &hash_of("s"),
        )) else {
            panic!("a well-formed knock");
        };
        assert_eq!(name_hash, [3; NAME_HASH_LEN]);
        let intro = intro.unwrap();
        assert_eq!(intro.chars().count(), KNOCK_INTRO_MAX_CHARS);
        assert!(intro.starts_with("\u{4e2d}\u{1f600}"));

        let hostile = format!(
            "\u{1b}[31mEVIL\u{1b}[0m\r\nline2\u{202E}{}",
            "x".repeat(10_000)
        );
        let KnockMessage::Knock { intro, .. } = decode_knock_message(&inbound(
            Some(good.clone()),
            Some(hostile.into_bytes()),
            &hash_of("s"),
        )) else {
            panic!("a well-formed knock");
        };
        let intro = intro.unwrap();
        assert!(intro.starts_with("EVIL  line2x"), "{intro:?}");
        assert!(!intro.chars().any(|c| c.is_control() || c == '\u{202E}'));
        assert_eq!(intro.chars().count(), KNOCK_INTRO_MAX_CHARS);

        for content in [None, Some(Vec::new()), Some(b"  \x1b[2J \n".to_vec())] {
            assert_eq!(
                decode_knock_message(&inbound(Some(good.clone()), content.clone(), &hash_of("s"))),
                KnockMessage::Knock {
                    name_hash: [3; NAME_HASH_LEN],
                    intro: None,
                },
                "{content:?}"
            );
        }
        assert_eq!(
            decode_knock_message(&inbound(
                Some(good),
                Some(vec![0xff, b'h', b'i']),
                &hash_of("s")
            )),
            KnockMessage::Knock {
                name_hash: [3; NAME_HASH_LEN],
                intro: Some("\u{FFFD}hi".to_string()),
            },
            "invalid UTF-8 is read lossily, never refused"
        );
    }

    #[test]
    fn a_known_identity_from_an_untrusted_instance_is_admitted_surfaced_once_and_cached() {
        let identity = hash_of("id-a");
        let rig = Rig::new(
            "knock-gate-admit",
            TrustList::default().identity(&identity, false),
        );
        rig.peers.observe(
            PeerSighting {
                destination_hash: hash_of("inst-a1"),
                identity_hash: identity.clone(),
                name_hash: "00".repeat(10),
                display_name: Some("Bea\u{200B}trice".to_string()),
                protocol_version: 1,
                hops: 4,
            },
            SystemTime::now(),
        );

        let first = rig.gate.admit(
            knock_from(&identity, &hash_of("inst-a1"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );
        assert_eq!(first, Admission::Admitted { surfaced: true });
        let second = rig.gate.admit(
            InboundKnock {
                intro: None,
                ..knock_from(&identity, &hash_of("inst-a2"), KnockVia::StoreAndForward)
            },
            now(),
            SystemTime::now(),
        );
        assert_eq!(second, Admission::Admitted { surfaced: false });

        let notes = rig.surface.notes.lock();
        assert_eq!(notes.len(), 1, "one line per identity per session");
        assert_eq!(notes[0].source, Source::Knock);
        assert_eq!(notes[0].origin, Origin::Peer(identity[..8].to_string()));
        assert!(
            notes[0].model_note.is_none(),
            "knocks are for the human only"
        );
        let text = &notes[0].text;
        let lines: Vec<&str> = text.split('\n').collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(
            lines[0],
            format!(
                "\"Beatrice\" (identity {identity}) knocks from instance {}: \"hello\"",
                hash_of("inst-a1")
            )
        );
        assert_eq!(
            lines[1],
            format!(
                "trust: .mesh trust {dest} | who: .mesh info {dest} | silence: .mesh block {identity}",
                dest = hash_of("inst-a1")
            )
        );
        assert!(!text.to_ascii_lowercase().contains("deny"));
        drop(notes);

        let cached = rig.cached();
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].destination_hash, hash_of("inst-a2"));
        assert_eq!(cached[0].intro, None);
        assert_eq!(cached[1].destination_hash, hash_of("inst-a1"));
        assert_eq!(cached[1].intro.as_deref(), Some("hello"));
        assert_eq!(cached[1].display_name.as_deref(), Some("Beatrice"));
        assert_eq!(cached[1].hops, 4);
        assert_eq!(cached[1].version, KNOCK_RECORD_VERSION);
    }

    #[test]
    fn the_surfaced_text_names_every_remedy_with_full_hashes_and_stays_under_the_line_cap() {
        let identity = hash_of("id-hint");
        let rig = Rig::new(
            "knock-gate-hint",
            TrustList::default().identity(&identity, false),
        );
        rig.peers.observe(
            PeerSighting {
                destination_hash: hash_of("inst-hint"),
                identity_hash: identity.clone(),
                name_hash: "00".repeat(10),
                display_name: Some("n".repeat(MAX_DISPLAY_NAME_BYTES)),
                protocol_version: 1,
                hops: 1,
            },
            SystemTime::now(),
        );
        let hostile = format!(
            "\u{1b}[31mEVIL\u{1b}[0m\r\nline2\u{202E}{}",
            "x".repeat(10_000)
        );

        let admission = rig.gate.admit(
            InboundKnock {
                intro: intro_from_r3_body(Some(&Value::Map(vec![(
                    Value::from("intro"),
                    Value::from(hostile.as_str()),
                )]))),
                ..knock_from(&identity, &hash_of("inst-hint"), KnockVia::Direct)
            },
            now(),
            SystemTime::now(),
        );

        assert_eq!(admission, Admission::Admitted { surfaced: true });
        let texts = rig.surface.texts();
        let text = &texts[0];
        for needle in [
            ".mesh trust ",
            ".mesh info ",
            ".mesh block ",
            identity.as_str(),
            hash_of("inst-hint").as_str(),
        ] {
            assert!(text.contains(needle), "{needle:?} missing from {text:?}");
        }
        assert!(!text.to_ascii_lowercase().contains("deny"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text:?}");
        assert!(!text.contains('\u{202E}'), "{text:?}");
        assert!(!text.contains('\r'), "{text:?}");
        let intro_start = text.find(": \"").unwrap() + 3;
        let intro_end = text.find("\"\ntrust:").unwrap();
        let intro = &text[intro_start..intro_end];
        assert!(intro.starts_with("EVIL  line2x"), "{intro:?}");
        assert_eq!(intro.chars().count(), KNOCK_INTRO_MAX_CHARS);
        let rendered = Notification::new(Source::Knock, text.clone()).render_lines();
        assert_eq!(rendered.len(), 2, "{rendered:?}");
        assert!(
            rendered[0].ends_with(&format!("{intro}\"")),
            "the longest line survives rendering uncut: {}",
            rendered[0]
        );
        assert!(rendered[1].ends_with(&format!(".mesh block {identity}")));
        assert!(
            rendered
                .iter()
                .all(|line| line.starts_with("[mesh:knock] "))
        );
        assert!(rendered[0].chars().count() < NOTIFICATION_LINE_MAX_CHARS);
    }

    #[test]
    fn one_identity_is_rate_limited_per_identity_and_surfaced_once() {
        let identity = hash_of("id-burst");
        let rig = Rig::new(
            "knock-gate-burst",
            TrustList::default().identity(&identity, false),
        );
        let start = now();

        let admissions: Vec<Admission> = (0..5)
            .map(|n| {
                rig.gate.admit(
                    knock_from(&identity, &hash_of(&format!("inst-{n}")), KnockVia::Direct),
                    start,
                    SystemTime::now(),
                )
            })
            .collect();

        assert_eq!(
            admissions,
            vec![
                Admission::Admitted { surfaced: true },
                Admission::Admitted { surfaced: false },
                Admission::Admitted { surfaced: false },
                Admission::RateLimited,
                Admission::RateLimited,
            ]
        );
        assert_eq!(rig.surface.texts().len(), 1);
        assert_eq!(rig.cached().len(), KNOCK_BUCKET_BURST as usize);

        let refilled = rig.gate.admit(
            knock_from(&identity, &hash_of("inst-late"), KnockVia::Direct),
            start + KNOCK_BUCKET_REFILL_INTERVAL,
            SystemTime::now(),
        );
        assert_eq!(refilled, Admission::Admitted { surfaced: false });
        let again = rig.gate.admit(
            knock_from(&identity, &hash_of("inst-later"), KnockVia::Direct),
            start + KNOCK_BUCKET_REFILL_INTERVAL,
            SystemTime::now(),
        );
        assert_eq!(again, Admission::RateLimited);
        assert_eq!(rig.surface.texts().len(), 1);
        assert_eq!(rig.cached().len(), KNOCK_BUCKET_BURST as usize + 1);
    }

    #[test]
    #[serial_test::serial(knock_cache_overflow)]
    fn the_gate_forgets_the_least_recently_seen_identity_past_its_cap() {
        let mut list = TrustList::default();
        let identities: Vec<String> = (0..=KNOCK_GATE_MAX_IDENTITIES)
            .map(|n| hash_of(&format!("id{n}")))
            .collect();
        for identity in &identities {
            list = list.identity(identity, false);
        }
        let rig = Rig::new("knock-gate-cap", list);
        let start = now();

        for (n, identity) in identities[..KNOCK_GATE_MAX_IDENTITIES].iter().enumerate() {
            rig.gate.admit(
                knock_from(identity, &hash_of("inst"), KnockVia::Direct),
                start + Duration::from_secs(n as u64 + 1),
                SystemTime::now(),
            );
        }
        // The first identity comes back, so the second is now the stalest.
        rig.gate.admit(
            knock_from(&identities[0], &hash_of("inst"), KnockVia::Direct),
            start + Duration::from_secs(1_000),
            SystemTime::now(),
        );
        rig.gate.admit(
            knock_from(
                &identities[KNOCK_GATE_MAX_IDENTITIES],
                &hash_of("inst"),
                KnockVia::Direct,
            ),
            start + Duration::from_secs(1_001),
            SystemTime::now(),
        );

        let tracked = rig.gate.tracked_identities();
        assert_eq!(tracked.len(), KNOCK_GATE_MAX_IDENTITIES);
        assert!(
            !tracked.contains(&identities[1]),
            "the stalest identity goes"
        );
        assert!(tracked.contains(&identities[0]));
        assert!(tracked.contains(&identities[KNOCK_GATE_MAX_IDENTITIES]));
    }

    #[test]
    fn a_blocked_identity_leaves_no_trace_on_either_path() {
        let identity = hash_of("id-blocked");
        let rig = Rig::new(
            "knock-gate-blocked",
            TrustList::default()
                .identity(&identity, false)
                .block(&identity),
        );

        for via in [KnockVia::Direct, KnockVia::StoreAndForward] {
            let admission = rig.gate.admit(
                knock_from(&identity, &hash_of("inst"), via),
                now(),
                SystemTime::now(),
            );
            assert_eq!(admission, Admission::Blocked, "{via:?}");
        }

        assert!(rig.surface.texts().is_empty());
        assert!(!rig.gate.cache().path().exists());
        assert!(rig.gate.tracked_identities().is_empty());
    }

    #[test]
    fn an_unknown_identity_is_never_a_knock() {
        let rig = Rig::new("knock-gate-unknown", TrustList::default());

        for via in [KnockVia::Direct, KnockVia::StoreAndForward] {
            let admission = rig.gate.admit(
                knock_from(&hash_of("stranger"), &hash_of("inst"), via),
                now(),
                SystemTime::now(),
            );
            assert_eq!(admission, Admission::Unknown, "{via:?}");
        }

        assert!(rig.surface.texts().is_empty());
        assert!(!rig.gate.cache().path().exists());
        assert!(rig.gate.tracked_identities().is_empty());
    }

    #[test]
    fn a_denied_instance_and_a_trusted_one_are_not_knocks() {
        install_log_collector();
        let identity = hash_of("id-standing");
        let rig = Rig::new(
            "knock-gate-standing",
            TrustList::default()
                .destination(&hash_of("inst-ok"), &identity)
                .deny(&hash_of("inst-denied")),
        );

        let denied = rig.gate.admit(
            knock_from(&identity, &hash_of("inst-denied"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );
        let trusted = rig.gate.admit(
            knock_from(&identity, &hash_of("inst-ok"), KnockVia::StoreAndForward),
            now(),
            SystemTime::now(),
        );

        assert_eq!(denied, Admission::Denied);
        assert_eq!(trusted, Admission::AlreadyTrusted);
        assert!(rig.surface.texts().is_empty());
        assert!(!rig.gate.cache().path().exists());
        assert!(rig.gate.tracked_identities().is_empty());
        let debugs = debug_snapshot();
        let line = format!(
            "Mesh knock from {} via StoreAndForward is not a knock: already trusted from {}",
            &identity[..8],
            &hash_of("inst-ok")[..8]
        );
        assert!(debugs.contains(&line), "{line}");
        let line = format!(
            "Mesh knock from {} for destination {} dropped: destination denied",
            &identity[..8],
            &hash_of("inst-denied")[..8]
        );
        assert!(debugs.contains(&line), "{line}");
    }

    #[test]
    fn without_a_surface_the_knock_is_cached_but_not_marked_surfaced() {
        let identity = hash_of("id-nosurface");
        let (trust, tmp) = TrustList::default()
            .identity(&identity, false)
            .open("knock-gate-no-surface");
        let peers =
            Arc::new(PeerTable::load(tmp.path.join("peers.json"), SystemTime::now()).unwrap());
        let gate = KnockGate::new(trust, peers, KnockCache::new(&tmp.path, 24));

        let first = gate.admit(
            knock_from(&identity, &hash_of("inst"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );
        assert_eq!(first, Admission::Admitted { surfaced: false });
        assert_eq!(gate.cache().list(SystemTime::now()).unwrap().len(), 1);

        let surface = Arc::new(RecordingSurface::default());
        gate.attach(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        let second = gate.admit(
            knock_from(&identity, &hash_of("inst"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );
        assert_eq!(second, Admission::Admitted { surfaced: true });
        assert_eq!(surface.texts().len(), 1);
        assert!(surface.texts()[0].starts_with("A peer (identity "));
    }

    #[test]
    fn a_hostile_display_name_is_cleaned_before_it_reaches_the_cache_or_the_line() {
        let identity = hash_of("id-name");
        let rig = Rig::new(
            "knock-gate-name",
            TrustList::default().identity(&identity, false),
        );
        rig.peers.observe(
            PeerSighting {
                destination_hash: hash_of("inst"),
                identity_hash: identity.clone(),
                name_hash: "00".repeat(10),
                display_name: Some("Al\u{1b}[2Jex\u{202E}".to_string()),
                protocol_version: 1,
                hops: 2,
            },
            SystemTime::now(),
        );

        let admission = rig.gate.admit(
            knock_from(&identity, &hash_of("inst"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );

        assert_eq!(admission, Admission::Admitted { surfaced: true });
        assert!(rig.surface.texts()[0].starts_with("\"Alex\" (identity "));
        assert_eq!(rig.cached()[0].display_name.as_deref(), Some("Alex"));
    }

    #[test]
    fn quotes_in_peer_text_cannot_pose_as_the_frame() {
        let identity = hash_of("id-quote");
        let rig = Rig::new(
            "knock-gate-quote",
            TrustList::default().identity(&identity, false),
        );
        rig.peers.observe(
            PeerSighting {
                destination_hash: hash_of("inst-quote"),
                identity_hash: identity.clone(),
                name_hash: "00".repeat(10),
                display_name: Some("Al\" (identity 00) knocks from instance \"".to_string()),
                protocol_version: 1,
                hops: 1,
            },
            SystemTime::now(),
        );

        rig.gate.admit(
            InboundKnock {
                intro: Some("hi\" said \"nobody".to_string()),
                ..knock_from(&identity, &hash_of("inst-quote"), KnockVia::Direct)
            },
            now(),
            SystemTime::now(),
        );

        let texts = rig.surface.texts();
        let first_line = texts[0].split('\n').next().unwrap();
        assert_eq!(
            first_line,
            format!(
                "\"Al' (identity 00) knocks from instance '\" (identity {identity}) knocks from instance {}: \"hi' said 'nobody\"",
                hash_of("inst-quote")
            )
        );
        assert_eq!(first_line.matches('"').count(), 4);
    }

    #[test]
    fn the_knocking_instance_names_itself_and_another_instance_only_lends_a_name() {
        let identity = hash_of("id-two-inst");
        let rig = Rig::new(
            "knock-gate-two-instances",
            TrustList::default().identity(&identity, false),
        );
        let base = SystemTime::now();
        for (n, (destination, name, hops)) in [("inst-old", "Older", 5), ("inst-new", "Newer", 2)]
            .into_iter()
            .enumerate()
        {
            rig.peers.observe(
                PeerSighting {
                    destination_hash: hash_of(destination),
                    identity_hash: identity.clone(),
                    name_hash: "00".repeat(10),
                    display_name: Some(name.to_string()),
                    protocol_version: 1,
                    hops,
                },
                base + Duration::from_secs(n as u64),
            );
        }

        rig.gate.admit(
            knock_from(&identity, &hash_of("inst-old"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );
        rig.gate.admit(
            knock_from(
                &identity,
                &hash_of("inst-unseen"),
                KnockVia::StoreAndForward,
            ),
            now(),
            SystemTime::now(),
        );

        let cached = rig.cached();
        let unseen = cached
            .iter()
            .find(|record| record.destination_hash == hash_of("inst-unseen"))
            .unwrap();
        assert_eq!(unseen.display_name.as_deref(), Some("Newer"));
        assert_eq!(
            unseen.hops, 0,
            "an instance never announced has no distance"
        );
        let old = cached
            .iter()
            .find(|record| record.destination_hash == hash_of("inst-old"))
            .unwrap();
        assert_eq!(old.display_name.as_deref(), Some("Older"));
        assert_eq!(old.hops, 5);
        assert!(rig.surface.texts()[0].starts_with("\"Older\" (identity "));
    }

    #[test]
    fn a_long_multibyte_name_is_cut_to_the_byte_cap_so_the_cache_takes_it() {
        let identity = hash_of("id-long-name");
        let rig = Rig::new(
            "knock-gate-long-name",
            TrustList::default().identity(&identity, false),
        );
        rig.peers.observe(
            PeerSighting {
                destination_hash: hash_of("inst-long"),
                identity_hash: identity.clone(),
                name_hash: "00".repeat(10),
                display_name: Some("\u{e9}".repeat(MAX_DISPLAY_NAME_BYTES)),
                protocol_version: 1,
                hops: 1,
            },
            SystemTime::now(),
        );

        let admission = rig.gate.admit(
            knock_from(&identity, &hash_of("inst-long"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );

        assert_eq!(admission, Admission::Admitted { surfaced: true });
        let cached = rig.cached();
        assert_eq!(cached.len(), 1, "the byte cap never refuses the record");
        let name = cached[0].display_name.as_deref().unwrap();
        assert_eq!(name, "\u{e9}".repeat(MAX_DISPLAY_NAME_BYTES / 2));
        assert_eq!(name.len(), MAX_DISPLAY_NAME_BYTES);
    }

    /// A surface that takes nothing, as the slot does when the idle driver's queue is full.
    #[derive(Default)]
    struct RejectingSurface {
        offered: Mutex<usize>,
    }

    impl KnockSurface for RejectingSurface {
        fn surface(&self, _note: IdleNotify) -> bool {
            *self.offered.lock() += 1;
            false
        }
    }

    #[test]
    fn a_line_the_surface_drops_is_offered_again_on_the_next_knock() {
        let identity = hash_of("id-rearm");
        let (trust, tmp) = TrustList::default()
            .identity(&identity, false)
            .open("knock-gate-rearm");
        let peers =
            Arc::new(PeerTable::load(tmp.path.join("peers.json"), SystemTime::now()).unwrap());
        let gate = KnockGate::new(trust, peers, KnockCache::new(&tmp.path, 24));
        let rejecting = Arc::new(RejectingSurface::default());
        gate.attach(Arc::downgrade(&rejecting) as Weak<dyn KnockSurface>);

        let dropped = gate.admit(
            knock_from(&identity, &hash_of("inst"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );
        assert_eq!(dropped, Admission::Admitted { surfaced: false });
        assert_eq!(*rejecting.offered.lock(), 1);
        assert_eq!(gate.cache().list(SystemTime::now()).unwrap().len(), 1);

        let recording = Arc::new(RecordingSurface::default());
        gate.attach(Arc::downgrade(&recording) as Weak<dyn KnockSurface>);
        let shown = gate.admit(
            knock_from(&identity, &hash_of("inst"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );
        assert_eq!(shown, Admission::Admitted { surfaced: true });
        assert_eq!(recording.texts().len(), 1);

        gate.attach(Arc::downgrade(&rejecting) as Weak<dyn KnockSurface>);
        let spent = gate.admit(
            knock_from(&identity, &hash_of("inst"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );
        assert_eq!(spent, Admission::Admitted { surfaced: false });
        assert_eq!(
            *rejecting.offered.lock(),
            1,
            "a line already shown is not offered again"
        );
    }

    #[test]
    fn routing_forwards_messages_drops_malformed_knocks_and_gates_real_ones() {
        install_log_collector();
        let identity = hash_of("id-route");
        let rig = Rig::new(
            "knock-routing",
            TrustList::default().identity(&identity, false),
        );
        let inner = CountingSink::default();
        let routing = KnockRouting {
            gate: &rig.gate,
            inner: &inner,
        };
        let origin = OriginName([5u8; NAME_HASH_LEN]);
        let knock = knock_message(&KnockIntro::new("stored").unwrap(), &origin);

        routing.deliver(inbound(
            Some(Value::Map(vec![])),
            Some(b"a message".to_vec()),
            &identity,
        ));
        routing.deliver(inbound(
            Some(Value::Map(vec![(
                Value::from(FIELD_CUSTOM_TYPE),
                Value::from(KNOCK_TYPE),
            )])),
            Some(b"broken".to_vec()),
            &identity,
        ));
        routing.deliver(inbound(
            knock.fields.clone(),
            Some(knock.content.clone()),
            &identity,
        ));

        assert_eq!(inner.count(), 1, "only the message reaches the inner sink");
        assert_eq!(
            inner.only().content.as_deref(),
            Some(b"a message".as_slice())
        );
        let expected_destination = destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&identity).unwrap(),
        )
        .to_hex_string();
        let cached = rig.cached();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].destination_hash, expected_destination);
        assert_eq!(cached[0].intro.as_deref(), Some("stored"));
        let texts = rig.surface.texts();
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains(&format!(".mesh trust {expected_destination}")));
        assert!(debug_snapshot().iter().any(|line| line
            == &format!(
                "Propagated knock from {} dropped: custom data is missing or not a map",
                &identity[..8]
            )));
    }

    #[test]
    fn routing_a_knock_from_a_trusted_instance_is_not_a_knock() {
        install_log_collector();
        let identity = hash_of("id-route-trusted");
        let origin = OriginName([6u8; NAME_HASH_LEN]);
        let destination = destination_address(
            &origin.0,
            &AddressHash::new_from_hex_string(&identity).unwrap(),
        )
        .to_hex_string();
        let rig = Rig::new(
            "knock-routing-trusted",
            TrustList::default().destination(&destination, &identity),
        );
        let inner = CountingSink::default();
        let routing = KnockRouting {
            gate: &rig.gate,
            inner: &inner,
        };
        let knock = knock_message(&KnockIntro::new("already in").unwrap(), &origin);

        routing.deliver(inbound(
            knock.fields.clone(),
            Some(knock.content.clone()),
            &identity,
        ));

        assert_eq!(inner.count(), 0);
        assert!(rig.surface.texts().is_empty());
        assert!(!rig.gate.cache().path().exists());
        let line = format!(
            "Mesh knock from {} via StoreAndForward is not a knock: already trusted from {}",
            &identity[..8],
            &destination[..8]
        );
        assert!(debug_snapshot().contains(&line), "{line}");
    }

    #[derive(Default)]
    struct CountingSink {
        messages: Mutex<Vec<InboundMessage>>,
    }

    impl CountingSink {
        fn count(&self) -> usize {
            self.messages.lock().len()
        }

        fn only(&self) -> InboundMessage {
            let messages = self.messages.lock();
            assert_eq!(messages.len(), 1);
            messages[0].clone()
        }
    }

    impl InboundSink for CountingSink {
        fn deliver(&self, message: InboundMessage) {
            self.messages.lock().push(message);
        }
    }

    fn event(identity: &str, destination: &str, data: Option<Value>) -> KnockEvent {
        KnockEvent {
            identity_hash: identity.to_string(),
            destination_hash: destination.to_string(),
            link_id: LinkId::new_from_rand(OsRng),
            path_hash: PathHash::of(STATUS_PATH),
            data,
        }
    }

    #[test]
    fn the_channel_sink_never_waits_on_a_reader_and_counts_what_it_drops() {
        install_log_collector();
        let (sink, _rx) = ChannelKnockSink::new(1);
        let started = Instant::now();

        for _ in 0..10_000 {
            sink.knock(event(&hash_of("id-flood"), &hash_of("inst"), None));
        }

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(sink.overflow(), 9_999);
        let logs = debug_snapshot();
        assert!(
            logs.iter()
                .any(|line| line.contains("Mesh knock queue is full (9999 dropped so far)"))
        );
        assert!(
            !logs.iter().any(|line| line.contains(&hash_of("id-flood"))),
            "the sink logs short hashes only"
        );
    }

    #[tokio::test]
    async fn the_drain_task_feeds_the_gate_and_stops_on_cancel() {
        let identity = hash_of("id-drain");
        let (trust, tmp) = TrustList::default()
            .identity(&identity, false)
            .open("knock-drain");
        let peers =
            Arc::new(PeerTable::load(tmp.path.join("peers.json"), SystemTime::now()).unwrap());
        let gate = Arc::new(KnockGate::new(trust, peers, KnockCache::new(&tmp.path, 24)));
        let surface = Arc::new(RecordingSurface::default());
        gate.attach(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        let (sink, rx) = ChannelKnockSink::new(KNOCK_QUEUE_CAPACITY);
        let cancel = CancellationToken::new();
        let task = tokio::spawn(drain_knocks(rx, gate.clone(), cancel.clone()));

        sink.knock(event(
            &identity,
            &hash_of("inst"),
            Some(KnockIntro::new("via the queue").unwrap().to_r3_body()),
        ));
        sink.knock(event(&hash_of("stranger"), &hash_of("inst"), None));
        let deadline = Instant::now() + Duration::from_secs(5);
        while gate.cache().list(SystemTime::now()).unwrap().is_empty() {
            assert!(
                Instant::now() < deadline,
                "the drain task never filed the knock"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let cached = gate.cache().list(SystemTime::now()).unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].intro.as_deref(), Some("via the queue"));
        assert_eq!(surface.texts().len(), 1);
        assert!(surface.texts()[0].contains("\"via the queue\""));
        assert_eq!(gate.tracked_identities(), vec![identity]);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the drain task stops on cancel")
            .unwrap();
    }

    // Spec-first usage probes: each pins one promise of the knock contract as a consumer
    // observes it, independent of how the gate happens to be built.

    #[test]
    fn lxmf_knock_wire_shape_is_exactly_the_typed_two_field_layout() {
        assert_eq!(FIELD_CUSTOM_TYPE, 0xFB);
        assert_eq!(FIELD_CUSTOM_DATA, 0xFC);
        let origin = OriginName([9u8; NAME_HASH_LEN]);
        let message = knock_message(&KnockIntro::new("hey there").unwrap(), &origin);

        assert_eq!(message.title, None);
        assert_eq!(message.content, b"hey there".to_vec());
        assert_eq!(
            message.fields,
            Some(Value::Map(vec![
                (Value::from(0xFBu8), Value::from("coyote.knock/1")),
                (
                    Value::from(0xFCu8),
                    Value::Map(vec![(
                        Value::from("name_hash"),
                        Value::Binary(vec![9u8; 10]),
                    )]),
                ),
            ]))
        );

        let wordless = knock_message(&KnockIntro::new("   ").unwrap(), &origin);
        assert!(wordless.content.is_empty());
        assert_eq!(
            decode_knock_message(&inbound(
                wordless.fields,
                Some(wordless.content),
                &hash_of("signer")
            )),
            KnockMessage::Knock {
                name_hash: origin.0,
                intro: None,
            }
        );
    }

    #[test]
    fn a_blocked_identity_is_silent_before_any_trust_rule_is_consulted() {
        let everywhere = hash_of("id-blocked-all");
        let by_instance = hash_of("id-blocked-inst");
        let stranger = hash_of("id-blocked-stranger");
        let rig = Rig::new(
            "knock-gate-block-order",
            TrustList::default()
                .identity(&everywhere, true)
                .block(&everywhere)
                .destination(&hash_of("inst-b"), &by_instance)
                .block(&by_instance)
                .block(&stranger),
        );

        for (identity, destination) in [
            (&everywhere, hash_of("inst-a")),
            (&by_instance, hash_of("inst-b")),
            (&stranger, hash_of("inst-c")),
        ] {
            for via in [KnockVia::Direct, KnockVia::StoreAndForward] {
                let admission = rig.gate.admit(
                    knock_from(identity, &destination, via),
                    now(),
                    SystemTime::now(),
                );
                assert_eq!(
                    admission,
                    Admission::Blocked,
                    "{} via {via:?}",
                    &identity[..8]
                );
            }
        }

        assert!(rig.surface.texts().is_empty());
        assert!(!rig.gate.cache().path().exists());
        assert!(rig.gate.tracked_identities().is_empty());
    }

    #[test]
    fn the_bucket_refills_one_token_per_interval_and_never_past_the_burst() {
        let noisy = hash_of("id-noisy");
        let quiet = hash_of("id-quiet");
        let rig = Rig::new(
            "knock-gate-refill",
            TrustList::default()
                .identity(&noisy, false)
                .identity(&quiet, false),
        );
        let start = now();
        let knock = |identity: &str, n: usize, at: Instant| {
            rig.gate.admit(
                knock_from(identity, &hash_of(&format!("i{n}")), KnockVia::Direct),
                at,
                SystemTime::now(),
            )
        };

        for n in 0..KNOCK_BUCKET_BURST as usize {
            assert!(matches!(
                knock(&noisy, n, start),
                Admission::Admitted { .. }
            ));
        }
        assert_eq!(knock(&noisy, 10, start), Admission::RateLimited);
        assert_eq!(
            knock(&quiet, 11, start),
            Admission::Admitted { surfaced: true },
            "one identity's flood never touches another's bucket"
        );
        assert_eq!(rig.surface.texts().len(), 2);

        let two_later = start + KNOCK_BUCKET_REFILL_INTERVAL * 2;
        assert_eq!(
            knock(&noisy, 20, two_later),
            Admission::Admitted { surfaced: false }
        );
        assert_eq!(
            knock(&noisy, 21, two_later),
            Admission::Admitted { surfaced: false }
        );
        assert_eq!(
            knock(&noisy, 22, two_later),
            Admission::RateLimited,
            "two intervals return exactly two tokens"
        );

        let much_later = start + KNOCK_BUCKET_REFILL_INTERVAL * 100;
        for n in 30..30 + KNOCK_BUCKET_BURST as usize {
            assert_eq!(
                knock(&noisy, n, much_later),
                Admission::Admitted { surfaced: false }
            );
        }
        assert_eq!(
            knock(&noisy, 40, much_later),
            Admission::RateLimited,
            "a long silence refills to the burst and no further"
        );

        assert_eq!(rig.surface.texts().len(), 2, "still one line per identity");
        let cached = rig.cached();
        let noisy_records = cached
            .iter()
            .filter(|record| record.identity_hash == noisy)
            .count();
        assert_eq!(noisy_records, KNOCK_BUCKET_BURST as usize * 2 + 2);
        assert_eq!(cached.len(), noisy_records + 1);
    }

    #[test]
    #[serial_test::serial(knock_cache_overflow)]
    fn an_identity_the_gate_forgot_is_surfaced_again_when_it_returns() {
        let mut list = TrustList::default();
        let identities: Vec<String> = (0..=KNOCK_GATE_MAX_IDENTITIES)
            .map(|n| hash_of(&format!("re{n}")))
            .collect();
        for identity in &identities {
            list = list.identity(identity, false);
        }
        let rig = Rig::new("knock-gate-resurface", list);
        let start = now();

        for (n, identity) in identities.iter().enumerate() {
            let admission = rig.gate.admit(
                knock_from(identity, &hash_of("inst"), KnockVia::Direct),
                start + Duration::from_secs(n as u64 + 1),
                SystemTime::now(),
            );
            assert_eq!(admission, Admission::Admitted { surfaced: true });
        }
        assert_eq!(rig.surface.texts().len(), KNOCK_GATE_MAX_IDENTITIES + 1);
        assert!(!rig.gate.tracked_identities().contains(&identities[0]));

        let back = rig.gate.admit(
            knock_from(&identities[0], &hash_of("inst"), KnockVia::Direct),
            start + Duration::from_secs(5_000),
            SystemTime::now(),
        );
        assert_eq!(
            back,
            Admission::Admitted { surfaced: true },
            "once-per-lifetime is bounded by the identity map: an evicted identity is new again"
        );
        assert_eq!(rig.surface.texts().len(), KNOCK_GATE_MAX_IDENTITIES + 2);
        assert!(!rig.gate.tracked_identities().contains(&identities[1]));
    }

    #[test]
    fn a_knock_without_words_or_a_known_name_renders_the_bare_two_line_form() {
        let identity = hash_of("id-bare");
        let rig = Rig::new(
            "knock-gate-bare",
            TrustList::default().identity(&identity, false),
        );

        let admission = rig.gate.admit(
            InboundKnock {
                intro: None,
                ..knock_from(&identity, &hash_of("inst-bare"), KnockVia::StoreAndForward)
            },
            now(),
            SystemTime::now(),
        );

        assert_eq!(admission, Admission::Admitted { surfaced: true });
        let notes = rig.surface.notes.lock();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].source, Source::Knock);
        assert_eq!(notes[0].origin, Origin::Peer(identity[..8].to_string()));
        assert!(
            notes[0].model_note.is_none(),
            "knocks never carry a model note"
        );
        let dest = hash_of("inst-bare");
        assert_eq!(
            notes[0].text,
            format!(
                "A peer (identity {identity}) knocks from instance {dest}\n\
                 trust: .mesh trust {dest} | who: .mesh info {dest} | silence: .mesh block {identity}"
            )
        );
        assert!(!notes[0].text.contains('"'), "no quote without words");
        assert!(!notes[0].text.to_ascii_lowercase().contains("deny"));
        drop(notes);
        assert_eq!(rig.cached()[0].display_name, None);
        assert_eq!(rig.cached()[0].intro, None);
    }

    /// The slot that owned the surface is gone but the gate still holds its `Weak`: the
    /// upgrade fails before `surfaced` is touched, so the knock is cached and the identity
    /// still earns its line once a live surface is attached.
    #[test]
    fn a_dead_surface_is_treated_as_no_surface_and_does_not_spend_the_line() {
        let identity = hash_of("id-dead-surface");
        let (trust, tmp) = TrustList::default()
            .identity(&identity, false)
            .open("knock-gate-dead-surface");
        let peers =
            Arc::new(PeerTable::load(tmp.path.join("peers.json"), SystemTime::now()).unwrap());
        let gate = KnockGate::new(trust, peers, KnockCache::new(&tmp.path, 24));
        let gone = Arc::new(RecordingSurface::default());
        gate.attach(Arc::downgrade(&gone) as Weak<dyn KnockSurface>);
        drop(gone);

        let first = gate.admit(
            knock_from(&identity, &hash_of("inst"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );
        assert_eq!(first, Admission::Admitted { surfaced: false });
        assert_eq!(gate.cache().list(SystemTime::now()).unwrap().len(), 1);

        let live = Arc::new(RecordingSurface::default());
        gate.attach(Arc::downgrade(&live) as Weak<dyn KnockSurface>);
        let second = gate.admit(
            knock_from(&identity, &hash_of("inst"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );
        assert_eq!(second, Admission::Admitted { surfaced: true });
        assert_eq!(live.texts().len(), 1);
        assert_eq!(gate.cache().list(SystemTime::now()).unwrap().len(), 2);

        let third = gate.admit(
            knock_from(&identity, &hash_of("inst"), KnockVia::Direct),
            now(),
            SystemTime::now(),
        );
        assert_eq!(third, Admission::Admitted { surfaced: false });
        assert_eq!(live.texts().len(), 1, "one line per identity per session");
    }
}
