//! Review lanes read the code under review and report only what the diff
//! holds. These tests pin the deterministic guards behind that:
//!
//! - the read-only tools (`assets/functions/tools/{fs_*,ast_grep}.sh` through
//!   `assets/functions/utils/review-paths.sh`) resolve a relative path against
//!   the review checkout and refuse a read of tracked code inside another
//!   clone of the same origin repository, while other repositories, untracked
//!   local files and every non-review agent are unaffected; with the default
//!   `project_dir` of `.` the guard is inert;
//! - the review agents declare the `review_grounding` variable that turns the
//!   guard on;
//! - code-reviewer never reports an unverified finding as blocking, and the
//!   review gauntlet reads a verification that failed as a whole as an
//!   incomplete review;
//! - code-reviewer's anchor gate drops findings that do not point at a change
//!   of this diff, and facts.py keeps generated files out of review;
//! - the synthesis wording keeps prior-art notes as context and anchors
//!   consumer findings at the changed line.
//!
//! The fixtures mirror the failures they guard against: a review checkout, a
//! stale clone of the same repository in the working directory under the
//! repository's name, a worktree of the main clone, and an unrelated shared
//! library.

use serde_json::{Value, json};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::Duration;

const TOOLS: [&str; 6] = [
    "fs_read.sh",
    "fs_cat.sh",
    "fs_grep.sh",
    "fs_glob.sh",
    "fs_ls.sh",
    "ast_grep.sh",
];

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn tool_path(tool: &str) -> PathBuf {
    manifest().join("assets/functions/tools").join(tool)
}

/// The first bash on PATH outside System32. The one Windows puts there is
/// the WSL launcher, which is not a shell: it prints a banner about missing
/// distributions and exits 1.
fn bash_program() -> Option<PathBuf> {
    which::which_all("bash")
        .ok()?
        .find(|path| is_outside_system32(path))
}

/// Twin of `is_outside_system32` in src/testing.rs; change both together.
/// Splits on both separators because `Path::components` leaves a Windows
/// path whole on unix, where this is unit-tested.
fn is_outside_system32(path: &Path) -> bool {
    !path
        .to_string_lossy()
        .split(['/', '\\'])
        .any(|part| part.eq_ignore_ascii_case("system32"))
}

fn tools_available() -> bool {
    let ok = bash_program().is_some() && which::which("git").is_ok();
    if !ok {
        eprintln!("skipping: bash/git not available");
    }
    ok
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Unique per call: tests run in parallel threads of one process, so the pid
/// alone (or a clock) would hand two of them the same directory.
fn temp_dir(label: &str) -> TempDir {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::SeqCst);
    let dir = env::temp_dir().join(format!(
        "coyote-grounding-{label}-{}-{n}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=t",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `root/review` (the checkout under review, origin in scp form), `root/billing`
/// (a stale clone of the same repository, origin in https form) and
/// `root/shared-lib` (another repository). `root` is named by the caller, so
/// a root holding a space or a quote is exercised too.
struct Clones {
    _dir: TempDir,
    root: PathBuf,
    review: PathBuf,
    shadow: PathBuf,
    other: PathBuf,
}

fn clones_in(root_name: &str) -> Clones {
    let dir = temp_dir("clones");
    let root = dir.0.join(root_name);
    let seed = root.join("seed");
    fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q"]);
    write(&seed.join("internal/plan.go"), "package plan // HEAD\n");
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-qm", "seed"]);
    let review = root.join("review");
    let shadow = root.join("billing");
    for clone in [&review, &shadow] {
        git(&root, &["clone", "-q", &s(&seed), &s(clone)]);
    }
    git(
        &review,
        &["remote", "set-url", "origin", "git@github.com:Org/Billing.git"],
    );
    git(
        &shadow,
        &["remote", "set-url", "origin", "https://github.com/org/billing/"],
    );
    write(&shadow.join("internal/plan.go"), "package plan // STALE\n");
    let other = root.join("shared-lib");
    fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-q"]);
    git(
        &other,
        &["remote", "add", "origin", "git@github.com:org/shared-lib.git"],
    );
    write(&other.join("caller.go"), "package lib // CALLER\n");
    Clones {
        _dir: dir,
        root,
        review,
        shadow,
        other,
    }
}

fn clones() -> Clones {
    clones_in("root")
}

/// How a tool is run: the agent variables, and whether LLM_PROMPT_UTILS_FILE
/// points at a utils dir the way the engine's run-tool shim sets it.
#[derive(Default, Clone)]
struct Run<'a> {
    vars: Vec<(&'a str, String)>,
    utils_file: Option<PathBuf>,
}

fn review_run(review: &Path) -> Run<'static> {
    Run {
        vars: vec![
            ("LLM_AGENT_VAR_PROJECT_DIR", s(review)),
            ("LLM_AGENT_VAR_REVIEW_GROUNDING", "1".to_string()),
        ],
        utils_file: None,
    }
}

/// Runs one tool the way the engine does for an agent: `main` with the
/// `argc_*` options in the environment, cwd = `cwd`, the agent variables in
/// the env and the tool output collected from `$LLM_OUTPUT`.
fn run_script_tool(
    script: &Path,
    cwd: &Path,
    args: &[(&str, &str)],
    run: &Run,
    cache: &Path,
) -> String {
    let scratch = temp_dir("out");
    let output = scratch.0.join("llm-output");
    let mut cmd = Command::new(bash_program().unwrap());
    cmd.arg("-c")
        .arg("source \"$TOOL_UNDER_TEST\"; main")
        .current_dir(cwd)
        .env("TOOL_UNDER_TEST", script)
        .env("LLM_OUTPUT", &output)
        .env("COYOTE_REVIEW_ORIGIN_CACHE", cache)
        .env_remove("LLM_PROMPT_UTILS_FILE")
        .env_remove("LLM_AGENT_VAR_PROJECT_DIR")
        .env_remove("LLM_AGENT_VAR_REVIEW_GROUNDING");
    for (k, v) in args {
        cmd.env(format!("argc_{k}"), v);
    }
    for (k, v) in &run.vars {
        cmd.env(k, v);
    }
    if let Some(utils) = &run.utils_file {
        cmd.env("LLM_PROMPT_UTILS_FILE", utils);
    }
    let out = cmd.output().unwrap();
    let text = fs::read_to_string(&output).unwrap_or_default();
    format!(
        "{text}{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn run_tool(tool: &str, cwd: &Path, args: &[(&str, &str)], run: &Run, cache: &Path) -> String {
    run_script_tool(&tool_path(tool), cwd, args, run, cache)
}

fn assert_refused(out: &str, checkout_path_tail: &str, label: &str) {
    assert!(
        out.contains("Refused:") && out.contains("another clone of the repository under review"),
        "{label}: must refuse: {out}"
    );
    let named = out
        .lines()
        .find_map(|l| l.strip_prefix("It may not match the code being reviewed. Read the review checkout instead: "))
        .unwrap_or_else(|| panic!("{label}: no checkout path named: {out}"));
    assert!(
        named.replace('\\', "/").ends_with(checkout_path_tail),
        "{label}: must name {checkout_path_tail}: {out}"
    );
}

/// The #1001 shape: the engine's cwd holds a stale clone under the repo name
/// and the reviewer passes a relative path. Every read tool resolves it in the
/// review checkout, whatever characters the checkout's path holds.
#[test]
fn relative_paths_resolve_against_the_review_checkout() {
    if !tools_available() {
        return;
    }
    for root_name in ["root", "root with space", "root's quote"] {
        let c = clones_in(root_name);
        let cache = c.root.join("cache");
        let run = review_run(&c.review);
        for tool in ["fs_cat.sh", "fs_read.sh"] {
            let out = run_tool(tool, &c.root, &[("path", "internal/plan.go")], &run, &cache);
            assert!(out.contains("// HEAD"), "{root_name} {tool}: {out}");
            assert!(!out.contains("STALE"), "{root_name} {tool} read the stale clone: {out}");
        }
        let out = run_tool("fs_grep.sh", &c.root, &[("pattern", "package plan")], &run, &cache);
        assert!(
            out.contains("// HEAD") && !out.contains("STALE"),
            "{root_name}: fs_grep with no path must search the checkout: {out}"
        );
        let out = run_tool("fs_glob.sh", &c.root, &[("pattern", "*.go")], &run, &cache);
        assert!(
            out.contains("review/internal/plan.go") && !out.contains("billing/"),
            "{root_name}: fs_glob with no path must search the checkout: {out}"
        );
        let out = run_tool("fs_ls.sh", &c.root, &[("path", "internal")], &run, &cache);
        assert!(out.contains("plan.go"), "{root_name}: {out}");

        let out = run_tool(
            "fs_read.sh",
            &c.root,
            &[("path", "billing/internal/plan.go")],
            &run,
            &cache,
        );
        assert!(!out.contains("STALE"), "{root_name}: {out}");
        assert!(
            out.contains("did you mean") && out.contains("internal/plan.go"),
            "{root_name}: the miss must name the checkout file it most likely means: {out}"
        );
    }
}

/// The engine's run-tool shim exports LLM_PROMPT_UTILS_FILE; the tools find
/// the guard next to it.
#[test]
fn tools_find_the_guard_through_the_engine_utils_file() {
    if !tools_available() {
        return;
    }
    let c = clones();
    let mut run = review_run(&c.review);
    run.utils_file = Some(manifest().join("assets/functions/utils/prompt-utils.sh"));
    let out = run_tool(
        "fs_cat.sh",
        &c.root,
        &[("path", "internal/plan.go")],
        &run,
        &c.root.join("cache"),
    );
    assert!(out.contains("// HEAD"), "{out}");
}

/// A tool installed without the guard refuses in a review lane and reads
/// normally everywhere else.
#[test]
fn a_missing_guard_refuses_only_in_a_review_lane() {
    if !tools_available() {
        return;
    }
    let c = clones();
    let lone = temp_dir("lone-tool");
    let tools = lone.0.join("tools");
    fs::create_dir_all(&tools).unwrap();
    let script = tools.join("fs_cat.sh");
    fs::copy(tool_path("fs_cat.sh"), &script).unwrap();
    let target = c.review.join("internal/plan.go");
    let target = s(&target);
    let cache = c.root.join("cache");
    let out = run_script_tool(&script, &c.root, &[("path", &target)], &review_run(&c.review), &cache);
    assert!(
        out.contains("Refused: the review path guard") && out.contains("is not installed"),
        "{out}"
    );
    let out = run_script_tool(&script, &c.root, &[("path", &target)], &Run::default(), &cache);
    assert!(out.contains("// HEAD"), "{out}");
}

/// A tool with the guard block and its call lines removed: the tool as it was
/// before review grounding existed.
fn ungrounded_copy(tool: &str, dir: &Path) -> PathBuf {
    let text = fs::read_to_string(tool_path(tool)).unwrap();
    let start = text.find("# Review lanes ground relative paths").unwrap();
    let end_marker = "        REVIEW_GROUNDED_PATH=\"$1\"\n    }\nfi\n";
    let end = text[start..].find(end_marker).unwrap() + start + end_marker.len();
    let mut stripped = format!("{}{}", &text[..start], &text[end..]);
    stripped = stripped
        .lines()
        .filter(|l| !l.contains("review_ground_path \"") && !l.contains("=\"$REVIEW_GROUNDED_PATH\""))
        .map(|l| format!("{l}\n"))
        .collect();
    let path = dir.join(tool);
    fs::write(&path, stripped).unwrap();
    path
}

/// Every non-review agent (no grounding variable) gets byte-identical output
/// from all six tools, project_dir set or not.
#[test]
fn non_review_agents_get_byte_identical_tool_output() {
    if !tools_available() {
        return;
    }
    let c = clones();
    let cache = c.root.join("cache");
    let baselines = temp_dir("baseline");
    let shadow_file = c.shadow.join("internal/plan.go");
    let shadow_file = s(&shadow_file);
    let shadow_dir = s(&c.shadow);
    let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
        ("fs_read.sh", vec![("path", "billing/internal/plan.go")]),
        ("fs_read.sh", vec![("path", &*shadow_file)]),
        ("fs_cat.sh", vec![("path", "billing/internal/plan.go")]),
        ("fs_cat.sh", vec![("path", "missing.go")]),
        ("fs_grep.sh", vec![("pattern", "package")]),
        ("fs_grep.sh", vec![("pattern", "STALE"), ("path", &*shadow_dir)]),
        ("fs_glob.sh", vec![("pattern", "*.go")]),
        ("fs_ls.sh", vec![("path", "billing")]),
        ("ast_grep.sh", vec![("pattern", "package $X"), ("path", &*shadow_dir)]),
    ];
    let project = Run {
        vars: vec![("LLM_AGENT_VAR_PROJECT_DIR", s(&c.review))],
        utils_file: None,
    };
    for (tool, args) in &cases {
        let baseline = ungrounded_copy(tool, &baselines.0);
        for run in [Run::default(), project.clone()] {
            let expected = run_script_tool(&baseline, &c.root, args, &run, &cache);
            let actual = run_tool(tool, &c.root, args, &run, &cache);
            assert_eq!(actual, expected, "{tool} {args:?}");
        }
    }
}

/// The guard block is the same text in every tool, so no tool drifts.
#[test]
fn the_guard_block_is_identical_in_every_tool() {
    let block = |tool: &str| {
        let text = fs::read_to_string(tool_path(tool)).unwrap();
        let start = text.find("# Review lanes ground relative paths").unwrap();
        let end_marker = "        REVIEW_GROUNDED_PATH=\"$1\"\n    }\nfi\n";
        let end = text[start..].find(end_marker).unwrap() + start + end_marker.len();
        text[start..end].to_string()
    };
    let first = block(TOOLS[0]);
    for tool in TOOLS {
        assert_eq!(block(tool), first, "{tool}");
        assert!(
            fs::read_to_string(tool_path(tool))
                .unwrap()
                .contains("review_ground_path \""),
            "{tool} must ground its path argument"
        );
    }
}

/// A read that lands on tracked code in another clone of the reviewed
/// repository is refused, naming the same file in the review checkout,
/// whether it is absolute or relative, a file or a directory.
#[test]
fn reads_in_a_same_origin_clone_are_redirected_to_the_checkout() {
    if !tools_available() {
        return;
    }
    let c = clones();
    let cache = c.root.join("cache");
    let run = review_run(&c.review);
    let shadow_file = c.shadow.join("internal/plan.go");
    let shadow_file = s(&shadow_file);
    let shadow_dir = s(&c.shadow);
    let shadow_sub = s(&c.shadow.join("internal"));
    for (tool, args, tail) in [
        ("fs_cat.sh", vec![("path", &*shadow_file)], "review/internal/plan.go"),
        ("fs_read.sh", vec![("path", &*shadow_file)], "review/internal/plan.go"),
        (
            "fs_read.sh",
            vec![("path", "../billing/internal/plan.go")],
            "review/internal/plan.go",
        ),
        ("fs_ls.sh", vec![("path", &*shadow_dir)], "review"),
        ("fs_grep.sh", vec![("pattern", "STALE"), ("path", &*shadow_sub)], "review/internal"),
        ("fs_glob.sh", vec![("pattern", "*.go"), ("path", &*shadow_dir)], "review"),
        ("ast_grep.sh", vec![("pattern", "package $X"), ("path", &*shadow_sub)], "review/internal"),
    ] {
        let out = run_tool(tool, &c.root, &args, &run, &cache);
        assert_refused(&out, tail, &format!("{tool} {args:?}"));
        assert!(!out.contains("package plan // STALE"), "{tool}: {out}");
    }
}

/// Owner rule: reviewers still read callers and shared libraries in other
/// repositories, files outside any repository, the review checkout itself by
/// absolute path, and untracked local files of a same-origin clone.
#[test]
fn reads_in_other_repositories_and_untracked_files_still_succeed() {
    if !tools_available() {
        return;
    }
    let c = clones();
    let cache = c.root.join("cache");
    let run = review_run(&c.review);
    let caller = s(&c.other.join("caller.go"));
    let out = run_tool("fs_cat.sh", &c.root, &[("path", &caller)], &run, &cache);
    assert!(out.contains("// CALLER"), "{out}");
    let other = s(&c.other);
    let out = run_tool("fs_grep.sh", &c.root, &[("pattern", "CALLER"), ("path", &other)], &run, &cache);
    assert!(out.contains("// CALLER"), "{out}");
    let own = s(&c.review.join("internal/plan.go"));
    let out = run_tool("fs_read.sh", &c.root, &[("path", &own)], &run, &cache);
    assert!(out.contains("// HEAD"), "{out}");
    let outside = c.root.join("notes.txt");
    write(&outside, "NOT A REPO\n");
    let out = run_tool("fs_cat.sh", &c.root, &[("path", &s(&outside))], &run, &cache);
    assert!(out.contains("NOT A REPO"), "{out}");
    let local = c.shadow.join("plans/x.md");
    write(&local, "LOCAL PLAN\n");
    let out = run_tool("fs_cat.sh", &c.root, &[("path", &s(&local))], &run, &cache);
    assert!(
        out.contains("LOCAL PLAN"),
        "an untracked file only the other clone has stays readable: {out}"
    );
}

/// A main clone and a worktree of it (same origin, shared config), the way a
/// task worktree sits next to the checkout coyote was started in.
struct Worktree {
    _dir: TempDir,
    main: PathBuf,
    worktree: PathBuf,
}

fn worktree() -> Worktree {
    let dir = temp_dir("worktree");
    let main = dir.0.join("main");
    fs::create_dir_all(&main).unwrap();
    git(&main, &["init", "-q"]);
    git(
        &main,
        &["remote", "add", "origin", "git@github.com:org/coyote.git"],
    );
    write(&main.join("src/lib.rs"), "// MAIN\n");
    write(&main.join("src/gone.rs"), "// GONE IN THE WORKTREE\n");
    git(&main, &["add", "."]);
    git(&main, &["commit", "-qm", "base"]);
    let worktree = dir.0.join("task-1");
    git(&main, &["worktree", "add", "-q", &s(&worktree), "-b", "task"]);
    write(&worktree.join("src/lib.rs"), "// TASK\n");
    fs::remove_file(worktree.join("src/gone.rs")).unwrap();
    write(&main.join("plans/x.md"), "LOCAL PLAN\n");
    let exclude = main.join(".git/info/exclude");
    let mut text = fs::read_to_string(&exclude).unwrap_or_default();
    text.push_str("plans/\n");
    fs::write(&exclude, text).unwrap();
    Worktree {
        _dir: dir,
        main,
        worktree,
    }
}

/// The default project_dir (`.`, or none at all) names no checkout, so the
/// guard is inert even when the cwd is a same-origin clone of the reviewed
/// repository: reads behave exactly as without the guard.
#[test]
fn the_default_project_dir_leaves_reads_untouched() {
    if !tools_available() {
        return;
    }
    let w = worktree();
    let cache = w.main.join("../cache");
    let task_file = s(&w.worktree.join("src/lib.rs"));
    for project_dir in [None, Some("."), Some("./")] {
        let mut run = Run {
            vars: vec![("LLM_AGENT_VAR_REVIEW_GROUNDING", "1".to_string())],
            utils_file: None,
        };
        if let Some(p) = project_dir {
            run.vars.push(("LLM_AGENT_VAR_PROJECT_DIR", p.to_string()));
        }
        let out = run_tool("fs_cat.sh", &w.main, &[("path", "src/lib.rs")], &run, &cache);
        assert_eq!(out.trim(), "// MAIN", "{project_dir:?}: {out}");
        let out = run_tool("fs_cat.sh", &w.main, &[("path", &task_file)], &run, &cache);
        assert_eq!(out.trim(), "// TASK", "{project_dir:?}: {out}");
    }
    // An inherited project_dir that repeats the cwd's checkout says nothing
    // about which checkout is under review: inert too.
    let run = review_run(&w.main);
    let out = run_tool("fs_cat.sh", &w.main, &[("path", &task_file)], &run, &cache);
    assert_eq!(out.trim(), "// TASK", "{out}");
}

/// project_dir names the task worktree and coyote runs in the main clone: the
/// worktree reads normally, the main clone's tracked code is redirected to the
/// worktree, and only to a path that exists there.
#[test]
fn a_worktree_review_from_the_main_clone_is_grounded() {
    if !tools_available() {
        return;
    }
    let w = worktree();
    let cache = w.main.join("../cache");
    let run = review_run(&w.worktree);
    let out = run_tool("fs_cat.sh", &w.main, &[("path", "src/lib.rs")], &run, &cache);
    assert_eq!(out.trim(), "// TASK", "{out}");
    let task_file = s(&w.worktree.join("src/lib.rs"));
    let out = run_tool("fs_read.sh", &w.main, &[("path", &task_file)], &run, &cache);
    assert!(out.contains("1: // TASK"), "{out}");
    let main_file = s(&w.main.join("src/lib.rs"));
    let out = run_tool("fs_cat.sh", &w.main, &[("path", &main_file)], &run, &cache);
    assert_refused(&out, "task-1/src/lib.rs", "main clone file with a counterpart");
    let gone = s(&w.main.join("src/gone.rs"));
    let out = run_tool("fs_cat.sh", &w.main, &[("path", &gone)], &run, &cache);
    assert_refused(&out, "task-1", "tracked file the worktree deleted");
    assert!(!out.contains("GONE IN THE WORKTREE"), "{out}");
    let plan = s(&w.main.join("plans/x.md"));
    let out = run_tool("fs_cat.sh", &w.main, &[("path", &plan)], &run, &cache);
    assert_eq!(out.trim(), "LOCAL PLAN", "an excluded local file stays readable: {out}");
}

/// The origin lookup is cached per checkout root under a hashed key and
/// refreshed when the repository config changes.
#[test]
fn origin_lookups_are_cached_and_refreshed_with_the_config() {
    if !tools_available() {
        return;
    }
    let c = clones();
    let cache = c.root.join("cache");
    let run = review_run(&c.review);
    let shadow_file = s(&c.shadow.join("internal/plan.go"));
    let out = run_tool("fs_cat.sh", &c.root, &[("path", &shadow_file)], &run, &cache);
    assert!(out.contains("Refused:"), "{out}");
    let entries = || -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = fs::read_dir(&cache)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    fs::read_to_string(e.path()).unwrap(),
                )
            })
            .collect();
        v.sort();
        v
    };
    let first = entries();
    assert_eq!(first.len(), 2, "one entry per checkout root: {first:?}");
    assert!(
        first
            .iter()
            .all(|(k, v)| k.len() == 40 && k.chars().all(|c| c.is_ascii_hexdigit()) && v == "github.com/org/billing"),
        "hashed keys; scp and https origins normalize to one form: {first:?}"
    );
    // A coarse mtime clock must still see the config as newer.
    sleep(Duration::from_millis(1100));
    git(
        &c.shadow,
        &["remote", "set-url", "origin", "git@github.com:org/other.git"],
    );
    let out = run_tool("fs_cat.sh", &c.root, &[("path", &shadow_file)], &run, &cache);
    assert!(
        out.contains("// STALE"),
        "a clone whose origin changed is no longer the reviewed repository: {out}"
    );
    assert!(
        entries().iter().any(|(_, v)| v == "github.com/org/other"),
        "{:?}",
        entries()
    );
}

/// Every review agent that reads code with the fs tools declares the variable
/// that turns the guard on, defaulting to on.
#[test]
fn review_agents_declare_review_grounding_on_by_default() {
    for rel in [
        "code-reviewer/graph.yaml",
        "domain-reviewer/config.yaml",
        "file-reviewer/config.yaml",
        "finding-verifier/graph.yaml",
        "adversary/graph.yaml",
        "security-reviewer/config.yaml",
    ] {
        let path = manifest().join("assets/agents").join(rel);
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let var = doc["variables"]
            .as_sequence()
            .unwrap_or_else(|| panic!("{rel}: variables"))
            .iter()
            .find(|v| v["name"].as_str() == Some("review_grounding"))
            .unwrap_or_else(|| panic!("{rel}: no review_grounding variable"));
        assert_eq!(var["default"].as_str(), Some("1"), "{rel}");
    }
}

fn python_available() -> bool {
    let ok = which::which("python3").is_ok();
    if !ok {
        eprintln!("skipping: python3 not available");
    }
    ok
}

/// Runs a shipped graph script with `state` delivered via GRAPH_STATE_FILE.
fn run_script(rel: &str, state: &Value) -> Value {
    let dir = temp_dir("state");
    let state_file = dir.0.join("state.json");
    fs::write(&state_file, state.to_string()).unwrap();
    let out = Command::new("python3")
        .arg(manifest().join("assets/agents").join(rel))
        .env("GRAPH_STATE_FILE", &state_file)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env_remove("GRAPH_STATE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{rel}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{rel} emitted invalid JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn critical(id: &str, file: &str, lines: &str) -> Value {
    json!({
        "id": id, "severity": "🔴 CRITICAL", "marker": "", "file": file, "lines": lines,
        "title": format!("critical {id}"), "block": format!("#### critical {id}")
    })
}

/// verdict.py, render.py and the gauntlet's verdict_gate.py over one state.
fn review_round(mut state: Value) -> (Value, String, Value) {
    let verdict = run_script("code-reviewer/scripts/verdict.py", &state)["verdict_out"].clone();
    state["verdict_out"] = verdict.clone();
    let report = run_script("code-reviewer/scripts/render.py", &state)["final_report"]
        .as_str()
        .unwrap()
        .to_string();
    let gauntlet = run_script(
        "review-gauntlet/scripts/verdict_gate.py",
        &json!({
            "code_review_results": [report],
            "adversary_results": [],
            "security_results": [],
            "probe_results": []
        }),
    );
    (verdict, report, gauntlet)
}

/// No unverified finding is blocking, the report lists them separately, and
/// the gauntlet never routes them as findings to fix.
fn no_blocking_criticals(label: &str, verdict: &Value, report: &str, gauntlet: &Value) {
    assert_eq!(verdict["counts"]["🔴"], 0, "{label}: {verdict}");
    assert_eq!(verdict["verdict"], "NEEDS-HUMAN", "{label}: {verdict}");
    assert!(
        verdict["findings_final"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["section"] != "blocking"),
        "{label}: no unverified finding may be blocking: {verdict}"
    );
    assert!(
        report.contains("## Unverified findings (verification incomplete)"),
        "{label}: {report}"
    );
    assert!(report.contains("found 0 critical"), "{label}: {report}");
    let detailed = report
        .split("## Detailed Findings")
        .nth(1)
        .unwrap()
        .split("## Unverified findings")
        .next()
        .unwrap();
    assert!(
        detailed.contains("No blocking findings."),
        "{label}: unverified findings must not render as detailed findings: {report}"
    );
    let text = gauntlet["gauntlet_report"].as_str().unwrap();
    assert!(
        !text.contains("🔴 CRITICAL finding(s)"),
        "{label}: the gauntlet must not route unverified criticals as findings to fix: {text}"
    );
}

fn code_review_row(gauntlet: &Value) -> String {
    gauntlet["gauntlet_report"]
        .as_str()
        .unwrap()
        .lines()
        .find(|l| l.starts_with("| code-review |"))
        .unwrap_or_default()
        .to_string()
}

/// The #1001 failure: the verifier faulted and the unverified criticals were
/// posted as blockers. A verification that failed as a whole is an
/// incomplete review; single unverified findings are attention. Neither is
/// ever a blocking finding. Each row pins its exact signal.
#[test]
fn verifier_fault_yields_review_incomplete_and_no_blocking_findings() {
    if !python_available() {
        return;
    }
    let base = json!({
        "changed_files": ["internal/plan.go"],
        "domain_reports": ["clean report. DOMAIN_REVIEW_COMPLETE"],
        "findings": [critical("f1", "internal/plan.go", "10-12"), critical("f2", "internal/plan.go", "20")]
    });
    let soft = r#"[{"id":"f1","verdict":"UNVERIFIABLE","evidence":"","note":"PIPELINE-FAULT: verifier lane failed after retries"},{"id":"f2","verdict":"VERIFIED","evidence":"line 20","note":""}]"#;
    let mut state = base.clone();
    state["verifier_output"] = json!(soft);
    let (verdict, report, gauntlet) = review_round(state);
    assert_eq!(
        verdict["counts"]["🔴"], 1,
        "a verified critical still blocks beside an unverified one: {verdict}"
    );
    assert!(
        verdict["reason"]
            .as_str()
            .unwrap()
            .starts_with("1 finding(s) unverified, listed separately and not blocking"),
        "{verdict}"
    );
    assert!(report.contains("## Unverified findings (verification incomplete)"));
    assert_eq!(gauntlet["gauntlet_incomplete_line"], "", "{gauntlet}");
    assert_eq!(
        code_review_row(&gauntlet),
        "| code-review | BLOCKED | 1 🔴 finding(s) |",
        "{gauntlet}"
    );

    // (label, verifier output, reason prefix, gauntlet verdict, incomplete
    // line, code-review row)
    let whole = "review incomplete: finding verification failed, 2 finding(s) unverified";
    let incomplete_line = "\nGAUNTLET_REVIEW_INCOMPLETE: code-review";
    let degraded_row = "| code-review | BLOCKED | PIPELINE-FAULT (degraded lane) |";
    let attention_row = "| code-review | GREEN (attention) | NEEDS-HUMAN, no 🔴; 2 unverified finding(s) |";
    for (label, verifier_output, reason, gauntlet_verdict, machine, row) in [
        (
            "per-finding faults",
            json!(
                r#"[{"id":"f1","verdict":"UNVERIFIABLE","evidence":"","note":"PIPELINE-FAULT: x"},{"id":"f2","verdict":"UNVERIFIABLE","evidence":"","note":"PIPELINE-FAULT: x"}]"#
            ),
            "2 finding(s) unverified, listed separately and not blocking",
            "PASS",
            "",
            attention_row,
        ),
        (
            "no verdicts returned",
            json!("[]"),
            "2 finding(s) unverified, listed separately and not blocking",
            "PASS",
            "",
            attention_row,
        ),
        (
            "dead verifier",
            json!("Agent node failed: timed out"),
            whole,
            "BLOCKED",
            incomplete_line,
            degraded_row,
        ),
        (
            "verified nothing",
            json!(
                r#"FINDING_VERIFIER_RESULTS
[{"id":"pipeline-fault","verdict":"UNVERIFIABLE","evidence":"","note":"PIPELINE-FAULT: parse failed"}]"#
            ),
            whole,
            "BLOCKED",
            incomplete_line,
            degraded_row,
        ),
    ] {
        let mut state = base.clone();
        state["verifier_output"] = verifier_output;
        let (verdict, report, gauntlet) = review_round(state);
        no_blocking_criticals(label, &verdict, &report, &gauntlet);
        assert!(
            verdict["reason"].as_str().unwrap().starts_with(reason),
            "{label}: {verdict}"
        );
        assert_eq!(gauntlet["gauntlet_verdict"], gauntlet_verdict, "{label}: {gauntlet}");
        assert_eq!(gauntlet["gauntlet_incomplete_line"], machine, "{label}: {gauntlet}");
        assert_eq!(code_review_row(&gauntlet), row, "{label}: {gauntlet}");
        let text = gauntlet["gauntlet_report"].as_str().unwrap();
        if gauntlet_verdict == "PASS" {
            assert!(
                text.contains("- code-review: 2 unverified finding(s) listed separately for a human; not blocking"),
                "{label}: {text}"
            );
        } else {
            assert!(
                text.contains("- code-review: review incomplete \u{2014} finding verification failed; 2 unverified finding(s)"),
                "{label}: {text}"
            );
        }
    }
}

/// Control: a completed verification keeps its verified criticals blocking and
/// drops the provably false ones, with no review-incomplete wording.
#[test]
fn completed_verification_keeps_verified_criticals_blocking() {
    if !python_available() {
        return;
    }
    let state = json!({
        "changed_files": ["internal/plan.go"],
        "domain_reports": ["clean report. DOMAIN_REVIEW_COMPLETE"],
        "findings": [critical("f1", "internal/plan.go", "10"), critical("f2", "internal/plan.go", "20")],
        "verifier_output": r#"[{"id":"f1","verdict":"VERIFIED","evidence":"line 10","note":""},{"id":"f2","verdict":"FALSE","evidence":"","note":"no such call"}]"#
    });
    let (verdict, report, gauntlet) = review_round(state);
    assert_eq!(verdict["counts"]["🔴"], 1, "{verdict}");
    assert_eq!(verdict["dropped_count"], 1, "{verdict}");
    assert!(
        !verdict["reason"]
            .as_str()
            .unwrap()
            .contains("review incomplete"),
        "{verdict}"
    );
    assert!(!report.contains("## Unverified findings"), "{report}");
    assert!(
        gauntlet["gauntlet_report"]
            .as_str()
            .unwrap()
            .contains("1 🔴 CRITICAL finding(s)"),
        "{gauntlet}"
    );
    assert_eq!(gauntlet["gauntlet_incomplete_line"], "", "{gauntlet}");
}

/// A git repository with a base commit and a head commit, the shape a PR
/// review checks out: `internal/plan.go` grows from 30 to 38 lines with line 5
/// changed, and the head adds generated files of every recognised kind.
struct PrRepo {
    _dir: TempDir,
    root: PathBuf,
    repo: PathBuf,
    base: String,
}

fn numbered(n: usize, changed: Option<usize>) -> String {
    (1..=n)
        .map(|i| {
            if Some(i) == changed {
                format!("changed {i}\n")
            } else {
                format!("line {i}\n")
            }
        })
        .collect()
}

fn pr_repo() -> PrRepo {
    let dir = temp_dir("pr");
    let root = dir.0.clone();
    let repo = root.join("pr-review-billing-1001");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    write(&repo.join("internal/plan.go"), &numbered(30, None));
    write(&repo.join("internal/other.go"), "package other\n");
    write(
        &repo.join(".gitattributes"),
        "api/*.ts linguist-generated\n",
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let out = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let base = String::from_utf8_lossy(&out.stdout).trim().to_string();
    write(&repo.join("internal/plan.go"), &numbered(38, Some(5)));
    write(&repo.join("gen/go/admin/v1/service.pb.go"), "package v1\n");
    write(&repo.join("internal/rpc/admin.connect.go"), "package rpc\n");
    write(&repo.join("internal/store/plan_gen.go"), "package store\n");
    write(&repo.join("internal/msgs_pb.ts"), "export {}\n");
    write(&repo.join("api/client.ts"), "export {}\n");
    write(
        &repo.join("internal/codec.go"),
        "// Code generated by protoc-gen-x. DO NOT EDIT.\npackage internal\n",
    );
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "head"]);
    PrRepo {
        _dir: dir,
        root,
        repo,
        base,
    }
}

fn finding(id: &str, severity: &str, file: &str, lines: &str) -> Value {
    json!({
        "id": id, "severity": severity, "marker": "", "file": file, "lines": lines,
        "title": format!("finding {id}"), "block": format!("#### finding {id}")
    })
}

fn facts(pr: &PrRepo) -> Value {
    run_script(
        "code-reviewer/scripts/facts.py",
        &json!({"project_dir_in": pr.repo.to_string_lossy(), "diff_spec": pr.base}),
    )
}

/// Generated files never reach domain grouping or review, and the report
/// lists them as generated, not reviewed.
#[test]
fn generated_files_are_excluded_from_review_and_listed() {
    if !python_available() || !tools_available() {
        return;
    }
    let pr = pr_repo();
    let f = facts(&pr);
    assert_eq!(f["changed_files"], json!(["internal/plan.go"]), "{f}");
    assert_eq!(f["file_count"], 1, "{f}");
    let mut generated: Vec<&str> = f["generated_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g.as_str().unwrap())
        .collect();
    generated.sort_unstable();
    assert_eq!(
        generated,
        [
            "api/client.ts",
            "gen/go/admin/v1/service.pb.go",
            "internal/codec.go",
            "internal/msgs_pb.ts",
            "internal/rpc/admin.connect.go",
            "internal/store/plan_gen.go",
        ],
        "linguist-generated, the DO NOT EDIT header and every generated path shape: {f}"
    );
    let groups = run_script("code-reviewer/scripts/group_domains.py", &f);
    let grouped: Vec<&str> = groups["proposed_groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["files"].as_array().unwrap())
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(grouped, ["internal/plan.go"], "{groups}");

    let mut state = f.clone();
    state["domain_reports"] = json!(["clean report. DOMAIN_REVIEW_COMPLETE"]);
    state["findings"] = json!([]);
    let (verdict, report, _) = review_round(state);
    assert_eq!(verdict["verdict"], "MERGE-READY", "{verdict}");
    let section = report
        .split("## Generated, not reviewed")
        .nth(1)
        .unwrap_or_else(|| panic!("the report must list generated files: {report}"));
    assert!(
        section.contains("`gen/go/admin/v1/service.pb.go`"),
        "{report}"
    );
}

/// The anchor gate keeps findings inside or next to a changed hunk and drops
/// the rest with a reason: a path outside the diff, a generated file, a line
/// past the end of the file, a range far from every change, no range at all.
#[test]
fn anchor_gate_drops_unanchored_findings_and_keeps_valid_ones() {
    if !python_available() || !tools_available() {
        return;
    }
    let pr = pr_repo();
    let mut state = facts(&pr);
    let absolute = pr.repo.join("internal/plan.go");
    state["findings"] = json!([
        finding("past-eof", "🔴 CRITICAL", "internal/plan.go", "45-50"),
        finding("on-change", "🔴 CRITICAL", "internal/plan.go", "5"),
        finding("adjacent", "🟡 WARNING", "internal/plan.go", "L7-L8"),
        finding("appended", "🟡 WARNING", "internal/plan.go", "33-35"),
        finding(
            "repo-prefixed",
            "🟡 WARNING",
            "billing/internal/plan.go",
            "4-6"
        ),
        finding(
            "absolute",
            "🟢 SUGGESTION",
            &absolute.to_string_lossy(),
            "5"
        ),
        finding("not-in-diff", "🔴 CRITICAL", "internal/other.go", "1"),
        finding(
            "generated",
            "🔴 CRITICAL",
            "gen/go/admin/v1/service.pb.go",
            "1"
        ),
        finding("far", "🟡 WARNING", "internal/plan.go", "15-17"),
        finding("no-lines", "🟢 SUGGESTION", "internal/plan.go", "")
    ]);
    let out = run_script("code-reviewer/scripts/anchor_gate.py", &state);
    assert_eq!(out["anchor_checked"], true, "{out}");
    assert_eq!(out["anchor_error"], "", "{out}");
    let kept: Vec<(&str, &str)> = out["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["id"].as_str().unwrap(), f["file"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kept,
        [
            ("on-change", "internal/plan.go"),
            ("adjacent", "internal/plan.go"),
            ("appended", "internal/plan.go"),
            ("repo-prefixed", "internal/plan.go"),
            ("absolute", "internal/plan.go"),
        ],
        "{out}"
    );
    let reasons: Vec<(&str, &str)> = out["unanchored"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| (u["id"].as_str().unwrap(), u["reason"].as_str().unwrap()))
        .collect();
    let reason = |id: &str| {
        reasons
            .iter()
            .find(|(i, _)| *i == id)
            .unwrap_or_else(|| panic!("{id} must be dropped: {out}"))
            .1
    };
    assert!(reason("past-eof").contains("outside internal/plan.go (38 lines)"));
    assert!(reason("not-in-diff").contains("not a changed file of this diff"));
    assert!(reason("generated").contains("generated file, not reviewed"));
    assert!(reason("far").contains("not within 3 lines of a change"));
    assert!(reason("no-lines").contains("no line range"));
    assert_eq!(reasons.len(), 5, "{out}");
}

/// A gate that never ran (state still at its seeded false) or crashed is a
/// pipeline fault, never an unchecked pass; dropped criticals keep the round
/// NEEDS-HUMAN and are listed under "Could not anchor".
#[test]
fn anchor_gate_faults_and_drops_reach_the_verdict() {
    if !python_available() {
        return;
    }
    let base = json!({
        "changed_files": ["a.go"],
        "domain_reports": ["clean report. DOMAIN_REVIEW_COMPLETE"],
        "findings": [finding("f1", "🟢 SUGGESTION", "a.go", "1")],
        "verifier_output": "[]"
    });
    let mut state = base.clone();
    state["anchor_checked"] = json!(false);
    let verdict = run_script("code-reviewer/scripts/verdict.py", &state)["verdict_out"].clone();
    assert!(
        verdict["reason"]
            .as_str()
            .unwrap()
            .contains("pipeline fault(s) recorded"),
        "{verdict}"
    );
    let nowhere = json!({"project_dir": "/nonexistent/checkout", "findings": []});
    let empty = run_script("code-reviewer/scripts/anchor_gate.py", &nowhere);
    assert_eq!(
        empty,
        json!({"findings": [], "unanchored": [], "anchor_checked": true, "anchor_error": ""}),
        "no findings: an empty result without running git"
    );
    let crashed = run_script(
        "code-reviewer/scripts/anchor_gate.py",
        &json!({
            "project_dir": "/nonexistent/checkout",
            "findings": [finding("f1", "🟢 SUGGESTION", "a.go", "1")]
        }),
    );
    assert!(
        crashed["anchor_error"]
            .as_str()
            .unwrap()
            .starts_with("PIPELINE-FAULT: anchor check failed"),
        "{crashed}"
    );
    let mut state = base.clone();
    state["anchor_checked"] = json!(true);
    state["anchor_error"] = crashed["anchor_error"].clone();
    let verdict = run_script("code-reviewer/scripts/verdict.py", &state)["verdict_out"].clone();
    assert_eq!(verdict["verdict"], "NEEDS-HUMAN", "{verdict}");

    let mut state = base;
    state["anchor_checked"] = json!(true);
    state["unanchored"] = json!([{
        "id": "f9", "severity": "🔴 CRITICAL", "title": "store.SetEndDate is unsafe",
        "file": "internal/plan.go", "lines": "45-50",
        "reason": "lines 45-50 are outside internal/plan.go (38 lines)"
    }]);
    let (verdict, report, gauntlet) = review_round(state);
    assert_eq!(verdict["verdict"], "NEEDS-HUMAN", "{verdict}");
    assert!(
        verdict["reason"]
            .as_str()
            .unwrap()
            .contains("1 🔴 finding(s) could not be anchored in the diff"),
        "{verdict}"
    );
    let section = report
        .split("## Could not anchor")
        .nth(1)
        .unwrap_or_else(|| panic!("{report}"));
    assert!(
        section.contains("store.SetEndDate is unsafe") && section.contains("38 lines"),
        "{report}"
    );
    assert!(report.contains("found 0 critical"), "{report}");
    assert!(
        gauntlet["gauntlet_report"]
            .as_str()
            .unwrap()
            .contains("- code-review: 1 critical finding(s) could not be anchored; listed for a human"),
        "{gauntlet}"
    );
    assert_eq!(code_review_row(&gauntlet), "| code-review | GREEN (attention) | NEEDS-HUMAN, no 🔴 |");
}

/// The #1001 round end to end: a stale clone of the same repository sits in
/// the engine's cwd under the repository name, a reviewer reads a relative
/// path, the synthesis invents a range past the end of a file, the verifier
/// faults. The read is redirected, the fabricated finding is dropped, and the
/// round carries no blocker.
#[test]
fn regression_pr_1001_shape_posts_no_fabricated_blockers() {
    if !python_available() || !tools_available() {
        return;
    }
    let pr = pr_repo();
    let shadow = pr.root.join("billing");
    git(
        &pr.root,
        &[
            "clone",
            "-q",
            pr.repo.to_str().unwrap(),
            shadow.to_str().unwrap(),
        ],
    );
    git(&shadow, &["checkout", "-q", &pr.base]);
    git(
        &pr.repo,
        &[
            "remote",
            "add",
            "origin",
            "git@github.com:docker/billing.git",
        ],
    );
    git(
        &shadow,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/docker/billing",
        ],
    );
    let run = review_run(&pr.repo);
    let cache = pr.root.join("cache");
    let out = run_tool(
        "fs_read.sh",
        &pr.root,
        &[("path", "billing/internal/plan.go")],
        &run,
        &cache,
    );
    assert!(
        !out.contains("1: line 1") && out.contains("did you mean"),
        "the relative path #1001 used must resolve in the checkout, never the stale clone: {out}"
    );
    let stale = shadow.join("internal/plan.go");
    let out = run_tool(
        "fs_read.sh",
        &pr.root,
        &[("path", &stale.to_string_lossy())],
        &run,
        &cache,
    );
    assert!(
        out.contains("Refused:") && out.contains("pr-review-billing-1001/internal/plan.go"),
        "a read in the stale clone must be redirected to the review checkout: {out}"
    );
    let out = run_tool(
        "fs_read.sh",
        &pr.root,
        &[("path", "internal/plan.go")],
        &run,
        &cache,
    );
    assert!(
        out.contains("changed 5") && out.contains("38: line 38"),
        "a relative read must see the PR head: {out}"
    );

    let mut state = facts(&pr);
    state["findings"] = json!([
        finding("fabricated", "🔴 CRITICAL", "internal/plan.go", "45-50"),
        finding(
            "generated",
            "🔴 CRITICAL",
            "gen/go/admin/v1/service.pb.go",
            "1"
        ),
        finding("real", "🔴 CRITICAL", "internal/plan.go", "5")
    ]);
    let anchored = run_script("code-reviewer/scripts/anchor_gate.py", &state);
    for key in ["findings", "unanchored", "anchor_checked", "anchor_error"] {
        state[key] = anchored[key].clone();
    }
    assert_eq!(state["findings"].as_array().unwrap().len(), 1, "{state}");
    state["domain_reports"] = json!(["clean report. DOMAIN_REVIEW_COMPLETE"]);
    state["verifier_output"] = json!("Agent node failed: verification could not be completed");
    let (verdict, report, gauntlet) = review_round(state);
    no_blocking_criticals("pr-1001", &verdict, &report, &gauntlet);
    assert!(report.contains("## Could not anchor"), "{report}");
    assert!(report.contains("## Generated, not reviewed"), "{report}");
}

/// The synthesis keeps prior-art notes as context and anchors consumer
/// findings at the changed line; the reviewers anchor and read the checkout.
#[test]
fn synthesis_and_reviewer_wording_is_in_place() {
    let read = |rel: &str| fs::read_to_string(manifest().join("assets/agents").join(rel)).unwrap();
    let graph = read("code-reviewer/graph.yaml");
    for needle in [
        "**Prior-art and incident notes are context, not requirements.**",
        "An incident id that appears nowhere in the diff is context only.",
        "ANCHORED AT THE CHANGED LINE IN THIS",
        "naming\n          the consuming repo + file:line as evidence",
        "**Anchor every finding in the diff.**",
        "next: anchor_gate",
        "script: scripts/anchor_gate.py",
    ] {
        assert!(
            graph.contains(needle),
            "code-reviewer graph.yaml lacks {needle:?}"
        );
    }
    for rel in ["domain-reviewer/config.yaml", "file-reviewer/config.yaml"] {
        let text = read(rel).split_whitespace().collect::<Vec<_>>().join(" ");
        for needle in [
            "**Anchor every finding in the diff.**",
            "A caller, consumer or unchanged twin outside the diff, in this repository or another, is anchored at the changed line with the external location (repo + path:line) cited as evidence.",
            "or lines within 3 of it",
            "**Read the checkout under review.**",
        ] {
            assert!(text.contains(needle), "{rel} lacks {needle:?}");
        }
    }
    let security = read("security-reviewer/instructions.md");
    assert!(security.contains("**Read the checkout under review.**"));
    let gate = fs::read_to_string(manifest().join("assets/agents/code-reviewer/scripts/anchor_gate.py")).unwrap();
    assert!(gate.contains("\nADJACENT = 3\n"), "the adjacency the wording names");
    assert!(gate.contains("not within {ADJACENT} lines of a change"));
    assert!(graph.contains("within 3 lines of a changed hunk"));
    let readme = read("code-reviewer/README.md");
    assert!(readme.contains("The `probe` lane is not grounded"));
}

/// A repository whose head renames-and-edits a file, adds files whose names
/// git would quote or tab-terminate (a space, a quote, non-ASCII), and deletes
/// lines from another file.
fn awkward_repo() -> (TempDir, PathBuf, String) {
    let dir = temp_dir("awkward");
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    write(&repo.join("src/old_name.go"), &numbered(20, None));
    write(&repo.join("src/del.go"), &numbered(20, None));
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["mv", "src/old_name.go", "src/new_name.go"]);
    write(&repo.join("src/new_name.go"), &numbered(20, Some(10)));
    let kept: String = numbered(20, None)
        .lines()
        .enumerate()
        .filter(|(i, _)| !(11..15).contains(i))
        .map(|(_, l)| format!("{l}\n"))
        .collect();
    write(&repo.join("src/del.go"), &kept);
    write(&repo.join("src/with space.go"), &numbered(5, None));
    write(&repo.join("src/na\u{ef}ve.go"), &numbered(5, None));
    if cfg!(unix) {
        write(&repo.join("src/qu\"ote.go"), &numbered(5, None));
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "head"]);
    (dir, repo, base)
}

/// Renamed-and-edited, spaced, quoted and non-ASCII paths anchor on their
/// changed lines; deleted lines anchor by their old-side numbers; several
/// ranges anchor when one does; an absolute path outside the checkout is
/// dropped as such, even when its tail matches a changed file.
#[test]
fn anchor_gate_reads_renames_and_awkward_paths() {
    if !python_available() || !tools_available() {
        return;
    }
    let (_dir, repo, base) = awkward_repo();
    let mut state = run_script(
        "code-reviewer/scripts/facts.py",
        &json!({"project_dir_in": s(&repo), "diff_spec": base}),
    );
    let changed: Vec<&str> = state["changed_files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    for path in ["src/new_name.go", "src/with space.go", "src/na\u{ef}ve.go"] {
        assert!(changed.contains(&path), "{path} must be a verbatim changed path: {changed:?}");
    }
    let mut findings = vec![
        finding("rename", "🔴 CRITICAL", "src/new_name.go", "10"),
        finding("space", "🔴 CRITICAL", "src/with space.go", "3"),
        finding("non-ascii", "🔴 CRITICAL", "src/na\u{ef}ve.go", "3"),
        finding("deleted-side", "🔴 CRITICAL", "src/del.go", "13-14"),
        finding("multi-range", "🟡 WARNING", "src/del.go", "1, 11"),
        finding("outside", "🔴 CRITICAL", &s(&env::temp_dir().join("elsewhere/src/del.go")), "12"),
    ];
    if cfg!(unix) {
        findings.push(finding("quote", "🔴 CRITICAL", "src/qu\"ote.go", "2"));
    }
    state["findings"] = json!(findings);
    state["diff_spec"] = json!(base);
    let out = run_script("code-reviewer/scripts/anchor_gate.py", &state);
    let kept: Vec<&str> = out["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    let mut expected = vec!["rename", "space", "non-ascii", "deleted-side", "multi-range"];
    if cfg!(unix) {
        expected.push("quote");
    }
    assert_eq!(kept, expected, "{out}");
    let dropped = &out["unanchored"][0];
    assert_eq!(dropped["id"], "outside", "{out}");
    assert!(
        dropped["reason"]
            .as_str()
            .unwrap()
            .contains("cites a file outside the review checkout"),
        "{out}"
    );
}

/// A diff that edits .gitattributes cannot hide files behind linguist marks,
/// and a file that gains the generated header in this diff is still reviewed;
/// both say so. `gen/` alone is not a generated shape.
#[test]
fn generated_marks_cannot_launder_a_change() {
    if !python_available() || !tools_available() {
        return;
    }
    let dir = temp_dir("launder");
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    write(&repo.join("src/auth.go"), "package auth\n");
    write(&repo.join("README.md"), "x\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    write(&repo.join(".gitattributes"), "src/*.go linguist-generated\n");
    write(
        &repo.join("src/auth.go"),
        "// Code generated by hand. DO NOT EDIT.\npackage auth // BYPASS\n",
    );
    write(&repo.join("tools/gen/main.go"), "package main\n");
    write(
        &repo.join("src/note.go"),
        "package note\n// see Code generated by x. DO NOT EDIT. in the docs\n",
    );
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "head"]);
    let f = run_script(
        "code-reviewer/scripts/facts.py",
        &json!({"project_dir_in": s(&repo), "diff_spec": base}),
    );
    assert_eq!(f["generated_files"], json!([]), "{f}");
    for path in ["src/auth.go", "tools/gen/main.go", "src/note.go"] {
        assert!(
            f["changed_files"].as_array().unwrap().contains(&json!(path)),
            "{path} must be reviewed: {f}"
        );
    }
    let notes = f["generated_notes"].to_string();
    assert!(notes.contains("changes .gitattributes"), "{f}");
    assert!(notes.contains("src/auth.go gained a 'Code generated"), "{f}");
    let mut state = f.clone();
    state["domain_reports"] = json!(["clean report. DOMAIN_REVIEW_COMPLETE"]);
    state["findings"] = json!([]);
    let verdict = run_script("code-reviewer/scripts/verdict.py", &state)["verdict_out"].clone();
    assert_eq!(verdict["verdict"], "NEEDS-HUMAN", "{verdict}");
    assert!(
        verdict["attention"].to_string().contains("gained a 'Code generated"),
        "{verdict}"
    );
}

/// A diff of generated files only is NEEDS-HUMAN with the files listed, never
/// the "No changes to review." stub.
#[test]
fn a_generated_only_diff_is_never_a_silent_pass() {
    if !python_available() {
        return;
    }
    let state = json!({
        "changed_files": [],
        "generated_files": ["gen/go/v1/service.pb.go"],
        "domain_reports": [],
        "findings": []
    });
    let (verdict, report, _) = review_round(state);
    assert_eq!(verdict["verdict"], "NEEDS-HUMAN", "{verdict}");
    assert!(
        verdict["reason"]
            .as_str()
            .unwrap()
            .contains("1 generated file(s) changed, none reviewed"),
        "{verdict}"
    );
    assert!(!report.contains("No changes to review."), "{report}");
    assert!(
        report.contains("## Generated, not reviewed") && report.contains("gen/go/v1/service.pb.go"),
        "{report}"
    );
}
