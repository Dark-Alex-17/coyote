use crate::mesh::announce::is_control_or_invisible;
use crate::mesh::node::MeshSlot;
use crate::mesh::peers::{PeerRecord, PeerTable};
use crate::mesh::{canonical_hash, mesh_config_dir, parse_rfc3339, rfc3339_utc, write_atomically};

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use rns_transport::hash::{AddressHash, Hash};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::Digest;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const TRUST_FILE_VERSION: u64 = 1;

const MESH_OFF: &str = "Mesh is off, so the trust list cannot be changed. Run `.mesh on` first.";

/// What a trust mutation needs from a running mesh: proof it is on, and the peer table
/// that turns a destination into its announced identity.
pub(crate) trait LiveMesh {
    /// `None` while the mesh is off.
    fn peers(&self) -> Option<Arc<PeerTable>>;
}

impl LiveMesh for MeshSlot {
    fn peers(&self) -> Option<Arc<PeerTable>> {
        self.get().map(|runtime| runtime.peers())
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

#[derive(Deserialize)]
struct VersionProbe {
    version: u64,
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

/// The user's trust list, mirrored to `<config_dir>/mesh/trust.yaml`.
///
/// Only the mutation methods write the file, and each of them needs a running mesh: a
/// destination is trusted by proving, from the peer table's copy of its announce, which
/// identity derived it. Queries never touch the disk. `mark_seen` and session trust live in
/// memory only, so `last_seen_at` on disk is as of the last mutation that touched a record
/// while `records` reports the fresher in-memory value; nothing calls `mark_seen` yet.
///
/// Nothing here removes a record on its own: removal is `untrust_*`, `block_identity` and
/// `prune_destinations`, all at the user's request.
#[derive(Debug)]
pub(crate) struct TrustStore {
    path: PathBuf,
    inner: Mutex<State>,
}

// Reached by the REPL mesh commands and the dispatcher once they land.
#[allow(dead_code)]
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
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The one place the precedence lives: destination deny, then destination allow, then
    /// identity allow, then default-closed. A blocked identity is refused before either
    /// allow and reported under its own rule. A destination record only allows the identity
    /// it was proven to belong to.
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
            .is_some_and(|entry| entry.identity == identity);
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
    /// `deny` is holding back; a deny with no record of its own is listed too, so the
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
                }),
        );
        records
    }

    pub(crate) fn blocked(&self) -> Vec<OverlayRecord> {
        overlay_records(&self.inner.lock().file.blocked_identities)
    }

    pub(crate) fn denied(&self) -> Vec<OverlayRecord> {
        overlay_records(&self.inner.lock().file.denied_destinations)
    }

    /// Notes a fresh sighting of a trusted destination in memory only; the file keeps the
    /// value from the last mutation so announces never cause writes. A destination without
    /// a record is ignored, so announces cannot grow this map either.
    pub(crate) fn mark_seen(&self, destination_hash: &str, at: SystemTime) {
        let destination = destination_hash.to_ascii_lowercase();
        let mut state = self.inner.lock();
        if !state.file.destinations.contains_key(&destination)
            && !state.session_destinations.contains_key(&destination)
        {
            return;
        }
        state
            .seen
            .entry(destination)
            .and_modify(|seen| *seen = (*seen).max(at))
            .or_insert(at);
    }

    /// Trusts one destination by proving which identity announced it, and makes sure that
    /// identity has a record (without `all_destinations`, which only `trust_identity` sets).
    /// The identity comes from the announce's own hashes, never from the caller.
    pub(crate) fn trust_destination(
        &self,
        mesh: &dyn LiveMesh,
        destination_hash: &str,
        opts: TrustOptions,
        now: SystemTime,
    ) -> Result<TrustOutcome> {
        let peers = live(mesh)?;
        check_options(&opts)?;
        let peer = resolve_destination(&peers, destination_hash)?;
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
                    },
                );
                TrustChange::Added
            }
        };
        file.identities
            .entry(peer.identity_hash.clone())
            .or_insert_with(|| identity_entry(now, peer.last_seen, false));
        self.commit(
            &mut state,
            file,
            &format!("trust destination {}", peer.destination_hash),
        )?;
        // Both now live on disk, so a session twin would list the same hash twice.
        state.session_destinations.remove(&peer.destination_hash);
        state.session_identities.remove(&peer.identity_hash);
        Ok(TrustOutcome {
            identity_hash: peer.identity_hash,
            destination_hash: peer.destination_hash,
            change,
        })
    }

    /// Same proof as `trust_destination`, kept in memory only: the pair is forgotten when
    /// the process ends and never reaches `trust.yaml`. A hash already on disk gets no
    /// session twin: the disk record is the stronger one and lists once.
    pub(crate) fn trust_destination_for_session(
        &self,
        mesh: &dyn LiveMesh,
        destination_hash: &str,
        now: SystemTime,
    ) -> Result<TrustOutcome> {
        let peers = live(mesh)?;
        let peer = resolve_destination(&peers, destination_hash)?;
        let mut state = self.inner.lock();
        refuse_if_blocked(&state.file, &peer.identity_hash)?;
        if state.file.destinations.contains_key(&peer.destination_hash) {
            return Ok(TrustOutcome {
                identity_hash: peer.identity_hash,
                destination_hash: peer.destination_hash,
                change: TrustChange::Updated,
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
        })
    }

    /// Trusts every destination of an identity (`--identity`): the record's
    /// `all_destinations` flag is set, or the record is created with it set. Only peer rows
    /// whose destination the identity provably derives count towards `last_seen_at`; the
    /// identity column alone is a claim.
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
        let change = match file.identities.get_mut(&identity) {
            Some(entry) => {
                entry.all_destinations = true;
                if let Some(seen) = last_seen {
                    entry.last_seen_at = entry.last_seen_at.max(Stamp::new(seen));
                }
                apply_options(&mut entry.label, &mut entry.note, opts);
                TrustChange::Updated
            }
            None => {
                let mut entry = identity_entry(now, last_seen.unwrap_or(now), true);
                entry.label = opts.label;
                entry.note = opts.note;
                file.identities.insert(identity.clone(), entry);
                TrustChange::Added
            }
        };
        self.commit(&mut state, file, &format!("trust identity {identity}"))?;
        Ok(change)
    }

    /// Removes one destination record; the identity record stays.
    pub(crate) fn untrust_destination(
        &self,
        mesh: &dyn LiveMesh,
        destination_hash: &str,
    ) -> Result<()> {
        live(mesh)?;
        let destination = normalize_hash(destination_hash);
        let mut state = self.inner.lock();
        let mut file = state.file.clone();
        let on_disk = file.destinations.remove(&destination).is_some();
        let in_session = state.session_destinations.contains_key(&destination);
        if !on_disk && !in_session {
            bail!(
                "Destination {destination} is not in the trust list, so there is nothing to untrust."
            );
        }
        if on_disk {
            self.commit(
                &mut state,
                file,
                &format!("untrust destination {destination}"),
            )?;
        }
        state.forget_destination(&destination);
        Ok(())
    }

    /// Removes an identity record and every destination bound to it; returns the removed
    /// destination hashes.
    pub(crate) fn untrust_identity(
        &self,
        mesh: &dyn LiveMesh,
        identity_hash: &str,
    ) -> Result<Vec<String>> {
        live(mesh)?;
        let identity = normalize_hash(identity_hash);
        let mut state = self.inner.lock();
        let mut file = state.file.clone();
        let on_disk = file.identities.remove(&identity).is_some();
        let in_session = state.session_identities.contains_key(&identity);
        if !on_disk && !in_session {
            bail!("Identity {identity} is not in the trust list, so there is nothing to untrust.");
        }
        let removed = remove_destinations_of(&mut file, &identity);
        if on_disk || !removed.is_empty() {
            self.commit(
                &mut state,
                file,
                &format!(
                    "untrust identity {identity} (+{} destinations)",
                    removed.len()
                ),
            )?;
        }
        state.forget_identity(&identity, &removed);
        Ok(removed)
    }

    /// Suppresses an identity's knocks and drops its trust records, on disk and for the
    /// session; returns the removed destination hashes. Blocking is the one mutation that
    /// works on an identity the trust list has never seen.
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
        file.identities.remove(&identity);
        let removed = remove_destinations_of(&mut file, &identity);
        upsert_overlay(&mut file.blocked_identities, identity.clone(), note, now);
        self.commit(
            &mut state,
            file,
            &format!(
                "block identity {identity} (+{} destinations)",
                removed.len()
            ),
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
        self.commit(&mut state, file, &format!("unblock identity {identity}"))
    }

    /// Refuses one destination even when its identity is trusted. The destination's own
    /// trust record, if any, is kept so the user sees what the deny overrides.
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
        self.commit(&mut state, file, &format!("deny {destination}"))
    }

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
        self.commit(&mut state, file, &format!("undeny {destination}"))
    }

    /// Destination records not seen for `older_than`, judged on the fresher of the disk and
    /// in-memory `last_seen_at`; removed unless `dry_run`. Identities are never pruned: they
    /// are the user's statement about a person, not about an instance that may be gone.
    pub(crate) fn prune_destinations(
        &self,
        mesh: &dyn LiveMesh,
        older_than: Duration,
        now: SystemTime,
        dry_run: bool,
    ) -> Result<Vec<String>> {
        live(mesh)?;
        let mut state = self.inner.lock();
        let stale: Vec<String> = state
            .file
            .destinations
            .iter()
            .filter(|(hash, entry)| {
                // A sighting in the future (clock stepped back) reads as just seen.
                now.duration_since(state.effective_last_seen(hash, entry))
                    .unwrap_or_default()
                    >= older_than
            })
            .map(|(hash, _)| hash.clone())
            .collect();
        if dry_run || stale.is_empty() {
            return Ok(stale);
        }
        let mut file = state.file.clone();
        for hash in &stale {
            file.destinations.remove(hash);
        }
        self.commit(
            &mut state,
            file,
            &format!("prune {} destinations", stale.len()),
        )?;
        for hash in &stale {
            state.seen.remove(hash);
        }
        Ok(stale)
    }

    /// Writes `file` and only then makes it the current list, so a failed write leaves
    /// memory and disk agreeing on the previous state.
    fn commit(&self, state: &mut State, file: TrustFile, what: &str) -> Result<()> {
        self.persist(&file)?;
        state.file = file;
        debug!("Mesh trust list updated: {what}");
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
}

/// Reads the version alone first so a file from a newer Coyote gets a precise message
/// rather than an unknown-field error from whatever the newer layout added.
fn parse_trust_file(path: &Path, text: &str) -> Result<TrustFile> {
    let probe: VersionProbe = serde_yaml::from_str(text).with_context(|| {
        format!(
            "Mesh trust list '{}' has no readable `version` field. Fix the file, or move it aside to start a fresh trust list.",
            path.display()
        )
    })?;
    if probe.version != TRUST_FILE_VERSION {
        bail!(
            "Mesh trust list '{}' is version {} but this Coyote reads version {TRUST_FILE_VERSION}. If it was written by a newer Coyote, upgrade Coyote; otherwise move the file aside to start a fresh trust list.",
            path.display(),
            probe.version
        );
    }
    let file: TrustFile = serde_yaml::from_str(text).with_context(|| {
        format!(
            "Mesh trust list '{}' could not be parsed. Fix the file, or move it aside to start a fresh trust list.",
            path.display()
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

fn normalize_hash(input: &str) -> String {
    input.trim().to_ascii_lowercase()
}

/// The lower-hex form of a hash the user typed, refused unless it is a well-formed address
/// hash so a typo cannot become a key in the file.
fn valid_hash(what: &str, input: &str) -> Result<String> {
    canonical_hash(&normalize_hash(input))
        .ok_or_else(|| anyhow!("'{input}' is not a valid {what} hash: expected 32 hex characters."))
}

fn parse_hash(text: &str) -> Option<AddressHash> {
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
    last_seen: SystemTime,
}

/// Finds the announce behind `destination_hash` in the peer table and proves the identity
/// it recorded derives that destination.
fn resolve_destination(peers: &PeerTable, destination_hash: &str) -> Result<ResolvedPeer> {
    let destination = normalize_hash(destination_hash);
    let Some(record) = peers.get(&destination) else {
        bail!(
            "Destination {destination} is not in the peer table. Run `.mesh peers` to see the nodes that have announced, and trust one of those."
        );
    };
    let identity = verified_identity(&record)?;
    Ok(ResolvedPeer {
        destination_hash: destination,
        identity_hash: identity.to_hex_string(),
        last_seen: record.last_seen,
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
    let name_hash = decode_hex(&record.name_hash).ok_or_else(|| {
        anyhow!("The peer table holds an unreadable name hash for {destination}; wait for its next announce.")
    })?;
    let identity = parse_hash(&record.identity_hash).ok_or_else(|| {
        anyhow!("The peer table holds an unreadable identity hash for {destination}; wait for its next announce.")
    })?;
    let claimed = parse_hash(destination).ok_or_else(|| {
        anyhow!("'{destination}' is not a destination hash: expected 32 hex characters.")
    })?;
    let expected = expected_destination(&name_hash, &identity);
    if expected != claimed {
        bail!(
            "Destination {destination} does not match identity {} in the peer table (that identity would announce {}); nothing was trusted.",
            identity.to_hex_string(),
            expected.to_hex_string()
        );
    }
    Ok(identity)
}

/// Reticulum's destination derivation: the address hash is the truncated SHA-256 of the
/// name hash followed by the identity's address hash.
fn expected_destination(name_hash: &[u8], identity: &AddressHash) -> AddressHash {
    AddressHash::new_from_hash(&Hash::new(
        Hash::generator()
            .chain_update(name_hash)
            .chain_update(identity.as_slice())
            .finalize()
            .into(),
    ))
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;
    use crate::mesh::hex_lower;
    use crate::mesh::peers::{PEER_TTL, PeerSighting};

    use rand_core::OsRng;
    use rns_transport::destination::{DestinationName, SingleInputDestination};
    use rns_transport::identity::PrivateIdentity;

    struct MeshOff;

    impl LiveMesh for MeshOff {
        fn peers(&self) -> Option<Arc<PeerTable>> {
            None
        }
    }

    struct MeshOn(Arc<PeerTable>);

    impl LiveMesh for MeshOn {
        fn peers(&self) -> Option<Arc<PeerTable>> {
            Some(self.0.clone())
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
        let identity = PrivateIdentity::new_from_rand(OsRng);
        let name = DestinationName::new("coyote", &format!("mesh.{aspect}"));
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
                mesh: MeshOn(Arc::new(peers)),
                tmp,
            }
        }

        fn announce(&self, peer: &Announced, at: SystemTime) {
            self.mesh.0.observe(sighting(peer), at);
        }

        fn file_bytes(&self) -> Option<Vec<u8>> {
            fs::read(self.store.path()).ok()
        }

        fn reopen(&self) -> TrustStore {
            TrustStore::open(&self.tmp.path).unwrap()
        }
    }

    fn fake_hash(fill: u8) -> String {
        hex_lower(&[fill; 16])
    }

    fn verdict(decision: Decision, rule: Rule) -> Verdict {
        Verdict { decision, rule }
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
    fn trust_destination_of_unknown_peer_points_at_mesh_peers() {
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
        assert!(err.contains(&fake_hash(0x9f)), "{err}");
        assert_eq!(fx.file_bytes(), None);
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
            "version: 1\n\
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
        fs::write(&path, "version: 2\nidentities: {}\nfuture_section: {}\n").unwrap();

        let err = TrustStore::open(&tmp.path).unwrap_err().to_string();

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("version 2"), "{err}");
        assert!(err.contains("upgrade Coyote"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
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
    }

    #[test]
    fn open_refuses_unknown_fields_and_bad_timestamps_rather_than_partially_loading() {
        let tmp = TempDir::new("trust-strict");
        let path = mesh_config_dir(&tmp.path).join("trust.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let identity = fake_hash(0x1a);
        let good = format!(
            "version: 1\nidentities:\n  {identity}:\n    added_at: 2026-01-01T00:00:00Z\n    last_seen_at: 2026-01-01T00:00:00Z\n    label: null\n    note: null\n    all_destinations: true\n"
        );
        fs::write(&path, &good).unwrap();
        assert_eq!(TrustStore::open(&tmp.path).unwrap().records().len(), 1);

        fs::write(&path, format!("{good}    tier: identity\n")).unwrap();
        let err = format!("{:#}", TrustStore::open(&tmp.path).unwrap_err());
        assert!(err.contains("could not be parsed"), "{err}");
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
                "version: 1\ndenied_destinations:\n  {upper}:\n    added_at: 2026-01-01T00:00:00Z\n    note: null\n"
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
                "version: 1\ndestinations:\n  {}:\n    identity: {}\n    added_at: 2026-01-01T00:00:00Z\n    last_seen_at: 2026-01-01T00:00:00Z\n    label: null\n    note: null\n",
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
                "version: 1\ndestinations:\n  {destination}:\n    identity: {missing}\n    added_at: 2026-01-01T00:00:00Z\n    last_seen_at: 2026-01-01T00:00:00Z\n    label: null\n    note: null\n"
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
        fx.store.mark_seen(&first.destination_hash, t(9_000));

        fx.store
            .untrust_destination(&fx.mesh, &format!(" {}", first.destination_hash))
            .unwrap();
        fx.store
            .untrust_destination(&fx.mesh, &session.destination_hash)
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
                .untrust_destination(&fx.mesh, &first.destination_hash)
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
        fx.store.mark_seen(&first.destination_hash, t(9_000));

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
        fx.store.mark_seen(&stale.destination_hash, t(9_000));
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
        fx.store.mark_seen(&revived.destination_hash, t(5_000));
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
        store.mark_seen(&peer.destination_hash, t(9_000));
        store.mark_seen(&peer.destination_hash, t(8_000));

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
                store.untrust_destination(&mesh, &dest),
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
}
