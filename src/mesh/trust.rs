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
//! and stays trusted for `D`); the new identity stands on its own record, a stranger when
//! it has none; the human is told once per record while the mark stands; the first
//! conflicting identity is what the mark records, and later conflicts against an
//! already-marked record write nothing, so an attacker cannot drive disk churn; the wording
//! says "presented under another identity", never "rotated"; and nothing is ever re-bound
//! or re-trusted on its own. Every colliding record is marked, a denied one and one whose
//! seen identity is trusted for all destinations included; only a blocked identity seen, or
//! one that already holds its own record for the instance (both bindings then being the
//! human's), marks nothing. While the record stands, the mark is cleared only by the human
//! trusting the new destination, which is the one answer that confirms the new key; a
//! confirmed `.mesh trust --prune` removes a stale marked record with the rest, and the dry
//! run shows the mark.
//!
//! Whether the colliding identity is served depends on its own standing and on
//! `mesh.collision_protection`: off, an identity trusted for all destinations is served
//! and the human gets a warning; on, the collision rung is judged before identity allow
//! and it is refused with an error until the human trusts its new destination. A
//! destination allow admits in either mode, and a collision no allow admits is refused by
//! rule identity changed rather than default closed, so no knock invites the human to
//! trust the new key as a stranger. An identity trusted for all destinations carries no
//! per-instance record of its own, so a rotation of it is detected only against the peer
//! table: when a row holds the instance under such an identity the human is told once,
//! best effort, and nothing is marked. With protection off the presenting identity is
//! served and the line is a warning; on, the verdict itself reads the table, for an
//! identity-allow verdict alone, and the identity is refused by rule identity changed with
//! an error until the human trusts its new destination. Once that line has been earned
//! the refusal is remembered for the node's lifetime (bounded by the surfacing cap), so
//! the old key's row ageing out of the table does not lift it; blocking or untrusting the
//! old key does, as does trusting the new destination; re-trusting the old key for all
//! destinations re-arms the memory, which refuses only while the key it names holds that
//! grant. Every identity trusted for all destinations that the rung refuses is told once
//! per instance; strangers presenting the instance share one line per instance and key it
//! was heard under. While both rows are live the rung is symmetric: the old key
//! presenting the instance is refused too, earning no second line, and is admitted again
//! once the new key's row is gone or the new key is blocked.
//! The listings label a refused identity by its grant. The collision rung judges what
//! this node serves: the requests, knocks, stored messages and stored access requests it
//! receives. What it sends or broadcasts is gated by plain `authorize` and does not change
//! with the setting.
//!
//! The store-and-forward peer-MESSAGE path (`src/mesh/message.rs`) and ACCESS path
//! (`src/mesh/access.rs`) judge through `authorize_origin` and mark like the link, knock
//! and announce paths: a rotated peer's stored message or access request is served or
//! dropped by the same verdict and earns the same line.
//!
//! On the R3 and store-and-forward paths a freshly rotated peer has no standing and is
//! silenced before any verdict, so its name hash is never seen there.
//! `Rule::IdentityChanged` fires at the verdict stage only when a proven identity that has
//! standing (for another instance, say) names an instance whose destination is bound to a
//! different identity.

use crate::mesh::announce::is_control_or_invisible;
use crate::mesh::idle::{IdleNotify, Origin};
use crate::mesh::knock::KnockSurface;
use crate::mesh::node::MeshSlot;
use crate::mesh::notify::Source;
use crate::mesh::peers::{PeerRecord, PeerTable};
use crate::mesh::r3::NAME_HASH_LEN;
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::{
    canonical_hash, decode_hex, destination_address, hex_lower, mesh_config_dir, parse_rfc3339,
    redact_hashes, rfc3339_utc, short, write_atomically,
};

use anyhow::{Context, Result, anyhow, bail};
use arc_swap::ArcSwapOption;
use parking_lot::Mutex;
use rns_transport::hash::AddressHash;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
    BlockIdentity {
        identity: String,
        was_granting: bool,
        removed: Vec<String>,
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
    /// The destinations whose name hash was presented under `seen_identity`, an identity
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
                identity, removed, ..
            } => write!(
                f,
                "block identity {} (+{} destinations)",
                short(identity),
                removed.len()
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
                "key change marked on {} destinations presented under identity {}",
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
    /// The other records the trusted instance's name hash re-derives under their bound
    /// identities: the instance was trusted under another key before. Their key-change
    /// marks are cleared; the records themselves, a deny on them included, stay.
    pub superseded: Vec<BindingConflict>,
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
        /// No record and no session twin held the destination when the call was made,
        /// so the write pass back-fills one bound to the resolved identity.
        record_missing: bool,
    },
}

/// A trusted destination record whose name hash `binding_conflicts` was asked about
/// re-derives under its bound identity, which is not the identity asked about. `denied`
/// when a deny record stands on the destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BindingConflict {
    pub destination_hash: String,
    pub bound_identity: String,
    pub label: Option<String>,
    pub denied: bool,
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
    pub denied: bool,
}

/// How the identity that presented a collision was answered; picks the line the human
/// gets. `Served` is the warning, `Refused` the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeyChangeOutcome {
    Served,
    Refused,
}

/// `authorize_origin`'s answer: the verdict, the destination it judged, and the records
/// the origin's name hash collides with, so the caller marks only when there is something
/// to mark. `collisions` is empty whenever the verdict is a destination allow or a block:
/// the identity then holds the instance itself, or the human has already answered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OriginVerdict {
    pub verdict: Verdict,
    pub destination: AddressHash,
    pub collisions: Vec<BindingConflict>,
}

/// The peer table as the trust list sees it: a presence cache that may hold an instance
/// under an identity other than the one now presenting it. An identity trusted for all
/// destinations has no per-instance record, so this is the only place its rotation shows.
pub(crate) trait InstancePresence: Send + Sync {
    /// Every cached row announcing the instance `name_hash` (lower hex) under an identity
    /// other than `identity_hash`, as `(destination_hash, identity_hash)`, most recently
    /// seen first; a row expired at `now` is not a presence.
    fn heard_under_other_identities(
        &self,
        name_hash: &str,
        identity_hash: &str,
        now: SystemTime,
    ) -> Vec<(String, String)>;
}

/// Pairs of (name hash, identity the peer row holds it under) whose presence-cache
/// collision has been surfaced; the oldest pair is forgotten when the set outgrows this,
/// so a long run stays bounded. Forgetting a pair also lifts the refusal it remembers.
pub(crate) const PRESENCE_SURFACED_CAP: usize = 4096;

/// A presence collision whose owner line is due: the row's destination and the identity
/// it holds the instance under, and which dedupe entries this detection recorded, so a
/// dropped line rolls back exactly what it armed.
struct PresenceLine {
    old_destination: String,
    old_identity: String,
    remembered: bool,
    told: bool,
}

/// What became of a key-change line offered to the surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Offered {
    Shown,
    Dropped,
    NoSurface,
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
    /// The origin name hash re-derives a trusted destination under a different identity,
    /// or, under collision protection, is held by the peer table (or the presence rung's
    /// memory) under another identity trusted for all destinations; always a refusal.
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
    /// `(name hash, identity the row held it under)` for every presence collision whose
    /// line was earned, with insertion order for eviction and a per-instance index of the
    /// holders for the verdict's lookup; per instance, the identities trusted for all
    /// destinations that have been told of their own refusal, dropped with the instance's
    /// last holder. `remember_presence` and `forget_presence` keep the four in step.
    presence_surfaced: HashSet<(String, String)>,
    presence_surfaced_order: VecDeque<(String, String)>,
    presence_remembered: HashMap<String, BTreeSet<String>>,
    presence_lines_told: HashMap<String, BTreeSet<String>>,
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
    /// The peer table, held weakly for the same reason.
    presence: Mutex<Option<Weak<dyn InstancePresence>>>,
    collision_protection: AtomicBool,
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
            presence: Mutex::new(None),
            collision_protection: AtomicBool::new(false),
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

    /// Installs the presence cache `note_key_change` consults when no record collides, so
    /// the rotation of an identity trusted for all destinations is surfaced best effort.
    pub(crate) fn attach_presence(&self, presence: Weak<dyn InstancePresence>) {
        *self.presence.lock() = Some(presence);
    }

    /// `mesh.collision_protection`: on, the collision rung of `authorize_origin` precedes
    /// identity allow.
    pub(crate) fn set_collision_protection(&self, on: bool) {
        self.collision_protection.store(on, Ordering::Relaxed);
    }

    fn collision_protection(&self) -> bool {
        self.collision_protection.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The one place the precedence lives: destination deny, then destination allow, then
    /// identity allow, then default-closed. A blocked identity is refused before either
    /// allow and reported under its own rule. A destination record only allows the identity
    /// it was proven to belong to. `authorize_origin` layers the one rule that needs the
    /// name hash, identity changed, between identity allow and default-closed, or before
    /// identity allow under collision protection.
    ///
    /// The map lookups are keyed on values the peer already knows and are not treated as a
    /// timing boundary. The destination binding is the one direct equality against a
    /// caller-supplied identity, and it runs in constant time (`same_hash`). Callers
    /// consume the verdict; none of them compares identities again.
    pub(crate) fn authorize(&self, identity_hash: &str, destination_hash: &str) -> Verdict {
        self.inner.lock().authorize(
            &identity_hash.to_ascii_lowercase(),
            &destination_hash.to_ascii_lowercase(),
        )
    }

    /// `authorize` for a request whose origin is known by name hash: the destination judged
    /// is the one `name_hash` and `identity` derive, and the records that name hash
    /// re-derives under another identity come back with the verdict. A default-closed
    /// verdict becomes `IdentityChanged` when there are any; so does an identity-allow
    /// verdict under collision protection; a destination deny stands, its collisions
    /// reported for marking. A destination allow is never asked, the identity then holding
    /// the instance itself, so its collisions are empty without a scan, and a blocked
    /// identity's are empty too. The conflicting records' grants stay with the identities
    /// they are bound to. An identity-allow verdict under collision protection that no
    /// record collides with is judged once more against the peer table: the instance heard
    /// there under another identity trusted for all destinations is a rotation with no
    /// record to mark, and it is refused by rule identity changed with empty collisions.
    /// That refusal is remembered: once the owner line for the pair (instance, holder) has
    /// been earned, the presenter stays refused while the holder is trusted for all
    /// destinations, whether or not the holder's row is still in the table, until the
    /// memory is evicted by `PRESENCE_SURFACED_CAP` or the node restarts. The memory is
    /// consulted before the table and never refuses the holder it names. No other rule and
    /// no unprotected store reads either here. `now` decides which peer table rows have
    /// expired; every ingress hands the same instant to `note_key_change`, so the rows the
    /// verdict sees are the rows the owner line sees.
    pub(crate) fn authorize_origin_at(
        &self,
        identity: &AddressHash,
        name_hash: &[u8; NAME_HASH_LEN],
        now: SystemTime,
    ) -> OriginVerdict {
        let destination = destination_address(name_hash, identity);
        let identity_hex = identity.to_hex_string();
        let state = self.inner.lock();
        let mut verdict = state.authorize(&identity_hex, &destination.to_hex_string());
        let collisions = match verdict.rule {
            Rule::IdentityTrusted | Rule::DefaultClosed | Rule::DestinationDenied => {
                state.binding_conflicts(&identity_hex, name_hash)
            }
            _ => Vec::new(),
        };
        let presence_refuses = || {
            verdict.rule == Rule::IdentityTrusted
                && self.collision_protection()
                && collisions.is_empty()
                && {
                    let name_hash_hex = hex_lower(name_hash);
                    state.remembered_under_trusted_for_all(&name_hash_hex, &identity_hex)
                        || self
                            .heard_under_trusted_for_all(
                                &state,
                                &name_hash_hex,
                                name_hash,
                                &identity_hex,
                                now,
                            )
                            .is_some()
                }
        };
        if self.collision_refuses(verdict.rule, &collisions) || presence_refuses() {
            verdict = Verdict {
                decision: Decision::Refuse,
                rule: Rule::IdentityChanged,
            };
        }
        OriginVerdict {
            verdict,
            destination,
            collisions,
        }
    }

    /// `authorize_origin_at` judged now.
    #[cfg(test)]
    pub(crate) fn authorize_origin(
        &self,
        identity: &AddressHash,
        name_hash: &[u8; NAME_HASH_LEN],
    ) -> OriginVerdict {
        self.authorize_origin_at(identity, name_hash, SystemTime::now())
    }

    /// The verdict half of a collision: whether `collisions`, found for a verdict under
    /// `rule`, refuse the identity. An identity with no grant of its own is refused by them
    /// outright; one trusted for all destinations only under collision protection.
    fn collision_refuses(&self, rule: Rule, collisions: &[BindingConflict]) -> bool {
        !collisions.is_empty()
            && match rule {
                Rule::DefaultClosed => true,
                Rule::IdentityTrusted => self.collision_protection(),
                _ => false,
            }
    }

    /// The detection half: the trusted destination records, on disk then session, that
    /// `name_hash` re-derives under their bound identity when that identity is not
    /// `identity_hash`. The instance they belong to has been seen under another key.
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

    /// Marks every destination record whose instance `name_hash` names when it is seen
    /// under `identity_hash` rather than the identity the record is bound to, a denied
    /// record and one whose seen identity is trusted for all destinations included, and
    /// surfaces one line per record newly marked, a warning or an error by `outcome`. A
    /// record already marked keeps its first sighting and is neither written nor reported
    /// again, so a repeated or forged announce costs nothing; session records are marked in
    /// memory only. The grant is untouched: the bound identity still proves its own key.
    /// An unreadable hash marks nothing, and neither does a blocked `identity_hash` (the
    /// human has already answered it) or one that already holds the instance
    /// (`State::binding_conflicts`). When no record collides the peer table is asked
    /// instead: an identity trusted for all destinations has no record to mark, so a row
    /// holding the instance under such an identity is surfaced once per (instance, identity
    /// the row holds it under) while the node runs, whatever identity presents it, and once
    /// more to each identity trusted for all destinations that presents it and is refused;
    /// nothing is written. A line no surface shows, dropped or with nothing attached, arms
    /// nothing: the next presentation offers it again.
    pub(crate) fn note_key_change(
        &self,
        identity_hash: &str,
        name_hash: &str,
        outcome: KeyChangeOutcome,
        now: SystemTime,
    ) -> Vec<MarkedKeyChange> {
        let (Some(seen), Some(name_hash_bytes)) =
            (parse_hash(identity_hash), decode_name_hash(name_hash))
        else {
            return Vec::new();
        };
        let name_hash = hex_lower(&name_hash_bytes);
        let seen_identity = seen.to_hex_string();
        let new_destination = destination_address(&name_hash_bytes, &seen).to_hex_string();
        let marked = {
            let mut state = self.inner.lock();
            if state.file.blocked_identities.contains_key(&seen_identity) {
                return Vec::new();
            }
            let conflicts = state.binding_conflicts(&seen_identity, &name_hash_bytes);
            if conflicts.is_empty() {
                let heard = self.presence_collision(
                    &mut state,
                    &name_hash,
                    &name_hash_bytes,
                    &seen_identity,
                    now,
                );
                drop(state);
                if let Some(line) = heard {
                    let offered = self.offer_line(
                        &seen_identity,
                        &line.old_destination,
                        presence_collision_text(
                            &line.old_destination,
                            &line.old_identity,
                            &seen_identity,
                            &new_destination,
                            outcome,
                        ),
                    );
                    if offered != Offered::Shown {
                        self.inner
                            .lock()
                            .forget_presence(&name_hash, &seen_identity, &line);
                    }
                    if offered == Offered::Dropped {
                        warn!(
                            "Mesh key-change notice for {} was dropped; nothing is marked and it is offered again when the instance is next presented",
                            short(&line.old_destination)
                        );
                    }
                }
                return Vec::new();
            }
            let fresh: Vec<BindingConflict> = conflicts
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
                    denied: conflict.denied,
                })
                .collect::<Vec<_>>()
        };
        self.surface_key_changes(&marked, &new_destination, outcome);
        marked
    }

    /// The presence-cache half of detection, for an identity no record collides with: the
    /// peer table row holding `name_hash` under an identity trusted for all destinations
    /// (the freshest such row when several), other than `seen_identity`, unless
    /// `seen_identity` holds the instance itself, as `(destination, identity)`. Reads
    /// nothing but the table and the store, and the table not at all while no identity is
    /// trusted for all destinations.
    fn heard_under_trusted_for_all(
        &self,
        state: &State,
        name_hash: &str,
        name_hash_bytes: &[u8; NAME_HASH_LEN],
        seen_identity: &str,
        now: SystemTime,
    ) -> Option<(String, String)> {
        if !state
            .file
            .identities
            .values()
            .any(|entry| entry.all_destinations)
        {
            return None;
        }
        let presence = self.presence.lock().as_ref().and_then(Weak::upgrade)?;
        if state.holds_instance(seen_identity, name_hash_bytes) {
            return None;
        }
        presence
            .heard_under_other_identities(name_hash, seen_identity, now)
            .into_iter()
            .find(|(_, identity)| state.trusted_for_all(identity))
    }

    /// The memory's half, for when the holder's row is gone: the remembered holder of
    /// `name_hash` whose memory still refuses `seen_identity`, with the destination its
    /// row held, derived again since no row remains to read it from. Same shape and same
    /// exemption for a presenter holding the instance itself as the table's half.
    fn remembered_refusal(
        state: &State,
        name_hash: &str,
        name_hash_bytes: &[u8; NAME_HASH_LEN],
        seen_identity: &str,
    ) -> Option<(String, String)> {
        if state.holds_instance(seen_identity, name_hash_bytes) {
            return None;
        }
        let holder = state.remembered_holder_refusing(name_hash, seen_identity)?;
        let destination = destination_address(name_hash_bytes, &parse_hash(holder)?);
        Some((destination.to_hex_string(), holder.to_string()))
    }

    /// `heard_under_trusted_for_all` as the line it earns, deduped while the node runs. A
    /// presenter that is itself a holder the memory names for the instance earns none, so
    /// the old key heard again after a rotation line does not restate it with the roles
    /// swapped. Any other presenter trusted for all destinations earns its own line once
    /// per instance, since it is the one refused and the line names its remedies, and
    /// earns it from the memory alone once the holder's row is gone, since the memory
    /// refuses it just the same; every other presenter shares one line per (instance,
    /// identity the row holds it under), since only the holder's own signed announces make
    /// such rows and any number of strangers may present the instance. The pair is
    /// remembered whichever way the line is deduped, so the refusal is durable. Nothing on
    /// disk changes.
    fn presence_collision(
        &self,
        state: &mut State,
        name_hash: &str,
        name_hash_bytes: &[u8; NAME_HASH_LEN],
        seen_identity: &str,
        now: SystemTime,
    ) -> Option<PresenceLine> {
        if state
            .presence_surfaced
            .contains(&(name_hash.to_string(), seen_identity.to_string()))
        {
            return None;
        }
        let own_line = self.collision_protection() && state.trusted_for_all(seen_identity);
        let heard =
            self.heard_under_trusted_for_all(state, name_hash, name_hash_bytes, seen_identity, now);
        let (old_destination, old_identity) = match heard {
            Some(heard) => heard,
            None if own_line => {
                Self::remembered_refusal(state, name_hash, name_hash_bytes, seen_identity)?
            }
            None => return None,
        };
        let remembered = state.remember_presence((name_hash.to_string(), old_identity.clone()));
        let told = own_line && state.tell_presence_line(name_hash, seen_identity);
        let due = if own_line { told } else { remembered };
        if !due {
            return None;
        }
        debug!(
            "Mesh instance {} heard under identity {} is presented by identity {}; no record carries it",
            short(&old_destination),
            short(&old_identity),
            short(seen_identity)
        );
        Some(PresenceLine {
            old_destination,
            old_identity,
            remembered,
            told,
        })
    }

    /// One line per newly marked record, offered after the store lock is released. Nothing
    /// in the line comes from the peer: the label is the human's own, admitted by
    /// `check_text`, and the rest is hashes. A dropped line is not offered again; the mark
    /// stands and `.mesh peers` shows it.
    fn surface_key_changes(
        &self,
        marked: &[MarkedKeyChange],
        new_destination: &str,
        outcome: KeyChangeOutcome,
    ) {
        for change in marked {
            let offered = self.offer_line(
                &change.seen_identity,
                &change.destination_hash,
                key_change_text(change, new_destination, outcome),
            );
            if offered == Offered::Dropped {
                warn!(
                    "Mesh key-change notice for {} was dropped; the mark stands and .mesh peers shows it",
                    short(&change.destination_hash)
                );
            }
        }
    }

    /// Offers one key-change line about `destination_hash`, seen under `seen_identity`, to
    /// whatever is attached.
    fn offer_line(&self, seen_identity: &str, destination_hash: &str, text: String) -> Offered {
        let Some(surface) = self.surface.lock().as_ref().and_then(Weak::upgrade) else {
            debug!(
                "Mesh key change on destination {} was not surfaced: nothing is attached",
                short(destination_hash)
            );
            return Offered::NoSurface;
        };
        let shown = surface.surface(IdleNotify {
            source: Source::Mesh,
            origin: Origin::Peer(short(seen_identity).to_string()),
            text,
            model_note: None,
        });
        if shown {
            Offered::Shown
        } else {
            Offered::Dropped
        }
    }

    /// Trusts one destination by proving which identity announced it, and makes sure that
    /// identity has a record (without `all_destinations`, which only `trust_identity` sets).
    /// The identity comes from the announce's or the knock's own hashes, never from the
    /// caller. Trusting the new destination of an instance whose old records carry a
    /// key-change mark clears those marks: the human has confirmed the new key. Re-trusting
    /// a marked destination itself leaves its mark, since that answers nothing about the
    /// key it was seen under.
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
        let superseded = decode_name_hash(&peer.name_hash)
            .map(|name_hash| state.binding_conflicts(&peer.identity_hash, &name_hash))
            .unwrap_or_default();
        for conflict in &superseded {
            if let Some(entry) = file.destinations.get_mut(&conflict.destination_hash) {
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
        for conflict in &superseded {
            if let Some(entry) = state
                .session_destinations
                .get_mut(&conflict.destination_hash)
            {
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
    /// identity column alone is a claim. Key-change marks naming this identity as the one
    /// seen stand: only trusting the new destination clears one. The grant carries no
    /// per-instance record, so a rotation of this identity is surfaced from the peer table
    /// when heard and never marked.
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
    /// forgetting the identity forgets the refusal too. A knocker that never announced has
    /// no row to resolve through and cannot be refused until it is heard. `dry_run` reports
    /// the outcome and writes nothing. Forgetting a record under a plain identity drops any
    /// deny left on it, so no orphan deny survives the record.
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
            return Err(nothing_to_untrust(&file, &destination));
        };
        let trusted_all = file
            .identities
            .get(&identity)
            .is_some_and(|entry| entry.all_destinations);
        if trusted_all {
            let already_refused = file.denied_destinations.contains_key(&destination);
            let has_record = file.destinations.contains_key(&destination)
                || state.session_destinations.contains_key(&destination);
            let record_missing = !has_record;
            if dry_run || (already_refused && !record_missing) {
                return Ok(UntrustOutcome::Refused {
                    identity,
                    already_refused,
                    record_missing,
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
                record_missing,
            });
        }
        let on_disk = file.destinations.remove(&destination);
        let in_session = state.session_destinations.contains_key(&destination);
        if on_disk.is_none() && !in_session {
            return Err(nothing_to_untrust(&file, &destination));
        }
        if dry_run {
            return Ok(UntrustOutcome::Forgotten { identity });
        }
        let deny_dropped = file.denied_destinations.remove(&destination).is_some();
        if on_disk.is_some() || deny_dropped {
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
    /// works on an identity the trust list has never seen. The block silences the identity;
    /// a key-change mark naming it as the one seen stands until the new destination is
    /// trusted.
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
        upsert_overlay(&mut file.blocked_identities, identity.clone(), note, now);
        self.commit(
            &mut state,
            file,
            TrustMutation::BlockIdentity {
                identity: identity.clone(),
                was_granting,
                removed: removed.clone(),
            },
        )?;
        state.forget_identity(&identity, &removed);
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
    /// verified to be the record's; removed unless `dry_run`. A refused record is the
    /// operator's answer about that instance and is never pruned. Identities are never
    /// pruned: they are the user's statement about a person, not about an instance that
    /// may be gone.
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
            .filter(|(hash, _)| !state.file.denied_destinations.contains_key(*hash))
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

    /// `identity` and `destination` are canonical lower hex; see `TrustStore::authorize`.
    fn authorize(&self, identity: &str, destination: &str) -> Verdict {
        if self.file.denied_destinations.contains_key(destination) {
            return Verdict {
                decision: Decision::Refuse,
                rule: Rule::DestinationDenied,
            };
        }
        if self.file.blocked_identities.contains_key(identity) {
            return Verdict {
                decision: Decision::Refuse,
                rule: Rule::IdentityBlocked,
            };
        }
        let bound_to_identity = self
            .file
            .destinations
            .get(destination)
            .or_else(|| self.session_destinations.get(destination))
            .is_some_and(|entry| same_hash(&entry.identity, identity));
        if bound_to_identity {
            return Verdict {
                decision: Decision::Allow,
                rule: Rule::DestinationTrusted,
            };
        }
        if self.trusted_for_all(identity) {
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

    fn trusted_for_all(&self, identity: &str) -> bool {
        self.file
            .identities
            .get(identity)
            .is_some_and(|entry| entry.all_destinations)
    }

    /// Whether a remembered presence collision for `name_hash` names a holder other than
    /// `identity` that is still trusted for all destinations. The holder itself is never
    /// refused by its own memory.
    fn remembered_under_trusted_for_all(&self, name_hash: &str, identity: &str) -> bool {
        self.remembered_holder_refusing(name_hash, identity)
            .is_some()
    }

    /// The remembered holder behind `remembered_under_trusted_for_all`, the first in
    /// holder order when several.
    fn remembered_holder_refusing(&self, name_hash: &str, identity: &str) -> Option<&str> {
        self.presence_remembered
            .get(name_hash)?
            .iter()
            .find(|holder| *holder != identity && self.trusted_for_all(holder))
            .map(String::as_str)
    }

    /// Records `(name hash, holder)` as surfaced, evicting the oldest pair past
    /// `PRESENCE_SURFACED_CAP`; false when the pair was already remembered. An instance
    /// whose last holder is evicted forgets who was told of it too, so a presenter's line
    /// can be earned again once the memory that refused it is gone.
    fn remember_presence(&mut self, key: (String, String)) -> bool {
        if !self.presence_surfaced.insert(key.clone()) {
            return false;
        }
        self.presence_remembered
            .entry(key.0.clone())
            .or_default()
            .insert(key.1.clone());
        self.presence_surfaced_order.push_back(key);
        if self.presence_surfaced_order.len() > PRESENCE_SURFACED_CAP
            && let Some((name_hash, holder)) = self.presence_surfaced_order.pop_front()
        {
            self.presence_surfaced
                .remove(&(name_hash.clone(), holder.clone()));
            self.drop_remembered_holder(&name_hash, &holder);
        }
        true
    }

    /// Records that `presenter`, trusted for all destinations, was told it is refused the
    /// instance; false when it had been told already.
    fn tell_presence_line(&mut self, name_hash: &str, presenter: &str) -> bool {
        self.presence_lines_told
            .entry(name_hash.to_string())
            .or_default()
            .insert(presenter.to_string())
    }

    /// Rolls back what one detection recorded when its line was dropped: the pair, if
    /// this detection remembered it, and the presenter's line, if this detection told it.
    /// What an earlier, surfaced line armed stands.
    fn forget_presence(&mut self, name_hash: &str, presenter: &str, line: &PresenceLine) {
        if line.remembered {
            let key = (name_hash.to_string(), line.old_identity.clone());
            self.presence_surfaced.remove(&key);
            self.presence_surfaced_order.retain(|pair| pair != &key);
            self.drop_remembered_holder(name_hash, &line.old_identity);
        }
        if line.told
            && let Some(told) = self.presence_lines_told.get_mut(name_hash)
        {
            told.remove(presenter);
            if told.is_empty() {
                self.presence_lines_told.remove(name_hash);
            }
        }
    }

    fn drop_remembered_holder(&mut self, name_hash: &str, holder: &str) {
        let Some(holders) = self.presence_remembered.get_mut(name_hash) else {
            return;
        };
        holders.remove(holder);
        if holders.is_empty() {
            self.presence_remembered.remove(name_hash);
            self.presence_lines_told.remove(name_hash);
        }
    }

    /// `identity` is canonical lower hex. An entry whose bound identity does not parse is
    /// skipped: `open` refuses such a file, so none exists, but nothing here panics. Nothing
    /// conflicts once `identity` holds its own trusted record for the instance, both
    /// bindings then being the human's. A denied record conflicts like any other, and so
    /// does one whose `identity` is trusted for all destinations: the verdict decides what
    /// a conflict means, detection only reports it.
    fn binding_conflicts(
        &self,
        identity: &str,
        name_hash: &[u8; NAME_HASH_LEN],
    ) -> Vec<BindingConflict> {
        if self.holds_instance(identity, name_hash) {
            return Vec::new();
        }
        self.file
            .destinations
            .iter()
            .chain(&self.session_destinations)
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
                denied: self.file.denied_destinations.contains_key(hash),
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

fn nothing_to_untrust(file: &TrustFile, destination: &str) -> anyhow::Error {
    if file.denied_destinations.contains_key(destination) {
        return anyhow!(
            "Destination {destination} is refused and its identity is not trusted; `.mesh trust {destination}` lifts that once the peer is heard."
        );
    }
    anyhow!("Destination {destination} is not in the trust list, so there is nothing to untrust.")
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

/// The one line a key change earns, opening with a literal `warning: ` or `error: ` so
/// the severity reads without colour. It says "presented under", not "rotated": anyone
/// who has heard the instance id can present it under their own key, so the human
/// confirms with the identity's holder out of band before trusting the new destination,
/// which clears the mark, or blocks the new identity, which silences it. The same line
/// serves a mark raised by an announce and one raised by a link, knock or stored request,
/// so it names no path and speaks of what the identity gets when it asks, not of what it
/// got. Full hashes, because the human pastes them.
fn key_change_text(
    change: &MarkedKeyChange,
    new_destination: &str,
    outcome: KeyChangeOutcome,
) -> String {
    let instance = change
        .label
        .clone()
        .unwrap_or_else(|| short(&change.destination_hash).to_string());
    let bound = &change.bound_identity;
    let seen = &change.seen_identity;
    let standing = if change.denied {
        "the old key stays refused"
    } else {
        "its grant stays with the old key"
    };
    match outcome {
        KeyChangeOutcome::Served => format!(
            "warning: instance {instance} is bound to identity {bound} but was presented under \
             identity {seen}, which is trusted for all destinations and is served while it \
             asks; the record is marked and {standing}. If the peer rotated, confirm with its \
             holder out of band and run .mesh trust {new_destination} to clear the mark; \
             otherwise .mesh block {seen}"
        ),
        KeyChangeOutcome::Refused => format!(
            "error: instance {instance} is bound to identity {bound} but was presented under \
             identity {seen} and is refused when it asks; {standing} and the new key is a \
             stranger. Confirm with the identity's holder out of band before granting anything: \
             if the peer rotated, run .mesh trust {new_destination} to trust the new key and \
             clear the mark; otherwise .mesh block {seen} silences it"
        ),
    }
}

/// The line a collision found only in the peer table earns: the instance was heard under
/// `old_identity`, trusted for all destinations, and is now presented by `seen_identity`.
/// No record carries the instance, so nothing is marked and the line says so. It is served
/// only while `mesh.collision_protection` is off, and the warning names the setting.
fn presence_collision_text(
    old_destination: &str,
    old_identity: &str,
    seen_identity: &str,
    new_destination: &str,
    outcome: KeyChangeOutcome,
) -> String {
    let instance = short(old_destination);
    match outcome {
        KeyChangeOutcome::Served => format!(
            "warning: instance {instance} was heard under identity {old_identity}, trusted for all \
             destinations, and is now presented under identity {seen_identity}, which is also \
             trusted for all destinations and is served while it asks because \
             collision_protection is off; no record carries the instance, so nothing is marked. \
             If the peer rotated, confirm with its holder out of band; otherwise \
             .mesh block {seen_identity}"
        ),
        KeyChangeOutcome::Refused => format!(
            "error: instance {instance} was heard under identity {old_identity}, trusted for all \
             destinations, and is now presented under identity {seen_identity}, which is \
             refused when it asks; no record carries the instance, so nothing is marked. Confirm \
             with the identity's holder out of band before granting anything: if the peer rotated, run \
             .mesh trust {new_destination}; otherwise .mesh block {seen_identity}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{TempDir, siblings_of};
    use super::*;
    use crate::hooks::HookEvent;
    use crate::mesh::events::{
        MeshHooks, RecordingHookSink, TrustHookObserver, env_value, one_fire,
    };
    use crate::mesh::knock::RecordingSurface;
    use crate::mesh::peers::{PEER_TTL, PeerSighting};
    use crate::mesh::session_destination_name;
    use crate::testing::{install_log_collector, warn_snapshot};

    use rand_core::OsRng;
    use rns_transport::destination::SingleInputDestination;
    use rns_transport::identity::PrivateIdentity;
    use std::sync::atomic::AtomicUsize;

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
            .verdict
    }

    /// The outcome the announce path hands `note_key_change`: `peer`'s identity judged as
    /// its request would be.
    fn announce_outcome(store: &TrustStore, peer: &Announced) -> KeyChangeOutcome {
        match origin_verdict(store, peer).decision {
            Decision::Allow => KeyChangeOutcome::Served,
            Decision::Refuse => KeyChangeOutcome::Refused,
        }
    }

    fn record(store: &TrustStore, hash: &str) -> TrustRecord {
        store
            .records()
            .into_iter()
            .find(|record| record.hash == hash)
            .unwrap_or_else(|| panic!("no record for {hash}"))
    }

    fn superseded_destinations(outcome: &TrustOutcome) -> Vec<String> {
        outcome
            .superseded
            .iter()
            .map(|conflict| conflict.destination_hash.clone())
            .collect()
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
            TrustMutation::RefuseDestination {
                destination: destination.clone(),
            },
            TrustMutation::BlockIdentity {
                identity: identity.clone(),
                was_granting: false,
                removed: vec![destination.clone()],
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
        assert_eq!(
            siblings_of(fx.store.path()),
            ["trust.yaml"],
            "the atomic write leaves no temp file behind"
        );
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

    /// Usage probe: a `trust.yaml` the pre-SCOPE build wrote (a well-formed
    /// version-1 file whose destination hashes derive from the old application name) is
    /// refused with a clear message, never opened as an empty trust list.
    #[test]
    fn usage_probe_open_refuses_a_well_formed_version_1_trust_file_written_before_scope() {
        assert_eq!(
            TRUST_FILE_VERSION, 2,
            "the SCOPE wire rename bumps the trust file 1 -> 2"
        );
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
            record_missing: true,
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
                record_missing: false,
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
                record_missing: false,
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
                record_missing: true,
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
                record_missing: false,
            }
        );
        assert_eq!(fx.file_bytes(), before);
        assert_eq!(fx.store.denied().len(), 1);
    }

    #[test]
    fn a_repeat_refusal_backfills_the_record_of_a_legacy_deny_only_row() {
        let fx = Fixture::new("trust-untrust-refuse-backfill");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_identity(&peer.identity_hash, t(3_000));
        fx.store
            .deny_destination(&fx.mesh, &peer.destination_hash, None, t(3_500))
            .unwrap();
        assert_eq!(
            record(&fx.store, &peer.destination_hash).identity,
            None,
            "a deny-only row is listed unbound"
        );
        let sink = fx.observed();

        let outcome = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(4_000), false)
            .unwrap();

        assert_eq!(
            outcome,
            UntrustOutcome::Refused {
                identity: peer.identity_hash.clone(),
                already_refused: true,
                record_missing: true,
            }
        );
        let backfilled = record(&fx.store, &peer.destination_hash);
        assert!(backfilled.denied);
        assert_eq!(
            backfilled.identity.as_deref(),
            Some(peer.identity_hash.as_str())
        );
        assert_eq!(backfilled.added_at, t(4_000));
        assert_eq!(backfilled.last_seen_at, t(2_000));
        assert_eq!(fx.store.denied().len(), 1);
        assert_eq!(fx.store.denied()[0].added_at, t(3_500));
        assert!(sink.drain().is_empty(), "a back-fill fires nothing");
        assert_eq!(fx.reopen().records(), fx.store.records());
    }

    #[test]
    fn untrust_of_a_deny_only_row_names_the_refusal_not_a_missing_record() {
        let fx = Fixture::new("trust-untrust-deny-only");
        let unheard = fake_hash(0xcc);
        fx.store
            .deny_destination(&fx.mesh, &unheard, None, t(2_000))
            .unwrap();
        let heard = announced("alpha");
        fx.announce(&heard, t(2_000));
        fx.store
            .deny_destination(&fx.mesh, &heard.destination_hash, None, t(2_000))
            .unwrap();
        let before = fx.file_bytes();

        for destination in [&unheard, &heard.destination_hash] {
            let err = fx
                .store
                .untrust_destination(&fx.mesh, destination, t(3_000), false)
                .unwrap_err()
                .to_string();
            assert_eq!(
                err,
                format!(
                    "Destination {destination} is refused and its identity is not trusted; `.mesh trust {destination}` lifts that once the peer is heard."
                )
            );
        }
        assert_eq!(fx.file_bytes(), before);
        assert_eq!(fx.store.denied().len(), 2);
    }

    #[test]
    fn untrust_of_a_denied_record_under_a_plain_identity_forgets_the_deny_too() {
        let fx = Fixture::new("trust-untrust-denied-record-plain-identity");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_destination(&peer, t(2_000));
        fx.store
            .deny_destination(&fx.mesh, &peer.destination_hash, None, t(2_500))
            .unwrap();
        assert!(record(&fx.store, &peer.destination_hash).denied);
        let sink = fx.observed();

        let outcome = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(3_000), false)
            .unwrap();

        assert_eq!(
            outcome,
            UntrustOutcome::Forgotten {
                identity: peer.identity_hash.clone()
            }
        );
        assert!(
            fx.store
                .records()
                .iter()
                .all(|record| record.hash != peer.destination_hash),
            "no orphan deny row survives the record"
        );
        assert!(fx.store.denied().is_empty());
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        let envs = one_fire(&sink, HookEvent::MeshTrustRevoked);
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_TRUST_TIER"),
            Some("destination")
        );
        assert_eq!(
            env_value(&envs, "COYOTE_MESH_PEER_DESTINATION"),
            Some(peer.destination_hash.as_str())
        );
    }

    #[test]
    fn untrust_dry_run_of_a_denied_record_under_a_plain_identity_writes_nothing() {
        let fx = Fixture::new("trust-untrust-denied-record-plain-identity-dry-run");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_destination(&peer, t(2_000));
        fx.store
            .deny_destination(&fx.mesh, &peer.destination_hash, None, t(2_500))
            .unwrap();
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
        assert!(record(&fx.store, &peer.destination_hash).denied);
        assert_eq!(fx.store.denied().len(), 1);
        assert!(sink.drain().is_empty());
    }

    #[test]
    fn prune_never_prunes_a_refused_instance() {
        let fx = Fixture::new("trust-prune-refused");
        let refused = announced("alpha");
        let sibling = announced("beta");
        fx.announce(&refused, t(1_000));
        fx.announce(&sibling, t(1_000));
        fx.trust_identity(&refused.identity_hash, t(1_000));
        fx.trust_destination(&sibling, t(1_000));
        fx.store
            .untrust_destination(&fx.mesh, &refused.destination_hash, t(1_500), false)
            .unwrap();
        assert!(record(&fx.store, &refused.destination_hash).denied);
        let sink = fx.observed();
        let now = t(1_000 + 3_600);
        let horizon = Duration::from_secs(3_600);

        let dry = fx
            .store
            .prune_destinations(&fx.mesh, horizon, now, true)
            .unwrap();
        assert_eq!(dry, vec![sibling.destination_hash.clone()]);

        let pruned = fx
            .store
            .prune_destinations(&fx.mesh, horizon, now, false)
            .unwrap();
        assert_eq!(pruned, vec![sibling.destination_hash.clone()]);
        assert!(record(&fx.store, &refused.destination_hash).denied);
        assert_eq!(fx.store.denied().len(), 1);
        assert!(
            !fx.store
                .records()
                .iter()
                .any(|record| record.hash == sibling.destination_hash)
        );
        let fired = sink.drain();
        assert_eq!(fired.len(), 1, "{fired:?}");
        assert_eq!(
            env_value(&fired[0].1, "COYOTE_MESH_PEER_DESTINATION"),
            Some(sibling.destination_hash.as_str())
        );
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

    /// Usage probe: the record a refusal writes for a peer-table-only instance is a
    /// real binding. A second refusal is a no-op that rewrites nothing; `trust <dest>`
    /// then lifts the deny on that intact record (`Updated`, `deny_lifted`), keeps the
    /// identity it was bound to, takes the label the lift carries, fires no trust hook
    /// (nothing new is granted), and the lifted record survives a reopen.
    #[test]
    fn usage_probe_refusal_of_a_record_less_instance_is_a_binding_trust_lifts_in_place() {
        let fx = Fixture::new("trust-usage-probe-record-less-refusal-lift");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.trust_identity(&peer.identity_hash, t(3_000));
        let sink = fx.observed();

        fx.store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(4_000), false)
            .unwrap();
        let after_refusal = fx.file_bytes();
        assert!(after_refusal.is_some());

        let again = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(5_000), false)
            .unwrap();
        assert_eq!(
            again,
            UntrustOutcome::Refused {
                identity: peer.identity_hash.clone(),
                already_refused: true,
                record_missing: false,
            }
        );
        assert_eq!(
            fx.file_bytes(),
            after_refusal,
            "a second refusal rewrites nothing"
        );
        assert_eq!(
            record(&fx.store, &peer.destination_hash).added_at,
            t(4_000),
            "the binding keeps the stamp of the refusal that wrote it"
        );

        let lifted = fx
            .store
            .trust_destination(
                &fx.mesh,
                &peer.destination_hash,
                TrustOptions {
                    label: Some("Alpha desk".to_string()),
                    note: None,
                },
                t(6_000),
            )
            .unwrap();
        assert_eq!(lifted.change, TrustChange::Updated);
        assert!(lifted.deny_lifted);
        assert_eq!(lifted.identity_hash, peer.identity_hash);
        assert!(fx.store.denied().is_empty());
        let listed = record(&fx.store, &peer.destination_hash);
        assert!(!listed.denied);
        assert_eq!(
            listed.identity.as_deref(),
            Some(peer.identity_hash.as_str())
        );
        assert_eq!(listed.label.as_deref(), Some("Alpha desk"));
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
        assert!(
            sink.drain().is_empty(),
            "refuse, refuse again and lift on the intact record fire nothing"
        );
        assert_eq!(fx.reopen().records(), fx.store.records());
    }

    /// Usage probe: an instance trusted for the session only, under an identity trusted
    /// for all on disk, is refused through its session twin: the refusal writes a disk
    /// record bound to the twin's identity with the twin's sighting, the session entry is
    /// folded into it (the hash is listed once, not as a session row), the verdict is the
    /// deny, and `untrust_identity` sweeps record and deny together.
    #[test]
    fn usage_probe_refusal_through_a_session_twin_persists_the_binding_once() {
        let fx = Fixture::new("trust-usage-probe-session-twin-refusal");
        let peer = announced("alpha");
        fx.announce(&peer, t(2_000));
        fx.store
            .trust_destination_for_session(&fx.mesh, &peer.destination_hash, t(2_500))
            .unwrap();
        fx.trust_identity(&peer.identity_hash, t(3_000));
        assert!(record(&fx.store, &peer.destination_hash).session);
        let before = fx.file_bytes();
        let sink = fx.observed();

        let dry = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(4_000), true)
            .unwrap();
        assert_eq!(
            dry,
            UntrustOutcome::Refused {
                identity: peer.identity_hash.clone(),
                already_refused: false,
                record_missing: false,
            }
        );
        assert_eq!(fx.file_bytes(), before, "a dry run writes nothing");
        assert!(record(&fx.store, &peer.destination_hash).session);
        assert!(fx.store.denied().is_empty());

        let outcome = fx
            .store
            .untrust_destination(&fx.mesh, &peer.destination_hash, t(4_000), false)
            .unwrap();
        assert_eq!(outcome, dry);
        let rows: Vec<TrustRecord> = fx
            .store
            .records()
            .into_iter()
            .filter(|record| record.hash == peer.destination_hash)
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "one row, not a disk record plus a session twin: {rows:?}"
        );
        let listed = &rows[0];
        assert!(!listed.session);
        assert!(listed.denied);
        assert_eq!(
            listed.identity.as_deref(),
            Some(peer.identity_hash.as_str())
        );
        assert_eq!(listed.last_seen_at, t(2_000));
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        assert_eq!(
            fx.reopen()
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Refuse, Rule::DestinationDenied),
            "the refusal outlives the session"
        );
        assert!(sink.drain().is_empty(), "a refusal fires nothing");

        let removed = fx
            .store
            .untrust_identity(&fx.mesh, &peer.identity_hash)
            .unwrap();
        assert_eq!(removed, vec![peer.destination_hash.clone()]);
        assert!(fx.store.denied().is_empty());
        assert!(
            !fx.store
                .records()
                .iter()
                .any(|record| record.hash == peer.destination_hash)
        );
        assert_eq!(
            fx.store
                .authorize(&peer.identity_hash, &peer.destination_hash),
            verdict(Decision::Refuse, Rule::DefaultClosed)
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

        let before = fx.store.authorize_origin(
            &parse_hash(&new.identity_hash).unwrap(),
            &decode_name_hash(&new.name_hash).unwrap(),
        );
        assert_eq!(
            before.verdict,
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert_eq!(before.destination.to_hex_string(), new.destination_hash);

        let marked = fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );
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

        let marked = fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );

        assert_eq!(
            marked,
            vec![MarkedKeyChange {
                destination_hash: old.destination_hash.clone(),
                bound_identity: old.identity_hash.clone(),
                seen_identity: new.identity_hash.clone(),
                label: None,
                denied: false,
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
        assert!(text.contains("presented under"), "{text}");
        let after_first = fx.file_bytes().unwrap();
        assert!(
            String::from_utf8(after_first.clone())
                .unwrap()
                .contains("key_changed:")
        );

        let again = fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(6_000),
        );
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(fx.file_bytes().unwrap(), after_first);

        let third = announced("alpha");
        let other = fx.store.note_key_change(
            &third.identity_hash,
            &third.name_hash,
            KeyChangeOutcome::Refused,
            t(7_000),
        );
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
        fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );

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
                .note_key_change(
                    &new.identity_hash,
                    &new.name_hash,
                    KeyChangeOutcome::Refused,
                    t(6_000)
                )
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
    fn re_trusting_the_marked_destination_keeps_its_mark() {
        let fx = Fixture::new("trust-key-change-retrust");
        let (old, new) = trusted_then_rotated(&fx);
        fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );
        let mark = record(&fx.store, &old.destination_hash).key_changed;
        assert!(mark.is_some());

        assert_eq!(fx.trust_destination(&old, t(7_000)), TrustChange::Updated);

        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, mark);
        assert_eq!(
            record(&fx.reopen(), &old.destination_hash).key_changed,
            mark
        );
        assert!(
            fx.store
                .note_key_change(
                    &new.identity_hash,
                    &new.name_hash,
                    KeyChangeOutcome::Refused,
                    t(8_000)
                )
                .is_empty(),
            "the mark stands, so the next conflicting sighting writes nothing"
        );
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
    }

    #[test]
    fn blocking_the_seen_identity_keeps_the_mark() {
        let fx = Fixture::new("trust-key-change-block-seen");
        let (old, new) = trusted_then_rotated(&fx);
        fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );
        let mark = record(&fx.store, &old.destination_hash).key_changed;
        assert!(mark.is_some());

        let removed = fx
            .store
            .block_identity(&fx.mesh, &new.identity_hash, None, t(6_000))
            .unwrap();

        assert!(removed.is_empty(), "the new identity held no records");
        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, mark);
        assert_eq!(
            record(&fx.reopen(), &old.destination_hash).key_changed,
            mark
        );
        assert_eq!(
            fx.store
                .authorize(&old.identity_hash, &old.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "the bound identity's grant is untouched"
        );
        let third = announced("alpha");
        assert!(
            fx.store
                .note_key_change(
                    &third.identity_hash,
                    &third.name_hash,
                    KeyChangeOutcome::Refused,
                    t(7_000)
                )
                .is_empty(),
            "the mark keeps its first sighting"
        );
        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, mark);
    }

    #[test]
    fn trusting_the_seen_identity_for_all_destinations_keeps_the_mark() {
        let fx = Fixture::new("trust-key-change-trust-seen");
        let (old, new) = trusted_then_rotated(&fx);
        fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );
        let mark = record(&fx.store, &old.destination_hash).key_changed;
        assert!(mark.is_some());

        assert_eq!(
            fx.trust_identity(&new.identity_hash, t(6_000)),
            TrustChange::Added
        );

        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, mark);
        assert_eq!(
            record(&fx.reopen(), &old.destination_hash).key_changed,
            mark
        );
        assert!(
            String::from_utf8(fx.file_bytes().unwrap())
                .unwrap()
                .contains("key_changed")
        );
        assert_eq!(
            fx.store
                .authorize(&old.identity_hash, &old.destination_hash),
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "the bound identity's grant is untouched"
        );
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "the grant admits the identity; the mark stands as the human's reminder"
        );
        assert!(
            fx.store
                .note_key_change(
                    &new.identity_hash,
                    &new.name_hash,
                    KeyChangeOutcome::Served,
                    t(7_000)
                )
                .is_empty(),
            "a record already marked is not written again"
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

        let marked = fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );

        assert!(marked.is_empty(), "{marked:?}");
        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, None);
        assert_eq!(fx.file_bytes().unwrap(), before);
    }

    #[test]
    fn a_denied_record_is_marked_too() {
        let fx = Fixture::new("trust-key-change-denied-record");
        let (old, new) = trusted_then_rotated(&fx);
        fx.store
            .deny_destination(&fx.mesh, &old.destination_hash, None, t(4_000))
            .unwrap();
        let before = fx.file_bytes().unwrap();

        let marked = fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );

        assert_eq!(marked.len(), 1, "{marked:?}");
        let listed = record(&fx.store, &old.destination_hash);
        assert!(listed.denied, "the deny stands");
        assert_eq!(
            listed.key_changed,
            Some(KeyChange {
                seen_identity: new.identity_hash.clone(),
                at: t(5_000),
            })
        );
        assert_ne!(fx.file_bytes().unwrap(), before);
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
    }

    #[test]
    fn an_all_destinations_identity_is_served_and_marks_with_a_warning() {
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
        let outcome = announce_outcome(&fx.store, &new);
        assert_eq!(outcome, KeyChangeOutcome::Served);

        let marked =
            fx.store
                .note_key_change(&new.identity_hash, &new.name_hash, outcome, t(5_000));

        assert_eq!(marked.len(), 1, "{marked:?}");
        assert_eq!(
            record(&fx.store, &old.destination_hash).key_changed,
            Some(KeyChange {
                seen_identity: new.identity_hash.clone(),
                at: t(5_000),
            })
        );
        assert_ne!(fx.file_bytes().unwrap(), before);
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        let text = &texts[0];
        assert!(text.starts_with("warning: "), "{text}");
        assert!(text.contains("is served"), "{text}");
        assert!(
            text.contains(&format!(".mesh trust {}", new.destination_hash)),
            "{text}"
        );
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
    }

    #[test]
    fn an_identity_tier_grants_rotation_is_surfaced_from_the_peer_table_but_unmarked() {
        let fx = Fixture::new("trust-key-change-identity-tier-grant");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
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

        let marked = fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(4_500),
        );

        assert!(marked.is_empty(), "{marked:?}");
        assert_eq!(
            fx.store.identity_standing(&new.identity_hash),
            IdentityStanding::Unknown
        );
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Refuse, Rule::DefaultClosed),
            "with no destination record there is no binding to conflict with; the stranger knocks"
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
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        let text = &texts[0];
        assert!(text.starts_with("error: "), "{text}");
        assert!(text.contains(&old.identity_hash), "{text}");
        assert!(text.contains(&new.identity_hash), "{text}");
        assert!(text.contains("nothing is marked"), "{text}");
        assert!(
            text.contains(&format!(".mesh trust {}", new.destination_hash)),
            "{text}"
        );
        assert!(
            text.contains(&format!(".mesh block {}", new.identity_hash)),
            "{text}"
        );
    }

    /// Usage probe: the presence-cache dedupe is keyed on the canonical lower-hex name
    /// hash. A knock or envelope may spell the instance id in upper case; the peer table
    /// row is still found, the one line is still earned, and a later lower-case (or
    /// upper-case again) presentation of the same instance under the same key earns none.
    #[test]
    fn usage_probe_an_identity_tier_rotation_spelled_in_upper_case_hex_is_surfaced_once_under_the_canonical_key()
     {
        let fx = Fixture::new("trust-key-change-identity-tier-upper-hex");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        assert_eq!(
            fx.trust_identity(&old.identity_hash, t(3_000)),
            TrustChange::Added
        );
        let new = announced("alpha");
        assert_eq!(new.name_hash, old.name_hash);
        fx.announce(&new, t(4_000));
        let before = fx.file_bytes().unwrap();
        let upper = new.name_hash.to_ascii_uppercase();
        assert_ne!(upper, new.name_hash, "the fixture's name hash has letters");

        let marked = fx.store.note_key_change(
            &new.identity_hash,
            &upper,
            KeyChangeOutcome::Refused,
            t(4_500),
        );

        assert!(marked.is_empty(), "{marked:?}");
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].starts_with("error: "), "{}", texts[0]);
        assert!(texts[0].contains(&old.identity_hash), "{}", texts[0]);
        assert!(texts[0].contains(&new.identity_hash), "{}", texts[0]);
        assert!(
            texts[0].contains(&format!(".mesh trust {}", new.destination_hash)),
            "the new destination is derived from the canonical hash: {}",
            texts[0]
        );

        for spelling in [new.name_hash.clone(), upper] {
            assert!(
                fx.store
                    .note_key_change(
                        &new.identity_hash,
                        &spelling,
                        KeyChangeOutcome::Refused,
                        t(5_000),
                    )
                    .is_empty()
            );
            assert_eq!(
                surface.texts().len(),
                1,
                "{spelling} is the same instance: not told again"
            );
        }
        assert_eq!(fx.file_bytes().unwrap(), before, "nothing is written");
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

        let marked = fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );

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
                .note_key_change(
                    &new.identity_hash,
                    &new.name_hash,
                    KeyChangeOutcome::Refused,
                    t(6_000)
                )
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
        fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );
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
        assert_eq!(
            superseded_destinations(&outcome),
            vec![old.destination_hash.clone()]
        );
        assert!(!outcome.superseded[0].denied);
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
        let again = fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(8_000),
        );

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

    fn origin_of(store: &TrustStore, peer: &Announced) -> OriginVerdict {
        store.authorize_origin(
            &parse_hash(&peer.identity_hash).unwrap(),
            &decode_name_hash(&peer.name_hash).unwrap(),
        )
    }

    #[test]
    fn authorize_origin_returns_the_collisions_its_verdict_was_judged_on() {
        let fx = Fixture::new("trust-origin-verdict-collisions");
        let (old, new) = trusted_then_rotated(&fx);

        let stranger = origin_of(&fx.store, &new);
        assert_eq!(
            stranger.verdict,
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert_eq!(stranger.destination.to_hex_string(), new.destination_hash);
        assert_eq!(
            stranger.collisions,
            vec![BindingConflict {
                destination_hash: old.destination_hash.clone(),
                bound_identity: old.identity_hash.clone(),
                label: None,
                denied: false,
                already_marked: false,
            }]
        );

        let bound = origin_of(&fx.store, &old);
        assert_eq!(
            bound.verdict,
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
        assert!(bound.collisions.is_empty(), "{:?}", bound.collisions);

        fx.trust_identity(&new.identity_hash, t(4_000));
        let served = origin_of(&fx.store, &new);
        assert_eq!(
            served.verdict,
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
        assert_eq!(served.collisions.len(), 1, "{:?}", served.collisions);

        fx.store
            .deny_destination(&fx.mesh, &new.destination_hash, None, t(5_000))
            .unwrap();
        let denied = origin_of(&fx.store, &new);
        assert_eq!(
            denied.verdict,
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        assert_eq!(
            denied.collisions.len(),
            1,
            "a deny on the requester still reports what it collides with: {:?}",
            denied.collisions
        );
        fx.store
            .undeny_destination(&fx.mesh, &new.destination_hash)
            .unwrap();

        fx.store
            .block_identity(&fx.mesh, &new.identity_hash, None, t(6_000))
            .unwrap();
        let blocked = origin_of(&fx.store, &new);
        assert_eq!(
            blocked.verdict,
            verdict(Decision::Refuse, Rule::IdentityBlocked)
        );
        assert!(blocked.collisions.is_empty(), "{:?}", blocked.collisions);
    }

    /// A known identity whose own destination is denied names an instance bound to another
    /// identity: the deny is the verdict, and the colliding record rides along for marking,
    /// `denied` telling the bound record's standing apart from the requester's.
    #[test]
    fn a_denied_requester_over_a_colliding_record_gets_the_collisions_with_its_verdict() {
        let fx = Fixture::new("trust-origin-verdict-denied-requester");
        let (old, new) = trusted_then_rotated(&fx);
        fx.announce(&new, t(3_500));
        fx.trust_identity(&new.identity_hash, t(4_000));
        fx.store
            .deny_destination(&fx.mesh, &new.destination_hash, None, t(5_000))
            .unwrap();

        let denied = origin_of(&fx.store, &new);

        assert_eq!(
            denied.verdict,
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        assert_eq!(
            denied.collisions,
            vec![BindingConflict {
                destination_hash: old.destination_hash.clone(),
                bound_identity: old.identity_hash.clone(),
                label: None,
                denied: false,
                already_marked: false,
            }]
        );

        fx.store
            .deny_destination(&fx.mesh, &old.destination_hash, None, t(6_000))
            .unwrap();
        let both_denied = origin_of(&fx.store, &new);
        assert_eq!(
            both_denied.verdict,
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        assert_eq!(
            both_denied
                .collisions
                .iter()
                .map(|conflict| conflict.denied)
                .collect::<Vec<_>>(),
            vec![true]
        );
    }

    #[test]
    fn binding_conflicts_reports_a_denied_record_and_an_all_destinations_identity() {
        let fx = Fixture::new("trust-binding-conflicts-detection");
        let (old, new) = trusted_then_rotated(&fx);
        let name_hash = decode_name_hash(&new.name_hash).unwrap();
        let conflicting = |store: &TrustStore| -> Vec<String> {
            store
                .binding_conflicts(&new.identity_hash, &name_hash)
                .into_iter()
                .map(|conflict| conflict.destination_hash)
                .collect()
        };

        assert_eq!(conflicting(&fx.store), vec![old.destination_hash.clone()]);
        assert!(
            fx.store
                .binding_conflicts(&old.identity_hash, &name_hash)
                .is_empty(),
            "the bound identity conflicts with nothing"
        );

        fx.store
            .deny_destination(&fx.mesh, &old.destination_hash, None, t(4_000))
            .unwrap();
        assert_eq!(
            conflicting(&fx.store),
            vec![old.destination_hash.clone()],
            "a denied record still collides"
        );

        fx.trust_identity(&new.identity_hash, t(5_000));
        assert_eq!(
            conflicting(&fx.store),
            vec![old.destination_hash.clone()],
            "an identity trusted for all destinations still collides"
        );

        fx.announce(&new, t(6_000));
        fx.trust_destination(&new, t(7_000));
        assert!(
            conflicting(&fx.store).is_empty(),
            "an identity holding its own record for the instance conflicts with nothing"
        );
    }

    #[test]
    fn note_key_change_marks_every_record_binding_conflicts_reports() {
        let fx = Fixture::new("trust-key-change-marks-all");
        let (first, second) = trusted_then_rotated(&fx);
        fx.announce(&second, t(3_500));
        fx.store
            .trust_destination_for_session(&fx.mesh, &second.destination_hash, t(4_000))
            .unwrap();
        let third = announced("alpha");
        let name_hash = decode_name_hash(&third.name_hash).unwrap();
        let mut expected: Vec<String> = fx
            .store
            .binding_conflicts(&third.identity_hash, &name_hash)
            .into_iter()
            .map(|conflict| conflict.destination_hash)
            .collect();
        expected.sort();
        assert_eq!(expected.len(), 2, "{expected:?}");

        let mut marked: Vec<String> = fx
            .store
            .note_key_change(
                &third.identity_hash,
                &third.name_hash,
                KeyChangeOutcome::Refused,
                t(5_000),
            )
            .into_iter()
            .map(|change| change.destination_hash)
            .collect();
        marked.sort();

        assert_eq!(marked, expected);
        for destination in [&first.destination_hash, &second.destination_hash] {
            assert_eq!(
                record(&fx.store, destination).key_changed,
                Some(KeyChange {
                    seen_identity: third.identity_hash.clone(),
                    at: t(5_000),
                })
            );
        }
        let on_disk = String::from_utf8(fx.file_bytes().unwrap()).unwrap();
        assert_eq!(
            on_disk.matches("key_changed:").count(),
            1,
            "the session record is marked in memory only"
        );
        assert!(
            fx.store
                .binding_conflicts(&third.identity_hash, &name_hash)
                .iter()
                .all(|conflict| conflict.already_marked)
        );
    }

    #[test]
    fn trusting_the_new_destination_clears_the_old_record_even_when_its_identity_is_trusted_for_all()
     {
        let fx = Fixture::new("trust-key-change-superseded-all-destinations");
        let (old, new) = trusted_then_rotated(&fx);
        fx.trust_identity(&new.identity_hash, t(4_000));
        fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Served,
            t(5_000),
        );
        assert!(
            record(&fx.store, &old.destination_hash)
                .key_changed
                .is_some()
        );
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
        assert_eq!(
            superseded_destinations(&outcome),
            vec![old.destination_hash.clone()]
        );
        assert_eq!(record(&fx.store, &old.destination_hash).key_changed, None);
        assert_eq!(
            record(&fx.reopen(), &old.destination_hash).key_changed,
            None
        );
        assert!(fx.store.is_trusted_identity(&new.identity_hash));
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "the destination record outranks the identity grant"
        );
        assert!(
            fx.store
                .note_key_change(
                    &new.identity_hash,
                    &new.name_hash,
                    KeyChangeOutcome::Served,
                    t(8_000)
                )
                .is_empty()
        );
    }

    /// The superseded record is a denied one: trusting the new destination clears its
    /// mark and names it with its deny, which stands as before.
    #[test]
    fn trusting_the_new_destination_clears_a_denied_old_records_mark_and_keeps_its_deny() {
        let fx = Fixture::new("trust-key-change-superseded-denied");
        let (old, new) = trusted_then_rotated(&fx);
        fx.store
            .deny_destination(&fx.mesh, &old.destination_hash, None, t(3_500))
            .unwrap();
        fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );
        let old_record = record(&fx.store, &old.destination_hash);
        assert!(old_record.denied);
        assert!(old_record.key_changed.is_some());
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

        assert_eq!(
            outcome.superseded,
            vec![BindingConflict {
                destination_hash: old.destination_hash.clone(),
                bound_identity: old.identity_hash.clone(),
                label: None,
                denied: true,
                already_marked: true,
            }]
        );
        let reopened = record(&fx.reopen(), &old.destination_hash);
        assert_eq!(reopened.key_changed, None);
        assert!(reopened.denied, "the deny on the old key stands");
        assert_eq!(
            origin_verdict(&fx.store, &old),
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
    }

    #[test]
    fn collision_protection_refuses_an_all_destinations_identity_over_a_colliding_record() {
        let fx = Fixture::new("trust-collision-protection-on");
        let (old, new) = trusted_then_rotated(&fx);
        fx.trust_identity(&new.identity_hash, t(4_000));
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
        assert_eq!(announce_outcome(&fx.store, &new), KeyChangeOutcome::Served);

        fx.store.set_collision_protection(true);

        let refused = origin_of(&fx.store, &new);
        assert_eq!(
            refused.verdict,
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert_eq!(refused.collisions.len(), 1, "{:?}", refused.collisions);
        assert_eq!(announce_outcome(&fx.store, &new), KeyChangeOutcome::Refused);
        assert_eq!(
            fx.store
                .authorize(&new.identity_hash, &new.destination_hash),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "only the origin-aware rung knows the name hash"
        );
        assert_eq!(
            origin_verdict(&fx.store, &old),
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "the bound identity's grant is untouched"
        );

        fx.store.set_collision_protection(false);
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
    }

    /// An identity trusted for all destinations whose own derived destination is denied is
    /// refused when it asks, so its announce over a colliding record marks with the error
    /// line, whatever the protection setting.
    #[test]
    fn an_all_destinations_identity_with_a_denied_destination_marks_with_an_error() {
        let fx = Fixture::new("trust-key-change-all-destinations-denied");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        let (old, new) = trusted_then_rotated(&fx);
        fx.trust_identity(&new.identity_hash, t(4_000));
        fx.store
            .deny_destination(&fx.mesh, &new.destination_hash, None, t(4_500))
            .unwrap();
        let refused = origin_of(&fx.store, &new);
        assert_eq!(
            refused.verdict,
            verdict(Decision::Refuse, Rule::DestinationDenied)
        );
        assert_eq!(refused.collisions.len(), 1, "{:?}", refused.collisions);
        let outcome = announce_outcome(&fx.store, &new);
        assert_eq!(outcome, KeyChangeOutcome::Refused);

        let marked =
            fx.store
                .note_key_change(&new.identity_hash, &new.name_hash, outcome, t(5_000));

        assert_eq!(marked.len(), 1, "{marked:?}");
        assert_eq!(
            record(&fx.store, &old.destination_hash)
                .key_changed
                .map(|mark| mark.seen_identity),
            Some(new.identity_hash.clone())
        );
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].starts_with("error: "), "{}", texts[0]);
        assert!(texts[0].contains("is refused when it asks"), "{}", texts[0]);
    }

    #[test]
    fn a_destination_allow_admits_in_either_collision_protection_mode() {
        let fx = Fixture::new("trust-collision-protection-destination-allow");
        let (old, new) = trusted_then_rotated(&fx);
        fx.announce(&new, t(4_000));
        fx.trust_destination(&new, t(5_000));

        for protection in [false, true] {
            fx.store.set_collision_protection(protection);
            for peer in [&old, &new] {
                let origin = origin_of(&fx.store, peer);
                assert_eq!(
                    origin.verdict,
                    verdict(Decision::Allow, Rule::DestinationTrusted),
                    "protection {protection}"
                );
                assert!(origin.collisions.is_empty(), "{:?}", origin.collisions);
            }
        }
    }

    #[test]
    fn a_presence_collision_writes_nothing_and_is_surfaced_once_per_pair() {
        let fx = Fixture::new("trust-presence-collision-no-write");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        fx.announce(&new, t(4_000));
        let before = fx.file_bytes().unwrap();

        for at in [4_100, 4_200] {
            let marked = fx.store.note_key_change(
                &new.identity_hash,
                &new.name_hash,
                KeyChangeOutcome::Refused,
                t(at),
            );
            assert!(marked.is_empty(), "{marked:?}");
        }

        assert_eq!(fx.file_bytes().unwrap(), before);
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());

        let stranger_instance = announced("beta");
        fx.announce(&stranger_instance, t(9_000));
        let other_stranger = announced("beta");
        fx.announce(&other_stranger, t(10_000));
        fx.store.note_key_change(
            &other_stranger.identity_hash,
            &other_stranger.name_hash,
            KeyChangeOutcome::Refused,
            t(11_000),
        );
        assert_eq!(
            surface.texts().len(),
            1,
            "two strangers sharing an instance id are the human's business only once one is trusted"
        );
    }

    /// The dedupe is keyed on the identity the peer row holds the instance under, not on
    /// the one presenting it: only that identity's own signed announces make such rows, so
    /// however many keys announce the instance the human hears of it once. A row that has
    /// aged out of the peer table is no presence at all.
    #[test]
    fn two_fresh_identities_presenting_the_same_instance_earn_one_presence_line() {
        let fx = Fixture::new("trust-presence-collision-one-line");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        let rotating = PrivateIdentity::new_from_rand(OsRng);
        let old = announced_as(rotating.clone(), "alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let before = fx.file_bytes().unwrap();

        for (n, at) in [3_100, 3_200, 3_300].into_iter().enumerate() {
            let fresh = announced("alpha");
            fx.announce(&fresh, t(at));
            let marked = fx.store.note_key_change(
                &fresh.identity_hash,
                &fresh.name_hash,
                KeyChangeOutcome::Refused,
                t(at + 50),
            );
            assert!(marked.is_empty(), "{marked:?}");
            assert_eq!(
                surface.texts().len(),
                1,
                "after {} keys: {:#?}",
                n + 1,
                surface.texts()
            );
        }
        assert!(surface.texts()[0].contains(&old.identity_hash));
        assert_eq!(fx.file_bytes().unwrap(), before);

        let old_gamma = announced_as(rotating, "gamma");
        fx.announce(&old_gamma, t(2_000));
        let late = announced("gamma");
        let expired = t(2_000) + PEER_TTL;
        fx.announce(&late, expired);
        fx.store.note_key_change(
            &late.identity_hash,
            &late.name_hash,
            KeyChangeOutcome::Refused,
            expired,
        );
        assert_eq!(
            surface.texts().len(),
            1,
            "a row that has aged out of the peer table is no presence"
        );
    }

    /// After the line that names the old key as the recorded identity and the new one as
    /// the presenter, the old key heard again is not a fresh collision with the roles
    /// swapped: the pair has been told once.
    #[test]
    fn the_old_key_heard_again_after_a_rotation_line_earns_no_second_line() {
        let fx = Fixture::new("trust-presence-collision-role-inverted");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        fx.announce(&new, t(4_000));
        fx.trust_identity(&new.identity_hash, t(4_200));
        fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Served,
            t(4_500),
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());

        fx.announce(&old, t(5_000));
        let marked = fx.store.note_key_change(
            &old.identity_hash,
            &old.name_hash,
            KeyChangeOutcome::Served,
            t(5_100),
        );

        assert!(marked.is_empty(), "{marked:?}");
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].contains(&old.identity_hash), "{}", texts[0]);
        assert!(texts[0].contains(&new.identity_hash), "{}", texts[0]);
    }

    #[test]
    fn a_denied_records_line_says_the_old_key_stays_refused() {
        let fx = Fixture::new("trust-key-change-denied-record-text");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        let (old, new) = trusted_then_rotated(&fx);
        fx.store
            .deny_destination(&fx.mesh, &old.destination_hash, None, t(4_000))
            .unwrap();

        let marked = fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(5_000),
        );

        assert_eq!(marked.len(), 1, "{marked:?}");
        assert!(marked[0].denied);
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        let text = &texts[0];
        assert!(text.starts_with("error: "), "{text}");
        assert!(text.contains("the old key stays refused"), "{text}");
        assert!(!text.contains("grant stays"), "{text}");
        assert!(text.contains("presented under"), "{text}");
        assert!(
            text.contains(&format!(".mesh block {}", new.identity_hash)),
            "{text}"
        );
    }

    #[test]
    fn a_presence_collision_between_two_all_destinations_identities_is_a_warning() {
        let fx = Fixture::new("trust-presence-collision-warning");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        let rotating = PrivateIdentity::new_from_rand(OsRng);
        let old = announced_as(rotating.clone(), "alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        fx.announce(&new, t(4_000));
        fx.trust_identity(&new.identity_hash, t(5_000));
        let outcome = announce_outcome(&fx.store, &new);
        assert_eq!(outcome, KeyChangeOutcome::Served);

        let marked =
            fx.store
                .note_key_change(&new.identity_hash, &new.name_hash, outcome, t(4_500));

        assert!(marked.is_empty(), "{marked:?}");
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        let text = &texts[0];
        assert!(text.starts_with("warning: "), "{text}");
        assert!(text.contains(&old.identity_hash), "{text}");
        assert!(text.contains(&new.identity_hash), "{text}");
        assert!(text.contains("nothing is marked"), "{text}");
        assert!(
            text.contains(&format!(".mesh block {}", new.identity_hash)),
            "{text}"
        );
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );

        let old_beta = announced_as(rotating, "beta");
        fx.announce(&old_beta, t(7_000));
        let new_beta = announced("beta");
        fx.announce(&new_beta, t(8_000));
        fx.trust_destination(&new_beta, t(9_000));
        assert!(
            fx.store
                .note_key_change(
                    &new_beta.identity_hash,
                    &new_beta.name_hash,
                    KeyChangeOutcome::Refused,
                    t(10_000)
                )
                .is_empty()
        );
        assert_eq!(
            surface.texts().len(),
            1,
            "an identity holding its own record for the instance is not a presence collision"
        );
    }

    /// The presence rung of `authorize_origin`: under collision protection an identity
    /// trusted for all destinations that presents an instance the peer table holds under
    /// another such identity is refused by identity changed, with no record collisions to
    /// report. The grant alone still allows, the refusal earns one `error:` line naming
    /// both identities and `.mesh trust <new destination>`, nothing is marked or written,
    /// the destination allow then admits, and protection off serves as before.
    #[test]
    fn collision_protection_refuses_a_presence_detected_rotation_of_an_identity_trusted_for_all() {
        let fx = Fixture::new("trust-presence-collision-protected");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        assert_eq!(new.name_hash, old.name_hash);
        fx.announce(&new, t(4_000));
        fx.trust_identity(&new.identity_hash, t(5_000));
        let before = fx.file_bytes().unwrap();
        let identity = parse_hash(&new.identity_hash).unwrap();
        let name_hash = decode_name_hash(&new.name_hash).unwrap();

        let origin = fx
            .store
            .authorize_origin_at(&identity, &name_hash, t(4_500));
        assert_eq!(
            origin.verdict,
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert!(origin.collisions.is_empty(), "{:?}", origin.collisions);
        assert_eq!(
            fx.store
                .authorize(&new.identity_hash, &new.destination_hash),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "the grant alone still allows what the node sends"
        );
        assert_eq!(
            fx.store
                .authorize_origin_at(
                    &parse_hash(&old.identity_hash).unwrap(),
                    &name_hash,
                    t(4_500)
                )
                .verdict,
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the table does not say which key is the rotation, so the old key is judged alike"
        );

        let marked = fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Refused,
            t(4_500),
        );
        assert!(marked.is_empty(), "{marked:?}");
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        let text = &texts[0];
        assert!(text.starts_with("error: "), "{text}");
        assert!(text.contains(&old.identity_hash), "{text}");
        assert!(text.contains(&new.identity_hash), "{text}");
        assert!(text.contains("is refused when it asks"), "{text}");
        assert!(text.contains("nothing is marked"), "{text}");
        assert!(text.contains("presented under"), "{text}");
        assert!(!text.contains("announced under"), "{text}");
        assert!(
            text.contains(&format!(
                ".mesh trust {}; otherwise .mesh block {}",
                new.destination_hash, new.identity_hash
            )),
            "{text}"
        );
        assert_eq!(fx.file_bytes().unwrap(), before, "nothing is written");
        assert!(
            fx.store
                .records()
                .iter()
                .all(|record| record.key_changed.is_none()),
            "{:#?}",
            fx.store.records()
        );

        fx.store.set_collision_protection(false);
        assert_eq!(
            fx.store
                .authorize_origin_at(&identity, &name_hash, t(4_500))
                .verdict,
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "protection off serves the identity allow"
        );
        fx.store.set_collision_protection(true);

        assert_eq!(fx.trust_destination(&new, t(6_000)), TrustChange::Added);
        assert_eq!(
            fx.store
                .authorize_origin_at(&identity, &name_hash, t(6_500))
                .verdict,
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "the human trusting the new destination admits it under protection"
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());
    }

    /// `InstancePresence` that counts how often the store consults it.
    struct CountingPresence(Arc<PeerTable>, AtomicUsize);

    impl InstancePresence for CountingPresence {
        fn heard_under_other_identities(
            &self,
            name_hash: &str,
            identity_hash: &str,
            now: SystemTime,
        ) -> Vec<(String, String)> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0
                .heard_under_other_identities(name_hash, identity_hash, now)
        }
    }

    /// The presence rung costs a peer table scan, so `authorize_origin` pays it only for
    /// an identity-allow verdict under collision protection: a stranger (default closed),
    /// a destination allow and a blocked identity never reach the table, and with
    /// protection off neither does the identity allow.
    #[test]
    fn authorize_origin_consults_the_peer_table_only_for_an_identity_allow_under_protection() {
        let fx = Fixture::new("trust-presence-rung-is-identity-allow-only");
        let presence = Arc::new(CountingPresence(fx.mesh.0.clone(), AtomicUsize::new(0)));
        fx.store
            .attach_presence(Arc::downgrade(&presence) as Weak<dyn InstancePresence>);
        let old = announced("alpha");
        fx.announce(&old, t(4_000));
        fx.trust_identity(&old.identity_hash, t(4_000));
        let new = announced("alpha");
        fx.announce(&new, t(4_100));
        let identity = parse_hash(&new.identity_hash).unwrap();
        let name_hash = decode_name_hash(&new.name_hash).unwrap();
        let origin = |now| {
            fx.store
                .authorize_origin_at(&identity, &name_hash, now)
                .verdict
        };
        let scans = || presence.1.load(Ordering::SeqCst);

        fx.store.set_collision_protection(true);
        assert_eq!(
            origin(t(4_500)),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert_eq!(scans(), 0, "a stranger is not judged against the table");

        fx.trust_identity(&new.identity_hash, t(5_000));
        fx.store.set_collision_protection(false);
        assert_eq!(
            origin(t(5_500)),
            verdict(Decision::Allow, Rule::IdentityTrusted)
        );
        assert_eq!(scans(), 0, "protection off never reads the table");

        fx.store.set_collision_protection(true);
        assert_eq!(
            origin(t(5_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert_eq!(
            scans(),
            1,
            "the identity allow under protection is the one rung that scans"
        );

        assert_eq!(fx.trust_destination(&new, t(6_000)), TrustChange::Added);
        assert_eq!(
            origin(t(6_500)),
            verdict(Decision::Allow, Rule::DestinationTrusted)
        );
        assert_eq!(
            scans(),
            1,
            "a destination allow is not judged against the table"
        );

        fx.store
            .block_identity(&fx.mesh, &new.identity_hash, None, t(7_000))
            .unwrap();
        assert_eq!(
            origin(t(7_500)),
            verdict(Decision::Refuse, Rule::IdentityBlocked)
        );
        assert_eq!(
            scans(),
            1,
            "a blocked identity is not judged against the table"
        );
    }

    /// The presence rung's memory is of a detection, not a guess: before any owner line
    /// was earned, and with the old row aged past `PEER_TTL`, the presenter is admitted.
    /// Once `note_key_change` has surfaced the line, the refusal outlives the old row:
    /// still identity changed at `+PEER_TTL`. Blocking the presenter admits the holder,
    /// since the memory never refuses the identity it names; blocking (or untrusting) the
    /// holder admits the presenter, since the memory refuses only for a holder still
    /// trusted for all destinations. Trusting the presenter's destination admits it
    /// regardless. Judging writes nothing and marks nothing throughout.
    #[test]
    fn a_presence_refusal_outlives_the_old_row_once_its_line_was_earned() {
        let fx = Fixture::new("trust-presence-refusal-outlives-row");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store.set_collision_protection(true);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        assert_eq!(new.name_hash, old.name_hash);
        fx.announce(&new, t(4_000));
        fx.trust_identity(&new.identity_hash, t(5_000));
        let before = fx.file_bytes().unwrap();
        let name_hash = decode_name_hash(&new.name_hash).unwrap();
        let origin = |peer: &Announced, now| {
            fx.store
                .authorize_origin_at(&parse_hash(&peer.identity_hash).unwrap(), &name_hash, now)
                .verdict
        };
        let aged_out = t(4_000) + PEER_TTL;

        assert_eq!(
            origin(&new, t(4_500)),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "no peer table attached: nothing to consult, the identity allow stands"
        );

        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        assert_eq!(
            origin(&new, aged_out),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "no line earned and both rows aged out: nothing is remembered"
        );
        assert_eq!(
            origin(&new, t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the old row is live at 4 500 s"
        );
        assert_eq!(
            origin(&old, t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "while both rows are live the holder is refused too"
        );

        assert!(
            fx.store
                .note_key_change(
                    &new.identity_hash,
                    &new.name_hash,
                    KeyChangeOutcome::Refused,
                    t(4_500),
                )
                .is_empty(),
            "nothing is marked"
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());
        assert_eq!(
            origin(&new, aged_out),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the line was earned, so the refusal outlives the old row"
        );
        assert_eq!(
            origin(&old, aged_out),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "the memory never refuses the holder it names; with the presenter's row gone too, \
             the holder is admitted"
        );
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");

        fx.store
            .block_identity(&fx.mesh, &new.identity_hash, None, t(5_000))
            .unwrap();
        assert_eq!(
            origin(&new, t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityBlocked)
        );
        assert_eq!(
            origin(&old, t(4_500)),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "blocking the presenter restores the holder while the presenter's row is still live"
        );
        fx.store
            .unblock_identity(&fx.mesh, &new.identity_hash)
            .unwrap();
        fx.trust_identity(&new.identity_hash, t(5_100));
        assert_eq!(
            origin(&new, aged_out),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "re-trusting the presenter for all destinations does not lift the memory"
        );

        fx.store
            .untrust_identity(&fx.mesh, &old.identity_hash)
            .unwrap();
        assert_eq!(
            origin(&new, aged_out),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "the holder no longer trusted for all destinations: the memory refuses nobody"
        );
        fx.trust_identity(&old.identity_hash, t(5_200));
        assert_eq!(
            origin(&new, aged_out),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the memory stands while the node runs; the holder trusted again, it refuses again"
        );
        fx.store
            .block_identity(&fx.mesh, &old.identity_hash, None, t(5_300))
            .unwrap();
        assert_eq!(
            origin(&new, aged_out),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "blocking the holder admits the presenter"
        );
        fx.store
            .unblock_identity(&fx.mesh, &old.identity_hash)
            .unwrap();
        fx.trust_identity(&old.identity_hash, t(5_400));

        assert_eq!(fx.trust_destination(&new, t(6_000)), TrustChange::Added);
        assert_eq!(
            origin(&new, aged_out),
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "the human trusting the new destination admits it under protection"
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());
        assert!(
            fx.store
                .records()
                .iter()
                .all(|record| record.key_changed.is_none()),
            "{:#?}",
            fx.store.records()
        );
    }

    /// The remembered pairs and their per-instance index stay in step across the
    /// `PRESENCE_SURFACED_CAP` eviction: a pair evicted from the oldest end refuses nobody,
    /// a remembered one still does, and the index holds no instance without a holder.
    #[test]
    fn remembered_presence_pairs_and_their_index_evict_together_at_the_cap() {
        let holder = fake_hash(0x11);
        let presenter = fake_hash(0x22);
        let mut state = State::default();
        state
            .file
            .identities
            .insert(holder.clone(), identity_entry(t(1_000), t(1_000), true));
        let instance = |n: usize| format!("instance-{n}");

        assert!(state.remember_presence((instance(0), holder.clone())));
        assert!(
            !state.remember_presence((instance(0), holder.clone())),
            "a pair is remembered once"
        );
        assert!(state.remembered_under_trusted_for_all(&instance(0), &presenter));
        assert!(
            !state.remembered_under_trusted_for_all(&instance(0), &holder),
            "the holder is never refused by its own memory"
        );
        assert!(state.tell_presence_line(&instance(0), &presenter));
        assert!(
            !state.tell_presence_line(&instance(0), &presenter),
            "a presenter is told once per instance"
        );

        for n in 1..=PRESENCE_SURFACED_CAP {
            assert!(state.remember_presence((instance(n), holder.clone())));
        }
        assert_eq!(state.presence_surfaced.len(), PRESENCE_SURFACED_CAP);
        assert_eq!(state.presence_surfaced_order.len(), PRESENCE_SURFACED_CAP);
        assert_eq!(state.presence_remembered.len(), PRESENCE_SURFACED_CAP);
        assert!(
            !state
                .presence_surfaced
                .contains(&(instance(0), holder.clone())),
            "the oldest pair was evicted"
        );
        assert!(
            !state.remembered_under_trusted_for_all(&instance(0), &presenter),
            "the evicted pair refuses nobody"
        );
        assert!(
            state.tell_presence_line(&instance(0), &presenter),
            "the evicted instance forgot who it told"
        );
        assert!(
            state.remembered_under_trusted_for_all(&instance(1), &presenter),
            "the next-oldest pair still does"
        );
        assert!(
            state.remembered_under_trusted_for_all(&instance(PRESENCE_SURFACED_CAP), &presenter),
            "and so does the newest"
        );
        assert!(
            state
                .presence_remembered
                .values()
                .all(|holders| !holders.is_empty())
        );
        assert!(
            state.remember_presence((instance(0), holder.clone())),
            "an evicted pair can be remembered again"
        );
        assert!(!state.presence_surfaced.contains(&(instance(1), holder)));
        assert!(!state.remembered_under_trusted_for_all(&instance(1), &presenter));
    }

    /// Usage probe: a presence-only rotation is refused by the verdict alone; the line it
    /// earns is deduped once per (instance, recorded identity) while the refusal itself
    /// holds for every request, and the dedupe is shared with the protection-off warning,
    /// so flipping the setting after the line was earned restates nothing.
    #[test]
    fn usage_probe_a_presence_refusal_holds_per_request_while_its_line_is_earned_once() {
        let fx = Fixture::new("trust-probe-presence-refusal-holds");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        fx.announce(&new, t(4_000));
        fx.trust_identity(&new.identity_hash, t(5_000));
        let identity = parse_hash(&new.identity_hash).unwrap();
        let name_hash = decode_name_hash(&new.name_hash).unwrap();

        for round in 0..3 {
            let origin = fx
                .store
                .authorize_origin_at(&identity, &name_hash, t(4_500 + round));
            assert_eq!(
                origin.verdict,
                verdict(Decision::Refuse, Rule::IdentityChanged),
                "round {round}"
            );
            assert!(
                fx.store
                    .note_key_change(
                        &new.identity_hash,
                        &new.name_hash,
                        KeyChangeOutcome::Refused,
                        t(4_500 + round),
                    )
                    .is_empty(),
                "round {round}: nothing is marked"
            );
        }
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "one line for three refusals: {texts:#?}");
        assert!(texts[0].starts_with("error: "), "{}", texts[0]);

        fx.store.set_collision_protection(false);
        assert!(
            fx.store
                .note_key_change(
                    &new.identity_hash,
                    &new.name_hash,
                    KeyChangeOutcome::Served,
                    t(4_600),
                )
                .is_empty()
        );
        assert_eq!(
            surface.texts().len(),
            1,
            "the served variant shares the dedupe: {:#?}",
            surface.texts()
        );
    }

    /// Usage probe: the served presence line, the one `mesh.collision_protection: false`
    /// earns, names the setting that let the identity through and no longer promises that
    /// "nothing is needed"; both variants say "presented under", never "announced under",
    /// since the same line serves an announce, a link, a knock and a stored request.
    #[test]
    fn usage_probe_the_served_presence_line_names_the_setting_and_promises_nothing() {
        let fx = Fixture::new("trust-probe-presence-served-line");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        fx.announce(&new, t(4_000));
        fx.trust_identity(&new.identity_hash, t(5_000));
        assert_eq!(
            origin_verdict(&fx.store, &new),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "protection off: served"
        );

        fx.store.note_key_change(
            &new.identity_hash,
            &new.name_hash,
            KeyChangeOutcome::Served,
            t(4_500),
        );

        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        let text = &texts[0];
        assert!(text.starts_with("warning: "), "{text}");
        assert!(
            text.contains("is served while it asks because collision_protection is off"),
            "{text}"
        );
        assert!(!text.contains("nothing is needed"), "{text}");
        assert!(text.contains("presented under"), "{text}");
        assert!(!text.contains("announced under"), "{text}");
        assert!(text.contains("nothing is marked"), "{text}");
        assert!(
            text.contains("confirm with its holder out of band"),
            "{text}"
        );
        assert!(
            text.contains(&format!(".mesh block {}", new.identity_hash)),
            "{text}"
        );
    }

    /// Usage probe: two memos for one instance. A stranger presents the instance while
    /// the table holds it under A (trusted for all destinations) and earns the pair line
    /// naming A; later, with the table holding it under B too (also trusted for all
    /// destinations, the fresher row), the stranger presents again and earns the pair line
    /// naming B. Both memos now stand. A remembered holder refused by the OTHER holder's
    /// memory earns no line of its own: A presenting is refused as identity changed by
    /// B's memo, B by A's, silently, and both stay refused once every row has aged out.
    /// `.mesh block <B>` restores A, whose grant admits it again, while B is blocked.
    /// Nothing is marked and `trust.yaml` keeps its bytes until the block writes it.
    #[test]
    fn usage_probe_a_remembered_holder_refused_by_another_holders_memory_earns_no_line_and_block_restores_it()
     {
        let fx = Fixture::new("trust-probe-presence-two-memos");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let a = announced("alpha");
        fx.announce(&a, t(2_000));
        fx.trust_identity(&a.identity_hash, t(2_100));
        let stranger = announced("alpha");
        let name_hash = decode_name_hash(&a.name_hash).unwrap();
        let judge = |peer: &Announced, now| {
            let origin = fx.store.authorize_origin_at(
                &parse_hash(&peer.identity_hash).unwrap(),
                &name_hash,
                now,
            );
            let outcome = match origin.verdict.decision {
                Decision::Allow => KeyChangeOutcome::Served,
                Decision::Refuse => KeyChangeOutcome::Refused,
            };
            assert!(
                fx.store
                    .note_key_change(&peer.identity_hash, &peer.name_hash, outcome, now)
                    .is_empty(),
                "a presence-only collision marks nothing"
            );
            origin.verdict
        };

        assert_eq!(
            judge(&stranger, t(2_500)),
            verdict(Decision::Refuse, Rule::DefaultClosed),
            "a stranger with no allow is default closed, presence or not"
        );
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].starts_with("error: "), "{}", texts[0]);
        assert!(texts[0].contains(&a.identity_hash), "{}", texts[0]);
        assert!(texts[0].contains(&stranger.identity_hash), "{}", texts[0]);

        let b = announced("alpha");
        fx.announce(&b, t(3_000));
        fx.trust_identity(&b.identity_hash, t(3_100));
        let before = fx.file_bytes().unwrap();
        assert_eq!(
            judge(&stranger, t(3_500)),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        let texts = surface.texts();
        assert_eq!(
            texts.len(),
            2,
            "the fresher row under B earns the pair line naming B: {texts:#?}"
        );
        assert!(texts[1].starts_with("error: "), "{}", texts[1]);
        assert!(texts[1].contains(&b.identity_hash), "{}", texts[1]);
        assert!(!texts[1].contains(&a.identity_hash), "{}", texts[1]);

        for (holder, label) in [(&a, "A"), (&b, "B")] {
            assert_eq!(
                judge(holder, t(3_600)),
                verdict(Decision::Refuse, Rule::IdentityChanged),
                "{label} is refused by the other holder's memory while both rows live"
            );
        }
        assert_eq!(
            surface.texts().len(),
            2,
            "a remembered holder refused by another holder's memory earns no line: {:#?}",
            surface.texts()
        );

        let every_row_gone = t(3_000) + PEER_TTL + Duration::from_secs(1);
        for (holder, label) in [(&a, "A"), (&b, "B")] {
            assert_eq!(
                judge(holder, every_row_gone),
                verdict(Decision::Refuse, Rule::IdentityChanged),
                "{label} stays refused from the memory once every row has aged out"
            );
        }
        assert_eq!(
            judge(&stranger, every_row_gone),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert_eq!(surface.texts().len(), 2, "{:#?}", surface.texts());
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");
        assert!(
            fx.store
                .records()
                .iter()
                .all(|record| record.key_changed.is_none()),
            "{:#?}",
            fx.store.records()
        );

        fx.store
            .block_identity(&fx.mesh, &b.identity_hash, None, t(6_000))
            .unwrap();
        assert_eq!(
            judge(&a, t(6_100)),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "blocking B restores A: B's memo refuses only while B is trusted for all destinations"
        );
        assert_eq!(
            judge(&b, t(6_100)),
            verdict(Decision::Refuse, Rule::IdentityBlocked)
        );
        assert_eq!(
            surface.texts().len(),
            2,
            "the restoration tells nobody: {:#?}",
            surface.texts()
        );

        fx.store
            .block_identity(&fx.mesh, &a.identity_hash, None, t(6_200))
            .unwrap();
        fx.store
            .unblock_identity(&fx.mesh, &b.identity_hash)
            .unwrap();
        fx.trust_identity(&b.identity_hash, t(6_400));
        assert_eq!(
            judge(&b, t(6_500)),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "with A blocked, A's memo refuses nobody: B's restored grant admits it"
        );
        assert_eq!(surface.texts().len(), 2, "{:#?}", surface.texts());
    }

    /// Usage probe: the symmetric half of the presence rung, driven the way every ingress
    /// drives it (verdict, then `note_key_change` with that verdict). Both rows live, the new
    /// key presents first and earns the one `error:` line, which remembers (instance, old).
    /// The old key then presents and is refused by the live scan too, but its own trip
    /// through `note_key_change` earns no second line and plants no memory naming the new
    /// key: once the new key's row has aged out the old key is admitted again while the new
    /// key stays refused from the remembered line. `trust.yaml` is byte-identical throughout.
    #[test]
    fn usage_probe_the_holders_symmetric_refusal_earns_no_line_and_leaves_no_memory_of_its_own() {
        let fx = Fixture::new("trust-probe-presence-holder-no-second-line");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        fx.announce(&new, t(4_000));
        fx.trust_identity(&new.identity_hash, t(5_000));
        let before = fx.file_bytes().unwrap();
        let name_hash = decode_name_hash(&new.name_hash).unwrap();
        let judge = |peer: &Announced, now| {
            let origin = fx.store.authorize_origin_at(
                &parse_hash(&peer.identity_hash).unwrap(),
                &name_hash,
                now,
            );
            let outcome = match origin.verdict.decision {
                Decision::Allow => KeyChangeOutcome::Served,
                Decision::Refuse => KeyChangeOutcome::Refused,
            };
            let marked =
                fx.store
                    .note_key_change(&peer.identity_hash, &peer.name_hash, outcome, now);
            assert!(marked.is_empty(), "a presence-only collision marks nothing");
            origin.verdict
        };

        assert_eq!(
            judge(&new, t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the new key presenting over the old key's live row is refused"
        );
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].starts_with("error: "), "{}", texts[0]);
        assert!(texts[0].contains(&new.identity_hash), "{}", texts[0]);

        assert_eq!(
            judge(&old, t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "while both rows are live the old key is refused too"
        );
        assert_eq!(
            surface.texts().len(),
            1,
            "the holder's refusal earns no second line: {:#?}",
            surface.texts()
        );

        let new_row_gone = t(4_000) + PEER_TTL;
        assert_eq!(
            judge(&old, new_row_gone),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "the holder's refusal left no memory naming the new key: with the new key's row \
             gone the old key is admitted again"
        );
        assert_eq!(
            judge(&new, new_row_gone),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the new key stays refused from the remembered line"
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");
        assert!(
            fx.store
                .records()
                .iter()
                .all(|record| record.key_changed.is_none()),
            "{:#?}",
            fx.store.records()
        );
    }

    /// Usage probe: the memory names whichever identity held the row when the FIRST
    /// presenter earned the line, so when both rows are live and the earlier key happens to
    /// present first, the earlier key is the one remembered as refused and the later key
    /// is admitted once the earlier row is gone. The line is still earned once, the
    /// remedies the spec names still hold (`.mesh block <remembered holder>` admits the
    /// refused key; `.mesh trust <refused key's destination>` admits it too), and nothing is
    /// written or marked. This pins the documented symmetry, roles assigned by order.
    #[test]
    fn usage_probe_the_first_presenter_while_both_rows_are_live_is_the_one_remembered() {
        let fx = Fixture::new("trust-probe-presence-first-presenter-remembered");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        fx.announce(&new, t(4_000));
        fx.trust_identity(&new.identity_hash, t(5_000));
        let before = fx.file_bytes().unwrap();
        let name_hash = decode_name_hash(&new.name_hash).unwrap();
        let judge = |peer: &Announced, now| {
            let origin = fx.store.authorize_origin_at(
                &parse_hash(&peer.identity_hash).unwrap(),
                &name_hash,
                now,
            );
            let outcome = match origin.verdict.decision {
                Decision::Allow => KeyChangeOutcome::Served,
                Decision::Refuse => KeyChangeOutcome::Refused,
            };
            assert!(
                fx.store
                    .note_key_change(&peer.identity_hash, &peer.name_hash, outcome, now)
                    .is_empty()
            );
            origin.verdict
        };

        assert_eq!(
            judge(&old, t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the earlier key presenting over the later key's live row is refused"
        );
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(texts[0].starts_with("error: "), "{}", texts[0]);
        assert!(
            texts[0].contains(&old.identity_hash) && texts[0].contains(&new.identity_hash),
            "{}",
            texts[0]
        );
        assert_eq!(
            judge(&new, t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the later key is refused by the live scan while the earlier row stands"
        );
        assert_eq!(
            surface.texts().len(),
            1,
            "no second line: {:#?}",
            surface.texts()
        );

        let rows_gone = t(4_000) + PEER_TTL;
        assert_eq!(
            judge(&old, rows_gone),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the earlier key presented first, so it is the one remembered as refused"
        );
        assert_eq!(
            judge(&new, rows_gone),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "the later key is the remembered holder and is admitted once the earlier row is gone"
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");

        fx.store
            .block_identity(&fx.mesh, &new.identity_hash, None, t(6_000))
            .unwrap();
        assert_eq!(
            judge(&old, rows_gone),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "blocking the remembered holder admits the refused key"
        );
        fx.store
            .unblock_identity(&fx.mesh, &new.identity_hash)
            .unwrap();
        fx.trust_identity(&new.identity_hash, t(6_100));
        assert_eq!(
            judge(&old, rows_gone),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the memory stands once the holder is trusted for all destinations again"
        );
        assert_eq!(fx.trust_destination(&old, t(6_200)), TrustChange::Added);
        assert_eq!(
            judge(&old, rows_gone),
            verdict(Decision::Allow, Rule::DestinationTrusted),
            "trusting the refused key's own destination admits it, the rung not consulted"
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());
        assert!(
            fx.store
                .records()
                .iter()
                .all(|record| record.key_changed.is_none()),
            "{:#?}",
            fx.store.records()
        );
    }

    /// `InstancePresence` that reports every instance as held by one identity while `live`,
    /// and nothing once switched off, so the remembered pairs can be told from the table.
    struct SwitchablePresence {
        holder: String,
        live: AtomicBool,
    }

    impl InstancePresence for SwitchablePresence {
        fn heard_under_other_identities(
            &self,
            name_hash: &str,
            identity_hash: &str,
            _now: SystemTime,
        ) -> Vec<(String, String)> {
            if !self.live.load(Ordering::SeqCst) || identity_hash == self.holder {
                return Vec::new();
            }
            let destination = format!("{name_hash}{}", &fake_hash(0x7d)[name_hash.len()..]);
            vec![(destination, self.holder.clone())]
        }
    }

    /// Usage probe: the memory's bound seen through the verdict rather than the `State`
    /// internals. One presenter earns a line for `PRESENCE_SURFACED_CAP` + 1 instances all
    /// held by one identity trusted for all destinations; the oldest pair is evicted, so with
    /// the table silent that instance is admitted again while the next-oldest and the newest
    /// stay refused, and the evicted instance's line can be earned a second time once the
    /// table holds it again. The file never changes.
    #[test]
    fn usage_probe_an_evicted_presence_memory_lifts_its_refusal_and_its_line_can_be_earned_again() {
        let fx = Fixture::new("trust-probe-presence-cap-through-the-verdict");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store.set_collision_protection(true);
        let holder = announced("holder");
        fx.announce(&holder, t(2_000));
        fx.trust_identity(&holder.identity_hash, t(3_000));
        let presenter = announced("presenter");
        fx.announce(&presenter, t(2_000));
        fx.trust_identity(&presenter.identity_hash, t(3_000));
        let presence = Arc::new(SwitchablePresence {
            holder: holder.identity_hash.clone(),
            live: AtomicBool::new(true),
        });
        fx.store
            .attach_presence(Arc::downgrade(&presence) as Weak<dyn InstancePresence>);
        let before = fx.file_bytes().unwrap();
        let identity = parse_hash(&presenter.identity_hash).unwrap();
        let instance = |n: usize| {
            let name = session_destination_name(&format!("cap-{n}"));
            let bytes: [u8; NAME_HASH_LEN] = name.as_name_hash_slice().try_into().unwrap();
            (bytes, hex_lower(&bytes))
        };
        let judge = |n: usize, now| {
            let (bytes, hex) = instance(n);
            let origin = fx.store.authorize_origin_at(&identity, &bytes, now);
            let outcome = match origin.verdict.decision {
                Decision::Allow => KeyChangeOutcome::Served,
                Decision::Refuse => KeyChangeOutcome::Refused,
            };
            assert!(
                fx.store
                    .note_key_change(&presenter.identity_hash, &hex, outcome, now)
                    .is_empty()
            );
            origin.verdict
        };

        for n in 0..=PRESENCE_SURFACED_CAP {
            assert_eq!(
                judge(n, t(4_000)),
                verdict(Decision::Refuse, Rule::IdentityChanged),
                "instance {n}"
            );
        }
        assert_eq!(
            surface.texts().len(),
            PRESENCE_SURFACED_CAP + 1,
            "one line per (instance, holder) pair"
        );

        presence.live.store(false, Ordering::SeqCst);
        assert_eq!(
            judge(0, t(4_100)),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "the oldest pair was evicted at the cap: with the table silent it refuses nobody"
        );
        assert_eq!(
            judge(1, t(4_100)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the next-oldest pair is still remembered"
        );
        assert_eq!(
            judge(PRESENCE_SURFACED_CAP, t(4_100)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "and so is the newest"
        );
        assert_eq!(
            surface.texts().len(),
            PRESENCE_SURFACED_CAP + 1,
            "remembered refusals restate nothing"
        );

        presence.live.store(true, Ordering::SeqCst);
        assert_eq!(
            judge(0, t(4_200)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "heard again, the evicted instance is refused from the table"
        );
        assert_eq!(
            surface.texts().len(),
            PRESENCE_SURFACED_CAP + 2,
            "and its line is earned a second time, the dedupe having forgotten it"
        );
        presence.live.store(false, Ordering::SeqCst);
        assert_eq!(
            judge(0, t(4_300)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the re-earned line remembers the pair again"
        );
        assert_eq!(
            judge(1, t(4_300)),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "re-remembering instance 0 evicted the then-oldest pair, instance 1"
        );
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");
    }

    /// The owner line is deduped by who is refused, not only by the pair. A stranger
    /// announcing an instance heard under an identity trusted for all destinations spends
    /// the pair's one line and arms the memory; a successor trusted for all destinations
    /// that then presents the instance is refused from that memory and still earns its own
    /// `error:` line, naming it and its destination's remedy, once: presenting again earns
    /// nothing, and the holder's symmetric refusal while the successor's row is live earns
    /// nothing either. A stranger could otherwise spend the line the human needs to see.
    #[test]
    fn a_trusted_presenter_the_presence_rung_refuses_earns_its_own_line_after_a_strangers() {
        let fx = Fixture::new("trust-presence-trusted-presenter-own-line");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let holder = announced("alpha");
        fx.announce(&holder, t(2_000));
        fx.trust_identity(&holder.identity_hash, t(3_000));
        let stranger = announced("alpha");
        fx.announce(&stranger, t(4_000));
        let name_hash = decode_name_hash(&holder.name_hash).unwrap();
        let judge = |peer: &Announced, now| {
            let origin = fx.store.authorize_origin_at(
                &parse_hash(&peer.identity_hash).unwrap(),
                &name_hash,
                now,
            );
            let outcome = match origin.verdict.decision {
                Decision::Allow => KeyChangeOutcome::Served,
                Decision::Refuse => KeyChangeOutcome::Refused,
            };
            assert!(
                fx.store
                    .note_key_change(&peer.identity_hash, &peer.name_hash, outcome, now)
                    .is_empty(),
                "nothing is marked"
            );
            origin.verdict
        };

        assert_eq!(
            judge(&stranger, t(4_500)),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(
            texts[0].starts_with("error: ") && texts[0].contains(&stranger.identity_hash),
            "{}",
            texts[0]
        );

        let successor = announced("alpha");
        fx.announce(&successor, t(4_550));
        fx.trust_identity(&successor.identity_hash, t(4_550));
        let before = fx.file_bytes().unwrap();
        assert_eq!(
            judge(&successor, t(4_600)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        let texts = surface.texts();
        assert_eq!(texts.len(), 2, "{texts:#?}");
        assert!(texts[1].starts_with("error: "), "{}", texts[1]);
        assert!(
            texts[1].contains(&format!(
                "presented under identity {}",
                successor.identity_hash
            )),
            "{}",
            texts[1]
        );
        assert!(
            texts[1].contains(&format!(".mesh trust {}", successor.destination_hash)),
            "{}",
            texts[1]
        );
        assert!(
            texts[1].contains(&format!("heard under identity {}", holder.identity_hash)),
            "{}",
            texts[1]
        );

        assert_eq!(
            judge(&successor, t(4_650)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert_eq!(
            surface.texts().len(),
            2,
            "told once: {:#?}",
            surface.texts()
        );
        assert_eq!(
            judge(&holder, t(4_650)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the holder is refused while the successor's row is live"
        );
        assert_eq!(
            surface.texts().len(),
            2,
            "the holder's symmetric refusal is silent: {:#?}",
            surface.texts()
        );
        assert_eq!(
            judge(&successor, t(4_550) + PEER_TTL),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the stranger's line armed the memory"
        );
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");
        assert!(
            fx.store
                .records()
                .iter()
                .all(|record| record.key_changed.is_none()),
            "{:#?}",
            fx.store.records()
        );
    }

    /// A presence line the surface drops arms no memory: the pair is rolled back, so the
    /// verdict refuses by the live table only while the holder's row is live and admits
    /// once it has aged out, and the next presentation offers the line again. Once a
    /// surface takes it, the line is surfaced once and the memory outlives the row.
    #[test]
    fn a_dropped_presence_line_arms_no_memory_and_is_offered_again() {
        install_log_collector();
        let fx = Fixture::new("trust-presence-dropped-line-no-memory");
        let dropping = Arc::new(DroppingSurface);
        fx.store
            .attach_surface(Arc::downgrade(&dropping) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        fx.announce(&new, t(4_000));
        fx.trust_identity(&new.identity_hash, t(5_000));
        let name_hash = decode_name_hash(&new.name_hash).unwrap();
        let origin = |now| {
            fx.store
                .authorize_origin_at(&parse_hash(&new.identity_hash).unwrap(), &name_hash, now)
                .verdict
        };
        let old_row_gone = t(2_000) + PEER_TTL;

        assert_eq!(
            origin(t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert!(
            fx.store
                .note_key_change(
                    &new.identity_hash,
                    &new.name_hash,
                    KeyChangeOutcome::Refused,
                    t(4_500),
                )
                .is_empty()
        );
        assert!(
            warn_snapshot().iter().any(|line| line
                .contains("was dropped; nothing is marked and it is offered again")),
            "{:#?}",
            warn_snapshot()
        );
        assert_eq!(
            origin(t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the live row still refuses"
        );
        assert_eq!(
            origin(old_row_gone),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "the dropped line armed no memory"
        );
        {
            let state = fx.store.inner.lock();
            assert!(state.presence_surfaced.is_empty());
            assert!(state.presence_surfaced_order.is_empty());
            assert!(state.presence_remembered.is_empty());
            assert!(state.presence_lines_told.is_empty());
        }

        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        assert!(
            fx.store
                .note_key_change(
                    &new.identity_hash,
                    &new.name_hash,
                    KeyChangeOutcome::Refused,
                    t(4_600),
                )
                .is_empty()
        );
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(
            texts[0].starts_with("error: ") && texts[0].contains(&new.identity_hash),
            "{}",
            texts[0]
        );
        assert_eq!(
            origin(old_row_gone),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "surfaced, the line arms the memory"
        );
        assert!(
            fx.store
                .note_key_change(
                    &new.identity_hash,
                    &new.name_hash,
                    KeyChangeOutcome::Refused,
                    t(4_700),
                )
                .is_empty()
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());
    }

    /// Judges `peer` presenting `name_hash` the way every ingress path does: the verdict,
    /// then `note_key_change` with the outcome that verdict earns. A presence-only
    /// collision marks nothing whichever way it goes.
    fn present(fx: &Fixture, peer: &Announced, name_hash: &str, now: SystemTime) -> Verdict {
        let bytes = decode_name_hash(name_hash).unwrap();
        let origin =
            fx.store
                .authorize_origin_at(&parse_hash(&peer.identity_hash).unwrap(), &bytes, now);
        let outcome = match origin.verdict.decision {
            Decision::Allow => KeyChangeOutcome::Served,
            Decision::Refuse => KeyChangeOutcome::Refused,
        };
        assert!(
            fx.store
                .note_key_change(&peer.identity_hash, name_hash, outcome, now)
                .is_empty(),
            "a presence-only collision marks nothing"
        );
        origin.verdict
    }

    /// The destination `peer`'s identity derives for `name_hash`: what the owner line's
    /// `.mesh trust` remedy names.
    fn destination_for(peer: &Announced, name_hash: &str) -> String {
        destination_address(
            &decode_name_hash(name_hash).unwrap(),
            &parse_hash(&peer.identity_hash).unwrap(),
        )
        .to_hex_string()
    }

    /// The own-line dedupe is per refused presenter: two identities trusted for all
    /// destinations that each present an instance held by a third are each told once,
    /// naming themselves and their own destination's remedy, and presenting again earns
    /// neither a second line. A stranger or a destination-tier identity presenting the
    /// instance after them shares the pair's line, which the first detection already
    /// spent, so they add nothing. The memory outlives the holder's row for both refused
    /// keys and never refuses the holder; `trust.yaml` is left byte for byte.
    #[test]
    fn usage_probe_each_trusted_for_all_presenter_the_rung_refuses_is_told_once_and_later_presenters_add_nothing()
     {
        let fx = Fixture::new("trust-probe-presence-own-line-per-presenter");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let holder = announced("alpha");
        fx.announce(&holder, t(2_000));
        fx.trust_identity(&holder.identity_hash, t(3_000));
        // Two more identities trusted for all destinations, heard under their own
        // instances, and one trusted for its own destination only.
        let first = announced("beta");
        fx.announce(&first, t(2_000));
        fx.trust_identity(&first.identity_hash, t(3_000));
        let second = announced("gamma");
        fx.announce(&second, t(2_000));
        fx.trust_identity(&second.identity_hash, t(3_000));
        let destination_tier = announced("delta");
        fx.announce(&destination_tier, t(2_000));
        assert_eq!(
            fx.trust_destination(&destination_tier, t(3_000)),
            TrustChange::Added
        );
        let stranger = announced("alpha");
        let before = fx.file_bytes().unwrap();
        let instance = holder.name_hash.as_str();

        assert_eq!(
            present(&fx, &first, instance, t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(
            texts[0].starts_with("error: ")
                && texts[0].contains(&format!("presented under identity {}", first.identity_hash))
                && texts[0].contains(&format!(
                    ".mesh trust {}",
                    destination_for(&first, instance)
                )),
            "{}",
            texts[0]
        );

        assert_eq!(
            present(&fx, &second, instance, t(4_600)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        let texts = surface.texts();
        assert_eq!(
            texts.len(),
            2,
            "the second refused key earns its own line: {texts:#?}"
        );
        assert!(
            texts[1].starts_with("error: ")
                && texts[1].contains(&format!(
                    "presented under identity {}",
                    second.identity_hash
                ))
                && texts[1].contains(&format!(
                    ".mesh trust {}",
                    destination_for(&second, instance)
                ))
                && !texts[1].contains(&first.identity_hash),
            "{}",
            texts[1]
        );

        assert_eq!(
            present(&fx, &first, instance, t(4_650)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert_eq!(
            present(&fx, &second, instance, t(4_650)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert_eq!(
            surface.texts().len(),
            2,
            "each refused key is told once: {:#?}",
            surface.texts()
        );

        assert_eq!(
            present(&fx, &stranger, instance, t(4_680)),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert_eq!(
            present(&fx, &destination_tier, instance, t(4_680)),
            verdict(Decision::Refuse, Rule::DefaultClosed),
            "a known identity with no allow of its own still knocks for a presence-only instance"
        );
        assert_eq!(
            surface.texts().len(),
            2,
            "a stranger and a destination-tier identity share the pair's spent line: {:#?}",
            surface.texts()
        );

        let holder_row_gone = t(2_000) + PEER_TTL;
        assert_eq!(
            present(&fx, &first, instance, holder_row_gone),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the first refused key stays refused from the memory"
        );
        assert_eq!(
            present(&fx, &second, instance, holder_row_gone),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "and so does the second"
        );
        assert_eq!(
            present(&fx, &holder, instance, holder_row_gone),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "the memory never refuses the holder it names"
        );
        assert_eq!(surface.texts().len(), 2, "{:#?}", surface.texts());
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");
        assert!(
            fx.store
                .records()
                .iter()
                .all(|record| record.key_changed.is_none()),
            "{:#?}",
            fx.store.records()
        );
    }

    /// A refused key's own line that the surface drops is offered again, while the
    /// memory a stranger's surfaced line already armed stands: the roll-back undoes only
    /// what the dropped detection added. Once a surface takes the line it is told once.
    #[test]
    fn usage_probe_a_dropped_own_line_leaves_a_strangers_memory_armed_and_is_offered_again() {
        install_log_collector();
        let fx = Fixture::new("trust-probe-presence-dropped-own-line");
        let recording = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&recording) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let holder = announced("alpha");
        fx.announce(&holder, t(2_000));
        fx.trust_identity(&holder.identity_hash, t(3_000));
        let refused = announced("beta");
        fx.announce(&refused, t(2_000));
        fx.trust_identity(&refused.identity_hash, t(3_000));
        let stranger = announced("alpha");
        let before = fx.file_bytes().unwrap();
        let instance = holder.name_hash.as_str();
        let holder_row_gone = t(2_000) + PEER_TTL;

        assert_eq!(
            present(&fx, &stranger, instance, t(4_500)),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert_eq!(recording.texts().len(), 1, "{:#?}", recording.texts());

        let dropping = Arc::new(DroppingSurface);
        fx.store
            .attach_surface(Arc::downgrade(&dropping) as Weak<dyn KnockSurface>);
        assert_eq!(
            present(&fx, &refused, instance, t(4_600)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert!(
            warn_snapshot().iter().any(|line| line
                .contains("was dropped; nothing is marked and it is offered again")),
            "{:#?}",
            warn_snapshot()
        );
        assert_eq!(
            present(&fx, &refused, instance, holder_row_gone),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the stranger's surfaced line armed the memory; the dropped line did not disarm it"
        );

        fx.store
            .attach_surface(Arc::downgrade(&recording) as Weak<dyn KnockSurface>);
        assert_eq!(
            present(&fx, &refused, instance, t(4_650)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        let texts = recording.texts();
        assert_eq!(
            texts.len(),
            2,
            "the dropped line is offered again: {texts:#?}"
        );
        assert!(
            texts[1].starts_with("error: ")
                && texts[1].contains(&format!(
                    "presented under identity {}",
                    refused.identity_hash
                )),
            "{}",
            texts[1]
        );
        assert_eq!(
            present(&fx, &refused, instance, t(4_680)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert_eq!(
            recording.texts().len(),
            2,
            "told once: {:#?}",
            recording.texts()
        );
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");
    }

    /// A presence line with no surface attached to take it arms no memory either: the
    /// pair and the presenter's own line are rolled back as for a dropped line, so the
    /// verdict admits once the holder's row has aged out, and the line is offered again,
    /// once, when a surface is attached.
    #[test]
    fn a_presence_line_with_nothing_attached_arms_no_memory_either() {
        let fx = Fixture::new("trust-presence-no-surface-no-memory");
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        fx.announce(&new, t(4_000));
        fx.trust_identity(&new.identity_hash, t(5_000));
        let before = fx.file_bytes().unwrap();
        let instance = new.name_hash.as_str();
        let old_row_gone = t(2_000) + PEER_TTL;

        assert_eq!(
            present(&fx, &new, instance, t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the live row refuses"
        );
        {
            let state = fx.store.inner.lock();
            assert!(state.presence_surfaced.is_empty());
            assert!(state.presence_surfaced_order.is_empty());
            assert!(state.presence_remembered.is_empty());
            assert!(state.presence_lines_told.is_empty());
        }
        assert_eq!(
            present(&fx, &new, instance, old_row_gone),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "a line nothing took armed no memory"
        );

        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        assert_eq!(
            present(&fx, &new, instance, t(4_600)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        let texts = surface.texts();
        assert_eq!(texts.len(), 1, "{texts:#?}");
        assert!(
            texts[0].starts_with("error: ") && texts[0].contains(&new.identity_hash),
            "{}",
            texts[0]
        );
        assert_eq!(
            present(&fx, &new, instance, old_row_gone),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "surfaced, the line arms the memory"
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");
    }

    /// A presenter the memory alone refuses still earns its own line: a stranger spent
    /// the pair's line while the holder's row was live, the human then trusted the
    /// stranger for all destinations after the row aged out, and its first refused
    /// presentation names it and `.mesh trust <its destination>` with the holder's
    /// destination derived again, since no row remains. Presenting again adds nothing.
    #[test]
    fn a_presenter_refused_by_memory_alone_still_earns_its_own_line() {
        let fx = Fixture::new("trust-presence-memory-only-own-line");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let holder = announced("alpha");
        fx.announce(&holder, t(2_000));
        fx.trust_identity(&holder.identity_hash, t(3_000));
        let newcomer = announced("alpha");
        let instance = holder.name_hash.as_str();
        let holder_row_gone = t(2_000) + PEER_TTL;

        assert_eq!(
            present(&fx, &newcomer, instance, t(4_500)),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());

        fx.trust_identity(&newcomer.identity_hash, holder_row_gone);
        let before = fx.file_bytes().unwrap();
        assert_eq!(
            present(&fx, &newcomer, instance, holder_row_gone),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the memory refuses with the holder's row gone"
        );
        let texts = surface.texts();
        assert_eq!(
            texts.len(),
            2,
            "the refused key earns its own line from the memory: {texts:#?}"
        );
        assert!(
            texts[1].starts_with("error: ")
                && texts[1].contains(&format!(
                    "presented under identity {}",
                    newcomer.identity_hash
                ))
                && texts[1].contains(&holder.identity_hash)
                && texts[1].contains(&format!(
                    "instance {} was heard",
                    short(&holder.destination_hash)
                ))
                && texts[1].contains(&format!(
                    ".mesh trust {}",
                    destination_for(&newcomer, instance)
                )),
            "{}",
            texts[1]
        );

        assert_eq!(
            present(&fx, &newcomer, instance, holder_row_gone + PEER_TTL),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert_eq!(
            surface.texts().len(),
            2,
            "told once: {:#?}",
            surface.texts()
        );
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");
        assert!(
            fx.store
                .records()
                .iter()
                .all(|record| record.key_changed.is_none()),
            "{:#?}",
            fx.store.records()
        );
    }

    /// A line the memory alone earns, once the holder's row is gone, rolls back only what
    /// its own detection recorded when the surface drops it: the pair a stranger's
    /// surfaced line armed stands, so the refusal holds, and the refused key's own line is
    /// offered again until a surface takes it, then told once, with the holder's
    /// destination derived from the instance and the remembered identity. Nothing is
    /// re-seeded by that line: the memory still names one holder in one order slot. The
    /// holder itself and a fresh stranger presenting the instance after the row is gone
    /// are judged by their grants and add nothing.
    #[test]
    fn usage_probe_a_dropped_memory_only_own_line_keeps_the_memory_and_is_offered_again() {
        install_log_collector();
        let fx = Fixture::new("trust-probe-presence-memory-only-dropped-own-line");
        let recording = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&recording) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let holder = announced("alpha");
        fx.announce(&holder, t(2_000));
        fx.trust_identity(&holder.identity_hash, t(3_000));
        let stranger = announced("alpha");
        let newcomer = announced("alpha");
        let instance = holder.name_hash.as_str();
        let holder_row_gone = t(2_000) + PEER_TTL;

        assert_eq!(
            present(&fx, &stranger, instance, t(4_500)),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert_eq!(recording.texts().len(), 1, "{:#?}", recording.texts());

        fx.trust_identity(&newcomer.identity_hash, holder_row_gone);
        let before = fx.file_bytes().unwrap();
        let dropping = Arc::new(DroppingSurface);
        fx.store
            .attach_surface(Arc::downgrade(&dropping) as Weak<dyn KnockSurface>);
        for at in [holder_row_gone, holder_row_gone + PEER_TTL] {
            assert_eq!(
                present(&fx, &newcomer, instance, at),
                verdict(Decision::Refuse, Rule::IdentityChanged),
                "the memory refuses with the holder's row gone"
            );
            let state = fx.store.inner.lock();
            assert_eq!(
                state.presence_remembered.get(instance),
                Some(&BTreeSet::from([holder.identity_hash.clone()])),
                "the stranger's memory stands"
            );
            assert_eq!(state.presence_surfaced_order.len(), 1);
            assert!(
                !state.presence_lines_told.contains_key(instance),
                "a dropped own line is not told: {:#?}",
                state.presence_lines_told
            );
        }
        assert!(
            warn_snapshot().iter().any(|line| line
                .contains("was dropped; nothing is marked and it is offered again")),
            "{:#?}",
            warn_snapshot()
        );

        fx.store
            .attach_surface(Arc::downgrade(&recording) as Weak<dyn KnockSurface>);
        assert_eq!(
            present(&fx, &newcomer, instance, holder_row_gone + 2 * PEER_TTL),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        let texts = recording.texts();
        assert_eq!(texts.len(), 2, "offered again: {texts:#?}");
        assert!(
            texts[1].starts_with("error: ")
                && texts[1].contains(&format!(
                    "instance {} was heard under identity {}",
                    short(&holder.destination_hash),
                    holder.identity_hash
                ))
                && texts[1].contains(&format!(
                    "presented under identity {}",
                    newcomer.identity_hash
                ))
                && texts[1].contains(&format!(
                    ".mesh trust {}",
                    destination_for(&newcomer, instance)
                ))
                && texts[1].contains(&format!(".mesh block {}", newcomer.identity_hash)),
            "{}",
            texts[1]
        );
        {
            let state = fx.store.inner.lock();
            assert_eq!(
                state.presence_remembered.get(instance),
                Some(&BTreeSet::from([holder.identity_hash.clone()])),
                "nothing is re-seeded"
            );
            assert_eq!(state.presence_surfaced_order.len(), 1);
            assert_eq!(
                state.presence_lines_told.get(instance),
                Some(&BTreeSet::from([newcomer.identity_hash.clone()]))
            );
        }
        assert_eq!(
            present(&fx, &newcomer, instance, holder_row_gone + 3 * PEER_TTL),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert_eq!(recording.texts().len(), 2, "told once");

        assert_eq!(
            present(&fx, &holder, instance, holder_row_gone + 3 * PEER_TTL),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "the memory never refuses the holder it names"
        );
        let late_stranger = announced("alpha");
        assert_eq!(
            present(
                &fx,
                &late_stranger,
                instance,
                holder_row_gone + 3 * PEER_TTL
            ),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert_eq!(
            recording.texts().len(),
            2,
            "the holder and a late stranger add nothing: {:#?}",
            recording.texts()
        );
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");
    }

    /// A surface that was attached and has since gone away is no surface: the line
    /// reaches nobody, so the pair and the own line are rolled back as with nothing
    /// attached, the verdict admits once the row has aged out, and a surface attached
    /// later is offered the line on the next presentation and arms the memory then.
    #[test]
    fn usage_probe_a_surface_that_went_away_takes_no_line_and_arms_no_memory() {
        install_log_collector();
        let fx = Fixture::new("trust-probe-presence-surface-went-away");
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        fx.store.set_collision_protection(true);
        let gone = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&gone) as Weak<dyn KnockSurface>);
        drop(gone);
        let old = announced("alpha");
        fx.announce(&old, t(2_000));
        fx.trust_identity(&old.identity_hash, t(3_000));
        let new = announced("alpha");
        fx.announce(&new, t(4_000));
        fx.trust_identity(&new.identity_hash, t(5_000));
        let before = fx.file_bytes().unwrap();
        let instance = new.name_hash.as_str();
        let old_row_gone = t(2_000) + PEER_TTL;

        assert_eq!(
            present(&fx, &new, instance, t(4_500)),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "the live row refuses"
        );
        {
            let state = fx.store.inner.lock();
            assert!(state.presence_surfaced.is_empty());
            assert!(state.presence_surfaced_order.is_empty());
            assert!(state.presence_remembered.is_empty());
            assert!(state.presence_lines_told.is_empty());
        }
        let dropped = format!("notice for {} was dropped", short(&old.destination_hash));
        assert!(
            !warn_snapshot().iter().any(|line| line.contains(&dropped)),
            "nothing attached is not a drop: {:#?}",
            warn_snapshot()
        );
        assert_eq!(
            present(&fx, &new, instance, old_row_gone),
            verdict(Decision::Allow, Rule::IdentityTrusted),
            "a line nobody could see armed no memory"
        );

        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        assert_eq!(
            present(&fx, &new, instance, t(4_600)),
            verdict(Decision::Refuse, Rule::IdentityChanged)
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());
        assert_eq!(
            present(&fx, &new, instance, old_row_gone),
            verdict(Decision::Refuse, Rule::IdentityChanged),
            "surfaced, the line arms the memory"
        );
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");
    }

    /// With `collision_protection` off the own-line rule is not applied: the pair's one
    /// warning is all the human hears, so once a stranger has spent it an identity trusted
    /// for all destinations presenting the same instance is served and told nothing, and
    /// so is the next such identity; nobody is recorded as told and nothing is written.
    #[test]
    fn usage_probe_with_protection_off_a_trusted_for_all_presenter_after_a_stranger_adds_no_line() {
        let fx = Fixture::new("trust-probe-presence-protection-off-no-own-line");
        let surface = Arc::new(RecordingSurface::default());
        fx.store
            .attach_surface(Arc::downgrade(&surface) as Weak<dyn KnockSurface>);
        fx.store
            .attach_presence(Arc::downgrade(&fx.mesh.0) as Weak<dyn InstancePresence>);
        let holder = announced("alpha");
        fx.announce(&holder, t(2_000));
        fx.trust_identity(&holder.identity_hash, t(3_000));
        let first = announced("beta");
        fx.announce(&first, t(2_000));
        fx.trust_identity(&first.identity_hash, t(3_000));
        let second = announced("gamma");
        fx.announce(&second, t(2_000));
        fx.trust_identity(&second.identity_hash, t(3_000));
        let stranger = announced("alpha");
        let before = fx.file_bytes().unwrap();
        let instance = holder.name_hash.as_str();

        assert_eq!(
            present(&fx, &stranger, instance, t(4_500)),
            verdict(Decision::Refuse, Rule::DefaultClosed)
        );
        assert_eq!(surface.texts().len(), 1, "{:#?}", surface.texts());

        for (peer, at) in [(&first, t(4_600)), (&second, t(4_650)), (&first, t(4_690))] {
            assert_eq!(
                present(&fx, peer, instance, at),
                verdict(Decision::Allow, Rule::IdentityTrusted),
                "protection off serves"
            );
            assert_eq!(
                surface.texts().len(),
                1,
                "one line per pair, no own line: {:#?}",
                surface.texts()
            );
        }
        {
            let state = fx.store.inner.lock();
            assert!(
                state.presence_lines_told.is_empty(),
                "{:#?}",
                state.presence_lines_told
            );
            assert_eq!(
                state.presence_remembered.get(instance),
                Some(&BTreeSet::from([holder.identity_hash.clone()]))
            );
        }
        assert_eq!(fx.file_bytes().unwrap(), before, "judging writes nothing");
    }
}
