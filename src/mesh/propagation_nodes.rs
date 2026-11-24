//! The LXMF propagation nodes this node has heard announce, and which of them a fetch
//! should ask. Mirrored to `<cache_dir>/mesh/propagation-nodes.json` so a sync right
//! after a restart finds the node it used before, rather than waiting out the node's
//! announce interval. Each record keeps the node's public key, and its destination is
//! rebuilt from that key on load, so a record whose key does not derive its stored hash is
//! dropped rather than trusted.

use crate::mesh::peers::PeerChange;
use crate::mesh::propagation::{
    MAX_ACCEPTED_STAMP_COST, PropagationNode, PropagationNodeError, propagation_node_destination,
};
use crate::mesh::propagation_fetch::FetchError;
use crate::mesh::request::{redact_hashes, short};
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_cause, version_refusal};
use crate::mesh::write_atomically_owner_only;

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use rns_transport::destination::DestinationDesc;
use rns_transport::identity::Identity;
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// Upper bound on remembered propagation nodes; the least recently heard is evicted first.
/// A mesh runs a handful of nodes, not hundreds: every one stores every message for every
/// recipient it serves, so operators deploy few of them, and a fetch only ever asks one.
pub(crate) const PROPAGATION_NODE_TABLE_MAX_ENTRIES: usize = 32;

/// A node not heard announcing for this long is never picked, and is dropped when the table
/// loads. lxmd announces a propagation node every `announce_interval` minutes, 360 in the
/// config it writes by default, so this is four of those: a node that missed three announces
/// in a row reads as gone, and a dead nearby node stops costing every sync a path wait.
pub(crate) const PROPAGATION_NODE_TTL: Duration = Duration::from_secs(4 * 360 * 60);

pub(crate) const PROPAGATION_NODE_TABLE_VERSION: u64 = 1;

pub(crate) const PROPAGATION_NODE_TABLE_FILE: &str = "propagation-nodes.json";
/// The shortest gap between two rounds of path requests for the configured propagation
/// nodes, and how often a round runs while one is not filed: six path-request waits, so a
/// listed node that never answers costs one request per 30 s, not one per post.
pub(crate) const CONFIGURED_NODE_ASK_GAP: Duration =
    Duration::from_secs(crate::mesh::announce::PATH_REQUEST_TIMEOUT.as_secs() * 6);

/// `propagation-nodes.json` whole: the version first, then the nodes.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PropagationNodeFile {
    pub version: u64,
    pub nodes: Vec<StoredNode>,
}

/// One remembered node. A field may be added inside a version only with a default, so an
/// older file still loads. A field this build does not know makes the file unreadable, so
/// it is set aside as corrupt; only a file of another version is refused and left in place.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredNode {
    pub destination_hash: String,
    /// Lower hex as `Identity::to_hex_string` writes it.
    pub public_key: String,
    pub hops: u8,
    pub last_heard: SystemTime,
    pub stamp_cost: u32,
    pub per_transfer_limit_kb: u64,
    pub propagation_enabled: bool,
}

impl StoredNode {
    fn of(record: &PropagationNodeRecord) -> Self {
        Self {
            destination_hash: record.node.destination.address_hash.to_hex_string(),
            public_key: record.node.destination.identity.to_hex_string(),
            hops: record.hops,
            last_heard: record.last_seen,
            stamp_cost: record.node.stamp_cost,
            per_transfer_limit_kb: record.node.per_transfer_limit_kb,
            propagation_enabled: record.node.propagation_enabled,
        }
    }

    /// The record with its destination rebuilt from the key; `None` when the key is not
    /// one or does not derive the stored hash.
    fn into_record(self) -> Option<PropagationNodeRecord> {
        let Ok(identity) = Identity::new_from_hex_string(&self.public_key) else {
            debug!(
                "Dropped stored propagation node {}: its public key is not a key",
                short(&self.destination_hash)
            );
            return None;
        };
        let destination = propagation_node_destination(identity);
        if destination.address_hash.to_hex_string() != self.destination_hash {
            debug!(
                "Dropped stored propagation node {}: its public key derives {}",
                short(&self.destination_hash),
                short(&destination.address_hash.to_hex_string())
            );
            return None;
        }
        Some(PropagationNodeRecord {
            node: PropagationNode {
                destination,
                stamp_cost: self.stamp_cost,
                per_transfer_limit_kb: self.per_transfer_limit_kb,
                propagation_enabled: self.propagation_enabled,
            },
            hops: self.hops,
            last_seen: self.last_heard,
        })
    }
}

enum NodeFileParse {
    Version(u64),
    Corrupt(String),
}

#[derive(Clone)]
pub(crate) struct PropagationNodeRecord {
    pub node: PropagationNode,
    pub hops: u8,
    pub last_seen: SystemTime,
}

impl PropagationNodeRecord {
    /// Not heard announcing for `PROPAGATION_NODE_TTL` at `now`, so never picked.
    pub(crate) fn is_expired(&self, now: SystemTime) -> bool {
        // A `last_seen` in the future (clock stepped back) reads as just heard, not as expired.
        now.duration_since(self.last_seen).unwrap_or_default() >= PROPAGATION_NODE_TTL
    }
}

/// When a save is attempted, which decides what a failed one costs.
#[derive(Clone, Copy)]
pub(crate) enum PersistTrigger {
    Timer,
    Stop,
}

/// Keyed by destination hash and mirrored to `path` as JSON. Writes and `load` take the time
/// so ordering is testable without a clock; the pickers read the clock, so no caller can
/// pick a node that has aged out.
pub(crate) struct PropagationNodeTable {
    path: PathBuf,
    inner: Mutex<BTreeMap<String, PropagationNodeRecord>>,
    /// Set by every change since the last write, so a burst of announces costs one write.
    dirty: AtomicBool,
    /// The owner's configured nodes, lower case, in config order (`pin`).
    pinned: Mutex<Vec<String>>,
    /// Which pinned node the next fetch asks, modulo how many are filed.
    fetch_turn: AtomicUsize,
}

impl PropagationNodeTable {
    /// An empty table with nowhere it is ever written.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::empty(PathBuf::new())
    }

    fn empty(path: PathBuf) -> Self {
        Self {
            path,
            inner: Mutex::new(BTreeMap::new()),
            dirty: AtomicBool::new(false),
            pinned: Mutex::new(Vec::new()),
            fetch_turn: AtomicUsize::new(0),
        }
    }

    /// `load_pinned` with nothing pinned.
    #[cfg(test)]
    pub(crate) fn load(path: PathBuf, now: SystemTime) -> Result<Self> {
        Self::load_pinned(path, now, &[])
    }

    /// Loads the table at `path`, dropping nodes not heard for `PROPAGATION_NODE_TTL` at
    /// `now` and keeping, of the rest, every node in `pinned` (lower case) and the
    /// `PROPAGATION_NODE_TABLE_MAX_ENTRIES` most recently heard others besides; the table
    /// is marked with `pinned` as `pin` does. A missing file is an empty table. A file
    /// whose version is readable but not `PROPAGATION_NODE_TABLE_VERSION` is refused and
    /// left in place, since the
    /// remedy depends on which Coyote wrote it; one that cannot be read or parsed is moved
    /// aside to `<path>.corrupt` with a warning and the table starts empty, since nodes
    /// announce again and must never keep the mesh from starting.
    pub(crate) fn load_pinned(path: PathBuf, now: SystemTime, pinned: &[String]) -> Result<Self> {
        let stored = match fs::read(&path) {
            Ok(bytes) => match parse_node_file(&bytes) {
                Ok(stored) => stored,
                Err(NodeFileParse::Version(found)) => bail!(version_refusal(
                    "propagation node table",
                    &path,
                    None,
                    found,
                    PROPAGATION_NODE_TABLE_VERSION,
                    Remedy::Cache
                )),
                Err(NodeFileParse::Corrupt(what)) => set_aside_corrupt(&path, what),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => set_aside_corrupt(&path, format!("could not be read: {err}")),
        };
        let mut records: Vec<PropagationNodeRecord> = stored
            .into_iter()
            .filter_map(StoredNode::into_record)
            .filter(|record| {
                let expired = record.is_expired(now);
                if expired {
                    debug!(
                        "Dropped stored propagation node {}: not heard announcing for {} hours",
                        short(&record.node.destination.address_hash.to_hex_string()),
                        PROPAGATION_NODE_TTL.as_secs() / 3600
                    );
                }
                !expired
            })
            .collect();
        records.sort_by_key(|record| Reverse(record.last_seen));
        let is_pinned = |record: &PropagationNodeRecord| {
            pinned.contains(&record.node.destination.address_hash.to_hex_string())
        };
        let (kept, rest): (Vec<_>, Vec<_>) =
            records.into_iter().partition(|record| is_pinned(record));
        let records = kept
            .into_iter()
            .chain(rest.into_iter().take(PROPAGATION_NODE_TABLE_MAX_ENTRIES));
        let table = Self::empty(path);
        table.pin(pinned);
        table.inner.lock().extend(
            records.map(|record| (record.node.destination.address_hash.to_hex_string(), record)),
        );
        Ok(table)
    }

    /// Writes the table only if it changed since the last write; the flag is cleared before
    /// the snapshot is taken so a change mid-write is not lost, and restored on failure.
    pub(crate) fn persist_if_dirty(&self) -> Result<()> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        self.persist()
            .inspect_err(|_| self.dirty.store(true, Ordering::Release))
    }

    /// `persist_if_dirty`, warning instead of failing: the table is cache, so a failed
    /// save costs only the nodes heard since the last one, and the timer retries it because
    /// the dirty flag stays set.
    pub(crate) fn persist_if_dirty_or_warn(&self, trigger: PersistTrigger) {
        let Err(err) = self.persist_if_dirty() else {
            return;
        };
        let cost = match trigger {
            PersistTrigger::Timer => {
                "The next periodic save tries again; until one succeeds, a restart forgets the propagation nodes heard since the last save"
            }
            PersistTrigger::Stop => {
                "The next start will not know the propagation nodes heard since the last save until they announce again"
            }
        };
        warn!(
            "Could not save the mesh propagation node table to '{}': {}. {cost}. Check that '{}' is writable and has space.",
            self.path.display(),
            redact_hashes(&format!("{err:#}")),
            self.path.parent().unwrap_or(self.path.as_path()).display()
        );
    }

    fn persist(&self) -> Result<()> {
        let file = PropagationNodeFile {
            version: PROPAGATION_NODE_TABLE_VERSION,
            nodes: self.snapshot().iter().map(StoredNode::of).collect(),
        };
        let json = serde_json::to_vec_pretty(&file)
            .context("Failed to serialize the mesh propagation node table")?;
        write_atomically_owner_only(&self.path, &json)
    }

    /// Files one received announce when it is a propagation node's. Returns `false` for
    /// every announce that is not an `lxmf.propagation` one, so the caller can go on to
    /// read it as something else; a propagation announce that does not parse is dropped
    /// with a debug line and still counts as handled.
    /// A path response (an announce answering this node's path request, or a shared
    /// instance replaying what it knows) says where a node is, not that it is there: for a
    /// node already in the table it refreshes the record and the hops but not `last_seen`,
    /// so a node heard only that way ages out after `PROPAGATION_NODE_TTL` like one gone
    /// quiet. An unknown node files as heard.
    pub(crate) fn observe_announce(
        &self,
        destination: &DestinationDesc,
        app_data: &[u8],
        hops: u8,
        is_path_response: bool,
        now: SystemTime,
    ) -> bool {
        let destination_hex = destination.address_hash.to_hex_string();
        match PropagationNode::from_announce(destination, app_data) {
            Ok(node) => {
                let change = if is_path_response {
                    self.refresh_route(node, hops, now)
                } else {
                    self.observe(node, hops, now)
                };
                debug!(
                    "{} propagation node {} ({hops} hops)",
                    match change {
                        PeerChange::Added => "Added",
                        PeerChange::Refreshed => "Refreshed",
                    },
                    short(&destination_hex)
                );
                true
            }
            Err(PropagationNodeError::NotAPropagationNode) => false,
            Err(err) => {
                debug!(
                    "Ignored propagation node announce from {} ({hops} hops): {}",
                    short(&destination_hex),
                    redact_hashes(&err.to_string())
                );
                true
            }
        }
    }

    /// Files the announce that answered this node's own path request for a configured
    /// propagation node, by the path-response rules: a relay may answer from a copy it
    /// stored, so a node not on file is filed as first heard at `now` and ages out as
    /// any other, and one on file only has its route refreshed, its `last_seen` kept, so
    /// an aged-out record is never revived by it. Whether the node is filed and current
    /// afterwards; why it was not filed otherwise.
    pub(crate) fn file_configured(
        &self,
        destination: &DestinationDesc,
        app_data: &[u8],
        hops: u8,
        now: SystemTime,
    ) -> Result<bool, PropagationNodeError> {
        let node = PropagationNode::from_announce(destination, app_data)?;
        let hash = node.destination.address_hash.to_hex_string();
        self.refresh_route(node, hops, now);
        Ok(self.holds(&hash, now))
    }

    /// Files a path response: a known node keeps its `last_seen`, anything else is
    /// observed as a first sighting.
    fn refresh_route(&self, node: PropagationNode, hops: u8, now: SystemTime) -> PeerChange {
        let key = node.destination.address_hash.to_hex_string();
        {
            let mut nodes = self.inner.lock();
            if let Some(record) = nodes.get_mut(&key) {
                record.node = node;
                record.hops = hops;
                self.dirty.store(true, Ordering::Release);
                return PeerChange::Refreshed;
            }
        }
        self.observe(node, hops, now)
    }

    pub(crate) fn observe(&self, node: PropagationNode, hops: u8, now: SystemTime) -> PeerChange {
        let key = node.destination.address_hash.to_hex_string();
        let pinned = self.pinned.lock().clone();
        let mut nodes = self.inner.lock();
        let change = if nodes.contains_key(&key) {
            PeerChange::Refreshed
        } else {
            PeerChange::Added
        };
        nodes.insert(
            key,
            PropagationNodeRecord {
                node,
                hops,
                last_seen: now,
            },
        );
        // The cap counts the nodes heard, not the owner's pinned ones: those are never
        // evicted and never take a heard node's place.
        let mut heard: Vec<(SystemTime, String)> = nodes
            .iter()
            .filter(|(key, _)| !pinned.contains(*key))
            .map(|(key, record)| (record.last_seen, key.clone()))
            .collect();
        let excess = heard
            .len()
            .saturating_sub(PROPAGATION_NODE_TABLE_MAX_ENTRIES);
        heard.sort();
        for (_, oldest) in heard.into_iter().take(excess) {
            nodes.remove(&oldest);
            debug!("Evicted propagation node {}", short(&oldest));
        }
        self.dirty.store(true, Ordering::Release);
        change
    }

    /// Marks `hashes` (lower case) as the owner's pinned nodes: never evicted, and while
    /// one is filed the only nodes posted to and fetched from.
    pub(crate) fn pin(&self, hashes: &[String]) {
        *self.pinned.lock() = hashes.to_vec();
    }

    /// The pinned nodes on file and not aged out at `now`, in the order the owner lists
    /// them.
    fn filed_pinned(&self, now: SystemTime) -> Vec<PropagationNodeRecord> {
        let pinned = self.pinned.lock().clone();
        let nodes = self.inner.lock();
        pinned
            .iter()
            .filter_map(|hash| nodes.get(hash))
            .filter(|record| !record.is_expired(now))
            .cloned()
            .collect()
    }

    /// How many nodes a full round of fetches asks: every pinned node on file, else one.
    pub(crate) fn fetch_rotation(&self) -> usize {
        self.filed_pinned(SystemTime::now()).len().max(1)
    }

    /// The pinned nodes on file and current, in the order the owner lists them.
    pub(crate) fn pinned_nodes(&self) -> Vec<PropagationNode> {
        self.filed_pinned(SystemTime::now())
            .into_iter()
            .map(|record| record.node)
            .collect()
    }

    /// Moves the fetch rotation on to the next pinned node.
    pub(crate) fn advance_fetch_turn(&self) {
        self.fetch_turn.fetch_add(1, Ordering::AcqRel);
    }

    /// The node a fetch should ask. While any pinned node is filed, the pinned node whose
    /// turn it is, so fetches rotate over every one of them (`advance_fetch_turn` moves the
    /// turn on); another filed node is never asked then. Otherwise the nearest by hops,
    /// the most recently heard breaking a tie, among nodes heard within
    /// `PROPAGATION_NODE_TTL`. Neither the enabled flag nor the announced cost is
    /// consulted: slot `[2]` is `propagation_node and not from_static_only`
    /// (`LXMRouter.py:309`), which gates posting only, since `message_get_request` never
    /// reads it (`LXMRouter.py:1427-1429`), and a fetch mines no stamp. Both stay on the
    /// record for the posting picker.
    pub(crate) fn select(&self) -> Result<PropagationNode, FetchError> {
        let now = SystemTime::now();
        let pinned = self.filed_pinned(now);
        if !pinned.is_empty() {
            let turn = self.fetch_turn.load(Ordering::Acquire) % pinned.len();
            return Ok(pinned[turn].node.clone());
        }
        self.inner
            .lock()
            .values()
            .filter(|record| !record.is_expired(now))
            .min_by_key(|record| (record.hops, Reverse(record.last_seen)))
            .map(|record| record.node.clone())
            .ok_or(FetchError::NoPropagationNode)
    }

    /// The node a post should go to: nearest by hops and most recently heard, as `select`,
    /// but only among nodes that take posted messages at a cost this node will mine.
    /// `propagate` refuses a node over the ceiling anyway; choosing around it means a
    /// dear node nearby does not hide an affordable one further off. While any pinned
    /// node is filed only pinned nodes are considered, so held mail never goes to a
    /// stranger's node that the owner's other machines do not fetch from.
    pub(crate) fn select_for_posting(&self) -> Result<PropagationNode, FetchError> {
        let now = SystemTime::now();
        let pinned = self.filed_pinned(now);
        let candidates = if pinned.is_empty() {
            self.inner.lock().values().cloned().collect()
        } else {
            pinned
        };
        candidates
            .iter()
            .filter(|record| {
                !record.is_expired(now)
                    && record.node.propagation_enabled
                    && record.node.stamp_cost <= MAX_ACCEPTED_STAMP_COST
            })
            .min_by_key(|record| (record.hops, Reverse(record.last_seen)))
            .map(|record| record.node.clone())
            .ok_or(FetchError::NoPropagationNode)
    }

    pub(crate) fn snapshot(&self) -> Vec<PropagationNodeRecord> {
        self.inner.lock().values().cloned().collect()
    }

    /// Whether the node at `destination_hex`, a lower-case hash, is on file and not aged
    /// out at `now`.
    pub(crate) fn holds(&self, destination_hex: &str, now: SystemTime) -> bool {
        self.inner
            .lock()
            .get(destination_hex)
            .is_some_and(|record| !record.is_expired(now))
    }
}

/// What a propagation node is picked for: posting takes only one that takes posts at a
/// cost this node will mine, fetching any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodePurpose {
    Posting,
    Fetching,
}

/// The propagation nodes the owner lists in `mesh.propagation_nodes`, which the node asks
/// the mesh for by path request whenever the table does not hold them.
#[derive(Default)]
pub(crate) struct ConfiguredPropagationNodes {
    hashes: Vec<String>,
    /// Set while a round of path requests is out, so a post or a sync that finds the
    /// table empty meanwhile does not send a second one.
    resolving: AtomicBool,
    /// When the latest round began, so a sync retrying every few seconds with no node
    /// filed asks at most once per `CONFIGURED_NODE_ASK_GAP`.
    last_round: Mutex<Option<Instant>>,
    /// The hashes already logged as unresolved in this run.
    told: Mutex<HashSet<String>>,
}

/// One round of path requests for the configured nodes; the next may start once it is
/// dropped.
pub(crate) struct ResolvingRound<'a>(&'a AtomicBool);

impl Drop for ResolvingRound<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl ConfiguredPropagationNodes {
    /// `hashes` lower case, as `MeshConfig::propagation_node_hashes` reads them.
    pub(crate) fn new(hashes: Vec<String>) -> Self {
        Self {
            hashes,
            ..Self::default()
        }
    }

    pub(crate) fn hashes(&self) -> &[String] {
        &self.hashes
    }

    /// The configured hashes `table` does not hold at `now`.
    pub(crate) fn missing(&self, table: &PropagationNodeTable, now: SystemTime) -> Vec<String> {
        self.hashes
            .iter()
            .filter(|hash| !table.holds(hash, now))
            .cloned()
            .collect()
    }

    /// The round of path requests beginning at `now`, `None` while another is out or
    /// when one began within `CONFIGURED_NODE_ASK_GAP`.
    pub(crate) fn begin_round(&self, now: Instant) -> Option<ResolvingRound<'_>> {
        let mut last_round = self.last_round.lock();
        if last_round
            .is_some_and(|last| now.saturating_duration_since(last) < CONFIGURED_NODE_ASK_GAP)
            || self.resolving.swap(true, Ordering::AcqRel)
        {
            return None;
        }
        *last_round = Some(now);
        Some(ResolvingRound(&self.resolving))
    }

    /// How long from `now` until a round may begin, as far as the gap goes: `None` when
    /// none has begun, zero once the gap since the latest one has passed.
    pub(crate) fn until_next_round(&self, now: Instant) -> Option<Duration> {
        self.last_round
            .lock()
            .map(|last| (last + CONFIGURED_NODE_ASK_GAP).saturating_duration_since(now))
    }

    /// Whether a round of path requests is out now.
    #[cfg(test)]
    pub(crate) fn is_resolving(&self) -> bool {
        self.resolving.load(Ordering::Acquire)
    }

    /// Forgets when the latest round began, as `CONFIGURED_NODE_ASK_GAP` passing does.
    #[cfg(test)]
    pub(crate) fn forget_last_round(&self) {
        *self.last_round.lock() = None;
    }

    /// Whether `hash` is unresolved for the first time in this run, so it is logged once.
    pub(crate) fn first_telling(&self, hash: &str) -> bool {
        self.told.lock().insert(hash.to_string())
    }
}

/// Reads the version alone first so a file from another layout is refused by its version
/// rather than set aside as corrupt for whatever field that layout added or dropped.
fn parse_node_file(bytes: &[u8]) -> Result<Vec<StoredNode>, NodeFileParse> {
    let probe: VersionProbe = serde_json::from_slice(bytes).map_err(|err| {
        NodeFileParse::Corrupt(format!(
            "{}: {err}",
            unversioned_cause(PROPAGATION_NODE_TABLE_VERSION)
        ))
    })?;
    if probe.version != PROPAGATION_NODE_TABLE_VERSION {
        return Err(NodeFileParse::Version(probe.version));
    }
    let file: PropagationNodeFile = serde_json::from_slice(bytes).map_err(|err| {
        NodeFileParse::Corrupt(format!(
            "is not a version-{PROPAGATION_NODE_TABLE_VERSION} propagation node table: {err}"
        ))
    })?;
    Ok(file.nodes)
}

/// Renames a table that cannot be used to `<path>.corrupt` (replacing any earlier one),
/// warns with the outcome and returns the empty list the node starts with. A failed rename
/// is only warned about; the file is cache and nothing downstream depends on the move.
fn set_aside_corrupt(path: &Path, what_happened: String) -> Vec<StoredNode> {
    let aside = path.with_extension("json.corrupt");
    match fs::rename(path, &aside) {
        Ok(()) => warn!(
            "Mesh propagation node table '{}' {}, so it was moved to '{}' and the mesh starts with no propagation nodes; they re-appear as they announce. Nothing needs doing; keep '{}' for a bug report or delete it.",
            path.display(),
            redact_hashes(&what_happened),
            aside.display(),
            aside.display()
        ),
        Err(err) => warn!(
            "Mesh propagation node table '{}' {}, and moving it to '{}' failed: {}. The mesh starts with no propagation nodes and the next save overwrites the file. Move '{}' aside by hand to keep it for a bug report, or remove it.",
            path.display(),
            redact_hashes(&what_happened),
            aside.display(),
            redact_hashes(&err.to_string()),
            path.display()
        ),
    }
    Vec::new()
}

#[cfg(test)]
mod persisted_table_tests {
    use super::super::test_support::TempDir;
    use super::*;
    use crate::testing::{debug_snapshot, install_log_collector, warn_snapshot};

    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use rns_transport::rand_core::OsRng;
    use std::sync::LazyLock;

    fn node() -> PropagationNode {
        PropagationNode {
            destination: propagation_node_destination(
                *TransportIdentity::new_from_rand(OsRng).as_identity(),
            ),
            stamp_cost: 8,
            per_transfer_limit_kb: 256,
            propagation_enabled: true,
        }
    }

    /// A fixed instant a few hours back, so every `t(secs)` here is in the past and well
    /// inside `PROPAGATION_NODE_TTL` of the clock the pickers read.
    static BASE: LazyLock<SystemTime> =
        LazyLock::new(|| SystemTime::now() - Duration::from_secs(10_000));

    fn t(secs: u64) -> SystemTime {
        *BASE + Duration::from_secs(secs)
    }

    fn path_in(tmp: &TempDir) -> PathBuf {
        tmp.path.join("mesh").join(PROPAGATION_NODE_TABLE_FILE)
    }

    fn load_now(tmp: &TempDir) -> Result<PropagationNodeTable> {
        PropagationNodeTable::load(path_in(tmp), SystemTime::now())
    }

    fn hex(node: &PropagationNode) -> String {
        node.destination.address_hash.to_hex_string()
    }

    #[test]
    fn a_heard_node_round_trips_and_is_selected_after_a_reload() {
        let tmp = TempDir::new("pn-table-round-trip");
        let table = load_now(&tmp).unwrap();
        let near = node();
        let mut far = node();
        far.stamp_cost = 3;
        far.propagation_enabled = false;
        table.observe(near.clone(), 1, t(100));
        table.observe(far.clone(), 4, t(200));
        table.persist_if_dirty().unwrap();

        let reloaded = load_now(&tmp).unwrap();

        assert_eq!(hex(&reloaded.select().unwrap()), hex(&near));
        let far_record = reloaded
            .snapshot()
            .into_iter()
            .find(|record| hex(&record.node) == hex(&far))
            .unwrap();
        assert_eq!(
            (far_record.hops, far_record.last_seen),
            (4, t(200)),
            "hops and when it was last heard survive"
        );
        assert_eq!(far_record.node.stamp_cost, 3);
        assert_eq!(far_record.node.per_transfer_limit_kb, 256);
        assert!(!far_record.node.propagation_enabled);
        assert_eq!(
            far_record.node.destination.identity.to_hex_string(),
            far.destination.identity.to_hex_string()
        );
    }

    /// A node loaded from the file is picked for a fetch and for a post exactly as by the
    /// table that heard it, and is held until it ages out.
    #[test]
    fn a_node_loaded_from_disk_is_picked_for_fetch_and_post_as_the_heard_one_is() {
        let tmp = TempDir::new("pn-table-loaded-picks");
        let heard = load_now(&tmp).unwrap();
        let mut near = node();
        near.propagation_enabled = false;
        let far = node();
        heard.observe(near.clone(), 1, t(100));
        heard.observe(far.clone(), 3, t(200));
        heard.persist_if_dirty().unwrap();

        let loaded = load_now(&tmp).unwrap();

        let now = SystemTime::now();
        for (label, table) in [("heard", &heard), ("loaded", &loaded)] {
            assert_eq!(hex(&table.select().unwrap()), hex(&near), "{label}");
            assert_eq!(
                hex(&table.select_for_posting().unwrap()),
                hex(&far),
                "{label}"
            );
            assert!(table.holds(&hex(&far), now), "{label}");
            assert!(!table.holds(&hex(&node()), now), "{label}");
            assert!(
                !table.holds(&hex(&far), t(200) + PROPAGATION_NODE_TTL),
                "{label}"
            );
        }
    }

    #[test]
    fn a_clean_table_is_not_written_and_a_missing_file_creates_nothing() {
        let tmp = TempDir::new("pn-table-dirty");
        let table = load_now(&tmp).unwrap();
        assert!(table.snapshot().is_empty());
        table.persist_if_dirty().unwrap();
        assert!(!tmp.path.join("mesh").exists());

        table.observe(node(), 1, t(1));
        table.persist_if_dirty().unwrap();
        assert!(path_in(&tmp).exists());
        fs::remove_file(path_in(&tmp)).unwrap();
        table.persist_if_dirty().unwrap();
        assert!(
            !path_in(&tmp).exists(),
            "a clean table is not written again"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_readable_by_its_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("pn-table-mode");
        let table = load_now(&tmp).unwrap();
        table.observe(node(), 1, t(1));
        table.persist_if_dirty().unwrap();

        let mode = fs::metadata(path_in(&tmp)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn another_version_is_refused_and_left_in_place() {
        let tmp = TempDir::new("pn-table-version");
        fs::create_dir_all(tmp.path.join("mesh")).unwrap();
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": PROPAGATION_NODE_TABLE_VERSION + 1,
            "nodes": [],
        }))
        .unwrap();
        fs::write(path_in(&tmp), &bytes).unwrap();

        let err = match load_now(&tmp) {
            Ok(_) => panic!("a newer table must be refused"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("Mesh propagation node table"), "{err}");
        assert!(err.contains(&path_in(&tmp).display().to_string()), "{err}");
        assert!(err.contains("upgrade Coyote"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert_eq!(fs::read(path_in(&tmp)).unwrap(), bytes);
    }

    #[test]
    fn an_unreadable_file_is_set_aside_and_the_table_starts_empty() {
        install_log_collector();
        let tmp = TempDir::new("pn-table-corrupt");
        fs::create_dir_all(tmp.path.join("mesh")).unwrap();
        let aside = path_in(&tmp).with_extension("json.corrupt");
        for bytes in [
            b"{not json".to_vec(),
            serde_json::to_vec(&serde_json::json!({
                "version": PROPAGATION_NODE_TABLE_VERSION,
                "nodes": [],
                "later": true,
            }))
            .unwrap(),
        ] {
            fs::write(path_in(&tmp), &bytes).unwrap();

            let table = load_now(&tmp).unwrap();

            assert!(table.snapshot().is_empty());
            assert!(!path_in(&tmp).exists());
            assert_eq!(fs::read(&aside).unwrap(), bytes);
        }
        let start = format!("Mesh propagation node table '{}' ", path_in(&tmp).display());
        let outcome = format!(
            ", so it was moved to '{aside}' and the mesh starts with no propagation nodes; they re-appear as they announce. Nothing needs doing; keep '{aside}' for a bug report or delete it.",
            aside = aside.display()
        );
        let warns = warn_snapshot();
        assert!(
            warns
                .iter()
                .any(|line| line.starts_with(&start) && line.ends_with(&outcome)),
            "no warning {start:?}...{outcome:?} in {warns:?}"
        );
    }

    #[test]
    fn a_file_that_cannot_be_moved_aside_says_to_move_or_remove_it_by_hand() {
        install_log_collector();
        let tmp = TempDir::new("pn-table-stuck");
        let aside = path_in(&tmp).with_extension("json.corrupt");
        // A non-empty directory where the set-aside copy goes makes the rename fail.
        fs::create_dir_all(aside.join("occupied")).unwrap();
        fs::write(path_in(&tmp), b"{not json").unwrap();

        let table = load_now(&tmp).unwrap();

        assert!(table.snapshot().is_empty());
        let start = format!("Mesh propagation node table '{}' ", path_in(&tmp).display());
        let moving = format!(", and moving it to '{}' failed: ", aside.display());
        let next = format!(
            ". The mesh starts with no propagation nodes and the next save overwrites the file. Move '{}' aside by hand to keep it for a bug report, or remove it.",
            path_in(&tmp).display()
        );
        let warns = warn_snapshot();
        assert!(
            warns.iter().any(|line| line.starts_with(&start)
                && line.contains(&moving)
                && line.ends_with(&next)),
            "no warning {start:?}...{moving:?}...{next:?} in {warns:?}"
        );
    }

    /// A record whose key does not derive its stored hash is dropped on load; the rest of
    /// the file still loads.
    #[test]
    fn a_record_whose_key_does_not_derive_its_hash_is_dropped() {
        install_log_collector();
        let tmp = TempDir::new("pn-table-tampered");
        let table = load_now(&tmp).unwrap();
        let kept = node();
        let tampered = node();
        table.observe(kept.clone(), 1, t(1));
        table.observe(tampered.clone(), 1, t(2));
        table.persist_if_dirty().unwrap();
        let mut file: serde_json::Value =
            serde_json::from_slice(&fs::read(path_in(&tmp)).unwrap()).unwrap();
        for stored in file["nodes"].as_array_mut().unwrap() {
            if stored["destination_hash"] == serde_json::json!(hex(&tampered)) {
                stored["public_key"] = serde_json::json!(kept.destination.identity.to_hex_string());
            }
        }
        fs::write(path_in(&tmp), serde_json::to_vec(&file).unwrap()).unwrap();

        let reloaded = load_now(&tmp).unwrap();

        let hashes: Vec<String> = reloaded.snapshot().iter().map(|r| hex(&r.node)).collect();
        assert_eq!(hashes, vec![hex(&kept)]);
        let dropped = format!(
            "Dropped stored propagation node {}: its public key derives {}",
            short(&hex(&tampered)),
            short(&hex(&kept))
        );
        assert!(
            debug_snapshot().iter().any(|line| line == &dropped),
            "no debug line {dropped:?}"
        );
    }

    #[test]
    fn a_file_over_the_cap_keeps_the_most_recently_heard() {
        let tmp = TempDir::new("pn-table-cap");
        let nodes: Vec<PropagationNode> = (0..=PROPAGATION_NODE_TABLE_MAX_ENTRIES)
            .map(|_| node())
            .collect();
        let records: Vec<StoredNode> = nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                StoredNode::of(&PropagationNodeRecord {
                    node: node.clone(),
                    hops: 1,
                    last_seen: t(1_000 + i as u64),
                })
            })
            .collect();
        fs::create_dir_all(tmp.path.join("mesh")).unwrap();
        let file = PropagationNodeFile {
            version: PROPAGATION_NODE_TABLE_VERSION,
            nodes: records,
        };
        fs::write(path_in(&tmp), serde_json::to_vec(&file).unwrap()).unwrap();

        let table = load_now(&tmp).unwrap();

        let kept: Vec<String> = table.snapshot().iter().map(|r| hex(&r.node)).collect();
        assert_eq!(kept.len(), PROPAGATION_NODE_TABLE_MAX_ENTRIES);
        assert!(
            !kept.contains(&hex(&nodes[0])),
            "the oldest is the one left out"
        );
    }

    #[test]
    fn a_stale_nearer_node_loses_to_a_fresh_farther_one() {
        let table = PropagationNodeTable::new();
        let stale = node();
        let fresh = node();
        table.observe(stale.clone(), 1, SystemTime::now() - PROPAGATION_NODE_TTL);
        table.observe(fresh.clone(), 2, SystemTime::now());

        assert_eq!(hex(&table.select().unwrap()), hex(&fresh));
        assert_eq!(hex(&table.select_for_posting().unwrap()), hex(&fresh));
    }

    #[test]
    fn a_stale_record_is_dropped_on_load() {
        install_log_collector();
        let tmp = TempDir::new("pn-table-stale");
        let now = SystemTime::now();
        let table = PropagationNodeTable::load(path_in(&tmp), now).unwrap();
        let stale = node();
        let fresh = node();
        table.observe(stale.clone(), 1, now - PROPAGATION_NODE_TTL);
        table.observe(
            fresh.clone(),
            1,
            now - PROPAGATION_NODE_TTL + Duration::from_secs(1),
        );
        table.persist_if_dirty().unwrap();

        let reloaded = PropagationNodeTable::load(path_in(&tmp), now).unwrap();

        let kept: Vec<String> = reloaded.snapshot().iter().map(|r| hex(&r.node)).collect();
        assert_eq!(kept, vec![hex(&fresh)]);
        let dropped = format!(
            "Dropped stored propagation node {}: not heard announcing for 24 hours",
            short(&hex(&stale))
        );
        assert!(
            debug_snapshot().iter().any(|line| line == &dropped),
            "no debug line {dropped:?}"
        );
    }

    #[test]
    fn a_failed_save_warns_with_the_path_the_cost_and_what_to_check() {
        install_log_collector();
        let tmp = TempDir::new("pn-table-unwritable");
        // A regular file where the mesh directory goes makes every save fail.
        fs::write(tmp.path.join("mesh"), b"").unwrap();
        let table = PropagationNodeTable::empty(path_in(&tmp));
        table.observe(node(), 1, t(1));

        table.persist_if_dirty_or_warn(PersistTrigger::Timer);
        table.persist_if_dirty_or_warn(PersistTrigger::Stop);

        let start = format!(
            "Could not save the mesh propagation node table to '{}': ",
            path_in(&tmp).display()
        );
        let check = format!(
            ". Check that '{}' is writable and has space.",
            tmp.path.join("mesh").display()
        );
        let warns = warn_snapshot();
        for cost in [
            ". The next periodic save tries again; until one succeeds, a restart forgets the propagation nodes heard since the last save",
            ". The next start will not know the propagation nodes heard since the last save until they announce again",
        ] {
            assert!(
                warns
                    .iter()
                    .any(|line| line.starts_with(&start)
                        && line.ends_with(&format!("{cost}{check}"))),
                "no warning {start:?}...{cost:?}{check:?} in {warns:?}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::propagation::pn_announce_app_data;
    use crate::mesh::session_destination_name;
    use crate::testing::{debug_snapshot, install_log_collector};

    use rns_transport::destination::{DestinationName, SingleOutputDestination};
    use rns_transport::identity::PrivateIdentity as TransportIdentity;
    use rns_transport::rand_core::OsRng;
    use std::sync::LazyLock;

    fn desc(aspect: &str) -> DestinationDesc {
        SingleOutputDestination::new(
            *TransportIdentity::new_from_rand(OsRng).as_identity(),
            DestinationName::new("lxmf", aspect),
        )
        .desc
    }

    fn node(enabled: bool) -> PropagationNode {
        PropagationNode {
            destination: desc("propagation"),
            stamp_cost: 8,
            per_transfer_limit_kb: 256,
            propagation_enabled: enabled,
        }
    }

    /// A fixed instant a few hours back, so every `t(secs)` here is in the past and well
    /// inside `PROPAGATION_NODE_TTL` of the clock the pickers read.
    static BASE: LazyLock<SystemTime> =
        LazyLock::new(|| SystemTime::now() - Duration::from_secs(10_000));

    fn t(secs: u64) -> SystemTime {
        *BASE + Duration::from_secs(secs)
    }

    fn hex(node: &PropagationNode) -> String {
        node.destination.address_hash.to_hex_string()
    }

    fn select_error(table: &PropagationNodeTable) -> FetchError {
        match table.select() {
            Ok(node) => panic!("expected no node, got {}", hex(&node)),
            Err(err) => err,
        }
    }

    #[test]
    fn empty_table_selects_nothing_with_the_teaching_error() {
        let table = PropagationNodeTable::new();
        let err = select_error(&table);
        assert_eq!(err, FetchError::NoPropagationNode);
        let text = err.to_string();
        assert!(
            text.contains("No LXMF propagation node has announced"),
            "{text}"
        );
        assert!(text.contains("interfaces"), "{text}");
    }

    #[test]
    fn select_prefers_fewest_hops_and_a_disabled_nearer_node_is_still_fetched() {
        let table = PropagationNodeTable::new();
        let far = node(true);
        let near = node(true);
        let nearest_but_disabled = node(false);
        table.observe(far.clone(), 3, t(10));
        table.observe(near.clone(), 2, t(5));
        assert_eq!(hex(&table.select().unwrap()), hex(&near));

        table.observe(nearest_but_disabled.clone(), 1, t(20));
        let selected = table.select().unwrap();
        assert_eq!(hex(&selected), hex(&nearest_but_disabled));
        assert!(!selected.propagation_enabled);
        let record = table
            .snapshot()
            .into_iter()
            .find(|record| hex(&record.node) == hex(&nearest_but_disabled))
            .unwrap();
        assert!(!record.node.propagation_enabled);
    }

    #[test]
    fn select_breaks_a_hop_tie_by_most_recent_announce() {
        let table = PropagationNodeTable::new();
        let stale = node(true);
        let fresh = node(true);
        table.observe(stale.clone(), 2, t(100));
        table.observe(fresh.clone(), 2, t(200));
        assert_eq!(hex(&table.select().unwrap()), hex(&fresh));

        table.observe(stale.clone(), 2, t(300));
        assert_eq!(hex(&table.select().unwrap()), hex(&stale));
    }

    #[test]
    fn select_for_posting_skips_disabled_and_expensive_nodes() {
        let table = PropagationNodeTable::new();
        assert!(matches!(
            table.select_for_posting(),
            Err(FetchError::NoPropagationNode)
        ));
        let disabled_near = node(false);
        let mut dear_near = node(true);
        dear_near.stamp_cost = MAX_ACCEPTED_STAMP_COST + 1;
        let mut affordable_far = node(true);
        affordable_far.stamp_cost = MAX_ACCEPTED_STAMP_COST;
        table.observe(disabled_near.clone(), 1, t(30));
        table.observe(dear_near.clone(), 1, t(20));
        table.observe(affordable_far.clone(), 4, t(10));

        assert_eq!(
            hex(&table.select_for_posting().unwrap()),
            hex(&affordable_far)
        );
        assert_eq!(
            hex(&table.select().unwrap()),
            hex(&disabled_near),
            "the fetch picker is unchanged"
        );

        let affordable_near = node(true);
        table.observe(affordable_near.clone(), 1, t(5));
        assert_eq!(
            hex(&table.select_for_posting().unwrap()),
            hex(&affordable_near)
        );
    }

    #[test]
    fn observe_refreshes_in_place_and_reports_the_change() {
        let table = PropagationNodeTable::new();
        let first = node(true);
        assert_eq!(table.observe(first.clone(), 4, t(1)), PeerChange::Added);
        let mut updated = first.clone();
        updated.stamp_cost = 12;
        assert_eq!(table.observe(updated, 1, t(2)), PeerChange::Refreshed);

        let records = table.snapshot();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].hops, 1);
        assert_eq!(records[0].last_seen, t(2));
        assert_eq!(records[0].node.stamp_cost, 12);
    }

    #[test]
    fn cap_evicts_the_least_recently_heard_and_logs_it() {
        install_log_collector();
        let table = PropagationNodeTable::new();
        let mut nodes = Vec::new();
        for i in 0..PROPAGATION_NODE_TABLE_MAX_ENTRIES {
            let candidate = node(true);
            table.observe(candidate.clone(), 1, t(1_000 + i as u64));
            nodes.push(candidate);
        }
        // The oldest is refreshed so eviction has to follow last_seen, not insertion order.
        table.observe(nodes[0].clone(), 1, t(5_000));

        let newcomer = node(true);
        table.observe(newcomer.clone(), 1, t(5_001));

        let kept: Vec<String> = table.snapshot().iter().map(|r| hex(&r.node)).collect();
        assert_eq!(kept.len(), PROPAGATION_NODE_TABLE_MAX_ENTRIES);
        assert!(kept.contains(&hex(&nodes[0])));
        assert!(kept.contains(&hex(&newcomer)));
        assert!(!kept.contains(&hex(&nodes[1])));
        let evicted = format!("Evicted propagation node {}", short(&hex(&nodes[1])));
        assert!(
            debug_snapshot().iter().any(|line| line == &evicted),
            "no debug line {evicted:?}"
        );
    }

    /// The answer to a configured node's own path request follows the path-response
    /// rules: it files a node not on file, but never revives an aged-out record, as a
    /// relay may answer from a copy it stored; an announce that is no propagation node's
    /// is refused.
    #[test]
    fn a_configured_nodes_own_path_response_never_revives_an_aged_out_record() {
        let table = PropagationNodeTable::new();
        let pn = desc("propagation");
        let key = pn.address_hash.to_hex_string();
        assert!(table.observe_announce(&pn, &pn_announce_app_data(true, 8, 256), 2, false, t(1)));
        let aged = t(1) + PROPAGATION_NODE_TTL + Duration::from_secs(1);
        assert!(!table.holds(&key, aged));

        assert_eq!(
            table.file_configured(&pn, &pn_announce_app_data(true, 8, 256), 3, aged),
            Ok(false)
        );
        assert!(!table.holds(&key, aged), "a stored copy does not revive it");
        assert_eq!(table.snapshot().pop().unwrap().hops, 3);

        let fresh = desc("propagation");
        assert_eq!(
            table.file_configured(&fresh, &pn_announce_app_data(true, 8, 256), 1, aged),
            Ok(true),
            "a node not on file is filed from its first path response"
        );
        let later = aged + PROPAGATION_NODE_TTL;
        assert_eq!(
            table.file_configured(&fresh, &pn_announce_app_data(true, 8, 256), 1, later),
            Ok(false),
            "and ages out as any other"
        );

        let coyote = desc("delivery");
        assert_eq!(
            table.file_configured(&coyote, b"not a pn", 1, aged),
            Err(PropagationNodeError::NotAPropagationNode)
        );
        assert!(!table.holds(&coyote.address_hash.to_hex_string(), aged));
    }

    /// A relay's cached path for a dead pinned node does not keep it filed: its record
    /// stays aged out, so posts and fetches fall back to a live stranger's node.
    #[test]
    fn a_cached_path_for_a_dead_pinned_node_does_not_block_a_live_stranger() {
        let table = PropagationNodeTable::new();
        let pinned = desc("propagation");
        table.pin(&[pinned.address_hash.to_hex_string()]);
        let old = SystemTime::now() - PROPAGATION_NODE_TTL - Duration::from_secs(60);
        table
            .file_configured(&pinned, &pn_announce_app_data(true, 8, 256), 1, old)
            .unwrap();
        let stranger = node(true);
        table.observe(stranger.clone(), 4, SystemTime::now());
        let cached = table.file_configured(
            &pinned,
            &pn_announce_app_data(true, 8, 256),
            1,
            SystemTime::now(),
        );
        assert_eq!(cached, Ok(false));
        assert_eq!(hex(&table.select_for_posting().unwrap()), hex(&stranger));
        assert_eq!(hex(&table.select().unwrap()), hex(&stranger));
    }

    /// A path response refreshes a known node's record and hops but not its `last_seen`,
    /// so a node heard only through path responses ages out; an unknown node it names is
    /// filed as heard.
    #[test]
    fn a_path_response_refreshes_a_known_node_without_moving_last_seen() {
        let table = PropagationNodeTable::new();
        let pn = desc("propagation");
        assert!(table.observe_announce(&pn, &pn_announce_app_data(true, 8, 256), 2, false, t(1)));

        assert!(table.observe_announce(&pn, &pn_announce_app_data(true, 12, 256), 4, true, t(9)));
        let record = table.snapshot().pop().unwrap();
        assert_eq!(record.last_seen, t(1), "a path response is not a sighting");
        assert_eq!(record.hops, 4, "the route it names is kept");
        assert_eq!(record.node.stamp_cost, 12, "the record it carries is kept");

        assert!(table.observe_announce(&pn, &pn_announce_app_data(true, 12, 256), 4, false, t(10)));
        assert_eq!(
            table.snapshot().pop().unwrap().last_seen,
            t(10),
            "an announce is"
        );

        let fresh = desc("propagation");
        assert!(table.observe_announce(
            &fresh,
            &pn_announce_app_data(true, 8, 256),
            1,
            true,
            t(11)
        ));
        let filed = table
            .snapshot()
            .into_iter()
            .find(|record| hex(&record.node) == fresh.address_hash.to_hex_string())
            .expect("an unknown node named by a path response is filed");
        assert_eq!(filed.last_seen, t(11));
    }

    #[test]
    fn observe_announce_files_a_propagation_announce_and_passes_others_through() {
        install_log_collector();
        let table = PropagationNodeTable::new();
        let pn = desc("propagation");
        let coyote = SingleOutputDestination::new(
            *TransportIdentity::new_from_rand(OsRng).as_identity(),
            session_destination_name("abc"),
        )
        .desc;

        assert!(table.observe_announce(&pn, &pn_announce_app_data(true, 8, 256), 2, false, t(1)));
        assert!(!table.observe_announce(&coyote, b"not a pn", 1, false, t(1)));
        assert_eq!(table.snapshot().len(), 1);
        assert_eq!(table.select().unwrap().stamp_cost, 8);

        // A cost this node would never mine is still filed: it bounds posting, not fetching.
        let dear = desc("propagation");
        assert!(table.observe_announce(
            &dear,
            &pn_announce_app_data(true, 30, 256),
            3,
            false,
            t(1)
        ));
        let filed = table
            .snapshot()
            .into_iter()
            .find(|record| hex(&record.node) == dear.address_hash.to_hex_string())
            .unwrap();
        assert_eq!(filed.node.stamp_cost, 30);
        assert_eq!(table.snapshot().len(), 2);

        // A propagation announce that does not parse is handled (dropped), not passed on.
        let broken = desc("propagation");
        assert!(table.observe_announce(&broken, b"\x90", 1, false, t(2)));
        assert_eq!(table.snapshot().len(), 2);
        let ignored = format!(
            "Ignored propagation node announce from {} (1 hops)",
            short(&broken.address_hash.to_hex_string())
        );
        assert!(
            debug_snapshot()
                .iter()
                .any(|line| line.starts_with(&ignored)),
            "no debug line starting {ignored:?}"
        );
    }

    /// The owner's node is heard first, then 33 strangers' nodes, nearer by hops: the
    /// table evicts strangers only, so the pinned node stays, and while it is filed it is
    /// the only node posted to and fetched from. With it gone, strangers' nodes are used.
    #[test]
    fn a_pinned_node_outlives_33_strangers_and_is_the_only_post_and_fetch_target() {
        let table = PropagationNodeTable::new();
        let pinned = node(true);
        table.pin(&[hex(&pinned)]);
        table.observe(pinned.clone(), 9, t(1));
        let strangers: Vec<PropagationNode> = (0..33).map(|_| node(true)).collect();
        for (index, stranger) in strangers.iter().enumerate() {
            table.observe(stranger.clone(), 1, t(2 + index as u64));
        }

        let held: Vec<String> = table
            .snapshot()
            .iter()
            .map(|record| hex(&record.node))
            .collect();
        assert_eq!(
            held.len(),
            PROPAGATION_NODE_TABLE_MAX_ENTRIES + 1,
            "32 others + pinned"
        );
        assert!(
            held.contains(&hex(&pinned)),
            "the pinned node is never evicted"
        );
        assert!(
            !held.contains(&hex(&strangers[0])) && held.contains(&hex(&strangers[1])),
            "only the oldest stranger made room: the pinned node takes no heard node's place"
        );
        assert_eq!(hex(&table.select_for_posting().unwrap()), hex(&pinned));
        assert_eq!(hex(&table.select().unwrap()), hex(&pinned));
        assert_eq!(table.fetch_rotation(), 1);

        table.pin(&[]);
        assert_ne!(
            hex(&table.select_for_posting().unwrap()),
            hex(&pinned),
            "with no pinned node filed the nearest stranger is used"
        );
    }

    /// A pinned node that takes no posts leaves nothing to post to while it is the only
    /// pinned node filed: a stranger's node is not used in its place.
    /// Nodes are pinned but none of them is filed, or the one filed aged out: strangers'
    /// nodes are posted to and fetched from.
    #[test]
    fn with_no_pinned_node_filed_a_strangers_node_is_used() {
        let table = PropagationNodeTable::new();
        let (pinned, aged, stranger) = (node(true), node(true), node(true));
        table.pin(&[hex(&pinned), hex(&aged)]);
        table.observe(stranger.clone(), 3, t(1));
        table.observe(aged, 1, SystemTime::now() - PROPAGATION_NODE_TTL * 2);
        assert_eq!(hex(&table.select().unwrap()), hex(&stranger));
        assert_eq!(hex(&table.select_for_posting().unwrap()), hex(&stranger));
        assert_eq!(table.fetch_rotation(), 1);
    }

    #[test]
    fn a_filed_pinned_node_that_takes_no_posts_leaves_no_stranger_to_post_to() {
        let table = PropagationNodeTable::new();
        let pinned = node(false);
        table.pin(&[hex(&pinned)]);
        table.observe(pinned.clone(), 3, t(1));
        table.observe(node(true), 1, t(2));
        assert!(table.select_for_posting().is_err());
        assert_eq!(hex(&table.select().unwrap()), hex(&pinned));
    }

    /// With two pinned nodes filed, the fetches rotate over both, a stranger's node never
    /// among them, and a table loaded with them pinned keeps them over the cap.
    #[test]
    fn fetches_rotate_over_every_pinned_node_and_a_load_keeps_them() {
        let tmp = crate::mesh::test_support::TempDir::new("pn-table-pinned-rotation");
        let path = tmp.path.join(PROPAGATION_NODE_TABLE_FILE);
        let table = PropagationNodeTable::load(path.clone(), SystemTime::now()).unwrap();
        let (first, second) = (node(true), node(true));
        let pinned = [hex(&first), hex(&second)];
        table.pin(&pinned);
        table.observe(first.clone(), 4, t(1));
        table.observe(second.clone(), 5, t(2));
        for index in 0..40 {
            table.observe(node(true), 1, t(3 + index));
        }
        assert_eq!(table.fetch_rotation(), 2);
        let mut asked = Vec::new();
        for _ in 0..4 {
            asked.push(hex(&table.select().unwrap()));
            table.advance_fetch_turn();
        }
        assert_eq!(
            asked,
            [hex(&first), hex(&second), hex(&first), hex(&second)]
        );
        table.persist_if_dirty().unwrap();

        let reloaded = PropagationNodeTable::load_pinned(path, SystemTime::now(), &pinned).unwrap();
        let held: Vec<String> = reloaded
            .snapshot()
            .iter()
            .map(|record| hex(&record.node))
            .collect();
        assert_eq!(
            held.len(),
            PROPAGATION_NODE_TABLE_MAX_ENTRIES + 2,
            "32 others + pinned"
        );
        assert!(held.contains(&hex(&first)) && held.contains(&hex(&second)));
        assert_eq!(reloaded.fetch_rotation(), 2);
    }

    /// The configured hashes the table does not hold are the ones asked for; one round
    /// of asking runs at a time, at most one per `CONFIGURED_NODE_ASK_GAP`, and each
    /// unresolved hash is told once.
    #[test]
    fn configured_nodes_the_table_lacks_are_asked_for_one_round_at_a_time_and_told_once() {
        let table = PropagationNodeTable::new();
        let filed = node(true);
        table.observe(filed.clone(), 1, SystemTime::now());
        let unresolved = "ab".repeat(16);
        let configured = ConfiguredPropagationNodes::new(vec![hex(&filed), unresolved.clone()]);
        assert_eq!(configured.hashes(), [hex(&filed), unresolved.clone()]);
        assert_eq!(
            configured.missing(&table, SystemTime::now()),
            std::slice::from_ref(&unresolved)
        );
        assert!(
            ConfiguredPropagationNodes::default()
                .missing(&table, SystemTime::now())
                .is_empty()
        );

        let t0 = Instant::now();
        let round = configured.begin_round(t0).expect("no round is out");
        let later = t0 + CONFIGURED_NODE_ASK_GAP;
        assert!(
            configured.begin_round(later).is_none(),
            "one round at a time"
        );
        drop(round);
        assert!(
            configured
                .begin_round(later - Duration::from_secs(1))
                .is_none(),
            "not within the gap"
        );
        assert!(configured.begin_round(later).is_some(), "the gap is up");

        assert!(configured.first_telling(&unresolved));
        assert!(!configured.first_telling(&unresolved));
    }

    /// An announce for a configured hash files it whenever it arrives, outside any round
    /// of path requests: a plain announce, or a path response for a node not on file (a
    /// sibling session's request answered through the same rnsd). A path response never
    /// revives the configured node's aged-out record.
    #[test]
    fn an_announce_for_a_configured_node_files_it_outside_a_request() {
        let table = PropagationNodeTable::new();
        let pn = desc("propagation");
        let key = pn.address_hash.to_hex_string();
        table.pin(std::slice::from_ref(&key));
        let now = SystemTime::now();
        assert!(table.observe_announce(&pn, &pn_announce_app_data(true, 8, 256), 2, true, now));
        assert!(
            table.holds(&key, now),
            "a path response for a node not on file"
        );

        let aged = now + PROPAGATION_NODE_TTL + Duration::from_secs(1);
        assert!(table.observe_announce(&pn, &pn_announce_app_data(true, 8, 256), 2, true, aged));
        assert!(!table.holds(&key, aged), "a path response never revives it");
        assert!(table.observe_announce(&pn, &pn_announce_app_data(true, 8, 256), 2, false, aged));
        assert!(table.holds(&key, aged), "its own announce does");
    }

    /// A configured node that does not answer is asked for at most once per 30 s, six
    /// path-request waits, and asked for again once that has passed.
    #[test]
    fn a_configured_node_is_asked_for_again_every_thirty_seconds() {
        assert_eq!(CONFIGURED_NODE_ASK_GAP, Duration::from_secs(30));
        let table = PropagationNodeTable::new();
        table.observe(node(true), 1, SystemTime::now());
        let configured = ConfiguredPropagationNodes::new(vec!["ab".repeat(16)]);
        assert_eq!(
            configured.missing(&table, SystemTime::now()).len(),
            1,
            "strangers filed"
        );
        let t0 = Instant::now();
        drop(configured.begin_round(t0).expect("the first round"));
        assert!(
            configured
                .begin_round(t0 + Duration::from_secs(29))
                .is_none()
        );
        assert_eq!(
            configured.until_next_round(t0 + Duration::from_secs(29)),
            Some(Duration::from_secs(1)),
            "the timer waits out the gap from the round's own stamp"
        );
        assert_eq!(
            ConfiguredPropagationNodes::default().until_next_round(t0),
            None,
            "no round has begun"
        );
        assert!(
            configured
                .begin_round(t0 + Duration::from_secs(30))
                .is_some()
        );
    }
}
