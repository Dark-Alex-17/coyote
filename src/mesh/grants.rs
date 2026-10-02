//! One-shot grants: the files a human has let one peer fetch past the share set, in
//! `<cache_dir>/mesh/grants-<instance_id>.jsonl`. A grant answers one access request by
//! id, names the requesting instance by destination hash, and lends each path a small
//! number of uses before a short expiry, so a path a peer was handed once is not a share
//! forever. A grant sits after every deny and before the allow: it lets a path outside the
//! share set through, never one a deny or the built-in deny refuses. Every read parses the
//! whole file and refuses it on the first bad line, as the pending store does, and the
//! file is cache, so the remedy is to move it aside.

use crate::mesh::schema::{Remedy, VersionProbe, unversioned_refusal, version_refusal};
use crate::mesh::shares::PeerRef;
use crate::mesh::trust::same_hash;
use crate::mesh::wire_path::WirePath;
use crate::mesh::{canonical_hash, mesh_cache_dir, parse_rfc3339, rfc3339_utc, write_atomically};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) const GRANT_RECORD_VERSION: u64 = 1;
/// Long enough for the peer to fetch what it asked for after the human said yes, short
/// enough that a forgotten grant does not outlive the conversation.
pub(crate) const DEFAULT_GRANT_TTL: Duration = Duration::from_secs(15 * 60);
pub(crate) const DEFAULT_GRANT_USES: u32 = 1;
/// An access request names a handful of files; a grant for more than this is a share
/// list by another name and belongs in the share set.
pub(crate) const GRANT_MAX_PATHS: usize = 16;

/// One line of `grants-<instance_id>.jsonl`. The shape is a stable on-disk record other
/// code reads back. It rejects fields it does not know, so any change to the layout, a
/// field added included, bumps `GRANT_RECORD_VERSION` and a reader refuses the whole file
/// on a version it does not write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrantRecord {
    pub version: u64,
    /// The access request this grant answers.
    pub id: String,
    /// Lower-hex destination hash of the requesting instance.
    pub peer: String,
    pub paths: Vec<GrantedPath>,
    /// RFC 3339 UTC seconds.
    pub expires: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GrantedPath {
    pub path: String,
    pub uses_left: u32,
}

impl GrantRecord {
    /// A record whose expiry does not parse reads as expired: the reader refuses such a
    /// line, so this only decides for a stamp it has already accepted.
    fn is_expired(&self, now: SystemTime) -> bool {
        parse_rfc3339(&self.expires).is_none_or(|at| now >= at)
    }

    fn is_exhausted(&self) -> bool {
        self.paths.iter().all(|path| path.uses_left == 0)
    }

    fn is_live(&self, now: SystemTime) -> bool {
        !self.is_expired(now) && !self.is_exhausted()
    }

    fn is_for(&self, peer: &PeerRef<'_>) -> bool {
        same_hash(&self.peer, peer.destination)
    }

    fn has_use_for(&self, path: &str) -> bool {
        self.paths
            .iter()
            .any(|granted| granted.path == path && granted.uses_left > 0)
    }

    /// Takes one use of `path` from this grant; `false` when it has none to give.
    fn spend(&mut self, path: &str) -> bool {
        match self
            .paths
            .iter_mut()
            .find(|granted| granted.path == path && granted.uses_left > 0)
        {
            Some(granted) => {
                granted.uses_left -= 1;
                true
            }
            None => false,
        }
    }
}

/// The grants one instance has given, keyed by instance because a fork answers its own
/// access requests and must not hand out the original's.
pub(crate) struct GrantStore {
    path: PathBuf,
    /// Orders the read-modify-write of every mutation within this process; `file_lock`
    /// does the same across processes.
    write_lock: Mutex<()>,
}

impl GrantStore {
    pub(crate) fn new(cache_dir: &Path, instance_id: &str) -> Self {
        Self {
            path: mesh_cache_dir(cache_dir).join(format!("grants-{instance_id}.jsonl")),
            write_lock: Mutex::new(()),
        }
    }

    /// `new`, then a sweep of what expired while nothing was running; a missing file
    /// creates nothing.
    pub(crate) fn open(cache_dir: &Path, instance_id: &str, now: SystemTime) -> Result<Self> {
        let store = Self::new(cache_dir, instance_id);
        store.prune(now)?;
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Records `paths` as fetchable by the instance at `peer_destination`, each
    /// `DEFAULT_GRANT_USES` times until `now + ttl`. A grant with the same `id` replaces
    /// the earlier one rather than adding to it, so answering a request twice does not
    /// double the uses. The peer must be a canonical 32-hex hash and every path must pass
    /// the wire grammar, since `is_granted` compares against the text a peer sends.
    pub(crate) fn grant(
        &self,
        id: &str,
        peer_destination: &str,
        paths: &[String],
        ttl: Option<Duration>,
        now: SystemTime,
    ) -> Result<GrantRecord> {
        let Some(peer) = canonical_hash(peer_destination) else {
            bail!("A grant names its peer by a 32-hex destination hash; refusing to store it.");
        };
        if paths.is_empty() || paths.len() > GRANT_MAX_PATHS {
            bail!(
                "A grant covers between 1 and {GRANT_MAX_PATHS} paths, not {}; refusing to store it.",
                paths.len()
            );
        }
        let mut granted: Vec<GrantedPath> = Vec::new();
        for path in paths {
            WirePath::parse(path)
                .with_context(|| format!("A granted path `{path}` is not a wire path"))?;
            if !granted.iter().any(|existing| existing.path == *path) {
                granted.push(GrantedPath {
                    path: path.clone(),
                    uses_left: DEFAULT_GRANT_USES,
                });
            }
        }
        let record = GrantRecord {
            version: GRANT_RECORD_VERSION,
            id: id.to_string(),
            peer,
            paths: granted,
            expires: rfc3339_utc(now + ttl.unwrap_or(DEFAULT_GRANT_TTL)),
        };
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut records = self.read_all()?;
        records.retain(|existing| existing.id != record.id && existing.is_live(now));
        records.push(record.clone());
        self.write_all(&records)?;
        Ok(record)
    }

    /// Whether `peer` may fetch `path` right now: a live grant for its destination names
    /// exactly this text with a use left. Expired grants are swept first, so the file
    /// never serves a stale answer twice; nothing is consumed here, the caller does that
    /// once the read has succeeded.
    pub(crate) fn is_granted(
        &self,
        peer: &PeerRef<'_>,
        path: &str,
        now: SystemTime,
    ) -> Result<bool> {
        if !self.path.exists() {
            return Ok(false);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut records = self.read_all()?;
        if evict(&mut records, now) > 0 {
            self.write_all(&records)?;
        }
        Ok(records
            .iter()
            .any(|record| record.is_for(peer) && record.has_use_for(path)))
    }

    /// Spends one use of `path` for `peer`, on the first grant that still has one, and
    /// drops a grant whose every path is spent. `Ok(false)` when there was nothing to spend.
    pub(crate) fn consume(&self, peer: &PeerRef<'_>, path: &str, now: SystemTime) -> Result<bool> {
        if !self.path.exists() {
            return Ok(false);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut records = self.read_all()?;
        let swept = evict(&mut records, now);
        let spent = records
            .iter_mut()
            .filter(|record| record.is_for(peer))
            .any(|record| record.spend(path));
        records.retain(|record| !record.is_exhausted());
        if spent || swept > 0 {
            self.write_all(&records)?;
        }
        Ok(spent)
    }

    /// Drops expired and spent grants and returns how many went; writes only if any did.
    pub(crate) fn prune(&self, now: SystemTime) -> Result<usize> {
        // Nothing to prune means nothing to lock: taking the file lock would create the
        // cache directory for a store that does not exist yet.
        if !self.path.exists() {
            return Ok(0);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut records = self.read_all()?;
        let removed = evict(&mut records, now);
        if removed > 0 {
            self.write_all(&records)?;
        }
        Ok(removed)
    }

    #[cfg(test)]
    pub(crate) fn list(&self) -> Result<Vec<GrantRecord>> {
        self.read_all()
    }

    /// An exclusive lock on `<path>.lock`, held until the returned `File` drops. Every
    /// Coyote process of one identity shares the cache directory, and `write_atomically`
    /// renames over the file itself, so the lock lives on a sibling that is never replaced.
    fn file_lock(&self) -> Result<File> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory '{}'", parent.display()))?;
        }
        let lock_path = self.path.with_added_extension("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| {
                format!(
                    "Failed to open mesh grant store lock '{}'",
                    lock_path.display()
                )
            })?;
        file.lock().with_context(|| {
            format!(
                "Failed to lock mesh grant store lock '{}'",
                lock_path.display()
            )
        })?;
        Ok(file)
    }

    /// Every record in the file, refusing the whole file on the first line that is not a
    /// grant of this version with a readable expiry: a grant whose expiry does not parse
    /// would otherwise read as one that never expires.
    fn read_all(&self) -> Result<Vec<GrantRecord>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to read mesh grant store '{}'", self.path.display())
                });
            }
        };
        let mut records = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            // The version is read on its own first so a record from a newer Coyote is
            // named as such rather than failing on whatever field the newer layout added.
            let probe: VersionProbe = serde_json::from_str(line).with_context(|| {
                unversioned_refusal(
                    "grant store",
                    &self.path,
                    Some(index + 1),
                    GRANT_RECORD_VERSION,
                    Remedy::Cache,
                )
            })?;
            if probe.version != GRANT_RECORD_VERSION {
                bail!(version_refusal(
                    "grant store",
                    &self.path,
                    Some(index + 1),
                    probe.version,
                    GRANT_RECORD_VERSION,
                    Remedy::Cache
                ));
            }
            let record: GrantRecord = serde_json::from_str(line).with_context(|| {
                format!(
                    "Mesh grant store '{}' line {} is not a grant. {}",
                    self.path.display(),
                    index + 1,
                    Remedy::Cache.sentence()
                )
            })?;
            if parse_rfc3339(&record.expires).is_none() {
                bail!(
                    "Mesh grant store '{}' line {} has an `expires` that is not an RFC 3339 timestamp. {}",
                    self.path.display(),
                    index + 1,
                    Remedy::Cache.sentence()
                );
            }
            records.push(record);
        }
        Ok(records)
    }

    fn write_all(&self, records: &[GrantRecord]) -> Result<()> {
        let mut text = String::new();
        for record in records {
            if record.version != GRANT_RECORD_VERSION {
                bail!(
                    "A grant record is version {} but this Coyote writes version {GRANT_RECORD_VERSION}; refusing to store it.",
                    record.version
                );
            }
            text.push_str(
                &serde_json::to_string(record).context("Failed to serialize a mesh grant")?,
            );
            text.push('\n');
        }
        write_atomically(&self.path, text.as_bytes())
    }
}

/// Drops expired and spent grants; returns how many went.
fn evict(records: &mut Vec<GrantRecord>, now: SystemTime) -> usize {
    let before = records.len();
    records.retain(|record| record.is_live(now));
    before - records.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::hex_lower;
    use crate::mesh::test_support::TempDir;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn fake_hash(fill: u8) -> String {
        hex_lower(&[fill; 16])
    }

    fn paths(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_string()).collect()
    }

    fn store(tag: &str) -> (GrantStore, TempDir) {
        let tmp = TempDir::new(tag);
        let store = GrantStore::new(&tmp.path, "inst");
        (store, tmp)
    }

    /// Rewrites the store as one record, as a hand edit or another build would leave it.
    fn write_line(store: &GrantStore, line: &serde_json::Value) {
        fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        fs::write(store.path(), format!("{line}\n")).unwrap();
    }

    #[test]
    fn missing_file_is_empty_and_creates_nothing() {
        let tmp = TempDir::new("grants-missing");
        let destination = fake_hash(0x2b);
        let peer = PeerRef {
            identity: &fake_hash(0x1a),
            destination: &destination,
        };

        let store = GrantStore::open(&tmp.path, "inst", t(1_000)).unwrap();

        assert!(store.list().unwrap().is_empty());
        assert!(!store.is_granted(&peer, "docs/a.md", t(1_000)).unwrap());
        assert!(!store.consume(&peer, "docs/a.md", t(1_000)).unwrap());
        assert_eq!(store.prune(t(1_000)).unwrap(), 0);
        assert!(!mesh_cache_dir(&tmp.path).exists());
    }

    #[test]
    fn default_ttl_is_fifteen_minutes() {
        assert_eq!(DEFAULT_GRANT_TTL, Duration::from_secs(900));
        assert_eq!(DEFAULT_GRANT_USES, 1);
        assert_eq!(GRANT_RECORD_VERSION, 1);
        let (store, _tmp) = store("grants-ttl");

        let record = store
            .grant(
                "req-1",
                &fake_hash(0x2b),
                &paths(&["docs/a.md"]),
                None,
                t(1_000),
            )
            .unwrap();

        assert_eq!(record.expires, rfc3339_utc(t(1_900)));
        assert_eq!(
            record.paths,
            [GrantedPath {
                path: "docs/a.md".into(),
                uses_left: 1,
            }]
        );
        assert_eq!(record.version, GRANT_RECORD_VERSION);
    }

    #[test]
    fn a_grant_stores_the_peer_canonically_dedupes_its_paths_and_replaces_its_own_id() {
        let (store, _tmp) = store("grants-upsert");
        let destination = fake_hash(0x2b);

        store
            .grant(
                "req-1",
                &destination.to_ascii_uppercase(),
                &paths(&["docs/a.md", "docs/b.md", "docs/a.md"]),
                Some(Duration::from_secs(60)),
                t(1_000),
            )
            .unwrap();
        store
            .grant("req-2", &destination, &paths(&["src/x.rs"]), None, t(1_000))
            .unwrap();
        let replaced = store
            .grant(
                "req-1",
                &destination,
                &paths(&["docs/c.md"]),
                None,
                t(1_000),
            )
            .unwrap();

        let records = store.list().unwrap();
        assert_eq!(records.len(), 2, "{records:#?}");
        assert_eq!(records[0].id, "req-2");
        assert_eq!(records[1], replaced);
        assert_eq!(replaced.peer, destination);
        assert_eq!(replaced.paths.len(), 1);
        let peer = PeerRef {
            identity: &fake_hash(0x1a),
            destination: &destination,
        };
        assert!(!store.is_granted(&peer, "docs/a.md", t(1_000)).unwrap());
        assert!(store.is_granted(&peer, "docs/c.md", t(1_000)).unwrap());
    }

    #[test]
    fn a_grant_refuses_a_peer_that_is_not_a_hash_or_paths_outside_the_grammar_or_count() {
        let (store, _tmp) = store("grants-refused");

        let err = store
            .grant("req", "bob", &paths(&["docs/a.md"]), None, t(0))
            .unwrap_err()
            .to_string();
        assert!(err.contains("32-hex destination hash"), "{err}");
        let err = store
            .grant("req", &fake_hash(0x2b), &[], None, t(0))
            .unwrap_err()
            .to_string();
        assert!(err.contains("between 1 and 16"), "{err}");
        let many: Vec<String> = (0..17).map(|n| format!("docs/{n}.md")).collect();
        let err = store
            .grant("req", &fake_hash(0x2b), &many, None, t(0))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not 17"), "{err}");
        for bad in ["../../.bashrc", "/etc/passwd", "docs\\a.md", ""] {
            let err = format!(
                "{:#}",
                store
                    .grant("req", &fake_hash(0x2b), &paths(&[bad]), None, t(0))
                    .unwrap_err()
            );
            assert!(err.contains("not a wire path"), "{bad:?}: {err}");
        }
        assert!(!store.path().exists());
    }

    #[test]
    fn a_grant_matches_the_destination_alone_and_exactly_the_path_text() {
        let (store, _tmp) = store("grants-match");
        let (identity, destination, other) = (fake_hash(0x1a), fake_hash(0x2b), fake_hash(0x3c));
        store
            .grant("req", &destination, &paths(&["docs/a.md"]), None, t(1_000))
            .unwrap();

        let peer = PeerRef {
            identity: &identity,
            destination: &destination,
        };
        assert!(store.is_granted(&peer, "docs/a.md", t(1_000)).unwrap());
        assert!(!store.is_granted(&peer, "docs/A.md", t(1_000)).unwrap());
        assert!(!store.is_granted(&peer, "docs/a.md/", t(1_000)).unwrap());
        let by_identity_only = PeerRef {
            identity: &destination,
            destination: &other,
        };
        assert!(
            !store
                .is_granted(&by_identity_only, "docs/a.md", t(1_000))
                .unwrap(),
            "a grant is for one instance, not every instance of the identity"
        );
    }

    #[test]
    fn three_granted_paths_are_each_fetchable_once_and_a_fourth_use_of_any_is_not_served() {
        let (store, _tmp) = store("grants-three");
        let destination = fake_hash(0x2b);
        let peer = PeerRef {
            identity: &fake_hash(0x1a),
            destination: &destination,
        };
        let granted = ["docs/a.md", "src/b.rs", "notes/c.txt"];
        store
            .grant("req", &destination, &paths(&granted), None, t(1_000))
            .unwrap();

        for path in granted {
            assert!(store.is_granted(&peer, path, t(1_001)).unwrap(), "{path}");
            assert!(store.consume(&peer, path, t(1_001)).unwrap(), "{path}");
        }

        for path in granted {
            assert!(!store.is_granted(&peer, path, t(1_002)).unwrap(), "{path}");
            assert!(!store.consume(&peer, path, t(1_002)).unwrap(), "{path}");
        }
        assert!(
            store.list().unwrap().is_empty(),
            "a grant whose every path is spent leaves the file"
        );
    }

    #[test]
    fn consume_spends_one_path_and_keeps_the_others() {
        let (store, _tmp) = store("grants-consume");
        let destination = fake_hash(0x2b);
        let peer = PeerRef {
            identity: &fake_hash(0x1a),
            destination: &destination,
        };
        store
            .grant(
                "req",
                &destination,
                &paths(&["docs/a.md", "docs/b.md"]),
                None,
                t(1_000),
            )
            .unwrap();

        assert!(store.consume(&peer, "docs/a.md", t(1_000)).unwrap());

        let records = store.list().unwrap();
        assert_eq!(
            records[0].paths,
            [
                GrantedPath {
                    path: "docs/a.md".into(),
                    uses_left: 0,
                },
                GrantedPath {
                    path: "docs/b.md".into(),
                    uses_left: 1,
                },
            ]
        );
        assert!(!store.is_granted(&peer, "docs/a.md", t(1_000)).unwrap());
        assert!(store.is_granted(&peer, "docs/b.md", t(1_000)).unwrap());
    }

    #[test]
    fn expired_grants_are_swept_on_open_and_on_every_check() {
        let tmp = TempDir::new("grants-expiry");
        let destination = fake_hash(0x2b);
        let peer = PeerRef {
            identity: &fake_hash(0x1a),
            destination: &destination,
        };
        let store = GrantStore::new(&tmp.path, "inst");
        store
            .grant(
                "short",
                &destination,
                &paths(&["docs/a.md"]),
                Some(Duration::from_secs(10)),
                t(1_000),
            )
            .unwrap();
        store
            .grant("long", &destination, &paths(&["docs/b.md"]), None, t(1_000))
            .unwrap();

        assert!(store.is_granted(&peer, "docs/a.md", t(1_009)).unwrap());
        assert!(
            !store.is_granted(&peer, "docs/a.md", t(1_010)).unwrap(),
            "a grant expires exactly at its stamp"
        );
        let ids: Vec<String> = store.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            ["long"],
            "the check rewrote the file without the expired grant"
        );

        let reopened = GrantStore::open(&tmp.path, "inst", t(1_000 + 15 * 60)).unwrap();
        assert!(reopened.list().unwrap().is_empty());
        assert!(
            !reopened
                .is_granted(&peer, "docs/b.md", t(1_000 + 15 * 60))
                .unwrap()
        );
        assert!(
            !reopened
                .consume(&peer, "docs/b.md", t(1_000 + 15 * 60))
                .unwrap()
        );
    }

    #[test]
    fn prune_drops_expired_and_spent_grants_and_counts_them() {
        let (store, _tmp) = store("grants-prune");
        let destination = fake_hash(0x2b);
        let peer = PeerRef {
            identity: &fake_hash(0x1a),
            destination: &destination,
        };
        store
            .grant(
                "spent",
                &destination,
                &paths(&["docs/a.md"]),
                None,
                t(1_000),
            )
            .unwrap();
        store
            .grant(
                "expired",
                &destination,
                &paths(&["docs/b.md"]),
                Some(Duration::from_secs(1)),
                t(1_000),
            )
            .unwrap();
        store
            .grant("live", &destination, &paths(&["docs/c.md"]), None, t(1_000))
            .unwrap();
        assert!(store.consume(&peer, "docs/a.md", t(1_000)).unwrap());

        assert_eq!(store.prune(t(1_001)).unwrap(), 1);
        assert_eq!(store.prune(t(1_001)).unwrap(), 0);

        let ids: Vec<String> = store.list().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, ["live"]);
    }

    #[test]
    fn a_newer_grant_line_refuses_the_whole_store() {
        let (store, _tmp) = store("grants-newer");
        let destination = fake_hash(0x2b);
        let record = store
            .grant("req", &destination, &paths(&["docs/a.md"]), None, t(1_000))
            .unwrap();
        let mut newer = serde_json::to_value(&record).unwrap();
        newer["version"] = serde_json::json!(GRANT_RECORD_VERSION + 1);
        newer["future_field"] = serde_json::json!(1);
        write_line(&store, &newer);
        let peer = PeerRef {
            identity: &fake_hash(0x1a),
            destination: &destination,
        };

        let err = store
            .is_granted(&peer, "docs/a.md", t(1_000))
            .unwrap_err()
            .to_string();

        assert!(err.contains(&store.path().display().to_string()), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(
            err.contains(&format!("version {}", GRANT_RECORD_VERSION + 1)),
            "{err}"
        );
        assert!(err.contains("upgrade Coyote"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(store.consume(&peer, "docs/a.md", t(1_000)).is_err());
        assert!(store.prune(t(1_000)).is_err());
        assert!(
            store
                .grant("more", &destination, &paths(&["docs/b.md"]), None, t(1_000))
                .is_err()
        );
    }

    #[test]
    fn a_pre_baseline_grant_line_refuses_as_having_no_migration() {
        let (store, _tmp) = store("grants-older");
        let record = store
            .grant(
                "req",
                &fake_hash(0x2b),
                &paths(&["docs/a.md"]),
                None,
                t(1_000),
            )
            .unwrap();
        let mut older = serde_json::to_value(&record).unwrap();
        older["version"] = serde_json::json!(0);
        write_line(&store, &older);

        let err = store.list().unwrap_err().to_string();

        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("version 0"), "{err}");
        assert!(err.contains("no migration"), "{err}");
        assert!(!err.contains("upgrade Coyote"), "{err}");
    }

    #[test]
    fn a_grant_line_without_a_version_refuses_the_whole_store() {
        let (store, _tmp) = store("grants-unversioned");
        let record = store
            .grant(
                "req",
                &fake_hash(0x2b),
                &paths(&["docs/a.md"]),
                None,
                t(1_000),
            )
            .unwrap();
        let mut unversioned = serde_json::to_value(&record).unwrap();
        unversioned.as_object_mut().unwrap().remove("version");
        write_line(&store, &unversioned);

        let err = format!("{:#}", store.list().unwrap_err());

        assert!(err.contains(&store.path().display().to_string()), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("no readable `version` field"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
    }

    #[test]
    fn a_grant_with_an_unknown_field_is_refused() {
        let (store, _tmp) = store("grants-unknown-field");
        let record = store
            .grant(
                "req",
                &fake_hash(0x2b),
                &paths(&["docs/a.md"]),
                None,
                t(1_000),
            )
            .unwrap();
        let mut extra = serde_json::to_value(&record).unwrap();
        extra["note"] = serde_json::json!("hand edited");
        write_line(&store, &extra);
        let err = format!("{:#}", store.list().unwrap_err());
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("is not a grant"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");

        let mut extra = serde_json::to_value(&record).unwrap();
        extra["paths"][0]["note"] = serde_json::json!("hand edited");
        write_line(&store, &extra);
        let err = format!("{:#}", store.list().unwrap_err());
        assert!(err.contains("is not a grant"), "{err}");
    }

    #[test]
    fn a_grant_whose_expiry_is_not_a_timestamp_is_refused() {
        let (store, _tmp) = store("grants-bad-expiry");
        let record = store
            .grant(
                "req",
                &fake_hash(0x2b),
                &paths(&["docs/a.md"]),
                None,
                t(1_000),
            )
            .unwrap();
        let mut bad = serde_json::to_value(&record).unwrap();
        bad["expires"] = serde_json::json!("tomorrow");
        write_line(&store, &bad);

        let err = store.list().unwrap_err().to_string();

        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("`expires`"), "{err}");
        assert!(err.contains("RFC 3339"), "{err}");
    }

    #[test]
    fn the_writer_refuses_a_record_of_another_version() {
        let (store, _tmp) = store("grants-writer-version");
        let record = GrantRecord {
            version: GRANT_RECORD_VERSION + 1,
            id: "req".into(),
            peer: fake_hash(0x2b),
            paths: vec![GrantedPath {
                path: "docs/a.md".into(),
                uses_left: 1,
            }],
            expires: rfc3339_utc(t(2_000)),
        };

        let err = store.write_all(&[record]).unwrap_err().to_string();

        assert!(err.contains("refusing to store it"), "{err}");
        assert!(!store.path().exists());
    }

    #[test]
    fn blank_lines_are_skipped_and_an_unparsable_line_refuses() {
        let (store, _tmp) = store("grants-blank");
        let record = store
            .grant(
                "req",
                &fake_hash(0x2b),
                &paths(&["docs/a.md"]),
                None,
                t(1_000),
            )
            .unwrap();
        let line = serde_json::to_string(&record).unwrap();
        fs::write(store.path(), format!("\n{line}\n\n")).unwrap();
        assert_eq!(store.list().unwrap(), [record]);

        fs::write(store.path(), format!("{line}\nnot json\n")).unwrap();
        let err = format!("{:#}", store.list().unwrap_err());
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("no readable `version` field"), "{err}");
    }
}
