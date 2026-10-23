#![allow(dead_code)] // TASK-237

//! On-disk layout for sessions spawned by one coyote process on behalf of
//! another. Each spawning process owns a root directory under the cache's
//! `sessions/` tree holding a `root.yaml` identity file and a `children/`
//! directory of child session files, each paired with a `.meta.yaml`
//! sidecar describing the child's agent, status and owning process.

use crate::config::paths;
use crate::utils::fs::{create_private_dir_all, write_private_atomic};
use crate::utils::process::ProcessStamp;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use log::warn;
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

    /// Creates the root directory, writes `root.yaml`, then creates
    /// `children/`, in that order: a crash between the steps leaves a
    /// directory that already identifies itself rather than an anonymous one.
    pub(crate) fn ensure_dir(&self, root: &RootFile) -> Result<()> {
        let dir = self.dir();
        create_private_dir_all(&dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
        root.save(&self.root_file())?;
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
    use crate::testing::CacheDirGuard;
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
}
