use crate::config::mesh_config::MeshBrief;
use crate::config::todo::TodoList;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Ancestors walked when looking for `.git`; deep enough for any real checkout, bounded so a
/// pathological path never turns a turn boundary into a long walk.
const REPO_WALK_LIMIT: usize = 64;
/// Bytes read from the `.git` pointer file and from `HEAD`; both are one-line files.
const GIT_READ_LIMIT: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnState {
    Idle { since: SystemTime },
    Working { since: SystemTime },
}

impl TurnState {
    pub(crate) fn idle_now() -> Self {
        Self::Idle {
            since: SystemTime::now(),
        }
    }

    pub(crate) fn working_now() -> Self {
        Self::Working {
            since: SystemTime::now(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoInfo {
    pub(crate) root: PathBuf,
    pub(crate) branch: Option<String>,
}

impl RepoInfo {
    /// Filesystem-only discovery: no subprocess, so a turn boundary never blocks on `git`.
    pub(crate) fn discover(cwd: &Path) -> Option<RepoInfo> {
        let root = cwd
            .ancestors()
            .take(REPO_WALK_LIMIT)
            .find(|dir| dir.join(".git").exists())?;
        let branch = gitdir(root).and_then(|gitdir| head_branch(&gitdir));
        Some(RepoInfo {
            root: root.to_path_buf(),
            branch,
        })
    }
}

/// Resolves the `.git` entry to the directory holding `HEAD`; a `.git` file (worktrees,
/// submodules) points there through its `gitdir:` line.
fn gitdir(root: &Path) -> Option<PathBuf> {
    let entry = root.join(".git");
    if entry.is_dir() {
        return Some(entry);
    }
    let text = read_prefix(&entry, GIT_READ_LIMIT)?;
    let target = text
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:"))?
        .trim();
    if target.is_empty() {
        return None;
    }
    Some(root.join(target))
}

fn head_branch(gitdir: &Path) -> Option<String> {
    let head = read_prefix(&gitdir.join("HEAD"), GIT_READ_LIMIT)?;
    let name = head
        .trim()
        .strip_prefix("ref: ")?
        .strip_prefix("refs/heads/")?;
    (!name.is_empty()).then(|| name.to_string())
}

/// The first `limit` bytes of a regular file; FIFOs and devices are skipped rather than read.
fn read_prefix(path: &Path, limit: u64) -> Option<String> {
    if !path.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(limit)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The brief as it stood when the snapshot was captured, kept so a snapshot is
/// self-describing. The live `MeshSlot::brief()` is authoritative, including `None`:
/// whatever serves peers or shows the brief locally must read the live brief, never this
/// copy. `digest_generated_at` is the same anchor the live brief carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BriefState {
    pub(crate) mode: MeshBrief,
    pub(crate) text: Option<String>,
    pub(crate) digest_generated_at: Option<SystemTime>,
}

/// Plain owned data captured from the session context at a turn boundary. Serving code reads
/// this instead of the turn's live state, which stays write-locked for the whole turn.
#[derive(Debug, Clone)]
pub(crate) struct MeshSnapshot {
    pub(crate) objective: Option<String>,
    pub(crate) state: TurnState,
    pub(crate) repo: Option<RepoInfo>,
    pub(crate) todo: TodoList,
    pub(crate) brief: BriefState,
    /// The share root the `/list` and `/fetch` providers serve from.
    pub(crate) cwd: PathBuf,
    pub(crate) captured_at: SystemTime,
}

impl MeshSnapshot {
    /// Saturating: a clock that went backwards reports zero, never panics.
    pub(crate) fn age(&self, now: SystemTime) -> Duration {
        now.duration_since(self.captured_at).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::test_support::{TempDir, snapshot_fixture};
    use std::fs;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn age_is_measured_from_capture_and_saturates() {
        let snap = snapshot_fixture();
        assert_eq!(
            snap.age(snap.captured_at + Duration::from_secs(5)),
            Duration::from_secs(5)
        );
        assert_eq!(
            snap.age(snap.captured_at - Duration::from_secs(5)),
            Duration::ZERO
        );
    }

    #[test]
    fn repo_discover_reads_an_attached_branch() {
        let tmp = TempDir::new("snap-repo");
        write(&tmp.path.join(".git/HEAD"), "ref: refs/heads/main\n");
        let info = RepoInfo::discover(&tmp.path).unwrap();
        assert_eq!(info.root, tmp.path);
        assert_eq!(info.branch.as_deref(), Some("main"));
    }

    #[test]
    fn repo_discover_keeps_slashes_in_branch_names() {
        let tmp = TempDir::new("snap-repo");
        write(
            &tmp.path.join(".git/HEAD"),
            "ref: refs/heads/feat/nested/name\n",
        );
        let info = RepoInfo::discover(&tmp.path).unwrap();
        assert_eq!(info.branch.as_deref(), Some("feat/nested/name"));
    }

    #[test]
    fn repo_discover_reports_no_branch_when_detached() {
        let tmp = TempDir::new("snap-repo");
        write(
            &tmp.path.join(".git/HEAD"),
            "9abd098c0ffee0ffee0ffee0ffee0ffee0ffee00\n",
        );
        let info = RepoInfo::discover(&tmp.path).unwrap();
        assert_eq!(info.root, tmp.path);
        assert_eq!(info.branch, None);
    }

    #[test]
    fn repo_discover_reports_no_branch_for_non_heads_refs_or_missing_head() {
        let tmp = TempDir::new("snap-repo");
        write(
            &tmp.path.join(".git/HEAD"),
            "ref: refs/remotes/origin/main\n",
        );
        assert_eq!(RepoInfo::discover(&tmp.path).unwrap().branch, None);
        fs::remove_file(tmp.path.join(".git/HEAD")).unwrap();
        assert_eq!(RepoInfo::discover(&tmp.path).unwrap().branch, None);
    }

    #[test]
    fn repo_discover_follows_a_gitdir_file_for_worktrees() {
        let tmp = TempDir::new("snap-repo");
        let worktree = tmp.path.join("wt");
        write(
            &tmp.path.join("common/worktrees/wt/HEAD"),
            "ref: refs/heads/wt-branch\n",
        );
        write(&worktree.join(".git"), "gitdir: ../common/worktrees/wt\n");
        let info = RepoInfo::discover(&worktree).unwrap();
        assert_eq!(info.root, worktree);
        assert_eq!(info.branch.as_deref(), Some("wt-branch"));
    }

    #[test]
    fn repo_discover_returns_the_root_with_no_branch_for_a_malformed_gitdir_file() {
        let tmp = TempDir::new("snap-repo");
        write(&tmp.path.join(".git"), "not a gitdir pointer\n");
        let info = RepoInfo::discover(&tmp.path).unwrap();
        assert_eq!(info.root, tmp.path);
        assert_eq!(info.branch, None);
    }

    #[test]
    fn repo_discover_ignores_a_gitdir_line_past_the_read_limit() {
        let tmp = TempDir::new("snap-repo");
        write(&tmp.path.join("common/HEAD"), "ref: refs/heads/main\n");
        let padding = "x".repeat(GIT_READ_LIMIT as usize);
        write(
            &tmp.path.join(".git"),
            &format!("{padding}\ngitdir: common\n"),
        );
        let info = RepoInfo::discover(&tmp.path).unwrap();
        assert_eq!(info.root, tmp.path);
        assert_eq!(info.branch, None);
    }

    #[test]
    fn repo_discover_ignores_a_head_ref_past_the_read_limit() {
        let tmp = TempDir::new("snap-repo");
        let padding = " ".repeat(GIT_READ_LIMIT as usize);
        write(
            &tmp.path.join(".git/HEAD"),
            &format!("{padding}ref: refs/heads/main\n"),
        );
        assert_eq!(RepoInfo::discover(&tmp.path).unwrap().branch, None);
    }

    #[test]
    fn repo_discover_finds_the_root_from_a_nested_directory() {
        let tmp = TempDir::new("snap-repo");
        write(&tmp.path.join(".git/HEAD"), "ref: refs/heads/main\n");
        let nested = tmp.path.join("src/deep/er");
        fs::create_dir_all(&nested).unwrap();
        let info = RepoInfo::discover(&nested).unwrap();
        assert_eq!(info.root, tmp.path);
        assert_eq!(info.branch.as_deref(), Some("main"));
    }

    #[test]
    fn repo_discover_finds_nothing_inside_a_tree_without_a_git_entry() {
        let tmp = TempDir::new("snap-repo");
        let nested = tmp.path.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        // The temp dir itself may sit inside a checkout; only roots under it would be wrong.
        assert!(RepoInfo::discover(&nested).is_none_or(|info| !info.root.starts_with(&tmp.path)));
    }

    #[test]
    fn repo_discover_stops_after_the_walk_limit() {
        let tmp = TempDir::new("snap-repo");
        write(&tmp.path.join(".git/HEAD"), "ref: refs/heads/main\n");
        let mut deep = tmp.path.clone();
        for _ in 0..REPO_WALK_LIMIT {
            deep.push("d");
        }
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(RepoInfo::discover(&deep), None);
        assert!(RepoInfo::discover(deep.parent().unwrap()).is_some());
    }

    #[test]
    fn snapshot_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MeshSnapshot>();
        assert_send_sync::<TurnState>();
        assert_send_sync::<RepoInfo>();
        assert_send_sync::<BriefState>();
    }

    #[test]
    fn snapshot_holds_only_owned_data() {
        let source = include_str!("snapshot.rs");
        let start = source.find("pub(crate) struct MeshSnapshot {").unwrap();
        let body = &source[start..];
        let body = &body[..body.find('}').unwrap()];
        // Assembled at runtime so this test's own text does not match the probes.
        let needles = [
            ["Mu", "tex"].concat(),
            ["Rw", "Lock"].concat(),
            ["Arc", "<"].concat(),
            ["Ce", "ll"].concat(),
            ["&", "'"].concat(),
        ];
        for needle in &needles {
            assert!(
                !body.contains(needle),
                "MeshSnapshot must not contain {needle}"
            );
        }
    }
}
