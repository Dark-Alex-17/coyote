use crate::mesh::announce::{MAX_DISPLAY_NAME_BYTES, is_control_or_invisible};
use crate::mesh::{canonical_hash, mesh_cache_dir, parse_rfc3339, write_atomically};

use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) const KNOCK_RECORD_VERSION: u64 = 1;

/// The knock path caps the knocker's text here; `append` refuses anything longer as a
/// backstop rather than truncating it.
pub(crate) const KNOCK_INTRO_MAX_CHARS: usize = 200;

/// An unauthenticated peer can mint identities freely, so without a cap the file would grow
/// without bound. Expired knocks go first, then each identity's surplus past
/// `KNOCK_CACHE_MAX_PER_IDENTITY`; past this many live ones, the oldest fall off the end and
/// every write that had to do so says so once in a warning.
pub(crate) const KNOCK_CACHE_MAX_ENTRIES: usize = 256;

/// One identity can mint a fresh instance, and so a fresh destination hash, for every knock
/// it sends. Without a cap per identity, a single peer knocking every few minutes would push
/// every other identity's unread knock out of the `KNOCK_CACHE_MAX_ENTRIES` cache within a
/// day; with it, that peer only ever displaces its own older knocks.
pub(crate) const KNOCK_CACHE_MAX_PER_IDENTITY: usize = 16;

/// One line of `knocks.jsonl`. The shape is a stable on-disk record other code reads back:
/// fields are only ever added, never renamed or removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KnockRecord {
    pub version: u64,
    /// RFC 3339 UTC seconds.
    pub received_at: String,
    /// Lower-hex, the identity the knocker proved over the link.
    pub identity_hash: String,
    /// Lower-hex, the knocking instance.
    pub destination_hash: String,
    pub display_name: Option<String>,
    /// The knocker's text, at most `KNOCK_INTRO_MAX_CHARS`.
    pub intro: Option<String>,
    pub hops: u8,
}

#[derive(Deserialize)]
struct VersionProbe {
    version: u64,
}

struct Evicted {
    expired: usize,
    crowded: usize,
    overflowed: usize,
}

/// Knocks not yet acted on, newest first in `<cache_dir>/mesh/knocks.jsonl`. Every read
/// parses the whole file and refuses it on the first bad line: a partial list would hide a
/// knock the user has not seen, and the file is cache, so the remedy is to move it aside.
pub(crate) struct KnockCache {
    path: PathBuf,
    /// `None` when the configured hours overflow a `Duration`: the config has no upper
    /// bound, and an absurd value means "keep everything", never a panic.
    retention: Option<Duration>,
    /// Orders the read-modify-write of `append` and `prune` within this process so two
    /// knocks landing together cannot each rewrite the file from the same stale read;
    /// `file_lock` does the same across processes.
    write_lock: Mutex<()>,
}

// Reached by the knock path and the REPL mesh commands once they land.
#[allow(dead_code)]
impl KnockCache {
    pub(crate) fn new(cache_dir: &Path, retention_hours: u64) -> Self {
        Self {
            path: mesh_cache_dir(cache_dir).join("knocks.jsonl"),
            retention: retention_hours.checked_mul(3_600).map(Duration::from_secs),
            write_lock: Mutex::new(()),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Refuses, with the file untouched, anything `read_all` would refuse on the way back,
    /// so no caller can write a line that poisons the whole cache.
    pub(crate) fn append(&self, record: KnockRecord, now: SystemTime) -> Result<()> {
        if record.version != KNOCK_RECORD_VERSION {
            bail!(
                "A knock record is version {} but this Coyote writes version {KNOCK_RECORD_VERSION}; refusing to cache it.",
                record.version
            );
        }
        if parse_rfc3339(&record.received_at).is_none() {
            bail!("A knock's `received_at` is not an RFC 3339 timestamp; refusing to cache it.");
        }
        for (field, hash) in [
            ("identity_hash", &record.identity_hash),
            ("destination_hash", &record.destination_hash),
        ] {
            if canonical_hash(hash).as_deref() != Some(hash.as_str()) {
                bail!("A knock {field} is not 32 lowercase hex characters; refusing to cache it.");
            }
        }
        if record
            .display_name
            .as_deref()
            .is_some_and(|name| name.len() > MAX_DISPLAY_NAME_BYTES)
        {
            bail!(
                "A knock display_name is longer than {MAX_DISPLAY_NAME_BYTES} bytes; refusing to cache it."
            );
        }
        if let Some(intro) = &record.intro
            && intro.chars().count() > KNOCK_INTRO_MAX_CHARS
        {
            bail!(
                "A knock intro is longer than {KNOCK_INTRO_MAX_CHARS} characters; refusing to cache it."
            );
        }
        for (field, text) in [
            ("intro", &record.intro),
            ("display_name", &record.display_name),
        ] {
            if text
                .as_deref()
                .is_some_and(|text| text.chars().any(is_control_or_invisible))
            {
                bail!(
                    "A knock {field} contains control or invisible characters; refusing to cache it."
                );
            }
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut records = self.read_all()?;
        records.insert(0, record);
        self.evict(&mut records, now);
        self.write_all(&records)
    }

    /// Newest first, with expired knocks left out; the file is not touched.
    pub(crate) fn list(&self, now: SystemTime) -> Result<Vec<KnockRecord>> {
        let mut records = self.read_all()?;
        records.retain(|record| !self.is_expired(record, now));
        Ok(records)
    }

    /// Drops expired knocks and any over the per-identity or global cap from the file and
    /// returns how many went; writes only if any did.
    pub(crate) fn prune(&self, now: SystemTime) -> Result<usize> {
        // Nothing to prune means nothing to lock: taking the file lock would create the
        // cache directory for a cache that does not exist yet.
        if !self.path.exists() {
            return Ok(0);
        }
        let _guard = self.write_lock.lock();
        let _file_lock = self.file_lock()?;
        let mut records = self.read_all()?;
        let evicted = self.evict(&mut records, now);
        let removed = evicted.expired + evicted.crowded + evicted.overflowed;
        if removed > 0 {
            self.write_all(&records)?;
        }
        Ok(removed)
    }

    /// Expired first, then each identity's oldest past `KNOCK_CACHE_MAX_PER_IDENTITY`, then
    /// the oldest past `KNOCK_CACHE_MAX_ENTRIES`; `records` is newest first, so a running
    /// count per identity and a truncate both keep the right end. Only the global overflow
    /// warns: a crowded identity's surplus is its own doing and costs no one else a knock.
    fn evict(&self, records: &mut Vec<KnockRecord>, now: SystemTime) -> Evicted {
        let before = records.len();
        records.retain(|record| !self.is_expired(record, now));
        let expired = before - records.len();
        let before = records.len();
        let mut per_identity: HashMap<String, usize> = HashMap::new();
        records.retain(|record| {
            let seen = per_identity
                .entry(record.identity_hash.clone())
                .or_default();
            *seen += 1;
            *seen <= KNOCK_CACHE_MAX_PER_IDENTITY
        });
        let crowded = before - records.len();
        let overflowed = records.len().saturating_sub(KNOCK_CACHE_MAX_ENTRIES);
        records.truncate(KNOCK_CACHE_MAX_ENTRIES);
        match overflowed {
            0 => {}
            1 => warn!(
                "Mesh knock cache is over {KNOCK_CACHE_MAX_ENTRIES} entries; dropped 1 knock still inside the retention window (the oldest)"
            ),
            n => warn!(
                "Mesh knock cache is over {KNOCK_CACHE_MAX_ENTRIES} entries; dropped {n} knocks still inside the retention window (oldest first)"
            ),
        }
        Evicted {
            expired,
            crowded,
            overflowed,
        }
    }

    /// An exclusive lock on `<path>.lock`, held until the returned `File` drops. Every
    /// Coyote process of one identity shares the cache, and `write_atomically` renames over
    /// the file itself, so the lock lives on a sibling that is never replaced. Waits for a
    /// holder rather than refusing: a knock is never dropped for landing at the same time
    /// as another.
    fn file_lock(&self) -> Result<File> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory '{}'", parent.display()))?;
        }
        let path = self.path.with_added_extension("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| {
                format!("Failed to open mesh knock cache lock '{}'", path.display())
            })?;
        file.lock().with_context(|| {
            format!("Failed to lock mesh knock cache lock '{}'", path.display())
        })?;
        Ok(file)
    }

    fn is_expired(&self, record: &KnockRecord, now: SystemTime) -> bool {
        let Some(retention) = self.retention else {
            return false;
        };
        // `read_all` already refused a `received_at` that does not parse; a timestamp in
        // the future (clock stepped back) reads as just received.
        parse_rfc3339(&record.received_at).is_some_and(|received_at| {
            now.duration_since(received_at).unwrap_or_default() >= retention
        })
    }

    fn read_all(&self) -> Result<Vec<KnockRecord>> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("Failed to read mesh knock cache '{}'", self.path.display())
                });
            }
        };
        let mut records = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let not_a_record = || {
                format!(
                    "Mesh knock cache '{}' line {} is not a knock record. It is cache: move the file aside to start fresh.",
                    self.path.display(),
                    index + 1
                )
            };
            // The version is read on its own first so a record from a newer Coyote is
            // named as such rather than failing on whatever field the newer layout added.
            let probe: VersionProbe = serde_json::from_str(line).with_context(not_a_record)?;
            if probe.version != KNOCK_RECORD_VERSION {
                bail!(
                    "Mesh knock cache '{}' line {} is a version {} record but this Coyote reads version {KNOCK_RECORD_VERSION}. Upgrade Coyote if it was written by a newer Coyote; otherwise move the file aside (it is cache) to start fresh.",
                    self.path.display(),
                    index + 1,
                    probe.version
                );
            }
            let record: KnockRecord = serde_json::from_str(line).with_context(not_a_record)?;
            if parse_rfc3339(&record.received_at).is_none() {
                bail!(
                    "Mesh knock cache '{}' line {} has a `received_at` that is not an RFC 3339 timestamp. It is cache: move the file aside to start fresh.",
                    self.path.display(),
                    index + 1
                );
            }
            records.push(record);
        }
        Ok(records)
    }

    fn write_all(&self, records: &[KnockRecord]) -> Result<()> {
        let mut text = String::new();
        for record in records {
            text.push_str(
                &serde_json::to_string(record).context("Failed to serialize a mesh knock")?,
            );
            text.push('\n');
        }
        write_atomically(&self.path, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;
    use crate::mesh::{hex_lower, rfc3339_utc};
    use crate::testing::{install_log_collector, warn_snapshot};

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// The hashes are the tag's bytes as canonical 32-hex, since `append` refuses any other shape.
    fn knock(tag: &str, received_at: SystemTime) -> KnockRecord {
        let mut seed = [0u8; 16];
        for (slot, byte) in seed.iter_mut().zip(tag.bytes()) {
            *slot = byte;
        }
        KnockRecord {
            version: KNOCK_RECORD_VERSION,
            received_at: rfc3339_utc(received_at),
            identity_hash: hex_lower(&seed),
            destination_hash: hex_lower(&seed.map(|byte| !byte)),
            display_name: Some(tag.to_string()),
            intro: Some(format!("hello from {tag}")),
            hops: 3,
        }
    }

    fn tags(records: &[KnockRecord]) -> Vec<&str> {
        records
            .iter()
            .map(|record| record.display_name.as_deref().unwrap())
            .collect()
    }

    fn overflow_warnings() -> Vec<String> {
        warn_snapshot()
            .into_iter()
            .filter(|line| line.starts_with("Mesh knock cache is over"))
            .collect()
    }

    /// `live` knocks received at `t(base)`, then `expired` more received an hour before
    /// them, written as one file under a one-hour retention. `append` never leaves the
    /// file over the cap, so the file is written directly.
    fn full_cache(tag: &str, live: usize, expired: usize, base: u64) -> (KnockCache, TempDir) {
        let tmp = TempDir::new(tag);
        let cache = KnockCache::new(&tmp.path, 1);
        let mut records: Vec<KnockRecord> = (0..live)
            .rev()
            .map(|n| knock(&format!("k{n}"), t(base)))
            .collect();
        records.extend((0..expired).map(|n| knock(&format!("stale{n}"), t(base - 3_600))));
        cache.write_all(&records).unwrap();
        (cache, tmp)
    }

    /// The `n`th knock from identity `tag`: same identity hash as `knock(tag, ..)`, a
    /// destination hash of its own, and `display_name` `"<tag>-<n>"`.
    fn knock_from(tag: &str, n: u8, received_at: SystemTime) -> KnockRecord {
        KnockRecord {
            destination_hash: hex_lower(&[n; 16]),
            display_name: Some(format!("{tag}-{n}")),
            ..knock(tag, received_at)
        }
    }

    #[test]
    fn missing_file_lists_empty_and_creates_nothing() {
        let tmp = TempDir::new("knocks-missing");
        let cache = KnockCache::new(&tmp.path, 24);

        assert!(cache.list(t(1_000)).unwrap().is_empty());
        assert_eq!(cache.prune(t(1_000)).unwrap(), 0);
        assert!(!tmp.path.join("mesh").exists());
        assert_eq!(cache.path(), tmp.path.join("mesh").join("knocks.jsonl"));
    }

    #[test]
    fn append_keeps_newest_first_and_round_trips_received_at() {
        let tmp = TempDir::new("knocks-order");
        let cache = KnockCache::new(&tmp.path, 24);

        cache.append(knock("first", t(1_000)), t(1_000)).unwrap();
        cache.append(knock("second", t(2_000)), t(2_000)).unwrap();
        cache.append(knock("third", t(3_000)), t(3_000)).unwrap();

        let listed = cache.list(t(3_000)).unwrap();
        assert_eq!(tags(&listed), vec!["third", "second", "first"]);
        assert_eq!(listed[2], knock("first", t(1_000)));
        assert_eq!(listed[0].received_at, rfc3339_utc(t(3_000)));
        let text = fs::read_to_string(cache.path()).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.lines().next().unwrap().contains("\"third\""), "{text}");
        assert!(!cache.path().with_extension("jsonl.tmp").exists());
    }

    #[test]
    fn prune_expires_exactly_at_the_retention_boundary() {
        let tmp = TempDir::new("knocks-prune");
        let cache = KnockCache::new(&tmp.path, 1);
        cache.append(knock("old", t(1_000)), t(1_000)).unwrap();
        cache.append(knock("new", t(1_001)), t(1_001)).unwrap();
        let bytes_before = fs::read(cache.path()).unwrap();

        assert_eq!(cache.prune(t(1_000 + 3_599)).unwrap(), 0);
        assert_eq!(
            fs::read(cache.path()).unwrap(),
            bytes_before,
            "a prune that removed nothing must not write"
        );
        assert_eq!(
            tags(&cache.list(t(1_000 + 3_600)).unwrap()),
            vec!["new"],
            "list must hide an expired knock without writing"
        );
        assert_eq!(fs::read(cache.path()).unwrap(), bytes_before);

        assert_eq!(cache.prune(t(1_000 + 3_600)).unwrap(), 1);

        assert_eq!(tags(&cache.list(t(1_000 + 3_600)).unwrap()), vec!["new"]);
        assert_eq!(fs::read_to_string(cache.path()).unwrap().lines().count(), 1);
        assert!(
            cache.list(t(0)).unwrap().len() == 1,
            "a knock received in the future reads as fresh"
        );
    }

    #[test]
    fn append_drops_knocks_that_expired_meanwhile() {
        let tmp = TempDir::new("knocks-append-prunes");
        let cache = KnockCache::new(&tmp.path, 1);
        cache.append(knock("old", t(1_000)), t(1_000)).unwrap();

        cache.append(knock("new", t(5_000)), t(5_000)).unwrap();

        assert_eq!(tags(&cache.list(t(5_000)).unwrap()), vec!["new"]);
        assert_eq!(fs::read_to_string(cache.path()).unwrap().lines().count(), 1);
    }

    #[test]
    fn append_refuses_oversize_or_control_character_text() {
        let tmp = TempDir::new("knocks-refuse-text");
        let cache = KnockCache::new(&tmp.path, 24);
        let oversize = KnockRecord {
            intro: Some("x".repeat(KNOCK_INTRO_MAX_CHARS + 1)),
            ..knock("long", t(1_000))
        };
        let control_intro = KnockRecord {
            intro: Some("hello\u{7}".to_string()),
            ..knock("bell", t(1_000))
        };
        let invisible_name = KnockRecord {
            display_name: Some("Bob\u{202E}".to_string()),
            ..knock("bidi", t(1_000))
        };

        for (what, record) in [
            ("oversize intro", oversize),
            ("control intro", control_intro),
            ("invisible display_name", invisible_name),
        ] {
            let err = cache.append(record, t(1_000)).unwrap_err().to_string();
            assert!(err.contains("refusing"), "{what}: {err}");
        }
        assert!(
            !cache.path().exists(),
            "a refused knock must not create the file"
        );

        let at_cap = KnockRecord {
            intro: Some("\u{20ac}".repeat(KNOCK_INTRO_MAX_CHARS)),
            ..knock("max", t(1_000))
        };
        cache.append(at_cap, t(1_000)).unwrap();
        assert_eq!(tags(&cache.list(t(1_000)).unwrap()), vec!["max"]);
    }

    #[test]
    fn append_keeps_a_variation_selector_and_refuses_every_format_character() {
        let tmp = TempDir::new("knocks-vs-vs-cf");
        let cache = KnockCache::new(&tmp.path, 24);
        let heart = "Alex \u{2764}\u{FE0F}";
        let styled = KnockRecord {
            display_name: Some(heart.to_string()),
            intro: Some("hi \u{2764}\u{FE0F} from \u{9053}\u{E0100}".to_string()),
            ..knock("styled", t(1_000))
        };

        cache.append(styled, t(1_000)).unwrap();

        let listed = cache.list(t(1_000)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].display_name.as_deref(), Some(heart));
        assert_eq!(
            listed[0].intro.as_deref(),
            Some("hi \u{2764}\u{FE0F} from \u{9053}\u{E0100}"),
            "a variation selector is filter grade only: the cache keeps it verbatim"
        );

        for (what, name) in [
            ("soft hyphen", "so\u{00AD}ft"),
            ("Arabic letter mark", "\u{061C}mark"),
            ("Mongolian vowel separator", "mongol\u{180E}ian"),
            ("tag character", "tag\u{E0041}"),
            ("interlinear annotation", "note\u{FFF9}"),
        ] {
            let record = KnockRecord {
                display_name: Some(name.to_string()),
                ..knock(what, t(1_000))
            };
            let err = cache.append(record, t(1_000)).unwrap_err().to_string();
            assert!(err.contains("control or invisible"), "{what}: {err}");
        }
        let listed = cache.list(t(1_000)).unwrap();
        assert_eq!(listed.len(), 1, "a refused knock leaves the file as it was");
        assert_eq!(listed[0].display_name.as_deref(), Some(heart));
    }

    #[test]
    fn append_refuses_records_the_reader_would_refuse() {
        let tmp = TempDir::new("knocks-refuse-shape");
        let cache = KnockCache::new(&tmp.path, 24);
        let wrong_version = KnockRecord {
            version: KNOCK_RECORD_VERSION + 1,
            ..knock("version", t(1_000))
        };
        let bad_stamp = KnockRecord {
            received_at: "yesterday".to_string(),
            ..knock("stamp", t(1_000))
        };
        let upper_identity = KnockRecord {
            identity_hash: "AB".repeat(16),
            ..knock("id", t(1_000))
        };
        let short_destination = KnockRecord {
            destination_hash: "abc".to_string(),
            ..knock("dest", t(1_000))
        };
        let long_name = KnockRecord {
            display_name: Some("n".repeat(MAX_DISPLAY_NAME_BYTES + 1)),
            ..knock("name", t(1_000))
        };

        for (what, record, needle) in [
            ("wrong version", wrong_version, "version"),
            ("bad received_at", bad_stamp, "RFC 3339"),
            ("upper-case identity_hash", upper_identity, "identity_hash"),
            (
                "short destination_hash",
                short_destination,
                "destination_hash",
            ),
            ("oversize display_name", long_name, "display_name"),
        ] {
            let err = cache.append(record, t(1_000)).unwrap_err().to_string();
            assert!(err.contains("refusing"), "{what}: {err}");
            assert!(err.contains(needle), "{what}: {err}");
        }
        assert!(
            !cache.path().exists(),
            "a refused knock must not create the file"
        );

        cache.append(knock("fine", t(1_000)), t(1_000)).unwrap();
        let bytes_before = fs::read(cache.path()).unwrap();
        let err = cache
            .append(
                KnockRecord {
                    received_at: "yesterday".to_string(),
                    ..knock("late", t(1_000))
                },
                t(1_000),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("refusing"), "{err}");
        assert_eq!(fs::read(cache.path()).unwrap(), bytes_before);
        assert!(!cache.path().with_extension("jsonl.tmp").exists());
    }

    #[test]
    #[serial_test::serial(knock_cache_overflow)]
    fn append_caps_the_cache_at_max_entries_newest_wins() {
        let tmp = TempDir::new("knocks-cap");
        let cache = KnockCache::new(&tmp.path, 24);
        for n in 0..KNOCK_CACHE_MAX_ENTRIES {
            cache
                .append(knock(&format!("k{n}"), t(1_000)), t(1_000))
                .unwrap();
        }
        assert_eq!(cache.list(t(1_000)).unwrap().len(), KNOCK_CACHE_MAX_ENTRIES);

        cache.append(knock("newest", t(1_001)), t(1_001)).unwrap();

        let listed = cache.list(t(1_001)).unwrap();
        assert_eq!(listed.len(), KNOCK_CACHE_MAX_ENTRIES);
        assert_eq!(listed[0].display_name.as_deref(), Some("newest"));
        assert_eq!(
            listed[KNOCK_CACHE_MAX_ENTRIES - 1].display_name.as_deref(),
            Some("k1"),
            "the oldest knock is the one that falls off"
        );
        assert_eq!(
            fs::read_to_string(cache.path()).unwrap().lines().count(),
            KNOCK_CACHE_MAX_ENTRIES
        );
    }

    #[test]
    #[serial_test::serial(knock_cache_overflow)]
    fn append_evicts_expired_knocks_before_live_ones_without_warning() {
        install_log_collector();
        let before = overflow_warnings().len();
        let (cache, _tmp) = full_cache(
            "knocks-evict-expired-first",
            KNOCK_CACHE_MAX_ENTRIES - 1,
            3,
            10_000,
        );
        assert_eq!(
            fs::read_to_string(cache.path()).unwrap().lines().count(),
            KNOCK_CACHE_MAX_ENTRIES + 2
        );

        cache.append(knock("newest", t(10_000)), t(10_000)).unwrap();

        let listed = cache.list(t(10_000)).unwrap();
        assert_eq!(listed.len(), KNOCK_CACHE_MAX_ENTRIES);
        assert_eq!(listed[0].display_name.as_deref(), Some("newest"));
        assert_eq!(
            listed[KNOCK_CACHE_MAX_ENTRIES - 1].display_name.as_deref(),
            Some("k0"),
            "no live knock may go while expired ones remain"
        );
        assert!(!tags(&listed).iter().any(|tag| tag.starts_with("stale")));
        assert_eq!(
            fs::read_to_string(cache.path()).unwrap().lines().count(),
            KNOCK_CACHE_MAX_ENTRIES
        );
        assert_eq!(overflow_warnings().len(), before);
    }

    #[test]
    #[serial_test::serial(knock_cache_overflow)]
    fn append_over_the_cap_drops_the_oldest_live_knock_and_warns_once() {
        install_log_collector();
        let before = overflow_warnings().len();
        let (cache, _tmp) = full_cache("knocks-evict-overflow", KNOCK_CACHE_MAX_ENTRIES, 0, 20_000);

        cache.append(knock("newest", t(20_000)), t(20_000)).unwrap();

        let listed = cache.list(t(20_000)).unwrap();
        assert_eq!(listed.len(), KNOCK_CACHE_MAX_ENTRIES);
        assert_eq!(listed[0].display_name.as_deref(), Some("newest"));
        assert!(!tags(&listed).contains(&"k0"), "only the oldest goes");
        assert!(tags(&listed).contains(&"k1"));
        let warnings = overflow_warnings();
        assert_eq!(warnings.len(), before + 1, "{warnings:?}");
        assert!(
            warnings
                .last()
                .unwrap()
                .contains("dropped 1 knock still inside the retention window (the oldest)"),
            "{warnings:?}"
        );
    }

    #[test]
    #[serial_test::serial(knock_cache_overflow)]
    fn prune_applies_the_cap_after_expiry() {
        install_log_collector();
        let before = overflow_warnings().len();
        let (cache, _tmp) = full_cache("knocks-prune-cap", KNOCK_CACHE_MAX_ENTRIES, 2, 30_000);
        let path = cache.path().to_path_buf();
        let mut text = fs::read_to_string(&path).unwrap();
        for n in 0..4 {
            text.push_str(&serde_json::to_string(&knock(&format!("extra{n}"), t(29_999))).unwrap());
            text.push('\n');
        }
        fs::write(&path, text).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap().lines().count(),
            KNOCK_CACHE_MAX_ENTRIES + 6
        );

        assert_eq!(cache.prune(t(30_000)).unwrap(), 6);

        let listed = cache.list(t(30_000)).unwrap();
        assert_eq!(listed.len(), KNOCK_CACHE_MAX_ENTRIES);
        assert!(!tags(&listed).iter().any(|tag| tag.starts_with("stale")));
        assert!(!tags(&listed).iter().any(|tag| tag.starts_with("extra")));
        assert_eq!(overflow_warnings().len(), before + 1);
        assert_eq!(cache.prune(t(30_000)).unwrap(), 0);
        assert_eq!(overflow_warnings().len(), before + 1);
    }

    #[test]
    #[serial_test::serial(knock_cache_overflow)]
    fn append_keeps_only_the_newest_per_identity_and_never_warns_for_it() {
        install_log_collector();
        let before = overflow_warnings().len();
        let tmp = TempDir::new("knocks-per-identity-cap");
        let cache = KnockCache::new(&tmp.path, 24);
        for n in 0..20 {
            cache
                .append(knock_from("a", n, t(1_000 + u64::from(n))), t(1_020))
                .unwrap();
        }
        cache.append(knock("b", t(1_020)), t(1_020)).unwrap();

        let listed = cache.list(t(1_020)).unwrap();
        assert_eq!(listed.len(), KNOCK_CACHE_MAX_PER_IDENTITY + 1);
        assert_eq!(listed[0].display_name.as_deref(), Some("b"));
        let expected: Vec<String> = (4..20).rev().map(|n| format!("a-{n}")).collect();
        assert_eq!(
            tags(&listed[1..]),
            expected,
            "the newest 16 from identity a survive"
        );
        assert!(
            listed[1..]
                .iter()
                .all(|record| record.identity_hash == knock("a", t(0)).identity_hash)
        );
        assert_eq!(
            fs::read_to_string(cache.path()).unwrap().lines().count(),
            KNOCK_CACHE_MAX_PER_IDENTITY + 1
        );
        assert_eq!(overflow_warnings().len(), before);
    }

    #[test]
    fn a_flood_from_one_identity_does_not_evict_another_identitys_older_knock() {
        let tmp = TempDir::new("knocks-per-identity-flood");
        let cache = KnockCache::new(&tmp.path, 24);
        cache.append(knock("b", t(1_000)), t(1_000)).unwrap();
        for n in 0..40 {
            cache
                .append(knock_from("a", n, t(2_000 + u64::from(n))), t(2_040))
                .unwrap();
        }

        let listed = cache.list(t(2_040)).unwrap();
        assert_eq!(listed.len(), KNOCK_CACHE_MAX_PER_IDENTITY + 1);
        assert_eq!(
            listed.last().unwrap().display_name.as_deref(),
            Some("b"),
            "the older knock from the quiet identity outlives the flood"
        );
        assert_eq!(
            tags(&listed)
                .iter()
                .filter(|tag| tag.starts_with("a-"))
                .count(),
            KNOCK_CACHE_MAX_PER_IDENTITY
        );
        assert_eq!(listed[0].display_name.as_deref(), Some("a-39"));
    }

    #[test]
    fn prune_applies_the_per_identity_cap_and_counts_it() {
        let tmp = TempDir::new("knocks-prune-per-identity");
        let cache = KnockCache::new(&tmp.path, 1);
        let mut records: Vec<KnockRecord> = (0..20)
            .rev()
            .map(|n| knock_from("a", n, t(10_000)))
            .collect();
        records.push(knock("b", t(10_000)));
        records.push(knock("stale", t(10_000 - 3_600)));
        cache.write_all(&records).unwrap();

        assert_eq!(cache.prune(t(10_000)).unwrap(), 5);

        let listed = cache.list(t(10_000)).unwrap();
        assert_eq!(listed.len(), KNOCK_CACHE_MAX_PER_IDENTITY + 1);
        assert_eq!(listed[0].display_name.as_deref(), Some("a-19"));
        assert_eq!(
            listed[KNOCK_CACHE_MAX_PER_IDENTITY - 1]
                .display_name
                .as_deref(),
            Some("a-4")
        );
        assert_eq!(listed.last().unwrap().display_name.as_deref(), Some("b"));
        assert_eq!(cache.prune(t(10_000)).unwrap(), 0);
    }

    #[test]
    fn overflowing_retention_keeps_everything() {
        let tmp = TempDir::new("knocks-forever");
        let cache = KnockCache::new(&tmp.path, u64::MAX);
        cache.append(knock("ancient", t(0)), t(0)).unwrap();

        assert_eq!(cache.prune(t(u64::MAX / 4)).unwrap(), 0);
        assert_eq!(tags(&cache.list(t(u64::MAX / 4)).unwrap()), vec!["ancient"]);
    }

    #[test]
    fn newer_record_version_refuses_naming_the_file() {
        let tmp = TempDir::new("knocks-newer");
        let cache = KnockCache::new(&tmp.path, 24);
        cache.append(knock("fine", t(1_000)), t(1_000)).unwrap();
        let mut newer = serde_json::to_value(knock("future", t(2_000))).unwrap();
        newer["version"] = serde_json::json!(KNOCK_RECORD_VERSION + 1);
        newer["future_field"] = serde_json::json!(1);
        let existing = fs::read_to_string(cache.path()).unwrap();
        fs::write(cache.path(), format!("{newer}\n\n{existing}")).unwrap();

        let err = cache.list(t(2_000)).unwrap_err().to_string();

        assert!(err.contains(&cache.path().display().to_string()), "{err}");
        assert!(err.contains("line 1"), "{err}");
        assert!(err.contains("Upgrade Coyote"), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert!(cache.append(knock("more", t(3_000)), t(3_000)).is_err());
        assert!(cache.prune(t(3_000)).is_err());
    }

    #[test]
    fn concurrent_appends_lose_nothing() {
        let tmp = TempDir::new("knocks-concurrent");
        let cache = std::sync::Arc::new(KnockCache::new(&tmp.path, 24));

        let workers: Vec<_> = (0..4)
            .map(|worker| {
                let cache = cache.clone();
                std::thread::spawn(move || {
                    for i in 0..8 {
                        let at = t(1_000 + worker * 8 + i);
                        cache
                            .append(knock(&format!("w{worker}-k{i}"), at), at)
                            .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }

        let listed = cache.list(t(2_000)).unwrap();
        assert_eq!(listed.len(), 32);
        let mut seen = tags(&listed);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 32, "every knock must survive, none twice");
        assert_eq!(
            fs::read_to_string(cache.path()).unwrap().lines().count(),
            32
        );
    }

    /// Two `KnockCache`s over one directory share no in-process mutex, as two Coyote
    /// processes of one identity would not; the file lock alone keeps every row.
    #[test]
    fn appends_from_two_caches_over_one_file_lose_nothing() {
        let tmp = TempDir::new("knocks-two-caches");
        let caches = [
            std::sync::Arc::new(KnockCache::new(&tmp.path, 24)),
            std::sync::Arc::new(KnockCache::new(&tmp.path, 24)),
        ];

        let workers: Vec<_> = caches
            .iter()
            .enumerate()
            .map(|(worker, cache)| {
                let cache = cache.clone();
                std::thread::spawn(move || {
                    for i in 0..20 {
                        let at = t(1_000 + (worker * 20 + i) as u64);
                        cache
                            .append(knock(&format!("c{worker}-k{i}"), at), at)
                            .unwrap();
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }

        let listed = caches[0].list(t(2_000)).unwrap();
        assert_eq!(listed.len(), 40);
        let mut seen = tags(&listed);
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 40, "every knock must survive, none twice");
        assert!(caches[1].path().with_added_extension("lock").exists());
    }

    #[test]
    fn unparsable_line_refuses_and_blank_lines_are_skipped() {
        let tmp = TempDir::new("knocks-garbage");
        let cache = KnockCache::new(&tmp.path, 24);
        cache.append(knock("fine", t(1_000)), t(1_000)).unwrap();
        let existing = fs::read_to_string(cache.path()).unwrap();

        fs::write(cache.path(), format!("\n{existing}\n")).unwrap();
        assert_eq!(tags(&cache.list(t(1_000)).unwrap()), vec!["fine"]);

        fs::write(cache.path(), format!("{existing}{{not json\n")).unwrap();
        let err = format!("{:#}", cache.list(t(1_000)).unwrap_err());
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains(&cache.path().display().to_string()), "{err}");

        let bad_stamp = existing.replace(&rfc3339_utc(t(1_000)), "yesterday");
        fs::write(cache.path(), bad_stamp).unwrap();
        let err = cache.list(t(1_000)).unwrap_err().to_string();
        assert!(err.contains("RFC 3339"), "{err}");
    }
}
