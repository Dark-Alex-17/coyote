use crate::mesh::lock::acquire_mesh_lock;
use crate::mesh::message::is_wire_id;
use crate::mesh::schema::{Remedy, VersionProbe, unversioned_cause, version_refusal};
use crate::mesh::{mesh_cache_dir, redact_hashes, write_atomically_owner_only};
use crate::utils::fs::EXCLUSIVE_LOCK_WAIT;

use anyhow::{Context, Result, bail};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Upper bound on remembered threads, sent and received together. Received threads are
/// evicted first, so a peer sending many threads cannot push out the ones this identity
/// started.
pub(crate) const SENT_THREADS_MAX_ENTRIES: usize = 1024;

pub(crate) const SENT_THREADS_VERSION: u64 = 1;

const SENT_THREADS_FILE: &str = "sent-threads.json";

/// What the lock on the list is called in a refusal.
const SENT_THREADS_LOCK_WHAT: &str = "mesh sent-threads list";

/// How long recording a received thread waits for the lock. A delivery can run on the
/// propagation fetch task, so the wait is kept short there; the holder keeps the lock
/// for one read-modify-write of a small file.
const RECEIVED_LOCK_WAIT: Duration = Duration::from_millis(250);

/// The whole file: the version first, then each list oldest to newest. Rejects unknown
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
    /// The lists a file holds, held to what `record` keeps: wire ids only, each once at
    /// its newest place, a thread in both lists kept only as sent, and no more than the
    /// cap.
    fn normalized(file: SentThreadsFile) -> Self {
        let mut held = Self::default();
        for thread in file.sent.iter().filter(|thread| is_wire_id(thread)) {
            Self::bump(&mut held.sent, thread);
        }
        for thread in file.received.iter().filter(|thread| is_wire_id(thread)) {
            if !held.sent.contains(thread) {
                Self::bump(&mut held.received, thread);
            }
        }
        held.evict_over_cap();
        held
    }

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

    fn contains(&self, thread: &str) -> bool {
        self.sent
            .iter()
            .chain(&self.received)
            .any(|other| other == thread)
    }
}

/// The threads any session on this cache dir has sent or received, in
/// `<cache_dir>/mesh/sent-threads.json`: what `mesh__send` and `mesh__ask` accept as a
/// thread to continue. One list per config dir, like the trust list and the peer table,
/// so a thread one session (or a fork of it) opened or received is continuable from any
/// other session of the same identity. Nothing is held in memory: every lookup reads the
/// file and every change is a read-modify-write, both under `<file>.lock`, so a session
/// sees a thread another one recorded after it started and no two writers lose each
/// other's change.
pub(crate) struct SentThreads {
    path: PathBuf,
    lock_path: PathBuf,
}

impl SentThreads {
    /// The list of `cache_dir`, checked as `load` checks it. The per-instance lists
    /// earlier builds kept beside it are deleted unread: they are cache, and a thread
    /// in one is continued again once the peer writes in it.
    pub(crate) fn open(cache_dir: &Path) -> Result<Self> {
        let dir = mesh_cache_dir(cache_dir);
        remove_per_instance_lists(&dir);
        Self::load(dir.join(SENT_THREADS_FILE))
    }

    /// The list at `path`. A missing file is empty and nothing is created until the
    /// first thread is recorded. A file that cannot be read or parsed, or whose version
    /// cannot be read, is moved aside to `<path>.corrupt` with a warning and the lists
    /// start empty, as the peer table does. A readable version other than
    /// `SENT_THREADS_VERSION` is refused and the file left in place.
    pub(crate) fn load(path: PathBuf) -> Result<Self> {
        let store = Self {
            lock_path: path.with_added_extension("lock"),
            path,
        };
        if store.path.exists() {
            store.locked(EXCLUSIVE_LOCK_WAIT, || store.read())?;
        }
        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Whether any session on this cache dir sent or received `thread`, read from the
    /// file under its lock. A list that cannot be read answers no, with a warning, so
    /// the tools refuse the thread rather than continue one nobody can vouch for.
    pub(crate) fn contains(&self, thread: &str) -> bool {
        match self.locked(EXCLUSIVE_LOCK_WAIT, || self.read()) {
            Ok(held) => held.contains(thread),
            Err(err) => {
                warn!(
                    "Mesh sent-threads list '{}' could not be read, so mesh__send and mesh__ask treat thread {thread} as one this identity has not seen: {}. Try the call again; if this repeats, move '{}' aside and the list starts empty.",
                    self.path.display(),
                    redact_hashes(&format!("{err:#}")),
                    self.path.display()
                );
                false
            }
        }
    }

    /// Records `thread` as one this identity sent or opened, moving it out of the
    /// received list if a peer named it first.
    pub(crate) fn record_sent(&self, thread: &str) -> Result<()> {
        self.record(thread, EXCLUSIVE_LOCK_WAIT, |held| {
            let was_received = held.received.iter().any(|other| other == thread);
            if was_received {
                held.received.retain(|other| other != thread);
            }
            Held::bump(&mut held.sent, thread) || was_received
        })
    }

    /// Records `thread` as one a peer sent. A thread this identity sent stays in the sent
    /// list, only moved to its newest end, so a peer cannot make it the first evicted.
    /// The lock is waited for only `RECEIVED_LOCK_WAIT`, since a delivery may run on an
    /// async worker.
    pub(crate) fn record_received(&self, thread: &str) -> Result<()> {
        self.record(thread, RECEIVED_LOCK_WAIT, |held| {
            if held.sent.iter().any(|other| other == thread) {
                Held::bump(&mut held.sent, thread)
            } else {
                Held::bump(&mut held.received, thread)
            }
        })
    }

    /// Under the lock, reads the lists, applies `change` unless `thread` is not a wire
    /// id, and writes the file back if `change` reports it changed them.
    fn record(
        &self,
        thread: &str,
        wait: Duration,
        change: impl FnOnce(&mut Held) -> bool,
    ) -> Result<()> {
        if !is_wire_id(thread) {
            return Ok(());
        }
        self.locked(wait, || {
            let mut held = self.read()?;
            if !change(&mut held) {
                return Ok(());
            }
            held.evict_over_cap();
            let file = SentThreadsFile {
                version: SENT_THREADS_VERSION,
                sent: held.sent.into_iter().collect(),
                received: held.received.into_iter().collect(),
            };
            let json = serde_json::to_vec(&file)
                .context("Failed to serialize the mesh sent-threads list")?;
            write_atomically_owner_only(&self.path, &json)
        })
    }

    fn locked<T>(&self, wait: Duration, body: impl FnOnce() -> Result<T>) -> Result<T> {
        let _held = acquire_mesh_lock(&self.lock_path, SENT_THREADS_LOCK_WHAT, wait)?;
        body()
    }

    /// The lists on disk, normalized as `Held::normalized` says; called under the lock.
    fn read(&self) -> Result<Held> {
        match fs::read(&self.path) {
            Ok(bytes) => match parse_sent_threads(&bytes) {
                Ok(file) => Ok(Held::normalized(file)),
                Err(SentThreadsParse::Version(found)) => bail!(version_refusal(
                    "sent-threads list",
                    &self.path,
                    None,
                    found,
                    SENT_THREADS_VERSION,
                    Remedy::Cache
                )),
                Err(SentThreadsParse::Corrupt { cause, error }) => {
                    Ok(set_aside_corrupt(&self.path, &cause, &error))
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Held::default()),
            Err(err) => Ok(set_aside_corrupt(
                &self.path,
                "could not be read",
                &err.to_string(),
            )),
        }
    }

    #[cfg(test)]
    fn snapshot(&self) -> Held {
        self.locked(EXCLUSIVE_LOCK_WAIT, || self.read()).unwrap()
    }
}

/// Deletes the `sent-threads-<instance_id>.json` lists earlier builds kept per instance
/// in `dir`. A file that cannot be deleted is only logged: nothing reads it.
fn remove_per_instance_lists(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !(name.starts_with("sent-threads-") && name.ends_with(".json")) {
            continue;
        }
        let path = entry.path();
        match fs::remove_file(&path) {
            Ok(()) => debug!(
                "Deleted the per-instance mesh sent-threads list '{}'; every session now shares '{SENT_THREADS_FILE}'",
                path.display()
            ),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => debug!(
                "The per-instance mesh sent-threads list '{}' could not be deleted ({}); nothing reads it",
                path.display(),
                redact_hashes(&err.to_string())
            ),
        }
    }
}

enum SentThreadsParse {
    Version(u64),
    /// `cause` says in plain words what is wrong with the file; `error` is the parser's
    /// own account.
    Corrupt {
        cause: String,
        error: String,
    },
}

/// Reads the version alone first so a file from another layout is refused by its version
/// rather than set aside as corrupt for whatever field that layout added or dropped.
fn parse_sent_threads(bytes: &[u8]) -> Result<SentThreadsFile, SentThreadsParse> {
    let probe: VersionProbe =
        serde_json::from_slice(bytes).map_err(|err| SentThreadsParse::Corrupt {
            cause: unversioned_cause(SENT_THREADS_VERSION),
            error: err.to_string(),
        })?;
    if probe.version != SENT_THREADS_VERSION {
        return Err(SentThreadsParse::Version(probe.version));
    }
    serde_json::from_slice(bytes).map_err(|err| SentThreadsParse::Corrupt {
        cause: format!("is not a version-{SENT_THREADS_VERSION} sent-threads list"),
        error: err.to_string(),
    })
}

/// Warns about a list that cannot be used, `cause` first and the raw `error` after it,
/// renames it to `<path>.corrupt` (replacing any earlier one) and returns the empty lists
/// the next write starts from. A failed rename is only warned about; the file is cache
/// and nothing downstream depends on the move.
fn set_aside_corrupt(path: &Path, cause: &str, error: &str) -> Held {
    let aside = path.with_extension("json.corrupt");
    warn!(
        "Mesh sent-threads list '{}' {} ({}). Starting with an empty list, so mesh__send and mesh__ask refuse an earlier thread until the peer writes in it again; omit thread to start a new conversation. Nothing else needs to be done; delete '{}' once you no longer need it.",
        path.display(),
        redact_hashes(cause),
        redact_hashes(error),
        aside.display()
    );
    if let Err(err) = fs::rename(path, &aside) {
        warn!(
            "Mesh sent-threads list '{}' could not be moved to '{}' ({}), so it will be read and set aside again at the next lookup. Remove or move the file at '{}' to stop this warning.",
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
        SentThreads::open(&tmp.path).unwrap()
    }

    fn sent(store: &SentThreads) -> Vec<String> {
        store.snapshot().sent.into_iter().collect()
    }

    fn received(store: &SentThreads) -> Vec<String> {
        store.snapshot().received.into_iter().collect()
    }

    fn written(path: &Path) -> SentThreadsFile {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn the_file_is_one_per_cache_dir_under_the_mesh_cache() {
        let tmp = TempDir::new("sent-threads-path");
        assert_eq!(
            store(&tmp).path(),
            tmp.path.join("mesh").join("sent-threads.json")
        );
    }

    #[test]
    fn load_of_missing_file_is_empty_and_creates_nothing() {
        let tmp = TempDir::new("sent-threads-missing");

        let store = store(&tmp);

        assert!(!tmp.path.join("mesh").exists(), "opening creates nothing");
        assert!(!store.contains("m-1"));
        assert!(!store.path().exists());
    }

    /// Two handles on one cache dir, both opened before either records, as two sessions
    /// of one identity are: each sees what the other recorded afterwards, without
    /// reopening, and a thread neither recorded is still unknown to both.
    #[test]
    fn handles_opened_before_a_record_see_it() {
        let tmp = TempDir::new("sent-threads-shared");
        let a = store(&tmp);
        let c = store(&tmp);

        a.record_sent("m-a").unwrap();
        c.record_received("t-c").unwrap();

        for handle in [&a, &c] {
            assert!(handle.contains("m-a"));
            assert!(handle.contains("t-c"));
            assert!(!handle.contains("m-never"));
        }
        let file = written(a.path());
        assert_eq!(file.sent, vec!["m-a"]);
        assert_eq!(file.received, vec!["t-c"]);
    }

    /// Two handles recording at once lose none of each other's threads: every change is
    /// a read-modify-write under the file's lock.
    #[test]
    fn racing_handles_lose_no_thread() {
        let tmp = TempDir::new("sent-threads-race-handles");
        let handles = [store(&tmp), store(&tmp)];
        std::thread::scope(|scope| {
            for (side, handle) in ["a", "c"].into_iter().zip(&handles) {
                scope.spawn(move || {
                    for i in 0..25 {
                        handle.record_sent(&format!("{side}-{i}")).unwrap();
                    }
                });
            }
        });

        let file = written(handles[0].path());
        assert_eq!(file.sent.len(), 2 * 25, "{:?}", file.sent);
        for side in ["a", "c"] {
            for i in 0..25 {
                assert!(handles[1].contains(&format!("{side}-{i}")));
            }
        }
    }

    #[test]
    fn opening_deletes_the_per_instance_lists_unread() {
        let tmp = TempDir::new("sent-threads-per-instance");
        let dir = tmp.path.join("mesh");
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join("sent-threads-0123456789abcdef0123456789abcdef.json");
        fs::write(
            &old,
            br#"{"version": 1, "sent": ["m-old"], "received": ["t-old"]}"#,
        )
        .unwrap();
        let aside = dir.join("sent-threads-inst.json.corrupt");
        fs::write(&aside, b"{not json").unwrap();

        let store = store(&tmp);

        assert!(!old.exists(), "the per-instance list is deleted");
        assert!(!store.contains("m-old"), "and never read");
        assert!(!store.contains("t-old"));
        assert!(aside.exists(), "a set-aside file is the user's to delete");
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

        assert!(
            !store.path().exists(),
            "nothing changed, so nothing is written"
        );
        assert!(sent(&store).is_empty());
        assert!(received(&store).is_empty());
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
        // Spaced as no write of this build spaces it, so any write would show.
        let marker = br#"{ "version": 1, "sent": ["m-1"], "received": ["t-1"] }"#;
        fs::write(store.path(), marker).unwrap();

        store.record_sent("m-1").unwrap();
        store.record_received("t-1").unwrap();
        store.record_received("m-1").unwrap();

        assert_eq!(
            fs::read(store.path()).unwrap(),
            marker,
            "nothing changed, so nothing is written"
        );
    }

    mod record_order_tests {
        use super::*;

        /// Two threads recording at once through one handle: every change is read and
        /// written under the lock, so the file ends with every thread either recorded.
        #[test]
        fn racing_records_lose_no_thread() {
            let tmp = TempDir::new("sent-threads-race");
            let store = store(&tmp);
            for round in 0..10 {
                std::thread::scope(|scope| {
                    for side in ["a", "b"] {
                        let store = &store;
                        scope.spawn(move || {
                            for i in 0..10 {
                                store.record_sent(&format!("{side}-{round}-{i}")).unwrap();
                            }
                        });
                    }
                });

                let file = written(store.path());
                assert_eq!(file.sent, sent(&store), "round {round}");
                assert_eq!(file.received, received(&store), "round {round}");
            }
            assert_eq!(sent(&store).len(), 10 * 2 * 10);
        }
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
        let path = tmp.path.join("sent-threads.json");
        let aside = tmp.path.join("sent-threads.json.corrupt");
        fs::write(&path, b"{not json").unwrap();

        let store =
            SentThreads::load(path.clone()).expect("a corrupt list must not block the load");

        assert!(!path.exists(), "the corrupt file must be moved aside");
        assert!(sent(&store).is_empty());
        assert!(received(&store).is_empty());
        assert_eq!(fs::read(&aside).unwrap(), b"{not json");
        let path_text = path.display().to_string();
        let warns = warn_snapshot();
        assert!(
            warns.iter().any(|message| message.contains(&path_text)
                && message.contains(&unversioned_cause(SENT_THREADS_VERSION))
                && message.contains(&format!(
                    "until the peer writes in it again; omit thread to start a new conversation. Nothing else needs to be done; delete '{}' once you no longer need it.",
                    aside.display()
                ))),
            "no warning names {path_text}; captured: {warns:#?}"
        );
    }

    /// A list that goes bad while a handle is open is set aside at the next lookup, which
    /// answers no; the next record starts the list afresh.
    #[test]
    fn a_list_corrupted_after_open_is_set_aside_at_the_next_lookup() {
        install_log_collector();
        let tmp = TempDir::new("sent-threads-corrupt-later");
        let store = store(&tmp);
        store.record_sent("m-1").unwrap();
        fs::write(store.path(), b"{not json").unwrap();

        assert!(!store.contains("m-1"));
        assert!(store.path().with_extension("json.corrupt").exists());
        store.record_sent("m-2").unwrap();
        assert_eq!(written(store.path()).sent, vec!["m-2"]);
    }

    /// A file with a readable version 1 but another shape is not a list this build can
    /// use: it is set aside like any corrupt file and named for what it is not.
    #[test]
    fn load_sets_aside_an_earlier_shape_under_version_1_and_starts_empty() {
        install_log_collector();
        let tmp = TempDir::new("sent-threads-earlier-shape");
        let path = tmp.path.join("sent-threads.json");
        let aside = tmp.path.join("sent-threads.json.corrupt");
        let bytes = br#"{"version":1,"threads":["m-1"]}"#;
        fs::write(&path, bytes).unwrap();

        let store =
            SentThreads::load(path.clone()).expect("an earlier shape must not block the load");

        assert!(!path.exists(), "the file must be moved aside");
        assert!(sent(&store).is_empty());
        assert!(received(&store).is_empty());
        assert!(!store.contains("m-1"));
        assert_eq!(fs::read(&aside).unwrap(), bytes);
        let path_text = path.display().to_string();
        let warns = warn_snapshot();
        assert!(
            warns.iter().any(|message| message.contains(&path_text)
                && message.contains("is not a version-1 sent-threads list (")),
            "no warning names {path_text}; captured: {warns:#?}"
        );
    }

    /// A file this build did not write as it is now is read as `record` would have
    /// left it: ids that are not wire ids go, each list keeps a thread once at its
    /// newest place, a thread in both lists stays only as sent, and the oldest received
    /// ones go first over the cap.
    #[test]
    fn load_normalizes_the_lists_it_reads() {
        let tmp = TempDir::new("sent-threads-normalize");
        let path = tmp.path.join("sent-threads.json");
        let mut received_ids = vec![
            "t-1".to_string(),
            "has a space".to_string(),
            "m-1".to_string(),
            "t-2".to_string(),
            "t-1".to_string(),
        ];
        received_ids.extend((0..SENT_THREADS_MAX_ENTRIES).map(|i| format!("r-{i}")));
        let file = SentThreadsFile {
            version: SENT_THREADS_VERSION,
            sent: vec![
                "m-1".to_string(),
                String::new(),
                "m-2".to_string(),
                "m-1".to_string(),
            ],
            received: received_ids,
        };
        fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();

        let store = SentThreads::load(path).unwrap();

        assert_eq!(sent(&store), vec!["m-2", "m-1"]);
        let received = received(&store);
        assert_eq!(received.len(), SENT_THREADS_MAX_ENTRIES - 2);
        assert!(!received.iter().any(|thread| thread == "m-1"));
        assert!(!received.iter().any(|thread| thread == "has a space"));
        assert!(
            !store.contains("t-2"),
            "the oldest received go over the cap"
        );
        assert_eq!(
            received.last().map(String::as_str),
            Some(format!("r-{}", SENT_THREADS_MAX_ENTRIES - 1).as_str())
        );
    }

    /// A version this build does not write is refused outright, the file left in place:
    /// unlike a corrupt file, its remedy depends on which Coyote wrote it.
    fn assert_version_is_refused(tag: &str, found: u64, cause: &str) {
        let tmp = TempDir::new(tag);
        let path = tmp.path.join("sent-threads.json");
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
        assert!(!tmp.path.join("sent-threads.json.corrupt").exists());
    }

    #[test]
    fn load_refuses_a_newer_version_naming_the_path() {
        assert_version_is_refused("sent-threads-newer", 2, "upgrade Coyote");
    }

    #[test]
    fn load_refuses_a_pre_baseline_version_as_having_no_migration() {
        assert_version_is_refused("sent-threads-older", 0, "no migration");
    }

    /// A list another build wrote at a version this one does not is refused at the
    /// lookup too, which answers no rather than continue a thread it cannot vouch for.
    #[test]
    fn a_newer_list_written_after_open_answers_no_and_is_left_alone() {
        install_log_collector();
        let tmp = TempDir::new("sent-threads-newer-later");
        let store = store(&tmp);
        store.record_sent("m-1").unwrap();
        let newer = br#"{"version": 2, "sent": ["m-1"], "received": []}"#;
        fs::write(store.path(), newer).unwrap();

        assert!(!store.contains("m-1"));
        assert!(store.record_sent("m-2").is_err());
        assert_eq!(fs::read(store.path()).unwrap(), newer);
    }
}
