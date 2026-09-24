use crate::mesh::announce::{HEARTBEAT_SECS, PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

/// Upper bound on remembered peers; the least recently seen is evicted first.
pub(crate) const PEER_TABLE_MAX_ENTRIES: usize = 1024;

/// A peer silent for this long since `last_seen` is aged out.
pub(crate) const PEER_TTL: Duration =
    Duration::from_secs(HEARTBEAT_SECS * (PEER_MISSED_HEARTBEATS_BEFORE_AGE_OUT as u64));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PeerRecord {
    pub destination_hash: String,
    pub identity_hash: String,
    pub display_name: Option<String>,
    pub protocol_version: u16,
    pub hops: u8,
    pub first_seen: SystemTime,
    pub last_seen: SystemTime,
}

/// What one decoded Coyote announce says about its sender.
pub(crate) struct PeerSighting {
    pub destination_hash: String,
    pub identity_hash: String,
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
    /// is an empty table; an unreadable one is refused so a corrupt cache is noticed.
    pub(crate) fn load(path: PathBuf, now: SystemTime) -> Result<Self> {
        let records: Vec<PeerRecord> = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "Mesh peer table '{}' is not valid JSON. Remove the file to start with an empty peer table; peers re-appear as they announce.",
                    path.display()
                )
            })?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to read mesh peer table '{}'", path.display())
                });
            }
        };
        let inner = records
            .into_iter()
            .filter(|record| !is_expired(record, now))
            .map(|record| (record.destination_hash.clone(), record))
            .collect();
        Ok(Self {
            path,
            inner: Mutex::new(inner),
            dirty: AtomicBool::new(false),
        })
    }

    pub(crate) fn observe(&self, sighting: PeerSighting, now: SystemTime) -> PeerChange {
        let mut peers = self.inner.lock();
        let change = match peers.get_mut(&sighting.destination_hash) {
            Some(record) => {
                record.identity_hash = sighting.identity_hash;
                record.display_name = sighting.display_name;
                record.protocol_version = sighting.protocol_version;
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
                        display_name: sighting.display_name,
                        protocol_version: sighting.protocol_version,
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

    /// Writes the table to disk via a sibling temp file so a crash mid-write cannot leave a
    /// half-written table for the next load to refuse.
    fn persist(&self) -> Result<()> {
        let json = serde_json::to_vec_pretty(&self.snapshot())
            .context("Failed to serialize the mesh peer table")?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory '{}'", parent.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, json)
            .and_then(|()| fs::rename(&tmp, &self.path))
            .with_context(|| format!("Failed to write mesh peer table '{}'", self.path.display()))
    }
}

fn is_expired(record: &PeerRecord, now: SystemTime) -> bool {
    // A `last_seen` in the future (clock stepped back) reads as just seen, not as expired.
    now.duration_since(record.last_seen).unwrap_or_default() >= PEER_TTL
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;

    fn sighting(hash: &str, name: Option<&str>) -> PeerSighting {
        PeerSighting {
            destination_hash: hash.to_string(),
            identity_hash: format!("id-{hash}"),
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

    #[test]
    fn ttl_is_three_heartbeats() {
        assert_eq!(PEER_TTL, Duration::from_secs(2700));
        assert_eq!(PEER_TABLE_MAX_ENTRIES, 1024);
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
        assert_eq!(peers[0].hops, 2);
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
        assert_eq!(
            serde_json::from_slice::<Vec<PeerRecord>>(&fs::read(&path).unwrap()).unwrap(),
            Vec::new()
        );
    }

    #[test]
    fn load_refuses_corrupt_file_naming_path_and_remedy() {
        let tmp = TempDir::new("peers-corrupt");
        let path = tmp.path.join("peers.json");
        fs::write(&path, "{not json").unwrap();

        let err = PeerTable::load(path.clone(), SystemTime::now())
            .err()
            .expect("corrupt table must be refused")
            .to_string();

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("Remove the file"), "{err}");
    }
}
