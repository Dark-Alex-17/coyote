//! Inbound LXMF store-and-forward: fetching the messages a propagation node holds for this
//! node's `lxmf.delivery` destination. This is the client side of
//! `LXMRouter.request_messages_from_propagation_node` (`LXMRouter.py:495-501, 1507-1589`):
//! identify on the link, ask for the list, ask for the bodies, acknowledge them so the
//! node deletes them. Every fetched body is bounded, deduplicated, decrypted, verified
//! against the key its claimed source announced and checked against the trust list, in
//! that order, before anything outside this module sees it. What passes leaves through
//! `InboundSink` as plain owned data. A body whose claimed source has no known key yet is
//! neither recorded nor acknowledged, so the node keeps it for a later fetch, a bounded
//! number of times.

use crate::mesh::peers::PeerTable;
use crate::mesh::propagation::{PropagationNode, lxmf_delivery_hash};
use crate::mesh::r3::{
    DEFAULT_LINK_TIMEOUT, Deadline, MAX_R3_PAYLOAD_BYTES, R3Client, R3Error, RefusalCode,
    SizeBranch, link_to, short,
};
use crate::mesh::trust::{IdentityStanding, TrustStore};
use crate::mesh::{canonical_hash, hex_lower, parse_rfc3339, rfc3339_utc, write_atomically};

use async_trait::async_trait;
use lxmf_core::identity::PrivateIdentity as CorePrivateIdentity;
use lxmf_core::message::WireMessage;
use lxmf_core::stamp::invalid_stamp_value;
use rmpv::Value;
use rns_transport::hash::AddressHash;
use rns_transport::identity::{Identity, PrivateIdentity as TransportIdentity};
use rns_transport::identity_bridge::{to_core_identity, to_core_private_identity};
use rns_transport::transport::Transport;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio_util::sync::CancellationToken;

/// The request path every round of the conversation goes to (`LXMPeer.py:15`).
const GET_PATH: &str = "/get";

/// Ceiling on waiting for one round's response. Round 2 may carry up to
/// `FETCH_TRANSFER_LIMIT_KB` of bodies as a resource, which a slow link needs longer for
/// than a Coyote request/response; the reference scales its waits with the link RTT, a
/// fixed bound is simpler.
pub(crate) const FETCH_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How many transient ids of the node's list are considered at all. The reference client
/// walks the whole list (`LXMRouter.py:1526-1531`); a node can list any number, so the
/// rest of a longer list is left for the next fetch, once the acknowledged ones are gone.
pub(crate) const MAX_LISTED_IDS: usize = 1024;

/// How many bodies one round 2 asks for, walking the list in the node's order (smallest
/// stored size first, `LXMRouter.py:1447`). The reference caps this with
/// `propagation_transfer_max_messages` (`LXMRouter.py:1530`); the cap here also bounds
/// the id arrays in the request and the bodies accepted in the reply.
pub(crate) const MAX_WANTS_PER_FETCH: usize = 64;

/// The transfer limit sent in round 2, in the reference's kilobytes of 1000 bytes
/// (`LXMRouter.py:1471`, default `DELIVERY_LIMIT = 1000`, `:57`). The node skips any body
/// that would take the cumulative size over `limit * 1000`, counting 24 bytes of structure
/// and 16 per body (`:1476-1489`), so the reply it builds is at most
/// `FETCH_TRANSFER_LIMIT_KB * 1000 + 24 + 16 * MAX_WANTS_PER_FETCH` bytes, which must stay
/// under `MAX_R3_PAYLOAD_BYTES`: a node honouring our limit then never trips the response
/// cap that drops the whole reply. 240 * 1000 + 24 + 16 * 64 = 241048 <= 262144.
pub(crate) const FETCH_TRANSFER_LIMIT_KB: u64 = 240;

const _: () = assert!(
    FETCH_TRANSFER_LIMIT_KB as usize * 1000 + 24 + 16 * MAX_WANTS_PER_FETCH <= MAX_R3_PAYLOAD_BYTES
);

/// Largest fetched body that is looked at. The upstream transport assembles a resource of
/// up to 64 MiB (`resource.rs:86-90`) and the request client keeps whole responses under
/// `MAX_R3_PAYLOAD_BYTES` (256 KiB); a single message half that size is already far above
/// anything LXMF clients send, and the bound keeps one body from being the whole reply.
/// The reference has no per-body maximum; its per-transfer limit is the only one.
pub(crate) const MAX_FETCHED_MESSAGE_BYTES: usize = 128 * 1024;

/// Smallest body that can be a message: `LXMessage.LXMF_OVERHEAD` (`LXMessage.py:55-62`,
/// two 16-byte hashes, a 64-byte signature, an 8-byte timestamp and 8 bytes of msgpack
/// structure), the same floor `LXMRouter.lxmf_propagation` applies (`LXMRouter.py:2316`).
pub(crate) const MIN_FETCHED_MESSAGE_BYTES: usize = 112;

/// How many transient ids the dedup store keeps, and separately how many delivered message
/// ids. Past this, the oldest goes first. The reference keeps its caches unbounded for the
/// horizon below; a bound is what keeps a node that lists fresh ids forever from growing
/// our state without limit.
pub(crate) const DEDUP_CAPACITY: usize = 4096;

/// How long a transient or message id is remembered: `MESSAGE_EXPIRY * 6`, 180 days
/// (`LXMRouter.py:38, 962`). Entries older than this are dropped on load and on insert.
pub(crate) const DEDUP_HORIZON: Duration = Duration::from_secs(180 * 24 * 60 * 60);

/// How many times a body whose claimed source has no known key is left on the node before
/// it is given up on. The transport's announce cache is empty after every restart and a
/// Coyote peer never announces `lxmf.delivery`, so the key may well arrive later.
pub(crate) const MAX_UNKNOWN_SOURCE_DEFERRALS: u8 = 3;

/// How many deferred transient ids are tracked. Past this, the one first deferred longest
/// ago goes first; a node serving unresolvable bodies without end cannot grow the table.
pub(crate) const MAX_DEFERRED_IDS: usize = 256;

pub(crate) const PROPAGATION_STORE_VERSION: u64 = 1;

/// The delivery stamp cost this node demands of a fetched message. It announces no
/// delivery stamp cost, so nothing is demanded and the supplied value is only logged. The
/// propagation stamp cannot be observed here at all: the node strips it before serving
/// (`LXMRouter.py:1494`).
pub(crate) const REQUIRED_DELIVERY_STAMP_COST: u32 = 0;

type TransientId = [u8; 32];

/// Timeouts for one fetch.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FetchOptions {
    pub link_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            link_timeout: DEFAULT_LINK_TIMEOUT,
            request_timeout: FETCH_REQUEST_TIMEOUT,
        }
    }
}

/// Why a fetch did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FetchError {
    /// No propagation node has announced itself since the node started.
    NoPropagationNode,
    /// Another fetch is in progress, in this process or another one sharing the store;
    /// calls are refused rather than queued.
    AlreadyRunning,
    Cancelled,
    /// Link establishment, a send, a bounded wait or the request client itself failed.
    Link(R3Error),
    /// `ERROR_NO_IDENTITY` (`LXMPeer.py:24`): the node saw no identity on the link.
    NodeSawNoIdentity {
        node: String,
    },
    /// `ERROR_NO_ACCESS` (`LXMPeer.py:25`): the node has `auth_required` set and our
    /// identity is not on its `allowed_list` (`LXMRouter.py:1417-1429`).
    NodeRefusedAccess {
        node: String,
        identity_hash: String,
    },
    /// Any other refusal sentinel.
    NodeRefused(RefusalCode),
    /// The round 1 reply was not a list of 32-byte transient ids.
    MalformedList(String),
    /// The round 2 reply was not a list of binary bodies.
    MalformedBodies(String),
    /// The store on disk was written by a Coyote reading a different layout.
    StoreVersion {
        path: PathBuf,
        found: u64,
    },
    /// The store could not be written.
    Store(String),
}

impl fmt::Display for FetchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPropagationNode => write!(
                f,
                "No LXMF propagation node has announced itself on the mesh yet, so there is nothing to fetch from. Wait for a node's announce, or check that the mesh interfaces reach one"
            ),
            Self::AlreadyRunning => {
                write!(
                    f,
                    "A propagation fetch is already running in this or another Coyote session of this identity; wait for it to finish"
                )
            }
            Self::Cancelled => write!(f, "The propagation fetch was cancelled"),
            Self::Link(err) => write!(f, "{err}"),
            Self::NodeSawNoIdentity { node } => write!(
                f,
                "Propagation node {node} answered that it saw no identity on the link, so our identify did not reach it. Retry; if this persists the node may be running an incompatible LXMF version"
            ),
            Self::NodeRefusedAccess {
                node,
                identity_hash,
            } => write!(
                f,
                "Propagation node {node} requires authorisation (auth_required) and its allowed list does not include our identity {identity_hash}. Ask the node's operator to add it"
            ),
            Self::NodeRefused(code) => {
                write!(f, "The propagation node refused the request: {code}")
            }
            Self::MalformedList(reason) => write!(
                f,
                "The propagation node answered the list request with something other than a list of transient ids: {reason}"
            ),
            Self::MalformedBodies(reason) => write!(
                f,
                "The propagation node answered the get request with something other than a list of message bodies: {reason}"
            ),
            Self::StoreVersion { path, found } => write!(
                f,
                "Propagation fetch state '{}' is version {found} but this Coyote reads version {PROPAGATION_STORE_VERSION}. If it was written by a newer Coyote, upgrade Coyote; otherwise move the file aside, it is cache and rebuilds itself",
                path.display()
            ),
            Self::Store(reason) => {
                write!(
                    f,
                    "The propagation fetch state could not be written: {reason}"
                )
            }
        }
    }
}

impl std::error::Error for FetchError {}

/// Held for one fetch across every Coyote process of this identity; a sibling of the store
/// file, taken with the same advisory lock the instance lock uses. Two sessions of one
/// identity share `<cache_dir>/mesh/propagation.json`, and the store is read at the start
/// of a fetch and written back at the end, so without this a concurrent fetch would
/// overwrite the other's remembered ids. Released when the `File` drops; the file itself
/// is never removed, for the reason `InstanceLock` gives.
pub(crate) struct FetchLock {
    _file: File,
}

impl FetchLock {
    /// Locks `<store_path>.lock`, refusing while any process, including this one, holds it.
    pub(crate) fn acquire(store_path: &Path) -> Result<Self, FetchError> {
        if let Some(parent) = store_path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                FetchError::Store(format!("create directory '{}': {err}", parent.display()))
            })?;
        }
        let path = store_path.with_added_extension("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| FetchError::Store(format!("open '{}': {err}", path.display())))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(FetchError::AlreadyRunning),
            Err(TryLockError::Error(err)) => Err(FetchError::Store(format!(
                "lock '{}': {err}",
                path.display()
            ))),
        }
    }
}

/// A request client failure as the fetch reports it: the two refusals the reference
/// distinguishes (`LXMRouter.py:1508-1518`) become their own errors with their remedies.
fn link_error(err: R3Error, node: &str, identity_hash: &str) -> FetchError {
    match err {
        R3Error::Refused(RefusalCode::NoIdentity) => FetchError::NodeSawNoIdentity {
            node: node.to_string(),
        },
        R3Error::Refused(RefusalCode::NoAccess) => FetchError::NodeRefusedAccess {
            node: node.to_string(),
            identity_hash: identity_hash.to_string(),
        },
        R3Error::Refused(code) => FetchError::NodeRefused(code),
        other => FetchError::Link(other),
    }
}

/// One fetched message that passed every check, as plain owned data. Nothing of the wire
/// types crosses this boundary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InboundMessage {
    /// `sha256` of the body as served, the id the node and the dedup store know it by.
    pub transient_id: TransientId,
    /// `sha256(destination || source || payload)`, LXMF's own message id.
    pub message_id: [u8; 32],
    /// Lower-hex of the identity that signed the message.
    pub source_identity_hash: String,
    /// Lower-hex of the sender's `lxmf.delivery` destination, the message's `source`.
    pub source_delivery_hash: String,
    pub timestamp: f64,
    pub title: Option<Vec<u8>>,
    pub content: Option<Vec<u8>>,
    pub fields: Option<Value>,
    /// The work value of the delivery stamp the sender attached, when it attached one.
    pub stamp_value: Option<u32>,
}

/// Where trusted messages go once the fetch is done with them.
pub(crate) trait InboundSink: Send + Sync {
    fn deliver(&self, message: InboundMessage);
}

/// Notes each delivered message in the debug log by its hashes and lengths; the title and
/// content are the sender's bytes and are never logged raw.
// Installed by the REPL mesh commands once they land.
#[allow(dead_code)]
pub(crate) struct LoggingInboundSink;

impl InboundSink for LoggingInboundSink {
    fn deliver(&self, message: InboundMessage) {
        debug!(
            "Propagated message {} from {} (delivery {}): {} title bytes, {} content bytes, stamp value {}",
            short(&hex_lower(&message.message_id)),
            short(&message.source_identity_hash),
            short(&message.source_delivery_hash),
            message.title.as_ref().map_or(0, Vec::len),
            message.content.as_ref().map_or(0, Vec::len),
            message
                .stamp_value
                .map_or_else(|| "none".to_string(), |value| value.to_string())
        );
    }
}

/// What one fetch did, outcome by outcome. `received` bodies split into `delivered`,
/// `duplicates`, `discarded` and `deferred`; `acknowledged` is what round 3 told the node
/// to delete, which is every received body but the deferred ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FetchReport {
    pub node: String,
    pub listed: usize,
    pub wanted: usize,
    pub received: usize,
    pub delivered: usize,
    pub duplicates: usize,
    pub discarded: usize,
    pub deferred: usize,
    pub acknowledged: usize,
    pub response_branch: Option<SizeBranch>,
}

impl FetchReport {
    fn new(node: String) -> Self {
        Self {
            node,
            listed: 0,
            wanted: 0,
            received: 0,
            delivered: 0,
            duplicates: 0,
            discarded: 0,
            deferred: 0,
            acknowledged: 0,
            response_branch: None,
        }
    }
}

/// Round 1: `[None, None]` asks for the list (`LXMRouter.py:497-498`).
pub(crate) fn list_request() -> Value {
    Value::Array(vec![Value::Nil, Value::Nil])
}

/// Round 2: `[wants, haves, limit]` (`LXMRouter.py:1535-1537`). `haves` are purged on the
/// node (`:1453-1459`), `wants` are served up to `limit` kilobytes (`:1471-1495`).
pub(crate) fn get_request(wants: &[TransientId], haves: &[TransientId]) -> Value {
    Value::Array(vec![
        ids_value(wants),
        ids_value(haves),
        Value::from(FETCH_TRANSFER_LIMIT_KB),
    ])
}

/// Round 3: `[None, haves]` (`LXMRouter.py:1577-1579`), which deletes `haves` on the node.
pub(crate) fn ack_request(haves: &[TransientId]) -> Value {
    Value::Array(vec![Value::Nil, ids_value(haves)])
}

fn ids_value(ids: &[TransientId]) -> Value {
    Value::Array(ids.iter().map(|id| Value::Binary(id.to_vec())).collect())
}

/// The round 1 reply: a flat array of 32-byte binaries (`LXMRouter.py:1447-1449`), of
/// which at most `MAX_LISTED_IDS` are considered. Anything else is malformed, as the
/// reference treats a non-list (`:1547-1550`).
pub(crate) fn parse_listed_ids(value: Value) -> Result<Vec<TransientId>, FetchError> {
    let items = match value {
        Value::Array(items) => items,
        other => {
            return Err(FetchError::MalformedList(format!(
                "not an array ({})",
                value_kind(&other)
            )));
        }
    };
    if items.len() > MAX_LISTED_IDS {
        debug!(
            "Propagation node listed {} transient ids; considering the first {MAX_LISTED_IDS}",
            items.len()
        );
    }
    items
        .iter()
        .take(MAX_LISTED_IDS)
        .enumerate()
        .map(|(index, item)| match item {
            Value::Binary(bytes) => TransientId::try_from(bytes.as_slice()).map_err(|_| {
                FetchError::MalformedList(format!(
                    "transient id {index} is {} bytes, expected 32",
                    bytes.len()
                ))
            }),
            other => Err(FetchError::MalformedList(format!(
                "transient id {index} is {}, expected binary",
                value_kind(other)
            ))),
        })
        .collect()
}

/// The round 2 reply: a flat array of binary bodies (`LXMRouter.py:1494, 1499`), each
/// `destination hash || encrypted message` with the propagation stamp already stripped.
/// At most `MAX_WANTS_PER_FETCH` are considered, since that is all that was asked for.
pub(crate) fn parse_bodies(value: Value) -> Result<Vec<Vec<u8>>, FetchError> {
    let items = match value {
        Value::Array(items) => items,
        other => {
            return Err(FetchError::MalformedBodies(format!(
                "not an array ({})",
                value_kind(&other)
            )));
        }
    };
    if items.len() > MAX_WANTS_PER_FETCH {
        debug!(
            "Propagation node served {} bodies for at most {MAX_WANTS_PER_FETCH} wanted; considering the first {MAX_WANTS_PER_FETCH}",
            items.len()
        );
    }
    items
        .into_iter()
        .take(MAX_WANTS_PER_FETCH)
        .enumerate()
        .map(|(index, item)| match item {
            Value::Binary(bytes) => Ok(bytes),
            other => Err(FetchError::MalformedBodies(format!(
                "body {index} is {}, expected binary",
                value_kind(&other)
            ))),
        })
        .collect()
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Nil => "nil",
        Value::Boolean(_) => "a boolean",
        Value::Integer(_) => "an integer",
        Value::F32(_) | Value::F64(_) => "a float",
        Value::String(_) => "a string",
        Value::Binary(_) => "binary",
        Value::Array(_) => "an array",
        Value::Map(_) => "a map",
        Value::Ext(..) => "an extension",
    }
}

fn transient_id_of(body: &[u8]) -> TransientId {
    Sha256::digest(body).into()
}

/// The last completed sync. The `/get` protocol has no server-side cursor: the node
/// forgets what it served once round 3 acknowledges it, so this only records when and
/// with whom the last sync happened and how much it brought.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncCursor {
    pub node: String,
    pub last_synced_at: SystemTime,
    pub last_received: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    version: u64,
    seen: Vec<SeenRecord>,
    delivered: Vec<DeliveredRecord>,
    deferred: Vec<DeferredRecord>,
    cursor: Option<CursorRecord>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeenRecord {
    transient_id: String,
    seen_at: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveredRecord {
    message_id: String,
    delivered_at: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeferredRecord {
    transient_id: String,
    attempts: u8,
    first_seen_at: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorRecord {
    node: String,
    last_synced_at: String,
    last_received: u64,
}

#[derive(Deserialize)]
struct VersionProbe {
    version: u64,
}

enum StoreParse {
    Version(u64),
    Corrupt(String),
}

/// What the file held, validated but not yet bounded.
struct Loaded {
    seen: Vec<(TransientId, SystemTime)>,
    delivered: Vec<([u8; 32], SystemTime)>,
    deferred: Vec<(TransientId, Deferred)>,
    cursor: Option<SyncCursor>,
}

impl Loaded {
    fn empty() -> Self {
        Self {
            seen: Vec::new(),
            delivered: Vec::new(),
            deferred: Vec::new(),
            cursor: None,
        }
    }
}

/// Up to `DEDUP_CAPACITY` 32-byte hashes with when each was recorded, none older than
/// `DEDUP_HORIZON`, the oldest evicted first. `what` names an entry in the log.
struct BoundedSet {
    what: &'static str,
    at: BTreeMap<[u8; 32], SystemTime>,
    by_age: BTreeSet<(SystemTime, [u8; 32])>,
}

impl BoundedSet {
    fn new(what: &'static str) -> Self {
        Self {
            what,
            at: BTreeMap::new(),
            by_age: BTreeSet::new(),
        }
    }

    fn contains(&self, id: &[u8; 32]) -> bool {
        self.at.contains_key(id)
    }

    fn len(&self) -> usize {
        self.at.len()
    }

    /// Oldest first.
    fn iter(&self) -> impl Iterator<Item = &(SystemTime, [u8; 32])> {
        self.by_age.iter()
    }

    fn insert_at(&mut self, id: [u8; 32], recorded_at: SystemTime, now: SystemTime) -> bool {
        self.expire(now);
        if is_past_horizon(recorded_at, now) || self.at.contains_key(&id) {
            return false;
        }
        self.at.insert(id, recorded_at);
        self.by_age.insert((recorded_at, id));
        while self.at.len() > DEDUP_CAPACITY {
            let (_, oldest) = self
                .by_age
                .pop_first()
                .expect("a set over its capacity is not empty");
            self.at.remove(&oldest);
            debug!(
                "Forgot {} {} to stay within {DEDUP_CAPACITY} remembered",
                self.what,
                short(&hex_lower(&oldest))
            );
        }
        true
    }

    fn expire(&mut self, now: SystemTime) {
        while let Some((recorded_at, id)) = self.by_age.first().copied() {
            if !is_past_horizon(recorded_at, now) {
                break;
            }
            self.by_age.remove(&(recorded_at, id));
            self.at.remove(&id);
            debug!(
                "Forgot {} {} seen more than {} days ago",
                self.what,
                short(&hex_lower(&id)),
                DEDUP_HORIZON.as_secs() / (24 * 60 * 60)
            );
        }
    }
}

/// One transient id left on the node for want of its source's key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Deferred {
    pub attempts: u8,
    pub first_seen_at: SystemTime,
}

/// What `FetchStore::defer` decided for a sighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Deferral {
    /// Left on the node; `attempts` sightings have been deferred so far.
    Retry { attempts: u8 },
    /// Deferred `MAX_UNKNOWN_SOURCE_DEFERRALS` times already; the body is now discarded.
    BudgetSpent,
}

/// Transient ids already processed, so a body the node serves again (or another node
/// serves) is dropped before it is decrypted; message ids already delivered, so junk
/// filling the transient set cannot make a message deliverable again; transient ids
/// deferred for want of a key, with how often; plus the last-sync cursor. Bounded by
/// construction: `DEDUP_CAPACITY` ids in each set, none older than `DEDUP_HORIZON`, the
/// oldest evicted first; `MAX_DEFERRED_IDS` deferrals. Mirrored to
/// `<cache_dir>/mesh/propagation.json`.
pub(crate) struct FetchStore {
    path: PathBuf,
    seen: BoundedSet,
    delivered: BoundedSet,
    deferred: BTreeMap<TransientId, Deferred>,
    cursor: Option<SyncCursor>,
}

impl FetchStore {
    /// Loads the store at `path`, dropping entries past the horizon at `now` and, should
    /// the file hold more than the capacity, the oldest beyond it. A missing file is an
    /// empty store. A file from a different layout version is refused, since the remedy
    /// depends on which Coyote wrote it; one that cannot be read or parsed is moved aside
    /// to `<path>.corrupt` with a warning and the store starts empty, since it is cache and
    /// must never keep a fetch from running.
    pub(crate) fn load(path: PathBuf, now: SystemTime) -> Result<Self, FetchError> {
        let loaded = match fs::read(&path) {
            Ok(bytes) => match parse_store_file(&bytes) {
                Ok(loaded) => loaded,
                Err(StoreParse::Version(found)) => {
                    return Err(FetchError::StoreVersion { path, found });
                }
                Err(StoreParse::Corrupt(what)) => set_aside_corrupt(&path, what),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Loaded::empty(),
            Err(err) => set_aside_corrupt(&path, format!("could not be read: {err}")),
        };
        let mut store = Self {
            path,
            seen: BoundedSet::new("propagated transient"),
            delivered: BoundedSet::new("delivered message"),
            deferred: BTreeMap::new(),
            cursor: loaded.cursor,
        };
        for (id, seen_at) in loaded.seen {
            store.seen.insert_at(id, seen_at, now);
        }
        for (id, delivered_at) in loaded.delivered {
            store.delivered.insert_at(id, delivered_at, now);
        }
        store.deferred.extend(loaded.deferred);
        store.evict_deferred();
        Ok(store)
    }

    pub(crate) fn contains(&self, id: &TransientId) -> bool {
        self.seen.contains(id)
    }

    /// Records `id` as processed at `now`, ending any deferral of it; `false` when it was
    /// already there, in which case its original `seen_at` stands, as the reference's cache
    /// keeps first sight (`LXMRouter.py:2323`).
    pub(crate) fn insert(&mut self, id: TransientId, now: SystemTime) -> bool {
        self.deferred.remove(&id);
        self.seen.insert_at(id, now, now)
    }

    pub(crate) fn was_delivered(&self, message_id: &[u8; 32]) -> bool {
        self.delivered.contains(message_id)
    }

    pub(crate) fn record_delivered(&mut self, message_id: [u8; 32], now: SystemTime) -> bool {
        self.delivered.insert_at(message_id, now, now)
    }

    /// Counts one more sighting of `id` without a key for its source. The first
    /// `MAX_UNKNOWN_SOURCE_DEFERRALS` sightings are deferred; the next spends the budget,
    /// and the caller records the id as processed instead.
    pub(crate) fn defer(&mut self, id: TransientId, now: SystemTime) -> Deferral {
        if let Some(entry) = self.deferred.get_mut(&id) {
            if entry.attempts < MAX_UNKNOWN_SOURCE_DEFERRALS {
                entry.attempts += 1;
                return Deferral::Retry {
                    attempts: entry.attempts,
                };
            }
            return Deferral::BudgetSpent;
        }
        self.deferred.insert(
            id,
            Deferred {
                attempts: 1,
                first_seen_at: now,
            },
        );
        self.evict_deferred();
        Deferral::Retry { attempts: 1 }
    }

    #[cfg(test)]
    pub(crate) fn deferral_of(&self, id: &TransientId) -> Option<Deferred> {
        self.deferred.get(id).copied()
    }

    fn evict_deferred(&mut self) {
        while self.deferred.len() > MAX_DEFERRED_IDS {
            let oldest = self
                .deferred
                .iter()
                .min_by_key(|(_, entry)| entry.first_seen_at)
                .map(|(id, _)| *id)
                .expect("a table over its capacity is not empty");
            self.deferred.remove(&oldest);
            debug!(
                "Forgot deferred transient {} to stay within {MAX_DEFERRED_IDS} deferred",
                short(&hex_lower(&oldest))
            );
        }
    }

    pub(crate) fn record_sync(&mut self, node: &str, now: SystemTime, received: usize) {
        self.cursor = Some(SyncCursor {
            node: node.to_string(),
            last_synced_at: now,
            last_received: received as u64,
        });
    }

    pub(crate) fn cursor(&self) -> Option<&SyncCursor> {
        self.cursor.as_ref()
    }

    /// How many transient ids are remembered as processed.
    pub(crate) fn len(&self) -> usize {
        self.seen.len()
    }

    pub(crate) fn persist(&self) -> Result<(), FetchError> {
        let file = StoreFile {
            version: PROPAGATION_STORE_VERSION,
            seen: self
                .seen
                .iter()
                .map(|(seen_at, id)| SeenRecord {
                    transient_id: hex_lower(id),
                    seen_at: rfc3339_utc(*seen_at),
                })
                .collect(),
            delivered: self
                .delivered
                .iter()
                .map(|(delivered_at, id)| DeliveredRecord {
                    message_id: hex_lower(id),
                    delivered_at: rfc3339_utc(*delivered_at),
                })
                .collect(),
            deferred: self
                .deferred
                .iter()
                .map(|(id, entry)| DeferredRecord {
                    transient_id: hex_lower(id),
                    attempts: entry.attempts,
                    first_seen_at: rfc3339_utc(entry.first_seen_at),
                })
                .collect(),
            cursor: self.cursor.as_ref().map(|cursor| CursorRecord {
                node: cursor.node.clone(),
                last_synced_at: rfc3339_utc(cursor.last_synced_at),
                last_received: cursor.last_received,
            }),
        };
        let json = serde_json::to_vec_pretty(&file)
            .map_err(|err| FetchError::Store(format!("serialize: {err}")))?;
        write_atomically(&self.path, &json).map_err(|err| FetchError::Store(format!("{err:#}")))
    }
}

fn is_past_horizon(recorded_at: SystemTime, now: SystemTime) -> bool {
    // A time in the future (clock stepped back) reads as just recorded, not as expired.
    now.duration_since(recorded_at).unwrap_or_default() >= DEDUP_HORIZON
}

/// Reads the version alone first so a file from a newer Coyote gets a precise refusal
/// rather than an unknown-field error from whatever the newer layout added.
fn parse_store_file(bytes: &[u8]) -> Result<Loaded, StoreParse> {
    let probe: VersionProbe = serde_json::from_slice(bytes)
        .map_err(|err| StoreParse::Corrupt(format!("has no readable version field: {err}")))?;
    if probe.version != PROPAGATION_STORE_VERSION {
        return Err(StoreParse::Version(probe.version));
    }
    let file: StoreFile = serde_json::from_slice(bytes)
        .map_err(|err| StoreParse::Corrupt(format!("is not valid JSON: {err}")))?;
    let seen = file
        .seen
        .iter()
        .map(|record| {
            Ok((
                hash_field(&record.transient_id, "transient id")?,
                time_field(&record.seen_at, "seen_at")?,
            ))
        })
        .collect::<Result<Vec<_>, StoreParse>>()?;
    let delivered = file
        .delivered
        .iter()
        .map(|record| {
            Ok((
                hash_field(&record.message_id, "message id")?,
                time_field(&record.delivered_at, "delivered_at")?,
            ))
        })
        .collect::<Result<Vec<_>, StoreParse>>()?;
    let deferred = file
        .deferred
        .iter()
        .map(|record| {
            Ok((
                hash_field(&record.transient_id, "transient id")?,
                Deferred {
                    attempts: record.attempts,
                    first_seen_at: time_field(&record.first_seen_at, "first_seen_at")?,
                },
            ))
        })
        .collect::<Result<Vec<_>, StoreParse>>()?;
    let cursor = file
        .cursor
        .map(|record| {
            let last_synced_at = time_field(&record.last_synced_at, "last_synced_at")?;
            Ok(SyncCursor {
                node: record.node,
                last_synced_at,
                last_received: record.last_received,
            })
        })
        .transpose()?;
    Ok(Loaded {
        seen,
        delivered,
        deferred,
        cursor,
    })
}

fn hash_field(text: &str, field: &str) -> Result<[u8; 32], StoreParse> {
    hash_from_hex(text).ok_or_else(|| {
        StoreParse::Corrupt(format!(
            "holds a {field} that is not 64 hex digits: '{}'",
            excerpt(text)
        ))
    })
}

fn time_field(text: &str, field: &str) -> Result<SystemTime, StoreParse> {
    parse_rfc3339(text).ok_or_else(|| {
        StoreParse::Corrupt(format!(
            "holds a {field} that is not RFC 3339: '{}'",
            excerpt(text)
        ))
    })
}

fn hash_from_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut id = [0u8; 32];
    for (byte, pair) in id.iter_mut().zip(text.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(id)
}

/// At most 80 characters of a value quoted from the file, so a corrupt file cannot flood
/// the log through its own contents.
fn excerpt(text: &str) -> String {
    const LIMIT: usize = 80;
    match text.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text.to_string(),
    }
}

/// Warns about a store that cannot be used, renames it to `<path>.corrupt` (replacing any
/// earlier one) and returns the empty state the fetch starts with. A failed rename is only
/// warned about; the file is cache and nothing downstream depends on the move.
fn set_aside_corrupt(path: &Path, what_happened: String) -> Loaded {
    let aside = path.with_extension("json.corrupt");
    warn!(
        "Propagation fetch state '{}' {what_happened}. Starting with an empty store; already fetched messages may be delivered again. The file is kept at '{}'.",
        path.display(),
        aside.display()
    );
    if let Err(err) = fs::rename(path, &aside) {
        warn!(
            "Failed to move the unusable propagation fetch state '{}' to '{}': {err}",
            path.display(),
            aside.display()
        );
    }
    Loaded::empty()
}

/// Why one fetched body went no further, in the order the checks run. `process` decides
/// from the verdict what the store remembers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Discard {
    /// Longer than `MAX_FETCHED_MESSAGE_BYTES`; refused before anything read it.
    Oversize { len: usize },
    /// Shorter than `MIN_FETCHED_MESSAGE_BYTES`; cannot hold a message.
    Undersize { len: usize },
    /// Its transient id, or once decrypted its message id, was already in the store.
    Duplicate,
    /// Not addressed to us, not decryptable to us, or not a message once decrypted.
    Undecryptable(String),
    /// No announced public key matches the `lxmf.delivery` hash it names as its source.
    /// Deferred rather than recorded: the node keeps the body for a later fetch.
    UnknownSource,
    /// `UnknownSource` once more after `MAX_UNKNOWN_SOURCE_DEFERRALS` deferrals; recorded
    /// and acknowledged like any other discard.
    UnknownSourceBudgetSpent,
    /// The signature does not verify against the key the claimed source announced.
    BadSignature,
    /// The signer is not on the trust list.
    UntrustedSource,
    /// The signer is blocked on the trust list.
    BlockedSource,
}

impl Discard {
    /// Whether the body's transient id is recorded as processed, making the node's next
    /// listing of it a `have`. A body refused on length was never read, a duplicate is
    /// recorded already, and an unknown source is deferred instead.
    fn is_recorded(&self) -> bool {
        !matches!(
            self,
            Self::Oversize { .. } | Self::Undersize { .. } | Self::Duplicate | Self::UnknownSource
        )
    }
}

impl fmt::Display for Discard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversize { len } => {
                write!(
                    f,
                    "{len} bytes, above the {MAX_FETCHED_MESSAGE_BYTES}-byte limit"
                )
            }
            Self::Undersize { len } => {
                write!(
                    f,
                    "{len} bytes, below the {MIN_FETCHED_MESSAGE_BYTES}-byte minimum"
                )
            }
            Self::Duplicate => write!(f, "already processed"),
            Self::Undecryptable(reason) => write!(f, "not a message for us: {reason}"),
            Self::UnknownSource => write!(f, "no public key known for the claimed source"),
            Self::UnknownSourceBudgetSpent => write!(
                f,
                "no public key known for the claimed source after {MAX_UNKNOWN_SOURCE_DEFERRALS} deferrals; giving up on it"
            ),
            Self::BadSignature => {
                write!(f, "signature does not verify against the claimed source")
            }
            Self::UntrustedSource => write!(f, "signer is not trusted"),
            Self::BlockedSource => write!(f, "signer is blocked"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BodyOutcome {
    Delivered,
    /// Left on the node for a later fetch: neither recorded nor acknowledged.
    Deferred {
        attempts: u8,
    },
    Discarded(Discard),
}

/// Turns the `lxmf.delivery` hash a message names as its source into the public key that
/// must have signed it. Async because the production lookup asks the transport.
#[async_trait]
pub(crate) trait SourceKeys: Send + Sync {
    async fn identity_for(&self, source: &AddressHash) -> Result<Option<Identity>, R3Error>;
}

/// Keys from the transport's announce cache. A reference LXMF peer announces
/// `lxmf.delivery` itself, so the source hash is looked up directly. A Coyote peer
/// announces its own destination instead, so the peer table's destinations are resolved
/// once, up front, to the identities behind them and indexed by the delivery hash each
/// derives; the table is bounded, so the index is.
struct TransportKeys<'a> {
    transport: &'a Transport,
    deadline: Deadline,
    by_delivery: HashMap<AddressHash, Identity>,
}

impl<'a> TransportKeys<'a> {
    async fn build(
        transport: &'a Transport,
        peers: &PeerTable,
        deadline: Deadline,
    ) -> Result<Self, R3Error> {
        let mut by_delivery = HashMap::new();
        for record in peers.snapshot() {
            let Some(hex) = canonical_hash(&record.destination_hash) else {
                continue;
            };
            let Ok(destination) = AddressHash::new_from_hex_string(&hex) else {
                continue;
            };
            if let Some(identity) = deadline
                .bound(GET_PATH, transport.destination_identity(&destination))
                .await?
            {
                by_delivery.insert(lxmf_delivery_hash(&identity), identity);
            }
        }
        Ok(Self {
            transport,
            deadline,
            by_delivery,
        })
    }
}

#[async_trait]
impl SourceKeys for TransportKeys<'_> {
    async fn identity_for(&self, source: &AddressHash) -> Result<Option<Identity>, R3Error> {
        if let Some(identity) = self
            .deadline
            .bound(GET_PATH, self.transport.destination_identity(source))
            .await?
        {
            return Ok(Some(identity));
        }
        Ok(self.by_delivery.get(source).copied())
    }
}

/// Everything one body is checked against. `process` runs the checks in a fixed order and
/// hands only what passes them all to the sink.
pub(crate) struct BodyPipeline<'a> {
    pub recipient: &'a CorePrivateIdentity,
    /// The recipient's `lxmf.delivery` hash, which every body for us starts with in
    /// cleartext.
    pub delivery: AddressHash,
    pub keys: &'a dyn SourceKeys,
    pub trust: &'a TrustStore,
    pub sink: &'a dyn InboundSink,
    /// The node the body came from, as the log lines name it.
    pub node: &'a str,
}

/// Length alone, before a byte of the body is read.
pub(crate) fn check_bounds(body: &[u8]) -> Result<(), Discard> {
    if body.len() > MAX_FETCHED_MESSAGE_BYTES {
        return Err(Discard::Oversize { len: body.len() });
    }
    if body.len() < MIN_FETCHED_MESSAGE_BYTES {
        return Err(Discard::Undersize { len: body.len() });
    }
    Ok(())
}

impl BodyPipeline<'_> {
    /// Bounds, dedups, decrypts, resolves and verifies the signer, consults the trust list,
    /// then delivers. Dedup runs before decryption as in the reference (`LXMRouter.py:2319`)
    /// on the transient id, and again on the message id once decrypted (`:1799-1803`). The
    /// verdict decides what the store remembers: a delivered or discarded body's transient
    /// id is recorded (`:2323`) so it is not tried again, except one refused on length,
    /// which was never read; a body whose source has no known key is deferred instead and
    /// left unrecorded, so the node keeps serving it until a key turns up or the deferral
    /// budget is spent. The signature is checked before the trust list is consulted, so the
    /// trust list is only ever asked about a proven signer. Only a transport wait that
    /// expires is an error; every verdict about the body is an outcome.
    pub(crate) async fn process(
        &self,
        body: &[u8],
        store: &mut FetchStore,
        now: SystemTime,
    ) -> Result<BodyOutcome, R3Error> {
        let transient_id = transient_id_of(body);
        match self.screen(body, &transient_id, store).await? {
            Ok(message) => {
                store.insert(transient_id, now);
                store.record_delivered(message.message_id, now);
                self.sink.deliver(message);
                Ok(BodyOutcome::Delivered)
            }
            Err(Discard::UnknownSource) => match store.defer(transient_id, now) {
                Deferral::Retry { attempts } => {
                    debug!(
                        "Propagation fetch from {}: deferred transient {}: {}, sighting {attempts} of {MAX_UNKNOWN_SOURCE_DEFERRALS}",
                        self.node,
                        short(&hex_lower(&transient_id)),
                        Discard::UnknownSource
                    );
                    Ok(BodyOutcome::Deferred { attempts })
                }
                Deferral::BudgetSpent => {
                    Ok(self.discard(transient_id, Discard::UnknownSourceBudgetSpent, store, now))
                }
            },
            Err(discard) => Ok(self.discard(transient_id, discard, store, now)),
        }
    }

    fn discard(
        &self,
        transient_id: TransientId,
        discard: Discard,
        store: &mut FetchStore,
        now: SystemTime,
    ) -> BodyOutcome {
        if discard.is_recorded() {
            store.insert(transient_id, now);
        }
        debug!(
            "Propagation fetch from {}: discarded transient {}: {discard}",
            self.node,
            short(&hex_lower(&transient_id))
        );
        BodyOutcome::Discarded(discard)
    }

    /// Every check up to the verdict. Reads the store, never writes it.
    async fn screen(
        &self,
        body: &[u8],
        transient_id: &TransientId,
        store: &FetchStore,
    ) -> Result<Result<InboundMessage, Discard>, R3Error> {
        if let Err(discard) = check_bounds(body) {
            return Ok(Err(discard));
        }
        if store.contains(transient_id) {
            return Ok(Err(Discard::Duplicate));
        }
        if body[..self.delivery.as_slice().len()] != *self.delivery.as_slice() {
            return Ok(Err(Discard::Undecryptable(
                "destination is not ours".to_string(),
            )));
        }
        let wire = match WireMessage::unpack_paper(body, self.recipient) {
            Ok(wire) => wire,
            Err(err) => return Ok(Err(Discard::Undecryptable(err.to_string()))),
        };
        let message_id = match wire.try_message_id() {
            Ok(id) => id,
            Err(err) => return Ok(Err(Discard::Undecryptable(err.to_string()))),
        };
        if store.was_delivered(&message_id) {
            return Ok(Err(Discard::Duplicate));
        }
        let source = AddressHash::new(wire.source);
        let Some(identity) = self.keys.identity_for(&source).await? else {
            return Ok(Err(Discard::UnknownSource));
        };
        if wire.verify(&to_core_identity(&identity)) != Ok(true) {
            return Ok(Err(Discard::BadSignature));
        }
        let identity_hex = identity.address_hash.to_hex_string();
        match self.trust.identity_standing(&identity_hex) {
            IdentityStanding::Unknown => return Ok(Err(Discard::UntrustedSource)),
            IdentityStanding::Blocked => return Ok(Err(Discard::BlockedSource)),
            IdentityStanding::Trusted { .. } => {}
        }
        let stamp_value = invalid_stamp_value(
            wire.payload.stamp.as_deref().map(Vec::as_slice),
            &message_id,
        );
        debug!(
            "Propagation fetch from {}: transient {} stamp cost demanded {REQUIRED_DELIVERY_STAMP_COST} supplied {}",
            self.node,
            short(&hex_lower(transient_id)),
            stamp_value.map_or_else(|| "none".to_string(), |value| value.to_string())
        );
        Ok(Ok(InboundMessage {
            transient_id: *transient_id,
            message_id,
            source_identity_hash: identity_hex,
            source_delivery_hash: source.to_hex_string(),
            timestamp: wire.payload.timestamp,
            title: wire.payload.title.map(|bytes| bytes.into_vec()),
            content: wire.payload.content.map(|bytes| bytes.into_vec()),
            fields: wire.payload.fields,
            stamp_value,
        }))
    }
}

/// `wait` on the node, unless `cancel` fires first. Only the waits on the node are
/// cancellable: the body loop between rounds 2 and 3 runs to its end, so what it records
/// is persisted however the fetch ends. Stopping the node cancels this token and fails the
/// request client's pending table in the same instant, so the token is polled first: a
/// stopped fetch always reads as `Cancelled`, never as the request client's own shutdown.
async fn unless_cancelled<T>(
    cancel: &CancellationToken,
    wait: impl Future<Output = Result<T, R3Error>>,
    failed: impl Fn(R3Error) -> FetchError,
) -> Result<T, FetchError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(FetchError::Cancelled),
        outcome = wait => outcome.map_err(failed),
    }
}

/// One sync with `node`: link, identify once, then the three rounds on that link. The
/// store is loaded by the caller and persisted here once the bodies are processed, before
/// round 3 goes out and whether or not the processing completed, so a crash or error
/// between the two leaves the ids remembered and the node still holding the bodies, and
/// the next fetch purges them as `haves` instead of receiving them again. `cancel` firing
/// abandons the fetch at whichever wait on the node it is in, with `Cancelled`; between
/// those waits it runs on, so what was recorded always reaches the disk.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn fetch(
    transport: &Transport,
    client: &R3Client,
    identity: &TransportIdentity,
    node: &PropagationNode,
    peers: &PeerTable,
    trust: &TrustStore,
    store: &mut FetchStore,
    sink: &dyn InboundSink,
    options: &FetchOptions,
    cancel: CancellationToken,
) -> Result<FetchReport, FetchError> {
    let node_hex = node.destination.address_hash.to_hex_string();
    let node_short = short(&node_hex);
    let identity_hex = identity.as_identity().address_hash.to_hex_string();
    let failed = |err: R3Error| link_error(err, node_short, &identity_hex);
    let mut report = FetchReport::new(node_hex.clone());

    if let Some(cursor) = store.cursor() {
        debug!(
            "Propagation fetch from {node_short}: last synced with {} at {} receiving {}, {} transient ids remembered",
            short(&cursor.node),
            rfc3339_utc(cursor.last_synced_at),
            cursor.last_received,
            store.len()
        );
    }

    let link = unless_cancelled(
        &cancel,
        link_to(
            transport,
            identity,
            &node.destination,
            GET_PATH,
            options.link_timeout,
        ),
        failed,
    )
    .await?;
    debug!(
        "Propagation fetch from {node_short}: identified as {}, round 1 list requested",
        short(&identity_hex)
    );
    let listed = unless_cancelled(
        &cancel,
        client.request_on_link(
            transport,
            &link,
            GET_PATH,
            list_request(),
            Deadline::after(options.request_timeout),
        ),
        failed,
    )
    .await?;
    let ids = parse_listed_ids(listed.value)?;
    report.listed = ids.len();
    if ids.is_empty() {
        debug!("Propagation fetch from {node_short}: listed 0, nothing to fetch");
        store.record_sync(&node_hex, SystemTime::now(), 0);
        store.persist()?;
        return Ok(report);
    }

    let mut wants = Vec::new();
    let mut haves = Vec::new();
    for id in &ids {
        if store.contains(id) {
            haves.push(*id);
        } else if wants.len() < MAX_WANTS_PER_FETCH {
            wants.push(*id);
        }
    }
    report.wanted = wants.len();
    debug!(
        "Propagation fetch from {node_short}: listed {}, wanting {}, have {}",
        ids.len(),
        wants.len(),
        haves.len()
    );
    debug!(
        "Propagation fetch from {node_short}: round 2 requested {} bodies within {FETCH_TRANSFER_LIMIT_KB} kB",
        wants.len()
    );
    let served = unless_cancelled(
        &cancel,
        client.request_on_link(
            transport,
            &link,
            GET_PATH,
            get_request(&wants, &haves),
            Deadline::after(options.request_timeout),
        ),
        failed,
    )
    .await?;
    report.response_branch = Some(served.response_branch);
    let bodies = parse_bodies(served.value)?;
    report.received = bodies.len();
    debug!(
        "Propagation fetch from {node_short}: received {} bodies ({} bytes, {:?})",
        bodies.len(),
        bodies.iter().map(Vec::len).sum::<usize>(),
        served.response_branch
    );
    if bodies.is_empty() {
        store.record_sync(&node_hex, SystemTime::now(), 0);
        store.persist()?;
        return Ok(report);
    }

    let keys = TransportKeys::build(transport, peers, Deadline::after(options.request_timeout))
        .await
        .map_err(failed)?;
    let recipient = to_core_private_identity(identity);
    let pipeline = BodyPipeline {
        recipient: &recipient,
        delivery: lxmf_delivery_hash(identity.as_identity()),
        keys: &keys,
        trust,
        sink,
        node: node_short,
    };
    let acks =
        process_and_persist(&pipeline, &bodies, store, &mut report, &node_hex, failed).await?;

    debug!(
        "Propagation fetch from {node_short}: round 3 acknowledging {} bodies, leaving {} deferred on the node",
        acks.len(),
        report.deferred
    );
    unless_cancelled(
        &cancel,
        client.request_on_link(
            transport,
            &link,
            GET_PATH,
            ack_request(&acks),
            Deadline::after(options.request_timeout),
        ),
        failed,
    )
    .await?;
    report.acknowledged = acks.len();
    debug!(
        "Propagation fetch from {node_short}: round 3 acknowledged {}; delivered {}, duplicates {}, discarded {}, deferred {}",
        report.acknowledged, report.delivered, report.duplicates, report.discarded, report.deferred
    );
    Ok(report)
}

/// Runs every received body through the pipeline, tallying the outcomes into `report`, and
/// persists the store before the outcome is looked at, so what the loop recorded survives
/// a transport error inside it; the sync is only recorded when the loop completed. Returns
/// the ids round 3 tells the node to delete: every body but the deferred ones, which the
/// node keeps for a later fetch, each id once however often it was served.
async fn process_and_persist(
    pipeline: &BodyPipeline<'_>,
    bodies: &[Vec<u8>],
    store: &mut FetchStore,
    report: &mut FetchReport,
    node_hex: &str,
    failed: impl Fn(R3Error) -> FetchError,
) -> Result<Vec<TransientId>, FetchError> {
    let processed = process_bodies(pipeline, bodies, store, report).await;
    if processed.is_ok() {
        store.record_sync(node_hex, SystemTime::now(), bodies.len());
    }
    store.persist()?;
    processed.map_err(failed)
}

async fn process_bodies(
    pipeline: &BodyPipeline<'_>,
    bodies: &[Vec<u8>],
    store: &mut FetchStore,
    report: &mut FetchReport,
) -> Result<Vec<TransientId>, R3Error> {
    let mut acks = Vec::with_capacity(bodies.len());
    for body in bodies {
        match pipeline.process(body, store, SystemTime::now()).await? {
            BodyOutcome::Delivered => report.delivered += 1,
            BodyOutcome::Deferred { .. } => {
                report.deferred += 1;
                continue;
            }
            BodyOutcome::Discarded(Discard::Duplicate) => report.duplicates += 1,
            BodyOutcome::Discarded(_) => report.discarded += 1,
        }
        let id = transient_id_of(body);
        if !acks.contains(&id) {
            acks.push(id);
        }
    }
    Ok(acks)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::peers::PeerSighting;
    use crate::mesh::r3::RequestFrame;
    use crate::mesh::test_support::{TempDir, TrustList, rust_sources};
    use crate::testing::{debug_snapshot, install_log_collector, warn_snapshot};

    use lxmf_core::message::Payload;
    use lxmf_core::stamp::generate_stamp;
    use parking_lot::Mutex;
    use rand_core::OsRng;
    use rns_transport::identity_bridge::to_transport_identity;
    use rns_transport::transport::TransportConfig;
    use std::sync::Arc;

    fn packed(value: &Value) -> Vec<u8> {
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, value).unwrap();
        bytes
    }

    fn id(byte: u8) -> TransientId {
        [byte; 32]
    }

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// `bin 32` of `id`, as msgpack writes a 32-byte binary.
    fn bin32(id: &TransientId) -> Vec<u8> {
        let mut out = vec![0xc4, 0x20];
        out.extend_from_slice(id);
        out
    }

    /// The reference's round 2 body for `wants`, `haves` and the default limit, hand-packed.
    fn expected_get(wants: &[TransientId], haves: &[TransientId]) -> Vec<u8> {
        let mut out = vec![0x93];
        for ids in [wants, haves] {
            assert!(ids.len() < 16, "fixarray only");
            out.push(0x90 | ids.len() as u8);
            for id in ids {
                out.extend(bin32(id));
            }
        }
        out.extend([0xcc, 0xf0]);
        out
    }

    /// The reference's round 3 body for `haves`, hand-packed.
    fn expected_ack(haves: &[TransientId]) -> Vec<u8> {
        assert!(haves.len() < 16, "fixarray only");
        let mut out = vec![0x92, 0xc0, 0x90 | haves.len() as u8];
        for id in haves {
            out.extend(bin32(id));
        }
        out
    }

    #[test]
    fn request_bodies_match_the_reference_bytes() {
        assert_eq!(packed(&list_request()), [0x92, 0xc0, 0xc0]);

        let mut round2 = vec![0x93, 0x91];
        round2.extend(bin32(&id(0xaa)));
        round2.push(0x91);
        round2.extend(bin32(&id(0xbb)));
        round2.extend([0xcc, 0xf0]);
        assert_eq!(packed(&get_request(&[id(0xaa)], &[id(0xbb)])), round2);
        assert_eq!(round2, expected_get(&[id(0xaa)], &[id(0xbb)]));
        assert_eq!(
            FETCH_TRANSFER_LIMIT_KB, 240,
            "the trailing cc f0 is the limit"
        );

        let mut round3 = vec![0x92, 0xc0, 0x91];
        round3.extend(bin32(&id(0xbb)));
        assert_eq!(packed(&ack_request(&[id(0xbb)])), round3);
        assert_eq!(round3, expected_ack(&[id(0xbb)]));

        // Empty wants with haves still goes out, so the node purges the haves.
        let mut purge_only = vec![0x93, 0x90, 0x91];
        purge_only.extend(bin32(&id(0xcc)));
        purge_only.extend([0xcc, 0xf0]);
        assert_eq!(packed(&get_request(&[], &[id(0xcc)])), purge_only);
    }

    #[test]
    fn request_frame_around_a_round_is_time_path_hash_and_body() {
        let frame = RequestFrame::new(GET_PATH, list_request()).encode();
        assert_eq!(frame.len(), 1 + 9 + 18 + 3);
        assert_eq!(frame[0], 0x93);
        assert_eq!(frame[1], 0xcb);
        assert_eq!(&frame[10..12], &[0xc4, 0x10]);
        assert_eq!(&frame[12..28], &Sha256::digest(b"/get")[..16]);
        assert_eq!(&frame[28..], &[0x92, 0xc0, 0xc0]);
    }

    fn list_error(value: Value) -> String {
        match parse_listed_ids(value) {
            Ok(ids) => panic!("expected a malformed list, parsed {} ids", ids.len()),
            Err(FetchError::MalformedList(reason)) => reason,
            Err(other) => panic!("expected MalformedList, got {other:?}"),
        }
    }

    #[test]
    fn listed_ids_refuse_anything_but_32_byte_binaries() {
        assert_eq!(list_error(Value::Nil), "not an array (nil)");
        assert_eq!(list_error(Value::from(7)), "not an array (an integer)");
        assert_eq!(
            list_error(Value::Binary(vec![0; 32])),
            "not an array (binary)"
        );
        assert_eq!(
            list_error(Value::Array(vec![
                Value::Binary(id(1).to_vec()),
                Value::from("x"),
            ])),
            "transient id 1 is a string, expected binary"
        );
        assert_eq!(
            list_error(Value::Array(vec![Value::Binary(vec![0; 31])])),
            "transient id 0 is 31 bytes, expected 32"
        );
        assert_eq!(
            list_error(Value::Array(vec![Value::Binary(vec![0; 33])])),
            "transient id 0 is 33 bytes, expected 32"
        );
        assert_eq!(
            parse_listed_ids(Value::Array(vec![])).unwrap(),
            Vec::<TransientId>::new()
        );
    }

    #[test]
    fn listed_ids_beyond_the_cap_are_left_for_the_next_fetch() {
        install_log_collector();
        let mut items: Vec<Value> = (0..MAX_LISTED_IDS + 5)
            .map(|i| {
                let mut id = [0u8; 32];
                id[..8].copy_from_slice(&(i as u64).to_be_bytes());
                Value::Binary(id.to_vec())
            })
            .collect();
        // Past the cap nothing is looked at, so a malformed entry there is not an error.
        items.push(Value::from("junk"));

        let ids = parse_listed_ids(Value::Array(items)).unwrap();

        assert_eq!(ids.len(), MAX_LISTED_IDS);
        assert_eq!(&ids[0][..8], &0u64.to_be_bytes());
        assert_eq!(
            &ids[MAX_LISTED_IDS - 1][..8],
            &((MAX_LISTED_IDS - 1) as u64).to_be_bytes()
        );
        let capped = format!(
            "Propagation node listed {} transient ids; considering the first {MAX_LISTED_IDS}",
            MAX_LISTED_IDS + 6
        );
        assert!(
            debug_snapshot().iter().any(|line| line == &capped),
            "{capped:?}"
        );
    }

    #[test]
    fn bodies_refuse_anything_but_binaries_and_cap_at_the_wants() {
        assert!(matches!(
            parse_bodies(Value::Nil),
            Err(FetchError::MalformedBodies(reason)) if reason == "not an array (nil)"
        ));
        assert!(matches!(
            parse_bodies(Value::Array(vec![Value::Binary(vec![1]), Value::Nil])),
            Err(FetchError::MalformedBodies(reason)) if reason == "body 1 is nil, expected binary"
        ));
        let bodies = parse_bodies(Value::Array(
            (0..MAX_WANTS_PER_FETCH + 3)
                .map(|i| Value::Binary(vec![i as u8; 3]))
                .collect(),
        ))
        .unwrap();
        assert_eq!(bodies.len(), MAX_WANTS_PER_FETCH);
        assert_eq!(bodies[0], vec![0; 3]);
        assert_eq!(
            bodies[MAX_WANTS_PER_FETCH - 1],
            vec![(MAX_WANTS_PER_FETCH - 1) as u8; 3]
        );
    }

    #[test]
    fn bounds_leave_room_under_the_transport_and_response_caps() {
        const {
            assert!(MAX_FETCHED_MESSAGE_BYTES <= 64 * 1024 * 1024 / 64);
            assert!(MAX_FETCHED_MESSAGE_BYTES < MAX_R3_PAYLOAD_BYTES);
            assert!(
                FETCH_TRANSFER_LIMIT_KB as usize * 1000 + 24 + 16 * MAX_WANTS_PER_FETCH
                    <= MAX_R3_PAYLOAD_BYTES
            );
        }
        assert_eq!(MIN_FETCHED_MESSAGE_BYTES, 16 + 16 + 64 + 8 + 8);
        assert_eq!(DEDUP_HORIZON, Duration::from_secs(30 * 24 * 60 * 60 * 6));
    }

    /// Keys for whichever delivery hashes a test registers, with no transport behind them.
    #[derive(Default)]
    struct MapKeys(HashMap<AddressHash, Identity>);

    impl MapKeys {
        fn know(&mut self, identity: &Identity) {
            self.0.insert(lxmf_delivery_hash(identity), *identity);
        }
    }

    #[async_trait]
    impl SourceKeys for MapKeys {
        async fn identity_for(&self, source: &AddressHash) -> Result<Option<Identity>, R3Error> {
            Ok(self.0.get(source).copied())
        }
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
            assert_eq!(messages.len(), 1, "exactly one delivery");
            messages[0].clone()
        }
    }

    impl InboundSink for CountingSink {
        fn deliver(&self, message: InboundMessage) {
            self.messages.lock().push(message);
        }
    }

    fn identity_hex(identity: &Identity) -> String {
        identity.address_hash.to_hex_string()
    }

    fn transport_identity_of(core: &CorePrivateIdentity) -> Identity {
        to_transport_identity(core.as_identity())
    }

    /// An unsigned message to `recipient` naming `claimed_source` as its source, with the
    /// fixed timestamp, title and fields every test body shares.
    fn wire_to(
        claimed_source: &AddressHash,
        recipient: &Identity,
        content: &[u8],
        stamp: Option<Vec<u8>>,
    ) -> WireMessage {
        let mut source = [0u8; 16];
        source.copy_from_slice(claimed_source.as_slice());
        let mut destination = [0u8; 16];
        destination.copy_from_slice(lxmf_delivery_hash(recipient).as_slice());
        WireMessage::new(
            destination,
            source,
            Payload::new(
                1_700_000_000.5,
                Some(content.to_vec()),
                Some(b"hello".to_vec()),
                Some(Value::Map(vec![])),
                stamp,
            ),
        )
    }

    /// `wire` as the node serves it: `destination hash || encrypted message`, the
    /// propagation stamp already stripped, signed by `signer`.
    fn seal(mut wire: WireMessage, signer: &CorePrivateIdentity, recipient: &Identity) -> Vec<u8> {
        wire.sign(signer).unwrap();
        wire.pack_propagation_transient_with_rng(&to_core_identity(recipient), OsRng)
            .unwrap()
            .0
    }

    /// A body signed by `signer` and naming `claimed_source` (normally the signer's own
    /// delivery hash) as its source.
    fn body_from(
        signer: &CorePrivateIdentity,
        claimed_source: &AddressHash,
        recipient: &Identity,
        content: &[u8],
    ) -> Vec<u8> {
        seal(
            wire_to(claimed_source, recipient, content, None),
            signer,
            recipient,
        )
    }

    /// An honest body carrying a delivery stamp mined to `cost`, with the message id the
    /// stamp was mined for and the stamp itself. The message id leaves the stamp out, so
    /// it can be taken from the unstamped message.
    fn stamped_body(
        signer: &CorePrivateIdentity,
        recipient: &Identity,
        content: &[u8],
        cost: u32,
    ) -> (Vec<u8>, [u8; 32], Vec<u8>) {
        let source = lxmf_delivery_hash(&transport_identity_of(signer));
        let message_id = wire_to(&source, recipient, content, None)
            .try_message_id()
            .unwrap();
        let stamp = generate_stamp(&message_id, cost).expect("a tiny cost is mineable");
        let body = seal(
            wire_to(&source, recipient, content, Some(stamp.clone())),
            signer,
            recipient,
        );
        (body, message_id, stamp)
    }

    fn honest_body(signer: &CorePrivateIdentity, recipient: &Identity, content: &[u8]) -> Vec<u8> {
        body_from(
            signer,
            &lxmf_delivery_hash(&transport_identity_of(signer)),
            recipient,
            content,
        )
    }

    /// One recipient, a store and a sink, with whichever trust list and keys a test wants.
    struct Bench {
        recipient: CorePrivateIdentity,
        keys: MapKeys,
        trust: Arc<TrustStore>,
        sink: CountingSink,
        store: FetchStore,
        _tmp: TempDir,
    }

    impl Bench {
        fn new(tag: &str, list: TrustList) -> Self {
            let (trust, tmp) = list.open(tag);
            let store =
                FetchStore::load(tmp.path.join("propagation.json"), SystemTime::now()).unwrap();
            Self {
                recipient: CorePrivateIdentity::new_from_rand(OsRng),
                keys: MapKeys::default(),
                trust,
                sink: CountingSink::default(),
                store,
                _tmp: tmp,
            }
        }

        fn me(&self) -> Identity {
            transport_identity_of(&self.recipient)
        }

        async fn process(&mut self, body: &[u8]) -> BodyOutcome {
            let pipeline = BodyPipeline {
                recipient: &self.recipient,
                delivery: lxmf_delivery_hash(&self.me()),
                keys: &self.keys,
                trust: &self.trust,
                sink: &self.sink,
                node: "fake",
            };
            pipeline
                .process(body, &mut self.store, SystemTime::now())
                .await
                .unwrap()
        }
    }

    fn discarded(outcome: BodyOutcome) -> Discard {
        match outcome {
            BodyOutcome::Discarded(discard) => discard,
            BodyOutcome::Delivered => panic!("expected a discard, the body was delivered"),
            BodyOutcome::Deferred { attempts } => panic!("expected a discard, deferred {attempts}"),
        }
    }

    #[tokio::test]
    async fn garbage_bodies_are_discarded_in_bound_order_and_never_reach_the_sink() {
        let sender = CorePrivateIdentity::new_from_rand(OsRng);
        let mut bench = Bench::new("fetch-garbage", TrustList::default());
        bench.keys.know(&transport_identity_of(&sender));

        assert_eq!(
            discarded(bench.process(&[]).await),
            Discard::Undersize { len: 0 }
        );
        assert_eq!(
            discarded(bench.process(&[1]).await),
            Discard::Undersize { len: 1 }
        );
        assert_eq!(
            discarded(bench.process(&[0; 111]).await),
            Discard::Undersize { len: 111 }
        );
        // The first 16 bytes are the cleartext destination; a body not addressed to us is
        // refused on them alone, before any decryption is attempted.
        let not_ours = Discard::Undecryptable("destination is not ours".to_string());
        assert_eq!(discarded(bench.process(&[0; 112]).await), not_ours);
        let noise: Vec<u8> = (0..500u32).map(|i| (i * 7 + 13) as u8).collect();
        assert_eq!(discarded(bench.process(&noise).await), not_ours);
        let someone_else = transport_identity_of(&CorePrivateIdentity::new_from_rand(OsRng));
        let foreign = honest_body(&sender, &someone_else, b"not for you");
        assert_eq!(discarded(bench.process(&foreign).await), not_ours);
        let mut flipped = honest_body(&sender, &bench.me(), b"flip me");
        let middle = flipped.len() / 2;
        flipped[middle] ^= 0x01;
        let flipped_verdict = discarded(bench.process(&flipped).await);
        assert!(matches!(flipped_verdict, Discard::Undecryptable(_)));
        assert_ne!(
            flipped_verdict, not_ours,
            "addressed to us, so the failure is the decryption itself"
        );
        let mut readdressed = honest_body(&sender, &bench.me(), b"readdressed");
        readdressed[0] ^= 0x01;
        assert_eq!(discarded(bench.process(&readdressed).await), not_ours);
        assert_eq!(
            discarded(bench.process(&[0; 112]).await),
            Discard::Duplicate,
            "the transient id is checked before the destination"
        );
        let oversize = vec![0u8; MAX_FETCHED_MESSAGE_BYTES + 1];
        assert_eq!(
            discarded(bench.process(&oversize).await),
            Discard::Oversize {
                len: MAX_FETCHED_MESSAGE_BYTES + 1
            }
        );
        assert_eq!(
            check_bounds(&oversize),
            Err(Discard::Oversize {
                len: oversize.len()
            })
        );
        assert_eq!(check_bounds(&[0; MAX_FETCHED_MESSAGE_BYTES]), Ok(()));

        assert_eq!(bench.sink.count(), 0);
        // Bodies refused on length were never hashed into the store; the rest were, before
        // decryption, so a second sighting is a duplicate whatever it decrypts to.
        assert!(!bench.store.contains(&transient_id_of(&oversize)));
        assert!(!bench.store.contains(&transient_id_of(&[0; 111])));
        assert!(bench.store.contains(&transient_id_of(&[0; 112])));
        assert!(bench.store.contains(&transient_id_of(&noise)));
        assert!(bench.store.contains(&transient_id_of(&foreign)));
        assert!(bench.store.contains(&transient_id_of(&flipped)));
        assert_eq!(discarded(bench.process(&noise).await), Discard::Duplicate);
        assert_eq!(bench.store.len(), 5);
    }

    #[tokio::test]
    async fn a_message_signed_by_a_but_claiming_b_is_discarded_before_trust_is_consulted() {
        let a = CorePrivateIdentity::new_from_rand(OsRng);
        let b = CorePrivateIdentity::new_from_rand(OsRng);
        let (a_id, b_id) = (transport_identity_of(&a), transport_identity_of(&b));
        // B is trusted, A is not: were trust consulted for the claimed source, or before
        // the signature, the forged message would go through.
        let mut bench = Bench::new(
            "fetch-forged-source",
            TrustList::default().identity(&identity_hex(&b_id), true),
        );
        bench.keys.know(&a_id);
        bench.keys.know(&b_id);
        let me = bench.me();

        let forged = body_from(&a, &lxmf_delivery_hash(&b_id), &me, b"from b, honestly");
        assert_eq!(
            discarded(bench.process(&forged).await),
            Discard::BadSignature
        );
        assert_eq!(bench.sink.count(), 0);

        // Controls: the same forgery with B's key unknown stops one step earlier and is
        // deferred rather than discarded; A's own honest message fails on trust, B's goes
        // through.
        bench.keys.0.remove(&lxmf_delivery_hash(&b_id));
        let forged_again = body_from(&a, &lxmf_delivery_hash(&b_id), &me, b"again");
        assert_eq!(
            bench.process(&forged_again).await,
            BodyOutcome::Deferred { attempts: 1 }
        );
        assert!(!bench.store.contains(&transient_id_of(&forged_again)));
        bench.keys.know(&b_id);
        assert_eq!(
            discarded(bench.process(&honest_body(&a, &me, b"a speaks")).await),
            Discard::UntrustedSource
        );
        assert_eq!(bench.sink.count(), 0);
        assert_eq!(
            bench.process(&honest_body(&b, &me, b"b speaks")).await,
            BodyOutcome::Delivered
        );
        let delivered = bench.sink.only();
        assert_eq!(delivered.source_identity_hash, identity_hex(&b_id));
        assert_eq!(
            delivered.source_delivery_hash,
            lxmf_delivery_hash(&b_id).to_hex_string()
        );
        assert_eq!(delivered.content.as_deref(), Some(&b"b speaks"[..]));
        assert_eq!(delivered.title.as_deref(), Some(&b"hello"[..]));
        assert_eq!(delivered.fields, Some(Value::Map(vec![])));
        assert_eq!(delivered.timestamp, 1_700_000_000.5);
        assert_eq!(delivered.stamp_value, None);
    }

    #[tokio::test]
    async fn an_unknown_source_is_deferred_three_times_then_given_up_on() {
        install_log_collector();
        let sender = CorePrivateIdentity::new_from_rand(OsRng);
        let sender_id = transport_identity_of(&sender);
        let mut bench = Bench::new(
            "fetch-deferred",
            TrustList::default().identity(&identity_hex(&sender_id), true),
        );
        let me = bench.me();
        let body = honest_body(&sender, &me, b"early");
        let id = transient_id_of(&body);

        assert_eq!(
            bench.process(&body).await,
            BodyOutcome::Deferred { attempts: 1 }
        );
        assert!(
            !bench.store.contains(&id),
            "a deferred body is not recorded"
        );
        assert_eq!(bench.store.deferral_of(&id).map(|d| d.attempts), Some(1));
        let deferred_line = format!(
            "Propagation fetch from fake: deferred transient {}: no public key known for the claimed source, sighting 1 of {MAX_UNKNOWN_SOURCE_DEFERRALS}",
            short(&hex_lower(&id))
        );
        assert!(
            debug_snapshot().iter().any(|line| line == &deferred_line),
            "{deferred_line:?}"
        );
        for attempts in 2..=MAX_UNKNOWN_SOURCE_DEFERRALS {
            assert_eq!(
                bench.process(&body).await,
                BodyOutcome::Deferred { attempts }
            );
            assert!(!bench.store.contains(&id));
        }
        assert_eq!(
            bench.store.deferral_of(&id).map(|d| d.attempts),
            Some(MAX_UNKNOWN_SOURCE_DEFERRALS)
        );

        let spent = discarded(bench.process(&body).await);
        assert_eq!(spent, Discard::UnknownSourceBudgetSpent);
        assert!(spent.to_string().contains("after 3 deferrals"), "{spent}");
        assert!(bench.store.contains(&id), "the fourth sighting is recorded");
        assert_eq!(bench.store.deferral_of(&id), None);
        assert_eq!(bench.sink.count(), 0);
        // Once recorded, the key arriving no longer helps: the id is a duplicate.
        bench.keys.know(&sender_id);
        assert_eq!(discarded(bench.process(&body).await), Discard::Duplicate);

        // A body deferred once and then resolvable is delivered and leaves the table.
        bench.keys.0.clear();
        let later = honest_body(&sender, &me, b"later");
        let later_id = transient_id_of(&later);
        assert_eq!(
            bench.process(&later).await,
            BodyOutcome::Deferred { attempts: 1 }
        );
        bench.keys.know(&sender_id);
        assert_eq!(bench.process(&later).await, BodyOutcome::Delivered);
        assert_eq!(bench.sink.only().content, Some(b"later".to_vec()));
        assert!(bench.store.contains(&later_id));
        assert_eq!(
            bench.store.deferral_of(&later_id),
            None,
            "recording the id ends its deferral"
        );
    }

    #[test]
    fn deferrals_evict_the_longest_deferred_past_capacity_and_log_it() {
        install_log_collector();
        let tmp = TempDir::new("fetch-deferred-cap");
        let mut store = store_at(&tmp, t(1));
        let mut ids = Vec::new();
        for i in 0..MAX_DEFERRED_IDS {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&(i as u64).to_be_bytes());
            assert_eq!(
                store.defer(id, t(1_000 + i as u64)),
                Deferral::Retry { attempts: 1 }
            );
            ids.push(id);
        }
        // A repeat keeps its first sighting, so it is still the oldest.
        assert_eq!(
            store.defer(ids[0], t(9_000)),
            Deferral::Retry { attempts: 2 }
        );
        assert_eq!(
            store.deferral_of(&ids[0]).map(|d| d.first_seen_at),
            Some(t(1_000))
        );

        assert_eq!(
            store.defer(id(0xff), t(9_001)),
            Deferral::Retry { attempts: 1 }
        );

        assert_eq!(store.deferral_of(&ids[0]), None);
        assert!(store.deferral_of(&ids[1]).is_some());
        assert!(store.deferral_of(&id(0xff)).is_some());
        let evicted = format!(
            "Forgot deferred transient {} to stay within {MAX_DEFERRED_IDS} deferred",
            short(&hex_lower(&ids[0]))
        );
        assert!(
            debug_snapshot().iter().any(|line| line == &evicted),
            "{evicted:?}"
        );
        assert_eq!(store.len(), 0, "deferrals are not processed ids");
    }

    #[tokio::test]
    async fn junk_filling_the_transient_set_cannot_make_a_delivered_message_deliverable_again() {
        let sender = CorePrivateIdentity::new_from_rand(OsRng);
        let sender_id = transport_identity_of(&sender);
        let mut bench = Bench::new(
            "fetch-delivered-set",
            TrustList::default().identity(&identity_hex(&sender_id), true),
        );
        bench.keys.know(&sender_id);
        let body = honest_body(&sender, &bench.me(), b"once");
        assert_eq!(bench.process(&body).await, BodyOutcome::Delivered);
        let message_id = bench.sink.only().message_id;
        assert!(bench.store.was_delivered(&message_id));

        // Every insert is a distinct junk id newer than the delivery, so the whole
        // transient window turns over and the body's own transient id is forgotten.
        for i in 0..DEDUP_CAPACITY as u64 {
            let mut junk = [0xeeu8; 32];
            junk[..8].copy_from_slice(&i.to_be_bytes());
            assert!(
                bench
                    .store
                    .insert(junk, SystemTime::now() + Duration::from_secs(1 + i))
            );
        }
        assert!(!bench.store.contains(&transient_id_of(&body)));

        // The node re-serving the same body, or the sender re-sending the same message under
        // a fresh encryption (a new transient id), is still a duplicate by message id.
        assert_eq!(discarded(bench.process(&body).await), Discard::Duplicate);
        let reencrypted = honest_body(&sender, &bench.me(), b"once");
        assert_ne!(transient_id_of(&reencrypted), transient_id_of(&body));
        assert_eq!(
            discarded(bench.process(&reencrypted).await),
            Discard::Duplicate
        );
        assert_eq!(bench.sink.count(), 1);
    }

    #[tokio::test]
    async fn a_blocked_signer_is_discarded_and_the_stamp_line_is_logged_for_a_trusted_one() {
        install_log_collector();
        let blocked = CorePrivateIdentity::new_from_rand(OsRng);
        let trusted = CorePrivateIdentity::new_from_rand(OsRng);
        let (blocked_id, trusted_id) = (
            transport_identity_of(&blocked),
            transport_identity_of(&trusted),
        );
        let mut bench = Bench::new(
            "fetch-blocked",
            TrustList::default()
                .identity(&identity_hex(&trusted_id), true)
                .block(&identity_hex(&blocked_id)),
        );
        bench.keys.know(&blocked_id);
        bench.keys.know(&trusted_id);
        let me = bench.me();

        assert_eq!(
            discarded(bench.process(&honest_body(&blocked, &me, b"blocked")).await),
            Discard::BlockedSource
        );
        let accepted = honest_body(&trusted, &me, b"trusted");
        assert_eq!(bench.process(&accepted).await, BodyOutcome::Delivered);

        let stamp_line = format!(
            "Propagation fetch from fake: transient {} stamp cost demanded 0 supplied none",
            short(&hex_lower(&transient_id_of(&accepted)))
        );
        assert!(
            debug_snapshot().iter().any(|line| line == &stamp_line),
            "{stamp_line:?}"
        );
        assert_eq!(REQUIRED_DELIVERY_STAMP_COST, 0);

        // A stamped body: the supplied value is the stamp's own work value, at least the
        // cost it was mined to, and reaches the sink with the message.
        let (stamped, message_id, stamp) = stamped_body(&trusted, &me, b"stamped", 4);
        assert_eq!(bench.process(&stamped).await, BodyOutcome::Delivered);
        let value = invalid_stamp_value(Some(&stamp), &message_id).unwrap();
        assert!(value >= 4, "mined to 4, worth {value}");
        let stamped_line = format!(
            "Propagation fetch from fake: transient {} stamp cost demanded 0 supplied {value}",
            short(&hex_lower(&transient_id_of(&stamped)))
        );
        assert!(
            debug_snapshot().iter().any(|line| line == &stamped_line),
            "{stamped_line:?}"
        );
        let delivered = bench.sink.messages.lock().clone();
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[1].message_id, message_id);
        assert_eq!(delivered[1].stamp_value, Some(value));
        assert_eq!(delivered[0].stamp_value, None);
    }

    fn store_at(tmp: &TempDir, now: SystemTime) -> FetchStore {
        FetchStore::load(tmp.path.join("mesh").join("propagation.json"), now).unwrap()
    }

    #[test]
    fn fetch_lock_refuses_a_second_holder_until_the_first_drops() {
        let tmp = TempDir::new("fetch-lock");
        let store_path = tmp.path.join("mesh").join("propagation.json");
        let first = FetchLock::acquire(&store_path).unwrap();
        assert!(store_path.with_added_extension("lock").is_file());

        let contended = FetchLock::acquire(&store_path).err();
        assert_eq!(contended, Some(FetchError::AlreadyRunning));
        let text = contended.unwrap().to_string();
        assert!(text.contains("another Coyote session"), "{text}");

        drop(first);
        FetchLock::acquire(&store_path).unwrap();
    }

    #[test]
    fn dedup_evicts_the_oldest_past_capacity_and_logs_it() {
        install_log_collector();
        let tmp = TempDir::new("fetch-dedup-cap");
        let mut store = store_at(&tmp, t(1));
        let mut ids = Vec::new();
        for i in 0..DEDUP_CAPACITY {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&(i as u64).to_be_bytes());
            assert!(store.insert(id, t(1_000 + i as u64)));
            ids.push(id);
        }
        // A repeat keeps its first `seen_at`, so it is still the oldest.
        assert!(!store.insert(ids[0], t(9_000)));
        assert_eq!(store.len(), DEDUP_CAPACITY);

        assert!(store.insert(id(0xff), t(9_001)));

        assert_eq!(store.len(), DEDUP_CAPACITY);
        assert!(!store.contains(&ids[0]));
        assert!(store.contains(&ids[1]));
        assert!(store.contains(&id(0xff)));
        let evicted = format!(
            "Forgot propagated transient {} to stay within {DEDUP_CAPACITY} remembered",
            short(&hex_lower(&ids[0]))
        );
        assert!(
            debug_snapshot().iter().any(|line| line == &evicted),
            "{evicted:?}"
        );
    }

    #[test]
    fn dedup_forgets_past_the_horizon_on_insert_and_on_load() {
        let tmp = TempDir::new("fetch-dedup-horizon");
        let mut store = store_at(&tmp, t(1_000));
        assert!(store.insert(id(1), t(1_000)));
        assert!(store.insert(id(2), t(1_000) + DEDUP_HORIZON - Duration::from_secs(1)));
        assert!(store.contains(&id(1)));

        assert!(store.insert(id(3), t(1_000) + DEDUP_HORIZON));
        assert!(!store.contains(&id(1)), "exactly at the horizon is expired");
        assert!(store.contains(&id(2)));
        store.persist().unwrap();

        let reloaded = store_at(&tmp, t(1_000) + DEDUP_HORIZON);
        assert!(reloaded.contains(&id(2)));
        assert!(reloaded.contains(&id(3)));
        let later = store_at(&tmp, t(1_000) + DEDUP_HORIZON * 2);
        assert!(!later.contains(&id(2)));
        assert!(!later.contains(&id(3)));
        assert_eq!(later.len(), 0);
    }

    #[test]
    fn store_survives_a_reopen_in_the_documented_layout() {
        let tmp = TempDir::new("fetch-store-reopen");
        let path = tmp.path.join("mesh").join("propagation.json");
        let mut store = store_at(&tmp, t(5_000));
        assert!(store.insert(id(0xab), t(5_000)));
        assert!(store.record_delivered(id(0xcd), t(5_000)));
        assert_eq!(
            store.defer(id(0xef), t(4_999)),
            Deferral::Retry { attempts: 1 }
        );
        assert_eq!(
            store.defer(id(0xef), t(5_000)),
            Deferral::Retry { attempts: 2 }
        );
        store.record_sync("cafe", t(5_001), 3);
        store.persist().unwrap();
        assert!(!path.with_extension("json.tmp").exists());

        let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["seen"][0]["transient_id"], hex_lower(&id(0xab)));
        assert_eq!(json["seen"][0]["seen_at"], "1970-01-01T01:23:20Z");
        assert_eq!(json["seen"].as_array().unwrap().len(), 1);
        assert_eq!(json["delivered"][0]["message_id"], hex_lower(&id(0xcd)));
        assert_eq!(json["delivered"][0]["delivered_at"], "1970-01-01T01:23:20Z");
        assert_eq!(json["deferred"][0]["transient_id"], hex_lower(&id(0xef)));
        assert_eq!(json["deferred"][0]["attempts"], 2);
        assert_eq!(json["deferred"][0]["first_seen_at"], "1970-01-01T01:23:19Z");
        assert_eq!(json["deferred"].as_array().unwrap().len(), 1);
        assert_eq!(json["cursor"]["node"], "cafe");
        assert_eq!(json["cursor"]["last_synced_at"], "1970-01-01T01:23:21Z");
        assert_eq!(json["cursor"]["last_received"], 3);

        let reloaded = store_at(&tmp, t(5_002));
        assert!(reloaded.contains(&id(0xab)));
        assert!(!reloaded.contains(&id(0xac)));
        assert!(reloaded.was_delivered(&id(0xcd)));
        assert!(!reloaded.was_delivered(&id(0xab)));
        assert!(!reloaded.contains(&id(0xcd)), "the two sets are distinct");
        assert_eq!(
            reloaded.deferral_of(&id(0xef)),
            Some(Deferred {
                attempts: 2,
                first_seen_at: t(4_999),
            })
        );
        assert_eq!(reloaded.len(), 1);
        assert_eq!(
            reloaded.cursor(),
            Some(&SyncCursor {
                node: "cafe".to_string(),
                last_synced_at: t(5_001),
                last_received: 3,
            })
        );
    }

    #[test]
    fn store_from_a_newer_coyote_is_refused_by_name() {
        let tmp = TempDir::new("fetch-store-version");
        let path = tmp.path.join("propagation.json");
        fs::write(
            &path,
            r#"{"version": 2, "seen": [], "cursor": null, "extra": 1}"#,
        )
        .unwrap();

        let err = match FetchStore::load(path.clone(), t(1)) {
            Ok(_) => panic!("a version 2 file must be refused"),
            Err(err) => err,
        };

        assert_eq!(
            err,
            FetchError::StoreVersion {
                path: path.clone(),
                found: 2
            }
        );
        let text = err.to_string();
        assert!(text.contains(&path.display().to_string()), "{text}");
        assert!(text.contains("upgrade Coyote"), "{text}");
        assert!(text.contains("move the file aside"), "{text}");
        assert!(path.exists(), "a refused file is left where it is");
    }

    fn assert_set_aside(tag: &str, bytes: &[u8], what: &str) {
        install_log_collector();
        let tmp = TempDir::new(tag);
        let path = tmp.path.join("propagation.json");
        let aside = tmp.path.join("propagation.json.corrupt");
        fs::write(&path, bytes).unwrap();

        let store = FetchStore::load(path.clone(), t(1)).expect("cache never blocks a fetch");

        assert_eq!(store.len(), 0);
        assert_eq!(store.cursor(), None);
        assert!(!path.exists());
        assert_eq!(fs::read(&aside).unwrap(), bytes);
        let path_text = path.display().to_string();
        assert!(
            warn_snapshot().iter().any(|line| line.contains(&path_text)
                && line.contains(what)
                && line.contains(&aside.display().to_string())),
            "no warning names {path_text} with {what:?}"
        );
    }

    #[test]
    fn garbage_and_malformed_stores_are_set_aside() {
        assert_set_aside(
            "fetch-store-garbage",
            b"{not json",
            "no readable version field",
        );
        assert_set_aside(
            "fetch-store-bad-id",
            br#"{"version": 1, "seen": [{"transient_id": "zz", "seen_at": "1970-01-01T00:00:01Z"}], "delivered": [], "deferred": [], "cursor": null}"#,
            "holds a transient id that is not 64 hex digits: 'zz'",
        );
        assert_set_aside(
            "fetch-store-bad-message-id",
            br#"{"version": 1, "seen": [], "delivered": [{"message_id": "zz", "delivered_at": "1970-01-01T00:00:01Z"}], "deferred": [], "cursor": null}"#,
            "holds a message id that is not 64 hex digits: 'zz'",
        );
        assert_set_aside(
            "fetch-store-bad-deferral",
            br#"{"version": 1, "seen": [], "delivered": [], "deferred": [{"transient_id": "0000000000000000000000000000000000000000000000000000000000000000", "attempts": 1, "first_seen_at": "soon"}], "cursor": null}"#,
            "holds a first_seen_at that is not RFC 3339: 'soon'",
        );
        // A quoted value is cut short, so a corrupt file cannot flood the log.
        let long = format!(
            r#"{{"version": 1, "seen": [{{"transient_id": "{}", "seen_at": "x"}}], "delivered": [], "deferred": [], "cursor": null}}"#,
            "f".repeat(300)
        );
        assert_set_aside(
            "fetch-store-long-id",
            long.as_bytes(),
            &format!("'{}...'", "f".repeat(80)),
        );
        assert_set_aside(
            "fetch-store-unknown-field",
            br#"{"version": 1, "seen": [], "delivered": [], "deferred": [], "cursor": null, "later": true}"#,
            "is not valid JSON",
        );
    }

    #[test]
    fn refusal_errors_carry_distinct_remedies() {
        let no_identity = link_error(R3Error::Refused(RefusalCode::NoIdentity), "node1", "id1");
        let no_access = link_error(R3Error::Refused(RefusalCode::NoAccess), "node1", "id1");
        assert_eq!(
            no_identity,
            FetchError::NodeSawNoIdentity {
                node: "node1".to_string()
            }
        );
        assert_eq!(
            no_access,
            FetchError::NodeRefusedAccess {
                node: "node1".to_string(),
                identity_hash: "id1".to_string()
            }
        );
        assert_ne!(no_identity.to_string(), no_access.to_string());
        assert!(no_identity.to_string().contains("identify did not reach"));
        assert!(no_access.to_string().contains("auth_required"));
        assert!(no_access.to_string().contains("id1"));
        assert_eq!(
            link_error(R3Error::Refused(RefusalCode::Throttled), "n", "i"),
            FetchError::NodeRefused(RefusalCode::Throttled)
        );
        assert_eq!(
            link_error(R3Error::LinkClosed, "n", "i"),
            FetchError::Link(R3Error::LinkClosed)
        );
    }

    #[tokio::test]
    async fn transport_keys_skip_peer_records_whose_destination_is_not_a_hash() {
        let tmp = TempDir::new("fetch-keys-guard");
        let peers = PeerTable::load(tmp.path.join("peers.json"), SystemTime::now()).unwrap();
        let identity = TransportIdentity::new_from_rand(OsRng);
        let transport = Transport::new(TransportConfig::new("t", &identity, false));
        // 32 bytes whose first two are one character: upstream's parser slices the string
        // by byte and panics on it, as it does on 32 ASCII non-hex bytes.
        let non_ascii = format!("\u{e9}{}", "0".repeat(30));
        assert_eq!(non_ascii.len(), 32);
        assert_eq!(canonical_hash(&non_ascii), None);
        let unknown_but_valid = identity.as_identity().address_hash.to_hex_string();
        for (i, destination_hash) in [non_ascii, "zz".repeat(16), unknown_but_valid.clone()]
            .into_iter()
            .enumerate()
        {
            peers.observe(
                PeerSighting {
                    destination_hash,
                    identity_hash: unknown_but_valid.clone(),
                    name_hash: String::new(),
                    display_name: None,
                    protocol_version: 1,
                    hops: i as u8,
                },
                SystemTime::now(),
            );
        }
        assert_eq!(peers.snapshot().len(), 3);

        let keys =
            TransportKeys::build(&transport, &peers, Deadline::after(Duration::from_secs(5)))
                .await
                .unwrap();

        assert!(
            keys.by_delivery.is_empty(),
            "two records are not hashes and the third names a destination the transport never heard"
        );
    }

    #[test]
    fn fetch_modules_never_name_anything_that_could_act_on_a_message() {
        // Assembled at runtime so this test's own text does not match the probes.
        let needles = [
            ["crate::", "client"].concat(),
            ["crate::", "agent"].concat(),
            ["en", "voy"].concat(),
            ["En", "voy"].concat(),
            ["mo", "del"].concat(),
        ];
        let mut checked = 0;
        for path in rust_sources() {
            let name = path.file_name().unwrap().to_str().unwrap();
            if name != "propagation_fetch.rs" && name != "propagation_nodes.rs" {
                continue;
            }
            checked += 1;
            let source = fs::read_to_string(&path).unwrap();
            let production = source
                .split(&["#[cfg(test)]\n", "mod tests"].concat())
                .next()
                .unwrap();
            assert!(
                production.len() < source.len(),
                "{} has no tests",
                path.display()
            );
            for needle in &needles {
                assert!(
                    !production.contains(needle),
                    "{} must not reference {needle}",
                    path.display()
                );
            }
        }
        assert_eq!(checked, 2);
    }

    #[cfg(unix)]
    mod network {
        use super::*;
        use crate::config::Session;
        use crate::mesh::mesh_cache_dir;
        use crate::mesh::node::{MeshRuntime, MeshSlot, NodeOptions};
        use crate::mesh::propagation::pn_announce_app_data;
        use crate::mesh::r3::{
            Admission, InboundRequest, NAME_HASH_LEN, R3Server, Reply, RequestHandler,
        };
        use crate::mesh::test_support::{
            Connector, INTEROP_TIMEOUT, LEGACY_LINK_MTU, Listener, POLL, mesh_paths,
            private_config, started_runtime, wait_until,
        };

        use rns_transport::destination::DestinationName;
        use rns_transport::destination::link::LinkId;
        use rns_transport::identity::PrivateIdentity as TransportIdentity;
        use rns_transport::iface::InterfaceSharedConfig;
        use rns_transport::iface::tcp_server::TcpServer;
        use std::collections::VecDeque;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::time::{Instant, sleep, timeout};

        /// Ceiling on one whole fetch against the fake node: a link, an identify and up to
        /// three rounds over loopback.
        const FETCH_DEADLINE: Duration = Duration::from_secs(30);

        /// One `/get` round as the fake node saw it.
        #[derive(Clone)]
        struct Seen {
            request_id: String,
            identity: Option<AddressHash>,
            branch: SizeBranch,
            data: Value,
        }

        /// Answers each round with the next scripted value and records what arrived. A
        /// listening post, not a propagation node: it never reads the request.
        #[derive(Default)]
        struct Script {
            seen: Mutex<Vec<Seen>>,
            replies: Mutex<VecDeque<Value>>,
        }

        impl Script {
            fn reply_with(&self, values: impl IntoIterator<Item = Value>) {
                self.replies.lock().extend(values);
            }

            fn seen(&self) -> Vec<Seen> {
                self.seen.lock().clone()
            }
        }

        #[async_trait]
        impl RequestHandler for Script {
            fn admit(&self, _link_id: LinkId, _identity: Option<&Identity>) -> Admission {
                Admission::Admit
            }

            async fn handle(&self, request: InboundRequest) -> Reply {
                self.seen.lock().push(Seen {
                    request_id: request.request_id.to_hex_string(),
                    identity: request.identity.map(|identity| identity.address_hash),
                    branch: request.branch,
                    data: request.data,
                });
                let next = self.replies.lock().pop_front();
                match next {
                    Some(value) => Reply::Value(value),
                    None => Reply::Silent,
                }
            }
        }

        /// A `Listener` on `lxmf.propagation` with a `Script` behind it.
        struct FakeNode {
            listener: Listener,
            script: Arc<Script>,
        }

        impl FakeNode {
            async fn listen() -> Self {
                Self::listen_with_mtu(LEGACY_LINK_MTU).await
            }

            async fn listen_with_mtu(client_mtu: usize) -> Self {
                let script = Arc::new(Script::default());
                let listener = Listener::listen(
                    Arc::new(R3Server::new()),
                    script.clone(),
                    client_mtu,
                    TransportIdentity::new_from_rand(OsRng),
                    DestinationName::new("lxmf", "propagation"),
                )
                .await;
                Self { listener, script }
            }

            async fn announce(&self) {
                self.listener
                    .announce(Some(&pn_announce_app_data(
                        true,
                        0,
                        FETCH_TRANSFER_LIMIT_KB as i64,
                    )))
                    .await;
            }

            /// Announces `identity`'s `name` destination from the node's transport, the way
            /// a peer's own announce reaches us through the mesh, and returns its hash.
            async fn announce_as(
                &self,
                identity: TransportIdentity,
                name: DestinationName,
            ) -> AddressHash {
                let dest = self
                    .listener
                    .transport
                    .add_destination(identity, name)
                    .await;
                let mut dest = dest.lock().await;
                let packet = dest.announce(OsRng, None).unwrap();
                self.listener.transport.send_packet(packet).await;
                dest.desc.address_hash
            }

            fn hex(&self) -> String {
                self.listener.desc.address_hash.to_hex_string()
            }

            async fn stop(self) {
                self.listener.stop().await;
            }
        }

        /// Our side: a `Connector` joined to the fake, the fake as the `PropagationNode` its
        /// announce described, and a peer table, trust list, store path and sink of its own.
        struct Fetcher {
            connector: Connector,
            node: PropagationNode,
            peers: PeerTable,
            trust: Arc<TrustStore>,
            store_path: PathBuf,
            sink: CountingSink,
            _cache: TempDir,
            _trust_dir: TempDir,
        }

        impl Fetcher {
            async fn join(fake: &FakeNode, tag: &str, list: TrustList) -> Self {
                let mut connector = Connector::connect(fake.listener.port, LEGACY_LINK_MTU).await;
                fake.announce().await;
                let (desc, app_data) = connector.learn(&fake.listener.desc.address_hash).await;
                let node = PropagationNode::from_announce(&desc, &app_data).unwrap();
                let cache = TempDir::new(tag);
                let peers =
                    PeerTable::load(cache.path.join("peers.json"), SystemTime::now()).unwrap();
                let (trust, trust_dir) = list.open(tag);
                Self {
                    connector,
                    node,
                    peers,
                    trust,
                    store_path: cache.path.join("propagation.json"),
                    sink: CountingSink::default(),
                    _cache: cache,
                    _trust_dir: trust_dir,
                }
            }

            fn identity(&self) -> Identity {
                *self.connector.identity.as_identity()
            }

            /// Swaps the trust list, as an operator editing `trust.yaml` between fetches does;
            /// the store and peer table stay.
            fn trust_as(&mut self, tag: &str, list: TrustList) {
                let (trust, trust_dir) = list.open(tag);
                self.trust = trust;
                self._trust_dir = trust_dir;
            }

            /// Hears `identity`'s `lxmf.delivery` announce from the fake, as a reference
            /// peer's reaches us.
            async fn learn_delivery(&mut self, fake: &FakeNode, identity: TransportIdentity) {
                self.learn_announce(fake, identity, DestinationName::new("lxmf", "delivery"))
                    .await;
            }

            /// Hears `identity`'s `name` announce from the fake and waits until our transport
            /// can resolve the announced hash to the key behind it.
            async fn learn_announce(
                &mut self,
                fake: &FakeNode,
                identity: TransportIdentity,
                name: DestinationName,
            ) -> AddressHash {
                // Our interface is new to its transport, whose announce ingress control holds
                // a second announce arriving within a second of the node's for a minute.
                self.connector
                    .transport
                    .iface_manager()
                    .lock()
                    .await
                    .set_shared_config(
                        self.connector.iface,
                        InterfaceSharedConfig {
                            ingress_control: Some(false),
                            ..InterfaceSharedConfig::default()
                        },
                    );
                let announced = fake.announce_as(identity, name).await;
                self.connector.learn(&announced).await;
                let deadline = Instant::now() + INTEROP_TIMEOUT;
                while self
                    .connector
                    .transport
                    .destination_identity(&announced)
                    .await
                    .is_none()
                {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for the announce to be cached"
                    );
                    sleep(POLL).await;
                }
                announced
            }

            /// One fetch on a store reopened from disk, as `fetch_propagated` does each time.
            async fn fetch(&self) -> Result<FetchReport, FetchError> {
                let mut store = self.store();
                let options = FetchOptions {
                    link_timeout: DEFAULT_LINK_TIMEOUT,
                    request_timeout: INTEROP_TIMEOUT,
                };
                timeout(
                    FETCH_DEADLINE,
                    fetch(
                        &self.connector.transport,
                        &self.connector.client,
                        &self.connector.identity,
                        &self.node,
                        &self.peers,
                        &self.trust,
                        &mut store,
                        &self.sink,
                        &options,
                        CancellationToken::new(),
                    ),
                )
                .await
                .expect("the fetch must finish within FETCH_DEADLINE")
            }

            fn store(&self) -> FetchStore {
                FetchStore::load(self.store_path.clone(), SystemTime::now()).unwrap()
            }

            async fn stop(self) {
                self.connector.stop().await;
            }
        }

        fn bodies(bodies: &[Vec<u8>]) -> Value {
            Value::Array(
                bodies
                    .iter()
                    .map(|body| Value::Binary(body.clone()))
                    .collect(),
            )
        }

        /// The server's own record of which branch it sent `request_id`'s response on.
        fn response_sent_as(request_id: &str, branch: &str) -> bool {
            let line = format!("Sending mesh response {request_id} as a {branch} (");
            debug_snapshot()
                .iter()
                .any(|entry| entry.starts_with(&line))
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn three_rounds_identify_first_and_put_the_reference_bytes_on_the_wire() {
            install_log_collector();
            let fake = FakeNode::listen().await;
            let fetcher = Fetcher::join(&fake, "fetch-net-rounds", TrustList::default()).await;
            let ids = [id(1), id(2)];
            // Garbage past the size floor: discarded once received, which is all this test
            // needs of them. The second alone is over the 431-byte link MDU.
            let small = vec![0x5a; 200];
            let large = vec![0xa5; 600];
            fake.script.reply_with([
                ids_value(&ids),
                bodies(&[small.clone(), large.clone()]),
                Value::Array(vec![]),
            ]);

            let report = fetcher.fetch().await.unwrap();

            assert_eq!(
                report,
                FetchReport {
                    node: fake.hex(),
                    listed: 2,
                    wanted: 2,
                    received: 2,
                    delivered: 0,
                    duplicates: 0,
                    discarded: 2,
                    deferred: 0,
                    acknowledged: 2,
                    response_branch: Some(SizeBranch::Resource),
                }
            );
            let seen = fake.script.seen();
            assert_eq!(seen.len(), 3);
            let ours = fetcher.identity().address_hash;
            for round in &seen {
                assert_eq!(round.identity, Some(ours), "identified before every round");
                assert_eq!(
                    round.branch,
                    SizeBranch::Packet,
                    "every request fits a packet"
                );
            }
            assert_eq!(packed(&seen[0].data), vec![0x92, 0xc0, 0xc0]);
            let round2 = packed(&seen[1].data);
            assert_eq!(round2, expected_get(&ids, &[]));
            assert_eq!(&round2[..4], &[0x93, 0x92, 0xc4, 0x20]);
            assert_eq!(&round2[round2.len() - 3..], &[0x90, 0xcc, 0xf0]);
            let round3 = packed(&seen[2].data);
            assert_eq!(
                round3,
                expected_ack(&[transient_id_of(&small), transient_id_of(&large)])
            );
            assert_eq!(&round3[..5], &[0x92, 0xc0, 0x92, 0xc4, 0x20]);
            assert!(response_sent_as(&seen[0].request_id, "packet"));
            assert!(response_sent_as(&seen[1].request_id, "resource"));
            assert!(response_sent_as(&seen[2].request_id, "packet"));

            // Every round transition is logged with its counts, under the node's short hash.
            let prefix = format!("Propagation fetch from {}: ", short(&fake.hex()));
            let transitions = [
                format!(
                    "identified as {}, round 1 list requested",
                    short(&identity_hex(&fetcher.identity()))
                ),
                "listed 2, wanting 2, have 0".to_string(),
                format!("round 2 requested 2 bodies within {FETCH_TRANSFER_LIMIT_KB} kB"),
                "received 2 bodies (800 bytes, Resource)".to_string(),
                "round 3 acknowledging 2 bodies, leaving 0 deferred on the node".to_string(),
                "round 3 acknowledged 2; delivered 0, duplicates 0, discarded 2, deferred 0"
                    .to_string(),
            ];
            let log = debug_snapshot();
            for transition in &transitions {
                let line = format!("{prefix}{transition}");
                assert!(log.iter().any(|entry| entry == &line), "{line:?}");
            }

            fetcher.stop().await;
            fake.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn an_untrusted_sender_is_discarded_and_a_trusted_one_delivered() {
            let fake = FakeNode::listen().await;
            let mut fetcher =
                Fetcher::join(&fake, "fetch-net-untrusted", TrustList::default()).await;
            let sender = TransportIdentity::new_from_rand(OsRng);
            let sender_core = to_core_private_identity(&sender);
            let sender_hex = identity_hex(sender.as_identity());
            let sender_delivery = lxmf_delivery_hash(sender.as_identity()).to_hex_string();
            fetcher.learn_delivery(&fake, sender).await;

            let unwanted = honest_body(&sender_core, &fetcher.identity(), b"unwanted");
            let unwanted_id = transient_id_of(&unwanted);
            fake.script.reply_with([
                ids_value(&[unwanted_id]),
                bodies(&[unwanted]),
                Value::Array(vec![]),
            ]);
            let report = fetcher.fetch().await.unwrap();
            assert_eq!(fetcher.sink.count(), 0);
            assert_eq!(
                (report.received, report.discarded, report.delivered),
                (1, 1, 0)
            );
            assert!(fetcher.store().contains(&unwanted_id));
            assert_eq!(
                packed(&fake.script.seen()[2].data),
                expected_ack(&[unwanted_id])
            );

            fetcher.trust_as(
                "fetch-net-trusted",
                TrustList::default().identity(&sender_hex, true),
            );
            let wanted = honest_body(&sender_core, &fetcher.identity(), b"wanted");
            let wanted_id = transient_id_of(&wanted);
            fake.script.reply_with([
                ids_value(&[wanted_id]),
                bodies(&[wanted]),
                Value::Array(vec![]),
            ]);
            let report = fetcher.fetch().await.unwrap();
            assert_eq!(
                (report.received, report.discarded, report.delivered),
                (1, 0, 1)
            );
            let delivered = fetcher.sink.only();
            assert_eq!(delivered.transient_id, wanted_id);
            assert_eq!(delivered.source_identity_hash, sender_hex);
            assert_eq!(delivered.source_delivery_hash, sender_delivery);
            assert_eq!(delivered.title, Some(b"hello".to_vec()));
            assert_eq!(delivered.content, Some(b"wanted".to_vec()));
            let store = fetcher.store();
            assert!(store.contains(&wanted_id));
            assert!(
                store.contains(&unwanted_id),
                "the first fetch's id is still remembered"
            );
            assert_eq!(fake.script.seen().len(), 6);

            fetcher.stop().await;
            fake.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn the_reference_refusal_codes_become_their_own_errors() {
            let fake = FakeNode::listen().await;
            let fetcher = Fetcher::join(&fake, "fetch-net-refused", TrustList::default()).await;
            let node = short(&fake.hex()).to_string();
            let ours = identity_hex(&fetcher.identity());

            fake.script.reply_with([Value::from(0xf0u8)]);
            let no_identity = fetcher.fetch().await.unwrap_err();
            assert_eq!(
                no_identity,
                FetchError::NodeSawNoIdentity { node: node.clone() }
            );

            fake.script.reply_with([Value::from(0xf1u8)]);
            let no_access = fetcher.fetch().await.unwrap_err();
            assert_eq!(
                no_access,
                FetchError::NodeRefusedAccess {
                    node,
                    identity_hash: ours.clone(),
                }
            );
            assert!(no_access.to_string().contains(&ours));
            assert_ne!(no_identity.to_string(), no_access.to_string());
            assert_eq!(
                fake.script.seen().len(),
                2,
                "a refusal ends the fetch at round 1"
            );

            fetcher.stop().await;
            fake.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn remembered_ids_are_purged_as_haves_after_a_restart() {
            let fake = FakeNode::listen().await;
            let fetcher = Fetcher::join(&fake, "fetch-net-restart", TrustList::default()).await;
            let a = vec![0x11; 150];
            let b = vec![0x22; 150];
            let ids = [transient_id_of(&a), transient_id_of(&b)];
            fake.script
                .reply_with([ids_value(&ids), bodies(&[a, b]), Value::Array(vec![])]);
            let first = fetcher.fetch().await.unwrap();
            assert_eq!((first.received, first.acknowledged), (2, 2));

            // The next fetch reopens the store from disk; the node still lists both.
            fake.script
                .reply_with([ids_value(&ids), Value::Array(vec![])]);
            let second = fetcher.fetch().await.unwrap();
            assert_eq!(second.listed, 2);
            assert_eq!(second.wanted, 0);
            assert_eq!(second.received, 0);
            assert_eq!(second.duplicates, 0);
            assert_eq!(second.acknowledged, 0);
            let seen = fake.script.seen();
            assert_eq!(seen.len(), 5, "no round 3 when round 2 brought nothing");
            assert_eq!(packed(&seen[4].data), expected_get(&[], &ids));

            fetcher.stop().await;
            fake.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn an_unknown_source_is_left_on_the_node_while_a_forgery_is_acknowledged() {
            install_log_collector();
            let fake = FakeNode::listen().await;
            let mut fetcher =
                Fetcher::join(&fake, "fetch-net-deferred", TrustList::default()).await;
            let known = TransportIdentity::new_from_rand(OsRng);
            let known_delivery = lxmf_delivery_hash(known.as_identity());
            let stranger = to_core_private_identity(&TransportIdentity::new_from_rand(OsRng));
            fetcher.learn_delivery(&fake, known).await;
            let me = fetcher.identity();

            // The forgery names a source whose key we hold, so it is proven bad and deleted
            // from the node; the orphan's source has never announced, so the node keeps it.
            let forged = body_from(&stranger, &known_delivery, &me, b"forged");
            let orphan = honest_body(&stranger, &me, b"orphan");
            let (forged_id, orphan_id) = (transient_id_of(&forged), transient_id_of(&orphan));
            fake.script.reply_with([
                ids_value(&[forged_id, orphan_id]),
                bodies(&[forged, orphan.clone()]),
                Value::Array(vec![]),
            ]);
            let report = fetcher.fetch().await.unwrap();

            assert_eq!(
                (
                    report.received,
                    report.discarded,
                    report.deferred,
                    report.delivered,
                    report.acknowledged
                ),
                (2, 1, 1, 0, 1)
            );
            assert_eq!(
                packed(&fake.script.seen()[2].data),
                expected_ack(&[forged_id]),
                "round 3 names the forgery alone"
            );
            let store = fetcher.store();
            assert!(store.contains(&forged_id));
            assert!(!store.contains(&orphan_id));
            assert_eq!(store.deferral_of(&orphan_id).map(|d| d.attempts), Some(1));
            let deferred_line = format!(
                "Propagation fetch from {}: deferred transient {}: no public key known for the claimed source, sighting 1 of {MAX_UNKNOWN_SOURCE_DEFERRALS}",
                short(&fake.hex()),
                short(&hex_lower(&orphan_id))
            );
            assert!(
                debug_snapshot().iter().any(|line| line == &deferred_line),
                "{deferred_line:?}"
            );
            assert_eq!(fetcher.sink.count(), 0);

            // The node still lists both: the forgery is purged as a have, the orphan wanted,
            // served and deferred once more.
            fake.script.reply_with([
                ids_value(&[forged_id, orphan_id]),
                bodies(&[orphan]),
                Value::Array(vec![]),
            ]);
            let again = fetcher.fetch().await.unwrap();
            assert_eq!(
                (
                    again.wanted,
                    again.received,
                    again.deferred,
                    again.acknowledged
                ),
                (1, 1, 1, 0)
            );
            let seen = fake.script.seen();
            assert_eq!(
                packed(&seen[4].data),
                expected_get(&[orphan_id], &[forged_id])
            );
            assert_eq!(packed(&seen[5].data), expected_ack(&[]));
            assert_eq!(
                fetcher.store().deferral_of(&orphan_id).map(|d| d.attempts),
                Some(2)
            );

            fetcher.stop().await;
            fake.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_coyote_peer_is_resolved_through_the_peer_table() {
            install_log_collector();
            let fake = FakeNode::listen().await;
            let sender = TransportIdentity::new_from_rand(OsRng);
            let sender_core = to_core_private_identity(&sender);
            let sender_identity = *sender.as_identity();
            let mut fetcher = Fetcher::join(
                &fake,
                "fetch-net-coyote-peer",
                TrustList::default().identity(&identity_hex(&sender_identity), true),
            )
            .await;
            // A Coyote peer announces its own destination, never `lxmf.delivery`.
            let name = DestinationName::new("coyote", "mesh.test");
            let coyote_hash = fetcher.learn_announce(&fake, sender, name).await;
            let me = fetcher.identity();
            let body = honest_body(&sender_core, &me, b"from a coyote");
            let body_id = transient_id_of(&body);

            // Control: the announce is cached, but nothing yet maps the delivery hash the
            // body names to that identity.
            fake.script.reply_with([
                ids_value(&[body_id]),
                bodies(std::slice::from_ref(&body)),
                Value::Array(vec![]),
            ]);
            let without = fetcher.fetch().await.unwrap();
            assert_eq!((without.delivered, without.deferred), (0, 1));
            let deferred = format!("deferred transient {}", short(&hex_lower(&body_id)));
            assert!(
                debug_snapshot().iter().any(|line| line.contains(&deferred)
                    && line.contains("no public key known for the claimed source")),
                "{deferred:?}"
            );

            fetcher.peers.observe(
                PeerSighting {
                    destination_hash: coyote_hash.to_hex_string(),
                    identity_hash: identity_hex(&sender_identity),
                    name_hash: hex_lower(&name.hash.as_slice()[..NAME_HASH_LEN]),
                    display_name: Some("Sender".to_string()),
                    protocol_version: 1,
                    hops: 1,
                },
                SystemTime::now(),
            );
            fake.script
                .reply_with([ids_value(&[body_id]), bodies(&[body]), Value::Array(vec![])]);
            let with = fetcher.fetch().await.unwrap();

            assert_eq!(
                (with.delivered, with.deferred, with.acknowledged),
                (1, 0, 1)
            );
            let delivered = fetcher.sink.only();
            assert_eq!(delivered.transient_id, body_id);
            assert_eq!(
                delivered.source_identity_hash,
                identity_hex(&sender_identity)
            );
            assert_eq!(
                delivered.source_delivery_hash,
                lxmf_delivery_hash(&sender_identity).to_hex_string()
            );
            let store = fetcher.store();
            assert!(store.contains(&body_id));
            assert_eq!(store.deferral_of(&body_id), None);

            fetcher.stop().await;
            fake.stop().await;
        }

        /// Knows `known` for the first lookup and times out on every later one, as a
        /// transport wedging between two bodies would.
        struct KeysThatWedge {
            known: Identity,
            lookups: AtomicUsize,
        }

        #[async_trait]
        impl SourceKeys for KeysThatWedge {
            async fn identity_for(
                &self,
                _source: &AddressHash,
            ) -> Result<Option<Identity>, R3Error> {
                match self.lookups.fetch_add(1, Ordering::SeqCst) {
                    0 => Ok(Some(self.known)),
                    _ => Err(R3Error::Timeout {
                        path: GET_PATH.to_string(),
                        after: Duration::from_secs(1),
                    }),
                }
            }
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn what_the_loop_recorded_is_persisted_when_a_key_lookup_fails() {
            let fake = FakeNode::listen().await;
            let sender = TransportIdentity::new_from_rand(OsRng);
            let sender_core = to_core_private_identity(&sender);
            let fetcher = Fetcher::join(
                &fake,
                "fetch-net-persist-on-error",
                TrustList::default().identity(&identity_hex(sender.as_identity()), true),
            )
            .await;
            let me = fetcher.identity();
            let first = honest_body(&sender_core, &me, b"first");
            let second = honest_body(&sender_core, &me, b"second");
            let (first_id, second_id) = (transient_id_of(&first), transient_id_of(&second));

            let recipient = to_core_private_identity(&fetcher.connector.identity);
            let keys = KeysThatWedge {
                known: *sender.as_identity(),
                lookups: AtomicUsize::new(0),
            };
            let pipeline = BodyPipeline {
                recipient: &recipient,
                delivery: lxmf_delivery_hash(&me),
                keys: &keys,
                trust: &fetcher.trust,
                sink: &fetcher.sink,
                node: "fake",
            };
            let mut store = fetcher.store();
            let mut report = FetchReport::new(fake.hex());
            let err = process_and_persist(
                &pipeline,
                &[first, second.clone()],
                &mut store,
                &mut report,
                &fake.hex(),
                FetchError::Link,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, FetchError::Link(R3Error::Timeout { .. })),
                "{err:?}"
            );
            assert_eq!((report.delivered, report.deferred), (1, 0));
            assert_eq!(fetcher.sink.count(), 1);
            drop(store);

            let reopened = fetcher.store();
            assert!(reopened.contains(&first_id), "delivered before the failure");
            assert!(!reopened.contains(&second_id), "never reached a verdict");
            assert_eq!(reopened.cursor(), None, "an interrupted loop is not a sync");

            // The next fetch purges the first as a have; the second's source is unknown to
            // the transport, so it is deferred rather than delivered.
            fake.script.reply_with([
                ids_value(&[first_id, second_id]),
                bodies(&[second]),
                Value::Array(vec![]),
            ]);
            let report = fetcher.fetch().await.unwrap();
            assert_eq!(
                (
                    report.wanted,
                    report.delivered,
                    report.deferred,
                    report.acknowledged
                ),
                (1, 0, 1, 0)
            );
            assert_eq!(
                packed(&fake.script.seen()[1].data),
                expected_get(&[second_id], &[first_id])
            );
            assert_eq!(
                fetcher.sink.count(),
                1,
                "the first body is not delivered twice"
            );
            assert!(fetcher.store().cursor().is_some());

            fetcher.stop().await;
            fake.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_long_list_is_wanted_in_slices_and_a_body_served_twice_is_one_duplicate() {
            let fake = FakeNode::listen().await;
            let fetcher = Fetcher::join(&fake, "fetch-net-slices", TrustList::default()).await;
            let ids: Vec<TransientId> = (0..MAX_WANTS_PER_FETCH + 5)
                .map(|i| {
                    let mut id = [0u8; 32];
                    id[..8].copy_from_slice(&(i as u64).to_be_bytes());
                    id
                })
                .collect();
            let junk = vec![0x3c; 150];
            fake.script.reply_with([
                ids_value(&ids),
                bodies(&[junk.clone(), junk.clone()]),
                Value::Array(vec![]),
            ]);

            let report = fetcher.fetch().await.unwrap();

            assert_eq!(report.listed, MAX_WANTS_PER_FETCH + 5);
            assert_eq!(report.wanted, MAX_WANTS_PER_FETCH);
            assert_eq!(
                (
                    report.received,
                    report.discarded,
                    report.duplicates,
                    report.acknowledged
                ),
                (2, 1, 1, 1)
            );
            let seen = fake.script.seen();
            let Value::Array(round2) = &seen[1].data else {
                panic!("round 2 is an array");
            };
            assert_eq!(round2[0], ids_value(&ids[..MAX_WANTS_PER_FETCH]));
            assert_eq!(round2[1], Value::Array(vec![]));
            assert_eq!(
                packed(&seen[2].data),
                expected_ack(&[transient_id_of(&junk)]),
                "one id however often it was served"
            );
            let store = fetcher.store();
            assert!(store.contains(&transient_id_of(&junk)));
            assert!(
                ids.iter().all(|id| !store.contains(id)),
                "listed but unserved ids are left for the next fetch"
            );

            fetcher.stop().await;
            fake.stop().await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_runtime_that_heard_no_node_says_so() {
            let started = started_runtime("fetch-net-no-node").await;
            let sink = CountingSink::default();
            let err = timeout(FETCH_DEADLINE, started.runtime.fetch_propagated(&sink))
                .await
                .expect("a fetch with no node must return at once")
                .unwrap_err();
            assert_eq!(err, FetchError::NoPropagationNode);
            assert!(
                err.to_string()
                    .starts_with("No LXMF propagation node has announced itself on the mesh yet"),
                "{err}"
            );
            assert_eq!(sink.count(), 0);

            let slot = MeshSlot::default();
            slot.install(started.runtime.clone()).unwrap();
            assert!(slot.stop().await.unwrap());
            started.relay_handle.abort();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn the_runtime_fetches_from_the_node_it_heard_and_stopping_it_cancels_a_fetch() {
            // A `MeshRuntime` joins with `TcpClient`'s default MTU, so the fake matches it.
            let fake = FakeNode::listen_with_mtu(TcpServer::DEFAULT_CLIENT_MTU).await;
            let tmp = TempDir::new("fetch-net-runtime");
            let mut session = Session::default();
            let runtime = MeshRuntime::start(
                &private_config(fake.listener.port),
                true,
                &mut session,
                mesh_paths(&tmp),
                NodeOptions::default(),
            )
            .await
            .unwrap();
            fake.announce().await;
            wait_until("the runtime to file the propagation node", || {
                runtime.propagation_nodes().select().is_ok()
            })
            .await;
            assert_eq!(
                runtime
                    .propagation_nodes()
                    .select()
                    .unwrap()
                    .destination
                    .address_hash
                    .to_hex_string(),
                fake.hex()
            );
            assert!(
                runtime.peers().snapshot().is_empty(),
                "a propagation node is not filed as a Coyote peer"
            );

            let sink = Arc::new(CountingSink::default());
            fake.script.reply_with([Value::Array(vec![])]);
            let report = timeout(FETCH_DEADLINE, runtime.fetch_propagated(&*sink))
                .await
                .expect("the fetch must finish within FETCH_DEADLINE")
                .unwrap();
            assert_eq!(report.node, fake.hex());
            assert_eq!(report.listed, 0);
            let store_path = mesh_cache_dir(&tmp.path.join("cache")).join("propagation.json");
            assert!(store_path.exists(), "{}", store_path.display());
            let cursor = FetchStore::load(store_path, SystemTime::now())
                .unwrap()
                .cursor()
                .cloned()
                .expect("an empty list is still a completed sync");
            assert_eq!(cursor.node, fake.hex());
            assert_eq!(cursor.last_received, 0);

            // With nothing scripted the fake stays silent, so the next fetch blocks in
            // round 1; a second caller is refused, and stopping the node ends the first.
            let blocked = tokio::spawn({
                let runtime = runtime.clone();
                let sink = sink.clone();
                async move { runtime.fetch_propagated(&*sink).await }
            });
            wait_until("the blocked fetch to reach round 1", || {
                fake.script.seen().len() == 2
            })
            .await;
            assert_eq!(
                runtime.fetch_propagated(&*sink).await.unwrap_err(),
                FetchError::AlreadyRunning
            );
            let slot = MeshSlot::default();
            slot.install(runtime.clone()).unwrap();
            assert!(slot.stop().await.unwrap());
            let outcome = timeout(FETCH_DEADLINE, blocked)
                .await
                .expect("stopping the node must end the fetch")
                .unwrap();
            assert_eq!(outcome, Err(FetchError::Cancelled));
            assert_eq!(sink.count(), 0);

            fake.stop().await;
        }
    }
}
