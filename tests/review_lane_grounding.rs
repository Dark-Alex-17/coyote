//! Review lanes read the code under review and report only what the diff
//! holds. These tests pin the deterministic guards behind that:
//!
//! - the read-only fs tools (`assets/functions/tools/fs_*.sh` through
//!   `assets/functions/utils/review-paths.sh`) resolve a relative path against
//!   the review checkout and refuse a read inside another clone of the same
//!   origin repository, while other repositories stay readable;
//! - the review agents declare the `review_grounding` variable that turns the
//!   guard on.
//!
//! The fixture mirrors the failure it guards against: a review checkout, a
//! stale clone of the same repository in the working directory under the
//! repository's name, and an unrelated shared library.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The first bash on PATH outside System32, where Windows ships the WSL
/// launcher (not a shell).
fn bash_program() -> Option<PathBuf> {
    which::which_all("bash").ok()?.find(|p| {
        !p.to_string_lossy()
            .to_ascii_lowercase()
            .replace('\\', "/")
            .contains("/windows/system32/")
    })
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

fn temp_dir(label: &str) -> TempDir {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!("coyote-grounding-{label}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

fn git(dir: &Path, args: &[&str]) {
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
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// `root/review` (the checkout under review, origin in scp form), `root/billing`
/// (a stale clone of the same repository, origin in https form) and
/// `root/shared-lib` (another repository).
struct Clones {
    _dir: TempDir,
    root: PathBuf,
    review: PathBuf,
    shadow: PathBuf,
    other: PathBuf,
}

fn clones() -> Clones {
    let dir = temp_dir("clones");
    let root = dir.0.clone();
    let seed = root.join("seed");
    fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-q"]);
    write(&seed.join("internal/plan.go"), "package plan // HEAD\n");
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-qm", "seed"]);
    let review = root.join("review");
    let shadow = root.join("billing");
    for clone in [&review, &shadow] {
        git(
            &root,
            &["clone", "-q", seed.to_str().unwrap(), clone.to_str().unwrap()],
        );
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

/// Runs one fs tool the way the engine does for an agent: `main` with the
/// `argc_*` options set, cwd = `cwd`, the agent variables in the env and the
/// tool output collected from `$LLM_OUTPUT`.
fn run_tool(
    tool: &str,
    cwd: &Path,
    args: &[(&str, &str)],
    vars: &[(&str, &str)],
    cache: &Path,
) -> String {
    let script = manifest().join("assets/functions/tools").join(tool);
    let output = cwd.join(format!(".out-{tool}"));
    let _ = fs::remove_file(&output);
    let mut snippet = format!("source '{}'", script.display());
    for (k, v) in args {
        snippet.push_str(&format!("; argc_{k}='{v}'"));
    }
    snippet.push_str("; main");
    let mut cmd = Command::new(bash_program().unwrap());
    cmd.arg("-c")
        .arg(snippet)
        .current_dir(cwd)
        .env("LLM_OUTPUT", &output)
        .env("COYOTE_REVIEW_ORIGIN_CACHE", cache)
        .env_remove("LLM_PROMPT_UTILS_FILE")
        .env_remove("LLM_AGENT_VAR_PROJECT_DIR")
        .env_remove("LLM_AGENT_VAR_REVIEW_GROUNDING");
    for (k, v) in vars {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let text = fs::read_to_string(&output).unwrap_or_default();
    let _ = fs::remove_file(&output);
    format!("{text}{}", String::from_utf8_lossy(&out.stderr))
}

fn review_vars(review: &Path) -> Vec<(&'static str, String)> {
    vec![
        (
            "LLM_AGENT_VAR_PROJECT_DIR",
            review.to_string_lossy().into_owned(),
        ),
        ("LLM_AGENT_VAR_REVIEW_GROUNDING", "1".to_string()),
    ]
}

fn as_refs(vars: &[(&'static str, String)]) -> Vec<(&'static str, &str)> {
    vars.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

/// The #1001 shape: the engine's cwd holds a stale clone under the repo name
/// and the reviewer passes a relative path. Every read tool resolves it in the
/// review checkout.
#[test]
fn relative_paths_resolve_against_the_review_checkout() {
    if !tools_available() {
        return;
    }
    let c = clones();
    let cache = c.root.join("cache");
    let vars = review_vars(&c.review);
    let vars = as_refs(&vars);
    for tool in ["fs_cat.sh", "fs_read.sh"] {
        let out = run_tool(tool, &c.root, &[("path", "internal/plan.go")], &vars, &cache);
        assert!(out.contains("// HEAD"), "{tool} must read the checkout: {out}");
        assert!(!out.contains("STALE"), "{tool} read the stale clone: {out}");
    }
    let out = run_tool(
        "fs_grep.sh",
        &c.root,
        &[("pattern", "package plan")],
        &vars,
        &cache,
    );
    assert!(
        out.contains("// HEAD") && !out.contains("STALE"),
        "fs_grep with no path must search the checkout, not the cwd: {out}"
    );
    let out = run_tool("fs_glob.sh", &c.root, &[("pattern", "*.go")], &vars, &cache);
    assert!(
        out.contains("review/internal/plan.go") && !out.contains("billing/"),
        "fs_glob with no path must search the checkout: {out}"
    );
    let out = run_tool("fs_ls.sh", &c.root, &[("path", "internal")], &vars, &cache);
    assert!(out.contains("plan.go"), "fs_ls must list the checkout: {out}");

    let out = run_tool(
        "fs_read.sh",
        &c.root,
        &[("path", "billing/internal/plan.go")],
        &vars,
        &cache,
    );
    assert!(
        !out.contains("STALE"),
        "a repo-name-prefixed relative path must not reach the stale clone: {out}"
    );
    assert!(
        out.contains("did you mean") && out.contains("internal/plan.go"),
        "the miss must name the checkout file it most likely means: {out}"
    );
}

/// Without the review variable the tools keep their old cwd semantics, so
/// non-review agents are untouched.
#[test]
fn tools_outside_a_review_lane_keep_cwd_semantics() {
    if !tools_available() {
        return;
    }
    let c = clones();
    let cache = c.root.join("cache");
    let project = c.review.to_string_lossy().into_owned();
    let out = run_tool(
        "fs_cat.sh",
        &c.root,
        &[("path", "billing/internal/plan.go")],
        &[("LLM_AGENT_VAR_PROJECT_DIR", project.as_str())],
        &cache,
    );
    assert!(out.contains("STALE"), "{out}");
}

/// A read that lands in another clone of the reviewed repository is refused
/// with the review checkout's path, whether it is absolute or relative, a
/// file or a directory, present or missing.
#[test]
fn reads_in_a_same_origin_clone_are_redirected_to_the_checkout() {
    if !tools_available() {
        return;
    }
    let c = clones();
    let cache = c.root.join("cache");
    let vars = review_vars(&c.review);
    let vars = as_refs(&vars);
    let shadow_file = c.shadow.join("internal/plan.go");
    let shadow_file = shadow_file.to_string_lossy();
    let shadow_dir = c.shadow.to_string_lossy();
    let missing = c.shadow.join("internal/gone.go");
    let missing = missing.to_string_lossy();
    for (tool, args) in [
        ("fs_cat.sh", vec![("path", &*shadow_file)]),
        ("fs_read.sh", vec![("path", &*shadow_file)]),
        ("fs_read.sh", vec![("path", "../billing/internal/plan.go")]),
        ("fs_cat.sh", vec![("path", &*missing)]),
        ("fs_ls.sh", vec![("path", &*shadow_dir)]),
        ("fs_grep.sh", vec![("pattern", "STALE"), ("path", &*shadow_dir)]),
        ("fs_glob.sh", vec![("pattern", "*.go"), ("path", &*shadow_dir)]),
    ] {
        let out = run_tool(tool, &c.root, &args, &vars, &cache);
        assert!(
            out.contains("Refused:") && out.contains("another clone of the repository under review"),
            "{tool} {args:?} must refuse the shadow clone: {out}"
        );
        assert!(
            out.contains("Read the review checkout instead:") && out.contains("/review"),
            "{tool} {args:?} must name the review checkout: {out}"
        );
        assert!(!out.contains("package plan // STALE"), "{tool}: {out}");
    }
    let out = run_tool(
        "fs_cat.sh",
        &c.root,
        &[("path", &*shadow_file)],
        &vars,
        &cache,
    );
    assert!(
        out.contains("review/internal/plan.go"),
        "the refusal must point at the same file in the checkout: {out}"
    );
}

/// Owner rule: reviewers still read callers and shared libraries in other
/// repositories, and the review checkout itself by absolute path.
#[test]
fn reads_in_other_repositories_still_succeed() {
    if !tools_available() {
        return;
    }
    let c = clones();
    let cache = c.root.join("cache");
    let vars = review_vars(&c.review);
    let vars = as_refs(&vars);
    let caller = c.other.join("caller.go");
    let out = run_tool(
        "fs_cat.sh",
        &c.root,
        &[("path", &caller.to_string_lossy())],
        &vars,
        &cache,
    );
    assert!(out.contains("// CALLER"), "{out}");
    let out = run_tool(
        "fs_grep.sh",
        &c.root,
        &[("pattern", "CALLER"), ("path", &c.other.to_string_lossy())],
        &vars,
        &cache,
    );
    assert!(out.contains("// CALLER"), "{out}");
    let own = c.review.join("internal/plan.go");
    let out = run_tool(
        "fs_read.sh",
        &c.root,
        &[("path", &own.to_string_lossy())],
        &vars,
        &cache,
    );
    assert!(out.contains("// HEAD"), "{out}");
    let outside = c.root.join("notes.txt");
    write(&outside, "NOT A REPO\n");
    let out = run_tool(
        "fs_cat.sh",
        &c.root,
        &[("path", &outside.to_string_lossy())],
        &vars,
        &cache,
    );
    assert!(out.contains("NOT A REPO"), "{out}");
}

/// The origin lookup is cached per checkout root, and a cache directory the
/// user does not own is ignored rather than trusted.
#[test]
fn origin_lookups_are_cached_per_checkout_root() {
    if !tools_available() {
        return;
    }
    let c = clones();
    let cache = c.root.join("cache");
    let vars = review_vars(&c.review);
    let vars = as_refs(&vars);
    let shadow_file = c.shadow.join("internal/plan.go");
    run_tool(
        "fs_cat.sh",
        &c.root,
        &[("path", &shadow_file.to_string_lossy())],
        &vars,
        &cache,
    );
    let entries: Vec<String> = fs::read_dir(&cache)
        .unwrap()
        .map(|e| fs::read_to_string(e.unwrap().path()).unwrap())
        .collect();
    assert_eq!(entries.len(), 2, "one entry per checkout root: {entries:?}");
    assert!(
        entries.iter().all(|e| e == "github.com/org/billing"),
        "scp and https origins normalize to one form: {entries:?}"
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
    for tool in ["fs_read.sh", "fs_cat.sh", "fs_grep.sh", "fs_glob.sh", "fs_ls.sh"] {
        let text =
            fs::read_to_string(manifest().join("assets/functions/tools").join(tool)).unwrap();
        assert!(
            text.contains("review_ground_path \""),
            "{tool} must ground its path argument"
        );
    }
}
