//! The user's trust list and the verdicts it gives.
//!
//! # Key changes
//!
//! Reticulum binds a destination hash to its identity: `dest = trunc16(H(name_hash(10) ||
//! identity_hash(16)))` (`destination_address`). Upstream checks that derivation and the
//! announce signature before an announce reaches this module, and the R3 dispatcher derives
//! the destination from the envelope's origin name hash plus the identity proven on the
//! link. A trusted destination hash therefore can never be presented under a different
//! identity on either path; the `same_hash` branch in `authorize` is defensive only.
//!
//! A peer that rotates its identity keeps its instance id (its name hash) and so announces a
//! new destination hash under the new identity. The detectable signal of a key change is a
//! name hash that re-derives one of our trusted destination records under a different
//! identity: record `R` (destination `D`, bound identity `I1`) conflicts with an announce or
//! request carrying (identity `I2`, name hash `N`) iff `destination_address(N, I1) == D` and
//! `I1 != I2`.
//!
//! Instance ids are public, so anyone who has heard the peer can announce
//! `scope.session.<instance_id>` under their own identity: the signal is forgeable by any
//! stranger. Hence the mark never alters the existing grant (`I1` still proves its own key
//! and stays trusted for `D`); the new identity is a stranger and stays fail-closed; the
//! human is told once per record while the mark stands; the first conflicting identity is
//! what the mark records, and later conflicts against an already-marked record write
//! nothing, so an attacker cannot drive disk churn; the wording says "announced under
//! another identity", never "rotated"; and nothing is ever re-bound or re-trusted on its
//! own. While the record stands, only the human clears a mark: by trusting either
//! destination again, by trusting the identity seen for all destinations, or by blocking
//! the identity seen. A confirmed `.mesh trust --prune` removes a stale marked record with
//! the rest, and the dry run shows the mark. Once the new destination is trusted that
//! identity holds its own record for the instance and marks the old one no more, since both
//! bindings are the human's; an identity trusted for all destinations marks nothing either,
//! the human having answered for every instance of it; and a denied record is never marked,
//! the human having answered it already. The converse holds too: an identity-tier grant has
//! no instance binding, so a rotation of that identity is fail-closed (the new key is a
//! stranger) but is neither marked nor surfaced; only a destination record can carry a
//! key-change notice.
//!
//! The store-and-forward peer-MESSAGE path (`src/mesh/message.rs`) authorizes through plain
//! `authorize`, so a rotated peer fails closed there but is neither marked nor notified;
//! the announce path is the detector, and detection on the message path is a follow-up.
//!
//! On the R3 path a freshly rotated peer has no standing and is silenced before its frame
//! is decoded, so its name hash is never seen there. `Rule::IdentityChanged` fires at the
//! verdict stage only when a proven identity that has standing (for another instance, say)
//! names an instance whose destination is bound to a different identity.

use crate::mesh::announce::is_control_or_invisible;
use crate::mesh::idle::{IdleNotify, Origin};
use crate::mesh::knock::KnockSurface;
use crate::mesh::node::MeshSlot;
use crate::mesh::notify::Source;
use crate::mesh::peers::{PeerRecord, PeerTable};
use crate::mesh::r3::NAME_HASH_LEN;
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::{
    canonical_hash, decode_hex, destination_address, mesh_config_dir, parse_rfc3339, redact_hashes,
    rfc3339_utc, short, write_atomically,
};

use anyhow::{Context, Result, anyhow, bail};
use arc_swap::ArcSwapOption;
use parking_lot::Mutex;
use rns_transport::hash::AddressHash;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

pub(crate) const TRUST_FILE_VERSION: u64 = 2;
/// The trust list is the human's own decisions, so a refusal says what starting fresh costs.
const TRUST_FILE_REMEDY: Remedy =
    Remedy::UserFile("is the trust list, and a fresh one trusts nobody");

pub(crate) const MESH_OFF: &str =
    "Mesh is off, so the trust list cannot be changed. Run `.mesh on` first.";

/// What a trust mutation needs from a running mesh: proof it is on, and the peer table
/// that turns a destination into its announced identity, with the knock cache as the
/// fallback for an instance that asked to be trusted before it was heard announcing.
pub(crate) trait LiveMesh {
    /// `None` while the mesh is off.
    fn peers(&self) -> Option<Arc<PeerTable>>;

    /// The knock the cache holds for `destination_hash`, when the peer table has not heard
    /// it announce.
    fn knock(&self, destination_hash: &str, now: SystemTime) -> Option<KnockProof> {
        let _ = (destination_hash, now);
        None
    }
}

/// The hashes a cached knock carries, which prove its destination the same way an announce
/// does: the name hash and identity must derive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KnockProof {
    pub identity_hash: String,
    /// Lower-hex; empty when the knock was cached before the name hash was kept.
    pub name_hash: String,
    pub last_seen: SystemTime,
}

impl LiveMesh for MeshSlot {
    fn peers(&self) -> Option<Arc<PeerTable>> {
        self.get().map(|runtime| runtime.peers())
    }

    fn knock(&self, destination_hash: &str, now: SystemTime) -> Option<KnockProof> {
        let knocks = match self.get()?.knock_gate().cache().list(now) {
            Ok(knocks) => knocks,
            Err(err) => {
                debug!(
                    "knock cache unreadable while resolving a destination to trust: {}",
                    redact_hashes(&format!("{err:#}"))
                );
                return None;
            }
        };
        knocks
            .into_iter()
            .find(|knock| knock.destination_hash == destination_hash)
            .map(|knock| KnockProof {
                identity_hash: knock.identity_hash,
                name_hash: knock.name_hash,
                last_seen: parse_rfc3339(&knock.received_at).unwrap_or(now),
            })
    }
}

/// A timestamp that is RFC 3339 UTC seconds on disk, floored to the second at construction
/// so a record compares equal to its reloaded self. Parsing happens at load, so a
/// hand-edited value that is not a timestamp fails the whole load instead of one query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Stamp(SystemTime);

impl Stamp {
    fn new(at: SystemTime) -> Self {
        let secs = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        Self(UNIX_EPOCH + Duration::from_secs(secs))
    }
}

impl Serialize for Stamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&rfc3339_utc(self.0))
    }
}

impl<'de> Deserialize<'de> for Stamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse_rfc3339(&text).map(Stamp).ok_or_else(|| {
            serde::de::Error::custom(format!("'{text}' is not an RFC 3339 timestamp"))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    version: u64,
    #[serde(default)]
    identities: BTreeMap<String, IdentityEntry>,
    #[serde(default)]
    destinations: BTreeMap<String, DestinationEntry>,
    #[serde(default)]
    denied_destinations: BTreeMap<String, OverlayEntry>,
    #[serde(default)]
    blocked_identities: BTreeMap<String, OverlayEntry>,
}

impl Default for TrustFile {
    fn default() -> Self {
        Self {
            version: TRUST_FILE_VERSION,
            identities: BTreeMap::new(),
            destinations: BTreeMap::new(),
            denied_destinations: BTreeMap::new(),
            blocked_identities: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityEntry {
    added_at: Stamp,
    last_seen_at: Stamp,
    label: Option<String>,
    note: Option<String>,
    /// The `--identity` flag: every destination this identity announces is accepted.
    all_destinations: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DestinationEntry {
    identity: String,
    added_at: Stamp,
    last_seen_at: Stamp,
    label: Option<String>,
    note: Option<String>,
    /// Set once this destination's name hash was seen under an identity other than
    /// `identity`. Added before MESH-CODE-005 made every new field bump `TRUST_FILE_VERSION`,
    /// so a current-version file written without it loads with it absent; the default is a
    /// tolerance inside one version and is not a precedent for the next field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_changed: Option<KeyChanged>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyChanged {
    seen_identity: String,
    at: Stamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlayEntry {
    added_at: Stamp,
    note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    Identity,
    Destination,
}

impl Tier {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Destination => "destination",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RevokeReason {
    Untrust,
    Block,
    Prune,
}

impl RevokeReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Untrust => "untrust",
            Self::Block => "block",
            Self::Prune => "prune",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustGranted {
    pub tier: Tier,
    pub identity_hash: String,
    /// Set on the destination tier only.
    pub destination_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustRevoked {
    pub tier: Tier,
    /// A pruned destination carries the identity its record was bound to.
    pub identity_hash: Option<String>,
    pub destination_hash: Option<String>,
    pub reason: RevokeReason,
}

/// Told about every record the store's mutation API adds or removes, once the write
/// has landed. Called with the store's state mutex held: an implementation must not
/// block or call back into the store.
pub(crate) trait TrustObserver: Send + Sync {
    fn granted(&self, event: TrustGranted);
    fn revoked(&self, event: TrustRevoked);
}

/// What one mutation did to the file, for the debug line and the observer.
enum TrustMutation {
    TrustDestination {
        destination: String,
        identity: String,
        change: TrustChange,
    },
    /// `granted` is whether identity-tier trust was conferred: the record was created
    /// with `all_destinations`, or an existing binding-only record had the flag set.
    /// A repeat `trust_identity` reports `Updated` to the caller and fires nothing.
    TrustIdentity {
        identity: String,
        granted: bool,
    },
    UntrustDestination {
        destination: String,
        identity: String,
    },
    /// The identity is trusted for all, so untrusting one destination writes a deny
    /// instead; nothing is removed, so it is not a revocation.
    RefuseDestination {
        destination: String,
    },
    /// `was_granting` is whether the removed identity record carried `all_destinations`;
    /// a binding-only record held no identity-tier trust to revoke.
    UntrustIdentity {
        identity: String,
        was_granting: bool,
        removed: Vec<String>,
    },
    /// `marks_cleared` counts the key-change marks that named this identity as the one
    /// seen, resolved by the block.
    BlockIdentity {
        identity: String,
        was_granting: bool,
        removed: Vec<String>,
        marks_cleared: usize,
    },
    UnblockIdentity {
        identity: String,
    },
    /// Tests seed the deny overlay directly; the REPL writes it through `RefuseDestination`.
    #[cfg(test)]
    Deny {
        destination: String,
    },
    #[cfg(test)]
    Undeny {
        destination: String,
    },
    /// Each stale destination with the identity its record named.
    Prune {
        stale: Vec<(String, String)>,
    },
    /// The destinations whose name hash was announced under `seen_identity`, an identity
    /// other than the one each is bound to.
    KeyChanged {
        destinations: Vec<String>,
        seen_identity: String,
    },
}

/// The log rendering: every hash truncated with `short`, as MESH-LOG-002 requires of a log
/// line; read by `TrustStore::commit`'s debug line and `note_key_change`'s info line.
impl fmt::Display for TrustMutation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrustDestination { destination, .. } => {
                write!(f, "trust destination {}", short(destination))
            }
            Self::TrustIdentity { identity, .. } => {
                write!(f, "trust identity {}", short(identity))
            }
            Self::UntrustDestination { destination, .. } => {
                write!(f, "untrust destination {}", short(destination))
            }
            Self::RefuseDestination { destination } => {
                write!(f, "refuse destination {}", short(destination))
            }
            Self::UntrustIdentity {
                identity, removed, ..
            } => write!(
                f,
                "untrust identity {} (+{} destinations)",
                short(identity),
                removed.len()
            ),
            Self::BlockIdentity {
                identity,
                removed,
                marks_cleared,
                ..
            } => write!(
                f,
                "block identity {} (+{} destinations, {} key-change marks cleared)",
                short(identity),
                removed.len(),
                marks_cleared
            ),
            Self::UnblockIdentity { identity } => {
                write!(f, "unblock identity {}", short(identity))
            }
            #[cfg(test)]
            Self::Deny { destination } => write!(f, "deny {}", short(destination)),
            #[cfg(test)]
            Self::Undeny { destination } => write!(f, "undeny {}", short(destination)),
            Self::Prune { stale } => write!(f, "prune {} destinations", stale.len()),
            Self::KeyChanged {
                destinations,
                seen_identity,
            } => write!(
                f,
                "key change marked on {} destinations announced under identity {}",
                destinations.len(),
                short(seen_identity)
            ),
        }
    }
}

impl TrustMutation {
    fn notify(self, observer: &dyn TrustObserver) {
        let revoked = |tier, identity_hash, destination_hash, reason| TrustRevoked {
            tier,
            identity_hash,
            destination_hash,
            reason,
        };
        match self {
            Self::TrustDestination {
                destination,
                identity,
                change: TrustChange::Added,
            } => observer.granted(TrustGranted {
                tier: Tier::Destination,
                identity_hash: identity,
                destination_hash: Some(destination),
            }),
            Self::TrustIdentity {
                identity,
                granted: true,
                ..
            } => observer.granted(TrustGranted {
                tier: Tier::Identity,
                identity_hash: identity,
                destination_hash: None,
            }),
            Self::UntrustDestination {
                destination,
                identity,
            } => observer.revoked(revoked(
                Tier::Destination,
                Some(identity),
                Some(destination),
                RevokeReason::Untrust,
            )),
            Self::UntrustIdentity {
                identity,
                was_granting,
                removed,
            } => {
                if was_granting {
                    observer.revoked(revoked(
                        Tier::Identity,
                        Some(identity.clone()),
                        None,
                        RevokeReason::Untrust,
                    ));
                }
                for destination in removed {
                    observer.revoked(revoked(
                        Tier::Destination,
                        Some(identity.clone()),
                        Some(destination),
                        RevokeReason::Untrust,
                    ));
                }
            }
            Self::BlockIdentity {
                identity,
                was_granting,
                removed,
                ..
            } => {
                if was_granting {
                    observer.revoked(revoked(
                        Tier::Identity,
                        Some(identity.clone()),
                        None,
                        RevokeReason::Block,
                    ));
                }
                for destination in removed {
                    observer.revoked(revoked(
                        Tier::Destination,
                        Some(identity.clone()),
                        Some(destination),
                        RevokeReason::Block,
                    ));
                }
            }
            Self::Prune { stale } => {
                for (destination, identity) in stale {
                    observer.revoked(revoked(
                        Tier::Destination,
                        Some(identity),
                        Some(destination),
                        RevokeReason::Prune,
                    ));
                }
            }
            #[cfg(test)]
            Self::Deny { .. } | Self::Undeny { .. } => {}
            Self::TrustDestination {
                change: TrustChange::Updated,
                ..
            }
            | Self::TrustIdentity { granted: false, .. }
            | Self::RefuseDestination { .. }
            | Self::UnblockIdentity { .. }
            | Self::KeyChanged { .. } => {}
        }
    }
}

/// One trusted identity or destination as `records` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustRecord {
    pub tier: Tier,
    pub hash: String,
    /// The identity a destination is bound to; `None` on identity records.
    pub identity: Option<String>,
    pub label: Option<String>,
    pub note: Option<String>,
    pub added_at: SystemTime,
    pub last_seen_at: SystemTime,
    pub all_destinations: bool,
    pub denied: bool,
    pub session: bool,
    /// Set on a destination record whose name hash was seen under another identity.
    pub key_changed: Option<KeyChange>,
}

/// The key-change mark as `records` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeyChange {
    /// The first identity other than the bound one the name hash was seen under.
    pub seen_identity: String,
    pub at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OverlayRecord {
    pub hash: String,
    pub added_at: SystemTime,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct TrustOptions {
    pub label: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustChange {
    Added,
    Updated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrustOutcome {
    pub identity_hash: String,
    pub destination_hash: String,
    pub change: TrustChange,
    /// Destinations of other records the trusted instance's name hash re-derives under
    /// their bound identities: the instance was trusted under another key before. Their
    /// key-change marks are cleared; the records themselves stay until untrusted.
    pub superseded: Vec<String>,
    /// Whether a deny record on the destination went with the same write.
    pub deny_lifted: bool,
}

/// What `untrust_destination` did, or with `dry_run` would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UntrustOutcome {
    /// The destination record was removed; the record for its identity stays.
    Forgotten { identity: String },
    /// The identity is trusted for every destination, so forgetting this one means
    /// refusing it: a deny record is written and its record (if any) stays.
    Refused {
        identity: String,
        already_refused: bool,
    },
}

/// A trusted destination record whose name hash `binding_conflicts` was asked about
/// re-derives under its bound identity, which is not the identity asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BindingConflict {
    pub destination_hash: String,
    pub bound_identity: String,
    pub label: Option<String>,
    pub already_marked: bool,
}

/// One record `note_key_change` marked: the first sighting of its name hash under another
/// identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkedKeyChange {
    pub destination_hash: String,
    pub bound_identity: String,
    pub seen_identity: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    Refuse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rule {
    DestinationDenied,
    IdentityBlocked,
    DestinationTrusted,
    IdentityTrusted,
    /// The origin name hash re-derives a trusted destination under a different identity;
    /// always a refusal.
    IdentityChanged,
    DefaultClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub decision: Decision,
    pub rule: Rule,
}

/// Where an identity stands at the identity tier. `Unknown` is the silent-drop case;
/// `Blocked` suppresses knocks; `Trusted` means its instances may knock, and only with
/// `all_destinations` are they allowed without a per-destination record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityStanding {
    Unknown,
    Blocked,
    Trusted { all_destinations: bool },
}

#[derive(Debug, Default)]
struct State {
    file: TrustFile,
    session_identities: BTreeMap<String, IdentityEntry>,
    session_destinations: BTreeMap<String, DestinationEntry>,
    seen: BTreeMap<String, SystemTime>,
}

/// Equality of two hashes that takes the same time whether they differ in the first byte
/// or the last. The length check folds into the same `Choice` rather than short-circuiting;
/// both sides are canonical 32-hex in practice, so it only ever guards a malformed input.
pub(crate) fn same_hash(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    let same_len = left.len().ct_eq(&right.len());
    let shorter = left.len().min(right.len());
    let same_prefix = left[..shorter].ct_eq(&right[..shorter]);
    bool::from(same_len & same_prefix)
}

/// The user's trust list, mirrored to `<config_dir>/mesh/trust.yaml`.
///
/// Only the mutation methods write the file, and each of them needs a running mesh: a
/// destination is trusted by proving, from the peer table's copy of its announce or from
/// its cached knock, which identity derived it. Queries never touch the disk. `mark_seen`
/// and session trust live in memory only, so `last_seen_at` on disk is as of the last
/// mutation that touched a record while `records` reports the fresher in-memory value; the
/// node calls `mark_seen` for every announce it files. The one write an announce or a
/// request can cause is `note_key_change`, once per record: a mark that is already set is
/// never written again.
///
/// Nothing here removes a record on its own: removal is `untrust_*`, `block_identity` and
/// `prune_destinations`, all at the user's request.
pub(crate) struct TrustStore {
    path: PathBuf,
    inner: Mutex<State>,
    observer: ArcSwapOption<Arc<dyn TrustObserver>>,
    /// Where a key-change line goes; held weakly because the slot owns the runtime that
    /// owns this store.
    surface: Mutex<Option<Weak<dyn KnockSurface>>>,
}

impl fmt::Debug for TrustStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrustStore")
            .field("path", &self.path)
            .field("observer", &self.observer.load().is_some())
            .finish_non_exhaustive()
    }
}

impl TrustStore {
    /// Loads `trust.yaml` under `config_dir`. A missing file is an empty list and nothing is
    /// created; a file that cannot be parsed is an error, never a partial list, because the
    /// trust list is the user's and must not be quietly narrowed or replaced.
    pub(crate) fn open(config_dir: &Path) -> Result<Self> {
        let path = mesh_config_dir(config_dir).join("trust.yaml");
        let file = match fs::read_to_string(&path) {
            Ok(text) => parse_trust_file(&path, &text)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => TrustFile::default(),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to read mesh trust list '{}'", path.display())
                });
            }
        };
        Ok(Self {
            path,
            inner: Mutex::new(State {
                file,
                ..State::default()
            }),
            observer: ArcSwapOption::empty(),
            surface: Mutex::new(None),
        })
    }

    /// Installs who hears about grants and revocations; the one before it is dropped.
    pub(crate) fn set_observer(&self, observer: Arc<dyn TrustObserver>) {
        self.observer.store(Some(Arc::new(observer)));
    }

    /// Installs where `note_key_change` puts its one line per newly marked record. A mark
    /// made before this is called is on the record but was never surfaced.
    pub(crate) fn attach_surface(&self, surface: Weak<dyn KnockSurface>) {
        *self.surface.lock() = Some(surface);
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The one place the precedence lives: destination deny, then destination allow, then
    /// identity allow, then default-closed. A blocked identity is refused before either
    /// allow and reported under its own rule. A destination record only allows the identity
    /// it was proven to belong to. `authorize_origin` layers the one rule that needs the
    /// name hash, identity changed, between identity allow and default-closed.
    ///
    /// The map lookups are keyed on values the peer already knows and are not treated as a
    /// timing boundary. The destination binding is the one direct equality against a
    /// caller-supplied identity, and it runs in constant time (`same_hash`). Callers
    /// consume the verdict; none of them compares identities again.
    pub(crate) fn authorize(&self, identity_hash: &str, destination_hash: &str) -> Verdict {
        let identity = identity_hash.to_ascii_lowercase();
        let destination = destination_hash.to_ascii_lowercase();
        let state = self.inner.lock();
        if state.file.denied_destinations.contains_key(&destination) {
            return Verdict {
                decision: Decision::Refuse,
                rule: Rule::DestinationDenied,
            };
        }
        if state.file.blocked_identities.contains_key(&identity) {
            return Verdict {
                decision: Decision::Refuse,
                rule: Rule::IdentityBlocked,
            };
        }
        let bound_to_identity = state
            .file
            .destinations
            .get(&destination)
            .or_else(|| state.session_destinations.get(&destination))
            .is_some_and(|entry| same_hash(&entry.identity, &identity));
        if bound_to_identity {
            return Verdict {
                decision: Decision::Allow,
                rule: Rule::DestinationTrusted,
            };
        }
        if state
            .file
            .identities
            .get(&identity)
            .is_some_and(|entry| entry.all_destinations)
        {
            return Verdict {
                decision: Decision::Allow,
                rule: Rule::IdentityTrusted,
            };
        }
        Verdict {
            decision: Decision::Refuse,
            rule: Rule::DefaultClosed,
        }
    }

    /// `authorize` for a request whose origin is known by name hash: the destination judged
    /// is the one `name_hash` and `identity` derive, and a default-closed verdict becomes
    /// `IdentityChanged` when that name hash re-derives a trusted destination under another
    /// identity. That is the only difference: a refusal stays a refusal, only its rule
    /// changes, and the conflicting record's grant stays with the identity it is bound to.
    pub(crate) fn authorize_origin(
        &self,
        identity: &AddressHash,
        name_hash: &[u8; NAME_HASH_LEN],
    ) -> (Verdict, AddressHash) {
        let destination = destination_address(name_hash, identity);
        let identity_hex = identity.to_hex_string();
        let verdict = self.authorize(&identity_hex, &destination.to_hex_string());
        if verdict.rule == Rule::DefaultClosed
            && !self.binding_conflicts(&identity_hex, name_hash).is_empty()
        {
            return (
                Verdict {
                    decision: Decision::Refuse,
                    rule: Rule::IdentityChanged,
                },
                destination,
            );
        }
        (verdict, destination)
    }

    /// The trusted destination records, on disk then session, that `name_hash` re-derives
    /// under their bound identity when that identity is not `identity_hash`: the instance
    /// they belong to has been seen under another key.
    pub(crate) fn binding_conflicts(
        &self,
        identity_hash: &str,
        name_hash: &[u8; NAME_HASH_LEN],
    ) -> Vec<BindingConflict> {
        self.inner
            .lock()
            .binding_conflicts(&identity_hash.to_ascii_lowercase(), name_hash)
    }

    #[cfg(test)]
    pub(crate) fn is_trusted_destination(
        &self,
        identity_hash: &str,
        destination_hash: &str,
    ) -> bool {
        self.authorize(identity_hash, destination_hash).decision == Decision::Allow
    }

    /// The identity-tier gate: known (on disk or for the session) and not blocked, so its
    /// instances may knock. It says nothing about which destinations are allowed; that is
    /// `authorize`.
    #[cfg(test)]
    pub(crate) fn is_trusted_identity(&self, identity_hash: &str) -> bool {
        matches!(
            self.identity_standing(identity_hash),
            IdentityStanding::Trusted { .. }
        )
    }

    /// A block wins over any record; a session-only record never carries `all_destinations`.
    pub(crate) fn identity_standing(&self, identity_hash: &str) -> IdentityStanding {
        let identity = identity_hash.to_ascii_lowercase();
        let state = self.inner.lock();
        if state.file.blocked_identities.contains_key(&identity) {
            return IdentityStanding::Blocked;
        }
        if let Some(entry) = state.file.identities.get(&identity) {
            return IdentityStanding::Trusted {
                all_destinations: entry.all_destinations,
            };
        }
        if state.session_identities.contains_key(&identity) {
            return IdentityStanding::Trusted {
                all_destinations: false,
            };
        }
        IdentityStanding::Unknown
    }

    pub(crate) fn is_blocked_identity(&self, identity_hash: &str) -> bool {
        self.inner
            .lock()
            .file
            .blocked_identities
            .contains_key(&identity_hash.to_ascii_lowercase())
    }

    /// Every identity and destination record, disk first then session, each section in key
    /// order. A denied destination is still listed, flagged, so the user can see what a
    /// refusal is holding back; a deny with no record of its own is listed too, so the
    /// enumeration is the whole picture.
    pub(crate) fn records(&self) -> Vec<TrustRecord> {
        let state = self.inner.lock();
        let identity_records = |entries: &BTreeMap<String, IdentityEntry>, session: bool| {
            entries
                .iter()
                .map(|(hash, entry)| TrustRecord {
                    tier: Tier::Identity,
                    hash: hash.clone(),
                    identity: None,
                    label: entry.label.clone(),
                    note: entry.note.clone(),
                    added_at: entry.added_at.0,
                    last_seen_at: entry.last_seen_at.0,
                    all_destinations: entry.all_destinations,
                    denied: false,
                    session,
                    key_changed: None,
                })
                .collect::<Vec<_>>()
        };
        let destination_records = |entries: &BTreeMap<String, DestinationEntry>, session: bool| {
            entries
                .iter()
                .map(|(hash, entry)| TrustRecord {
                    tier: Tier::Destination,
                    hash: hash.clone(),
                    identity: Some(entry.identity.clone()),
                    label: entry.label.clone(),
                    note: entry.note.clone(),
                    added_at: entry.added_at.0,
                    last_seen_at: state.effective_last_seen(hash, entry),
                    all_destinations: false,
                    denied: state.file.denied_destinations.contains_key(hash),
                    session,
                    key_changed: entry.key_changed.as_ref().map(|mark| KeyChange {
                        seen_identity: mark.seen_identity.clone(),
                        at: mark.at.0,
                    }),
                })
                .collect::<Vec<_>>()
        };
        let mut records = identity_records(&state.file.identities, false);
        records.extend(destination_records(&state.file.destinations, false));
        records.extend(identity_records(&state.session_identities, true));
        records.extend(destination_records(&state.session_destinations, true));
        records.extend(
            state
                .file
                .denied_destinations
                .iter()
                .filter(|(hash, _)| {
                    !state.file.destinations.contains_key(*hash)
                        && !state.session_destinations.contains_key(*hash)
                })
                .map(|(hash, entry)| TrustRecord {
                    tier: Tier::Destination,
                    hash: hash.clone(),
                    identity: None,
                    label: None,
                    note: entry.note.clone(),
                    added_at: entry.added_at.0,
                    last_seen_at: entry.added_at.0,
                    all_destinations: false,
                    denied: true,
                    session: false,
                    key_changed: None,
                }),
        );
        records
    }

    pub(crate) fn blocked(&self) -> Vec<OverlayRecord> {
        overlay_records(&self.inner.lock().file.blocked_identities)
    }

    /// The deny overlay on its own; `records()` carries it as the `denied` flag.
    #[cfg(test)]
    pub(crate) fn denied(&self) -> Vec<OverlayRecord> {
        overlay_records(&self.inner.lock().file.denied_destinations)
    }

    /// Notes a fresh sighting of a trusted destination in memory only; the file keeps the
    /// value from the last mutation so announces never cause writes. The sighting counts
    /// only under the proof `records` and `prune_destinations` demand of an announce:
    /// `identity_hash` and `name_hash` must derive the destination, and that identity must
    /// be the one the record is bound to. A destination without a record, or an announce
    /// that fails the proof, is ignored, so a stranger reusing the hash cannot keep the
    /// record looking alive.
    pub(crate) fn mark_seen(
        &self,
        destination_hash: &str,
        identity_hash: &str,
        name_hash: &str,
        at: SystemTime,
    ) {
        let destination = destination_hash.to_ascii_lowercase();
        let mut state = self.inner.lock();
        let Some(bound) = state
            .file
            .destinations
            .get(&destination)
            .or_else(|| state.session_destinations.get(&destination))
            .map(|entry| entry.identity.clone())
        else {
            return;
        };
        let provable = verify_binding(&destination, identity_hash, name_hash, "the announce")
            .is_ok_and(|identity| same_hash(&identity.to_hex_string(), &bound));
        if !provable {
            return;
        }
        state
            .seen
            .entry(destination)
            .and_modify(|seen| *seen = (*seen).max(at))
            .or_insert(at);
    }

    /// Marks every trusted destination record whose instance `name_hash` names when it is
    /// seen under `identity_hash` rather than the identity the record is bound to, and
    /// surfaces one line per record newly marked. A record already marked keeps its first
    /// sighting and is neither written nor reported again, so a repeated or forged announce
    /// costs nothing; session records are marked in memory only. The grant is untouched:
    /// the bound identity still proves its own key. An unreadable hash marks nothing, and
    /// neither does a blocked `identity_hash`: the human has already answered it. The same
    /// goes for a denied record and for an `identity_hash` the human has already trusted,
    /// for the instance or for all destinations (`State::binding_conflicts`).
    pub(crate) fn note_key_change(
        &self,
        identity_hash: &str,
        name_hash: &str,
        now: SystemTime,
    ) -> Vec<MarkedKeyChange> {
        let (Some(seen), Some(name_hash)) =
            (parse_hash(identity_hash), decode_name_hash(name_hash))
        else {
            return Vec::new();
        };
        let seen_identity = seen.to_hex_string();
        let marked = {
            let mut state = self.inner.lock();
            if state.file.blocked_identities.contains_key(&seen_identity) {
                return Vec::new();
            }
            let fresh: Vec<BindingConflict> = state
                .binding_conflicts(&seen_identity, &name_hash)
                .into_iter()
                .filter(|conflict| !conflict.already_marked)
                .collect();
            if fresh.is_empty() {
                return Vec::new();
            }
            let mark = KeyChanged {
                seen_identity: seen_identity.clone(),
                at: Stamp::new(now),
            };
            let mut file = state.file.clone();
            let on_disk: Vec<String> = fresh
                .iter()
                .filter_map(|conflict| {
                    let entry = file.destinations.get_mut(&conflict.destination_hash)?;
                    entry.key_changed = Some(mark.clone());
                    Some(conflict.destination_hash.clone())
                })
                .collect();
            if !on_disk.is_empty() {
                let mutation = TrustMutation::KeyChanged {
                    destinations: on_disk,
                    seen_identity: seen_identity.clone(),
                };
                let summary = mutation.to_string();
                if let Err(err) = self.commit(&mut state, file, mutation) {
                    warn!(
                        "Mesh trust list could not record a key change: {}",
                        redact_hashes(&format!("{err:#}"))
                    );
                    return Vec::new();
                }
                info!("Mesh {summary}");
            }
            for conflict in &fresh {
                if let Some(entry) = state
                    .session_destinations
                    .get_mut(&conflict.destination_hash)
                {
                    entry.key_changed = Some(mark.clone());
                }
            }
            fresh
                .into_iter()
                .map(|conflict| MarkedKeyChange {
                    destination_hash: conflict.destination_hash,
                    bound_identity: conflict.bound_identity,
                    seen_identity: seen_identity.clone(),
                    label: conflict.label,
                })
                .collect::<Vec<_>>()
        };
        let new_destination = destination_address(&name_hash, &seen).to_hex_string();
        self.surface_key_changes(&marked, &new_destination);
        marked
    }

    /// One line per newly marked record, offered after the store lock is released. Nothing
    /// in the line comes from the peer: the label is the human's own, admitted by
    /// `check_text`, and the rest is hashes. A dropped line is not offered again; the mark
    /// stands and `.mesh peers` shows it.
    fn surface_key_changes(&self, marked: &[MarkedKeyChange], new_destination: &str) {
        let surface = self.surface.lock().as_ref().and_then(Weak::upgrade);
        for change in marked {
            let Some(surface) = &surface else {
                debug!(
                    "Mesh key change on destination {} was not surfaced: nothing is attached",
                    short(&change.destination_hash)
                );
                continue;
            };
            let shown = surface.surface(IdleNotify {
                source: Source::Mesh,
                origin: Origin::Peer(short(&change.seen_identity).to_string()),
                text: key_change_text(change, new_destination),
                model_note: None,
            });
            if !shown {
                warn!(
                    "Mesh key-change notice for {} was dropped; the mark stands and .mesh peers shows it",
                    short(&change.destination_hash)
                );
            }
        }
    }

    /// Trusts one destination by proving which identity announced it, and makes sure that
    /// identity has a record (without `all_destinations`, which only `trust_identity` sets).
    /// The identity comes from the announce's or the knock's own hashes, never from the
    /// caller. Re-trusting a destination clears its key-change mark, as does trusting the
    /// new destination of an instance whose old records carry one: the human has affirmed
    /// the binding.
    pub(crate) fn trust_destination(
        &self,
        mesh: &dyn LiveMesh,
        destination_hash: &str,
        opts: TrustOptions,
        now: SystemTime,
    ) -> Result<TrustOutcome> {
        let peers = live(mesh)?;
        check_options(&opts)?;
        let peer = resolve_destination(mesh, &peers, destination_hash, now)?;
        let mut state = self.inner.lock();
        let mut file = state.file.clone();
        refuse_if_blocked(&file, &peer.identity_hash)?;
        let change = match file.destinations.get_mut(&peer.destination_hash) {
            Some(entry) => {
                entry.identity = peer.identity_hash.clone();
                entry.last_seen_at = Stamp::new(peer.last_seen);
                entry.key_changed = None;
                apply_options(&mut entry.label, &mut entry.note, opts);
                TrustChange::Updated
            }
            None => {
                file.destinations.insert(
                    peer.destination_hash.clone(),
                    DestinationEntry {
                        identity: peer.identity_hash.clone(),
                        added_at: Stamp::new(now),
                        last_seen_at: Stamp::new(peer.last_seen),
                        label: opts.label,
                        note: opts.note,
                        key_changed: None,
                    },
                );
                TrustChange::Added
            }
        };
        let superseded: Vec<String> = decode_name_hash(&peer.name_hash)
            .map(|name_hash| state.binding_conflicts(&peer.identity_hash, &name_hash))
            .unwrap_or_default()
            .into_iter()
            .map(|conflict| conflict.destination_hash)
            .collect();
        for destination in &superseded {
            if let Some(entry) = file.destinations.get_mut(destination) {
                entry.key_changed = None;
            }
        }
        file.identities
            .entry(peer.identity_hash.clone())
            .or_insert_with(|| identity_entry(now, peer.last_seen, false));
        let deny_lifted = file
            .denied_destinations
            .remove(&peer.destination_hash)
            .is_some();
        self.commit(
            &mut state,
            file,
            TrustMutation::TrustDestination {
                destination: peer.destination_hash.clone(),
                identity: peer.identity_hash.clone(),
                change,
            },
        )?;
        for destination in &superseded {
            if let Some(entry) = state.session_destinations.get_mut(destination) {
                entry.key_changed = None;
            }
        }
        // Both now live on disk, so a session twin would list the same hash twice.
        state.session_destinations.remove(&peer.destination_hash);
        state.session_identities.remove(&peer.identity_hash);
        Ok(TrustOutcome {
            identity_hash: peer.identity_hash,
            destination_hash: peer.destination_hash,
            change,
            superseded,
            deny_lifted,
        })
    }

    /// Same proof as `trust_destination`, kept in memory only: the pair is forgotten when
    /// the process ends and never reaches `trust.yaml`. A hash already on disk gets no
    /// session twin: the disk record is the stronger one and lists once.
    #[cfg(test)]
    pub(crate) fn trust_destination_for_session(
        &self,
        mesh: &dyn LiveMesh,
        destination_hash: &str,
        now: SystemTime,
    ) -> Result<TrustOutcome> {
        let peers = live(mesh)?;
        let peer = resolve_destination(mesh, &peers, destination_hash, now)?;
        let mut state = self.inner.lock();
        refuse_if_blocked(&state.file, &peer.identity_hash)?;
        if state.file.destinations.contains_key(&peer.destination_hash) {
            return Ok(TrustOutcome {
                identity_hash: peer.identity_hash,
                destination_hash: peer.destination_hash,
                change: TrustChange::Updated,
                superseded: Vec::new(),
                deny_lifted: false,
            });
        }
        let change = match state.session_destinations.get_mut(&peer.destination_hash) {
            Some(entry) => {
                entry.identity = peer.identity_hash.clone();
                entry.last_seen_at = Stamp::new(peer.last_seen);
                entry.key_changed = None;
                TrustChange::Updated
            }
            None => {
                state.session_destinations.insert(
                    peer.destination_hash.clone(),
                    DestinationEntry {
                        identity: peer.identity_hash.clone(),
                        added_at: Stamp::new(now),
                        last_seen_at: Stamp::new(peer.last_seen),
                        label: None,
                        note: None,
                        key_changed: None,
                    },
                );
                TrustChange::Added
            }
        };
        if !state.file.identities.contains_key(&peer.identity_hash) {
            state
                .session_identities
                .entry(peer.identity_hash.clone())
                .or_insert_with(|| identity_entry(now, peer.last_seen, false));
        }
        Ok(TrustOutcome {
            identity_hash: peer.identity_hash,
            destination_hash: peer.destination_hash,
            change,
            superseded: Vec::new(),
            deny_lifted: false,
        })
    }

    /// Trusts every destination of an identity (`--identity`): the record's
    /// `all_destinations` flag is set, or the record is created with it set. Only peer rows
    /// whose destination the identity provably derives count towards `last_seen_at`; the
    /// identity column alone is a claim. Every key-change mark that named this identity as
    /// the one seen is cleared: the human has answered for every instance of it, and
    /// `binding_conflicts` would never re-evaluate the mark for an identity trusted for all.
    pub(crate) fn trust_identity(
        &self,
        mesh: &dyn LiveMesh,
        identity_hash: &str,
        opts: TrustOptions,
        now: SystemTime,
    ) -> Result<TrustChange> {
        let peers = live(mesh)?;
        let identity = valid_hash("identity", identity_hash)?;
        check_options(&opts)?;
        let last_seen = peers
            .snapshot()
            .iter()
            .filter(|peer| peer.identity_hash == identity && verified_identity(peer).is_ok())
            .map(|peer| peer.last_seen)
            .max();
        let mut state = self.inner.lock();
        let mut file = state.file.clone();
        refuse_if_blocked(&file, &identity)?;
        let (change, granted) = match file.identities.get_mut(&identity) {
            Some(entry) => {
                let granted = !entry.all_destinations;
                entry.all_destinations = true;
                if let Some(seen) = last_seen {
                    entry.last_seen_at = entry.last_seen_at.max(Stamp::new(seen));
                }
                apply_options(&mut entry.label, &mut entry.note, opts);
                (TrustChange::Updated, granted)
            }
            None => {
                let mut entry = identity_entry(now, last_seen.unwrap_or(now), true);
                entry.label = opts.label;
                entry.note = opts.note;
                file.identities.insert(identity.clone(), entry);
                (TrustChange::Added, true)
            }
        };
        clear_marks_seen_under(file.destinations.values_mut(), &identity);
        self.commit(
            &mut state,
            file,
            TrustMutation::TrustIdentity {
                identity: identity.clone(),
                granted,
            },
        )?;
        // The identity now lives on disk, so a session twin would list the same hash twice.
        state.session_identities.remove(&identity);
        clear_marks_seen_under(state.session_destinations.values_mut(), &identity);
        Ok(change)
    }

    /// Removes one destination record; the identity record stays. When that identity is
    /// trusted for every destination, removing the record alone would change nothing, so
    /// the destination is denied instead and nothing is removed: the identity record keeps
    /// its standing, the destination record (if any) keeps its label and binding, and
    /// `trust_destination` lifts the deny on the intact record. The identity is read from
    /// the destination's record, its session twin, or a peer table row whose binding
    /// verifies, so a destination never trusted on its own can still be refused; it gets a
    /// record bound to the identity the peer table proves, written with the deny, so
    /// forgetting the identity forgets the refusal too. `dry_run` reports the outcome and
    /// writes nothing.
    pub(crate) fn untrust_destination(
        &self,
        mesh: &dyn LiveMesh,
        destination_hash: &str,
        now: SystemTime,
        dry_run: bool,
    ) -> Result<UntrustOutcome> {
        let peers = live(mesh)?;
        let destination = normalize_hash(destination_hash);
        let mut state = self.inner.lock();
        let mut file = state.file.clone();
        let resolved = file
            .destinations
            .get(&destination)
            .or_else(|| state.session_destinations.get(&destination))
            .map(|entry| (entry.identity.clone(), entry.last_seen_at.0))
            .or_else(|| {
                peers.get(&destination).and_then(|peer| {
                    verified_identity(&peer)
                        .ok()
                        .map(|identity| (identity.to_hex_string(), peer.last_seen))
                })
            });
        let Some((identity, last_seen)) = resolved else {
            bail!(
                "Destination {destination} is not in the trust list, so there is nothing to untrust."
            );
        };
        let trusted_all = file
            .identities
            .get(&identity)
            .is_some_and(|entry| entry.all_destinations);
        if trusted_all {
            let already_refused = file.denied_destinations.contains_key(&destination);
            if dry_run || already_refused {
                return Ok(UntrustOutcome::Refused {
                    identity,
                    already_refused,
                });
            }
            file.destinations
                .entry(destination.clone())
                .or_insert_with(|| DestinationEntry {
                    identity: identity.clone(),
                    added_at: Stamp::new(now),
                    last_seen_at: Stamp::new(last_seen),
                    label: None,
                    note: None,
                    key_changed: None,
                });
            upsert_overlay(
                &mut file.denied_destinations,
                destination.clone(),
                None,
                now,
            );
            self.commit(
                &mut state,
                file,
                TrustMutation::RefuseDestination {
                    destination: destination.clone(),
                },
            )?;
            // The record now lives on disk, so a session twin would list the same hash twice.
            state.session_destinations.remove(&destination);
            return Ok(UntrustOutcome::Refused {
                identity,
                already_refused,
            });
        }
        let on_disk = file.destinations.remove(&destination);
        let in_session = state.session_destinations.contains_key(&destination);
        if on_disk.is_none() && !in_session {
            bail!(
                "Destination {destination} is not in the trust list, so there is nothing to untrust."
            );
        }
        if dry_run {
            return Ok(UntrustOutcome::Forgotten { identity });
        }
        if on_disk.is_some() {
            let mutation = TrustMutation::UntrustDestination {
                destination: destination.clone(),
                identity: identity.clone(),
            };
            self.commit(&mut state, file, mutation)?;
        }
        state.forget_destination(&destination);
        Ok(UntrustOutcome::Forgotten { identity })
    }

    /// Removes an identity record and every destination bound to it, denies included;
    /// returns the removed destination hashes.
    pub(crate) fn untrust_identity(
        &self,
        mesh: &dyn LiveMesh,
        identity_hash: &str,
    ) -> Result<Vec<String>> {
        live(mesh)?;
        let identity = normalize_hash(identity_hash);
        let mut state = self.inner.lock();
        let mut file = state.file.clone();
        let on_disk = file.identities.remove(&identity);
        let in_session = state.session_identities.contains_key(&identity);
        if on_disk.is_none() && !in_session {
            bail!("Identity {identity} is not in the trust list, so there is nothing to untrust.");
        }
        let removed = remove_destinations_of(&mut file, &identity);
        for hash in &removed {
            file.denied_destinations.remove(hash);
        }
        if on_disk.is_some() || !removed.is_empty() {
            self.commit(
                &mut state,
                file,
                TrustMutation::UntrustIdentity {
                    identity: identity.clone(),
                    was_granting: on_disk.is_some_and(|entry| entry.all_destinations),
                    removed: removed.clone(),
                },
            )?;
        }
        state.forget_identity(&identity, &removed);
        Ok(removed)
    }

    /// Suppresses an identity's knocks and drops its trust records, on disk and for the
    /// session; returns the removed destination hashes. Blocking is the one mutation that
    /// works on an identity the trust list has never seen. It also clears every key-change
    /// mark that named this identity as the one seen: the block is the notice's own remedy,
    /// and a later sighting under yet another identity can mark the record again.
    pub(crate) fn block_identity(
        &self,
        mesh: &dyn LiveMesh,
        identity_hash: &str,
        note: Option<String>,
        now: SystemTime,
    ) -> Result<Vec<String>> {
        live(mesh)?;
        let identity = valid_hash("identity", identity_hash)?;
        check_text("note", note.as_deref())?;
        let mut state = self.inner.lock();
        let mut file = state.file.clone();
        let was_granting = file
            .identities
            .remove(&identity)
            .is_some_and(|entry| entry.all_destinations);
        let removed = remove_destinations_of(&mut file, &identity);
        let marks_cleared = clear_marks_seen_under(file.destinations.values_mut(), &identity);
        upsert_overlay(&mut file.blocked_identities, identity.clone(), note, now);
        self.commit(
            &mut state,
            file,
            TrustMutation::BlockIdentity {
                identity: identity.clone(),
                was_granting,
                removed: removed.clone(),
                marks_cleared,
            },
        )?;
        state.forget_identity(&identity, &removed);
        clear_marks_seen_under(state.session_destinations.values_mut(), &identity);
        Ok(removed)
    }

    /// Removes the block only; trust has to be granted again explicitly.
    pub(crate) fn unblock_identity(&self, mesh: &dyn LiveMesh, identity_hash: &str) -> Result<()> {
        live(mesh)?;
        let identity = normalize_hash(identity_hash);
        let mut state = self.inner.lock();
        let mut file = state.file.clone();
        if file.blocked_identities.remove(&identity).is_none() {
            bail!("Identity {identity} is not blocked, so there is nothing to unblock.");
        }
        self.commit(
            &mut state,
            file,
            TrustMutation::UnblockIdentity { identity },
        )
    }

    /// Seeds the deny overlay for a destination regardless of its identity's standing; the
    /// destination's own trust record, if any, is kept. The REPL has no verb for this:
    /// `untrust_destination` writes the overlay when the identity is trusted for all.
    #[cfg(test)]
    pub(crate) fn deny_destination(
        &self,
        mesh: &dyn LiveMesh,
        destination_hash: &str,
        note: Option<String>,
        now: SystemTime,
    ) -> Result<()> {
        live(mesh)?;
        let destination = valid_hash("destination", destination_hash)?;
        check_text("note", note.as_deref())?;
        let mut state = self.inner.lock();
        let mut file = state.file.clone();
        upsert_overlay(
            &mut file.denied_destinations,
            destination.clone(),
            note,
            now,
        );
        self.commit(&mut state, file, TrustMutation::Deny { destination })
    }

    #[cfg(test)]
    pub(crate) fn undeny_destination(
        &self,
        mesh: &dyn LiveMesh,
        destination_hash: &str,
    ) -> Result<()> {
        live(mesh)?;
        let destination = normalize_hash(destination_hash);
        let mut state = self.inner.lock();
        let mut file = state.file.clone();
        if file.denied_destinations.remove(&destination).is_none() {
            bail!("Destination {destination} is not denied, so there is nothing to undeny.");
        }
        self.commit(&mut state, file, TrustMutation::Undeny { destination })
    }

    /// Destination records not seen for `older_than`, judged on the freshest of the disk
    /// `last_seen_at`, the in-memory sighting and a peer-table announce whose identity is
    /// verified to be the record's; removed unless `dry_run`. Identities are never pruned:
    /// they are the user's statement about a person, not about an instance that may be gone.
    pub(crate) fn prune_destinations(
        &self,
        mesh: &dyn LiveMesh,
        older_than: Duration,
        now: SystemTime,
        dry_run: bool,
    ) -> Result<Vec<String>> {
        let peers = live(mesh)?;
        let mut state = self.inner.lock();
        let stale: Vec<String> = state
            .file
            .destinations
            .iter()
            .filter(|(hash, entry)| {
                let on_record = state.effective_last_seen(hash, entry);
                let last_seen = verified_announce(&peers, hash, entry)
                    .map_or(on_record, |announced| announced.max(on_record));
                // A sighting in the future (clock stepped back) reads as just seen.
                now.duration_since(last_seen).unwrap_or_default() >= older_than
            })
            .map(|(hash, _)| hash.clone())
            .collect();
        if dry_run || stale.is_empty() {
            return Ok(stale);
        }
        let mut file = state.file.clone();
        let pruned = stale
            .iter()
            .filter_map(|hash| {
                file.destinations
                    .remove(hash)
                    .map(|entry| (hash.clone(), entry.identity))
            })
            .collect();
        self.commit(&mut state, file, TrustMutation::Prune { stale: pruned })?;
        for hash in &stale {
            state.seen.remove(hash);
        }
        Ok(stale)
    }

    /// Writes `file` and only then makes it the current list, so a failed write leaves
    /// memory and disk agreeing on the previous state. The observer hears only about a
    /// list that is on disk.
    fn commit(&self, state: &mut State, file: TrustFile, mutation: TrustMutation) -> Result<()> {
        self.persist(&file)?;
        state.file = file;
        debug!("Mesh trust list updated: {mutation}");
        // Trust events fire from the store's mutation API so every writer is covered; the
        // .mesh REPL verbs never fire them directly.
        if let Some(observer) = self.observer.load_full() {
            mutation.notify(&**observer);
        }
        Ok(())
    }

    /// The only writer of `trust.yaml`.
    fn persist(&self, file: &TrustFile) -> Result<()> {
        let yaml =
            serde_yaml::to_string(file).context("Failed to serialize the mesh trust list")?;
        write_atomically(&self.path, yaml.as_bytes())
    }
}

impl State {
    fn effective_last_seen(&self, destination_hash: &str, entry: &DestinationEntry) -> SystemTime {
        self.seen
            .get(destination_hash)
            .map_or(entry.last_seen_at.0, |seen| {
                (*seen).max(entry.last_seen_at.0)
            })
    }

    /// A destination that has just lost its record also loses its in-memory sighting, so a
    /// later re-trust starts from the peer table rather than the old overlay.
    fn forget_destination(&mut self, destination: &str) {
        self.session_destinations.remove(destination);
        self.seen.remove(destination);
    }

    /// `removed` is the on-disk destinations the caller already dropped; the session ones
    /// bound to `identity` go with them.
    fn forget_identity(&mut self, identity: &str, removed: &[String]) {
        self.session_identities.remove(identity);
        let in_session: Vec<String> = self
            .session_destinations
            .iter()
            .filter(|(_, entry)| entry.identity == identity)
            .map(|(hash, _)| hash.clone())
            .collect();
        for hash in removed.iter().chain(&in_session) {
            self.forget_destination(hash);
        }
    }

    /// `identity` is canonical lower hex. An entry whose bound identity does not parse is
    /// skipped: `open` refuses such a file, so none exists, but nothing here panics. Nothing
    /// conflicts once `identity` holds its own trusted record for the instance, both
    /// bindings then being the human's, nor once `identity` is trusted for all
    /// destinations, the human having answered for every instance of it; a denied record
    /// is left out, the human having answered it.
    fn binding_conflicts(
        &self,
        identity: &str,
        name_hash: &[u8; NAME_HASH_LEN],
    ) -> Vec<BindingConflict> {
        let trusted_for_all = self
            .file
            .identities
            .get(identity)
            .is_some_and(|entry| entry.all_destinations);
        if trusted_for_all || self.holds_instance(identity, name_hash) {
            return Vec::new();
        }
        self.file
            .destinations
            .iter()
            .chain(&self.session_destinations)
            .filter(|(hash, _)| !self.file.denied_destinations.contains_key(*hash))
            .filter(|(_, entry)| !same_hash(&entry.identity, identity))
            .filter(|(hash, entry)| {
                parse_hash(&entry.identity).is_some_and(|bound| {
                    parse_hash(hash) == Some(destination_address(name_hash, &bound))
                })
            })
            .map(|(hash, entry)| BindingConflict {
                destination_hash: hash.clone(),
                bound_identity: entry.identity.clone(),
                label: entry.label.clone(),
                already_marked: entry.key_changed.is_some(),
            })
            .collect()
    }

    /// Whether `identity` is bound, on disk or for the session, to the destination it
    /// derives from `name_hash`.
    fn holds_instance(&self, identity: &str, name_hash: &[u8; NAME_HASH_LEN]) -> bool {
        parse_hash(identity).is_some_and(|seen| {
            let own = destination_address(name_hash, &seen).to_hex_string();
            self.file
                .destinations
                .get(&own)
                .or_else(|| self.session_destinations.get(&own))
                .is_some_and(|entry| same_hash(&entry.identity, identity))
        })
    }
}

/// Reads the version alone first so a file from a newer Coyote gets a precise message
/// rather than an unknown-field error from whatever the newer layout added.
fn parse_trust_file(path: &Path, text: &str) -> Result<TrustFile> {
    let probe: VersionProbe = serde_yaml::from_str(text).with_context(|| {
        unversioned_refusal(
            "trust list",
            path,
            None,
            TRUST_FILE_VERSION,
            TRUST_FILE_REMEDY,
        )
    })?;
    if probe.version != TRUST_FILE_VERSION {
        bail!(version_refusal(
            "trust list",
            path,
            None,
            probe.version,
            TRUST_FILE_VERSION,
            TRUST_FILE_REMEDY
        ));
    }
    let file: TrustFile = serde_yaml::from_str(text).with_context(|| {
        format!(
            "Mesh trust list '{}' could not be parsed as version {TRUST_FILE_VERSION}. {}",
            path.display(),
            TRUST_FILE_REMEDY.sentence()
        )
    })?;
    refuse_non_canonical_keys(path, &file)?;
    refuse_dangling_identities(path, &file)?;
    Ok(file)
}

/// Queries lower-case their inputs and look keys up verbatim, so a hand-edited key that is
/// not lower hex would never match and a deny or block written that way would be inert.
fn refuse_non_canonical_keys(path: &Path, file: &TrustFile) -> Result<()> {
    let hashes = file
        .identities
        .keys()
        .map(|hash| ("identities", hash))
        .chain(file.destinations.keys().map(|hash| ("destinations", hash)))
        .chain(
            file.destinations
                .values()
                .map(|entry| ("destinations (identity field)", &entry.identity)),
        )
        .chain(
            file.destinations
                .values()
                .filter_map(|entry| entry.key_changed.as_ref())
                .map(|mark| {
                    (
                        "destinations (key_changed.seen_identity field)",
                        &mark.seen_identity,
                    )
                }),
        )
        .chain(
            file.denied_destinations
                .keys()
                .map(|hash| ("denied_destinations", hash)),
        )
        .chain(
            file.blocked_identities
                .keys()
                .map(|hash| ("blocked_identities", hash)),
        );
    for (section, hash) in hashes {
        if canonical_hash(hash).as_deref() != Some(hash.as_str()) {
            bail!(
                "Mesh trust list '{}' has a non-canonical hash '{hash}' under `{section}`: expected 32 lowercase hex characters. Fix the key, or move the file aside to start a fresh trust list.",
                path.display()
            );
        }
    }
    Ok(())
}

/// Every destination the store writes has its identity's record beside it, so a
/// destination bound to an identity the file does not list is a hand edit gone wrong.
fn refuse_dangling_identities(path: &Path, file: &TrustFile) -> Result<()> {
    for (destination, entry) in &file.destinations {
        if !file.identities.contains_key(&entry.identity) {
            bail!(
                "Mesh trust list '{}' binds destination {destination} to identity {} but has no such entry under `identities`. Fix the file, or move it aside to start a fresh trust list.",
                path.display(),
                entry.identity
            );
        }
    }
    Ok(())
}

fn live(mesh: &dyn LiveMesh) -> Result<Arc<PeerTable>> {
    mesh.peers().ok_or_else(|| anyhow!(MESH_OFF))
}

/// When the peer table's announce for `destination_hash` provably comes from the identity
/// the record is bound to, the time of that announce. A stranger reusing the hash, or an
/// announce that cannot be verified, is no evidence the trusted instance is still around.
fn verified_announce(
    peers: &PeerTable,
    destination_hash: &str,
    entry: &DestinationEntry,
) -> Option<SystemTime> {
    let record = peers.get(destination_hash)?;
    verified_identity(&record)
        .ok()
        .filter(|identity| same_hash(&identity.to_hex_string(), &entry.identity))
        .map(|_| record.last_seen)
}

fn normalize_hash(input: &str) -> String {
    input.trim().to_ascii_lowercase()
}

/// The lower-hex form of a hash the user typed, refused unless it is a well-formed address
/// hash so a typo cannot become a key in the file.
fn valid_hash(what: &str, input: &str) -> Result<String> {
    canonical_hash(&normalize_hash(input))
        .ok_or_else(|| anyhow!("'{input}' is not a valid {what} hash: expected 32 hex characters."))
}

pub(crate) fn parse_hash(text: &str) -> Option<AddressHash> {
    AddressHash::new_from_hex_string(&canonical_hash(text)?).ok()
}

fn check_options(opts: &TrustOptions) -> Result<()> {
    check_text("label", opts.label.as_deref())?;
    check_text("note", opts.note.as_deref())
}

/// The trust list is shown back to the user, so a label or note must not be able to carry a
/// terminal control sequence or a direction override into that listing, nor flood it.
fn check_text(field: &str, text: Option<&str>) -> Result<()> {
    if text.is_some_and(|text| text.chars().count() > 256) {
        bail!("The {field} is longer than 256 characters; nothing was changed.");
    }
    if text.is_some_and(|text| text.chars().any(is_control_or_invisible)) {
        bail!("The {field} contains control or invisible characters; nothing was changed.");
    }
    Ok(())
}

fn refuse_if_blocked(file: &TrustFile, identity: &str) -> Result<()> {
    if file.blocked_identities.contains_key(identity) {
        bail!(
            "Identity {identity} is blocked. Run `.mesh unblock {identity}` first if you want to trust it."
        );
    }
    Ok(())
}

fn identity_entry(now: SystemTime, last_seen: SystemTime, all_destinations: bool) -> IdentityEntry {
    IdentityEntry {
        added_at: Stamp::new(now),
        last_seen_at: Stamp::new(last_seen),
        label: None,
        note: None,
        all_destinations,
    }
}

fn apply_options(label: &mut Option<String>, note: &mut Option<String>, opts: TrustOptions) {
    if opts.label.is_some() {
        *label = opts.label;
    }
    if opts.note.is_some() {
        *note = opts.note;
    }
}

fn upsert_overlay(
    overlay: &mut BTreeMap<String, OverlayEntry>,
    hash: String,
    note: Option<String>,
    now: SystemTime,
) {
    match overlay.get_mut(&hash) {
        Some(entry) => {
            if note.is_some() {
                entry.note = note;
            }
        }
        None => {
            overlay.insert(
                hash,
                OverlayEntry {
                    added_at: Stamp::new(now),
                    note,
                },
            );
        }
    }
}

fn remove_destinations_of(file: &mut TrustFile, identity: &str) -> Vec<String> {
    let bound: Vec<String> = file
        .destinations
        .iter()
        .filter(|(_, entry)| entry.identity == identity)
        .map(|(hash, _)| hash.clone())
        .collect();
    for hash in &bound {
        file.destinations.remove(hash);
    }
    bound
}

/// Clears the key-change marks that name `identity` as the one seen; returns how many.
fn clear_marks_seen_under<'a>(
    entries: impl Iterator<Item = &'a mut DestinationEntry>,
    identity: &str,
) -> usize {
    let mut cleared = 0;
    for entry in entries {
        if entry
            .key_changed
            .as_ref()
            .is_some_and(|mark| mark.seen_identity == identity)
        {
            entry.key_changed = None;
            cleared += 1;
        }
    }
    cleared
}

fn overlay_records(overlay: &BTreeMap<String, OverlayEntry>) -> Vec<OverlayRecord> {
    overlay
        .iter()
        .map(|(hash, entry)| OverlayRecord {
            hash: hash.clone(),
            added_at: entry.added_at.0,
            note: entry.note.clone(),
        })
        .collect()
}

struct ResolvedPeer {
    destination_hash: String,
    identity_hash: String,
    /// Lower hex, verified to derive `destination_hash` with `identity_hash`.
    name_hash: String,
    last_seen: SystemTime,
}

/// Finds the announce behind `destination_hash` in the peer table, or failing that its
/// knock in the cache, and proves the identity it recorded derives that destination.
fn resolve_destination(
    mesh: &dyn LiveMesh,
    peers: &PeerTable,
    destination_hash: &str,
    now: SystemTime,
) -> Result<ResolvedPeer> {
    let destination = normalize_hash(destination_hash);
    if let Some(record) = peers.get(&destination) {
        let identity = verified_identity(&record)?;
        return Ok(ResolvedPeer {
            destination_hash: destination,
            identity_hash: identity.to_hex_string(),
            name_hash: record.name_hash.clone(),
            last_seen: record.last_seen,
        });
    }
    let Some(knock) = mesh.knock(&destination, now) else {
        bail!(
            "Destination {destination} is not in the peer table and has not knocked. Run `.mesh peers` to see the nodes that have announced or `.mesh knocks` to see who asked to be trusted, and trust one of those."
        );
    };
    if knock.name_hash.is_empty() {
        bail!(
            "Instance {destination} knocked before its name hash was kept, so its identity cannot be verified; wait for it to knock again."
        );
    }
    let identity = verify_binding(
        &destination,
        &knock.identity_hash,
        &knock.name_hash,
        "the knock cache",
    )?;
    Ok(ResolvedPeer {
        destination_hash: destination,
        identity_hash: identity.to_hex_string(),
        name_hash: knock.name_hash,
        last_seen: knock.last_seen,
    })
}

/// The identity column is data a peer sent; the derivation is what makes it the peer's own.
fn verified_identity(record: &PeerRecord) -> Result<AddressHash> {
    let destination = &record.destination_hash;
    if record.name_hash.is_empty() {
        bail!(
            "Peer {destination} was recorded before its name hash was kept, so its identity cannot be verified yet. Wait for its next announce and try again."
        );
    }
    verify_binding(
        destination,
        &record.identity_hash,
        &record.name_hash,
        "the peer table",
    )
}

/// Reticulum's own derivation, run on the hashes `source` recorded: `identity_hash` is the
/// destination's owner only if it and `name_hash` derive `destination`.
fn verify_binding(
    destination: &str,
    identity_hash: &str,
    name_hash: &str,
    source: &str,
) -> Result<AddressHash> {
    let name_hash = decode_name_hash(name_hash).ok_or_else(|| {
        anyhow!("The name hash {source} holds for {destination} is unreadable; wait until it is heard again.")
    })?;
    let identity = parse_hash(identity_hash).ok_or_else(|| {
        anyhow!("The identity hash {source} holds for {destination} is unreadable; wait until it is heard again.")
    })?;
    let claimed = parse_hash(destination).ok_or_else(|| {
        anyhow!("'{destination}' is not a destination hash: expected 32 hex characters.")
    })?;
    let expected = destination_address(&name_hash, &identity);
    if expected != claimed {
        bail!(
            "Destination {destination} does not match identity {} in {source} (that identity would announce {}); nothing was trusted.",
            identity.to_hex_string(),
            expected.to_hex_string()
        );
    }
    Ok(identity)
}

pub(crate) fn decode_name_hash(hex: &str) -> Option<[u8; NAME_HASH_LEN]> {
    decode_hex(hex)?.try_into().ok()
}

/// The one line a key change earns. It says "announced under", not "rotated": anyone who
/// has heard the instance id can announce it under their own key, so the human verifies
/// out of band before trusting the new destination, or blocks the new identity. Full
/// hashes, because the human pastes them.
fn key_change_text(change: &MarkedKeyChange, new_destination: &str) -> String {
    let instance = change
        .label
        .clone()
        .unwrap_or_else(|| short(&change.destination_hash).to_string());
    format!(
        "instance {instance} is bound to identity {bound} but was announced under identity {seen}; \
         its grant stays with the old key and the new key is a stranger. If the peer rotated, \
         verify the new identity out of band and run .mesh trust {new_destination} ; \
         otherwise .mesh block {seen}",
        bound = change.bound_identity,
        seen = change.seen_identity,
    )
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;
    use crate::hooks::HookEvent;
    use crate::mesh::events::{
        MeshHooks, RecordingHookSink, TrustHookObserver, env_value, one_fire,
    };
    use crate::mesh::hex_lower;
    use crate::mesh::knock::RecordingSurface;
    use crate::mesh::peers::{PEER_TTL, PeerSighting};
    use crate::mesh::session_destination_name;
    use crate::testing::{install_log_collector, warn_snapshot};

    use rand_core::OsRng;
    use rns_transport::destination::SingleInputDestination;
    use rns_transport::identity::PrivateIdentity;

    struct MeshOff;

    impl LiveMesh for MeshOff {
        fn peers(&self) -> Option<Arc<PeerTable>> {
            None
        }
    }

    /// A running mesh: the peer table, and the knocks its cache would hold.
    struct MeshOn(Arc<PeerTable>, Mutex<Vec<(String, KnockProof)>>);

    impl LiveMesh for MeshOn {
        fn peers(&self) -> Option<Arc<PeerTable>> {
            Some(self.0.clone())
        }

        fn knock(&self, destination_hash: &str, _now: SystemTime) -> Option<KnockProof> {
            self.1
                .lock()
                .iter()
                .find(|(destination, _)| destination == destination_hash)
                .map(|(_, proof)| proof.clone())
        }
    }

    /// A real announce's worth of hashes: the destination is derived from the name and
    /// identity exactly as the transport does it.
    struct Announced {
        destination_hash: String,
        identity_hash: String,
        name_hash: String,
    }

    fn announced(aspect: &str) -> Announced {
        announced_as(PrivateIdentity::new_from_rand(OsRng), aspect)
    }

    fn announced_as(identity: PrivateIdentity, aspect: &str) -> Announced {
        let name = session_destination_name(aspect);
        let destination = SingleInputDestination::new(identity, name);
        Announced {
            destination_hash: destination.desc.address_hash.to_hex_string(),
            identity_hash: destination.desc.identity.address_hash.to_hex_string(),
            name_hash: hex_lower(name.as_name_hash_slice()),
        }
    }

    fn sighting(peer: &Announced) -> PeerSighting {
        PeerSighting {
            destination_hash: peer.destination_hash.clone(),
            identity_hash: peer.identity_hash.clone(),
            name_hash: peer.name_hash.clone(),
            display_name: Some("Bob".to_string()),
            protocol_version: 1,
            hops: 1,
        }
    }

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    struct Fixture {
        store: TrustStore,
        mesh: MeshOn,
        tmp: TempDir,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let tmp = TempDir::new(tag);
            let store = TrustStore::open(&tmp.path).unwrap();
            let peers = PeerTable::load(tmp.path.join("peers.json"), t(1_000)).unwrap();
            Self {
                store,
                mesh: MeshOn(Arc::new(peers), Mutex::new(Vec::new())),
                tmp,
            }
        }

        fn announce(&self, peer: &Announced, at: SystemTime) {
            self.mesh.0.observe(sighting(peer), at);
        }

        /// Puts `peer`'s knock in the cache with the hashes it announced, `name_hash` aside.
        fn knocked(&self, peer: &Announced, name_hash: &str, at: SystemTime) {
            self.mesh.1.lock().push((
                peer.destination_hash.clone(),
                KnockProof {
                    identity_hash: peer.identity_hash.clone(),
                    name_hash: name_hash.to_string(),
                    last_seen: at,
                },
            ));
        }

        fn file_bytes(&self) -> Option<Vec<u8>> {
            fs::read(self.store.path()).ok()
        }

        fn reopen(&self) -> TrustStore {
            TrustStore::open(&self.tmp.path).unwrap()
        }

        /// Routes the store's trust events into a recording sink.
        fn observed(&self) -> Arc<RecordingHookSink> {
            let hooks = MeshHooks::default();
            let sink = RecordingHookSink::attach(&hooks);
            self.store.set_observer(Arc::new(TrustHookObserver(hooks)));
            sink
        }

        fn trust_identity(&self, identity_hash: &str, at: SystemTime) -> TrustChange {
            self.store
                .trust_identity(&self.mesh, identity_hash, TrustOptions::default(), at)
                .unwrap()
        }

        fn trust_destination(&self, peer: &Announced, at: SystemTime) -> TrustChange {
            self.store
                .trust_destination(
                    &self.mesh,
                    &peer.destination_hash,
                    TrustOptions::default(),
                    at,
                )
                .unwrap()
                .change
        }
    }

    fn fake_hash(fill: u8) -> String {
        hex_lower(&[fill; 16])
    }

    fn verdict(decision: Decision, rule: Rule) -> Verdict {
        Verdict { decision, rule }
    }

    /// The verdict the R3 dispatcher asks for: `peer`'s identity naming its own instance.
    fn origin_verdict(store: &TrustStore, peer: &Announced) -> Verdict {
        store
            .authorize_origin(
                &parse_hash(&peer.identity_hash).unwrap(),
                &decode_name_hash(&peer.name_hash).unwrap(),
            )
            .0
    }

    fn record(store: &TrustStore, hash: &str) -> TrustRecord {
        store
            .records()
            .into_iter()
            .find(|record| record.hash == hash)
            .unwrap_or_else(|| panic!("no record for {hash}"))
    }

    /// A trusted instance and the same instance announced under a fresh identity.
    fn trusted_then_rotated(fx: &Fixture) -> (Announced, Announced) {
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_destination(&old, t(3_000));
        let new = announced("alpha");
        assert_eq!(new.name_hash, old.name_hash);
        (old, new)
    }

    #[test]
    fn same_hash_is_constant_time_shaped() {
        let hash = fake_hash(0xab);
        let mut last_byte_differs = hash.clone();
        last_byte_differs.replace_range(31..32, "c");

        assert!(same_hash(&hash, &hash));
        assert!(!same_hash(&hash, &last_byte_differs));
        assert!(!same_hash("ab", "abc"));
        assert!(same_hash("", ""));
    }

    #[test]
    fn open_of_missing_file_is_empty_and_creates_nothing() {
        let fx = Fixture::new("trust-missing");

        assert!(fx.store.records().is_empty());
        assert!(fx.store.blocked().is_empty());
        assert_eq!(
            fx.store.authorize(&fake_hash(0x1a), &fake_hash(0x2b)),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert!(!fx.tmp.path.join("mesh").exists());
        assert_eq!(fx.store.path(), fx.tmp.path.join("mesh").join("trust.yaml"));
    }

    #[test]
    fn trust_destination_records_the_identity_the_formula_proves() {
        let fx = Fixture::new("trust-formula-ok");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000) + Duration::from_millis(250));

        let outcome = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash.to_ascii_uppercase(),
                TrustOptions {
                    label: Some("Bob".to_string()),
                    note: None,
                },
                t(3_000) + Duration::from_millis(250),
            )
            .unwrap();

        assert_eq!(outcome.change, TrustChange::Added);
        assert_eq!(outcome.identity_hash, peer.identity_hash);
        assert_eq!(outcome.destination_hash, peer.destination_hash);
        let records = fx.store.records();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].tier, Tier::Identity);
        assert_eq!(records[0].hash, peer.identity_hash);
        assert!(!records[0].all_destinations);
        assert_eq!(records[1].tier, Tier::Destination);
        assert_eq!(records[1].hash, peer.destination_hash);
        assert_eq!(
            records[1].identity.as_deref(),
            Some(peer.identity_hash.as_str())
        );
        assert_eq!(records[1].label.as_deref(), Some("Bob"));
        assert_eq!(records[1].added_at, t(3_000), "stamps floor to the second");
        assert_eq!(records[1].last_seen_at, t(2_000));
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );

        let reopened = fx.reopen();
        assert_eq!(
            reopened.records(),
            records,
            "a committed record must equal its reloaded self even when the input had nanos"
        );
    }

    #[test]
    fn trust_destination_refuses_a_tampered_identity_column_and_writes_nothing() {
        let fx = Fixture::new("trust-formula-tampered");
        let peer = announced("alpha");
        let impostor = announced("beta");
        fx.mesh.0.observe(
            PeerSighting {
                identity_hash: impostor.identity_hash.clone(),
                ..sighting(&peer)
            },
            t(2_000),
        );

        let err = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains(&peer.destination_hash), "{err}");
        assert!(err.contains(&impostor.identity_hash), "{err}");
        assert!(fx.store.records().is_empty());
        assert_eq!(fx.file_bytes(), None);
        assert!(!fx.store.is_trusted_identity(&impostor.identity_hash));
        assert!(!fx.store.is_trusted_identity(&peer.identity_hash));
    }

    #[test]
    fn trust_destination_refuses_a_forged_name_hash() {
        let fx = Fixture::new("trust-formula-name");
        let peer = announced("alpha");
        fx.mesh.0.observe(
            PeerSighting {
                name_hash: hex_lower(&[7; 10]),
                ..sighting(&peer)
            },
            t(2_000),
        );

        let err = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("does not match"), "{err}");
        assert_eq!(fx.file_bytes(), None);
    }

    #[test]
    fn trust_mutation_renders_every_hash_truncated() {
        let identity = fake_hash(0xab);
        let destination = fake_hash(0xcd);
        let mutations = [
            TrustMutation::TrustDestination {
                destination: destination.clone(),
                identity: identity.clone(),
                change: TrustChange::Added,
            },
            TrustMutation::TrustIdentity {
                identity: identity.clone(),
                granted: true,
            },
            TrustMutation::UntrustDestination {
                destination: destination.clone(),
                identity: identity.clone(),
            },
            TrustMutation::UntrustIdentity {
                identity: identity.clone(),
                was_granting: true,
                removed: vec![destination.clone()],
            },
            TrustMutation::BlockIdentity {
                identity: identity.clone(),
                was_granting: false,
                removed: vec![destination.clone()],
                marks_cleared: 1,
            },
            TrustMutation::UnblockIdentity {
                identity: identity.clone(),
            },
            TrustMutation::Deny {
                destination: destination.clone(),
            },
            TrustMutation::Undeny {
                destination: destination.clone(),
            },
            TrustMutation::Prune {
                stale: vec![(destination.clone(), identity.clone())],
            },
            TrustMutation::KeyChanged {
                destinations: vec![destination.clone()],
                seen_identity: identity.clone(),
            },
        ];
        for mutation in &mutations {
            let rendered = mutation.to_string();
            assert!(!rendered.contains(&identity), "{rendered}");
            assert!(!rendered.contains(&destination), "{rendered}");
            let names_a_short_hash =
                rendered.contains(short(&identity)) || rendered.contains(short(&destination));
            assert_eq!(
                names_a_short_hash,
                !matches!(mutation, TrustMutation::Prune { .. }),
                "{rendered}"
            );
        }
    }

    #[test]
    fn trust_destination_of_unknown_peer_points_at_mesh_peers_and_knocks() {
        let fx = Fixture::new("trust-unknown");

        let err = fx
            .store
            .trust_destination(
                &fx.mesh,
                &fake_hash(0x9f),
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains(".mesh peers"), "{err}");
        assert!(err.contains(".mesh knocks"), "{err}");
        assert!(err.contains("has not knocked"), "{err}");
        assert!(err.contains(&fake_hash(0x9f)), "{err}");
        assert_eq!(fx.file_bytes(), None);
    }

    #[test]
    fn trust_destination_accepts_a_knock_only_instance_the_formula_proves() {
        let fx = Fixture::new("trust-knock-only");
        let peer = announced("alpha");
        fx.knocked(&peer, &peer.name_hash, t(2_000));

        let outcome = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();

        assert_eq!(outcome.change, TrustChange::Added);
        assert_eq!(outcome.identity_hash, peer.identity_hash);
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
        let record = fx
            .store
            .records()
            .into_iter()
            .find(|record| record.hash == peer.destination_hash)
            .unwrap();
        assert_eq!(record.last_seen_at, t(2_000), "the knock is the sighting");
    }

    #[test]
    fn trust_destination_refuses_a_knock_whose_hashes_do_not_derive_it() {
        let fx = Fixture::new("trust-knock-forged");
        let peer = announced("alpha");
        let other = announced("beta");
        fx.knocked(&peer, &other.name_hash, t(2_000));

        let err = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("does not match identity"), "{err}");
        assert!(err.contains("knock cache"), "{err}");
        assert!(fx.store.records().is_empty());
        assert_eq!(fx.file_bytes(), None);
    }

    #[test]
    fn trust_destination_waits_for_a_knock_cached_without_its_name_hash() {
        let fx = Fixture::new("trust-knock-no-name-hash");
        let peer = announced("alpha");
        fx.knocked(&peer, "", t(2_000));

        let err = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("knocked before its name hash was kept"),
            "{err}"
        );
        assert!(err.contains("knock again"), "{err}");
        assert_eq!(fx.file_bytes(), None);
    }

    #[test]
    fn the_peer_table_outranks_the_knock_cache_as_the_proof_source() {
        let fx = Fixture::new("trust-announce-over-knock");
        let peer = announced("alpha");
        let other = announced("beta");
        fx.announce(&peer, t(2_000));
        fx.knocked(&peer, &other.name_hash, t(2_500));

        let outcome = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();

        assert_eq!(outcome.identity_hash, peer.identity_hash);
        let record = fx
            .store
            .records()
            .into_iter()
            .find(|record| record.hash == peer.destination_hash)
            .unwrap();
        assert_eq!(
            record.last_seen_at,
            t(2_000),
            "the announce is the sighting"
        );
    }

    #[test]
    fn trust_destination_waits_for_a_peer_recorded_without_its_name_hash() {
        let fx = Fixture::new("trust-no-name-hash");
        let peer = announced("alpha");
        fx.mesh.0.observe(
            PeerSighting {
                name_hash: String::new(),
                ..sighting(&peer)
            },
            t(2_000),
        );

        let err = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("next announce"), "{err}");
        assert_eq!(fx.file_bytes(), None);
    }

    #[test]
    fn trust_destination_twice_updates_in_place_and_keeps_the_identity_record() {
        let fx = Fixture::new("trust-twice");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.store
            .trust_identity(
                &fx.mesh,
                &peer.identity_hash,
                TrustOptions {
                    label: Some("Bob".to_string()),
                    note: None,
                },
                t(2_500),
            )
            .unwrap();
        fx.store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();
        fx.announce(&peer, t(4_000));

        let outcome = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions {
                    label: None,
                    note: Some("laptop".to_string()),
                },
                t(5_000),
            )
            .unwrap();

        assert_eq!(outcome.change, TrustChange::Updated);
        let records = fx.store.records();
        assert_eq!(records.len(), 2);
        assert!(
            records[0].all_destinations,
            "trust_destination must not clear --identity"
        );
        assert_eq!(records[0].label.as_deref(), Some("Bob"));
        assert_eq!(records[1].added_at, t(3_000));
        assert_eq!(records[1].last_seen_at, t(4_000));
        assert_eq!(records[1].note.as_deref(), Some("laptop"));
    }

    #[test]
    fn trust_identity_sets_the_flag_without_a_third_tier() {
        let fx = Fixture::new("trust-identity");
        let identity = fake_hash(0xab);

        let change = fx
            .store
            .trust_identity(
                &fx.mesh,
                &identity.to_ascii_uppercase(),
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();

        assert_eq!(change, TrustChange::Added);
        let records = fx.store.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tier, Tier::Identity);
        assert_eq!(records[0].hash, identity);
        assert!(records[0].all_destinations);
        assert!(fx.store.is_trusted_identity(&identity));
        assert_eq!(
            fx.store.authorize(&identity, &fake_hash(0xcd)),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
        let text = String::from_utf8(fx.file_bytes().unwrap()).unwrap();
        assert!(text.contains("all_destinations: true"), "{text}");
        assert!(!text.contains("tier"), "{text}");
    }

    #[test]
    fn trust_identity_refuses_a_malformed_hash() {
        let fx = Fixture::new("trust-identity-bad");

        let err = fx
            .store
            .trust_identity(&fx.mesh, "bob", TrustOptions::default(), t(3_000))
            .unwrap_err()
            .to_string();

        assert!(err.contains("32 hex"), "{err}");
        assert_eq!(fx.file_bytes(), None);
    }

    #[test]
    fn trust_identity_seeds_last_seen_only_from_rows_the_formula_proves() {
        let fx = Fixture::new("trust-identity-last-seen");
        let peer = announced("alpha");
        let impostor = announced("beta");
        fx.mesh.0.observe(
            PeerSighting {
                identity_hash: peer.identity_hash.clone(),
                ..sighting(&impostor)
            },
            t(9_000),
        );

        fx.store
            .trust_identity(
                &fx.mesh,
                &peer.identity_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();

        assert_eq!(
            fx.store.records()[0].last_seen_at,
            t(3_000),
            "a row that merely claims the identity must not move last_seen_at"
        );

        fx.announce(&peer, t(4_000));
        fx.store
            .trust_identity(
                &fx.mesh,
                &peer.identity_hash,
                TrustOptions::default(),
                t(5_000),
            )
            .unwrap();

        assert_eq!(fx.store.records()[0].last_seen_at, t(4_000));

        assert!(!fx.mesh.0.sweep(t(9_000) + PEER_TTL).is_empty());
        assert!(fx.mesh.0.snapshot().is_empty());
        fx.store
            .trust_identity(
                &fx.mesh,
                &peer.identity_hash,
                TrustOptions::default(),
                t(20_000),
            )
            .unwrap();

        assert_eq!(
            fx.store.records()[0].last_seen_at,
            t(4_000),
            "with no row to prove a sighting, a re-run must not fabricate one"
        );
    }

    #[test]
    fn labels_and_notes_refuse_control_characters() {
        let fx = Fixture::new("trust-text-hygiene");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        let identity = fake_hash(0xaa);
        let bidi = "Bob\u{202e}".to_string();
        let newline = "two\nlines".to_string();

        let attempts: Vec<(&str, Result<()>)> = vec![
            (
                "label",
                fx.store
                    .trust_destination(
                        &fx.mesh,
                        &peer.destination_hash,
                        TrustOptions {
                            label: Some(bidi.clone()),
                            note: None,
                        },
                        t(3_000),
                    )
                    .map(drop),
            ),
            (
                "note",
                fx.store
                    .trust_destination(
                        &fx.mesh,
                        &peer.destination_hash,
                        TrustOptions {
                            label: None,
                            note: Some(newline.clone()),
                        },
                        t(3_000),
                    )
                    .map(drop),
            ),
            (
                "label",
                fx.store
                    .trust_identity(
                        &fx.mesh,
                        &identity,
                        TrustOptions {
                            label: Some(newline.clone()),
                            note: None,
                        },
                        t(3_000),
                    )
                    .map(drop),
            ),
            (
                "note",
                fx.store
                    .trust_identity(
                        &fx.mesh,
                        &identity,
                        TrustOptions {
                            label: None,
                            note: Some(bidi.clone()),
                        },
                        t(3_000),
                    )
                    .map(drop),
            ),
            (
                "note",
                fx.store
                    .deny_destination(&fx.mesh, &peer.destination_hash, Some(bidi), t(3_000)),
            ),
            (
                "note",
                fx.store
                    .block_identity(&fx.mesh, &identity, Some(newline), t(3_000))
                    .map(drop),
            ),
        ];

        for (field, result) in attempts {
            let err = result
                .err()
                .unwrap_or_else(|| panic!("a {field} with control characters must be refused"))
                .to_string();
            assert!(err.contains(field), "{err}");
            assert!(err.contains("control"), "{err}");
        }
        let err = fx
            .store
            .trust_identity(
                &fx.mesh,
                &identity,
                TrustOptions {
                    label: Some("x".repeat(257)),
                    note: None,
                },
                t(3_000),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("label is longer than 256"), "{err}");
        assert!(fx.store.records().is_empty());
        assert!(fx.store.denied().is_empty());
        assert!(fx.store.blocked().is_empty());
        assert_eq!(fx.file_bytes(), None);
    }

    #[test]
    fn labels_keep_a_variation_selector_and_refuse_a_soft_hyphen() {
        let fx = Fixture::new("trust-text-variation-selector");
        let identity = fake_hash(0xac);
        let heart = "Alex \u{2764}\u{FE0F}";

        let change = fx
            .store
            .trust_identity(
                &fx.mesh,
                &identity,
                TrustOptions {
                    label: Some(heart.to_string()),
                    note: Some("styled \u{9053}\u{E0100}".to_string()),
                },
                t(3_000),
            )
            .unwrap();

        assert_eq!(change, TrustChange::Added);
        let records = fx.reopen().records();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].label.as_deref(),
            Some(heart),
            "a variation selector is kept verbatim in the trust list"
        );
        assert_eq!(records[0].note.as_deref(), Some("styled \u{9053}\u{E0100}"));

        let err = fx
            .store
            .trust_identity(
                &fx.mesh,
                &identity,
                TrustOptions {
                    label: Some("so\u{00AD}ft".to_string()),
                    note: None,
                },
                t(3_100),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("label"), "{err}");
        assert!(err.contains("control or invisible"), "{err}");
        let records = fx.reopen().records();
        assert_eq!(
            records[0].label.as_deref(),
            Some(heart),
            "nothing was changed"
        );
    }

    #[test]
    fn malformed_hashes_are_refused_without_panicking() {
        let fx = Fixture::new("trust-hash-shapes");
        let non_ascii_32_bytes = format!("\u{20ac}{}", "a".repeat(29));
        assert_eq!(non_ascii_32_bytes.len(), 32);
        let inputs = [
            non_ascii_32_bytes,
            "a".repeat(31),
            "a".repeat(33),
            format!("g{}", "a".repeat(31)),
        ];

        for input in &inputs {
            assert_eq!(canonical_hash(input), None, "{input:?}");
            let err = fx
                .store
                .trust_identity(&fx.mesh, input, TrustOptions::default(), t(3_000))
                .unwrap_err()
                .to_string();
            assert!(err.contains("32 hex"), "{input:?}: {err}");
            let err = fx
                .store
                .deny_destination(&fx.mesh, input, None, t(3_000))
                .unwrap_err()
                .to_string();
            assert!(err.contains("32 hex"), "{input:?}: {err}");
            assert!(
                fx.store
                    .trust_destination(&fx.mesh, input, TrustOptions::default(), t(3_000))
                    .is_err()
            );
        }
        assert_eq!(
            canonical_hash(&fake_hash(0xab).to_ascii_uppercase()),
            Some(fake_hash(0xab))
        );
        assert_eq!(fx.file_bytes(), None);
    }

    #[test]
    fn identity_standing_distinguishes_unknown_blocked_and_the_two_trusted_forms() {
        let fx = Fixture::new("trust-standing");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        let other_dest = fake_hash(0xcd);

        assert_eq!(
            fx.store.identity_standing(&peer.identity_hash),
            IdentityStanding::Unknown
        );
        assert!(!fx.store.is_trusted_identity(&peer.identity_hash));

        fx.store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();

        assert_eq!(
            fx.store.identity_standing(&peer.identity_hash),
            IdentityStanding::Trusted {
                all_destinations: false
            }
        );
        assert_eq!(
            fx.store.authorize(&peer.identity_hash, &other_dest),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert!(
            fx.store.is_trusted_identity(&peer.identity_hash),
            "a known identity may knock from an untrusted destination; this is the knock case, not an allow"
        );

        fx.store
            .trust_identity(
                &fx.mesh,
                &peer.identity_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();

        assert_eq!(
            fx.store.identity_standing(&peer.identity_hash),
            IdentityStanding::Trusted {
                all_destinations: true
            }
        );
        assert_eq!(
            fx.store.authorize(&peer.identity_hash, &other_dest),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );

        fx.store
            .block_identity(&fx.mesh, &peer.identity_hash, None, t(4_000))
            .unwrap();

        assert_eq!(
            fx.store.identity_standing(&peer.identity_hash),
            IdentityStanding::Blocked
        );
        assert!(!fx.store.is_trusted_identity(&peer.identity_hash));
        assert_eq!(
            fx.store.authorize(&peer.identity_hash, &other_dest),
            verdict(Decision::Refuse, Rule::IdentityBlocked)
        );
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Refuse, Rule::IdentityBlocked)
        );

        let session = announced("beta");
        fx.announce(&session, t(4_000));
        fx.store
            .trust_destination_for_session(&fx.mesh, &session.destination_hash, t(4_000))
            .unwrap();
        assert_eq!(
            fx.store.identity_standing(&session.identity_hash),
            IdentityStanding::Trusted {
                all_destinations: false
            }
        );
    }

    #[test]
    fn file_is_versioned_yaml_with_stable_key_order() {
        let fx = Fixture::new("trust-yaml");
        let peer = announced("alpha");
        fx.announce(&peer, t(1_790_000_000));
        fx.store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions {
                    label: Some("Bob".to_string()),
                    note: None,
                },
                t(1_790_000_000),
            )
            .unwrap();
        fx.store
            .deny_destination(
                &fx.mesh,
                &fake_hash(0xdd),
                Some("noisy".to_string()),
                t(1_790_000_000),
            )
            .unwrap();
        fx.store
            .block_identity(&fx.mesh, &fake_hash(0xbb), None, t(1_790_000_000))
            .unwrap();

        let text = String::from_utf8(fx.file_bytes().unwrap()).unwrap();

        let expected = format!(
            "version: {TRUST_FILE_VERSION}\n\
             identities:\n  {id}:\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: null\n    note: null\n    all_destinations: false\n\
             destinations:\n  {dest}:\n    identity: {id}\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: Bob\n    note: null\n\
             denied_destinations:\n  {denied}:\n    added_at: {ts}\n    note: noisy\n\
             blocked_identities:\n  {blocked}:\n    added_at: {ts}\n    note: null\n",
            id = peer.identity_hash,
            dest = peer.destination_hash,
            denied = fake_hash(0xdd),
            blocked = fake_hash(0xbb),
            ts = rfc3339_utc(t(1_790_000_000)),
        );
        assert_eq!(text, expected);
        assert!(!fx.store.path().with_extension("yaml.tmp").exists());
    }

    #[test]
    fn open_refuses_a_newer_file_version_naming_the_path() {
        let tmp = TempDir::new("trust-newer");
        let path = mesh_config_dir(&tmp.path).join("trust.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let newer = TRUST_FILE_VERSION + 1;
        fs::write(
            &path,
            format!("version: {newer}\nidentities: {{}}\nfuture_section: {{}}\n"),
        )
        .unwrap();

        let err = TrustStore::open(&tmp.path).unwrap_err().to_string();

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains(&format!("version {newer}")), "{err}");
        assert!(err.contains("upgrade Coyote"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
    }

    #[test]
    fn open_refuses_a_pre_baseline_file_version_as_having_no_migration() {
        let tmp = TempDir::new("trust-older");
        let path = mesh_config_dir(&tmp.path).join("trust.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "version: 0\nidentities: {}\n").unwrap();

        let err = TrustStore::open(&tmp.path).unwrap_err().to_string();

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("version 0"), "{err}");
        assert!(
            err.contains(&format!("version {TRUST_FILE_VERSION}")),
            "{err}"
        );
        assert!(err.contains("no migration"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(!err.contains("upgrade Coyote"), "{err}");
    }

    /// Usage probe for T33 (e): a `trust.yaml` the pre-SCOPE build wrote (a well-formed
    /// version-1 file whose destination hashes derive from the old application name) is
    /// refused with a clear message, never opened as an empty trust list.
    #[test]
    fn usage_probe_open_refuses_a_well_formed_version_1_trust_file_written_before_scope() {
        assert_eq!(TRUST_FILE_VERSION, 2, "T33 bumps the trust file 1 -> 2");
        let tmp = TempDir::new("trust-pre-scope-v1");
        let path = mesh_config_dir(&tmp.path).join("trust.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (identity, destination) = (fake_hash(0x1a), fake_hash(0x2b));
        let ts = "2026-01-01T00:00:00Z";
        let old = format!(
            "version: 1\n\
             identities:\n  {identity}:\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: null\n    note: null\n    all_destinations: false\n\
             destinations:\n  {destination}:\n    identity: {identity}\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: Bob\n    note: null\n"
        );
        fs::write(&path, &old).unwrap();

        let err = match TrustStore::open(&tmp.path) {
            Ok(store) => panic!(
                "a version-1 trust file must be refused, not opened with {} records",
                store.records().len()
            ),
            Err(err) => err.to_string(),
        };

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("version 1"), "{err}");
        assert!(err.contains("version 2"), "{err}");
        assert!(err.contains("no migration"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(!err.contains("upgrade Coyote"), "{err}");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            old,
            "a refused trust file is left as it was"
        );
    }

    #[test]
    fn open_refuses_a_file_without_a_version() {
        let tmp = TempDir::new("trust-unversioned");
        let path = mesh_config_dir(&tmp.path).join("trust.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "identities: {}\n").unwrap();

        let err = format!("{:#}", TrustStore::open(&tmp.path).unwrap_err());

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("`version`"), "{err}");
        assert!(err.contains("no readable `version` field"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
    }

    #[test]
    fn open_refuses_unknown_fields_and_bad_timestamps_rather_than_partially_loading() {
        let tmp = TempDir::new("trust-strict");
        let path = mesh_config_dir(&tmp.path).join("trust.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let identity = fake_hash(0x1a);
        let good = format!(
            "version: {TRUST_FILE_VERSION}\nidentities:\n  {identity}:\n    added_at: 2026-01-01T00:00:00Z\n    last_seen_at: 2026-01-01T00:00:00Z\n    label: null\n    note: null\n    all_destinations: true\n"
        );
        fs::write(&path, &good).unwrap();
        assert_eq!(TrustStore::open(&tmp.path).unwrap().records().len(), 1);

        fs::write(&path, format!("{good}    tier: identity\n")).unwrap();
        let err = format!("{:#}", TrustStore::open(&tmp.path).unwrap_err());
        assert!(
            err.contains(&format!(
                "could not be parsed as version {TRUST_FILE_VERSION}"
            )),
            "{err}"
        );
        assert!(err.contains("trusts nobody"), "{err}");
        assert!(err.contains("tier"), "{err}");

        fs::write(&path, good.replace("2026-01-01T00:00:00Z", "yesterday")).unwrap();
        let err = format!("{:#}", TrustStore::open(&tmp.path).unwrap_err());
        assert!(err.contains("RFC 3339"), "{err}");

        assert_eq!(
            fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1,
            "a refused file must be left where it is, not set aside"
        );
    }

    #[test]
    fn open_refuses_non_canonical_keys_rather_than_loading_an_inert_deny() {
        let tmp = TempDir::new("trust-canonical-keys");
        let path = mesh_config_dir(&tmp.path).join("trust.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let upper = fake_hash(0xdd).to_ascii_uppercase();
        fs::write(
            &path,
            format!(
                "version: {TRUST_FILE_VERSION}\ndenied_destinations:\n  {upper}:\n    added_at: 2026-01-01T00:00:00Z\n    note: null\n"
            ),
        )
        .unwrap();

        let err = TrustStore::open(&tmp.path).unwrap_err().to_string();

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("denied_destinations"), "{err}");
        assert!(err.contains(&upper), "{err}");
        assert!(err.contains("Fix the key"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");

        let identity = fake_hash(0x1a);
        fs::write(
            &path,
            format!(
                "version: {TRUST_FILE_VERSION}\ndestinations:\n  {}:\n    identity: {}\n    added_at: 2026-01-01T00:00:00Z\n    last_seen_at: 2026-01-01T00:00:00Z\n    label: null\n    note: null\n",
                fake_hash(0x2b),
                identity.to_ascii_uppercase()
            ),
        )
        .unwrap();

        let err = TrustStore::open(&tmp.path).unwrap_err().to_string();

        assert!(err.contains("identity field"), "{err}");
    }

    #[test]
    fn open_refuses_a_destination_bound_to_an_identity_the_file_does_not_list() {
        let tmp = TempDir::new("trust-dangling-identity");
        let path = mesh_config_dir(&tmp.path).join("trust.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (destination, missing) = (fake_hash(0x2b), fake_hash(0x1a));
        fs::write(
            &path,
            format!(
                "version: {TRUST_FILE_VERSION}\ndestinations:\n  {destination}:\n    identity: {missing}\n    added_at: 2026-01-01T00:00:00Z\n    last_seen_at: 2026-01-01T00:00:00Z\n    label: null\n    note: null\n"
            ),
        )
        .unwrap();

        let err = TrustStore::open(&tmp.path).unwrap_err().to_string();

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains(&destination), "{err}");
        assert!(err.contains(&missing), "{err}");
        assert!(err.contains("`identities`"), "{err}");
        assert!(err.contains("Fix the file, or move it aside"), "{err}");
    }

    #[test]
    fn session_trust_never_reaches_the_file() {
        let fx = Fixture::new("trust-session");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        let other = announced("beta");
        fx.announce(&other, t(2_000));
        fx.store
            .trust_destination(
                &fx.mesh,
                &other.destination_hash,
                TrustOptions::default(),
                t(2_500),
            )
            .unwrap();
        let before = fx.file_bytes().unwrap();

        let outcome = fx
            .store
            .trust_destination_for_session(&fx.mesh, &peer.destination_hash, t(3_000))
            .unwrap();

        assert_eq!(outcome.identity_hash, peer.identity_hash);
        assert!(
            fx.store
                .is_trusted_destination(&peer.identity_hash, &peer.destination_hash)
        );
        assert!(fx.store.is_trusted_identity(&peer.identity_hash));
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
        assert_eq!(fx.file_bytes().unwrap(), before);
        let session: Vec<_> = fx
            .store
            .records()
            .into_iter()
            .filter(|record| record.session)
            .map(|record| record.hash)
            .collect();
        assert_eq!(
            session,
            vec![peer.identity_hash.clone(), peer.destination_hash.clone()]
        );

        let reopened = fx.reopen();
        assert!(!reopened.is_trusted_identity(&peer.identity_hash));
        assert!(!reopened.is_trusted_destination(&peer.identity_hash, &peer.destination_hash));
        assert!(reopened.is_trusted_identity(&other.identity_hash));
    }

    #[test]
    fn session_trust_still_needs_the_formula_to_hold() {
        let fx = Fixture::new("trust-session-forged");
        let peer = announced("alpha");
        fx.mesh.0.observe(
            PeerSighting {
                identity_hash: fake_hash(0xee),
                ..sighting(&peer)
            },
            t(2_000),
        );

        assert!(
            fx.store
                .trust_destination_for_session(&fx.mesh, &peer.destination_hash, t(3_000))
                .is_err()
        );
        assert!(fx.store.records().is_empty());
    }

    #[test]
    fn session_and_disk_never_list_the_same_hash_twice() {
        let fx = Fixture::new("trust-session-disk-overlap");
        let disk_first = announced("alpha");
        let session_first = announced("beta");
        fx.announce(&disk_first, t(2_000));
        fx.announce(&session_first, t(2_000));
        let unique_hashes = |records: &[TrustRecord]| {
            let mut hashes: Vec<&str> = records.iter().map(|r| r.hash.as_str()).collect();
            hashes.sort_unstable();
            hashes.dedup();
            hashes.len()
        };

        fx.store
            .trust_destination(
                &fx.mesh,
                &disk_first.destination_hash,
                TrustOptions::default(),
                t(2_500),
            )
            .unwrap();
        let outcome = fx
            .store
            .trust_destination_for_session(&fx.mesh, &disk_first.destination_hash, t(3_000))
            .unwrap();
        assert_eq!(outcome.change, TrustChange::Updated);

        fx.store
            .trust_destination_for_session(&fx.mesh, &session_first.destination_hash, t(3_000))
            .unwrap();
        fx.announce(&session_first, t(4_000));
        let outcome = fx
            .store
            .trust_destination_for_session(&fx.mesh, &session_first.destination_hash, t(4_500))
            .unwrap();
        assert_eq!(outcome.change, TrustChange::Updated);

        let records = fx.store.records();
        assert_eq!(records.len(), 4);
        assert_eq!(unique_hashes(&records), 4);
        assert!(
            records
                .iter()
                .filter(
                    |r| r.hash == disk_first.destination_hash || r.hash == disk_first.identity_hash
                )
                .all(|r| !r.session),
            "a hash on disk gets no session twin"
        );
        let session = records
            .iter()
            .find(|r| r.hash == session_first.destination_hash)
            .unwrap();
        assert!(session.session);
        assert_eq!(
            session.added_at,
            t(3_000),
            "a second session trust keeps added_at"
        );
        assert_eq!(session.last_seen_at, t(4_000));

        fx.store
            .trust_destination(
                &fx.mesh,
                &session_first.destination_hash,
                TrustOptions::default(),
                t(5_000),
            )
            .unwrap();

        let records = fx.store.records();
        assert_eq!(records.len(), 4);
        assert_eq!(unique_hashes(&records), 4);
        assert!(
            records.iter().all(|r| !r.session),
            "committing a session pair to disk drops its session twins"
        );

        let identity_later = announced("gamma");
        fx.announce(&identity_later, t(6_000));
        fx.store
            .trust_destination_for_session(&fx.mesh, &identity_later.destination_hash, t(6_500))
            .unwrap();
        fx.store
            .trust_identity(
                &fx.mesh,
                &identity_later.identity_hash,
                TrustOptions::default(),
                t(7_000),
            )
            .unwrap();

        let records = fx.store.records();
        assert_eq!(records.len(), 6);
        assert_eq!(unique_hashes(&records), 6);
        let gamma_identity: Vec<&TrustRecord> = records
            .iter()
            .filter(|r| r.hash == identity_later.identity_hash)
            .collect();
        assert_eq!(
            gamma_identity.len(),
            1,
            "trusting an identity on disk drops its session twin"
        );
        assert!(!gamma_identity[0].session);
        assert!(gamma_identity[0].all_destinations);
        assert!(
            records
                .iter()
                .find(|r| r.hash == identity_later.destination_hash)
                .unwrap()
                .session,
            "the session destination is untouched by trusting its identity"
        );
    }

    #[test]
    fn authorize_is_default_closed_and_binds_destinations_to_their_identity() {
        let fx = Fixture::new("trust-authorize");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();

        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
        assert_eq!(
            fx.store.authorize(
                &peer.identity_hash.to_ascii_uppercase(),
                &peer.destination_hash.to_ascii_uppercase()
            ),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
        assert_eq!(
            fx.store.authorize(&fake_hash(0xee), &peer.destination_hash),
            verdict(Decision::Refuse, Rule::DefaultClosed),
            "a destination record must not allow an identity it is not bound to"
        );
        assert_eq!(
            fx.store.authorize(&peer.identity_hash, &fake_hash(0xcd)),
            verdict(Decision::Refuse, Rule::DefaultClosed),
            "trusting one destination must not trust the identity's others"
        );
        assert!(fx.store.is_trusted_identity(&peer.identity_hash));
        assert!(
            !fx.store
                .is_trusted_destination(&peer.identity_hash, &fake_hash(0xcd))
        );
    }

    #[test]
    fn deny_outranks_block_which_outranks_both_allows_even_when_all_coexist() {
        let fx = Fixture::new("trust-precedence");
        let (identity, destination) = (fake_hash(0x1a), fake_hash(0x2b));
        let ts = "2026-01-01T00:00:00Z";
        fs::create_dir_all(fx.store.path().parent().unwrap()).unwrap();
        fs::write(
            fx.store.path(),
            format!(
                "version: {TRUST_FILE_VERSION}\n\
                 identities:\n  {identity}:\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: null\n    note: null\n    all_destinations: true\n\
                 destinations:\n  {destination}:\n    identity: {identity}\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: null\n    note: null\n\
                 denied_destinations:\n  {destination}:\n    added_at: {ts}\n    note: null\n\
                 blocked_identities:\n  {identity}:\n    added_at: {ts}\n    note: null\n"
            ),
        )
        .unwrap();
        let store = fx.reopen();
        let allow_records = |store: &TrustStore| {
            let mut hashes: Vec<String> = store.records().into_iter().map(|r| r.hash).collect();
            hashes.sort();
            hashes
        };
        let mut both = vec![identity.clone(), destination.clone()];
        both.sort();

        assert_eq!(
            store.authorize(&identity, &destination),
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        assert_eq!(allow_records(&store), both);

        store.undeny_destination(&fx.mesh, &destination).unwrap();

        assert_eq!(
            store.authorize(&identity, &destination),
            verdict(Decision::Refuse, Rule::IdentityBlocked),
            "a block must win over a destination record bound to the identity and over all_destinations"
        );
        assert!(!store.is_trusted_identity(&identity));
        assert!(!store.is_trusted_destination(&identity, &destination));
        assert_eq!(
            allow_records(&store),
            both,
            "the block is judged against surviving allow records, not against their absence"
        );
        assert_eq!(store.blocked().len(), 1);
    }

    #[test]
    fn deny_overlay_refuses_one_destination_of_a_trusted_identity_and_keeps_it_listed() {
        let fx = Fixture::new("trust-deny");
        let bystander = announced("alpha");
        fx.announce(&bystander, t(2_000));
        let identity = fake_hash(0xaa);
        let (a, b, c) = (fake_hash(0x0a), fake_hash(0x0b), fake_hash(0x0c));
        fx.store
            .trust_identity(&fx.mesh, &identity, TrustOptions::default(), t(3_000))
            .unwrap();
        for dest in [&a, &b, &c] {
            assert_eq!(
                fx.store.authorize(&identity, dest),
                verdict(Decision::Allow, Rule::IdentityTrusted)
            );
        }
        let peers_before = fx.mesh.0.snapshot();
        assert_eq!(peers_before.len(), 1);

        fx.store
            .deny_destination(&fx.mesh, &b, Some("noisy".to_string()), t(4_000))
            .unwrap();

        assert_eq!(fx.mesh.0.snapshot(), peers_before);
        assert_eq!(
            fx.store.authorize(&identity, &b),
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        assert!(!fx.store.is_trusted_destination(&identity, &b));
        assert_eq!(
            fx.store.authorize(&identity, &a),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
        assert_eq!(
            fx.store.authorize(&identity, &c),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
        assert!(fx.store.is_trusted_identity(&identity));
        assert_eq!(fx.store.denied().len(), 1);
        assert_eq!(fx.store.denied()[0].hash, b);
        assert_eq!(fx.store.denied()[0].note.as_deref(), Some("noisy"));
        let listed = fx
            .store
            .records()
            .into_iter()
            .find(|record| record.hash == b)
            .expect("a deny without a record of its own must still be listed");
        assert_eq!(listed.tier, Tier::Destination);
        assert!(listed.denied);
        assert_eq!(listed.identity, None);
        assert_eq!(listed.note.as_deref(), Some("noisy"));
        assert_eq!(listed.added_at, t(4_000));
        assert!(!listed.session);
        assert_eq!(fx.reopen().records(), fx.store.records());

        fx.store.undeny_destination(&fx.mesh, &b).unwrap();

        assert_eq!(fx.mesh.0.snapshot(), peers_before);
        assert_eq!(
            fx.store.authorize(&identity, &b),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
        assert!(fx.store.denied().is_empty());
        assert!(fx.store.records().iter().all(|record| record.hash != b));
        assert!(fx.store.undeny_destination(&fx.mesh, &b).is_err());
    }

    #[test]
    fn denied_destination_with_its_own_record_stays_in_records_flagged() {
        let fx = Fixture::new("trust-deny-listed");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();

        fx.store
            .deny_destination(&fx.mesh, &peer.destination_hash, None, t(4_000))
            .unwrap();

        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        let listed = fx
            .store
            .records()
            .into_iter()
            .filter(|record| record.hash == peer.destination_hash)
            .collect::<Vec<_>>();
        let [listed] = listed.as_slice() else {
            panic!("the denied destination must be listed exactly once, got {listed:?}");
        };
        assert!(listed.denied);
        assert_eq!(
            listed.identity.as_deref(),
            Some(peer.identity_hash.as_str())
        );
        assert_eq!(fx.reopen().records(), fx.store.records());
    }

    #[test]
    fn trust_destination_lifts_a_deny_in_the_same_write() {
        let fx = Fixture::new("trust-deny-lifted");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.store
            .deny_destination(&fx.mesh, &peer.destination_hash, None, t(2_000))
            .unwrap();
        let plain = announced("beta");
        fx.announce(&plain, t(2_000));

        let outcome = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();

        assert_eq!(outcome.change, TrustChange::Added);
        assert!(outcome.deny_lifted);
        assert!(fx.store.denied().is_empty());
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
        assert_eq!(fx.reopen().denied(), fx.store.denied());

        let again = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(4_000),
            )
            .unwrap();
        assert_eq!(again.change, TrustChange::Updated);
        assert!(!again.deny_lifted);

        let never_denied = fx
            .store
            .trust_destination(
                &fx.mesh,
                &plain.destination_hash,
                TrustOptions::default(),
                t(4_000),
            )
            .unwrap();
        assert_eq!(never_denied.change, TrustChange::Added);
        assert!(!never_denied.deny_lifted);
    }

    #[test]
    fn block_identity_removes_its_records_and_suppresses_it() {
        let fx = Fixture::new("trust-block");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        let stranger = announced("beta");
        fx.announce(&stranger, t(2_000));
        fx.store
            .trust_identity(
                &fx.mesh,
                &peer.identity_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();
        fx.store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();
        fx.store
            .trust_destination(
                &fx.mesh,
                &stranger.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();

        let removed = fx
            .store
            .block_identity(
                &fx.mesh,
                &peer.identity_hash,
                Some("spam".to_string()),
                t(4_000),
            )
            .unwrap();

        assert_eq!(removed, vec![peer.destination_hash.clone()]);
        assert!(fx.store.is_blocked_identity(&peer.identity_hash));
        assert!(!fx.store.is_trusted_identity(&peer.identity_hash));
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Refuse, Rule::IdentityBlocked)
        );
        let hashes: Vec<String> = fx.store.records().into_iter().map(|r| r.hash).collect();
        assert_eq!(
            hashes,
            vec![
                stranger.identity_hash.clone(),
                stranger.destination_hash.clone()
            ]
        );
        assert_eq!(fx.store.blocked().len(), 1);
        assert_eq!(fx.store.blocked()[0].hash, peer.identity_hash);
        assert_eq!(fx.store.blocked()[0].note.as_deref(), Some("spam"));
        assert!(fx.reopen().is_blocked_identity(&peer.identity_hash));

        let err = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(5_000),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains(".mesh unblock"), "{err}");

        fx.store
            .unblock_identity(&fx.mesh, &peer.identity_hash)
            .unwrap();

        assert!(!fx.store.is_blocked_identity(&peer.identity_hash));
        assert!(
            !fx.store.is_trusted_identity(&peer.identity_hash),
            "unblock must not restore trust"
        );
        assert!(
            fx.store
                .unblock_identity(&fx.mesh, &peer.identity_hash)
                .is_err()
        );
    }

    #[test]
    fn block_identity_clears_its_session_trust_too() {
        let fx = Fixture::new("trust-block-session");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.store
            .trust_destination_for_session(&fx.mesh, &peer.destination_hash, t(3_000))
            .unwrap();

        fx.store
            .block_identity(&fx.mesh, &peer.identity_hash, None, t(4_000))
            .unwrap();

        assert!(fx.store.records().is_empty());
        assert!(!fx.store.is_trusted_identity(&peer.identity_hash));
        assert!(
            !fx.store
                .is_trusted_destination(&peer.identity_hash, &peer.destination_hash)
        );
    }

    #[test]
    fn untrust_destination_keeps_the_identity_and_untrust_identity_cascades() {
        let fx = Fixture::new("trust-untrust");
        let first = announced("alpha");
        fx.announce(&first, t(2_000));
        fx.store
            .trust_destination(
                &fx.mesh,
                &first.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();
        let identity = fake_hash(0xaa);
        fx.store
            .trust_identity(&fx.mesh, &identity, TrustOptions::default(), t(3_000))
            .unwrap();
        let session = announced("beta");
        fx.announce(&session, t(2_000));
        fx.store
            .trust_destination_for_session(&fx.mesh, &session.destination_hash, t(3_000))
            .unwrap();
        fx.store.mark_seen(
            &first.destination_hash,
            &first.identity_hash,
            &first.name_hash,
            t(9_000),
        );

        fx.store
            .untrust_destination(
                &fx.mesh,
                &format!(" {}", first.destination_hash),
                t(9_000),
                false,
            )
            .unwrap();
        fx.store
            .untrust_destination(&fx.mesh, &session.destination_hash, t(9_000), false)
            .unwrap();

        let mut hashes: Vec<String> = fx.store.records().into_iter().map(|r| r.hash).collect();
        hashes.sort();
        let mut expected = vec![
            identity.clone(),
            first.identity_hash.clone(),
            session.identity_hash.clone(),
        ];
        expected.sort();
        assert_eq!(hashes, expected);
        assert!(
            !fx.store
                .is_trusted_destination(&session.identity_hash, &session.destination_hash),
            "a session-trusted destination must be gone after untrust_destination"
        );
        assert!(
            fx.store
                .untrust_destination(&fx.mesh, &first.destination_hash, t(9_000), false)
                .is_err()
        );

        fx.announce(&first, t(4_000));
        fx.store
            .trust_destination(
                &fx.mesh,
                &first.destination_hash,
                TrustOptions::default(),
                t(4_000),
            )
            .unwrap();
        let retrusted = fx
            .store
            .records()
            .into_iter()
            .find(|record| record.hash == first.destination_hash)
            .unwrap();
        assert_eq!(
            retrusted.last_seen_at,
            t(4_000),
            "a re-trust must not inherit the sighting of the removed record"
        );
        fx.store.mark_seen(
            &first.destination_hash,
            &first.identity_hash,
            &first.name_hash,
            t(9_000),
        );

        let removed = fx
            .store
            .untrust_identity(&fx.mesh, &first.identity_hash.to_ascii_uppercase())
            .unwrap();

        assert_eq!(removed, vec![first.destination_hash.clone()]);
        let mut hashes: Vec<String> = fx.store.records().into_iter().map(|r| r.hash).collect();
        hashes.sort();
        let mut expected = vec![identity.clone(), session.identity_hash.clone()];
        expected.sort();
        assert_eq!(hashes, expected);
        let on_disk: Vec<TrustRecord> = fx
            .store
            .records()
            .into_iter()
            .filter(|record| !record.session)
            .collect();
        assert_eq!(fx.reopen().records(), on_disk);
        assert!(
            fx.store
                .untrust_identity(&fx.mesh, &first.identity_hash)
                .is_err()
        );

        fx.store
            .trust_destination(
                &fx.mesh,
                &first.destination_hash,
                TrustOptions::default(),
                t(5_000),
            )
            .unwrap();
        let retrusted = fx
            .store
            .records()
            .into_iter()
            .find(|record| record.hash == first.destination_hash)
            .unwrap();
        assert_eq!(retrusted.last_seen_at, t(4_000));
    }

    #[test]
    fn prune_removes_only_stale_destinations_and_honours_dry_run_and_mark_seen() {
        let fx = Fixture::new("trust-prune");
        let stale = announced("alpha");
        let fresh = announced("beta");
        let revived = announced("gamma");
        fx.store.mark_seen(
            &stale.destination_hash,
            &stale.identity_hash,
            &stale.name_hash,
            t(9_000),
        );
        for peer in [&stale, &fresh, &revived] {
            fx.announce(peer, t(1_000));
            fx.store
                .trust_destination(
                    &fx.mesh,
                    &peer.destination_hash,
                    TrustOptions::default(),
                    t(1_000),
                )
                .unwrap();
        }
        fx.announce(&fresh, t(5_000));
        fx.store
            .trust_destination(
                &fx.mesh,
                &fresh.destination_hash,
                TrustOptions::default(),
                t(5_000),
            )
            .unwrap();
        fx.store.mark_seen(
            &revived.destination_hash,
            &revived.identity_hash,
            &revived.name_hash,
            t(5_000),
        );
        let now = t(1_000 + 3_600);
        let horizon = Duration::from_secs(3_600);

        let dry = fx
            .store
            .prune_destinations(&fx.mesh, horizon, now, true)
            .unwrap();
        assert_eq!(
            dry,
            vec![stale.destination_hash.clone()],
            "a mark_seen before the destination had a record must have been ignored"
        );
        assert_eq!(fx.store.records().len(), 6, "a dry run must remove nothing");

        let removed = fx
            .store
            .prune_destinations(&fx.mesh, horizon, now, false)
            .unwrap();
        assert_eq!(removed, vec![stale.destination_hash.clone()]);
        let hashes: Vec<String> = fx
            .store
            .records()
            .into_iter()
            .filter(|r| r.tier == Tier::Destination)
            .map(|r| r.hash)
            .collect();
        let mut expected = vec![
            fresh.destination_hash.clone(),
            revived.destination_hash.clone(),
        ];
        expected.sort();
        assert_eq!(hashes, expected);
        assert_eq!(
            fx.store
                .records()
                .iter()
                .filter(|r| r.tier == Tier::Identity)
                .count(),
            3,
            "identities are never prune candidates"
        );
        assert!(
            fx.store
                .prune_destinations(&fx.mesh, horizon, t(0), false)
                .unwrap()
                .is_empty(),
            "a last_seen_at in the future reads as just seen"
        );
    }

    #[test]
    fn mark_seen_refreshes_only_a_provable_sighting_of_the_bound_identity() {
        let fx = Fixture::new("trust-mark-seen-proof");
        let peer = announced("alpha");
        let other = announced("beta");
        fx.announce(&peer, t(2_000));
        fx.store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();
        let before = fx.file_bytes().unwrap();
        let last_seen = || {
            fx.store
                .records()
                .into_iter()
                .find(|record| record.hash == peer.destination_hash)
                .unwrap()
                .last_seen_at
        };

        fx.store.mark_seen(
            &peer.destination_hash,
            &peer.identity_hash,
            &peer.name_hash,
            t(5_000),
        );
        assert_eq!(last_seen(), t(5_000));

        fx.store.mark_seen(
            &peer.destination_hash,
            &other.identity_hash,
            &other.name_hash,
            t(6_000),
        );
        assert_eq!(
            last_seen(),
            t(5_000),
            "a stranger announcing under the trusted hash is not a sighting"
        );

        fx.store
            .mark_seen(&peer.destination_hash, &peer.identity_hash, "", t(7_000));
        assert_eq!(
            last_seen(),
            t(5_000),
            "an announce without a name hash cannot be proved"
        );

        fx.store.mark_seen(
            &peer.destination_hash,
            &peer.identity_hash,
            &other.name_hash,
            t(8_000),
        );
        assert_eq!(
            last_seen(),
            t(5_000),
            "a name hash that does not derive the destination proves nothing"
        );
        assert_eq!(fx.file_bytes().unwrap(), before);
    }

    #[test]
    fn prune_counts_a_peer_table_announce_only_when_it_verifies_as_the_record_identity() {
        let fx = Fixture::new("trust-prune-peer-table");
        let reannounced = announced("alpha");
        let usurped = announced("beta");
        let stranger = announced("delta");
        for peer in [&reannounced, &usurped] {
            fx.announce(peer, t(1_000));
            fx.store
                .trust_destination(
                    &fx.mesh,
                    &peer.destination_hash,
                    TrustOptions::default(),
                    t(1_000),
                )
                .unwrap();
        }
        fx.announce(&reannounced, t(4_000));
        fx.mesh.0.observe(
            PeerSighting {
                destination_hash: usurped.destination_hash.clone(),
                identity_hash: stranger.identity_hash.clone(),
                name_hash: stranger.name_hash.clone(),
                display_name: Some("Mallory".to_string()),
                protocol_version: 1,
                hops: 1,
            },
            t(4_000),
        );
        let now = t(1_000 + 3_600);
        let horizon = Duration::from_secs(3_600);

        let dry = fx
            .store
            .prune_destinations(&fx.mesh, horizon, now, true)
            .unwrap();
        assert_eq!(
            dry,
            vec![usurped.destination_hash.clone()],
            "a fresh announce rescues its destination only when the record's identity provably sent it"
        );
        assert_eq!(fx.store.records().len(), 4);

        let removed = fx
            .store
            .prune_destinations(&fx.mesh, horizon, now, false)
            .unwrap();
        assert_eq!(removed, vec![usurped.destination_hash.clone()]);
        let destinations: Vec<String> = fx
            .store
            .records()
            .into_iter()
            .filter(|r| r.tier == Tier::Destination)
            .map(|r| r.hash)
            .collect();
        assert_eq!(destinations, vec![reannounced.destination_hash.clone()]);
        assert!(
            fx.store
                .prune_destinations(&fx.mesh, horizon, now, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn queries_and_mark_seen_never_write_or_remove() {
        let fx = Fixture::new("trust-readonly");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(3_000),
            )
            .unwrap();
        fx.store
            .deny_destination(&fx.mesh, &fake_hash(0xdd), None, t(3_000))
            .unwrap();
        fx.store
            .block_identity(&fx.mesh, &fake_hash(0xbb), None, t(3_000))
            .unwrap();
        let before = fx.file_bytes().unwrap();
        let store = fx.reopen();
        let count = store.records().len();

        store.authorize(&peer.identity_hash, &peer.destination_hash);
        store.authorize(&fake_hash(0xbb), &fake_hash(0xdd));
        store.is_trusted_destination(&peer.identity_hash, &peer.destination_hash);
        store.is_trusted_identity(&peer.identity_hash);
        store.is_blocked_identity(&fake_hash(0xbb));
        store.blocked();
        store.denied();
        store.mark_seen(
            &peer.destination_hash,
            &peer.identity_hash,
            &peer.name_hash,
            t(9_000),
        );
        store.mark_seen(
            &peer.destination_hash,
            &peer.identity_hash,
            &peer.name_hash,
            t(8_000),
        );

        assert_eq!(fx.file_bytes().unwrap(), before);
        assert_eq!(store.records().len(), count);
        let seen = store
            .records()
            .into_iter()
            .find(|record| record.hash == peer.destination_hash)
            .unwrap();
        assert_eq!(
            seen.last_seen_at,
            t(9_000),
            "records must report the fresher sighting"
        );
        assert_eq!(
            fx.reopen()
                .records()
                .into_iter()
                .find(|record| record.hash == peer.destination_hash)
                .unwrap()
                .last_seen_at,
            t(2_000),
            "mark_seen must not reach the file"
        );
    }

    #[test]
    fn every_mutation_refuses_while_mesh_is_off() {
        let tmp = TempDir::new("trust-mesh-off");
        let store = TrustStore::open(&tmp.path).unwrap();
        let mesh = MeshOff;
        let dest = fake_hash(0x0d);
        let identity = fake_hash(0x1d);
        let now = t(3_000);
        let attempts: Vec<(&str, Result<()>)> = vec![
            (
                "trust_destination",
                store
                    .trust_destination(&mesh, &dest, TrustOptions::default(), now)
                    .map(drop),
            ),
            (
                "trust_destination_for_session",
                store
                    .trust_destination_for_session(&mesh, &dest, now)
                    .map(drop),
            ),
            (
                "trust_identity",
                store
                    .trust_identity(&mesh, &identity, TrustOptions::default(), now)
                    .map(drop),
            ),
            (
                "untrust_destination",
                store
                    .untrust_destination(&mesh, &dest, now, false)
                    .map(drop),
            ),
            (
                "untrust_identity",
                store.untrust_identity(&mesh, &identity).map(drop),
            ),
            (
                "block_identity",
                store.block_identity(&mesh, &identity, None, now).map(drop),
            ),
            ("unblock_identity", store.unblock_identity(&mesh, &identity)),
            (
                "deny_destination",
                store.deny_destination(&mesh, &dest, None, now),
            ),
            ("undeny_destination", store.undeny_destination(&mesh, &dest)),
            (
                "prune_destinations",
                store
                    .prune_destinations(&mesh, Duration::from_secs(1), now, false)
                    .map(drop),
            ),
        ];

        for (name, result) in attempts {
            let err = result
                .err()
                .unwrap_or_else(|| panic!("{name} must refuse while mesh is off"))
                .to_string();
            assert!(err.contains(".mesh on"), "{name}: {err}");
        }
        assert!(store.records().is_empty());
        assert!(!store.path().exists());
        assert!(!tmp.path.join("mesh").exists());
    }

    /// The observer tests drive the store's mutation API directly; no REPL command is
    /// involved.
    #[test]
    fn trust_identity_fires_granted_once_and_an_update_fires_nothing() {
        let fx = Fixture::new("trust-hook-identity");
        let sink = fx.observed();
        let identity = fake_hash(0xaa);

        assert_eq!(fx.trust_identity(&identity, t(3_000)), TrustChange::Added);

        let envs = one_fire(&sink, HookEvent::MeshTrustGranted);
        assert_eq!(env_value(&envs, "COYOTE_MESH_TRUST_TIER"), Some("identity"));
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_IDENTITY"),
            Some(identity.as_str())
        );
        assert_eq!(env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"), None);

        assert_eq!(fx.trust_identity(&identity, t(4_000)), TrustChange::Updated);
        assert!(sink.drain().is_empty(), "an update grants nothing new");
    }

    /// `trust_destination` leaves a binding-only identity record behind, so the later
    /// `trust_identity` reads as an update to the caller while being the actual grant.
    #[test]
    fn trust_identity_over_a_binding_only_record_fires_granted_and_untrust_revokes_it() {
        let fx = Fixture::new("trust-hook-identity-upgrade");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_destination(&peer, t(3_000));
        let sink = fx.observed();

        assert_eq!(
            fx.trust_identity(&peer.identity_hash, t(4_000)),
            TrustChange::Updated
        );

        let envs = one_fire(&sink, HookEvent::MeshTrustGranted);
        assert_eq!(env_value(&envs, "COYOTE_MESH_TRUST_TIER"), Some("identity"));
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_IDENTITY"),
            Some(peer.identity_hash.as_str())
        );
        assert_eq!(env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"), None);

        fx.store
            .untrust_identity(&fx.mesh, &peer.identity_hash)
            .unwrap();

        let fired = sink.drain();
        let tiers: Vec<Option<&str>> = fired
            .iter()
            .map(|(_, envs)| env_value(envs, "COYOTE_MESH_TRUST_TIER"))
            .collect();
        assert_eq!(tiers, [Some("identity"), Some("destination")]);
    }

    #[test]
    fn untrusting_a_binding_only_identity_revokes_its_destinations_alone() {
        let fx = Fixture::new("trust-hook-identity-binding-only");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_destination(&peer, t(3_000));
        let sink = fx.observed();

        let removed = fx
            .store
            .untrust_identity(&fx.mesh, &peer.identity_hash)
            .unwrap();

        assert_eq!(removed, std::slice::from_ref(&peer.destination_hash));
        let envs = one_fire(&sink, HookEvent::MeshTrustRevoked);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_TRUST_TIER"),
            Some("destination")
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"),
            Some(peer.destination_hash.as_str())
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_IDENTITY"),
            Some(peer.identity_hash.as_str())
        );
    }

    #[test]
    fn trust_destination_fires_granted_with_both_hashes() {
        let fx = Fixture::new("trust-hook-destination");
        let sink = fx.observed();
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));

        assert_eq!(fx.trust_destination(&peer, t(3_000)), TrustChange::Added);

        let envs = one_fire(&sink, HookEvent::MeshTrustGranted);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_TRUST_TIER"),
            Some("destination")
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_IDENTITY"),
            Some(peer.identity_hash.as_str())
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"),
            Some(peer.destination_hash.as_str())
        );
    }

    #[test]
    fn untrust_identity_fires_revoked_for_the_identity_and_each_destination() {
        let fx = Fixture::new("trust-hook-untrust-identity");
        let identity = PrivateIdentity::new_from_rand(OsRng);
        let first = announced_as(identity.clone(), "alpha");
        let second = announced_as(identity, "beta");
        assert_eq!(first.identity_hash, second.identity_hash);
        fx.announce(&first, t(2_000));
        fx.announce(&second, t(2_000));
        fx.trust_identity(&first.identity_hash, t(3_000));
        fx.trust_destination(&first, t(3_000));
        fx.trust_destination(&second, t(3_000));
        let sink = fx.observed();

        let removed = fx
            .store
            .untrust_identity(&fx.mesh, &first.identity_hash)
            .unwrap();

        assert_eq!(removed.len(), 2);
        let fired = sink.drain();
        assert_eq!(fired.len(), 3, "{fired:?}");
        for (event, envs) in &fired {
            assert_eq!(*event, HookEvent::MeshTrustRevoked);
            assert_eq!(env_value(envs, "COYOTE_MESH_TRUST_REASON"), Some("untrust"));
            assert_eq!(
                env_value(envs, "COYOTE_MESH_PEER_IDENTITY"),
                Some(first.identity_hash.as_str())
            );
        }
        let tiers: Vec<Option<&str>> = fired
            .iter()
            .map(|(_, envs)| env_value(envs, "COYOTE_MESH_TRUST_TIER"))
            .collect();
        assert_eq!(
            tiers,
            [Some("identity"), Some("destination"), Some("destination")]
        );
        assert_eq!(env_value(&fired[0].1, "COYOTE_MESH_PEER_DESTINATION"), None);
        let mut destinations: Vec<&str> = fired[1..]
            .iter()
            .filter_map(|(_, envs)| env_value(envs, "COYOTE_MESH_PEER_DESTINATION"))
            .collect();
        destinations.sort_unstable();
        let mut expected = [
            first.destination_hash.as_str(),
            second.destination_hash.as_str(),
        ];
        expected.sort_unstable();
        assert_eq!(destinations, expected);
    }

    #[test]
    fn block_identity_fires_revoked_only_for_an_identity_on_the_list() {
        let fx = Fixture::new("trust-hook-block");
        let listed = fake_hash(0xaa);
        fx.trust_identity(&listed, t(3_000));
        let sink = fx.observed();

        fx.store
            .block_identity(&fx.mesh, &listed, Some("spam".to_string()), t(4_000))
            .unwrap();

        let envs = one_fire(&sink, HookEvent::MeshTrustRevoked);
        assert_eq!(env_value(&envs, "COYOTE_MESH_TRUST_TIER"), Some("identity"));
        assert_eq!(env_value(&envs, "COYOTE_MESH_TRUST_REASON"), Some("block"));
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_IDENTITY"),
            Some(listed.as_str())
        );

        let stranger = fake_hash(0xbb);
        fx.store
            .block_identity(&fx.mesh, &stranger, None, t(4_000))
            .unwrap();

        assert!(fx.store.is_blocked_identity(&stranger));
        assert!(
            sink.drain().is_empty(),
            "blocking an identity the list never trusted revokes nothing"
        );
    }

    #[test]
    fn prune_fires_revoked_per_stale_destination_and_a_dry_run_fires_nothing() {
        let fx = Fixture::new("trust-hook-prune");
        let stale = [announced("alpha"), announced("beta")];
        for peer in &stale {
            fx.announce(peer, t(1_000));
            fx.trust_destination(peer, t(1_000));
        }
        let sink = fx.observed();
        let now = t(1_000 + 3_600);
        let horizon = Duration::from_secs(3_600);

        let dry = fx
            .store
            .prune_destinations(&fx.mesh, horizon, now, true)
            .unwrap();
        assert_eq!(dry.len(), 2);
        assert!(sink.drain().is_empty(), "a dry run revokes nothing");

        let removed = fx
            .store
            .prune_destinations(&fx.mesh, horizon, now, false)
            .unwrap();
        assert_eq!(removed.len(), 2);

        let fired = sink.drain();
        assert_eq!(fired.len(), 2, "{fired:?}");
        for (event, envs) in &fired {
            assert_eq!(*event, HookEvent::MeshTrustRevoked);
            assert_eq!(
                env_value(envs, "COYOTE_MESH_TRUST_TIER"),
                Some("destination")
            );
            assert_eq!(env_value(envs, "COYOTE_MESH_TRUST_REASON"), Some("prune"));
            let destination = env_value(envs, "COYOTE_MESH_PEER_DESTINATION").unwrap();
            let peer = stale
                .iter()
                .find(|peer| peer.destination_hash == destination)
                .unwrap();
            assert_eq!(
                env_value(envs, "COYOTE_MESH_PEER_IDENTITY"),
                Some(peer.identity_hash.as_str())
            );
        }
    }

    #[test]
    fn deny_undeny_and_unblock_fire_nothing() {
        let fx = Fixture::new("trust-hook-overlays");
        let sink = fx.observed();
        let destination = fake_hash(0xcc);
        let identity = fake_hash(0xdd);

        fx.store
            .deny_destination(&fx.mesh, &destination, Some("noisy".to_string()), t(3_000))
            .unwrap();
        fx.store.undeny_destination(&fx.mesh, &destination).unwrap();
        fx.store
            .block_identity(&fx.mesh, &identity, None, t(3_000))
            .unwrap();
        fx.store.unblock_identity(&fx.mesh, &identity).unwrap();

        assert!(sink.drain().is_empty());
    }

    #[test]
    fn untrust_destination_fires_one_revoked_for_the_destination_tier() {
        let fx = Fixture::new("trust-hook-untrust-destination");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_destination(&peer, t(3_000));
        let sink = fx.observed();

        fx.store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(4_000), false)
            .unwrap();

        let envs = one_fire(&sink, HookEvent::MeshTrustRevoked);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_TRUST_TIER"),
            Some("destination")
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_TRUST_REASON"),
            Some("untrust")
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"),
            Some(peer.destination_hash.as_str())
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_IDENTITY"),
            Some(peer.identity_hash.as_str())
        );
    }

    #[test]
    fn untrust_destination_dry_run_reports_a_refusal_for_a_trusted_all_identity_and_writes_nothing()
    {
        let fx = Fixture::new("trust-untrust-refuse-dry-run");
        let peer = announced("alpha");
        let sibling = fake_hash(0xcc);
        fx.announce(&peer, t(2_000));
        fx.trust_identity(&peer.identity_hash, t(2_000));
        let sink = fx.observed();
        let before = fx.file_bytes();
        let refused = UntrustOutcome::Refused {
            identity: peer.identity_hash.clone(),
            already_refused: false,
        };

        let outcome = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(3_000), true)
            .unwrap();

        assert_eq!(outcome, refused);
        assert_eq!(fx.file_bytes(), before);
        assert!(fx.store.denied().is_empty());
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
        assert!(sink.drain().is_empty(), "a dry run fires nothing");

        let outcome = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(3_000), false)
            .unwrap();

        assert_eq!(outcome, refused);
        assert_eq!(fx.store.denied().len(), 1);
        assert_eq!(fx.store.denied()[0].hash, peer.destination_hash);
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        assert_eq!(
            fx.store.authorize(&peer.identity_hash, &sibling),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
        assert!(
            sink.drain().is_empty(),
            "a refusal removes nothing, so it revokes nothing"
        );

        let outcome = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(4_000), false)
            .unwrap();

        assert_eq!(
            outcome,
            UntrustOutcome::Refused {
                identity: peer.identity_hash.clone(),
                already_refused: true,
            }
        );
    }

    #[test]
    fn untrust_destination_of_a_bound_record_under_a_trusted_all_identity_refuses_it_and_keeps_the_record()
     {
        let fx = Fixture::new("trust-untrust-refuse-bound");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_identity(&peer.identity_hash, t(3_000));
        fx.store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions {
                    label: Some("Bob".to_string()),
                    note: None,
                },
                t(3_000),
            )
            .unwrap();
        let sink = fx.observed();

        let outcome = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(4_000), false)
            .unwrap();

        assert_eq!(
            outcome,
            UntrustOutcome::Refused {
                identity: peer.identity_hash.clone(),
                already_refused: false,
            }
        );
        let listed = record(&fx.store, &peer.destination_hash);
        assert_eq!(listed.tier, Tier::Destination);
        assert!(listed.denied);
        assert_eq!(
            listed.identity.as_deref(),
            Some(peer.identity_hash.as_str())
        );
        assert_eq!(listed.label.as_deref(), Some("Bob"));
        assert!(
            !fx.store
                .is_trusted_destination(&peer.identity_hash, &peer.destination_hash)
        );
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        assert_eq!(fx.store.denied().len(), 1);
        assert_eq!(fx.store.denied()[0].hash, peer.destination_hash);
        assert!(
            sink.drain().is_empty(),
            "the record stays, so there is nothing to revoke"
        );
        assert_eq!(fx.reopen().records(), fx.store.records());
    }

    #[test]
    fn untrust_destination_ignores_a_peer_row_whose_identity_column_does_not_derive_the_destination()
     {
        let fx = Fixture::new("trust-untrust-tampered-row");
        let peer = announced("alpha");
        let impostor = announced("beta");
        fx.announce(&impostor, t(2_000));
        fx.trust_identity(&impostor.identity_hash, t(2_000));
        fx.mesh.0.observe(
            PeerSighting {
                identity_hash: impostor.identity_hash.clone(),
                ..sighting(&peer)
            },
            t(2_000),
        );
        let before = fx.file_bytes();

        let err = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(3_000), false)
            .unwrap_err()
            .to_string();

        assert!(err.contains("not in the trust list"), "{err}");
        assert_eq!(fx.file_bytes(), before);
        assert!(fx.store.denied().is_empty());
    }

    #[test]
    fn untrust_destination_of_a_peer_table_only_instance_writes_a_bound_record_with_the_deny() {
        let fx = Fixture::new("trust-untrust-refuse-peer-table-only");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_identity(&peer.identity_hash, t(3_000));
        let sink = fx.observed();

        let outcome = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(4_000), false)
            .unwrap();

        assert_eq!(
            outcome,
            UntrustOutcome::Refused {
                identity: peer.identity_hash.clone(),
                already_refused: false,
            }
        );
        let listed = record(&fx.store, &peer.destination_hash);
        assert_eq!(listed.tier, Tier::Destination);
        assert_eq!(
            listed.identity.as_deref(),
            Some(peer.identity_hash.as_str())
        );
        assert_eq!(listed.label, None);
        assert!(listed.denied);
        assert!(!listed.session);
        assert_eq!(listed.added_at, t(4_000));
        assert_eq!(listed.last_seen_at, t(2_000));
        assert_eq!(fx.store.denied().len(), 1);
        assert_eq!(fx.store.denied()[0].hash, peer.destination_hash);
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        assert!(
            sink.drain().is_empty(),
            "a refusal grants nothing, so it fires nothing"
        );
        assert_eq!(fx.reopen().records(), fx.store.records());
    }

    #[test]
    fn untrust_identity_drops_the_denies_of_the_instances_it_forgets() {
        let fx = Fixture::new("trust-untrust-identity-sweeps-denies");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_identity(&peer.identity_hash, t(2_000));
        fx.trust_destination(&peer, t(2_000));
        fx.store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(3_000), false)
            .unwrap();
        assert_eq!(fx.store.denied().len(), 1);

        let removed = fx
            .store
            .untrust_identity(&fx.mesh, &peer.identity_hash)
            .unwrap();

        assert_eq!(removed, vec![peer.destination_hash.clone()]);
        assert!(fx.store.denied().is_empty());
        assert!(fx.store.records().is_empty());
        assert_eq!(fx.reopen().denied(), fx.store.denied());
    }

    #[test]
    fn untrust_identity_drops_the_deny_of_a_peer_table_only_instance_it_forgets() {
        let fx = Fixture::new("trust-untrust-identity-sweeps-peer-table-deny");
        let identity = PrivateIdentity::new_from_rand(OsRng);
        let bound = announced_as(identity.clone(), "alpha");
        let heard = announced_as(identity, "beta");
        fx.announce(&bound, t(2_000));
        fx.announce(&heard, t(2_000));
        fx.trust_identity(&bound.identity_hash, t(2_000));
        fx.trust_destination(&bound, t(2_000));
        fx.store
            .untrust_destination(&fx.mesh, &bound.destination_hash, t(3_000), false)
            .unwrap();
        fx.store
            .untrust_destination(&fx.mesh, &heard.destination_hash, t(3_000), false)
            .unwrap();
        assert_eq!(fx.store.denied().len(), 2);

        let mut removed = fx
            .store
            .untrust_identity(&fx.mesh, &bound.identity_hash)
            .unwrap();
        removed.sort();

        let mut expected = vec![
            bound.destination_hash.clone(),
            heard.destination_hash.clone(),
        ];
        expected.sort();
        assert_eq!(removed, expected);
        assert!(fx.store.denied().is_empty());
        assert!(fx.store.records().is_empty());
        assert_eq!(
            fx.store
                .authorize(&bound.identity_hash, &heard.destination_hash),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert_eq!(fx.reopen().denied(), fx.store.denied());
    }

    #[test]
    fn untrust_destination_dry_run_on_a_bound_record_writes_nothing() {
        let fx = Fixture::new("trust-untrust-forget-dry-run");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_destination(&peer, t(2_000));
        let sink = fx.observed();
        let before = fx.file_bytes();

        let outcome = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(3_000), true)
            .unwrap();

        assert_eq!(
            outcome,
            UntrustOutcome::Forgotten {
                identity: peer.identity_hash.clone()
            }
        );
        assert_eq!(fx.file_bytes(), before);
        assert_eq!(
            record(&fx.store, &peer.destination_hash).tier,
            Tier::Destination
        );
        assert!(
            fx.store
                .is_trusted_destination(&peer.identity_hash, &peer.destination_hash)
        );
        assert!(sink.drain().is_empty());
    }

    #[test]
    fn a_second_refusal_is_a_no_op_that_does_not_rewrite_the_file() {
        let fx = Fixture::new("trust-untrust-refuse-twice");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_identity(&peer.identity_hash, t(2_000));
        fx.store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(3_000), false)
            .unwrap();
        let before = fx.file_bytes();
        assert!(before.is_some());

        let outcome = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(4_000), false)
            .unwrap();

        assert_eq!(
            outcome,
            UntrustOutcome::Refused {
                identity: peer.identity_hash.clone(),
                already_refused: true,
            }
        );
        assert_eq!(fx.file_bytes(), before);
        assert_eq!(fx.store.denied().len(), 1);
    }

    /// Usage probe: the whole refuse-then-lift cycle on a trusted-all identity's
    /// instance is silent to hooks. The refusal removes nothing (no `revoked`), the
    /// second refusal changes nothing, and the lift is an `Updated` on the intact
    /// record (no `granted`, like every other update). Only forgetting the identity
    /// afterwards speaks, and then once per record. A hook author therefore learns of
    /// a refusal only from `.mesh` output, never from `mesh.trust.*`.
    #[test]
    fn usage_probe_refuse_lift_and_refuse_again_fire_no_trust_hooks() {
        let fx = Fixture::new("trust-usage-probe-refuse-lift-silent");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_identity(&peer.identity_hash, t(2_000));
        fx.store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions {
                    label: Some("Bob".to_string()),
                    note: None,
                },
                t(2_000),
            )
            .unwrap();
        let sink = fx.observed();

        fx.store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(3_000), false)
            .unwrap();
        assert!(sink.drain().is_empty(), "a refusal removes nothing");

        fx.store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(3_500), false)
            .unwrap();
        assert!(sink.drain().is_empty(), "a second refusal is a no-op");

        let lifted = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(4_000),
            )
            .unwrap();
        assert_eq!(lifted.change, TrustChange::Updated);
        assert!(lifted.deny_lifted);
        assert_eq!(
            record(&fx.store, &peer.destination_hash).label.as_deref(),
            Some("Bob"),
            "the lift keeps the label the record had before the refusal"
        );
        assert!(
            sink.drain().is_empty(),
            "lifting a refusal on an intact record is an update, not a grant"
        );

        let removed = fx
            .store
            .untrust_identity(&fx.mesh, &peer.identity_hash)
            .unwrap();
        assert_eq!(removed, vec![peer.destination_hash.clone()]);
        let fired = sink.drain();
        assert_eq!(fired.len(), 2, "{fired:?}");
        assert!(
            fired
                .iter()
                .all(|(event, _)| *event == HookEvent::MeshTrustRevoked),
            "{fired:?}"
        );
    }

    /// Usage probe: lifting a deny that has no record under it (a seeded overlay; a
    /// refusal always writes a record) is a fresh grant and fires `granted` with both
    /// hashes, so the two shapes of lift differ only in whether a hook speaks.
    #[test]
    fn usage_probe_lifting_a_record_less_deny_is_a_grant_that_fires_granted() {
        let fx = Fixture::new("trust-usage-probe-lift-orphan-grants");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_identity(&peer.identity_hash, t(2_000));
        fx.store
            .deny_destination(&fx.mesh, &peer.destination_hash, None, t(3_000))
            .unwrap();
        assert!(
            !fx.store
                .records()
                .iter()
                .any(|record| record.hash == peer.destination_hash && !record.denied),
            "no trusted record sits under the deny"
        );
        let sink = fx.observed();

        let lifted = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions::default(),
                t(4_000),
            )
            .unwrap();

        assert_eq!(lifted.change, TrustChange::Added);
        assert!(lifted.deny_lifted);
        let envs = one_fire(&sink, HookEvent::MeshTrustGranted);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"),
            Some(peer.destination_hash.as_str())
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_IDENTITY"),
            Some(peer.identity_hash.as_str())
        );
    }

    #[test]
    fn a_rotated_peer_is_a_stranger_to_its_old_grant() {
        let fx = Fixture::new("trust-rotated-stranger");
        let (old, new) = trusted_then_rotated(&fx);

        assert_eq!(
            fx.store
                .authorize(&new.identity_hash, &new.destination_hash),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert_eq!(
            fx.store
                .authorize(&new.identity_hash, &old.destination_hash),
            verdict(Decision::Refuse, Rule::DefaultClosed),
            "the old destination record must not allow the new key"
        );
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert!(!fx.store.is_trusted_identity(&new.identity_hash));
        assert_eq!(
            fx.store.identity_standing(&new.identity_hash),
            IdentityStanding::Unknown
        );
        assert_eq!(
            fx.store
                .authorize(&old.identity_hash, &old.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "the grant stays with the identity that proved it"
        );
        assert_eq!(
            origin_verdict(&fx.store, &old),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
    }

    /// The new identity has standing of its own here, so the dispatcher reaches the verdict
    /// stage; identity changed refuses it, and marking the record grants it nothing.
    #[test]
    fn identity_changed_is_never_an_allow() {
        let fx = Fixture::new("trust-identity-changed-refuses");
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_destination(&old, t(3_000));
        let rotated = PrivateIdentity::new_from_rand(OsRng);
        let new = announced_as(rotated.clone(), "alpha");
        let new_key_elsewhere = announced_as(rotated, "beta");
        fx.announce(&new_key_elsewhere, t(4_000));
        fx.trust_destination(&new_key_elsewhere, t(4_500));
        assert!(fx.store.is_trusted_identity(&new.identity_hash));

        let (before, destination) = fx.store.authorize_origin(
            &parse_hash(&new.identity_hash).unwrap(),
            &decode_name_hash(&new.name_hash).unwrap(),
        );
        assert_eq!(before, verdict(Decision::Refuse, Rule::IdentityChanged));
        assert_eq!(destination.to_hex_string(), new.destination_hash);

        let marked = fx
            .store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));
        assert_eq!(marked.len(), 1);

        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "a marked record still refuses the identity that caused the mark"
        );
        assert_eq!(
            fx.store
                .authorize(&new.identity_hash, &old.destination_hash),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert_eq!(
            record(&fx.store, &old.destination_hash).identity.as_deref(),
            Some(old.identity_hash.as_str()),
            "the mark must not re-bind the record"
        );
        assert_eq!(
            fx.store
                .authorize(&old.identity_hash, &old.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
        assert!(
            fx.store
                .records()
                .iter()
                .all(|record| record.key_changed.is_none() || record.hash == old.destination_hash),
            "only the conflicting record carries a mark"
        );
    }

    #[test]
    fn a_new_identity_on_a_known_instance_marks_the_record_once_and_notifies_once() {
        let fx = Fixture::new("trust-key-change-once");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        let (old, new) = trusted_then_rotated(&fx);
        let hooks = fx.observed();

        let marked = fx
            .store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));

        assert_eq!(
            marked,
            vec![MarkedKeyChange {
                destination_hash: old.destination_hash.clone(),
                bound_identity: old.identity_hash.clone(),
                seen_identity: new.identity_hash.clone(),
                label: None,
            }]
        );
        assert_eq!(
            record(&fx.store, &old.destination_hash).key_changed,
            Some(KeyChange {
                seen_identity: new.identity_hash.clone(),
                at: t(5_000),
            })
        );
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        let text = &texts[0];
        assert!(text.contains(short(&old.destination_hash)), "{text}");
        assert!(text.contains(short(&old.identity_hash)), "{text}");
        assert!(text.contains(short(&new.identity_hash)), "{text}");
        assert!(
            text.contains(&format!(".mesh trust {}", new.destination_hash)),
            "{text}"
        );
        assert!(
            text.contains(&format!(".mesh block {}", new.identity_hash)),
            "{text}"
        );
        assert!(text.contains("announced under"), "{text}");
        let after_first = fx.file_bytes().unwrap();
        assert!(
            String::from_utf8(after_first.clone())
                .unwrap()
                .contains("key_changed:")
        );

        let again = fx
            .store
            .note_key_change(&new.identity_hash, &new.name_hash, t(6_000));
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(fx.file_bytes().unwrap(), after_first);

        let third = announced("alpha");
        let other = fx
            .store
            .note_key_change(&third.identity_hash, &third.name_hash, t(7_000));
        assert!(other.is_empty(), "{other:?}");
        assert_eq!(fx.file_bytes().unwrap(), after_first);
        assert_eq!(
            record(&fx.store, &old.destination_hash)
                .key_changed
                .map(|mark| mark.seen_identity),
            Some(new.identity_hash.clone()),
            "the mark keeps its first sighting"
        );
        assert_eq!(surface.texts().len(), 1);
        assert!(hooks.drain().is_empty(), "a key change fires no trust hook");
    }

    #[test]
    fn key_change_mark_survives_restart() {
        let fx = Fixture::new("trust-key-change-restart");
        let (old, new) = trusted_then_rotated(&fx);
        fx.store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));

        let reopened = fx.reopen();

        assert_eq!(
            record(&reopened, &old.destination_hash).key_changed,
            Some(KeyChange {
                seen_identity: new.identity_hash.clone(),
                at: t(5_000),
            })
        );
        assert_eq!(
            origin_verdict(&reopened, &new),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert!(
            reopened
                .note_key_change(&new.identity_hash, &new.name_hash, t(6_000))
                .is_empty(),
            "a mark loaded from disk is not written again"
        );
    }

    #[test]
    fn open_still_refuses_unknown_fields_but_loads_a_record_without_key_changed() {
        let tmp = TempDir::new("trust-key-changed-schema");
        let path = mesh_config_dir(&tmp.path).join("trust.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let (identity, destination, seen) = (fake_hash(0x1a), fake_hash(0x2b), fake_hash(0x3c));
        let ts = "2026-01-01T00:00:00Z";
        let without_mark = format!(
            "version: {TRUST_FILE_VERSION}\nidentities:\n  {identity}:\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: null\n    note: null\n    all_destinations: false\ndestinations:\n  {destination}:\n    identity: {identity}\n    added_at: {ts}\n    last_seen_at: {ts}\n    label: null\n    note: null\n"
        );
        fs::write(&path, &without_mark).unwrap();
        let store = TrustStore::open(&tmp.path).unwrap();
        assert_eq!(record(&store, &destination).key_changed, None);

        let mark = |seen: &str| {
            format!("{without_mark}    key_changed:\n      seen_identity: {seen}\n      at: {ts}\n")
        };
        fs::write(&path, mark(&seen)).unwrap();
        let store = TrustStore::open(&tmp.path).unwrap();
        assert_eq!(
            record(&store, &destination).key_changed,
            Some(KeyChange {
                seen_identity: seen.clone(),
                at: parse_rfc3339(ts).unwrap(),
            })
        );

        fs::write(&path, mark(&seen.to_ascii_uppercase())).unwrap();
        let err = TrustStore::open(&tmp.path).unwrap_err().to_string();
        assert!(err.contains("key_changed.seen_identity field"), "{err}");
        assert!(err.contains(&seen.to_ascii_uppercase()), "{err}");

        fs::write(&path, format!("{}      rotated: true\n", mark(&seen))).unwrap();
        let err = format!("{:#}", TrustStore::open(&tmp.path).unwrap_err());
        assert!(err.contains("could not be parsed"), "{err}");
        assert!(err.contains("rotated"), "{err}");
    }

    #[test]
    fn explicit_re_trust_clears_the_key_change_mark() {
        let fx = Fixture::new("trust-key-change-retrust");
        let (old, new) = trusted_then_rotated(&fx);
        fx.store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));
        assert!(
            record(&fx.store, &old.destination_hash)
                .key_changed
                .is_some()
        );

        assert_eq!(fx.trust_destination(&old, t(7_000)), TrustChange::Updated);

        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, None);
        assert_eq!(
            record(&fx.reopen(), &old.destination_hash).key_changed,
            None
        );
        assert!(
            !String::from_utf8(fx.file_bytes().unwrap())
                .unwrap()
                .contains("key_changed")
        );
        assert_eq!(
            fx.store
                .note_key_change(&new.identity_hash, &new.name_hash, t(8_000))
                .len(),
            1,
            "once cleared, the next conflicting sighting marks again"
        );
    }

    #[test]
    fn blocking_the_seen_identity_clears_its_marks() {
        let fx = Fixture::new("trust-key-change-block-seen");
        let (old, new) = trusted_then_rotated(&fx);
        fx.store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));
        assert!(
            record(&fx.store, &old.destination_hash)
                .key_changed
                .is_some()
        );

        let removed = fx
            .store
            .block_identity(&fx.mesh, &new.identity_hash, None, t(6_000))
            .unwrap();

        assert!(removed.is_empty(), "the new identity held no records");
        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, None);
        assert_eq!(
            record(&fx.reopen(), &old.destination_hash).key_changed,
            None
        );
        assert_eq!(
            fx.store
                .authorize(&old.identity_hash, &old.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "the bound identity's grant is untouched"
        );
        let third = announced("alpha");
        assert_eq!(
            fx.store
                .note_key_change(&third.identity_hash, &third.name_hash, t(7_000))
                .len(),
            1,
            "a later sighting under yet another identity marks again"
        );
    }

    #[test]
    fn trusting_the_seen_identity_for_all_destinations_clears_its_marks() {
        let fx = Fixture::new("trust-key-change-trust-seen");
        let (old, new) = trusted_then_rotated(&fx);
        fx.store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));
        assert!(
            record(&fx.store, &old.destination_hash)
                .key_changed
                .is_some()
        );

        assert_eq!(
            fx.trust_identity(&new.identity_hash, t(6_000)),
            TrustChange::Added
        );

        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, None);
        assert_eq!(
            record(&fx.reopen(), &old.destination_hash).key_changed,
            None
        );
        assert!(
            !String::from_utf8(fx.file_bytes().unwrap())
                .unwrap()
                .contains("key_changed")
        );
        assert_eq!(
            fx.store
                .authorize(&old.identity_hash, &old.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "the bound identity's grant is untouched"
        );
        assert!(
            fx.store
                .note_key_change(&new.identity_hash, &new.name_hash, t(7_000))
                .is_empty(),
            "an identity trusted for all destinations marks nothing"
        );
    }

    #[test]
    fn a_blocked_identity_marks_nothing() {
        let fx = Fixture::new("trust-key-change-blocked-seen");
        let (old, new) = trusted_then_rotated(&fx);
        fx.store
            .block_identity(&fx.mesh, &new.identity_hash, None, t(4_000))
            .unwrap();
        let before = fx.file_bytes().unwrap();

        let marked = fx
            .store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));

        assert!(marked.is_empty(), "{marked:?}");
        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, None);
        assert_eq!(fx.file_bytes().unwrap(), before);
    }

    #[test]
    fn a_denied_record_marks_nothing() {
        let fx = Fixture::new("trust-key-change-denied-record");
        let (old, new) = trusted_then_rotated(&fx);
        fx.store
            .deny_destination(&fx.mesh, &old.destination_hash, None, t(4_000))
            .unwrap();
        let before = fx.file_bytes().unwrap();

        let marked = fx
            .store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));

        assert!(marked.is_empty(), "{marked:?}");
        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, None);
        assert_eq!(fx.file_bytes().unwrap(), before);
    }

    #[test]
    fn an_all_destinations_identity_marks_nothing() {
        let fx = Fixture::new("trust-key-change-all-destinations-seen");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        let (old, new) = trusted_then_rotated(&fx);
        assert_eq!(
            fx.trust_identity(&new.identity_hash, t(4_000)),
            TrustChange::Added
        );
        let before = fx.file_bytes().unwrap();

        let marked = fx
            .store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));

        assert!(marked.is_empty(), "{marked:?}");
        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, None);
        assert_eq!(fx.file_bytes().unwrap(), before);
        assert!(surface.texts().is_empty(), "{:#?}", surface.texts());
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
    }

    #[test]
    fn an_identity_tier_grants_rotation_is_fail_closed_but_unmarked() {
        let fx = Fixture::new("trust-key-change-identity-tier-grant");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        assert_eq!(
            fx.trust_identity(&old.identity_hash, t(3_000)),
            TrustChange::Added
        );
        assert_eq!(
            origin_verdict(&fx.store, &old),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
        let new = announced("alpha");
        assert_eq!(new.name_hash, old.name_hash);
        fx.announce(&new, t(4_000));
        let before = fx.file_bytes().unwrap();

        let marked = fx
            .store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));

        assert!(marked.is_empty(), "{marked:?}");
        assert_eq!(
            fx.store.identity_standing(&new.identity_hash),
            IdentityStanding::Unknown
        );
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Refuse, Rule::DefaultClosed),
            "with no destination record there is no binding to conflict with"
        );
        assert!(
            fx.store
                .records()
                .iter()
                .all(|record| record.key_changed.is_none()),
            "{:#?}",
            fx.store.records()
        );
        assert_eq!(fx.file_bytes().unwrap(), before);
        assert!(surface.texts().is_empty(), "{:#?}", surface.texts());
    }

    struct DroppingSurface;

    impl KnockSurface for DroppingSurface {
        fn surface(&self, _note: IdleNotify) -> bool {
            false
        }
    }

    #[test]
    fn a_dropped_key_change_line_is_warned_about_and_the_mark_stands() {
        install_log_collector();
        let fx = Fixture::new("trust-key-change-dropped-line");
        let surface = Arc::new(DroppingSurface);
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        let (old, new) = trusted_then_rotated(&fx);

        let marked = fx
            .store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));

        assert_eq!(marked.len(), 1);
        assert!(
            record(&fx.store, &old.destination_hash)
                .key_changed
                .is_some()
        );
        let line = format!(
            "Mesh key-change notice for {} was dropped; the mark stands and .mesh peers shows it",
            short(&old.destination_hash)
        );
        assert!(warn_snapshot().contains(&line), "{line}");
        assert!(
            fx.store
                .note_key_change(&new.identity_hash, &new.name_hash, t(6_000))
                .is_empty(),
            "a dropped line is not offered again"
        );
    }

    #[test]
    fn trusting_the_new_destination_clears_the_old_records_mark_and_names_it() {
        let fx = Fixture::new("trust-key-change-superseded");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        let (old, new) = trusted_then_rotated(&fx);
        fx.store
            .note_key_change(&new.identity_hash, &new.name_hash, t(5_000));
        assert_eq!(surface.texts().len(), 1);
        fx.announce(&new, t(6_000));

        let outcome = fx
            .store
            .trust_destination(
                &fx.mesh,
                &new.destination_hash,
                TrustOptions::default(),
                t(7_000),
            )
            .unwrap();

        assert_eq!(outcome.change, TrustChange::Added);
        assert_eq!(outcome.identity_hash, new.identity_hash);
        assert_eq!(outcome.superseded, vec![old.destination_hash.clone()]);
        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, None);
        assert_eq!(
            record(&fx.reopen(), &old.destination_hash).key_changed,
            None
        );
        assert_eq!(
            record(&fx.store, &new.destination_hash).identity.as_deref(),
            Some(new.identity_hash.as_str())
        );
        assert_eq!(
            fx.store
                .authorize(&old.identity_hash, &old.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "the old record stays until untrusted"
        );
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
        assert_eq!(
            origin_verdict(&fx.store, &old),
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "both keys are trusted for their own destinations; neither conflicts"
        );

        let after_trust = fx.file_bytes().unwrap();
        fx.announce(&new, t(8_000));
        let again = fx
            .store
            .note_key_change(&new.identity_hash, &new.name_hash, t(8_000));

        assert!(again.is_empty(), "{again:?}");
        assert_eq!(fx.file_bytes().unwrap(), after_trust);
        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, None);
        assert_eq!(
            surface.texts().len(),
            1,
            "the trusted identity's heartbeat marks the old record no more"
        );
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
    }
}
