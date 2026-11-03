use crate::mesh::message::is_wire_id;
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_cause, version_refusal};
use crate::mesh::{mesh_cache_dir, redact_hashes, write_atomically_owner_only};

use anyhow::{Context, Result, bail};
use log::warn;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};

/// Upper bound on remembered threads, sent and received together. Received threads are
/// evicted first, so a peer sending many threads cannot push out the ones this instance
/// started.
pub(crate) const SENT_THREADS_MAX_ENTRIES: usize = 1024;

pub(crate) const SENT_THREADS_VERSION: u64 = 1;

/// The file whole: the version first, then each list oldest to newest. Rejects unknown
/// fields, so any change to the layout bumps `SENT_THREADS_VERSION`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SentThreadsFile {
    pub version: u64,
    pub sent: Vec<String>,
    pub received: Vec<String>,
}

#[derive(Default)]
struct Held {
    sent: VecDeque<String>,
    received: VecDeque<String>,
}

impl Held {
    /// Moves `thread` to the newest end of `list`, adding it when absent. Returns whether
    /// the list changed: a thread already newest leaves it as it was.
    fn bump(list: &mut VecDeque<String>, thread: &str) -> bool {
        if list.back().is_some_and(|newest| newest == thread) {
            return false;
        }
        list.retain(|other| other != thread);
        list.push_back(thread.to_string());
        true
    }

    fn evict_over_cap(&mut self) {
        while self.sent.len() + self.received.len() > SENT_THREADS_MAX_ENTRIES {
            if self.received.pop_front().is_none() {
                self.sent.pop_front();
            }
        }
    }
}

/// The threads this instance has sent or received, in
/// `<cache_dir>/mesh/sent-threads-<instance_id>.json`: what `mesh__send` and `mesh__ask`
/// accept as a thread to continue. Keyed by instance because a fork holds its own
/// conversations. The instance lock keeps the file to one process, so the mutex alone
/// orders the writes.
pub(crate) struct SentThreads {
    path: PathBuf,
    held: Mutex<Held>,
}

impl SentThreads {
    pub(crate) fn open(cache_dir: &Path, instance_id: &str) -> Result<Self> {
        Self::load(mesh_cache_dir(cache_dir).join(format!("sent-threads-{instance_id}.json")))
    }

    /// Loads the lists at `path`. A missing file is empty and nothing is created until
    /// the first thread is recorded. A file that cannot be read or parsed, or whose
    /// version cannot be read, is moved aside to `<path>.corrupt` with a warning and the
    /// lists start empty, as the peer table does. A readable version other than
    /// `SENT_THREADS_VERSION` is refused and the file left in place.
    pub(crate) fn load(path: PathBuf) -> Result<Self> {
        let held = match fs::read(&path) {
            Ok(bytes) => match parse_sent_threads(&bytes) {
                Ok(file) => Held {
                    sent: file.sent.into(),
                    received: file.received.into(),
                },
                Err(SentThreadsParse::Version(found)) => bail!(version_refusal(
                    "sent-threads list",
                    &path,
                    None,
                    found,
                    SENT_THREADS_VERSION,
                    Remedy::Cache
                )),
                Err(SentThreadsParse::Corrupt(what)) => set_aside_corrupt(&path, what),
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Held::default(),
            Err(err) => set_aside_corrupt(&path, format!("could not be read: {err}")),
        };
        Ok(Self {
            path,
            held: Mutex::new(held),
        })
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn contains(&self, thread: &str) -> bool {
        let held = self.held.lock();
        held.sent
            .iter()
            .chain(&held.received)
            .any(|other| other == thread)
    }

    /// Records `thread` as one this instance sent or opened, moving it out of the
    /// received list if a peer named it first.
    pub(crate) fn record_sent(&self, thread: &str) -> Result<()> {
        self.record(thread, |held| {
            let was_received = held.received.iter().any(|other| other == thread);
            if was_received {
                held.received.retain(|other| other != thread);
            }
            Held::bump(&mut held.sent, thread) || was_received
        })
    }

    /// Records `thread` as one a peer sent. A thread this instance sent stays in the sent
    /// list, only moved to its newest end, so a peer cannot make it the first evicted.
    pub(crate) fn record_received(&self, thread: &str) -> Result<()> {
        self.record(thread, |held| {
            if held.sent.iter().any(|other| other == thread) {
                Held::bump(&mut held.sent, thread)
            } else {
                Held::bump(&mut held.received, thread)
            }
        })
    }

    /// Applies `change` to the lists unless `thread` is not a wire id, then writes the
    /// file if `change` reports it changed them. The lists in memory keep the change when
    /// the write fails, so the thread can still be continued until the node stops.
    fn record(&self, thread: &str, change: impl FnOnce(&mut Held) -> bool) -> Result<()> {
        if !is_wire_id(thread) {
            return Ok(());
        }
        let mut held = self.held.lock();
        if !change(&mut held) {
            return Ok(());
        }
        held.evict_over_cap();
        let file = SentThreadsFile {
            version: SENT_THREADS_VERSION,
            sent: held.sent.iter().cloned().collect(),
            received: held.received.iter().cloned().collect(),
        };
        let json = serde_json::to_vec_pretty(&file)
            .context("Failed to serialize the mesh sent-threads list")?;
        write_atomically_owner_only(&self.path, &json)
    }
}

enum SentThreadsParse {
    Version(u64),
    Corrupt(String),
}

/// Reads the version alone first so a file from another layout is refused by its version
/// rather than set aside as corrupt for whatever field that layout added or dropped.
fn parse_sent_threads(bytes: &[u8]) -> Result<SentThreadsFile, SentThreadsParse> {
    let probe: VersionProbe = serde_json::from_slice(bytes).map_err(|err| {
        SentThreadsParse::Corrupt(format!(
            "{}: {err}",
            unversioned_cause(SENT_THREADS_VERSION)
        ))
    })?;
    if probe.version != SENT_THREADS_VERSION {
        return Err(SentThreadsParse::Version(probe.version));
    }
    serde_json::from_slice(bytes).map_err(|err| {
        SentThreadsParse::Corrupt(format!(
            "is not a version-{SENT_THREADS_VERSION} sent-threads list: {err}"
        ))
    })
}

/// Warns about a list that cannot be used, renames it to `<path>.corrupt` (replacing any
/// earlier one) and returns the empty lists the node starts with. A failed rename is only
/// warned about; the file is cache and nothing downstream depends on the move.
fn set_aside_corrupt(path: &Path, what_happened: String) -> Held {
    let aside = path.with_extension("json.corrupt");
    warn!(
        "Mesh sent-threads list '{}' {}. Starting with an empty list, so mesh__send and mesh__ask refuse an earlier thread until it is sent or received again. The file is kept at '{}'.",
        path.display(),
        redact_hashes(&what_happened),
        aside.display()
    );
    if let Err(err) = fs::rename(path, &aside) {
        warn!(
            "Mesh sent-threads list '{}' could not be moved to '{}' ({}), so it will be read and set aside again at the next start. Remove or move the file at '{}' to stop this warning.",
            path.display(),
            aside.display(),
            redact_hashes(&err.to_string()),
            path.display()
        );
    }
    Held::default()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;
    use crate::testing::{install_log_collector, warn_snapshot};

    fn store(tmp: &TempDir) -> SentThreads {
        SentThreads::open(&tmp.path, "inst").unwrap()
    }

    fn sent(store: &SentThreads) -> Vec<String> {
        store.held.lock().sent.iter().cloned().collect()
    }

    fn received(store: &SentThreads) -> Vec<String> {
        store.held.lock().received.iter().cloned().collect()
    }

    fn written(path: &Path) -> SentThreadsFile {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn the_file_is_per_instance_under_the_mesh_cache() {
        let tmp = TempDir::new("sent-threads-path");
        assert_eq!(
            store(&tmp).path(),
            tmp.path.join("mesh").join("sent-threads-inst.json")
        );
    }

    #[test]
    fn load_of_missing_file_is_empty_and_creates_nothing() {
        let tmp = TempDir::new("sent-threads-missing");

        let store = store(&tmp);

        assert!(sent(&store).is_empty());
        assert!(received(&store).is_empty());
        assert!(!tmp.path.join("mesh").exists());
    }

    #[test]
    fn recorded_threads_are_written_as_version_1_and_reload() {
        let tmp = TempDir::new("sent-threads-reload");
        let first = store(&tmp);

        first.record_sent("m-1").unwrap();
        first.record_received("t-1").unwrap();
        first.record_sent("m-2").unwrap();

        let file = written(first.path());
        assert_eq!(file.version, 1);
        assert_eq!(file.sent, vec!["m-1", "m-2"]);
        assert_eq!(file.received, vec!["t-1"]);
        let reloaded = store(&tmp);
        assert_eq!(sent(&reloaded), vec!["m-1", "m-2"]);
        assert_eq!(received(&reloaded), vec!["t-1"]);
        assert!(reloaded.contains("t-1"));
        assert!(reloaded.contains("m-2"));
        assert!(!reloaded.contains("t-2"));
    }

    #[test]
    fn a_thread_that_is_not_a_wire_id_is_not_recorded() {
        let tmp = TempDir::new("sent-threads-not-an-id");
        let store = store(&tmp);

        store.record_sent("has a space").unwrap();
        store.record_received("").unwrap();

        assert!(sent(&store).is_empty());
        assert!(received(&store).is_empty());
        assert!(
            !store.path().exists(),
            "nothing changed, so nothing is written"
        );
    }

    #[test]
    fn recording_a_held_thread_again_moves_it_to_newest_within_its_list() {
        let tmp = TempDir::new("sent-threads-move");
        let store = store(&tmp);
        for thread in ["a", "b", "c"] {
            store.record_sent(thread).unwrap();
            store.record_received(&format!("r-{thread}")).unwrap();
        }

        store.record_sent("a").unwrap();
        store.record_received("r-a").unwrap();

        assert_eq!(sent(&store), vec!["b", "c", "a"]);
        assert_eq!(received(&store), vec!["r-b", "r-c", "r-a"]);
        let file = written(store.path());
        assert_eq!(file.sent, vec!["b", "c", "a"]);
        assert_eq!(file.received, vec!["r-b", "r-c", "r-a"]);
    }

    #[test]
    fn a_sent_thread_is_never_downgraded_to_received() {
        let tmp = TempDir::new("sent-threads-no-downgrade");
        let store = store(&tmp);
        store.record_sent("m-1").unwrap();
        store.record_sent("m-2").unwrap();

        store.record_received("m-1").unwrap();

        assert_eq!(sent(&store), vec!["m-2", "m-1"]);
        assert!(received(&store).is_empty());
    }

    #[test]
    fn sending_a_received_thread_moves_it_to_the_sent_list() {
        let tmp = TempDir::new("sent-threads-upgrade");
        let store = store(&tmp);
        store.record_received("t-1").unwrap();

        store.record_sent("t-1").unwrap();

        assert_eq!(sent(&store), vec!["t-1"]);
        assert!(received(&store).is_empty());
        let file = written(store.path());
        assert_eq!(file.sent, vec!["t-1"]);
        assert!(file.received.is_empty());
    }

    #[test]
    fn recording_the_newest_thread_again_skips_the_write() {
        let tmp = TempDir::new("sent-threads-unchanged");
        let store = store(&tmp);
        store.record_sent("m-1").unwrap();
        store.record_received("t-1").unwrap();
        fs::remove_file(store.path()).unwrap();

        store.record_sent("m-1").unwrap();
        store.record_received("t-1").unwrap();
        store.record_received("m-1").unwrap();

        assert!(
            !store.path().exists(),
            "nothing changed, so nothing is written"
        );
    }

    #[test]
    fn received_threads_beyond_the_cap_evict_received_ones_and_no_sent_one() {
        let tmp = TempDir::new("sent-threads-received-flood");
        let store = store(&tmp);
        let own: Vec<String> = (0..10).map(|i| format!("m-{i}")).collect();
        for thread in &own {
            store.record_sent(thread).unwrap();
        }

        for i in 0..1100 {
            store.record_received(&format!("r-{i}")).unwrap();
        }

        assert_eq!(sent(&store), own);
        for thread in &own {
            assert!(store.contains(thread), "{thread} was evicted");
        }
        let received = received(&store);
        assert_eq!(received.len(), SENT_THREADS_MAX_ENTRIES - own.len());
        assert_eq!(
            received.last().map(String::as_str),
            Some("r-1099"),
            "the newest received thread is kept"
        );
        assert!(!store.contains("r-0"), "the oldest received is evicted");
        let file = written(store.path());
        assert_eq!(
            file.sent.len() + file.received.len(),
            SENT_THREADS_MAX_ENTRIES
        );
    }

    #[test]
    fn the_oldest_sent_thread_is_evicted_at_the_cap_once_no_received_one_is_left() {
        let tmp = TempDir::new("sent-threads-cap");
        let store = store(&tmp);
        store.record_received("r-0").unwrap();

        for i in 0..=SENT_THREADS_MAX_ENTRIES {
            store.record_sent(&format!("m-{i}")).unwrap();
        }

        let threads = sent(&store);
        assert_eq!(threads.len(), SENT_THREADS_MAX_ENTRIES);
        assert!(received(&store).is_empty(), "the received one goes first");
        assert!(!store.contains("m-0"), "then the oldest sent one");
        assert_eq!(threads[0], "m-1");
        assert_eq!(
            threads.last().map(String::as_str),
            Some(format!("m-{SENT_THREADS_MAX_ENTRIES}").as_str())
        );
        assert_eq!(written(store.path()).sent, threads);
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(umask)]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let _umask = crate::testing::UmaskGuard::zero();
        let tmp = TempDir::new("sent-threads-owner-only");
        let store = store(&tmp);

        store.record_sent("m-1").unwrap();

        let mode = fs::metadata(store.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn load_sets_aside_a_corrupt_file_and_starts_empty() {
        install_log_collector();
        let tmp = TempDir::new("sent-threads-corrupt");
        let path = tmp.path.join("sent-threads-inst.json");
        let aside = tmp.path.join("sent-threads-inst.json.corrupt");
        fs::write(&path, b"{not json").unwrap();

        let store =
            SentThreads::load(path.clone()).expect("a corrupt list must not block the load");

        assert!(sent(&store).is_empty());
        assert!(received(&store).is_empty());
        assert!(!path.exists(), "the corrupt file must be moved aside");
        assert_eq!(fs::read(&aside).unwrap(), b"{not json");
        let path_text = path.display().to_string();
        let warns = warn_snapshot();
        assert!(
            warns.iter().any(|message| message.contains(&path_text)
                && message.contains(&unversioned_cause(SENT_THREADS_VERSION))
                && message.contains(&aside.display().to_string())),
            "no warning names {path_text}; captured: {warns:#?}"
        );
    }

    /// A version this build does not write is refused outright, the file left in place:
    /// unlike a corrupt file, its remedy depends on which Coyote wrote it.
    fn assert_version_is_refused(tag: &str, found: u64, cause: &str) {
        let tmp = TempDir::new(tag);
        let path = tmp.path.join("sent-threads-inst.json");
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": found,
            "sent": ["m-1"],
            "received": [],
        }))
        .unwrap();
        fs::write(&path, &bytes).unwrap();

        let err = match SentThreads::load(path.clone()) {
            Ok(_) => panic!("a version {found} list must be refused"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains(&format!("version {found}")), "{err}");
        assert!(
            err.contains(&format!("version {SENT_THREADS_VERSION}")),
            "{err}"
        );
        assert!(err.contains(cause), "{err}");
        assert!(err.contains("move the file aside"), "{err}");
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "a refused file is left as it was"
        );
        assert!(!tmp.path.join("sent-threads-inst.json.corrupt").exists());
    }

    #[test]
    fn load_refuses_a_newer_version_naming_the_path() {
        assert_version_is_refused("sent-threads-newer", 2, "upgrade Coyote");
    }

    #[test]
    fn load_refuses_a_pre_baseline_version_as_having_no_migration() {
        assert_version_is_refused("sent-threads-older", 0, "no migration");
    }
}
