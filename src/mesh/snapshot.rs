use crate::config::mesh_config::MeshBrief;
use crate::config::todo::TodoList;

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Ancestors walked when looking for `.git`; deep enough for any real checkout, bounded so a
/// pathological path never turns a turn boundary into a long walk.
const REPO_WALK_LIMIT: usize = 64;
/// Bytes read from the `.git` pointer file and from `HEAD`; both are one-line files.
const GIT_READ_LIMIT: u64 = 4096;
/// Plan files considered per scan, and the bytes read from each: frontmatter lives at the
/// top, so anything past the first KiB is not a status marker.
const PLAN_SCAN_LIMIT: usize = 64;
const PLAN_READ_LIMIT: u64 = 1024;

/// What a turn was doing when the snapshot was taken.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanRef {
    pub(crate) path: PathBuf,
    pub(crate) title: String,
}

impl PlanRef {
    /// The first `plans/PLAN-*.md` (by file name) whose frontmatter says `status: active`.
    pub(crate) fn discover(cwd: &Path) -> Option<PlanRef> {
        let entries = fs::read_dir(cwd.join("plans")).ok()?;
        let mut candidates: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| is_plan_file_name(path) && path.is_file())
            .collect();
        candidates.sort();
        candidates
            .into_iter()
            .take(PLAN_SCAN_LIMIT)
            .find_map(|path| {
                let head = read_prefix(&path, PLAN_READ_LIMIT)?;
                is_active(&head).then(|| PlanRef {
                    title: plan_title(&path, &head),
                    path,
                })
            })
    }
}

fn is_plan_file_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("PLAN-") && name.ends_with(".md"))
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

/// Splits the text into its frontmatter lines (between the opening `---` and the closing
/// `---`, trimmed) and the body lines after it. Text that does not start with `---` is all
/// body.
fn split_frontmatter(head: &str) -> (Vec<&str>, Vec<&str>) {
    let mut lines = head.lines();
    let Some(first) = lines.next() else {
        return (Vec::new(), Vec::new());
    };
    if first.trim_end() != "---" {
        return (Vec::new(), head.lines().collect());
    }
    let mut frontmatter = Vec::new();
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            break;
        }
        frontmatter.push(line.trim());
    }
    (frontmatter, lines.collect())
}

fn is_active(head: &str) -> bool {
    split_frontmatter(head).0.iter().any(|line| {
        line.strip_prefix("status:")
            .is_some_and(|value| value.trim() == "active")
    })
}

fn plan_title(path: &Path, head: &str) -> String {
    let (frontmatter, body) = split_frontmatter(head);
    let from_frontmatter = frontmatter.iter().find_map(|line| {
        let value = line.strip_prefix("title:")?.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        (!value.is_empty()).then(|| value.to_string())
    });
    from_frontmatter
        .or_else(|| {
            body.iter()
                .find_map(|line| line.strip_prefix("# "))
                .map(|heading| heading.trim().to_string())
                .filter(|heading| !heading.is_empty())
        })
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default()
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BriefState {
    pub(crate) mode: MeshBrief,
    pub(crate) text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionInfo {
    pub(crate) name: Option<String>,
    pub(crate) model: String,
    pub(crate) role: Option<String>,
}

/// Plain owned data captured from the session context at a turn boundary. Serving code reads
/// this instead of the turn's live state, which stays write-locked for the whole turn.
#[derive(Debug, Clone)]
pub(crate) struct MeshSnapshot {
    pub(crate) objective: Option<String>,
    pub(crate) state: TurnState,
    pub(crate) repo: Option<RepoInfo>,
    pub(crate) plan: Option<PlanRef>,
    pub(crate) todo: TodoList,
    // `brief`, `cwd` and `session` wait for the brief and message providers.
    #[allow(dead_code)]
    pub(crate) brief: BriefState,
    #[allow(dead_code)]
    pub(crate) cwd: PathBuf,
    pub(crate) captured_at: SystemTime,
    #[allow(dead_code)]
    pub(crate) session: SessionInfo,
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
    fn plan_discover_returns_none_without_a_plans_dir() {
        let tmp = TempDir::new("snap-plan");
        assert_eq!(PlanRef::discover(&tmp.path), None);
    }

    #[test]
    fn plan_discover_skips_non_active_plans_that_sort_first() {
        let tmp = TempDir::new("snap-plan");
        write(
            &tmp.path.join("plans/PLAN-aaa.md"),
            "---\nstatus: done\ntitle: Old\n---\n",
        );
        write(
            &tmp.path.join("plans/PLAN-bbb.md"),
            "---\ntitle: \"Mesh\"\nstatus: active\n---\n# Heading\n",
        );
        write(
            &tmp.path.join("plans/notes.md"),
            "---\nstatus: active\n---\n",
        );
        let plan = PlanRef::discover(&tmp.path).unwrap();
        assert_eq!(plan.path, tmp.path.join("plans/PLAN-bbb.md"));
        assert_eq!(plan.title, "Mesh");
    }

    #[test]
    fn plan_discover_requires_frontmatter_before_the_status_line() {
        let tmp = TempDir::new("snap-plan");
        write(
            &tmp.path.join("plans/PLAN-a.md"),
            "# No frontmatter\nstatus: active\n",
        );
        write(
            &tmp.path.join("plans/PLAN-b.md"),
            "---\ntitle: x\n---\nstatus: active\n",
        );
        assert_eq!(PlanRef::discover(&tmp.path), None);
    }

    #[test]
    fn plan_title_falls_back_to_heading_then_stem() {
        let tmp = TempDir::new("snap-plan");
        write(
            &tmp.path.join("plans/PLAN-heading.md"),
            "---\nstatus: active\n---\n\n# From Heading\n",
        );
        assert_eq!(PlanRef::discover(&tmp.path).unwrap().title, "From Heading");
        fs::remove_file(tmp.path.join("plans/PLAN-heading.md")).unwrap();
        write(
            &tmp.path.join("plans/PLAN-stem.md"),
            "---\nstatus: active\n---\nbody only\n",
        );
        assert_eq!(PlanRef::discover(&tmp.path).unwrap().title, "PLAN-stem");
    }

    #[test]
    fn plan_title_heading_fallback_skips_the_frontmatter() {
        let tmp = TempDir::new("snap-plan");
        write(
            &tmp.path.join("plans/PLAN-yaml-comment.md"),
            "---\nstatus: active\n# not the title\n---\n\n# Real Title\n",
        );
        assert_eq!(PlanRef::discover(&tmp.path).unwrap().title, "Real Title");
    }

    #[test]
    fn plan_discover_ignores_files_beyond_the_scan_limit() {
        let tmp = TempDir::new("snap-plan");
        for i in 0..PLAN_SCAN_LIMIT {
            write(
                &tmp.path.join(format!("plans/PLAN-{i:03}.md")),
                "---\nstatus: draft\n---\n",
            );
        }
        write(
            &tmp.path.join(format!("plans/PLAN-{PLAN_SCAN_LIMIT:03}.md")),
            "---\nstatus: active\n---\n",
        );
        assert_eq!(PlanRef::discover(&tmp.path), None);
    }

    #[test]
    fn plan_discover_ignores_a_status_past_the_read_limit() {
        let tmp = TempDir::new("snap-plan");
        let padding = "x".repeat(PLAN_READ_LIMIT as usize);
        write(
            &tmp.path.join("plans/PLAN-late.md"),
            &format!("---\nnote: {padding}\nstatus: active\n---\n"),
        );
        assert_eq!(PlanRef::discover(&tmp.path), None);
    }

    #[test]
    fn snapshot_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MeshSnapshot>();
        assert_send_sync::<TurnState>();
        assert_send_sync::<RepoInfo>();
        assert_send_sync::<PlanRef>();
        assert_send_sync::<BriefState>();
        assert_send_sync::<SessionInfo>();
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
