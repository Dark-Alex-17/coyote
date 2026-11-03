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

/// Upper bound on remembered threads; the oldest is evicted first.
pub(crate) const SENT_THREADS_MAX_ENTRIES: usize = 1024;

pub(crate) const SENT_THREADS_VERSION: u64 = 1;

/// The file whole: the version first, then the threads oldest to newest. Rejects unknown
/// fields, so any change to the layout bumps `SENT_THREADS_VERSION`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SentThreadsFile {
    pub version: u64,
    pub threads: Vec<String>,
}

/// The threads this instance has sent or received, in
/// `<cache_dir>/mesh/sent-threads-<instance_id>.json`: what `mesh__send` and `mesh__ask`
/// accept as a thread to continue. Keyed by instance because a fork holds its own
/// conversations. The instance lock keeps the file to one process, so the mutex alone
/// orders the writes.
pub(crate) struct SentThreads {
    path: PathBuf,
    threads: Mutex<VecDeque<String>>,
}

impl SentThreads {
    pub(crate) fn open(cache_dir: &Path, instance_id: &str) -> Result<Self> {
        Self::load(mesh_cache_dir(cache_dir).join(format!("sent-threads-{instance_id}.json")))
    }

    /// Loads the list at `path`. A missing file is an empty list and nothing is created
    /// until the first thread is recorded. A file that cannot be read or parsed, or whose
    /// version cannot be read, is moved aside to `<path>.corrupt` with a warning and the
    /// list starts empty, as the peer table does. A readable version other than
    /// `SENT_THREADS_VERSION` is refused and the file left in place.
    pub(crate) fn load(path: PathBuf) -> Result<Self> {
        let threads = match fs::read(&path) {
            Ok(bytes) => match parse_sent_threads(&bytes) {
                Ok(threads) => threads,
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
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => set_aside_corrupt(&path, format!("could not be read: {err}")),
        };
        Ok(Self {
            path,
            threads: Mutex::new(threads.into()),
        })
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn contains(&self, thread: &str) -> bool {
        self.threads.lock().iter().any(|held| held == thread)
    }

    /// Records each of `threads` as the newest, moving one already held to the end and
    /// skipping any that is not a wire id, then writes the list if that changed it. The
    /// list in memory keeps the change when the write fails, so the thread can still be
    /// continued until the node stops.
    pub(crate) fn record(&self, threads: &[&str]) -> Result<()> {
        let mut held = self.threads.lock();
        let before = held.clone();
        for thread in threads.iter().copied().filter(|thread| is_wire_id(thread)) {
            held.retain(|other| other != thread);
            held.push_back(thread.to_string());
        }
        while held.len() > SENT_THREADS_MAX_ENTRIES {
            held.pop_front();
        }
        if *held == before {
            return Ok(());
        }
        let file = SentThreadsFile {
            version: SENT_THREADS_VERSION,
            threads: held.iter().cloned().collect(),
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
fn parse_sent_threads(bytes: &[u8]) -> Result<Vec<String>, SentThreadsParse> {
    let probe: VersionProbe = serde_json::from_slice(bytes).map_err(|err| {
        SentThreadsParse::Corrupt(format!(
            "{}: {err}",
            unversioned_cause(SENT_THREADS_VERSION)
        ))
    })?;
    if probe.version != SENT_THREADS_VERSION {
        return Err(SentThreadsParse::Version(probe.version));
    }
    let file: SentThreadsFile = serde_json::from_slice(bytes).map_err(|err| {
        SentThreadsParse::Corrupt(format!(
            "is not a version-{SENT_THREADS_VERSION} sent-threads list: {err}"
        ))
    })?;
    Ok(file.threads)
}

/// Warns about a list that cannot be used, renames it to `<path>.corrupt` (replacing any
/// earlier one) and returns the empty list the node starts with. A failed rename is only
/// warned about; the file is cache and nothing downstream depends on the move.
fn set_aside_corrupt(path: &Path, what_happened: String) -> Vec<String> {
    let aside = path.with_extension("json.corrupt");
    warn!(
        "Mesh sent-threads list '{}' {}. Starting with an empty list, so mesh__send and mesh__ask refuse an earlier thread until it is sent or received again. The file is kept at '{}'.",
        path.display(),
        redact_hashes(&what_happened),
        aside.display()
    );
    if let Err(err) = fs::rename(path, &aside) {
        warn!(
            "Failed to move the unusable mesh sent-threads list '{}' to '{}': {}",
            path.display(),
            aside.display(),
            redact_hashes(&err.to_string())
        );
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::TempDir;
    use super::*;
    use crate::testing::{install_log_collector, warn_snapshot};

    fn store(tmp: &TempDir) -> SentThreads {
        SentThreads::open(&tmp.path, "inst").unwrap()
    }

    fn held(store: &SentThreads) -> Vec<String> {
        store.threads.lock().iter().cloned().collect()
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

        assert!(held(&store).is_empty());
        assert!(!tmp.path.join("mesh").exists());
    }

    #[test]
    fn recorded_threads_are_written_as_version_1_and_reload() {
        let tmp = TempDir::new("sent-threads-reload");
        let first = store(&tmp);

        first.record(&["m-1", "t-1"]).unwrap();
        first.record(&["m-2"]).unwrap();

        let file = written(first.path());
        assert_eq!(file.version, 1);
        assert_eq!(file.threads, vec!["m-1", "t-1", "m-2"]);
        let reloaded = store(&tmp);
        assert_eq!(held(&reloaded), vec!["m-1", "t-1", "m-2"]);
        assert!(reloaded.contains("t-1"));
        assert!(!reloaded.contains("t-2"));
    }

    #[test]
    fn a_thread_that_is_not_a_wire_id_is_not_recorded() {
        let tmp = TempDir::new("sent-threads-not-an-id");
        let store = store(&tmp);

        store.record(&["has a space", ""]).unwrap();

        assert!(held(&store).is_empty());
        assert!(
            !store.path().exists(),
            "nothing changed, so nothing is written"
        );
    }

    #[test]
    fn recording_a_held_thread_again_moves_it_to_newest() {
        let tmp = TempDir::new("sent-threads-move");
        let store = store(&tmp);
        store.record(&["a", "b", "c"]).unwrap();

        store.record(&["a"]).unwrap();

        assert_eq!(held(&store), vec!["b", "c", "a"]);
        assert_eq!(written(store.path()).threads, vec!["b", "c", "a"]);
    }

    #[test]
    fn the_oldest_thread_is_evicted_at_the_cap() {
        let tmp = TempDir::new("sent-threads-cap");
        let store = store(&tmp);
        let ids: Vec<String> = (0..=SENT_THREADS_MAX_ENTRIES)
            .map(|i| format!("m-{i}"))
            .collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();

        store.record(&refs).unwrap();

        let threads = held(&store);
        assert_eq!(threads.len(), SENT_THREADS_MAX_ENTRIES);
        assert!(!store.contains("m-0"), "the oldest is evicted");
        assert_eq!(threads[0], "m-1");
        assert_eq!(
            threads.last().map(String::as_str),
            Some(format!("m-{SENT_THREADS_MAX_ENTRIES}").as_str())
        );
        assert_eq!(written(store.path()).threads, threads);
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(umask)]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let _umask = crate::testing::UmaskGuard::zero();
        let tmp = TempDir::new("sent-threads-owner-only");
        let store = store(&tmp);

        store.record(&["m-1"]).unwrap();

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

        assert!(held(&store).is_empty());
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
            "threads": ["m-1"],
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
