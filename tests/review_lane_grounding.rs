//! Review lanes read the code under review and report only what the diff
//! holds. These tests pin the deterministic guards behind that:
//!
//! - the read-only fs tools (`assets/functions/tools/fs_*.sh` through
//!   `assets/functions/utils/review-paths.sh`) resolve a relative path against
//!   the review checkout and refuse a read inside another clone of the same
//!   origin repository, while other repositories stay readable;
//! - the review agents declare the `review_grounding` variable that turns the
//!   guard on;
//! - code-reviewer fails closed when finding verification does not complete:
//!   unverified findings are listed separately, never as blocking, and the
//!   review gauntlet reads the lane as an incomplete review.
//!
//! The fixture mirrors the failure it guards against: a review checkout, a
//! stale clone of the same repository in the working directory under the
//! repository's name, and an unrelated shared library.

use serde_json::{json, Value};
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
        panic!("{rel} emitted invalid JSON ({e}): {}", String::from_utf8_lossy(&out.stdout))
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

fn no_blocking_criticals(label: &str, verdict: &Value, report: &str, gauntlet: &Value) {
    assert_eq!(verdict["counts"]["🔴"], 0, "{label}: {verdict}");
    assert!(
        verdict["reason"]
            .as_str()
            .unwrap()
            .starts_with("review incomplete:"),
        "{label}: the reason must open with review incomplete: {verdict}"
    );
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
    assert_eq!(gauntlet["gauntlet_verdict"], "BLOCKED", "{label}: {gauntlet}");
}

/// The #1001 failure: the verifier faulted and the unverified criticals were
/// posted as blockers. Every way verification can fail to complete yields no
/// blocking finding and an explicit review incomplete.
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
            .starts_with("review incomplete: 1 finding(s) unverified"),
        "{verdict}"
    );
    assert!(report.contains("## Unverified findings (verification incomplete)"));
    assert!(
        gauntlet["gauntlet_incomplete_line"]
            .as_str()
            .unwrap()
            .contains("GAUNTLET_REVIEW_INCOMPLETE: code-review"),
        "an incomplete code review is named on the machine line: {gauntlet}"
    );

    for (label, verifier_output) in [
        (
            "per-finding faults",
            json!(r#"[{"id":"f1","verdict":"UNVERIFIABLE","evidence":"","note":"PIPELINE-FAULT: x"},{"id":"f2","verdict":"UNVERIFIABLE","evidence":"","note":"PIPELINE-FAULT: x"}]"#),
        ),
        ("dead verifier", json!("Agent node failed: timed out")),
        (
            "verified nothing",
            json!(r#"FINDING_VERIFIER_RESULTS
[{"id":"pipeline-fault","verdict":"UNVERIFIABLE","evidence":"","note":"PIPELINE-FAULT: parse failed"}]"#),
        ),
        ("no verdicts returned", json!("[]")),
    ] {
        let mut state = base.clone();
        state["verifier_output"] = verifier_output;
        let (verdict, report, gauntlet) = review_round(state);
        no_blocking_criticals(label, &verdict, &report, &gauntlet);
        let machine = gauntlet["gauntlet_incomplete_line"].as_str().unwrap();
        let text = gauntlet["gauntlet_report"].as_str().unwrap();
        assert!(
            machine.contains("code-review") || text.contains("internal PIPELINE-FAULT"),
            "{label}: the lane must read as incomplete or degraded, never as findings: {gauntlet}"
        );
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
