#![allow(dead_code)] // TASK-237

//! On-disk layout for sessions spawned by one coyote process on behalf of
//! another. Each spawning process owns a root directory under the cache's
//! `sessions/` tree holding a `root.yaml` identity file and a `children/`
//! directory of child session files, each paired with a `.meta.yaml`
//! sidecar describing the child's agent, status and owning process.

use crate::config::{Session, paths};
use crate::utils::fs::{create_private_dir_all, write_private_atomic};
use crate::utils::process::ProcessStamp;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// How many leading characters of a child's prompt the sidecar keeps, so a
/// listing can show what each child was asked without loading its session.
pub(crate) const PROMPT_HEAD_CHARS: usize = 200;

const META_SUFFIX: &str = ".meta.yaml";

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

    /// Creates the root directory, writes `root.yaml` when it is missing or
    /// unreadable, then creates `children/`, in that order: a crash between
    /// the steps leaves a directory that already identifies itself rather
    /// than an anonymous one. A `root.yaml` that parses is left untouched so
    /// a second spawn under the same root cannot erase the parent paths and
    /// save time recorded by the first; one that does not parse is replaced
    /// from `root` with a warning, since nothing in it could be kept.
    /// Anything other than a regular file in its place is an error that
    /// leaves it as found.
    pub(crate) fn ensure_dir(&self, root: &RootFile) -> Result<()> {
        let dir = self.dir();
        create_private_dir_all(&dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
        let root_file = self.root_file();
        if root_file_needs_writing(&root_file)? {
            root.save(&root_file)?;
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

    /// The newest `max` child session ids plus how many more exist. Meant for
    /// status lines, so a listing failure degrades to an empty answer.
    pub(crate) fn recent_child_ids(&self, max: usize) -> (Vec<String>, usize) {
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
        let ids = children
            .into_iter()
            .take(max)
            .map(|meta| meta.session_id)
            .collect();
        (ids, more)
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

/// Whether `ensure_dir` has to write `path`: it is absent, or a regular
/// file that does not hold a `RootFile`. A readable record is kept. A
/// symlink is never followed and, like a directory, is refused rather than
/// written over.
fn root_file_needs_writing(path: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to inspect {}", path.display()));
        }
    };
    if !metadata.file_type().is_file() {
        bail!("{} exists but is not a regular file", path.display());
    }
    let bytes =
        std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
    match serde_yaml::from_slice::<RootFile>(&bytes) {
        Ok(_) => Ok(false),
        Err(error) => {
            warn!(
                "Replacing child session root file {}: it does not parse ({error})",
                path.display()
            );
            Ok(true)
        }
    }
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
/// child leaves nothing behind. The read-modify-write of `root.yaml` takes
/// no lock, so two processes saving parents of one root at the same time
/// leave the last writer's record.
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
/// root whose identity file cannot be read is kept whole.
pub(crate) fn settle_parent_exit(root: &ChildSessionRoot, session: Option<&Session>) {
    let dir = root.dir();
    let uid = match root {
        ChildSessionRoot::Proc { .. } => {
            remove_dir(&dir, "process root exited");
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
    if !dir.exists() {
        return;
    }
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
/// id grammar before it becomes a path component. The grammar leaves the
/// agent segment free-form, so separators and `..` are refused on top.
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
    for path in [
        root.child_session_file(session_id),
        root.child_meta_file(session_id),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => warn!("Failed to remove {} ({reason}): {error}", path.display()),
        }
    }
    if removed {
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
/// is only unlinked: the saved copy a root knows about is its target.
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
        remove_dir(&dir, "its last parent session file was deleted");
    }
    Ok(())
}

/// True when retention must leave `dir` alone: it is a symlink, whose
/// target could be anything, or it cannot be inspected at all. A missing
/// directory is not refused.
fn refuse_symlinked_dir(dir: &Path, reason: &str) -> bool {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            warn!(
                "Refusing to touch {} ({reason}): it is a symlink",
                dir.display()
            );
            true
        }
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            warn!("Failed to inspect {} ({reason}): {error}", dir.display());
            true
        }
    }
}

/// Removes a root directory, refusing to follow a symlink planted at its
/// path. Retention must never fail an exit, so every error is a warning.
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

/// Sidecar describing one child session.
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

/// Splits a `new_session_id` result back into `(agent, hex)`; `None` for
/// anything not in that shape.
pub(crate) fn parse_session_id(id: &str) -> Option<(&str, &str)> {
    let rest = id.strip_prefix(SESSION_ID_PREFIX)?;
    let (agent, hex) = rest.rsplit_once('_')?;
    let hex_ok = hex.len() == SESSION_ID_HEX_LEN
        && hex
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    (!agent.is_empty() && hex_ok).then_some((agent, hex))
}

pub(crate) fn prompt_head(prompt: &str) -> String {
    prompt.chars().take(PROMPT_HEAD_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{CacheDirGuard, info_snapshot, install_log_collector, warn_snapshot};
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
        ] {
            assert_eq!(parse_session_id(bad), None, "{bad}");
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

    #[test]
    #[serial]
    fn ensure_dir_keeps_an_existing_root_file_byte_for_byte() {
        let _cache = CacheDirGuard::new("child-sessions-ensure-twice");
        let root = ChildSessionRoot::Uid(Uuid::new_v4());
        root.ensure_dir(&fixture_root()).unwrap();
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
        let seed = fixture_root();
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

        // Parseable: kept byte for byte even against a different seed.
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
    fn recent_child_ids_returns_newest_and_counts_the_rest() {
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

        let (ids, more) = root.recent_child_ids(2);
        assert_eq!(ids, vec!["sess_c_0123456789ab", "sess_b_0123456789ab"]);
        assert_eq!(more, 1);

        let (ids, more) = root.recent_child_ids(5);
        assert_eq!(
            ids,
            vec![
                "sess_c_0123456789ab",
                "sess_b_0123456789ab",
                "sess_a_0123456789ab"
            ]
        );
        assert_eq!(more, 0);

        let absent = ChildSessionRoot::Uid(Uuid::new_v4());
        assert_eq!(absent.recent_child_ids(2), (Vec::new(), 0));
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
        let _cache = CacheDirGuard::new("child-sessions-settle-proc");
        let root = ChildSessionRoot::from_stamp(fixture_stamp());
        root.ensure_dir(&RootFile::seed_at(fixture_time(), fixture_stamp(), None))
            .unwrap();
        write_child(&root, "sess_a_0123456789ab", 0);

        settle_parent_exit(&root, None);

        assert!(!root.dir().exists());
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
}
