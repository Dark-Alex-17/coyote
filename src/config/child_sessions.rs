//! On-disk layout for sessions spawned by one coyote process on behalf of
//! another. Each spawning process owns a root directory under the cache's
//! `sessions/` tree holding a `root.yaml` identity file and a `children/`
//! directory of child session files, each paired with a `.meta.yaml`
//! sidecar describing the child's agent, status and owning process.

use crate::config::{Session, paths};
use crate::utils::fs::{
    EXCLUSIVE_LOCK_WAIT, ExclusiveLock, create_private_dir_all, refuse_symlinked_dir,
    write_private_atomic,
};
use crate::utils::process::ProcessStamp;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;
use uuid::Uuid;

/// How many leading characters of a child's prompt the sidecar keeps, so a
/// listing can show what each child was asked without loading its session.
pub(crate) const PROMPT_HEAD_CHARS: usize = 200;

/// How many runs a child's sidecar remembers; the first spawn is run 0
/// and every resume adds one, dropping the oldest past this many.
const CHILD_RUN_HISTORY: usize = 50;

const META_SUFFIX: &str = ".meta.yaml";

/// Sibling of `root.yaml` whose kernel lock serialises every
/// read-modify-write of a root directory and every decision to delete it,
/// across processes sharing the directory. See `root_lock`.
const LOCK_FILE_NAME: &str = ".lock";

pub(crate) fn sessions_root() -> PathBuf {
    paths::cache_dir().join("sessions")
}

/// Identity of the directory a spawning process keeps its children under:
/// a stable uuid when the spawner has a saved session, otherwise the
/// spawner's process stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChildSessionRoot {
    Uid(Uuid),
    Proc { pid: u32, start: u64 },
}

impl ChildSessionRoot {
    pub(crate) fn from_stamp(stamp: ProcessStamp) -> Self {
        Self::Proc {
            pid: stamp.pid,
            start: stamp.start,
        }
    }

    pub(crate) fn dir(&self) -> PathBuf {
        match self {
            Self::Uid(id) => sessions_root().join(id.simple().to_string()),
            Self::Proc { pid, start } => {
                sessions_root().join("_proc").join(format!("{pid}-{start}"))
            }
        }
    }

    pub(crate) fn children_dir(&self) -> PathBuf {
        self.dir().join("children")
    }

    pub(crate) fn root_file(&self) -> PathBuf {
        self.dir().join("root.yaml")
    }

    pub(crate) fn child_session_file(&self, session_id: &str) -> PathBuf {
        self.children_dir().join(format!("{session_id}.yaml"))
    }

    pub(crate) fn child_meta_file(&self, session_id: &str) -> PathBuf {
        self.children_dir()
            .join(format!("{session_id}{META_SUFFIX}"))
    }

    pub(crate) fn lock_file(&self) -> PathBuf {
        self.dir().join(LOCK_FILE_NAME)
    }

    /// Takes the root directory's lock for one read-modify-write of
    /// `root.yaml` or a sidecar, or for one delete decision. The lock never
    /// creates the directory, so a caller that must leave an absent
    /// directory absent only has to check for it; a directory removed
    /// between that check and the lock is reported, not resurrected.
    pub(crate) fn lock(&self) -> Result<ExclusiveLock> {
        acquire_root_lock(&self.lock_file())
    }

    /// Creates the root directory, writes `root.yaml` when it is missing
    /// or does not parse, then creates `children/`, in that order: a crash
    /// between the steps never leaves children without an identity. A
    /// `root.yaml` that parses keeps its parent paths and save time, so a
    /// second spawn under the same root cannot erase what the first
    /// recorded; only its process stamp is refreshed to `root`'s, and only
    /// when the process it names is gone, so a REPL adopting a uid left by
    /// a dead one is visible to the orphan sweep before its first save.
    /// One that does not parse is replaced from `root` with a warning,
    /// since nothing in it could be kept. Anything other than a regular
    /// file in its place is an error that leaves it as found, and a
    /// symlink at the directory's path is refused outright: children are
    /// about to be written under it. The check and the write happen under
    /// the root lock, so a second spawner, or a parent save, cannot slip a
    /// record in between them.
    pub(crate) fn ensure_dir(&self, root: &RootFile) -> Result<()> {
        let dir = self.dir();
        if refuse_symlinked_dir(&dir, "child session root") {
            bail!(
                "Refusing to use {} as a child session root: it is a symlink or cannot be inspected",
                dir.display()
            );
        }
        create_private_dir_all(&dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
        let root_file = self.root_file();
        {
            let _lock = self.lock()?;
            match root_file_disposition(&root_file, last_process(root))? {
                RootDisposition::Seed => root.save(&root_file)?,
                RootDisposition::Keep => {}
                RootDisposition::Restamp(record) => record.save(&root_file)?,
            }
        }
        let children = self.children_dir();
        create_private_dir_all(&children)
            .with_context(|| format!("Failed to create {}", children.display()))
    }

    /// Every readable sidecar under `children/`, newest first. One corrupt
    /// sidecar is skipped with a warning so it cannot hide the others.
    pub(crate) fn list_children(&self) -> Result<Vec<ChildMeta>> {
        let dir = self.children_dir();
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", dir.display()));
            }
        };
        let mut children = Vec::new();
        for entry in entries {
            let path = entry
                .with_context(|| format!("Failed to read {}", dir.display()))?
                .path();
            let is_meta = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(META_SUFFIX));
            if !is_meta {
                continue;
            }
            match ChildMeta::load(&path) {
                Ok(meta) => children.push(meta),
                Err(error) => warn!(
                    "Skipping unreadable child session sidecar {}: {error:#}",
                    path.display()
                ),
            }
        }
        children.sort_by_key(|meta| std::cmp::Reverse(meta.created_at));
        Ok(children)
    }

    /// The newest `max` children plus how many more exist. Meant for
    /// status lines, so a listing failure degrades to an empty answer.
    pub(crate) fn recent_children(&self, max: usize) -> (Vec<ChildMeta>, usize) {
        let children = match self.list_children() {
            Ok(children) => children,
            Err(error) => {
                warn!(
                    "Failed to list child sessions under {}: {error:#}",
                    self.dir().display()
                );
                return (Vec::new(), 0);
            }
        };
        let more = children.len().saturating_sub(max);
        let newest = children.into_iter().take(max).collect();
        (newest, more)
    }

    pub(crate) fn load_child_meta(&self, session_id: &str) -> Result<Option<ChildMeta>> {
        load_yaml_if_present(&self.child_meta_file(session_id))
    }

    pub(crate) fn save_child_meta(&self, meta: &ChildMeta) -> Result<()> {
        meta.save(&self.child_meta_file(&meta.session_id))
    }
}

fn default_version() -> u32 {
    1
}

/// The lock every writer and every deleter of the root directory at `dir`
/// takes, waiting the standard bound. `dir` must already exist.
fn root_lock(dir: &Path) -> Result<ExclusiveLock> {
    acquire_root_lock(&dir.join(LOCK_FILE_NAME))
}

fn acquire_root_lock(lock_file: &Path) -> Result<ExclusiveLock> {
    ExclusiveLock::acquire(lock_file, "child session root", root_lock_wait())
}

#[cfg(not(test))]
fn root_lock_wait() -> std::time::Duration {
    EXCLUSIVE_LOCK_WAIT
}

#[cfg(test)]
fn root_lock_wait() -> std::time::Duration {
    ROOT_LOCK_WAIT_OVERRIDE
        .with(|wait| wait.get())
        .unwrap_or(EXCLUSIVE_LOCK_WAIT)
}

#[cfg(test)]
thread_local! {
    static ROOT_LOCK_WAIT_OVERRIDE: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

/// Runs `f` with the root lock's wait cut to a few retries on this thread,
/// so a test that expects the held-lock refusal does not sit out the full
/// bound.
#[cfg(test)]
pub(crate) fn with_short_root_lock_wait<T>(f: impl FnOnce() -> T) -> T {
    use crate::utils::fs::EXCLUSIVE_LOCK_RETRY;
    ROOT_LOCK_WAIT_OVERRIDE.with(|wait| wait.set(Some(EXCLUSIVE_LOCK_RETRY * 3)));
    let result = f();
    ROOT_LOCK_WAIT_OVERRIDE.with(|wait| wait.set(None));
    result
}

/// What `ensure_dir` does with the `root.yaml` it finds, decided under the
/// root lock.
enum RootDisposition {
    /// Absent, or a regular file that does not hold a `RootFile`: the
    /// caller's seed is written.
    Seed,
    /// A readable record whose last writer is the caller or still alive.
    Keep,
    /// A readable record whose last writer is gone, carried over whole
    /// with the caller's stamp in its place.
    Restamp(RootFile),
}

/// A symlink at `path` is never followed and, like a directory, is refused
/// rather than written over. The liveness check runs under the lock the
/// caller holds, so the stamp it reads cannot be mid-rewrite.
fn root_file_disposition(path: &Path, caller: ProcessStamp) -> Result<RootDisposition> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RootDisposition::Seed);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}", path.display()));
        }
    };
    if !metadata.file_type().is_file() {
        bail!("{} exists but is not a regular file", path.display());
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    let mut record = match serde_yaml::from_slice::<RootFile>(&bytes) {
        Ok(record) => record,
        Err(error) => {
            warn!(
                "Replacing child session root file {}: it does not parse ({error})",
                path.display()
            );
            return Ok(RootDisposition::Seed);
        }
    };
    let recorded = last_process(&record);
    if recorded == caller || recorded.is_alive() {
        return Ok(RootDisposition::Keep);
    }
    record.last_pid = caller.pid;
    record.last_pid_start = caller.start;
    Ok(RootDisposition::Restamp(record))
}

/// Identity record at the top of a spawner's directory: which parent
/// session files it was created for and which process last wrote to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RootFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub parent_paths: Vec<PathBuf>,
    #[serde(default)]
    pub last_pid: u32,
    #[serde(default)]
    pub last_pid_start: u64,
    #[serde(default)]
    pub last_saved_at: Option<DateTime<Utc>>,
}

impl RootFile {
    pub(crate) fn load(path: &Path) -> Result<RootFile> {
        load_yaml(path)
    }

    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        save_yaml(self, path)
    }

    pub(crate) fn seed(stamp: ProcessStamp, parent_path: Option<&Path>) -> RootFile {
        Self::seed_at(Utc::now(), stamp, parent_path)
    }

    /// A fresh identity for `stamp`'s root directory. The parent session file
    /// is recorded only when it already exists on disk; a spawner that has not
    /// saved its session yet, or whose file cannot be inspected, gets a root
    /// with no parent rather than no root at all.
    pub(crate) fn seed_at(
        now: DateTime<Utc>,
        stamp: ProcessStamp,
        parent_path: Option<&Path>,
    ) -> RootFile {
        let parent = parent_path.and_then(|path| match saved_parent(path) {
            Ok(parent) => parent,
            Err(error) => {
                warn!(
                    "Failed to inspect parent session file {}: {error}",
                    path.display()
                );
                None
            }
        });
        let (parent_paths, last_saved_at) = match parent {
            Some((path, saved_at)) => (vec![path], Some(saved_at)),
            None => (Vec::new(), None),
        };
        RootFile {
            version: 1,
            created_at: now,
            parent_paths,
            last_pid: stamp.pid,
            last_pid_start: stamp.start,
            last_saved_at,
        }
    }
}

/// Canonical path and mtime of an existing parent session file, so the
/// root can later be matched to it regardless of how the path was spelled.
/// `None` when the file is not there.
fn saved_parent(path: &Path) -> io::Result<Option<(PathBuf, DateTime<Utc>)>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let modified = metadata.modified()?;
    let canonical = std::fs::canonicalize(path)?;
    Ok(Some((canonical, DateTime::<Utc>::from(modified))))
}

/// Records that the parent session `uid` was just written to `path`, so an
/// exit can tell which children were created after the last save. A uid
/// without a directory gets no bookkeeping: a parent that never spawned a
/// child leaves nothing behind. The read-modify-write of `root.yaml` runs
/// under the root lock, so two processes saving parents of one root keep
/// both records.
pub(crate) fn note_parent_saved(uid: Uuid, path: &Path) -> Result<()> {
    note_parent_saved_at(Utc::now(), ProcessStamp::current(), uid, path)
}

pub(crate) fn note_parent_saved_at(
    now: DateTime<Utc>,
    stamp: ProcessStamp,
    uid: Uuid,
    path: &Path,
) -> Result<()> {
    let root = ChildSessionRoot::Uid(uid);
    let dir = root.dir();
    if refuse_symlinked_dir(&dir, "parent session saved") || !dir.is_dir() {
        return Ok(());
    }
    let root_file = root.root_file();
    let _lock = root.lock()?;
    let mut record = load_yaml_if_present::<RootFile>(&root_file)?
        .unwrap_or_else(|| RootFile::seed_at(now, stamp, None));
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("Failed to resolve {}", path.display()))?;
    if !record.parent_paths.contains(&canonical) {
        record.parent_paths.push(canonical);
    }
    record.last_saved_at = Some(now);
    record.last_pid = stamp.pid;
    record.last_pid_start = stamp.start;
    record.save(&root_file)
}

/// Decides what of `root` survives its parent's exit, from disk state
/// alone. A process root dies with its process. A uid root dies when the
/// session owning it is declared unsaved or no parent session file
/// remains; otherwise it stays and only the children created after the
/// parent's last save go, since the saved parent has never heard of them.
/// A session whose uid is not the root's says nothing about the root. A
/// root whose identity file cannot be read is kept whole. The decision is
/// made under the root lock and the lock is let go before the directory
/// is removed, since the lock file lives inside it.
pub(crate) fn settle_parent_exit(root: &ChildSessionRoot, session: Option<&Session>) {
    let dir = root.dir();
    let uid = match root {
        ChildSessionRoot::Proc { .. } => {
            remove_dir(&dir, "process root settled");
            return;
        }
        ChildSessionRoot::Uid(uid) => *uid,
    };
    if refuse_symlinked_dir(&dir, "parent exited") {
        return;
    }
    let owner = session.filter(|session| session.uid() == Some(uid));
    if owner.map(Session::effective_save_session) == Some(Some(false)) {
        remove_dir(&dir, "session is not saved");
        return;
    }
    if !dir.is_dir() {
        return;
    }
    let lock = match root.lock() {
        Ok(lock) => lock,
        Err(error) => {
            warn!("Keeping {}: {error:#}", dir.display());
            return;
        }
    };
    let root_file = match RootFile::load(&root.root_file()) {
        Ok(root_file) => root_file,
        Err(error) => {
            warn!("Keeping {}: {error:#}", dir.display());
            return;
        }
    };
    let any_parent_exists =
        root_file
            .parent_paths
            .iter()
            .any(|path| match std::fs::metadata(path) {
                Ok(_) => true,
                Err(error) => error.kind() != io::ErrorKind::NotFound,
            });
    if !any_parent_exists {
        drop(lock);
        remove_dir(&dir, "no parent session file remains");
        return;
    }
    let children = match root.list_children() {
        Ok(children) => children,
        Err(error) => {
            warn!(
                "Failed to list child sessions under {}: {error:#}",
                dir.display()
            );
            return;
        }
    };
    for meta in children {
        let reason = match root_file.last_saved_at {
            None => "the parent was never saved",
            Some(saved_at) if meta.created_at > saved_at => "created after the last save",
            Some(_) => continue,
        };
        remove_child(root, &meta.session_id, reason);
    }
}

/// The sidecar's `session_id` is file content, so it is checked against the
/// id grammar before it becomes a path component; the grammar already
/// holds the agent segment to a charset with no separator and no `..`, so
/// the extra check below is belt and braces.
fn remove_child(root: &ChildSessionRoot, session_id: &str, reason: &str) {
    let agent_is_plain = parse_session_id(session_id).is_some_and(|(agent, _)| {
        !agent.chars().any(std::path::is_separator) && !agent.contains("..")
    });
    if !agent_is_plain {
        warn!(
            "Refusing to remove child session {session_id:?} under {} ({reason}): malformed id",
            root.dir().display()
        );
        return;
    }
    let mut removed = false;
    let mut failed = false;
    for path in [
        root.child_session_file(session_id),
        root.child_meta_file(session_id),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                failed = true;
                warn!("Failed to remove {} ({reason}): {error}", path.display());
            }
        }
    }
    if removed && failed {
        warn!(
            "Partially removed child session {session_id} under {} ({reason}): one of its files could not be removed",
            root.dir().display()
        );
    } else if removed {
        info!(
            "Removed child session {session_id} under {} ({reason})",
            root.dir().display()
        );
    }
}

/// Deletes a parent session file and forgets it in the child session root
/// its `uid` names, removing that root once no parent file refers to it
/// any more. Only the delete itself can fail; the bookkeeping degrades to
/// warnings. A file without a `uid` has no root to update, and a symlink
/// is only unlinked: the saved copy a root knows about is its target. The
/// bookkeeping runs under the root lock, released before the directory
/// itself is removed.
pub(crate) fn unlink_parent_yaml(path: &Path) -> Result<()> {
    #[derive(Deserialize)]
    struct UidProbe {
        uid: Option<Uuid>,
    }
    let is_symlink = std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false);
    if is_symlink {
        return std::fs::remove_file(path)
            .with_context(|| format!("Failed to delete {}", path.display()));
    }
    let uid = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_yaml::from_str::<UidProbe>(&text).ok())
        .and_then(|probe| probe.uid);
    let canonical = std::fs::canonicalize(path).ok();
    std::fs::remove_file(path).with_context(|| format!("Failed to delete {}", path.display()))?;
    let Some(uid) = uid else {
        return Ok(());
    };
    let root = ChildSessionRoot::Uid(uid);
    let dir = root.dir();
    if refuse_symlinked_dir(&dir, "parent session file deleted") || !dir.is_dir() {
        return Ok(());
    }
    let root_file = root.root_file();
    let lock = match root.lock() {
        Ok(lock) => lock,
        Err(error) => {
            warn!(
                "Keeping {} after deleting {}: {error:#}",
                dir.display(),
                path.display()
            );
            return Ok(());
        }
    };
    let mut record = match RootFile::load(&root_file) {
        Ok(record) => record,
        Err(error) => {
            warn!(
                "Keeping {} after deleting {}: {error:#}",
                dir.display(),
                path.display()
            );
            return Ok(());
        }
    };
    if let Some(canonical) = canonical {
        record
            .parent_paths
            .retain(|recorded| *recorded != canonical);
    }
    if let Err(error) = record.save(&root_file) {
        warn!(
            "Failed to forget {} in {}: {error:#}",
            path.display(),
            root_file.display()
        );
    }
    if record.parent_paths.is_empty() {
        drop(lock);
        remove_dir(&dir, "its last parent session file was deleted");
    }
    Ok(())
}

/// Removes a root directory, refusing to follow a symlink planted at its
/// path. Retention must never fail an exit, so every error is a warning.
/// The caller decides under the root lock and drops it before calling:
/// the lock file is inside the directory, and Windows refuses to remove a
/// directory holding an open handle.
pub(crate) fn remove_dir(path: &Path, reason: &str) {
    if refuse_symlinked_dir(path, reason) {
        return;
    }
    match std::fs::remove_dir_all(path) {
        Ok(()) => info!(
            "Removed child session directory {} ({reason})",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => warn!(
            "Failed to remove child session directory {} ({reason}): {error}",
            path.display()
        ),
    }
}

/// How long a directory without a usable `root.yaml` is left alone before
/// the orphan sweep treats it as abandoned: a spawner that has created its
/// directory but not yet written its identity must not be swept mid-start.
const ORPHAN_IDENTITY_GRACE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Removes the child session directories nobody will come back for, once
/// at REPL start. The REPL must start whatever the sweep finds: an
/// unreadable `sessions/` tree skips the whole sweep and trouble with one
/// entry skips that entry, so the sweep never fails.
pub(crate) fn sweep_orphans() {
    sweep_orphans_with(&|_| {})
}

/// `hook` runs with each directory about to be removed, before its
/// `root.yaml` is re-read, so a test can stand in for a second process
/// adopting the directory in that window.
#[cfg(test)]
pub(crate) fn sweep_orphans_with_before_remove(hook: &dyn Fn(&Path)) {
    sweep_orphans_with(hook)
}

/// What one pass of the orphan sweep decided for a directory before any
/// process was looked up. Liveness is settled for every candidate at once
/// so a pass costs one process-table refresh, not one per directory.
enum SweepAction {
    Keep,
    Remove(&'static str),
    RemoveIfDead(ProcessStamp, &'static str),
}

impl SweepAction {
    fn reason(self, alive: &HashSet<ProcessStamp>) -> Option<&'static str> {
        match self {
            Self::Keep => None,
            Self::Remove(reason) => Some(reason),
            Self::RemoveIfDead(stamp, reason) => (!alive.contains(&stamp)).then_some(reason),
        }
    }
}

fn sweep_orphans_with(before_remove: &dyn Fn(&Path)) {
    let root = sessions_root();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) => {
            debug!(
                "Skipping the child session sweep in {}: {error}",
                root.display()
            );
            return;
        }
    };
    let now = SystemTime::now();
    let mut candidates: Vec<(PathBuf, SweepAction)> = Vec::new();
    for entry in entries.flatten() {
        // DirEntry::metadata does not traverse symlinks, so a planted link
        // is skipped rather than followed.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        let dir = entry.path();
        if entry.file_name() == "_proc" {
            candidates.extend(sweep_dead_process_roots(&dir));
            continue;
        }
        // Roots are named by `Uuid::simple`, 32 hex digits; the hyphenated
        // and braced forms also parse but were never ours.
        let is_uid_name = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.len() == 32 && Uuid::parse_str(name).is_ok());
        if !is_uid_name {
            debug!("Skipping {}: not a session root", dir.display());
            continue;
        }
        let action = orphan_reason(&dir, &metadata, now);
        candidates.push((dir, action));
    }
    let stamps: Vec<ProcessStamp> = candidates
        .iter()
        .filter_map(|(_, action)| match action {
            SweepAction::RemoveIfDead(stamp, _) => Some(*stamp),
            _ => None,
        })
        .collect();
    let alive = ProcessStamp::alive_set(&stamps);
    for (dir, action) in candidates {
        if let Some(reason) = action.reason(&alive) {
            remove_unless_adopted(&dir, reason, before_remove);
        }
    }
}

/// Why a uid directory is orphaned, or `Keep`. Without a usable identity
/// file it gets an hour from its last change for its spawner to write
/// one. With parent session files recorded it goes once every one of them
/// is gone from a directory that still exists; a parent directory that is
/// itself missing may be an unmounted volume, so nothing is concluded from
/// it. With no parents recorded it belongs to the process that last wrote
/// it and goes with that process, unless a recovery file claims it. An
/// identity file that exists but cannot be read is kept and reported
/// once per process, so a second sweep in the same process stays quiet
/// about a file that is not going to change by itself.
fn orphan_reason(dir: &Path, metadata: &std::fs::Metadata, now: SystemTime) -> SweepAction {
    let root = match read_root_identity(dir) {
        Ok(root) => root,
        Err(error) => {
            warn_unreadable_root_once(dir, &error);
            return SweepAction::Keep;
        }
    };
    let Some(root) = root else {
        let age = metadata
            .modified()
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok());
        return if age.is_some_and(|age| age >= ORPHAN_IDENTITY_GRACE) {
            SweepAction::Remove("orphan sweep arm 0: no usable root.yaml after an hour")
        } else {
            SweepAction::Keep
        };
    };
    if root.parent_paths.is_empty() {
        if recovery_present(dir) {
            return SweepAction::Keep;
        }
        return SweepAction::RemoveIfDead(
            last_process(&root),
            "orphan sweep arm 2: its process is gone and no parent session file was recorded",
        );
    }
    let unreachable = root.parent_paths.iter().find(|path| {
        path.parent()
            .is_none_or(|parent| std::fs::metadata(parent).is_err())
    });
    if let Some(path) = unreachable {
        debug!(
            "Keeping {}: the directory of parent session file {} is unreachable",
            dir.display(),
            path.display()
        );
        return SweepAction::Keep;
    }
    let every_parent_gone = root.parent_paths.iter().all(|path| {
        matches!(std::fs::metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
    });
    if every_parent_gone {
        SweepAction::Remove("orphan sweep arm 1: every parent session file is gone")
    } else {
        SweepAction::Keep
    }
}

fn warn_unreadable_root_once(dir: &Path, error: &io::Error) {
    static WARNED: OnceLock<parking_lot::Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let first_time = WARNED
        .get_or_init(Default::default)
        .lock()
        .insert(dir.to_path_buf());
    if first_time {
        warn!(
            "Keeping {}: cannot read its root.yaml: {error}",
            dir.display()
        );
    } else {
        debug!(
            "Keeping {}: cannot read its root.yaml: {error}",
            dir.display()
        );
    }
}

/// The identity a uid directory carries: `Ok(None)` when `root.yaml` is
/// missing, unparseable, not text, or a directory, all of which say the
/// directory was never finished or is not ours. A read failure of any
/// other kind, permissions above all, says nothing about the directory,
/// so it is reported as `Err` for the caller to keep it.
fn read_root_identity(dir: &Path) -> io::Result<Option<RootFile>> {
    let path = dir.join("root.yaml");
    match std::fs::read_to_string(&path) {
        Ok(text) => match parse_yaml::<RootFile>(&text, &path) {
            Ok(root) => Ok(Some(root)),
            Err(error) => {
                debug!("{} has no usable identity: {error:#}", dir.display());
                Ok(None)
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            // Windows opens a directory with PermissionDenied rather than
            // IsADirectory, so the shape is checked outright.
            let is_dir_entry = std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir());
            if is_dir_entry
                || matches!(
                    error.kind(),
                    io::ErrorKind::IsADirectory | io::ErrorKind::InvalidData
                )
            {
                debug!("{} has no usable identity: {error}", dir.display());
                return Ok(None);
            }
            Err(error)
        }
    }
}

/// The exemption hook of the orphan sweep: a directory carrying a
/// `recovery.yaml` is never swept for want of a live process.
fn recovery_present(dir: &Path) -> bool {
    dir.join("recovery.yaml").exists()
}

fn last_process(root: &RootFile) -> ProcessStamp {
    ProcessStamp {
        pid: root.last_pid,
        start: root.last_pid_start,
    }
}

/// `_proc/` holds one directory per spawner without a saved session, named
/// `<pid>-<start>`; each goes with its process, decided by the caller's
/// batched liveness lookup. A name in any other shape is not one of ours
/// and is left alone.
fn sweep_dead_process_roots(proc_dir: &Path) -> Vec<(PathBuf, SweepAction)> {
    let entries = match std::fs::read_dir(proc_dir) {
        Ok(entries) => entries,
        Err(error) => {
            debug!("Skipping process roots in {}: {error}", proc_dir.display());
            return Vec::new();
        }
    };
    entries
        .flatten()
        .filter(|entry| entry.metadata().is_ok_and(|metadata| metadata.is_dir()))
        .filter_map(|entry| {
            let stamp = entry
                .file_name()
                .to_str()
                .and_then(parse_process_root_name)?;
            Some((
                entry.path(),
                SweepAction::RemoveIfDead(stamp, "orphan sweep arm 3: its process is gone"),
            ))
        })
        .collect()
}

fn parse_process_root_name(name: &str) -> Option<ProcessStamp> {
    let (pid, start) = name.split_once('-')?;
    Some(ProcessStamp {
        pid: pid.parse().ok()?,
        start: start.parse().ok()?,
    })
}

/// Removes `dir` unless its `root.yaml` now names a live process: between
/// the sweep's decision and the delete a second REPL may have adopted the
/// uid and written itself in. An identity that cannot be read is not
/// evidence either way, so the directory stays. The re-read happens under
/// the root lock, so an adopter mid-write is seen whole or not at all; the
/// lock is released before the delete because its file is inside `dir`.
fn remove_unless_adopted(dir: &Path, reason: &str, before_remove: &dyn Fn(&Path)) {
    before_remove(dir);
    let lock = match root_lock(dir) {
        Ok(lock) => lock,
        Err(error) => {
            warn!("Keeping {} ({reason}): {error:#}", dir.display());
            return;
        }
    };
    let adopted = match read_root_identity(dir) {
        Ok(root) => root.is_some_and(|root| last_process(&root).is_alive()),
        Err(error) => {
            warn!(
                "Keeping {} ({reason}): cannot read its root.yaml: {error}",
                dir.display()
            );
            return;
        }
    };
    if adopted {
        debug!(
            "Keeping {} ({reason}): a live process adopted it",
            dir.display()
        );
        return;
    }
    drop(lock);
    remove_dir(dir, reason);
}

/// Status as last recorded by the child's own process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChildStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// `ChildStatus` reconciled against whether the recording process still
/// exists: a child that says it is running but whose process is gone
/// crashed without updating its sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffectiveStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    Crashed,
}

/// One run of a child session: its first spawn or a later resume, each
/// under its own agent id and process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChildRun {
    pub agent_id: String,
    pub pid: u32,
    pub pid_start: u64,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub status: Option<ChildStatus>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Sidecar describing one child session. The top-level status, agent id
/// and process stamp mirror the last entry of `runs`; sidecars written
/// before runs were recorded load with an empty ring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChildMeta {
    #[serde(default = "default_version")]
    pub version: u32,
    pub session_id: String,
    pub agent: String,
    pub status: ChildStatus,
    pub agent_id: String,
    pub pid: u32,
    pub pid_start: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub spawner_session_id: Option<String>,
    #[serde(default)]
    pub spawner_agent_id: Option<String>,
    #[serde(default)]
    pub prompt_head: String,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub runs: Vec<ChildRun>,
}

impl ChildMeta {
    pub(crate) fn load(path: &Path) -> Result<ChildMeta> {
        load_yaml(path)
    }

    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        save_yaml(self, path)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_running(
        session_id: &str,
        agent: &str,
        agent_id: &str,
        stamp: ProcessStamp,
        spawner_session_id: Option<String>,
        spawner_agent_id: Option<String>,
        prompt: &str,
        task_id: Option<String>,
    ) -> Self {
        Self::new_running_at(
            Utc::now(),
            session_id,
            agent,
            agent_id,
            stamp,
            spawner_session_id,
            spawner_agent_id,
            prompt,
            task_id,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_running_at(
        now: DateTime<Utc>,
        session_id: &str,
        agent: &str,
        agent_id: &str,
        stamp: ProcessStamp,
        spawner_session_id: Option<String>,
        spawner_agent_id: Option<String>,
        prompt: &str,
        task_id: Option<String>,
    ) -> Self {
        Self {
            version: 1,
            session_id: session_id.into(),
            agent: agent.into(),
            status: ChildStatus::Running,
            agent_id: agent_id.into(),
            pid: stamp.pid,
            pid_start: stamp.start,
            created_at: now,
            updated_at: now,
            spawner_session_id,
            spawner_agent_id,
            prompt_head: prompt_head(prompt),
            task_id,
            error: None,
            runs: vec![ChildRun {
                agent_id: agent_id.into(),
                pid: stamp.pid,
                pid_start: stamp.start,
                started_at: now,
                finished_at: None,
                status: None,
                error: None,
            }],
        }
    }

    /// Marks the child running again under `agent_id` and `stamp` for a
    /// resume, opening a new run.
    pub(crate) fn restamp(&mut self, stamp: ProcessStamp, agent_id: &str, now: DateTime<Utc>) {
        self.status = ChildStatus::Running;
        self.agent_id = agent_id.to_string();
        self.pid = stamp.pid;
        self.pid_start = stamp.start;
        self.updated_at = now;
        self.error = None;
        self.runs.push(ChildRun {
            agent_id: agent_id.to_string(),
            pid: stamp.pid,
            pid_start: stamp.start,
            started_at: now,
            finished_at: None,
            status: None,
            error: None,
        });
        if self.runs.len() > CHILD_RUN_HISTORY {
            self.runs.drain(..self.runs.len() - CHILD_RUN_HISTORY);
        }
    }

    pub(crate) fn finish(&mut self, status: ChildStatus, error: Option<String>) {
        self.finish_at(Utc::now(), status, error);
    }

    pub(crate) fn finish_at(
        &mut self,
        now: DateTime<Utc>,
        status: ChildStatus,
        error: Option<String>,
    ) {
        self.status = status;
        if let Some(run) = self.runs.last_mut() {
            run.finished_at = Some(now);
            run.status = Some(status);
            run.error = error.clone();
        }
        self.error = error;
        self.updated_at = now;
    }

    pub(crate) fn stamp(&self) -> ProcessStamp {
        ProcessStamp {
            pid: self.pid,
            start: self.pid_start,
        }
    }

    pub(crate) fn effective_status(&self) -> EffectiveStatus {
        self.effective_status_with(|stamp| stamp.is_alive())
    }

    pub(crate) fn effective_status_with(
        &self,
        alive: impl Fn(&ProcessStamp) -> bool,
    ) -> EffectiveStatus {
        match self.status {
            ChildStatus::Running if alive(&self.stamp()) => EffectiveStatus::Running,
            ChildStatus::Running => EffectiveStatus::Crashed,
            ChildStatus::Completed => EffectiveStatus::Completed,
            ChildStatus::Failed => EffectiveStatus::Failed,
            ChildStatus::Cancelled => EffectiveStatus::Cancelled,
        }
    }
}

fn load_yaml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    parse_yaml(&text, path)
}

fn load_yaml_if_present<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_yaml(&text, path).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Failed to read {}", path.display())),
    }
}

fn parse_yaml<T: for<'de> Deserialize<'de>>(text: &str, path: &Path) -> Result<T> {
    serde_yaml::from_str(text).with_context(|| format!("Failed to parse {}", path.display()))
}

fn save_yaml<T: Serialize>(value: &T, path: &Path) -> Result<()> {
    let text = serde_yaml::to_string(value)
        .with_context(|| format!("Failed to serialize {}", path.display()))?;
    write_private_atomic(path, &text).with_context(|| format!("Failed to write {}", path.display()))
}

const SESSION_ID_PREFIX: &str = "sess_";
const SESSION_ID_HEX_LEN: usize = 12;

/// Builds `sess_<agent>_<12 hex chars>`. The agent name may itself contain
/// underscores; `parse_session_id` splits on the last one.
pub(crate) fn new_session_id(agent: &str) -> String {
    let hex: String = Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(SESSION_ID_HEX_LEN)
        .collect();
    format!("{SESSION_ID_PREFIX}{agent}_{hex}")
}

/// Whether an agent name can be the agent segment of a session id: the
/// characters an agent name can contain, never spelling a parent directory.
/// Ids arrive from tool arguments and are joined into file names, so the
/// same rule gates both minting an id and parsing one.
pub(crate) fn agent_name_fits_session_id(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Splits a `new_session_id` result back into `(agent, hex)`; `None` for
/// anything not in that shape.
pub(crate) fn parse_session_id(id: &str) -> Option<(&str, &str)> {
    let rest = id.strip_prefix(SESSION_ID_PREFIX)?;
    let (agent, hex) = rest.rsplit_once('_')?;
    let agent_ok = agent_name_fits_session_id(agent);
    let hex_ok = hex.len() == SESSION_ID_HEX_LEN
        && hex
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    (agent_ok && hex_ok).then_some((agent, hex))
}

pub(crate) fn prompt_head(prompt: &str) -> String {
    prompt.chars().take(PROMPT_HEAD_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        CacheDirGuard, TestConfigDirGuard, info_snapshot, install_log_collector, warn_snapshot,
    };
    use chrono::{Duration, TimeZone};
    use fancy_regex::Regex;
    use serial_test::serial;
    use std::fs;

    fn fixture_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
    }

    fn fixture_root() -> RootFile {
        RootFile {
            version: 1,
            created_at: fixture_time(),
            parent_paths: vec![PathBuf::from("/tmp/a.yaml"), PathBuf::from("/tmp/b.yaml")],
            last_pid: 1234,
            last_pid_start: 1_700_000_000,
            last_saved_at: Some(fixture_time()),
        }
    }

    fn fixture_meta(status: ChildStatus) -> ChildMeta {
        ChildMeta {
            version: 1,
            session_id: "sess_coder_0123456789ab".into(),
            agent: "coder".into(),
            status,
            agent_id: "agent-1".into(),
            pid: 4242,
            pid_start: 1_700_000_000,
            created_at: fixture_time(),
            updated_at: fixture_time(),
            spawner_session_id: Some("sess_root_abcdefabcdef".into()),
            spawner_agent_id: Some("agent-0".into()),
            prompt_head: "do the thing".into(),
            task_id: Some("task-7".into()),
            error: Some("boom".into()),
        }
    }

    fn fixture_meta_at(session_id: &str, offset_secs: i64) -> ChildMeta {
        let mut meta = fixture_meta(ChildStatus::Running);
        meta.session_id = session_id.into();
        meta.created_at = fixture_time() + Duration::seconds(offset_secs);
        meta.updated_at = meta.created_at;
        meta
    }

    fn fixture_stamp() -> ProcessStamp {
        ProcessStamp {
            pid: 77,
            start: 1_700_000_000,
        }
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// `dir` reached through a symlink, so canonicalising visibly changes
    /// the path.
    #[cfg(unix)]
    fn aliased(dir: &Path) -> PathBuf {
        let link = dir.with_file_name("link");
        std::os::unix::fs::symlink(dir, &link).unwrap();
        link
    }

    #[cfg(not(unix))]
    fn aliased(dir: &Path) -> PathBuf {
        dir.to_path_buf()
    }

    /// A real file under the cache guard's root, so it canonicalises.
    fn parent_file(cache: &CacheDirGuard, name: &str) -> PathBuf {
        let path = cache.root.join(name);
        fs::write(&path, "id: x\n").unwrap();
        path
    }

    /// A child's sidecar plus a session file with some bytes, so the pair
    /// is observable on disk.
    fn write_child(root: &ChildSessionRoot, session_id: &str, offset_secs: i64) {
        root.save_child_meta(&fixture_meta_at(session_id, offset_secs))
            .unwrap();
        fs::write(root.child_session_file(session_id), "messages: []\n").unwrap();
    }

    fn child_present(root: &ChildSessionRoot, session_id: &str) -> bool {
        let meta = root.child_meta_file(session_id).exists();
        let session = root.child_session_file(session_id).exists();
        assert_eq!(
            meta, session,
            "{session_id}: sidecar and session file must go together"
        );
        meta
    }

    #[test]
    fn session_id_round_trips() {
        let id = new_session_id("coder");
        let re = Regex::new(r"^sess_coder_[0-9a-f]{12}$").unwrap();
        assert!(re.is_match(&id).unwrap(), "{id}");
        let (agent, hex) = parse_session_id(&id).unwrap();
        assert_eq!(agent, "coder");
        assert_eq!(id, format!("sess_coder_{hex}"));

        assert_eq!(
            parse_session_id("sess_my_agent-x_0123456789ab"),
            Some(("my_agent-x", "0123456789ab"))
        );
    }

    #[test]
    fn session_id_rejects_malformed() {
        for bad in [
            "agent_x_1234abcd",
            "sess_x_abc",
            "sess__abcdefabcdef",
            "sess_x_0123456789AB",
            "sess_x_0123456789abc",
            "sess_../x_0123456789ab",
            "sess_a/b_0123456789ab",
            "sess_a\\b_0123456789ab",
            "sess_a..b_0123456789ab",
            "sess_a b_0123456789ab",
        ] {
            assert_eq!(parse_session_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn session_id_agent_segment_allows_the_agent_name_characters() {
        assert_eq!(
            parse_session_id("sess_my_agent-v1.2_0123456789ab"),
            Some(("my_agent-v1.2", "0123456789ab"))
        );
        for agent in ["my_agent", "agent-x", "v1.2", "A_b-C.d"] {
            let id = new_session_id(agent);
            assert_eq!(parse_session_id(&id).map(|(a, _)| a), Some(agent), "{id}");
        }
    }

    /// The name gate agrees with the parser: every name it accepts mints an
    /// id that parses back to it, and every name it refuses would not.
    #[test]
    fn agent_name_fits_session_id_mirrors_the_parser() {
        for agent in ["coder", "my_agent", "agent-x", "v1.2", "A_b-C.d", "a.b"] {
            assert!(agent_name_fits_session_id(agent), "{agent}");
            assert_eq!(
                parse_session_id(&new_session_id(agent)).map(|(a, _)| a),
                Some(agent),
                "{agent}"
            );
        }
        for agent in ["", "..", "a..b", "a/b", "a\\b", "a b", "a+b", "caf\u{e9}"] {
            assert!(!agent_name_fits_session_id(agent), "{agent:?}");
            assert_eq!(
                parse_session_id(&format!("sess_{agent}_0123456789ab")),
                None,
                "{agent:?}"
            );
        }
    }

    #[test]
    fn root_file_round_trips() {
        let root = fixture_root();
        let yaml = serde_yaml::to_string(&root).unwrap();
        let back: RootFile = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, root);
    }

    #[test]
    fn child_meta_round_trips() {
        let meta = fixture_meta(ChildStatus::Failed);
        let yaml = serde_yaml::to_string(&meta).unwrap();
        let back: ChildMeta = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, meta);
    }

    #[test]
    fn uuid_round_trips_through_yaml() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Carrier {
            id: Uuid,
        }
        let carrier = Carrier { id: Uuid::new_v4() };
        let yaml = serde_yaml::to_string(&carrier).unwrap();
        let back: Carrier = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, carrier);
    }

    #[test]
    fn minimal_root_file_loads_with_defaults() {
        let from_version: RootFile = serde_yaml::from_str("version: 1\n").unwrap();
        let from_empty: RootFile = serde_yaml::from_str("{}\n").unwrap();
        for root in [from_version, from_empty] {
            assert_eq!(root.version, 1);
            assert_eq!(root.created_at, DateTime::<Utc>::default());
            assert!(root.parent_paths.is_empty());
            assert_eq!(root.last_pid, 0);
            assert_eq!(root.last_pid_start, 0);
            assert_eq!(root.last_saved_at, None);
        }
    }

    #[test]
    fn minimal_child_meta_loads_with_defaults() {
        let yaml = "\
session_id: sess_coder_0123456789ab
agent: coder
status: running
agent_id: agent-1
pid: 4242
pid_start: 1700000000
created_at: 2026-01-02T03:04:05Z
updated_at: 2026-01-02T03:04:05Z
";
        let meta: ChildMeta = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(meta.version, 1);
        assert_eq!(meta.status, ChildStatus::Running);
        assert_eq!(meta.created_at, fixture_time());
        assert_eq!(meta.spawner_session_id, None);
        assert_eq!(meta.spawner_agent_id, None);
        assert_eq!(meta.prompt_head, "");
        assert_eq!(meta.task_id, None);
        assert_eq!(meta.error, None);
    }

    #[test]
    fn effective_status_reconciles_liveness() {
        let running = fixture_meta(ChildStatus::Running);
        assert_eq!(
            running.effective_status_with(|_| false),
            EffectiveStatus::Crashed
        );
        assert_eq!(
            running.effective_status_with(|_| true),
            EffectiveStatus::Running
        );
        for (status, expected) in [
            (ChildStatus::Completed, EffectiveStatus::Completed),
            (ChildStatus::Failed, EffectiveStatus::Failed),
            (ChildStatus::Cancelled, EffectiveStatus::Cancelled),
        ] {
            let meta = fixture_meta(status);
            assert_eq!(meta.effective_status_with(|_| false), expected);
        }
    }

    #[test]
    #[serial]
    fn root_paths_follow_layout() {
        let _cache = CacheDirGuard::new("child-sessions-paths");
        let id = Uuid::new_v4();
        let uid_root = ChildSessionRoot::Uid(id);
        assert!(
            uid_root
                .dir()
                .ends_with(Path::new("sessions").join(id.simple().to_string()))
        );

        let proc_root = ChildSessionRoot::Proc {
            pid: 77,
            start: 1_700_000_000,
        };
        assert!(
            proc_root
                .dir()
                .ends_with(Path::new("sessions").join("_proc").join("77-1700000000"))
        );

        let sid = "sess_a_0123456789ab";
        assert!(
            proc_root
                .child_meta_file(sid)
                .ends_with(Path::new("children").join("sess_a_0123456789ab.meta.yaml"))
        );
        assert!(
            proc_root
                .child_session_file(sid)
                .ends_with(Path::new("children").join("sess_a_0123456789ab.yaml"))
        );
        assert!(proc_root.root_file().ends_with("root.yaml"));
    }

    #[test]
    #[serial]
    fn ensure_dir_creates_private_layout() {
        let _cache = CacheDirGuard::new("child-sessions-ensure");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let file = fixture_root();
        root.ensure_dir(&file).unwrap();

        assert!(root.dir().is_dir());
        assert!(root.children_dir().is_dir());
        assert_eq!(RootFile::load(&root.root_file()).unwrap(), file);
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&root.dir()), 0o700);
            assert_eq!(mode_of(&root.root_file()), 0o600);
            assert_eq!(mode_of(&root.children_dir()), 0o700);
        }
    }

    #[test]
    #[serial]
    fn ensure_dir_writes_root_file_before_children_dir() {
        let _cache = CacheDirGuard::new("child-sessions-order");
        let root = ChildSessionRoot::Proc {
            pid: 77,
            start: 1_700_000_000,
        };
        fs::create_dir_all(root.dir()).unwrap();
        fs::write(root.children_dir(), "not a directory").unwrap();

        let file = fixture_root();
        assert!(root.ensure_dir(&file).is_err());
        assert_eq!(RootFile::load(&root.root_file()).unwrap(), file);
    }

    /// The first spawner is this process, so the record names a live
    /// writer and a second spawn with another seed changes nothing.
    #[test]
    #[serial]
    fn ensure_dir_keeps_an_existing_root_file_byte_for_byte() {
        let _cache = CacheDirGuard::new("child-sessions-ensure-twice");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let mut first = fixture_root();
        first.last_pid = ProcessStamp::current().pid;
        first.last_pid_start = ProcessStamp::current().start;
        root.ensure_dir(&first).unwrap();
        let before = fs::read(root.root_file()).unwrap();

        let mut other = fixture_root();
        other.parent_paths.clear();
        other.last_saved_at = None;
        other.last_pid = 999;
        root.ensure_dir(&other).unwrap();

        assert_eq!(fs::read(root.root_file()).unwrap(), before);
        assert!(root.children_dir().is_dir());
    }

    #[test]
    #[serial]
    fn ensure_dir_replaces_an_unparseable_root_file_but_refuses_a_directory() {
        install_log_collector();
        let _cache = CacheDirGuard::new("child-sessions-ensure-garbage");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let root_file = root.root_file();
        let path_text = root_file.display().to_string();
        let mut seed = fixture_root();
        seed.last_pid = ProcessStamp::current().pid;
        seed.last_pid_start = ProcessStamp::current().start;
        let replaced = |needle: &str| {
            warn_snapshot().iter().any(|message| {
                message.contains("Replacing child session root file")
                    && message.contains(&path_text)
                    && message.contains(needle)
            })
        };

        // Absent: written from the seed, nothing to warn about.
        root.ensure_dir(&seed).unwrap();
        assert_eq!(RootFile::load(&root_file).unwrap(), seed);
        assert!(!replaced(""), "{:?}", warn_snapshot());

        // Garbage that is not even UTF-8: replaced from the seed, once.
        fs::write(&root_file, b"\x00\xff: [not: yaml").unwrap();
        root.ensure_dir(&seed).unwrap();
        assert_eq!(RootFile::load(&root_file).unwrap(), seed);
        assert!(replaced("does not parse"), "{:?}", warn_snapshot());
        #[cfg(unix)]
        assert_eq!(mode_of(&root_file), 0o600);

        // Parseable and naming a live writer: kept byte for byte even
        // against a different seed.
        let before = fs::read(&root_file).unwrap();
        let mut other = seed.clone();
        other.last_pid = 999;
        root.ensure_dir(&other).unwrap();
        assert_eq!(fs::read(&root_file).unwrap(), before);

        // A directory in its place: an error naming the path, left as found.
        fs::remove_file(&root_file).unwrap();
        fs::create_dir(&root_file).unwrap();
        fs::write(root_file.join("marker"), "x").unwrap();
        let error = root.ensure_dir(&seed).unwrap_err();
        let chain = format!("{error:#}");
        assert!(chain.contains(&path_text), "{chain}");
        assert!(chain.contains("not a regular file"), "{chain}");
        assert!(root_file.is_dir());
        assert!(root_file.join("marker").is_file());
        let leftovers: Vec<_> = fs::read_dir(root.dir())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// A symlink where `root.yaml` goes is neither read through nor written
    /// over: `ensure_dir` refuses it and the target keeps its bytes.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn ensure_dir_refuses_a_symlinked_root_file() {
        let cache = CacheDirGuard::new("child-sessions-ensure-link");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        fs::create_dir_all(root.dir()).unwrap();
        let target = cache.root.join("elsewhere.yaml");
        fs::write(&target, "version: 1\n").unwrap();
        std::os::unix::fs::symlink(&target, root.root_file()).unwrap();

        let error = root.ensure_dir(&fixture_root()).unwrap_err();
        assert!(format!("{error:#}").contains("not a regular file"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "version: 1\n");
        assert!(
            fs::symlink_metadata(root.root_file())
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    /// A record whose stamp is dead and differs from the caller's, so the
    /// caller is adopting a directory its writer left behind.
    fn abandoned_record() -> RootFile {
        let mut record = fixture_root();
        record.parent_paths = vec![PathBuf::from("/tmp/left-behind.yaml")];
        record.last_pid = dead_stamp().pid;
        record.last_pid_start = dead_stamp().start;
        record
    }

    fn replacement_warned(root: &ChildSessionRoot) -> bool {
        let path_text = root.root_file().display().to_string();
        warn_snapshot().iter().any(|message| {
            message.contains("Replacing child session root file") && message.contains(&path_text)
        })
    }

    #[test]
    #[serial]
    fn ensure_dir_restamps_a_record_whose_writer_is_dead_and_keeps_the_rest() {
        install_log_collector();
        let _cache = CacheDirGuard::new("child-sessions-ensure-restamp");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        fs::create_dir_all(root.dir()).unwrap();
        let abandoned = abandoned_record();
        write_root(&root.dir(), &abandoned);
        let current = ProcessStamp::current();

        root.ensure_dir(&RootFile::seed_at(Utc::now(), current, None))
            .unwrap();

        let mut expected = abandoned;
        expected.last_pid = current.pid;
        expected.last_pid_start = current.start;
        assert_eq!(RootFile::load(&root.root_file()).unwrap(), expected);
        assert!(!replacement_warned(&root), "{:?}", warn_snapshot());
    }

    #[test]
    #[serial]
    fn ensure_dir_leaves_a_record_whose_writer_is_alive_byte_for_byte() {
        install_log_collector();
        let _cache = CacheDirGuard::new("child-sessions-ensure-live-writer");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        fs::create_dir_all(root.dir()).unwrap();
        let mut live = abandoned_record();
        live.last_pid = ProcessStamp::current().pid;
        live.last_pid_start = ProcessStamp::current().start;
        write_root(&root.dir(), &live);
        let before = fs::read(root.root_file()).unwrap();
        let foreign = ProcessStamp {
            pid: 4242,
            start: 1,
        };

        root.ensure_dir(&RootFile::seed_at(Utc::now(), foreign, None))
            .unwrap();

        assert_eq!(fs::read(root.root_file()).unwrap(), before);
        assert!(!replacement_warned(&root), "{:?}", warn_snapshot());
    }

    #[test]
    #[serial]
    fn ensure_dir_leaves_a_record_already_carrying_the_callers_stamp_byte_for_byte() {
        install_log_collector();
        let _cache = CacheDirGuard::new("child-sessions-ensure-same-stamp");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        fs::create_dir_all(root.dir()).unwrap();
        write_root(&root.dir(), &abandoned_record());
        let before = fs::read(root.root_file()).unwrap();

        root.ensure_dir(&RootFile::seed_at(Utc::now(), dead_stamp(), None))
            .unwrap();

        assert_eq!(fs::read(root.root_file()).unwrap(), before);
        assert!(!replacement_warned(&root), "{:?}", warn_snapshot());
    }

    /// A symlink where the root directory goes is refused before anything
    /// is created, so nothing lands in the link's target.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn ensure_dir_refuses_a_symlinked_root_dir_and_writes_nothing_through_it() {
        let cache = CacheDirGuard::new("child-sessions-ensure-link-dir");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let target = cache.root.join("elsewhere");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(sessions_root()).unwrap();
        std::os::unix::fs::symlink(&target, root.dir()).unwrap();

        let error = root.ensure_dir(&fixture_root()).unwrap_err();

        assert!(
            format!("{error:#}").contains("Refusing to use"),
            "{error:#}"
        );
        assert!(
            fs::symlink_metadata(root.dir())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            0,
            "nothing may be written through the link"
        );
    }

    #[test]
    #[serial]
    fn note_parent_saved_at_accumulates_each_parent_path_once() {
        let cache = CacheDirGuard::new("child-sessions-note-saved");
        let uid = Uuid::new_v4();
        let root = ChildSessionRoot::Uid(uid);
        let a = parent_file(&cache, "a.yaml");
        let b = parent_file(&cache, "b.yaml");
        let seeded = RootFile::seed_at(fixture_time(), fixture_stamp(), None);
        let first_save = fixture_time() + Duration::seconds(30);
        let second_save = fixture_time() + Duration::seconds(60);
        let later_stamp = ProcessStamp {
            pid: 78,
            start: 1_700_000_001,
        };

        root.ensure_dir(&seeded).unwrap();
        note_parent_saved_at(first_save, fixture_stamp(), uid, &a).unwrap();
        root.ensure_dir(&seeded).unwrap();
        note_parent_saved_at(second_save, later_stamp, uid, &b).unwrap();
        note_parent_saved_at(second_save, later_stamp, uid, &a).unwrap();

        let record = RootFile::load(&root.root_file()).unwrap();
        assert_eq!(
            record.parent_paths,
            vec![fs::canonicalize(&a).unwrap(), fs::canonicalize(&b).unwrap()]
        );
        assert_eq!(record.last_saved_at, Some(second_save));
        assert_eq!(record.last_pid, 78);
        assert_eq!(record.last_pid_start, 1_700_000_001);
        assert_eq!(record.created_at, fixture_time());
    }

    #[test]
    #[serial]
    fn note_parent_saved_at_without_a_root_dir_writes_nothing() {
        let cache = CacheDirGuard::new("child-sessions-note-absent");
        let uid = Uuid::new_v4();
        let a = parent_file(&cache, "a.yaml");

        note_parent_saved_at(fixture_time(), fixture_stamp(), uid, &a).unwrap();

        assert!(!ChildSessionRoot::Uid(uid).dir().exists());
        assert!(!sessions_root().exists());
    }

    #[test]
    #[serial]
    fn note_parent_saved_at_seeds_a_missing_root_file() {
        let cache = CacheDirGuard::new("child-sessions-note-seed");
        let uid = Uuid::new_v4();
        let root = ChildSessionRoot::Uid(uid);
        let a = parent_file(&cache, "a.yaml");
        fs::create_dir_all(root.dir()).unwrap();

        note_parent_saved_at(fixture_time(), fixture_stamp(), uid, &a).unwrap();

        let record = RootFile::load(&root.root_file()).unwrap();
        assert_eq!(record.parent_paths, vec![fs::canonicalize(&a).unwrap()]);
        assert_eq!(record.last_saved_at, Some(fixture_time()));
        assert_eq!(record.created_at, fixture_time());
        assert_eq!(record.last_pid, 77);
    }

    #[test]
    fn root_file_load_errors_name_the_path() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "coyote-child-sessions-load-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();

        let missing = dir.join("missing.yaml");
        let error = RootFile::load(&missing).unwrap_err();
        assert!(format!("{error:#}").contains(&missing.display().to_string()));

        let garbage = dir.join("garbage.yaml");
        fs::write(&garbage, "not: [valid").unwrap();
        let error = RootFile::load(&garbage).unwrap_err();
        assert!(format!("{error:#}").contains(&garbage.display().to_string()));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn from_stamp_is_the_proc_root() {
        assert_eq!(
            ChildSessionRoot::from_stamp(fixture_stamp()),
            ChildSessionRoot::Proc {
                pid: 77,
                start: 1_700_000_000,
            }
        );
    }

    #[test]
    #[serial]
    fn list_children_skips_unparseable_and_sorts_newest_first() {
        let _cache = CacheDirGuard::new("child-sessions-list");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        root.ensure_dir(&fixture_root()).unwrap();
        let oldest = fixture_meta_at("sess_a_0123456789ab", 0);
        let middle = fixture_meta_at("sess_b_0123456789ab", 10);
        let newest = fixture_meta_at("sess_c_0123456789ab", 20);
        for meta in [&oldest, &middle, &newest] {
            root.save_child_meta(meta).unwrap();
        }
        fs::write(root.child_meta_file(&middle.session_id), "not: [valid").unwrap();

        assert_eq!(root.list_children().unwrap(), vec![newest, oldest]);
    }

    #[test]
    #[serial]
    fn recent_children_returns_newest_and_counts_the_rest() {
        let _cache = CacheDirGuard::new("child-sessions-recent");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        root.ensure_dir(&fixture_root()).unwrap();
        for (sid, offset) in [
            ("sess_a_0123456789ab", 0),
            ("sess_b_0123456789ab", 10),
            ("sess_c_0123456789ab", 20),
        ] {
            root.save_child_meta(&fixture_meta_at(sid, offset)).unwrap();
        }
        let ids_of = |children: Vec<ChildMeta>| -> Vec<String> {
            children.into_iter().map(|meta| meta.session_id).collect()
        };

        let (children, more) = root.recent_children(2);
        assert_eq!(
            ids_of(children),
            vec!["sess_c_0123456789ab", "sess_b_0123456789ab"]
        );
        assert_eq!(more, 1);

        let (children, more) = root.recent_children(5);
        assert_eq!(
            ids_of(children),
            vec![
                "sess_c_0123456789ab",
                "sess_b_0123456789ab",
                "sess_a_0123456789ab"
            ]
        );
        assert_eq!(more, 0);

        let absent = ChildSessionRoot::Uid(Uuid::new_v4());
        assert_eq!(absent.recent_children(2), (Vec::new(), 0));
    }

    #[test]
    #[serial]
    fn load_child_meta_missing_is_none() {
        let _cache = CacheDirGuard::new("child-sessions-load-missing");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        assert_eq!(root.load_child_meta("sess_x_0123456789ab").unwrap(), None);

        root.ensure_dir(&fixture_root()).unwrap();
        assert_eq!(root.load_child_meta("sess_x_0123456789ab").unwrap(), None);
    }

    #[test]
    #[serial]
    fn save_and_load_child_meta_round_trip_through_private_file() {
        let _cache = CacheDirGuard::new("child-sessions-round-trip");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        root.ensure_dir(&fixture_root()).unwrap();
        let meta = fixture_meta(ChildStatus::Failed);
        root.save_child_meta(&meta).unwrap();

        assert_eq!(
            root.load_child_meta(&meta.session_id).unwrap(),
            Some(meta.clone())
        );
        #[cfg(unix)]
        assert_eq!(mode_of(&root.child_meta_file(&meta.session_id)), 0o600);
    }

    #[test]
    #[serial]
    fn seed_records_canonical_parent_path_and_mtime() {
        let cache = CacheDirGuard::new("child-sessions-seed");
        let real = cache.root.join("real");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("parent.yaml"), "id: x\n").unwrap();
        let path = aliased(&real).join("parent.yaml");

        let root = RootFile::seed_at(fixture_time(), fixture_stamp(), Some(&path));

        assert_eq!(root.version, 1);
        assert_eq!(root.created_at, fixture_time());
        assert_eq!(root.parent_paths, vec![fs::canonicalize(&path).unwrap()]);
        assert_eq!(
            root.last_saved_at,
            Some(DateTime::<Utc>::from(
                fs::metadata(&path).unwrap().modified().unwrap()
            ))
        );
        assert_eq!(root.last_pid, 77);
        assert_eq!(root.last_pid_start, 1_700_000_000);
    }

    #[test]
    #[serial]
    fn seed_without_parent_records_nothing() {
        let cache = CacheDirGuard::new("child-sessions-seed-none");
        let missing = cache.root.join("missing.yaml");
        for parent in [None, Some(missing.as_path())] {
            let root = RootFile::seed_at(fixture_time(), fixture_stamp(), parent);
            assert!(root.parent_paths.is_empty(), "{parent:?}");
            assert_eq!(root.last_saved_at, None, "{parent:?}");
            assert_eq!(root.last_pid, 77);
            assert_eq!(root.last_pid_start, 1_700_000_000);
        }
    }

    #[test]
    fn new_running_at_applies_prompt_head_and_equal_timestamps() {
        let prompt = "x".repeat(300);
        let meta = ChildMeta::new_running_at(
            fixture_time(),
            "sess_coder_0123456789ab",
            "coder",
            "agent-1",
            fixture_stamp(),
            Some("sess_root_abcdefabcdef".into()),
            Some("agent-0".into()),
            &prompt,
            Some("task-7".into()),
        );

        assert_eq!(meta.version, 1);
        assert_eq!(meta.session_id, "sess_coder_0123456789ab");
        assert_eq!(meta.agent, "coder");
        assert_eq!(meta.agent_id, "agent-1");
        assert_eq!(meta.status, ChildStatus::Running);
        assert_eq!(meta.pid, 77);
        assert_eq!(meta.pid_start, 1_700_000_000);
        assert_eq!(meta.created_at, fixture_time());
        assert_eq!(meta.updated_at, fixture_time());
        assert_eq!(
            meta.spawner_session_id.as_deref(),
            Some("sess_root_abcdefabcdef")
        );
        assert_eq!(meta.spawner_agent_id.as_deref(), Some("agent-0"));
        assert_eq!(meta.prompt_head.chars().count(), PROMPT_HEAD_CHARS);
        assert_eq!(meta.task_id.as_deref(), Some("task-7"));
        assert_eq!(meta.error, None);
    }

    #[test]
    fn finish_at_sets_status_error_and_bumps_updated_at() {
        let mut meta = fixture_meta_at("sess_coder_0123456789ab", 0);
        meta.error = None;
        let later = fixture_time() + Duration::seconds(5);

        meta.finish_at(later, ChildStatus::Failed, Some("boom".into()));

        assert_eq!(meta.status, ChildStatus::Failed);
        assert_eq!(meta.error.as_deref(), Some("boom"));
        assert_eq!(meta.updated_at, later);
        assert!(meta.updated_at > meta.created_at);
        assert_eq!(meta.created_at, fixture_time());
    }

    #[test]
    fn prompt_head_truncates_by_char() {
        let ascii = "x".repeat(300);
        assert_eq!(prompt_head(&ascii).chars().count(), PROMPT_HEAD_CHARS);

        let wide = "日".repeat(250);
        let head = prompt_head(&wide);
        assert_eq!(head.chars().count(), PROMPT_HEAD_CHARS);
        assert!(head.chars().all(|c| c == '日'));

        assert_eq!(prompt_head("short"), "short");
    }

    #[test]
    #[serial]
    fn remove_dir_removes_a_real_directory_and_ignores_a_missing_one() {
        let cache = CacheDirGuard::new("child-sessions-remove-dir");
        let dir = cache.root.join("doomed");
        fs::create_dir_all(dir.join("children")).unwrap();
        fs::write(dir.join("root.yaml"), "version: 1\n").unwrap();

        remove_dir(&dir, "test");
        assert!(!dir.exists());

        remove_dir(&dir, "test");
        assert!(!dir.exists());
    }

    /// Every removal names the uid (the directory path carries it) and the
    /// reason at info level; a settle that removes nothing logs nothing.
    #[test]
    #[serial]
    fn every_retention_deletion_logs_the_uid_and_reason_at_info() {
        install_log_collector();
        let cache = CacheDirGuard::new("child-sessions-info-log");
        let uid = Uuid::new_v4();
        let uid_text = uid.simple().to_string();
        let root = ChildSessionRoot::Uid(uid);
        let mut record = fixture_root();
        record.parent_paths = vec![fs::canonicalize(parent_file(&cache, "kept.yaml")).unwrap()];
        root.ensure_dir(&record).unwrap();
        write_child(&root, "sess_old_0123456789ab", -10);
        write_child(&root, "sess_new_0123456789ab", 10);
        let logged = |needle: &str| {
            info_snapshot()
                .iter()
                .any(|message| message.contains(&uid_text) && message.contains(needle))
        };
        let own_lines = || {
            info_snapshot()
                .iter()
                .filter(|message| message.contains(&uid_text))
                .count()
        };

        settle_parent_exit(&root, None);
        assert!(
            logged("sess_new_0123456789ab")
                && logged("created after the last save")
                && !logged("sess_old_0123456789ab"),
            "{:?}",
            info_snapshot()
        );

        let before_second = own_lines();
        settle_parent_exit(&root, None);
        assert_eq!(own_lines(), before_second);

        let mut declined = Session::default();
        declined.set_uid(Some(uid));
        declined.set_save_session(Some(false));
        settle_parent_exit(&root, Some(&declined));
        assert!(!root.dir().exists());
        assert!(
            logged("Removed child session directory") && logged("session is not saved"),
            "{:?}",
            info_snapshot()
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn remove_dir_refuses_a_symlink_and_keeps_its_target() {
        install_log_collector();
        let cache = CacheDirGuard::new("child-sessions-remove-link");
        let target = cache.root.join("target");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("root.yaml"), "version: 1\n").unwrap();
        let link = cache.root.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        remove_dir(&link, "test");

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(target.join("root.yaml").is_file());
        let link_text = link.display().to_string();
        assert!(
            warn_snapshot().iter().any(
                |message| message.contains("Refusing to touch") && message.contains(&link_text)
            ),
            "{:?}",
            warn_snapshot()
        );
    }

    /// A uid root whose directory is a symlink to a fresh `target` holding
    /// an identity file naming `parent`, plus one child newer than the last
    /// save. Every retention step given this root would delete or rewrite
    /// something through the link if it followed it.
    #[cfg(unix)]
    fn symlinked_uid_root(
        cache: &CacheDirGuard,
        root: &ChildSessionRoot,
        parent: &Path,
    ) -> PathBuf {
        let target = cache.root.join("target");
        let children = target.join("children");
        fs::create_dir_all(&children).unwrap();
        let mut record = fixture_root();
        record.parent_paths = vec![fs::canonicalize(parent).unwrap()];
        record.save(&target.join("root.yaml")).unwrap();
        fixture_meta_at("sess_new_0123456789ab", 10)
            .save(&children.join("sess_new_0123456789ab.meta.yaml"))
            .unwrap();
        fs::write(
            children.join("sess_new_0123456789ab.yaml"),
            "messages: []\n",
        )
        .unwrap();
        fs::create_dir_all(root.dir().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, root.dir()).unwrap();
        target
    }

    #[cfg(unix)]
    fn assert_symlinked_uid_root_untouched(
        root: &ChildSessionRoot,
        target: &Path,
        root_bytes: &[u8],
    ) {
        assert!(
            fs::symlink_metadata(root.dir())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(target.join("root.yaml")).unwrap(), root_bytes);
        let children = target.join("children");
        assert!(children.join("sess_new_0123456789ab.meta.yaml").is_file());
        assert!(children.join("sess_new_0123456789ab.yaml").is_file());
    }

    #[test]
    #[serial]
    fn settle_parent_exit_removes_a_process_root() {
        install_log_collector();
        let _cache = CacheDirGuard::new("child-sessions-settle-proc");
        let root = ChildSessionRoot::from_stamp(fixture_stamp());
        root.ensure_dir(&RootFile::seed_at(fixture_time(), fixture_stamp(), None))
            .unwrap();
        write_child(&root, "sess_a_0123456789ab", 0);
        // The cache root is read from the environment on every `dir()`
        // call, so the pin keys on the process-root leaf the stamp fixes,
        // not on the full path a concurrent test could re-point.
        let leaf = format!(
            "{}-{} (process root settled)",
            fixture_stamp().pid,
            fixture_stamp().start
        );

        settle_parent_exit(&root, None);

        assert!(!root.dir().exists());
        assert!(
            info_snapshot().iter().any(|message| {
                message.starts_with("Removed child session directory") && message.ends_with(&leaf)
            }),
            "{:?}",
            info_snapshot()
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn settle_parent_exit_leaves_the_target_of_a_symlinked_process_root() {
        let cache = CacheDirGuard::new("child-sessions-settle-link");
        let root = ChildSessionRoot::from_stamp(fixture_stamp());
        let target = cache.root.join("target");
        fs::create_dir_all(target.join("children")).unwrap();
        fs::write(target.join("root.yaml"), "version: 1\n").unwrap();
        fs::create_dir_all(root.dir().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, root.dir()).unwrap();

        settle_parent_exit(&root, None);

        assert!(
            fs::symlink_metadata(root.dir())
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(target.join("root.yaml").is_file());
        assert!(target.join("children").is_dir());
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn settle_parent_exit_leaves_the_target_of_a_symlinked_uid_root() {
        let cache = CacheDirGuard::new("child-sessions-settle-uid-link");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let parent = parent_file(&cache, "kept.yaml");
        let target = symlinked_uid_root(&cache, &root, &parent);
        let root_bytes = fs::read(target.join("root.yaml")).unwrap();

        settle_parent_exit(&root, None);

        assert_symlinked_uid_root_untouched(&root, &target, &root_bytes);
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn note_parent_saved_at_does_not_write_through_a_symlinked_uid_root() {
        let cache = CacheDirGuard::new("child-sessions-note-uid-link");
        let uid = Uuid::new_v4();
        let root = ChildSessionRoot::Uid(uid);
        let parent = parent_file(&cache, "kept.yaml");
        let target = symlinked_uid_root(&cache, &root, &parent);
        let root_bytes = fs::read(target.join("root.yaml")).unwrap();
        let later = fixture_time() + Duration::seconds(60);

        note_parent_saved_at(later, fixture_stamp(), uid, &parent).unwrap();

        assert_symlinked_uid_root_untouched(&root, &target, &root_bytes);
    }

    #[test]
    #[serial]
    fn settle_parent_exit_refuses_a_sidecar_whose_id_escapes_the_root() {
        let cache = CacheDirGuard::new("child-sessions-settle-escape");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let mut record = fixture_root();
        record.parent_paths = vec![fs::canonicalize(parent_file(&cache, "kept.yaml")).unwrap()];
        record.last_saved_at = None;
        root.ensure_dir(&record).unwrap();
        fs::create_dir(root.children_dir().join("sess_up")).unwrap();
        let escapes = [
            ("sess_bare_0123456789ab", "../../escape"),
            (
                "sess_agent_0123456789ab",
                "sess_up/../../escape_0123456789ab",
            ),
        ];
        let mut planted = Vec::new();
        for (file_id, body_id) in escapes {
            let mut meta = fixture_meta_at(file_id, 10);
            meta.session_id = body_id.into();
            let sidecar = root.child_meta_file(file_id);
            meta.save(&sidecar).unwrap();
            let planted_session = root.child_session_file(body_id);
            let planted_meta = root.child_meta_file(body_id);
            assert!(planted_session.starts_with(sessions_root()));
            fs::write(&planted_session, "messages: []\n").unwrap();
            fs::write(&planted_meta, "version: 1\n").unwrap();
            planted.extend([sidecar, planted_session, planted_meta]);
        }

        settle_parent_exit(&root, None);

        for path in &planted {
            assert!(path.is_file(), "{}", path.display());
        }
    }

    #[test]
    #[serial]
    fn settle_parent_exit_removes_a_uid_root_when_no_parent_file_remains() {
        let cache = CacheDirGuard::new("child-sessions-settle-orphan");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let mut record = fixture_root();
        record.parent_paths = vec![cache.root.join("gone.yaml")];
        root.ensure_dir(&record).unwrap();
        write_child(&root, "sess_a_0123456789ab", -10);

        settle_parent_exit(&root, None);

        assert!(!root.dir().exists());
    }

    #[test]
    #[serial]
    fn settle_parent_exit_prunes_only_children_created_after_the_last_save() {
        let cache = CacheDirGuard::new("child-sessions-settle-prune");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let mut record = fixture_root();
        record.parent_paths = vec![
            cache.root.join("gone.yaml"),
            fs::canonicalize(parent_file(&cache, "kept.yaml")).unwrap(),
        ];
        root.ensure_dir(&record).unwrap();
        write_child(&root, "sess_old_0123456789ab", -10);
        write_child(&root, "sess_same_0123456789ab", 0);
        write_child(&root, "sess_new_0123456789ab", 10);
        write_child(&root, "sess_newer_0123456789ab", 20);
        let root_bytes = fs::read(root.root_file()).unwrap();

        settle_parent_exit(&root, None);

        assert!(child_present(&root, "sess_old_0123456789ab"));
        assert!(child_present(&root, "sess_same_0123456789ab"));
        assert!(!child_present(&root, "sess_new_0123456789ab"));
        assert!(!child_present(&root, "sess_newer_0123456789ab"));
        assert_eq!(fs::read(root.root_file()).unwrap(), root_bytes);
    }

    #[test]
    #[serial]
    fn settle_parent_exit_prunes_every_child_of_a_never_saved_parent() {
        let cache = CacheDirGuard::new("child-sessions-settle-unsaved");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let mut record = fixture_root();
        record.parent_paths = vec![fs::canonicalize(parent_file(&cache, "kept.yaml")).unwrap()];
        record.last_saved_at = None;
        root.ensure_dir(&record).unwrap();
        write_child(&root, "sess_old_0123456789ab", -10);
        write_child(&root, "sess_new_0123456789ab", 10);

        settle_parent_exit(&root, None);

        assert!(root.dir().is_dir());
        assert!(root.root_file().is_file());
        assert!(!child_present(&root, "sess_old_0123456789ab"));
        assert!(!child_present(&root, "sess_new_0123456789ab"));
    }

    #[test]
    #[serial]
    fn settle_parent_exit_keeps_a_root_whose_identity_file_is_corrupt() {
        let _cache = CacheDirGuard::new("child-sessions-settle-corrupt");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        root.ensure_dir(&fixture_root()).unwrap();
        write_child(&root, "sess_new_0123456789ab", 10);
        fs::write(root.root_file(), "not: [valid").unwrap();

        settle_parent_exit(&root, None);

        assert!(root.dir().is_dir());
        assert!(child_present(&root, "sess_new_0123456789ab"));
    }

    #[test]
    #[serial]
    fn settle_parent_exit_on_a_declined_uid_root_removes_it_whatever_the_disk_says() {
        let cache = CacheDirGuard::new("child-sessions-settle-declined");
        let uid = Uuid::new_v4();
        let root = ChildSessionRoot::Uid(uid);
        let mut record = fixture_root();
        record.parent_paths = vec![fs::canonicalize(parent_file(&cache, "kept.yaml")).unwrap()];
        root.ensure_dir(&record).unwrap();
        write_child(&root, "sess_old_0123456789ab", -10);
        let mut session = Session::default();
        session.set_uid(Some(uid));
        session.set_save_session(Some(false));

        settle_parent_exit(&root, Some(&session));

        assert!(!root.dir().exists());
    }

    #[test]
    #[serial]
    fn settle_parent_exit_ignores_a_session_that_does_not_own_the_root() {
        let cache = CacheDirGuard::new("child-sessions-settle-other-owner");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let mut record = fixture_root();
        record.parent_paths = vec![fs::canonicalize(parent_file(&cache, "kept.yaml")).unwrap()];
        root.ensure_dir(&record).unwrap();
        write_child(&root, "sess_old_0123456789ab", -10);
        let mut session = Session::default();
        session.set_uid(Some(Uuid::new_v4()));
        session.set_save_session(Some(false));

        settle_parent_exit(&root, Some(&session));

        assert!(root.dir().is_dir());
        assert!(child_present(&root, "sess_old_0123456789ab"));
    }

    #[test]
    #[serial]
    fn settle_parent_exit_with_a_saving_session_reads_the_disk() {
        let cache = CacheDirGuard::new("child-sessions-settle-saving");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let mut record = fixture_root();
        record.parent_paths = vec![fs::canonicalize(parent_file(&cache, "kept.yaml")).unwrap()];
        root.ensure_dir(&record).unwrap();
        write_child(&root, "sess_old_0123456789ab", -10);
        write_child(&root, "sess_new_0123456789ab", 10);
        let mut session = Session::default();
        session.set_save_session(Some(true));

        settle_parent_exit(&root, Some(&session));

        assert!(child_present(&root, "sess_old_0123456789ab"));
        assert!(!child_present(&root, "sess_new_0123456789ab"));
    }

    fn uid_yaml(cache: &CacheDirGuard, name: &str, uid: Uuid) -> PathBuf {
        let path = cache.root.join(name);
        fs::write(&path, format!("model: ''\nmessages: []\nuid: {uid}\n")).unwrap();
        path
    }

    #[test]
    #[serial]
    fn unlink_parent_yaml_removes_the_root_with_its_last_parent() {
        let cache = CacheDirGuard::new("child-sessions-unlink-last");
        let uid = Uuid::new_v4();
        let root = ChildSessionRoot::Uid(uid);
        let path = uid_yaml(&cache, "only.yaml", uid);
        let mut record = fixture_root();
        record.parent_paths = vec![fs::canonicalize(&path).unwrap()];
        root.ensure_dir(&record).unwrap();

        unlink_parent_yaml(&path).unwrap();

        assert!(!path.exists());
        assert!(!root.dir().exists());
    }

    #[test]
    #[serial]
    fn unlink_parent_yaml_forgets_one_of_several_parents() {
        let cache = CacheDirGuard::new("child-sessions-unlink-one");
        let uid = Uuid::new_v4();
        let root = ChildSessionRoot::Uid(uid);
        let doomed = uid_yaml(&cache, "doomed.yaml", uid);
        let kept = fs::canonicalize(uid_yaml(&cache, "kept.yaml", uid)).unwrap();
        let mut record = fixture_root();
        record.parent_paths = vec![fs::canonicalize(&doomed).unwrap(), kept.clone()];
        root.ensure_dir(&record).unwrap();
        write_child(&root, "sess_a_0123456789ab", 0);

        unlink_parent_yaml(&doomed).unwrap();

        assert!(!doomed.exists());
        assert!(root.dir().is_dir());
        assert!(child_present(&root, "sess_a_0123456789ab"));
        let after = RootFile::load(&root.root_file()).unwrap();
        assert_eq!(after.parent_paths, vec![kept]);
        assert_eq!(after.last_saved_at, record.last_saved_at);
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn unlink_parent_yaml_does_not_follow_a_symlinked_uid_root() {
        let cache = CacheDirGuard::new("child-sessions-unlink-uid-link");
        let uid = Uuid::new_v4();
        let root = ChildSessionRoot::Uid(uid);
        let path = uid_yaml(&cache, "only.yaml", uid);
        let target = symlinked_uid_root(&cache, &root, &path);
        let root_bytes = fs::read(target.join("root.yaml")).unwrap();

        unlink_parent_yaml(&path).unwrap();

        assert!(!path.exists());
        assert_symlinked_uid_root_untouched(&root, &target, &root_bytes);
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn unlink_parent_yaml_removes_a_symlinked_session_file_without_bookkeeping() {
        let cache = CacheDirGuard::new("child-sessions-unlink-file-link");
        let uid = Uuid::new_v4();
        let root = ChildSessionRoot::Uid(uid);
        let real = uid_yaml(&cache, "real.yaml", uid);
        let link = cache.root.join("link.yaml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut record = fixture_root();
        record.parent_paths = vec![fs::canonicalize(&real).unwrap()];
        root.ensure_dir(&record).unwrap();
        let root_bytes = fs::read(root.root_file()).unwrap();

        unlink_parent_yaml(&link).unwrap();

        assert!(fs::symlink_metadata(&link).is_err());
        assert!(real.is_file());
        assert!(root.dir().is_dir());
        assert_eq!(fs::read(root.root_file()).unwrap(), root_bytes);
    }

    #[test]
    #[serial]
    fn unlink_parent_yaml_without_a_uid_touches_only_the_file() {
        let cache = CacheDirGuard::new("child-sessions-unlink-nouid");
        let unrelated = ChildSessionRoot::Uid(Uuid::new_v4());
        unrelated.ensure_dir(&fixture_root()).unwrap();
        let plain = cache.root.join("plain.yaml");
        fs::write(&plain, "model: ''\nmessages: []\n").unwrap();
        let garbage = cache.root.join("garbage.yaml");
        fs::write(&garbage, "not: [valid").unwrap();

        unlink_parent_yaml(&plain).unwrap();
        unlink_parent_yaml(&garbage).unwrap();

        assert!(!plain.exists());
        assert!(!garbage.exists());
        assert!(unrelated.dir().is_dir());
        assert!(unrelated.root_file().is_file());
    }

    #[test]
    #[serial]
    fn unlink_parent_yaml_on_a_missing_file_fails_naming_it() {
        let cache = CacheDirGuard::new("child-sessions-unlink-missing");
        let missing = cache.root.join("missing.yaml");

        let error = unlink_parent_yaml(&missing).unwrap_err();

        assert!(format!("{error:#}").contains(&missing.display().to_string()));
    }

    fn dead_stamp() -> ProcessStamp {
        ProcessStamp {
            pid: u32::MAX - 1,
            start: 0,
        }
    }

    fn sweep_record(parent_paths: Vec<PathBuf>, stamp: ProcessStamp) -> RootFile {
        let mut record = fixture_root();
        record.parent_paths = parent_paths;
        record.last_pid = stamp.pid;
        record.last_pid_start = stamp.start;
        record
    }

    fn write_root(dir: &Path, record: &RootFile) {
        fs::write(
            dir.join("root.yaml"),
            serde_yaml::to_string(record).unwrap(),
        )
        .unwrap();
    }

    /// A fresh uid directory under the sessions root, with the uid's text
    /// for log assertions.
    fn bare_uid_dir() -> (PathBuf, String) {
        let uid = Uuid::new_v4();
        let dir = ChildSessionRoot::Uid(uid).dir();
        fs::create_dir_all(&dir).unwrap();
        (dir, uid.simple().to_string())
    }

    fn uid_dir_with(record: &RootFile) -> (PathBuf, String) {
        let (dir, uid_text) = bare_uid_dir();
        write_root(&dir, record);
        (dir, uid_text)
    }

    /// Backdates `dir`'s modification time two hours, past the sweep's
    /// grace window. The sweep itself reads that stamp through
    /// `metadata()`, so only this writer needs an open handle.
    fn age_past_grace(dir: &Path) {
        let two_hours_ago = SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        open_dir_for_timestamps(dir)
            .set_modified(two_hours_ago)
            .unwrap();
    }

    /// A directory handle that `set_modified` accepts on unix.
    #[cfg(not(windows))]
    fn open_dir_for_timestamps(dir: &Path) -> fs::File {
        fs::File::open(dir).unwrap()
    }

    /// Windows refuses a plain `File::open` on a directory (access denied)
    /// unless the handle carries FILE_FLAG_BACKUP_SEMANTICS, and SetFileTime
    /// needs FILE_WRITE_ATTRIBUTES rather than read access. This mirrors how
    /// the standard library opens a path for `set_times` on Windows.
    #[cfg(windows)]
    fn open_dir_for_timestamps(dir: &Path) -> fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
            .unwrap()
    }

    fn removal_logged(name: &str, arm: &str) -> bool {
        info_snapshot().iter().any(|message| {
            message.contains("Removed child session directory")
                && message.contains(name)
                && message.contains(arm)
        })
    }

    #[test]
    #[serial]
    fn sweep_removes_a_uid_root_once_every_parent_file_is_gone() {
        install_log_collector();
        let guard = TestConfigDirGuard::new("child-sessions-sweep-parents");
        let parents = guard.cache_root().join("parents");
        fs::create_dir_all(&parents).unwrap();
        let kept = parents.join("kept.yaml");
        fs::write(&kept, "id: x\n").unwrap();
        let (all_gone, all_gone_uid) = uid_dir_with(&sweep_record(
            vec![parents.join("missing.yaml")],
            dead_stamp(),
        ));
        let (one_remains, _) = uid_dir_with(&sweep_record(
            vec![kept, parents.join("missing.yaml")],
            dead_stamp(),
        ));
        let (unreachable, _) = uid_dir_with(&sweep_record(
            vec![guard.cache_root().join("absent-dir").join("x.yaml")],
            dead_stamp(),
        ));

        sweep_orphans();

        assert!(!all_gone.exists());
        assert!(one_remains.is_dir());
        assert!(unreachable.is_dir());
        assert!(
            removal_logged(&all_gone_uid, "orphan sweep arm 1"),
            "{:?}",
            info_snapshot()
        );
    }

    #[test]
    #[serial]
    fn sweep_removes_a_parentless_uid_root_with_its_process_unless_recovery_claims_it() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("child-sessions-sweep-parentless");
        let (dead, dead_uid) = uid_dir_with(&sweep_record(Vec::new(), dead_stamp()));
        let (live, _) = uid_dir_with(&sweep_record(Vec::new(), ProcessStamp::current()));
        let (recovering, _) = uid_dir_with(&sweep_record(Vec::new(), dead_stamp()));
        fs::write(recovering.join("recovery.yaml"), "").unwrap();

        sweep_orphans();

        assert!(!dead.exists());
        assert!(live.is_dir());
        assert!(recovering.is_dir());
        assert!(
            removal_logged(&dead_uid, "orphan sweep arm 2"),
            "{:?}",
            info_snapshot()
        );
    }

    #[test]
    #[serial]
    fn sweep_removes_a_uid_root_without_a_usable_identity_only_after_an_hour() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("child-sessions-sweep-identity");
        let (fresh_missing, _) = bare_uid_dir();
        let (old_missing, old_missing_uid) = bare_uid_dir();
        age_past_grace(&old_missing);
        let (fresh_broken, _) = bare_uid_dir();
        fs::write(fresh_broken.join("root.yaml"), "{not yaml").unwrap();
        let (old_broken, old_broken_uid) = bare_uid_dir();
        fs::write(old_broken.join("root.yaml"), "parent_paths: 7\n").unwrap();
        age_past_grace(&old_broken);

        sweep_orphans();

        assert!(fresh_missing.is_dir());
        assert!(fresh_broken.is_dir());
        assert!(!old_missing.exists());
        assert!(!old_broken.exists());
        assert!(
            removal_logged(&old_missing_uid, "orphan sweep arm 0")
                && removal_logged(&old_broken_uid, "orphan sweep arm 0"),
            "{:?}",
            info_snapshot()
        );
    }

    #[test]
    #[serial]
    fn sweep_removes_process_roots_whose_process_is_gone() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("child-sessions-sweep-proc");
        let dead = dead_stamp();
        let dead_dir = ChildSessionRoot::from_stamp(dead).dir();
        let live_dir = ChildSessionRoot::from_stamp(ProcessStamp::current()).dir();
        let garbage = sessions_root().join("_proc").join("garbage");
        for dir in [&dead_dir, &live_dir, &garbage] {
            fs::create_dir_all(dir).unwrap();
        }

        sweep_orphans();

        assert!(!dead_dir.exists());
        assert!(live_dir.is_dir());
        assert!(garbage.is_dir());
        assert!(
            removal_logged(
                &format!("{}-{}", dead.pid, dead.start),
                "orphan sweep arm 3"
            ),
            "{:?}",
            info_snapshot()
        );
    }

    /// A link to a sibling the sweep also visits directly survives as a
    /// link. A link out of the tree to a directory that would otherwise be
    /// swept is the tell: following it would end in `remove_dir` refusing
    /// the symlink with a warning, so no warning may name the link.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn sweep_leaves_a_symlink_in_the_sessions_tree_unfollowed() {
        install_log_collector();
        let guard = TestConfigDirGuard::new("child-sessions-sweep-symlink");
        let (sibling, sibling_uid) = uid_dir_with(&sweep_record(Vec::new(), dead_stamp()));
        fs::write(sibling.join("recovery.yaml"), "").unwrap();
        let link = sessions_root().join("link");
        std::os::unix::fs::symlink(&sibling, &link).unwrap();
        let outside = guard.cache_root().join("outside");
        fs::create_dir_all(&outside).unwrap();
        write_root(&outside, &sweep_record(Vec::new(), dead_stamp()));
        let outside_link = sessions_root().join(Uuid::new_v4().simple().to_string());
        std::os::unix::fs::symlink(&outside, &outside_link).unwrap();

        sweep_orphans();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(sibling.is_dir());
        assert!(
            !info_snapshot()
                .iter()
                .any(|message| message.contains(&sibling_uid)),
            "{:?}",
            info_snapshot()
        );
        assert!(
            fs::symlink_metadata(&outside_link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(outside.join("root.yaml").is_file());
        let link_text = outside_link.display().to_string();
        assert!(
            !warn_snapshot()
                .iter()
                .any(|message| message.contains(&link_text)),
            "{:?}",
            warn_snapshot()
        );
    }

    #[test]
    #[serial]
    fn sweep_keeps_a_uid_root_adopted_by_a_live_process_before_the_delete() {
        let _guard = TestConfigDirGuard::new("child-sessions-sweep-adopted");
        let (dir, _) = uid_dir_with(&sweep_record(Vec::new(), dead_stamp()));
        let adopt = |about_to_go: &Path| {
            write_root(
                about_to_go,
                &sweep_record(Vec::new(), ProcessStamp::current()),
            );
        };

        sweep_orphans_with_before_remove(&adopt);
        assert!(dir.is_dir());

        write_root(&dir, &sweep_record(Vec::new(), dead_stamp()));
        sweep_orphans();
        assert!(!dir.exists());
    }

    /// Root bypasses file modes, so a test that needs a read to be refused
    /// has nothing to assert there.
    #[cfg(unix)]
    fn running_as_root() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A `root.yaml` that exists but may not be read is neither missing
    /// nor unparseable: the sweep learns nothing from it and keeps the
    /// directory, both when deciding and when re-reading before a delete.
    /// The decision is reported once per process, not at every start.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn sweep_keeps_a_uid_root_whose_identity_cannot_be_read() {
        if running_as_root() {
            eprintln!("skipping: file modes do not bind root");
            return;
        }
        install_log_collector();
        let _guard = TestConfigDirGuard::new("child-sessions-sweep-unreadable-identity");
        let (undecidable, undecidable_uid) = uid_dir_with(&sweep_record(Vec::new(), dead_stamp()));
        set_mode(&undecidable.join("root.yaml"), 0o000);
        age_past_grace(&undecidable);
        let (unreadable_on_reread, _) = uid_dir_with(&sweep_record(Vec::new(), dead_stamp()));
        let revoke_read = |about_to_go: &Path| set_mode(&about_to_go.join("root.yaml"), 0o000);
        let unreadable_warns = || {
            warn_snapshot()
                .iter()
                .filter(|message| {
                    message.contains(&undecidable_uid)
                        && message.contains("cannot read its root.yaml")
                })
                .count()
        };

        sweep_orphans_with_before_remove(&revoke_read);
        sweep_orphans();

        assert!(undecidable.is_dir());
        assert!(unreadable_on_reread.is_dir());
        assert_eq!(unreadable_warns(), 1, "{:?}", warn_snapshot());
        assert!(
            !info_snapshot()
                .iter()
                .any(|message| message.contains(&undecidable_uid)),
            "{:?}",
            info_snapshot()
        );
        set_mode(&undecidable.join("root.yaml"), 0o600);
        set_mode(&unreadable_on_reread.join("root.yaml"), 0o600);
    }

    /// A directory where `root.yaml` goes is not an identity anyone wrote:
    /// it gets the same hour as a missing file and then goes.
    #[test]
    #[serial]
    fn sweep_removes_a_uid_root_whose_identity_is_a_directory_after_an_hour() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("child-sessions-sweep-dir-identity");
        let (fresh, _) = bare_uid_dir();
        fs::create_dir(fresh.join("root.yaml")).unwrap();
        let (old, old_uid) = bare_uid_dir();
        fs::create_dir(old.join("root.yaml")).unwrap();
        age_past_grace(&old);

        sweep_orphans();

        assert!(fresh.is_dir());
        assert!(!old.exists());
        assert!(
            removal_logged(&old_uid, "orphan sweep arm 0"),
            "{:?}",
            info_snapshot()
        );
        assert!(
            !warn_snapshot()
                .iter()
                .any(|message| message.contains(&old_uid)),
            "{:?}",
            warn_snapshot()
        );
    }

    /// Only directories named by a 32-hex uuid are session roots; anything
    /// else under `sessions/`, a hyphenated uuid included, is not the
    /// sweep's to age out.
    #[test]
    #[serial]
    fn sweep_leaves_a_directory_that_is_not_uuid_named_alone() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("child-sessions-sweep-stranger");
        let stranger = sessions_root().join("stranger");
        fs::create_dir_all(&stranger).unwrap();
        age_past_grace(&stranger);
        let hyphenated = sessions_root().join(Uuid::new_v4().hyphenated().to_string());
        fs::create_dir_all(&hyphenated).unwrap();
        age_past_grace(&hyphenated);
        let (old_missing, old_missing_uid) = bare_uid_dir();
        age_past_grace(&old_missing);

        sweep_orphans();

        assert!(stranger.is_dir());
        assert!(hyphenated.is_dir());
        assert!(!old_missing.exists());
        assert!(
            removal_logged(&old_missing_uid, "orphan sweep arm 0"),
            "{:?}",
            info_snapshot()
        );
        let stranger_text = stranger.display().to_string();
        assert!(
            !info_snapshot()
                .iter()
                .chain(warn_snapshot().iter())
                .any(|message| message.contains(&stranger_text)),
            "{:?} {:?}",
            info_snapshot(),
            warn_snapshot()
        );
    }

    #[test]
    #[serial]
    fn sweep_looks_the_process_table_up_once_for_every_candidate() {
        use crate::utils::process::ALIVE_SET_CALLS;
        use std::sync::atomic::Ordering;

        let _guard = TestConfigDirGuard::new("child-sessions-sweep-one-lookup");
        let current = ProcessStamp::current();
        let (first, _) = uid_dir_with(&sweep_record(Vec::new(), current));
        let (second, _) = uid_dir_with(&sweep_record(Vec::new(), current));
        let proc_dir = ChildSessionRoot::from_stamp(current).dir();
        fs::create_dir_all(&proc_dir).unwrap();

        let before = ALIVE_SET_CALLS.load(Ordering::SeqCst);
        sweep_orphans();
        let during = ALIVE_SET_CALLS.load(Ordering::SeqCst) - before;

        assert_eq!(during, 1);
        assert!(first.is_dir());
        assert!(second.is_dir());
        assert!(proc_dir.is_dir());

        fs::remove_dir_all(&first).unwrap();
        fs::remove_dir_all(&second).unwrap();
        fs::remove_dir_all(&proc_dir).unwrap();
        let before = ALIVE_SET_CALLS.load(Ordering::SeqCst);
        sweep_orphans();
        assert_eq!(
            ALIVE_SET_CALLS.load(Ordering::SeqCst) - before,
            0,
            "no candidate, no lookup"
        );
    }

    /// A `root.yaml` holding bytes that are not UTF-8 is no identity anyone
    /// wrote, same as a directory in its place: the uid dir gets the hour
    /// of grace and then goes under arm 0, without a warning naming it,
    /// and a fresh one is kept. The sweep never panics on the bytes.
    #[test]
    #[serial]
    fn usage_probe_sweep_removes_a_uid_root_whose_identity_is_not_utf8_after_an_hour() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("child-sessions-sweep-not-utf8-identity");
        let garbage: &[u8] = b"version: 1\n\xff\xfe\x00parent_paths: [\x80]\n";
        let (fresh, fresh_uid) = bare_uid_dir();
        fs::write(fresh.join("root.yaml"), garbage).unwrap();
        let (old, old_uid) = bare_uid_dir();
        fs::write(old.join("root.yaml"), garbage).unwrap();
        age_past_grace(&old);

        sweep_orphans();

        assert!(fresh.is_dir());
        assert!(fresh.join("root.yaml").is_file());
        assert!(!old.exists());
        assert!(
            removal_logged(&old_uid, "orphan sweep arm 0"),
            "{:?}",
            info_snapshot()
        );
        assert!(
            !info_snapshot()
                .iter()
                .any(|message| message.contains(&fresh_uid)),
            "{:?}",
            info_snapshot()
        );
        assert!(
            !warn_snapshot()
                .iter()
                .any(|message| message.contains(&old_uid) || message.contains(&fresh_uid)),
            "an unreadable identity is not a permission problem: {:?}",
            warn_snapshot()
        );

        age_past_grace(&fresh);
        sweep_orphans();
        assert!(!fresh.exists());
        assert!(
            removal_logged(&fresh_uid, "orphan sweep arm 0"),
            "{:?}",
            info_snapshot()
        );
    }

    /// The classifying pass never asks after a single process; only the
    /// re-read guarding the delete still does, since it checks one record
    /// under one lock.
    #[test]
    fn only_the_adoption_recheck_asks_after_a_single_process() {
        let source = include_str!("child_sessions.rs").replace("\r\n", "\n");
        let production = &source[..source.find("\n#[cfg(test)]\nmod tests").unwrap()];
        let fn_body = |head: &str| {
            let start = production.find(head).unwrap();
            let body = &production[start..];
            &body[..body.find("\n}\n").unwrap()]
        };
        for head in [
            "\nfn sweep_orphans_with(",
            "\nfn orphan_reason(",
            "\nfn sweep_dead_process_roots(",
        ] {
            assert!(!fn_body(head).contains(".is_alive("), "{head}");
        }
        assert!(fn_body("\nfn remove_unless_adopted(").contains(".is_alive("));
        assert_eq!(
            fn_body("\nfn sweep_orphans_with(")
                .matches("alive_set(")
                .count(),
            1
        );
        assert_eq!(production.matches("alive_set(").count(), 1);
    }

    /// A regular file at a root's path is not a root: settling it neither
    /// opens a lock inside it nor reports it.
    #[test]
    #[serial]
    fn settle_parent_exit_on_a_file_at_the_root_path_touches_nothing() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("child-sessions-settle-file");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        fs::create_dir_all(sessions_root()).unwrap();
        fs::write(root.dir(), "in the way\n").unwrap();

        settle_parent_exit(&root, None);

        assert_eq!(fs::read_to_string(root.dir()).unwrap(), "in the way\n");
        let dir_text = root.dir().display().to_string();
        assert!(
            !warn_snapshot()
                .iter()
                .any(|message| message.contains(&dir_text)),
            "{:?}",
            warn_snapshot()
        );
        let source = include_str!("child_sessions.rs").replace("\r\n", "\n");
        let start = source.find("\npub(crate) fn settle_parent_exit(").unwrap();
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").unwrap()];
        assert!(body.find("!dir.is_dir()").unwrap() < body.find("root.lock()").unwrap());
        assert!(!body.contains("!dir.exists()"));
    }

    /// A child whose sidecar cannot be unlinked is reported as partially
    /// removed, never as removed.
    #[test]
    #[serial]
    fn remove_child_reports_a_partial_removal_when_one_file_stays() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("child-sessions-remove-child-partial");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        root.ensure_dir(&fixture_root()).unwrap();
        let session_id = "sess_coder_0123456789ab";
        fs::write(root.child_session_file(session_id), "messages: []\n").unwrap();
        let meta = root.child_meta_file(session_id);
        fs::create_dir(&meta).unwrap();
        fs::write(meta.join("pin"), "x").unwrap();

        remove_child(&root, session_id, "test");

        assert!(!root.child_session_file(session_id).exists());
        assert!(meta.is_dir());
        let partial = |message: &String| {
            message.contains("Partially removed child session")
                && message.contains(session_id)
                && message.contains("(test)")
        };
        assert!(warn_snapshot().iter().any(partial), "{:?}", warn_snapshot());
        assert!(
            !info_snapshot().iter().any(|message| {
                message.contains("Removed child session") && message.contains(session_id)
            }),
            "{:?}",
            info_snapshot()
        );
    }

    #[test]
    #[serial]
    fn sweep_succeeds_when_the_sessions_root_cannot_be_read() {
        let _guard = TestConfigDirGuard::new("child-sessions-sweep-unreadable");
        assert!(!sessions_root().exists());
        sweep_orphans();

        fs::write(sessions_root(), "in the way\n").unwrap();
        sweep_orphans();
        assert!(sessions_root().is_file());
    }

    /// The REPL start runs the sweep exactly once, after the models refresh
    /// is spawned and before the interactive loop takes over.
    #[test]
    fn repl_start_runs_the_sweep_once_before_going_interactive() {
        let main = include_str!("../main.rs");
        assert_eq!(main.matches("child_sessions::sweep_orphans()").count(), 1);
        assert!(main.contains("config::child_sessions::sweep_orphans();"));
        assert!(!main.contains("if let Err(error) = config::child_sessions::sweep_orphans()"));
        let tty_check = main.find("bail!(\"No TTY for REPL\")").unwrap();
        let refresh = main
            .find("maybe_spawn_models_refresh(&ctx.app.config);")
            .unwrap();
        let sweep = main.find("child_sessions::sweep_orphans()").unwrap();
        let start = main.find("start_interactive(ctx).await").unwrap();
        assert!(tty_check < sweep);
        assert!(refresh < sweep && sweep < start);
    }

    /// Twice the retry interval, so a paced writer never starves a peer
    /// out of the lock's wait.
    fn pace() {
        std::thread::sleep(crate::utils::fs::EXCLUSIVE_LOCK_RETRY * 2);
    }

    fn hold_root_lock(root: &ChildSessionRoot) -> ExclusiveLock {
        ExclusiveLock::acquire(&root.lock_file(), "test", std::time::Duration::ZERO).unwrap()
    }

    #[test]
    #[serial]
    fn root_lock_file_sits_beside_root_yaml_and_is_owner_only() {
        let _cache = CacheDirGuard::new("child-sessions-lock-layout");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        root.ensure_dir(&fixture_root()).unwrap();

        assert_eq!(root.lock_file(), root.dir().join(".lock"));
        assert!(root.lock_file().is_file());
        #[cfg(unix)]
        assert_eq!(mode_of(&root.lock_file()), 0o600);
        assert!(root.list_children().unwrap().is_empty());
    }

    #[test]
    #[serial]
    fn concurrent_ensure_dir_and_note_parent_saved_at_writers_lose_no_parent_path() {
        let cache = CacheDirGuard::new("child-sessions-lock-writers");
        let uid = Uuid::new_v4();
        let root = ChildSessionRoot::Uid(uid);
        const WRITERS: usize = 4;
        const SAVES: usize = 5;
        let parents: Vec<Vec<PathBuf>> = (0..WRITERS)
            .map(|writer| {
                (0..SAVES)
                    .map(|save| parent_file(&cache, &format!("w{writer}-s{save}.yaml")))
                    .collect()
            })
            .collect();
        let seeded = RootFile::seed_at(fixture_time(), fixture_stamp(), None);

        let barrier = std::sync::Barrier::new(WRITERS);
        std::thread::scope(|scope| {
            for (writer, paths) in parents.iter().enumerate() {
                let barrier = &barrier;
                let root = &root;
                let seeded = &seeded;
                scope.spawn(move || {
                    let stamp = ProcessStamp {
                        pid: 100 + writer as u32,
                        start: 1_700_000_000,
                    };
                    barrier.wait();
                    for (save, path) in paths.iter().enumerate() {
                        root.ensure_dir(seeded).unwrap();
                        pace();
                        let now = fixture_time() + Duration::seconds(save as i64);
                        note_parent_saved_at(now, stamp, uid, path).unwrap();
                        pace();
                    }
                });
            }
        });

        let record = RootFile::load(&root.root_file()).unwrap();
        let mut recorded = record.parent_paths.clone();
        recorded.sort();
        let mut expected: Vec<PathBuf> = parents
            .iter()
            .flatten()
            .map(|path| fs::canonicalize(path).unwrap())
            .collect();
        expected.sort();
        assert_eq!(recorded, expected);
        assert_eq!(record.created_at, fixture_time());
        let leftovers: Vec<_> = fs::read_dir(root.dir())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    #[serial]
    fn settle_parent_exit_keeps_a_root_whose_lock_another_holder_keeps() {
        install_log_collector();
        let cache = CacheDirGuard::new("child-sessions-lock-settle");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        let mut record = fixture_root();
        record.parent_paths = vec![cache.root.join("gone.yaml")];
        root.ensure_dir(&record).unwrap();
        let held = hold_root_lock(&root);

        with_short_root_lock_wait(|| settle_parent_exit(&root, None));

        assert!(root.dir().is_dir());
        assert!(
            warn_snapshot()
                .iter()
                .any(|message| message.contains("child session root lock")
                    && message.contains("is held")
                    && message.contains(&root.dir().display().to_string())),
            "{:?}",
            warn_snapshot()
        );
        drop(held);
        settle_parent_exit(&root, None);
        assert!(!root.dir().exists());
    }

    /// The locked bookkeeping sites all check for the directory before
    /// taking the lock: the lock does not create it, and an absent root
    /// means no children.
    #[test]
    #[serial]
    fn retention_on_an_absent_uid_root_creates_nothing() {
        let cache = CacheDirGuard::new("child-sessions-lock-absent");
        let uid = Uuid::new_v4();
        let root = ChildSessionRoot::Uid(uid);
        let parent = uid_yaml(&cache, "parent.yaml", uid);

        settle_parent_exit(&root, None);
        unlink_parent_yaml(&parent).unwrap();

        assert!(!parent.exists());
        assert!(!root.dir().exists());
        assert!(!sessions_root().exists());
    }

    /// Every deleter decides under the lock and lets go of it before the
    /// delete, so the lock file goes with its directory. On Windows the
    /// lock is mandatory and an open handle pins the directory, so the
    /// order is what makes the delete possible there at all.
    fn removed_roots_leave_no_lock_file() {
        install_log_collector();
        let cache = CacheDirGuard::new("child-sessions-lock-removed");
        let settled = ChildSessionRoot::Uid(Uuid::new_v4());
        let mut record = fixture_root();
        record.parent_paths = vec![cache.root.join("gone.yaml")];
        settled.ensure_dir(&record).unwrap();
        let settled_lock = settled.lock_file();

        let unlinked_uid = Uuid::new_v4();
        let unlinked = ChildSessionRoot::Uid(unlinked_uid);
        let parent = uid_yaml(&cache, "parent.yaml", unlinked_uid);
        unlinked
            .ensure_dir(&RootFile::seed_at(
                fixture_time(),
                fixture_stamp(),
                Some(&parent),
            ))
            .unwrap();
        let unlinked_lock = unlinked.lock_file();

        let (swept_dir, _) = uid_dir_with(&sweep_record(Vec::new(), dead_stamp()));
        let dead = dead_stamp();
        let proc_dir = ChildSessionRoot::from_stamp(dead).dir();
        fs::create_dir_all(&proc_dir).unwrap();

        settle_parent_exit(&settled, None);
        unlink_parent_yaml(&parent).unwrap();
        sweep_orphans();

        for (dir, lock) in [
            (settled.dir(), settled_lock),
            (unlinked.dir(), unlinked_lock),
            (swept_dir.clone(), swept_dir.join(LOCK_FILE_NAME)),
            (proc_dir.clone(), proc_dir.join(LOCK_FILE_NAME)),
        ] {
            assert!(!dir.exists(), "{}", dir.display());
            assert!(!lock.exists(), "{}", lock.display());
        }
        assert!(
            removal_logged(
                &format!("{}-{}", dead.pid, dead.start),
                "orphan sweep arm 3"
            ),
            "{:?}",
            info_snapshot()
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn removed_roots_leave_no_lock_file_on_unix() {
        removed_roots_leave_no_lock_file();
    }

    #[cfg(windows)]
    #[test]
    #[serial]
    fn removed_roots_leave_no_lock_file_on_windows() {
        removed_roots_leave_no_lock_file();
    }

    #[test]
    #[serial]
    fn sweep_keeps_a_root_whose_lock_another_holder_keeps_at_the_delete() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("child-sessions-lock-sweep");
        let (dir, _) = uid_dir_with(&sweep_record(Vec::new(), dead_stamp()));
        let held = std::cell::RefCell::new(None);
        let hold = |about_to_go: &Path| {
            let lock = ExclusiveLock::acquire(
                &about_to_go.join(LOCK_FILE_NAME),
                "test",
                std::time::Duration::ZERO,
            )
            .unwrap();
            *held.borrow_mut() = Some(lock);
        };

        with_short_root_lock_wait(|| sweep_orphans_with_before_remove(&hold));

        assert!(
            held.borrow().is_some(),
            "the hook runs before the sweep locks"
        );
        assert!(dir.is_dir());
        assert!(
            warn_snapshot()
                .iter()
                .any(|message| message.contains("child session root lock")
                    && message.contains("is held")
                    && message.contains("orphan sweep arm 2")),
            "{:?}",
            warn_snapshot()
        );

        held.borrow_mut().take();
        sweep_orphans();
        assert!(!dir.exists());
    }

    #[test]
    #[serial]
    fn sweep_leaves_nothing_behind_when_the_root_vanishes_before_its_lock() {
        install_log_collector();
        let _guard = TestConfigDirGuard::new("child-sessions-vanished-sweep");
        let (dir, _) = uid_dir_with(&sweep_record(Vec::new(), dead_stamp()));
        let vanish = |about_to_go: &Path| fs::remove_dir_all(about_to_go).unwrap();

        sweep_orphans_with_before_remove(&vanish);

        assert!(!dir.exists(), "the lock must not recreate a removed root");
        let dir_text = dir.display().to_string();
        assert!(
            warn_snapshot()
                .iter()
                .any(|message| message.starts_with("Keeping ")
                    && message.contains(&dir_text)
                    && message.contains("Failed to open lock file")),
            "{:?}",
            warn_snapshot()
        );
    }

    /// Every read-modify-write of `root.yaml` and every delete decision
    /// takes the root lock, and no deleter still holds it when it calls
    /// `remove_dir`.
    #[test]
    fn every_root_writer_and_deleter_takes_the_lock_and_drops_it_before_removing() {
        let source = include_str!("child_sessions.rs").replace("\r\n", "\n");
        let production = &source[..source.find("\n#[cfg(test)]\nmod tests").unwrap()];
        let fn_body = |head: &str, end: &str| {
            let start = production.find(head).unwrap();
            let body = &production[start..];
            &body[..body.find(end).unwrap()]
        };
        for (head, end, remover) in [
            ("\n    pub(crate) fn ensure_dir(", "\n    }\n", false),
            ("\npub(crate) fn note_parent_saved_at(", "\n}\n", false),
            ("\npub(crate) fn settle_parent_exit(", "\n}\n", true),
            ("\npub(crate) fn unlink_parent_yaml(", "\n}\n", true),
            ("\nfn remove_unless_adopted(", "\n}\n", true),
        ] {
            let body = fn_body(head, end);
            let lock_at = match (body.find("root.lock()"), body.find("self.lock()")) {
                (Some(at), None) | (None, Some(at)) => at,
                _ => body.find("root_lock(dir)").unwrap(),
            };
            assert_eq!(
                body.matches(".lock()").count() + body.matches("root_lock(").count(),
                1,
                "{head}"
            );
            if remover {
                let locked = &body[lock_at..];
                assert_eq!(locked.matches("remove_dir(").count(), 1, "{head}");
                assert_eq!(locked.matches("drop(lock);").count(), 1, "{head}");
                assert!(
                    locked.find("drop(lock);").unwrap() < locked.find("remove_dir(").unwrap(),
                    "{head}"
                );
            } else {
                assert!(!body.contains("remove_dir("), "{head}");
            }
        }
        let remove_dir_body = fn_body("\npub(crate) fn remove_dir(", "\n}\n");
        assert!(!remove_dir_body.contains("lock"));
    }
}
