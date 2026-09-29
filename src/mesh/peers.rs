use crate::mesh::announce::{HEARTBEAT_SECS, PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT};
use crate::mesh::protocol::Compatibility;
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_cause, version_refusal};
use crate::mesh::{redact_hashes, short, write_atomically};

use anyhow::{Context, Result, bail};
use log::warn;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// Upper bound on remembered peers; the least recently seen is evicted first.
pub(crate) const PEER_TABLE_MAX_ENTRIES: usize = 1024;

/// A peer silent for this long since `last_seen` is aged out.
pub(crate) const PEER_TTL: Duration =
    Duration::from_secs(HEARTBEAT_SECS * (PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT as u64));

/// A peer silent for this long is flagged in listings as probably gone, one heartbeat
/// before `PEER_TTL` removes it.
pub(crate) const PEER_STALE_AFTER: Duration = Duration::from_secs(2 * HEARTBEAT_SECS);

pub(crate) const PEER_TABLE_VERSION: u64 = 1;

/// `peers.json` whole: the version first, then the peers. Rejects unknown fields, as does
/// each `PeerRecord`, so any change to the layout bumps `PEER_TABLE_VERSION`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PeerTableFile {
    pub version: u64,
    pub peers: Vec<PeerRecord>,
}

/// One remembered peer. A field with a default is one the table did not always keep; a
/// record without it still loads, but a field this build does not know refuses the record.
/// The defaults are a tolerance for hand-edited tables inside version 1, not a migration:
/// a layout change still bumps `PEER_TABLE_VERSION`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PeerRecord {
    pub destination_hash: String,
    pub identity_hash: String,
    /// Lower-hex of the announce's 10-byte name hash; it is what lets the trust store prove
    /// the destination belongs to the identity. Empty when the table predates the field.
    #[serde(default)]
    pub name_hash: String,
    pub display_name: Option<String>,
    pub protocol_version: u16,
    /// Whether this Coyote speaks `protocol_version`, kept so the outbound gate reads a
    /// verdict. A table written before the field was kept loads it as compatible and
    /// `load` reconciles it against `protocol_version`.
    #[serde(default)]
    pub compatibility: Compatibility,
    pub hops: u8,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
}

impl PeerRecord {
    /// One line for a peer listing, `None` when the peer speaks a protocol this Coyote does.
    pub(crate) fn compatibility_line(&self) -> Option<String> {
        self.compatibility.line()
    }

    /// A `last_seen` in the future (clock stepped back) reads as just seen.
    pub(crate) fn is_stale(&self, now: SystemTime) -> bool {
        now.duration_since(self.last_seen).unwrap_or_default() >= PEER_STALE_AFTER
    }
}

/// What one decoded Coyote announce says about its sender.
pub(crate) struct PeerSighting {
    pub destination_hash: String,
    pub identity_hash: String,
    pub name_hash: String,
    pub display_name: Option<String>,
    pub protocol_version: u16,
    pub hops: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerChange {
    Added,
    Refreshed,
}

/// Coyote peers seen on the mesh, keyed by destination hash and mirrored to `path` as JSON.
/// Time is always passed in so expiry is testable without a clock.
pub(crate) struct PeerTable {
    path: PathBuf,
    inner: Mutex<BTreeMap<String, PeerRecord>>,
    /// Set by every change since the last write, so a burst of announces costs one write.
    dirty: AtomicBool,
}

impl PeerTable {
    /// Loads the table at `path`, dropping entries already expired at `now`. A missing file
    /// is an empty table. A file that cannot be read or parsed (an unclean shutdown can
    /// leave it truncated or empty) or whose version cannot be read is moved aside to
    /// `<path>.corrupt` with a warning and the table starts empty: the peer table is
    /// disposable cache and must never keep the node from starting over a shape it cannot
    /// name, but the bytes are kept for a bug report rather than overwritten. The one
    /// exception is a readable version other than `PEER_TABLE_VERSION`: that file is
    /// refused and left in place, since the remedy depends on which Coyote wrote it.
    pub(crate) fn load(path: PathBuf, now: SystemTime) -> Result<Self> {
        let records = match fs::read(&path) {
            Ok(bytes) => match parse_peer_file(&bytes) {
                Ok(records) => records,
                Err(PeerFileParse::Version(found)) => bail!(version_refusal(
                    "peer table",
                    &path,
                    None,
                    found,
                    PEER_TABLE_VERSION,
                    Remedy::Cache
                )),
                Err(PeerFileParse::Corrupt(what)) => set_aside_corrupt(&path, what),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => set_aside_corrupt(&path, format!("could not be read: {err}")),
        };
        let inner = records
            .into_iter()
            .filter(|record| !is_expired(record, now))
            .map(|mut record| {
                // A record written before the field existed loads as compatible and is
                // judged from its announced version here. An `Incompatible` written by a
                // previous run is kept as written until the peer's next announce
                // re-judges it.
                if record.compatibility == Compatibility::Compatible {
                    record.compatibility = Compatibility::of(record.protocol_version);
                }
                record
            })
            .map(|record| (record.destination_hash.clone(), record))
            .collect();
        Ok(Self {
            path,
            inner: Mutex::new(inner),
            dirty: AtomicBool::new(false),
        })
    }

    /// A row whose identity differs from the sighting's follows the sighting: the transport
    /// verified that the announced identity and name hash derive the destination, so the
    /// row can only disagree when `peers.json` was corrupted or hand-edited. The change is
    /// logged rather than silent.
    pub(crate) fn observe(&self, sighting: PeerSighting, now: SystemTime) -> PeerChange {
        let mut peers = self.inner.lock();
        let change = match peers.get_mut(&sighting.destination_hash) {
            Some(record) => {
                if record.identity_hash != sighting.identity_hash {
                    warn!(
                        "Mesh peer {} re-bound from identity {} to {}: the row follows the verified announce",
                        short(&record.destination_hash),
                        short(&record.identity_hash),
                        short(&sighting.identity_hash)
                    );
                }
                record.identity_hash = sighting.identity_hash;
                record.name_hash = sighting.name_hash;
                record.display_name = sighting.display_name;
                record.protocol_version = sighting.protocol_version;
                record.compatibility = Compatibility::of(sighting.protocol_version);
                record.hops = sighting.hops;
                record.last_seen = now;
                PeerChange::Refreshed
            }
            None => {
                peers.insert(
                    sighting.destination_hash.clone(),
                    PeerRecord {
                        destination_hash: sighting.destination_hash,
                        identity_hash: sighting.identity_hash,
                        name_hash: sighting.name_hash,
                        display_name: sighting.display_name,
                        protocol_version: sighting.protocol_version,
                        compatibility: Compatibility::of(sighting.protocol_version),
                        hops: sighting.hops,
                        first_seen: now,
                        last_seen: now,
                    },
                );
                PeerChange::Added
            }
        };
        while peers.len() > PEER_TABLE_MAX_ENTRIES {
            let oldest = peers
                .values()
                .min_by_key(|record| record.last_seen)
                .map(|record| record.destination_hash.clone())
                .expect("a table over its cap is not empty");
            peers.remove(&oldest);
        }
        self.dirty.store(true, Ordering::Release);
        change
    }

    /// Files that the peer at `destination_hash` refused this node's protocol and speaks
    /// `found` instead. A peer the table does not know is left unknown; nothing about it
    /// is worth remembering until it announces.
    pub(crate) fn mark_incompatible(&self, destination_hash: &str, found: u16) {
        let mut peers = self.inner.lock();
        let Some(record) = peers.get_mut(destination_hash) else {
            return;
        };
        record.compatibility = Compatibility::Incompatible { found };
        self.dirty.store(true, Ordering::Release);
    }

    /// Removes every peer expired at `now` and returns their destination hashes.
    pub(crate) fn sweep(&self, now: SystemTime) -> Vec<String> {
        let mut peers = self.inner.lock();
        let expired: Vec<String> = peers
            .values()
            .filter(|record| is_expired(record, now))
            .map(|record| record.destination_hash.clone())
            .collect();
        for hash in &expired {
            peers.remove(hash);
        }
        if !expired.is_empty() {
            self.dirty.store(true, Ordering::Release);
        }
        expired
    }

    pub(crate) fn snapshot(&self) -> Vec<PeerRecord> {
        self.inner.lock().values().cloned().collect()
    }

    /// Exact-key lookup; keys are the lower-hex the transport's to_hex_string produces.
    pub(crate) fn get(&self, destination_hash: &str) -> Option<PeerRecord> {
        self.inner.lock().get(destination_hash).cloned()
    }

    /// Writes the table only if it changed since the last write. The flag is cleared before
    /// the snapshot is taken so a change that lands mid-write is not lost, and restored on
    /// failure so the next call retries.
    pub(crate) fn persist_if_dirty(&self) -> Result<()> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        self.persist()
            .inspect_err(|_| self.dirty.store(true, Ordering::Release))
    }

    fn persist(&self) -> Result<()> {
        let file = PeerTableFile {
            version: PEER_TABLE_VERSION,
            peers: self.snapshot(),
        };
        let json =
            serde_json::to_vec_pretty(&file).context("Failed to serialize the mesh peer table")?;
        write_atomically(&self.path, &json)
    }
}

enum PeerFileParse {
    Version(u64),
    Corrupt(String),
}

/// Reads the version alone first so a file from another layout is refused by its version
/// rather than set aside as corrupt for whatever field that layout added or dropped.
fn parse_peer_file(bytes: &[u8]) -> Result<Vec<PeerRecord>, PeerFileParse> {
    let probe: VersionProbe = serde_json::from_slice(bytes).map_err(|err| {
        PeerFileParse::Corrupt(format!("{}: {err}", unversioned_cause(PEER_TABLE_VERSION)))
    })?;
    if probe.version != PEER_TABLE_VERSION {
        return Err(PeerFileParse::Version(probe.version));
    }
    let file: PeerTableFile = serde_json::from_slice(bytes).map_err(|err| {
        PeerFileParse::Corrupt(format!(
            "is not a version-{PEER_TABLE_VERSION} peer table: {err}"
        ))
    })?;
    Ok(file.peers)
}

/// Warns about a peer table that cannot be used, renames it to `<path>.corrupt` (replacing
/// any earlier one) and returns the empty record list the node starts with. A failed rename
/// is only warned about; the file is cache and nothing downstream depends on the move.
fn set_aside_corrupt(path: &Path, what_happened: String) -> Vec<PeerRecord> {
    let aside = path.with_extension("json.corrupt");
    warn!(
        "Mesh peer table '{}' {}. Starting with an empty peer table; peers re-appear as they announce. The file is kept at '{}'.",
        path.display(),
        redact_hashes(&what_happened),
        aside.display()
    );
    if let Err(err) = fs::rename(path, &aside) {
        warn!(
            "Failed to move the unusable mesh peer table '{}' to '{}': {}",
            path.display(),
            aside.display(),
            redact_hashes(&err.to_string())
        );
    }
    Vec::new()
}

fn is_expired(record: &PeerRecord, now: SystemTime) -> bool {
    // A `last_seen` in the future (clock stepped back) reads as just seen, not as expired.
    now.duration_since(record.last_seen).unwrap_or_default() >= PEER_TTL
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;
    use crate::mesh::protocol::MESH_PROTOCOL_VERSION;
    use crate::testing::{install_log_collector, warn_snapshot};

    fn sighting(hash: &str, name: Option<&str>) -> PeerSighting {
        PeerSighting {
            destination_hash: hash.to_string(),
            identity_hash: format!("id-{hash}"),
            name_hash: format!("name-{hash}"),
            display_name: name.map(str::to_string),
            protocol_version: 1,
            hops: 2,
        }
    }

    fn table(tag: &str) -> (PeerTable, TempDir) {
        let tmp = TempDir::new(tag);
        let table =
            PeerTable::load(tmp.path.join("mesh").join("peers.json"), SystemTime::now()).unwrap();
        (table, tmp)
    }

    fn hashes(table: &PeerTable) -> Vec<String> {
        table
            .snapshot()
            .into_iter()
            .map(|record| record.destination_hash)
            .collect()
    }

    fn record(destination: &str, protocol_version: u16, at: SystemTime) -> PeerRecord {
        PeerRecord {
            destination_hash: destination.to_string(),
            identity_hash: format!("id-{destination}"),
            name_hash: format!("name-{destination}"),
            display_name: None,
            protocol_version,
            compatibility: Compatibility::Compatible,
            hops: 1,
            first_seen: at,
            last_seen: at,
        }
    }

    /// The file as this build writes it, as JSON to edit before writing.
    fn file_of(peers: Vec<PeerRecord>) -> serde_json::Value {
        serde_json::to_value(PeerTableFile {
            version: PEER_TABLE_VERSION,
            peers,
        })
        .unwrap()
    }

    #[test]
    fn ttl_is_three_heartbeats() {
        assert_eq!(PEER_TTL, Duration::from_secs(2700));
        assert_eq!(PEER_TABLE_MAX_ENTRIES, 1024);
    }

    #[test]
    fn stale_is_two_heartbeats_and_never_for_a_future_sighting() {
        let (table, _tmp) = table("peers-stale");
        let seen = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        table.observe(sighting("a", None), seen);
        let record = table.get("a").unwrap();

        assert!(!record.is_stale(seen + PEER_STALE_AFTER - Duration::from_secs(1)));
        assert!(record.is_stale(seen + PEER_STALE_AFTER));
        assert!(record.is_stale(seen + PEER_TTL));
        assert!(!record.is_stale(seen - Duration::from_secs(60)));
    }

    #[test]
    fn observe_adds_then_refreshes_keeping_first_seen() {
        let (table, _tmp) = table("peers-observe");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let t1 = t0 + Duration::from_secs(60);

        assert_eq!(
            table.observe(sighting("aa", Some("Alex")), t0),
            PeerChange::Added
        );
        assert_eq!(
            table.observe(sighting("aa", Some("Alexandra")), t1),
            PeerChange::Refreshed
        );

        let peers = table.snapshot();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].display_name.as_deref(), Some("Alexandra"));
        assert_eq!(peers[0].first_seen, t0);
        assert_eq!(peers[0].last_seen, t1);
        assert_eq!(peers[0].identity_hash, "id-aa");
        assert_eq!(peers[0].name_hash, "name-aa");
        assert_eq!(peers[0].hops, 2);
    }

    #[test]
    fn observe_logs_when_a_row_changes_identity() {
        install_log_collector();
        let (table, _tmp) = table("peers-observe-identity-change");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let destination = "rebound-row-destination";
        let old_identity = "first-key-of-rebound-row";
        let new_identity = "second-key-of-rebound-row";
        let rebound = |identity: &str| PeerSighting {
            identity_hash: identity.to_string(),
            ..sighting(destination, None)
        };

        table.observe(rebound(old_identity), t0);
        let change = table.observe(rebound(new_identity), t0 + Duration::from_secs(1));

        assert_eq!(change, PeerChange::Refreshed);
        assert_eq!(table.get(destination).unwrap().identity_hash, new_identity);
        let warns = warn_snapshot();
        assert!(
            warns.iter().any(|message| {
                message.contains(&format!(
                    "Mesh peer {} re-bound from identity {} to {}",
                    short(destination),
                    short(old_identity),
                    short(new_identity)
                ))
            }),
            "no warn line names the re-binding; captured: {warns:#?}"
        );
    }

    #[test]
    fn sweep_ages_out_exactly_at_ttl_and_keeps_fresher_peers() {
        let (table, _tmp) = table("peers-sweep");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        table.observe(sighting("old", None), t0);
        table.observe(sighting("new", None), t0 + Duration::from_secs(1));

        assert!(
            table
                .sweep(t0 + PEER_TTL - Duration::from_secs(1))
                .is_empty()
        );
        assert_eq!(table.sweep(t0 + PEER_TTL), vec!["old".to_string()]);
        assert_eq!(hashes(&table), vec!["new".to_string()]);
    }

    #[test]
    fn sweep_treats_future_last_seen_as_fresh() {
        let (table, _tmp) = table("peers-future");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        table.observe(sighting("aa", None), t0);

        assert!(table.sweep(t0 - Duration::from_secs(3_600)).is_empty());
    }

    #[test]
    fn cap_evicts_least_recently_seen() {
        let (table, _tmp) = table("peers-cap");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        for i in 0..PEER_TABLE_MAX_ENTRIES {
            table.observe(
                sighting(&format!("{i:04}"), None),
                t0 + Duration::from_secs(i as u64),
            );
        }
        // The oldest peer is refreshed so eviction must follow last_seen, not insertion order.
        table.observe(sighting("0000", None), t0 + Duration::from_secs(10_000));

        table.observe(sighting("newcomer", None), t0 + Duration::from_secs(10_001));

        let hashes = hashes(&table);
        assert_eq!(hashes.len(), PEER_TABLE_MAX_ENTRIES);
        assert!(hashes.contains(&"0000".to_string()));
        assert!(hashes.contains(&"newcomer".to_string()));
        assert!(!hashes.contains(&"0001".to_string()));
    }

    #[test]
    fn persist_and_reload_drops_expired_entries() {
        let tmp = TempDir::new("peers-reload");
        let path = tmp.path.join("mesh").join("peers.json");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let table = PeerTable::load(path.clone(), t0).unwrap();
        table.observe(sighting("stale", Some("Old")), t0);
        table.observe(
            sighting("fresh", Some("New")),
            t0 + Duration::from_secs(1_000),
        );
        table.persist().unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());

        let reloaded = PeerTable::load(path, t0 + PEER_TTL).unwrap();

        let peers = reloaded.snapshot();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].destination_hash, "fresh");
        assert_eq!(peers[0].display_name.as_deref(), Some("New"));
        assert_eq!(peers[0].last_seen, t0 + Duration::from_secs(1_000));
    }

    #[test]
    fn load_accepts_a_table_written_before_name_hash_was_kept() {
        let tmp = TempDir::new("peers-no-name-hash");
        let path = tmp.path.join("peers.json");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let mut old = file_of(vec![record("aa", 1, t0)]);
        old["peers"][0].as_object_mut().unwrap().remove("name_hash");
        fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let table = PeerTable::load(path, t0).unwrap();

        let peers = table.snapshot();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].name_hash, "");
    }

    #[test]
    fn observe_marks_an_unsupported_announce_version_incompatible() {
        let (table, _tmp) = table("peers-incompatible");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let newer = PeerSighting {
            protocol_version: MESH_PROTOCOL_VERSION + 1,
            ..sighting("aa", Some("Alex"))
        };

        assert_eq!(table.observe(newer, t0), PeerChange::Added);

        let peers = table.snapshot();
        assert_eq!(peers.len(), 1, "the sighting is recorded, not suppressed");
        assert_eq!(
            peers[0].compatibility,
            Compatibility::Incompatible {
                found: MESH_PROTOCOL_VERSION + 1
            }
        );
        assert_eq!(
            peers[0].compatibility_line().as_deref(),
            Some("incompatible: speaks protocol 2, this Coyote supports 1..=1")
        );

        table.observe(sighting("aa", Some("Alex")), t0 + Duration::from_secs(1));

        let record = table.get("aa").unwrap();
        assert_eq!(record.compatibility, Compatibility::Compatible);
        assert_eq!(record.compatibility_line(), None);
    }

    #[test]
    fn load_reconciles_compatibility_for_tables_written_before_the_field() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let load_without_field = |tag: &str, protocol_version: u16| {
            let tmp = TempDir::new(tag);
            let path = tmp.path.join("peers.json");
            let mut old = file_of(vec![record("aa", protocol_version, t0)]);
            assert!(
                old["peers"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("compatibility")
                    .is_some(),
                "the field must be present to be removed"
            );
            fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
            let table = PeerTable::load(path, t0).unwrap();
            table.get("aa").unwrap().compatibility
        };

        assert_eq!(
            load_without_field("peers-reconcile-newer", MESH_PROTOCOL_VERSION + 1),
            Compatibility::Incompatible {
                found: MESH_PROTOCOL_VERSION + 1
            }
        );
        assert_eq!(
            load_without_field("peers-reconcile-current", MESH_PROTOCOL_VERSION),
            Compatibility::Compatible
        );
    }

    #[test]
    fn mark_incompatible_sets_the_field_and_dirties_the_table() {
        let tmp = TempDir::new("peers-mark-incompatible");
        let path = tmp.path.join("mesh").join("peers.json");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let table = PeerTable::load(path.clone(), t0).unwrap();
        table.observe(sighting("aa", None), t0);
        table.persist_if_dirty().unwrap();
        fs::remove_file(&path).unwrap();

        table.mark_incompatible("nobody", 2);
        table.persist_if_dirty().unwrap();
        assert!(!path.exists(), "an unknown hash must not dirty the table");
        assert_eq!(table.snapshot().len(), 1);

        table.mark_incompatible("aa", 2);
        assert_eq!(
            table.get("aa").unwrap().compatibility,
            Compatibility::Incompatible { found: 2 }
        );
        table.persist_if_dirty().unwrap();
        assert!(path.exists(), "a marked peer must be written");

        let reloaded = PeerTable::load(path, t0).unwrap();
        assert_eq!(
            reloaded.get("aa").unwrap().compatibility,
            Compatibility::Incompatible { found: 2 },
            "a stored incompatibility survives a load whose announced version looks fine"
        );
    }

    #[test]
    fn an_announce_refresh_rejudges_a_wire_learned_mark() {
        let (table, _tmp) = table("peers-rejudge-mark");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        table.observe(sighting("aa", None), t0);

        table.mark_incompatible("aa", 2);
        assert_eq!(
            table.get("aa").unwrap().compatibility,
            Compatibility::Incompatible { found: 2 }
        );

        table.observe(sighting("aa", None), t0 + Duration::from_secs(1));
        assert_eq!(
            table.get("aa").unwrap().compatibility,
            Compatibility::Compatible,
            "an announce at a supported version overrides a wire-learned mark"
        );
    }

    #[test]
    fn persist_if_dirty_writes_once_per_change() {
        let tmp = TempDir::new("peers-dirty");
        let path = tmp.path.join("mesh").join("peers.json");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let table = PeerTable::load(path.clone(), t0).unwrap();

        table.persist_if_dirty().unwrap();
        assert!(!path.exists(), "an untouched table must not be written");

        table.observe(sighting("aa", None), t0);
        table.persist_if_dirty().unwrap();
        assert!(path.exists(), "an observed peer must be written");

        fs::remove_file(&path).unwrap();
        table.persist_if_dirty().unwrap();
        assert!(!path.exists(), "a clean table must not be written again");

        assert_eq!(
            table.sweep(t0 + Duration::from_secs(1)),
            Vec::<String>::new()
        );
        table.persist_if_dirty().unwrap();
        assert!(
            !path.exists(),
            "a sweep that removed nothing must not dirty the table"
        );

        assert_eq!(table.sweep(t0 + PEER_TTL), vec!["aa".to_string()]);
        table.persist_if_dirty().unwrap();
        assert!(path.exists(), "a sweep that removed a peer must be written");
    }

    #[test]
    fn load_of_missing_file_is_empty_and_creates_nothing() {
        let tmp = TempDir::new("peers-missing");
        let path = tmp.path.join("mesh").join("peers.json");

        let table = PeerTable::load(path.clone(), SystemTime::now()).unwrap();

        assert!(table.snapshot().is_empty());
        assert!(!tmp.path.join("mesh").exists());

        table.persist().unwrap();
        let written: PeerTableFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(written.version, PEER_TABLE_VERSION);
        assert_eq!(written.peers, Vec::new());
    }

    /// `bytes` is what a previous unclean run left behind; the load must warn naming the
    /// path with `what`, keep the bytes at `peers.json.corrupt` and start empty.
    fn assert_corrupt_file_is_set_aside(tag: &str, bytes: &[u8], what: &str) {
        install_log_collector();
        let tmp = TempDir::new(tag);
        let path = tmp.path.join("peers.json");
        let aside = tmp.path.join("peers.json.corrupt");
        fs::write(&path, bytes).unwrap();
        fs::write(&aside, "older corrupt copy").unwrap();

        let table = PeerTable::load(path.clone(), SystemTime::now())
            .expect("a corrupt table must not block the load");

        assert!(table.snapshot().is_empty());
        assert!(!path.exists(), "the corrupt file must be moved aside");
        assert_eq!(
            fs::read(&aside).unwrap(),
            bytes,
            "the corrupt bytes must survive at peers.json.corrupt"
        );
        let path_text = path.display().to_string();
        let warns = warn_snapshot();
        assert!(
            warns.iter().any(|message| message.contains(&path_text)
                && message.contains(what)
                && message.contains(&aside.display().to_string())),
            "no warning names {path_text}; captured: {warns:#?}"
        );
    }

    #[test]
    fn load_sets_aside_garbage_file_and_starts_empty() {
        assert_corrupt_file_is_set_aside(
            "peers-corrupt-garbage",
            b"{not json",
            &unversioned_cause(PEER_TABLE_VERSION),
        );
    }

    #[test]
    fn load_sets_aside_empty_file_and_starts_empty() {
        assert_corrupt_file_is_set_aside(
            "peers-corrupt-empty",
            b"",
            &unversioned_cause(PEER_TABLE_VERSION),
        );
    }

    /// A table written before the file carried a version is a bare array: its shape is
    /// unknown to this build, so it is set aside like any other unreadable file.
    #[test]
    fn load_sets_aside_an_unversioned_table_and_starts_empty() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let bare = serde_json::to_vec(&vec![record("aa", 1, t0)]).unwrap();
        assert_corrupt_file_is_set_aside(
            "peers-corrupt-unversioned",
            &bare,
            &unversioned_cause(PEER_TABLE_VERSION),
        );
    }

    #[test]
    fn load_sets_aside_a_current_table_with_an_unknown_field() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let mut file = file_of(vec![record("aa", 1, t0)]);
        file["peers"][0]["later"] = serde_json::json!(true);
        assert_corrupt_file_is_set_aside(
            "peers-corrupt-unknown-field",
            &serde_json::to_vec(&file).unwrap(),
            "is not a version-1 peer table",
        );
        let mut file = file_of(vec![record("aa", 1, t0)]);
        file["later"] = serde_json::json!(true);
        assert_corrupt_file_is_set_aside(
            "peers-corrupt-unknown-envelope-field",
            &serde_json::to_vec(&file).unwrap(),
            "is not a version-1 peer table",
        );
        let mut file = file_of(vec![record("aa", 1, t0)]);
        file["peers"][0]["compatibility"] =
            serde_json::json!({"incompatible": {"found": 2, "later": 1}});
        assert_corrupt_file_is_set_aside(
            "peers-corrupt-unknown-compatibility-field",
            &serde_json::to_vec(&file).unwrap(),
            "is not a version-1 peer table",
        );
    }

    #[test]
    fn load_round_trips_a_current_table() {
        let tmp = TempDir::new("peers-current");
        let path = tmp.path.join("peers.json");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let file = file_of(vec![record("aa", 1, t0), record("bb", 1, t0)]);
        fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();

        let table = PeerTable::load(path, t0).unwrap();

        assert_eq!(hashes(&table), vec!["aa", "bb"]);
    }

    /// A version this build does not write is refused outright, the file left in place:
    /// unlike a corrupt file, its remedy depends on which Coyote wrote it.
    fn assert_version_is_refused(tag: &str, found: u64, cause: &str) {
        let tmp = TempDir::new(tag);
        let path = tmp.path.join("peers.json");
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let mut file = file_of(vec![record("aa", 1, t0)]);
        file["version"] = serde_json::json!(found);
        let bytes = serde_json::to_vec(&file).unwrap();
        fs::write(&path, &bytes).unwrap();

        let err = match PeerTable::load(path.clone(), t0) {
            Ok(_) => panic!("a version {found} table must be refused"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains(&format!("version {found}")), "{err}");
        assert!(err.contains("version 1"), "{err}");
        assert!(err.contains(cause), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "a refused file is left as it was"
        );
        assert!(!tmp.path.join("peers.json.corrupt").exists());
    }

    #[test]
    fn load_refuses_a_newer_table_version_naming_the_path() {
        assert_version_is_refused("peers-newer", PEER_TABLE_VERSION + 1, "upgrade Coyote");
    }

    #[test]
    fn load_refuses_a_pre_baseline_table_version_as_having_no_migration() {
        assert_version_is_refused("peers-older", 0, "no migration");
    }
}
