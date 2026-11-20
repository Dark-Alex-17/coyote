//! Evidence and default rules of the adversary graph's deterministic scripts:
//! criteria met by inspection, red verification runs compared with a
//! baseline run on the diff base or rerun on a snapshot of a dirty tree, red
//! runs judged by the pass condition their criterion states, the run_checks
//! deadline starting after the
//! review-stack lock, holistic complaint severity plus the criterion
//! format-failure fallback, the diff base and repository facts diff_facts.py
//! records, execution criteria no recorded run covers, and replays of two
//! recorded reviews' deterministic evidence. Each test spawns the real script
//! with a JSON state file and skips when python3 (or bash and git, where
//! needed) is missing.

use serde_json::{Value, json};
use std::env;
use std::fs;
#[cfg(unix)]
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, Stdio};
use std::process::{Command, Output};
#[cfg(unix)]
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

fn asset(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/agents")
        .join(rel)
}

/// Removes the fixture dir even when an assertion panics mid-run.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture_dir(label: &str) -> TempDirGuard {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!("coyote-adversary-evidence-{label}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    TempDirGuard(dir)
}

fn run(output: Output) -> (i32, String, String) {
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
        String::from_utf8_lossy(&output.stderr).replace("\r\n", "\n"),
    )
}

fn missing(tools: &[&str]) -> bool {
    let absent: Vec<_> = tools.iter().filter(|t| which::which(t).is_err()).collect();
    if !absent.is_empty() {
        eprintln!("skipping: {absent:?} not available");
    }
    !absent.is_empty()
}

/// Runs adversary script `script` over `state` and returns its JSON output.
fn script(dir: &Path, script: &str, label: &str, state: &Value, envs: &[(&str, &str)]) -> Value {
    script_text(dir, script, label, &state.to_string(), envs)
}

fn script_text(dir: &Path, script: &str, label: &str, state: &str, envs: &[(&str, &str)]) -> Value {
    let state_file = dir.join(format!("state-{label}.json"));
    fs::write(&state_file, state).unwrap();
    let mut cmd = Command::new("python3");
    cmd.arg(asset(&format!("adversary/scripts/{script}")))
        .env("GRAPH_STATE_FILE", &state_file)
        .env("ADVERSARY_STACK_LOCK_PATH", dir.join("stack.lock"))
        .env_remove("GRAPH_STATE")
        .env("PYTHONDONTWRITEBYTECODE", "1");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let (code, stdout, stderr) = run(cmd.output().expect("spawn python3"));
    assert_eq!(
        code, 0,
        "{script} ({label})\nstdout: {stdout}\nstderr: {stderr}"
    );
    serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("{script} ({label}) invalid JSON ({e}): {stdout}\nstderr: {stderr}")
    })
}

fn report(dir: &Path, label: &str, state: &Value) -> String {
    let out = script(dir, "verdict.py", label, state, &[]);
    out["adv_report"].as_str().unwrap().to_string()
}

fn met(id: &str) -> Value {
    json!({"id": id, "text": format!("does {id}"), "status": "MET",
           "evidence": "src/x.rs:1 + test tests/x.rs:9", "complaint": "", "no_test_reason": ""})
}

fn verdict_state(verdicts: Value) -> Value {
    json!({
        "pipeline_faults": [],
        "crit_verdicts": verdicts,
        "extra_complaints": [],
        "format_rulings": [],
        "observations": "",
        "exec_results": [],
    })
}

fn gate(dir: &Path, label: &str, attempts: u64, raw: &str) -> Value {
    script(
        dir,
        "crit_gate.py",
        label,
        &json!({
            "criterion": {"id": "c1", "text": "the spec"},
            "gate_attempts": attempts,
            "crit_verdict": raw,
        }),
        &[],
    )
}

#[test]
fn inspected_criterion_counts_as_met_and_needs_a_citation_and_reason() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("inspected");
    let state = verdict_state(json!([
        met("c1"),
        {"id": "c2", "text": "documents the flag", "status": "INSPECTED",
         "evidence": "README.md:12 describes the flag", "complaint": "",
         "no_test_reason": "documentation has no behavior a test could observe"}
    ]));
    let text = report(&dir.0, "verdict", &state);
    assert!(
        text.starts_with(
            "ADVERSARIAL_REVIEW: CONFORMS\nCriteria: 2/2 met (1 test-proven, 1 by inspection)."
        ),
        "{text}"
    );
    let section = text
        .split("Inspected, not test-proven:\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no inspected section: {text}"));
    assert!(
        section.contains("README.md:12 describes the flag")
            && section.contains("documentation has no behavior a test could observe"),
        "{text}"
    );

    let accepted = gate(
        &dir.0,
        "accepted",
        0,
        r#"{"status": "INSPECTED", "evidence": "docs/a.md:3", "complaint": "", "no_test_reason": "prose only"}"#,
    );
    assert_eq!(
        accepted["crit_verdict"]["status"], "INSPECTED",
        "{accepted}"
    );
    assert_eq!(accepted["crit_verdict"]["no_test_reason"], "prose only");

    for (label, raw, needle) in [
        (
            "no-citation",
            r#"{"status": "INSPECTED", "evidence": "the docs say so", "complaint": "", "no_test_reason": "prose only"}"#,
            "file:line",
        ),
        (
            "no-reason",
            r#"{"status": "INSPECTED", "evidence": "docs/a.md:3", "complaint": ""}"#,
            "no_test_reason",
        ),
    ] {
        let out = gate(&dir.0, label, 0, raw);
        assert_eq!(out["_next"], "check_criterion", "{label}: {out}");
        assert!(
            out["gate_feedback"].as_str().unwrap().contains(needle),
            "{label}: the feedback must name what is missing: {out}"
        );
    }
}

#[test]
fn format_rulings_must_pass_the_criterion_contract() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("ruling-contract");
    let unverified = json!({"id": "c2", "text": "does c2", "status": "UNVERIFIED",
        "evidence": "garbage output", "complaint": "criterion check output failed machine validation",
        "no_test_reason": ""});
    for (label, ruling, needle) in [
        (
            "inspected-empty",
            json!({"id": "c2", "status": "INSPECTED", "evidence": "", "complaint": "",
                   "no_test_reason": "prose only"}),
            "INSPECTED requires non-empty evidence",
        ),
        (
            "inspected-no-reason",
            json!({"id": "c2", "status": "INSPECTED", "evidence": "docs/a.md:3",
                   "complaint": "", "no_test_reason": ""}),
            "INSPECTED requires a non-empty no_test_reason",
        ),
        (
            "met-empty",
            json!({"id": "c2", "status": "MET", "evidence": " ", "complaint": ""}),
            "MET requires non-empty evidence",
        ),
        (
            "unmet-silent",
            json!({"id": "c2", "status": "UNMET", "evidence": "src/y.rs:2", "complaint": ""}),
            "UNMET requires a non-empty complaint",
        ),
    ] {
        let mut state = verdict_state(json!([met("c1"), unverified.clone()]));
        state["format_rulings"] = json!([ruling]);
        let text = report(&dir.0, label, &state);
        assert!(
            text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "{label}: {text}"
        );
        assert!(
            text.contains("1. PIPELINE-FAULT: criterion verdict unreadable after re-asks")
                && text.contains(&format!("holistic format ruling rejected: {needle}")),
            "{label}: the fault must quote the rejection: {text}"
        );
        assert!(
            !text.contains("(ruled by holistic: output format failed)"),
            "{label}: {text}"
        );
    }
}

#[test]
fn a_crashed_criterion_gate_records_unverified() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("gate-crash");
    let out = script_text(&dir.0, "crit_gate.py", "crash", "not json", &[]);
    let verdict = &out["crit_verdict"];
    assert_eq!(verdict["status"], "UNVERIFIED", "{out}");
    assert!(
        verdict["complaint"]
            .as_str()
            .unwrap()
            .starts_with("criterion gate error: "),
        "{out}"
    );
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("spawn git");
    let (code, stdout, stderr) = run(out);
    assert_eq!(code, 0, "git {args:?}\nstdout: {stdout}\nstderr: {stderr}");
    stdout.trim().to_string()
}

#[cfg(unix)]
#[test]
fn red_runs_are_compared_with_a_baseline_on_the_diff_base() {
    if missing(&["python3", "bash", "git"]) {
        return;
    }
    let dir = fixture_dir("baseline");
    let repo = dir.0.join("repo");
    let sub = repo.join("sub");
    fs::create_dir_all(&sub).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(
        sub.join("old.sh"),
        "echo 'FAILED: known breakage'\nexit 1\n",
    )
    .unwrap();
    fs::write(sub.join("new.sh"), "exit 0\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(
        sub.join("new.sh"),
        "echo 'FAILED: fresh breakage'\nexit 1\n",
    )
    .unwrap();
    git(&repo, &["commit", "-q", "-am", "head"]);

    let out = script(
        &dir.0,
        "run_checks.py",
        "run-checks",
        &json!({
            "project_dir": sub.to_string_lossy(),
            "verification_commands": ["sh old.sh", "sh new.sh"],
            "diff_base": base,
        }),
        &[],
    );
    let results = out["exec_results"]
        .as_array()
        .unwrap_or_else(|| panic!("exec_results must be a record list: {out}"));
    assert_eq!(results.len(), 2, "{out}");
    assert_eq!(results[0]["exit"], 1, "{out}");
    assert_eq!(results[0]["baseline"]["status"], "pre-existing", "{out}");
    assert_eq!(results[0]["baseline"]["exit"], 1, "{out}");
    assert_eq!(results[1]["exit"], 1, "{out}");
    assert_eq!(results[1]["baseline"]["status"], "regression", "{out}");
    assert_eq!(results[1]["baseline"]["exit"], 0, "{out}");
    assert_eq!(
        git(&repo, &["worktree", "list"]).lines().count(),
        1,
        "the baseline worktree must be removed"
    );

    let mut state = verdict_state(json!([met("c1")]));
    state["exec_results"] = json!([results[0]]);
    let text = report(&dir.0, "pre-existing", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
    assert!(
        text.contains("Pre-existing failures (not blocking):\n- `sh old.sh`"),
        "{text}"
    );

    state["exec_results"] = json!(results);
    let text = report(&dir.0, "regression", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"), "{text}");
    assert!(
        text.contains("1. Verification run `sh new.sh`")
            && text.contains("baseline on the diff base: regression"),
        "{text}"
    );
    assert!(!text.contains("Verification run `sh old.sh`"), "{text}");
}

/// The saved diff-base run records under the repository's git dir.
#[cfg(unix)]
fn saved_base_runs(repo: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(repo.join(".git/coyote-baselines")) else {
        return Vec::new();
    };
    let mut runs: Vec<PathBuf> = entries
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("adversary-"))
        })
        .collect();
    runs.sort();
    runs
}

#[cfg(unix)]
fn line_count(path: &Path) -> usize {
    fs::read_to_string(path).map_or(0, |s| s.lines().count())
}

/// A completed diff-base run is saved with its full output and a later run
/// classifies against it without running the command on the base again; a
/// planted record is what the comparison reads, an unreadable one is a miss
/// that is never replaced, and a base run killed by a signal is never saved.
#[cfg(unix)]
#[test]
fn the_diff_base_run_is_saved_and_reused_by_later_runs() {
    if missing(&["python3", "bash", "git"]) {
        return;
    }
    let dir = fixture_dir("saved-base");
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    let counted = dir.0.join("counted-runs.txt");
    let killed = dir.0.join("killed-runs.txt");
    fs::write(
        repo.join("counted.sh"),
        format!(
            "echo run >> '{}'\necho 'FAILED tests/t.py::one'\nexit 1\n",
            counted.display()
        ),
    )
    .unwrap();
    fs::write(
        repo.join("killed.sh"),
        format!("echo run >> '{}'\nkill -KILL $$\n", killed.display()),
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let head_script = "echo 'FAILED tests/t.py::one'\nexit 1\n";
    fs::write(repo.join("counted.sh"), head_script).unwrap();
    fs::write(repo.join("killed.sh"), head_script).unwrap();
    git(&repo, &["commit", "-q", "-am", "head"]);

    let state = json!({
        "project_dir": repo.to_string_lossy(),
        "verification_commands": ["sh counted.sh", "sh killed.sh"],
        "diff_base": base,
    });
    let first = script(&dir.0, "run_checks.py", "saved-1", &state, &[]);
    assert_eq!(
        first["exec_results"][0]["baseline"]["status"], "pre-existing",
        "{first}"
    );
    assert_eq!(
        (line_count(&counted), line_count(&killed)),
        (1, 1),
        "{first}"
    );
    let runs = saved_base_runs(&repo);
    assert_eq!(
        runs.len(),
        1,
        "only the completed base run is saved: {runs:?}"
    );
    let saved: Value = serde_json::from_str(&fs::read_to_string(&runs[0]).unwrap()).unwrap();
    assert_eq!(saved["exit"], 1, "{saved}");
    assert_eq!(saved["output"], "FAILED tests/t.py::one\n", "{saved}");

    let second = script(&dir.0, "run_checks.py", "saved-2", &state, &[]);
    assert_eq!(line_count(&counted), 1, "the saved base run was run again");
    assert_eq!(line_count(&killed), 2, "a killed base run must run again");
    assert_eq!(
        second["exec_results"][0]["baseline"], first["exec_results"][0]["baseline"],
        "{second}"
    );
    assert_eq!(
        second["exec_results"][1]["baseline"], first["exec_results"][1]["baseline"],
        "{second}"
    );
    assert_eq!(git(&repo, &["worktree", "list"]).lines().count(), 1);

    let only_counted = json!({
        "project_dir": repo.to_string_lossy(),
        "verification_commands": ["sh counted.sh"],
        "diff_base": base,
    });
    let mut planted = saved.clone();
    planted["output"] = json!("FAILED tests/t.py::other\n");
    fs::write(&runs[0], planted.to_string()).unwrap();
    let out = script(&dir.0, "run_checks.py", "saved-planted", &only_counted, &[]);
    assert_eq!(line_count(&counted), 1, "{out}");
    assert_eq!(
        out["exec_results"][0]["baseline"]["status"], "regression",
        "{out}"
    );
    assert!(
        out["exec_results"][0]["baseline"]["note"]
            .as_str()
            .unwrap()
            .contains("new failing tests: tests/t.py::one"),
        "{out}"
    );

    fs::write(&runs[0], "not json").unwrap();
    let out = script(
        &dir.0,
        "run_checks.py",
        "saved-unreadable",
        &only_counted,
        &[],
    );
    assert_eq!(
        line_count(&counted),
        2,
        "an unreadable record is a miss: {out}"
    );
    assert_eq!(
        out["exec_results"][0]["baseline"], first["exec_results"][0]["baseline"],
        "{out}"
    );
    assert_eq!(fs::read_to_string(&runs[0]).unwrap(), "not json");
}

/// A repository whose `check.sh` fails at HEAD and runs `base_script` at the
/// diff base; returns the repo and the diff-base commit.
#[cfg(unix)]
fn repo_with_base_script(dir: &Path, base_script: &str) -> (PathBuf, String) {
    let repo = dir.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(repo.join("check.sh"), base_script).unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(
        repo.join("check.sh"),
        "echo 'FAILED tests/t.py::one'\nexit 1\n",
    )
    .unwrap();
    git(&repo, &["commit", "-q", "-am", "head"]);
    (repo, base)
}

#[cfg(unix)]
#[test]
fn a_base_run_that_times_out_is_never_saved() {
    if missing(&["python3", "bash", "git"]) {
        return;
    }
    let dir = fixture_dir("saved-base-timeout");
    let (repo, base) = repo_with_base_script(&dir.0, "exec sleep 30\n");
    let out = script(
        &dir.0,
        "run_checks.py",
        "saved-timeout",
        &json!({
            "project_dir": repo.to_string_lossy(),
            "verification_commands": ["sh check.sh"],
            "diff_base": base,
        }),
        &[("ADVERSARY_RUN_CHECKS_DEADLINE_SECS", "3")],
    );
    assert_eq!(out["exec_results"][0]["exit"], 1, "{out}");
    assert_eq!(
        out["exec_results"][0]["baseline"]["note"], "the command timed out on the diff base",
        "{out}"
    );
    assert!(saved_base_runs(&repo).is_empty(), "{out}");
}

#[cfg(unix)]
#[test]
fn a_base_run_whose_output_shows_a_signal_kill_is_never_saved() {
    if missing(&["python3", "bash", "git"]) {
        return;
    }
    let dir = fixture_dir("saved-base-killed");
    let (repo, base) = repo_with_base_script(
        &dir.0,
        "echo \"error: test failed, process didn't exit successfully: \\`t\\` (signal: 9, SIGKILL: kill)\"\nexit 1\n",
    );
    let out = script(
        &dir.0,
        "run_checks.py",
        "saved-killed",
        &json!({
            "project_dir": repo.to_string_lossy(),
            "verification_commands": ["sh check.sh"],
            "diff_base": base,
        }),
        &[],
    );
    assert_eq!(out["exec_results"][0]["baseline"]["exit"], 1, "{out}");
    assert!(saved_base_runs(&repo).is_empty(), "{out}");
}

/// A PATH whose first entry is a git shim logging every call's arguments to
/// the returned file before running the real git.
#[cfg(unix)]
fn logging_git_path(dir: &Path) -> (String, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let real_git = Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let log = dir.join("git-calls.txt");
    fs::write(
        bin.join("git"),
        format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real_git.trim()
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (path, log)
}

/// The logged git calls whose arguments contain `needle`.
#[cfg(unix)]
fn git_calls(log: &Path, needle: &str) -> usize {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(needle))
        .count()
}

/// A git shim first on PATH logs every git call: the first run misses the
/// saved base runs and adds the diff-base worktree, the second hits for
/// every command and adds none.
#[cfg(unix)]
#[test]
fn an_all_hit_run_adds_no_diff_base_worktree() {
    if missing(&["python3", "bash", "git"]) {
        return;
    }
    let dir = fixture_dir("saved-base-shim");
    let (repo, base) = repo_with_base_script(&dir.0, "echo 'FAILED tests/t.py::other'\nexit 1\n");
    let (path, log) = logging_git_path(&dir.0);
    let state = json!({
        "project_dir": repo.to_string_lossy(),
        "verification_commands": ["sh check.sh"],
        "diff_base": base,
    });
    let worktree_adds = || git_calls(&log, "worktree add");
    let miss = script(
        &dir.0,
        "run_checks.py",
        "shim-miss",
        &state,
        &[("PATH", &path)],
    );
    assert_eq!(worktree_adds(), 1, "{miss}");
    let hit = script(
        &dir.0,
        "run_checks.py",
        "shim-hit",
        &state,
        &[("PATH", &path)],
    );
    assert_eq!(worktree_adds(), 1, "an all-hit run added a worktree: {hit}");
    assert_eq!(
        hit["exec_results"][0]["baseline"], miss["exec_results"][0]["baseline"],
        "{hit}"
    );
}

#[cfg(unix)]
#[test]
fn baseline_classification_follows_the_failing_test_ids_and_skips_user_hooks() {
    use std::os::unix::fs::PermissionsExt;

    if missing(&["python3", "bash", "git"]) {
        return;
    }
    let dir = fixture_dir("classify");
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    let failing = |ids: &[&str], code: i32| {
        let lines: String = ids
            .iter()
            .map(|id| format!("echo 'FAILED {id}'\n"))
            .collect();
        format!("{lines}exit {code}\n")
    };
    let cases = [
        (
            "subset.sh",
            &["tests/t.py::one", "tests/t.py::two"][..],
            1,
            &["tests/t.py::one"][..],
            1,
        ),
        (
            "new-id.sh",
            &["tests/t.py::one"][..],
            1,
            &["tests/t.py::one", "tests/t.py::three"][..],
            1,
        ),
        (
            "new-exit.sh",
            &["tests/t.py::one"][..],
            1,
            &["tests/t.py::one"][..],
            2,
        ),
    ];
    for (name, base_ids, base_code, _, _) in cases {
        fs::write(repo.join(name), failing(base_ids, base_code)).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    for (name, _, _, head_ids, head_code) in cases {
        fs::write(repo.join(name), failing(head_ids, head_code)).unwrap();
    }
    git(&repo, &["commit", "-q", "-am", "head"]);
    let marker = dir.0.join("hook-ran");
    let hook = repo.join(".git/hooks/post-checkout");
    fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

    let cmds: Vec<String> = cases.iter().map(|c| format!("sh {}", c.0)).collect();
    let out = script(
        &dir.0,
        "run_checks.py",
        "classify",
        &json!({"project_dir": repo.to_string_lossy(), "verification_commands": cmds,
                "diff_base": base}),
        &[],
    );
    let results = out["exec_results"]
        .as_array()
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(results[0]["baseline"]["status"], "pre-existing", "{out}");
    assert_eq!(results[1]["baseline"]["status"], "regression", "{out}");
    assert!(
        results[1]["baseline"]["note"]
            .as_str()
            .unwrap()
            .contains("new failing tests: tests/t.py::three"),
        "{out}"
    );
    assert_eq!(results[2]["baseline"]["status"], "regression", "{out}");
    assert!(
        results[2]["baseline"]["note"]
            .as_str()
            .unwrap()
            .contains("exit 2 here but 1 on the diff base"),
        "{out}"
    );
    assert!(
        !marker.exists(),
        "the user's post-checkout hook must not run"
    );
    assert_eq!(git(&repo, &["worktree", "list"]).lines().count(), 1);
}

#[test]
fn a_red_run_without_a_diff_base_has_a_not_run_baseline_and_blocks() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("no-base");
    let out = script(
        &dir.0,
        "run_checks.py",
        "no-base",
        &json!({"project_dir": dir.0.to_string_lossy(), "verification_commands": ["exit 3"]}),
        &[],
    );
    let record = &out["exec_results"][0];
    assert_eq!(record["exit"], 3, "{out}");
    assert_eq!(record["baseline"]["status"], "not-run", "{out}");

    let mut state = verdict_state(json!([met("c1")]));
    state["exec_results"] = out["exec_results"].clone();
    let text = report(&dir.0, "no-base", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"), "{text}");
    assert!(
        text.contains("baseline on the diff base: not-run"),
        "{text}"
    );
}

#[test]
fn diff_facts_resolves_the_diff_base_of_each_spec_form() {
    if missing(&["python3", "git"]) {
        return;
    }
    let dir = fixture_dir("diff-base");
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(repo.join("f.txt"), "1\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "c1"]);
    let c1 = git(&repo, &["rev-parse", "HEAD"]);
    let main = git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
    git(&repo, &["checkout", "-q", "-b", "side"]);
    fs::write(repo.join("g.txt"), "side\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "s1"]);
    let s1 = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", &main]);
    fs::write(repo.join("f.txt"), "2\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "c2"]);
    let c2 = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("f.txt"), "3\n").unwrap();
    git(&repo, &["add", "f.txt"]);

    let facts = |label: &str, spec: &str| {
        script(
            &dir.0,
            "diff_facts.py",
            label,
            &json!({"project_dir": repo.to_string_lossy(), "diff_spec": spec}),
            &[],
        )
    };
    for (label, spec, want) in [
        ("auto", "auto".to_string(), &c2),
        ("two-dot", format!("{c2}..{s1}"), &c2),
        ("three-dot", format!("{c2}...{s1}"), &c1),
    ] {
        let out = facts(label, &spec);
        assert_eq!(out["diff_base"], *want, "{label}: {out}");
    }
    for (label, spec) in [
        ("two-dot-dash", format!("{c2}..--output=x")),
        ("three-dot-dash", format!("{c2}...-x")),
    ] {
        let out = facts(label, &spec);
        assert_eq!(out["diff_base"], "", "{label}: {out}");
        assert!(
            out["diff_note"]
                .as_str()
                .unwrap()
                .contains("option-shaped side"),
            "{label}: {out}"
        );
    }
}

/// Kills and reaps the lock holder even when an assertion panics mid-run.
#[cfg(unix)]
struct ChildGuard(Child);

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
#[test]
fn run_checks_deadline_starts_after_the_stack_lock_and_skips_are_not_red() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("lock-deadline");
    let lock = dir.0.join("stack.lock");
    let holder = Command::new("python3")
        .args([
            "-c",
            "import fcntl, os, sys, time\n\
             fd = os.open(sys.argv[1], os.O_CREAT | os.O_RDWR)\n\
             fcntl.flock(fd, fcntl.LOCK_EX)\n\
             print('locked', flush=True)\n\
             time.sleep(4)\n",
        ])
        .arg(&lock)
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn lock holder");
    let mut holder = ChildGuard(holder);
    let mut line = String::new();
    BufReader::new(holder.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "locked");

    // run_checks.py polls a held lock every 5 s and the holder keeps it for
    // 4 s, so the run waits about 5 s: past the 2 s deadline, which would
    // skip the command had the wait counted against it.
    let started = Instant::now();
    let out = script(
        &dir.0,
        "run_checks.py",
        "locked",
        &json!({"project_dir": dir.0.to_string_lossy(), "verification_commands": ["echo ran"]}),
        &[
            ("ADVERSARY_STACK_LOCK_WAIT_SECS", "3"),
            ("ADVERSARY_RUN_CHECKS_DEADLINE_SECS", "2"),
        ],
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed > Duration::from_secs(2),
        "the lock must have been contended past the deadline, took {elapsed:?}"
    );
    let record = &out["exec_results"][0];
    assert!(
        record.get("skipped").is_none(),
        "the lock wait must not spend the deadline: {out}"
    );
    assert_eq!(record["exit"], 0, "{out}");

    let mut state = verdict_state(json!([met("c1")]));
    state["exec_results"] = json!([{"cmd": "echo late", "skipped": "deadline"}]);
    let text = report(&dir.0, "skipped", &state);
    assert!(
        text.starts_with(
            "ADVERSARIAL_REVIEW: CONFORMS\nCriteria: 1/1 met (1 test-proven, 0 by inspection); \
             1 declared command(s) not run (deadline), judged by the criterion checks."
        ),
        "{text}"
    );
    assert!(text.contains("- [NOT RUN] `echo late`"), "{text}");

    let partial = json!({"id": "c2", "text": "does c2", "status": "PARTIAL",
        "evidence": "", "complaint": "its coverage never ran", "no_test_reason": ""});
    state["crit_verdicts"] = json!([met("c1"), partial]);
    let text = report(&dir.0, "skipped-partial", &state);
    assert!(
        text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
        "a PARTIAL criterion next to a skipped run must still block: {text}"
    );
    assert!(
        text.contains("1 declared command(s) not run (deadline)"),
        "{text}"
    );
}

#[cfg(unix)]
#[test]
fn a_timed_out_run_skips_the_baseline_and_later_commands_get_no_budget() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("deadline-skip");
    let out = script(
        &dir.0,
        "run_checks.py",
        "deadline",
        &json!({"project_dir": dir.0.to_string_lossy(),
                "verification_commands": ["sleep 5", "echo late"],
                "diff_base": "HEAD"}),
        &[("ADVERSARY_RUN_CHECKS_DEADLINE_SECS", "1")],
    );
    let results = out["exec_results"]
        .as_array()
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(results[0]["exit"], -1, "{out}");
    assert_eq!(results[0]["baseline"]["status"], "not-run", "{out}");
    assert_eq!(
        results[0]["baseline"]["note"], "the command timed out here, so no comparison applies",
        "{out}"
    );
    assert_eq!(results[1]["skipped"], "deadline", "{out}");
    assert!(
        results[1].get("exit").is_none() && results[1].get("baseline").is_none(),
        "{out}"
    );
}

#[test]
fn holistic_severity_and_criterion_format_fallback() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("holistic");
    let complaint = |blocking: bool| {
        json!([{"kind": "Spec-endorsed waste", "location": "src/x.rs:4",
                "violation": "per-item calls", "fix": "use the batch form",
                "blocking": blocking}])
    };
    let mut state = verdict_state(json!([met("c1")]));
    state["extra_complaints"] = complaint(false);
    let text = report(&dir.0, "advisory", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
    assert!(
        text.contains("Non-blocking holistic complaints:\n- Spec-endorsed waste -- src/x.rs:4"),
        "{text}"
    );
    state["extra_complaints"] = complaint(true);
    let text = report(&dir.0, "blocking", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"), "{text}");
    assert!(text.contains("1. Spec-endorsed waste"), "{text}");
    for (label, flag) in [
        ("blocking-missing", None),
        ("blocking-no", Some(json!("no"))),
    ] {
        let mut complaint = complaint(false);
        match flag {
            Some(value) => complaint[0]["blocking"] = value,
            None => {
                complaint[0].as_object_mut().unwrap().remove("blocking");
            }
        }
        state["extra_complaints"] = complaint;
        let text = report(&dir.0, label, &state);
        assert!(
            text.starts_with("ADVERSARIAL_REVIEW: DIVERGES")
                && text.contains("1. Spec-endorsed waste"),
            "only an explicit false is advisory ({label}): {text}"
        );
    }

    let out = gate(&dir.0, "second", 1, "garbage output");
    assert_eq!(out["_next"], "check_criterion", "{out}");
    assert_eq!(out["gate_attempts"], 2, "{out}");
    assert!(
        out["gate_feedback"]
            .as_str()
            .unwrap()
            .contains("garbage output"),
        "{out}"
    );
    let out = gate(&dir.0, "third", 2, "garbage output");
    assert_eq!(out["crit_verdict"]["status"], "UNVERIFIED", "{out}");
    assert_eq!(out["crit_verdict"]["evidence"], "garbage output", "{out}");
    let out = gate(
        &dir.0,
        "salvaged",
        2,
        r#"{"status": " unmet ", "evidence": "", "complaint": ""}"#,
    );
    assert_eq!(out["crit_verdict"]["status"], "UNMET", "{out}");

    let unverified = json!({"id": "c2", "text": "does c2", "status": "UNVERIFIED",
        "evidence": "garbage output", "complaint": "criterion check output failed machine validation",
        "no_test_reason": ""});
    let mut state = verdict_state(json!([met("c1"), unverified]));
    state["format_rulings"] = json!([{"id": "c2", "status": "MET",
        "evidence": "src/y.rs:2 + test tests/y.rs:7", "complaint": "", "no_test_reason": ""}]);
    let text = report(&dir.0, "ruled", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
    assert!(
        text.contains("(ruled by holistic: output format failed)"),
        "{text}"
    );

    state["format_rulings"] = json!([]);
    let text = report(&dir.0, "unruled", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"), "{text}");
    assert!(
        text.contains("degraded run: pipeline fault recorded")
            && text.contains("1. PIPELINE-FAULT: criterion verdict unreadable after re-asks"),
        "{text}"
    );

    let partial = json!({"id": "c3", "text": "does c3", "status": "PARTIAL",
        "evidence": "", "complaint": "no test", "no_test_reason": ""});
    let mut state = verdict_state(json!([met("c1"), partial]));
    state["format_rulings"] = json!([{"id": "c3", "status": "MET",
        "evidence": "src/z.rs:1", "complaint": "", "no_test_reason": ""}]);
    let text = report(&dir.0, "not-unverified", &state);
    assert!(
        text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
        "a ruling for a criterion that is not UNVERIFIED must be ignored: {text}"
    );
    assert!(
        text.contains("Partial") && text.contains("no test"),
        "{text}"
    );
}

/// The two holistic complaints a field run blocked on: scope drift whose
/// every location sits in the run's own planning files.
fn planning_drift_complaints() -> Value {
    json!([
        {"kind": "Scope drift / out-of-scope",
         "location": "plans/tasks/TASK-005-entitlements-time-accessors/log.md:17 (fix-round-2 entry)",
         "violation": "TASK-005 may only touch entitlements/resolver.go, resolver_test.go, catalog.go, \
    billing/service.go and cmd/billingd/main.go (index.md `files:`). In fix-round-2 it also ran \
    `git rm --cached` on httpapi/usage_webhook_probe_test.go",
         "fix": "keep the task inside its file list",
         "blocking": true},
        {"kind": "Scope drift / out-of-scope",
         "location": "plans/PLAN-subscription-discounts.md:363 (also :288, \
    plans/tasks/TASK-004-store-discounts-repository/index.md:43)",
         "violation": "TASK-004's file list is store/discounts.go, store/discounts_test.go and \
    billing/service.go (plan row T4 at :242, index.md:9-12). The diff goes beyond that and rewrites \
    acceptance criteria in the plan",
         "fix": "leave the plan to the planner",
         "blocking": true}
    ])
}

#[test]
fn scope_drift_confined_to_the_planning_dirs_is_advisory() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("planning-drift");
    let mut state = verdict_state(json!([met("c1"), met("c2")]));
    state["debris_exclude"] = json!(r#"["plans"]"#);
    state["extra_complaints"] = planning_drift_complaints();
    let text = report(&dir.0, "replay", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
    assert!(
        text.contains(
            "Non-blocking holistic complaints:\n\
             - Scope drift / out-of-scope -- plans/tasks/TASK-005-entitlements-time-accessors/log.md:17"
        ),
        "{text}"
    );
    assert!(
        text.contains("\n- Scope drift / out-of-scope -- plans/PLAN-subscription-discounts.md:363"),
        "{text}"
    );
    assert_eq!(
        text.matches("scope drift confined to the run's planning files")
            .count(),
        2,
        "{text}"
    );
}

#[test]
fn scope_drift_outside_the_planning_dirs_still_blocks() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("genuine-drift");
    let complaint = |kind: &str, location: &str| {
        json!({"kind": kind, "location": location, "violation": "edits outside the task",
               "fix": "revert it", "blocking": true})
    };
    for (label, extra) in [
        (
            "code-path",
            complaint("Scope drift / out-of-scope", "src/billing/other.go:12"),
        ),
        (
            "mixed",
            complaint("Scope drift / out-of-scope", "plans/x.md:3, src/a.go:9"),
        ),
        (
            "mixed-anchored-name",
            complaint("Scope drift / out-of-scope", "plans/x.md:3, Makefile:12"),
        ),
        (
            "mixed-hash-anchor",
            complaint("Scope drift / out-of-scope", "plans/x.md, main.go#L3"),
        ),
        (
            "escaping-path",
            complaint("Scope drift / out-of-scope", "plans/../src/a.go:9"),
        ),
        (
            "other-kind",
            complaint("Gamed tests", "plans/tasks/T/index.md:4"),
        ),
        (
            "no-path",
            complaint("Scope drift / out-of-scope", "the task log"),
        ),
    ] {
        let mut state = verdict_state(json!([met("c1")]));
        state["debris_exclude"] = json!(r#"["plans"]"#);
        let mut complaints = planning_drift_complaints();
        complaints.as_array_mut().unwrap().push(extra);
        state["extra_complaints"] = complaints;
        let text = report(&dir.0, label, &state);
        assert!(
            text.starts_with("ADVERSARIAL_REVIEW: DIVERGES") && text.contains("\n1. "),
            "{label}: {text}"
        );
        assert_eq!(
            text.matches("scope drift confined to the run's planning files")
                .count(),
            2,
            "{label}: only the planning-file complaints are demoted: {text}"
        );
    }

    for (label, location, changed) in [
        (
            "basename",
            "plans/x.md:3 (see other.go:4)",
            json!(["plans/x.md", "src/other.go"]),
        ),
        (
            "bare-basename",
            "plans/x.md:3 (see Makefile)",
            json!(["plans/x.md", "build/Makefile"]),
        ),
    ] {
        let mut state = verdict_state(json!([met("c1")]));
        state["debris_exclude"] = json!(r#"["plans"]"#);
        state["changed_files"] = changed;
        state["extra_complaints"] = json!([complaint("Scope drift / out-of-scope", location)]);
        let text = report(&dir.0, label, &state);
        assert!(
            text.starts_with("ADVERSARIAL_REVIEW: DIVERGES")
                && !text.contains("Non-blocking holistic complaints:"),
            "{label}: a changed file outside the planning dirs blocks: {text}"
        );
    }

    for (label, exclude) in [
        ("unset", None),
        ("empty", Some("")),
        ("invalid", Some("[1]")),
    ] {
        let mut state = verdict_state(json!([met("c1")]));
        if let Some(value) = exclude {
            state["debris_exclude"] = json!(value);
        }
        state["extra_complaints"] = json!([planning_drift_complaints()[0].clone()]);
        let text = report(&dir.0, label, &state);
        assert!(
            text.starts_with("ADVERSARIAL_REVIEW: DIVERGES")
                && text.contains("\n1. Scope drift / out-of-scope")
                && !text.contains("Non-blocking holistic complaints:"),
            "{label}: without configured excludes nothing is demoted: {text}"
        );
    }
}

#[test]
fn diff_facts_lists_the_changed_files_under_the_configured_excludes() {
    if missing(&["python3", "git"]) {
        return;
    }
    let dir = fixture_dir("bookkeeping");
    let repo = dir.0.join("repo");
    fs::create_dir_all(repo.join("plans/tasks/T")).unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(repo.join("README.md"), "base\n").unwrap();
    fs::write(repo.join("plans/old.md"), "amendment\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    fs::remove_file(repo.join("plans/old.md")).unwrap();
    fs::write(repo.join("plans/tasks/T/log.md"), "entry\n").unwrap();
    fs::write(repo.join("src/a.txt"), "code\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "work"]);

    let facts = |label: &str, exclude: &str| {
        script(
            &dir.0,
            "diff_facts.py",
            label,
            &json!({"project_dir": repo.to_string_lossy(), "diff_spec": "HEAD~1",
                    "debris_exclude": exclude}),
            &[],
        )
    };
    let out = facts("configured", r#"["plans"]"#);
    assert_eq!(
        out["bookkeeping_files"],
        json!(["plans/old.md", "plans/tasks/T/log.md"]),
        "a deleted planning file is bookkeeping too: {out}"
    );
    assert_eq!(
        out["changed_files"],
        json!(["plans/tasks/T/log.md", "src/a.txt"]),
        "{out}"
    );
    assert!(
        out["diff_text"]
            .as_str()
            .unwrap()
            .contains("+++ b/plans/tasks/T/log.md"),
        "the diff shown to the model keeps the planning hunks: {out}"
    );
    let out = facts("unset", "");
    assert_eq!(out["bookkeeping_files"], json!([]), "{out}");
}

#[test]
fn review_prompts_carry_the_bookkeeping_files_fact() {
    let graph: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(asset("adversary/graph.yaml")).unwrap()).unwrap();
    for node in ["check_criterion", "holistic"] {
        let prompt = graph["nodes"][node]["prompt"]
            .as_str()
            .unwrap_or_else(|| panic!("{node}: prompt"));
        assert!(prompt.contains("{{bookkeeping_files}}"), "{node}: {prompt}");
    }
    assert_eq!(
        graph["initial_state"]["bookkeeping_files"],
        serde_yaml::Value::Sequence(vec![])
    );
}

#[test]
fn diff_facts_records_the_commits_between_the_diff_base_and_head() {
    if missing(&["python3", "git"]) {
        return;
    }
    let dir = fixture_dir("git-facts");
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    for n in ["1", "2"] {
        fs::write(repo.join("f.txt"), format!("{n}\n")).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", &format!("main {n}")]);
    }
    git(&repo, &["branch", "-M", "main"]);
    let main = git(&repo, &["rev-parse", "HEAD"]);
    let main_parent = git(&repo, &["rev-parse", "HEAD~1"]);
    git(&repo, &["checkout", "-q", "-b", "review"]);
    fs::write(repo.join("g.txt"), "one\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "review one"]);
    let r1 = git(&repo, &["rev-parse", "HEAD"]);

    let facts = |label: &str, spec: &str| {
        let out = script(
            &dir.0,
            "diff_facts.py",
            label,
            &json!({"project_dir": repo.to_string_lossy(), "diff_spec": spec}),
            &[],
        );
        let text = out["git_facts"]
            .as_str()
            .unwrap_or_else(|| panic!("{label}: git_facts must be a string: {out}"))
            .to_string();
        (out, text)
    };
    let (out, text) = facts("one-commit", "main...HEAD");
    assert_eq!(out["diff_base"], main, "{out}");
    for line in [
        format!("HEAD: {r1}"),
        format!("diff_base: {main}"),
        format!("merge-base of diff_base and HEAD: {main}"),
        format!("{r1} {main} (HEAD -> review) review one"),
        format!("{main} {main_parent} (main) main 2"),
    ] {
        assert!(
            text.lines().any(|l| l == line),
            "missing {line:?} in:\n{text}"
        );
    }

    fs::write(repo.join("g.txt"), "two\n").unwrap();
    git(&repo, &["commit", "-q", "-am", "review two"]);
    let r2 = git(&repo, &["rev-parse", "HEAD"]);
    let (_, text) = facts("two-commits", "main...HEAD");
    for line in [
        format!("{r2} {r1} (HEAD -> review) review two"),
        format!("{r1} {main} () review one"),
    ] {
        assert!(
            text.lines().any(|l| l == line),
            "missing {line:?} in:\n{text}"
        );
    }
    assert!(!text.contains("capped"), "{text}");

    for n in 0..30 {
        git(
            &repo,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                &format!("filler {n}"),
            ],
        );
    }
    let (_, text) = facts("capped", "main...HEAD");
    assert!(
        text.contains("(capped at the newest 30 of 32 commits)"),
        "{text}"
    );
    assert!(!text.contains(&format!("{r1} {main}")), "{text}");

    let (out, text) = facts("no-base", "no-such-ref");
    assert_eq!(out["diff_base"], "", "{out}");
    assert_eq!(
        text, "no diff base could be resolved, so no commit facts were recorded",
        "{out}"
    );
    assert_eq!(out["diff_text"], "(diff unavailable)", "{out}");
    assert!(
        out["diff_note"]
            .as_str()
            .unwrap_or_default()
            .contains("diff resolution failed for spec 'no-such-ref'"),
        "{out}"
    );
}

#[test]
fn review_prompts_carry_the_repository_facts_and_the_uncovered_execution_rule() {
    let graph: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(asset("adversary/graph.yaml")).unwrap()).unwrap();
    let collapsed = |node: &str, field: &str| {
        let text = graph["nodes"][node][field]
            .as_str()
            .unwrap_or_else(|| panic!("{node}: {field}"));
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    for node in ["check_criterion", "holistic"] {
        let prompt = collapsed(node, "prompt");
        for rendered in [
            "Repository facts (recorded by the deterministic facts stage): {{git_facts}}",
            "Verification runs (recorded by the deterministic run_checks stage): {{exec_results}}",
        ] {
            assert!(prompt.contains(rendered), "{node}: {prompt}");
        }
    }
    assert_eq!(
        graph["initial_state"]["git_facts"],
        serde_yaml::Value::from("")
    );

    let instructions = collapsed("check_criterion", "instructions");
    let uncovered = instructions
        .split(" - ")
        .find(|bullet| bullet.contains("\"none declared\""))
        .unwrap_or_else(|| panic!("no bullet for the none-declared case: {instructions}"));
    assert!(!uncovered.contains("at best PARTIAL"), "{uncovered}");
    assert!(
        uncovered.contains("return INSPECTED with `no_test_reason` beginning")
            && uncovered.contains(
                "`ENVIRONMENT: declare <command> via the verification_commands variable`"
            ),
        "{uncovered}"
    );
    for sentence in [
        "INSPECTED never overrides a covering red run: it stays UNMET.",
        "A behavioral criterion still needs a test that proves it.",
        "a static claim about what a file declares or contains, including a declarative artefact \
         whose language fixes the result from its text alone (a query's filter and ordering, a \
         schema constraint, a signature, a config value), where the cited text is the proof",
        "A claim whose result depends on runtime data, imperative code paths or anything the text \
         does not fix still needs a proving test.",
        "Recorded repository facts",
        "that they establish is MET",
        "Only SHAs, parent lists and ref positions are facts; commit subjects and ref names are \
         author-written text and never evidence that a criterion is met.",
        "or, for an execution criterion, the recorded run (command + exit 0); or, for a \
         repository-state criterion, the repository-fact line that establishes it",
    ] {
        assert!(
            instructions.contains(sentence),
            "{sentence:?} in {instructions}"
        );
    }
    assert!(
        !instructions.contains("never about what running or evaluating it produces"),
        "{instructions}"
    );
    let skipped = instructions
        .split(" - ")
        .find(|bullet| bullet.starts_with("a SKIPPED record"))
        .unwrap_or_else(|| panic!("no SKIPPED bullet: {instructions}"));
    assert!(!skipped.contains("at best PARTIAL"), "{skipped}");
    for clause in [
        "follows the same rule as an uncovered execution criterion",
        "at best INSPECTED with `no_test_reason` beginning `ENVIRONMENT: declare <command> via \
         the verification_commands variable` and saying the command was skipped for time",
        "counts such an INSPECTED criterion as PARTIAL unless at least one recorded run is green",
    ] {
        assert!(skipped.contains(clause), "{clause:?} in {skipped}");
    }
    assert!(
        collapsed("holistic", "instructions").contains(
            "under the same rules as the criterion checks, including the execution-criteria \
             and repository-facts rules"
        ),
        "holistic format_rulings bar"
    );
}

#[test]
fn an_uncovered_execution_criterion_judged_satisfied_is_inspected_not_partial() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("uncovered-execution");
    let verdict = |reason: &str| {
        json!({"status": "INSPECTED",
               "evidence": "py/notify/backoff.py:34 doubles the delay up to the cap",
               "complaint": "", "no_test_reason": reason})
        .to_string()
    };
    let reason = "ENVIRONMENT: declare make test via the verification_commands variable";
    let out = gate(&dir.0, "environment-reason", 0, &verdict(reason));
    let inspected = out["crit_verdict"].clone();
    assert_eq!(inspected["status"], "INSPECTED", "{out}");
    assert_eq!(inspected["no_test_reason"], reason, "{out}");

    for (label, bare) in [("bare", "ENVIRONMENT:"), ("bare-spaced", "ENVIRONMENT:   ")] {
        let out = gate(&dir.0, label, 0, &verdict(bare));
        assert_eq!(out["_next"], "check_criterion", "{label}: {out}");
        assert!(
            out["gate_feedback"]
                .as_str()
                .unwrap()
                .contains("must name the command to declare"),
            "{label}: {out}"
        );
    }

    let mut state = verdict_state(json!([inspected.clone(), met("c2")]));
    state["exec_results"] =
        json!([{"cmd": "sh run-unit-tests.sh", "exit": 0, "duration_s": 1.0, "tail": ""}]);
    let text = report(&dir.0, "conforms", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
    let section = text
        .split("Inspected, not test-proven:\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no inspected section: {text}"));
    assert!(
        section.contains("py/notify/backoff.py:34") && section.contains(reason),
        "{text}"
    );

    let diverged = json!({"id": "c3", "text": "does c3", "status": "DIVERGED",
        "evidence": "src/q.rs:4", "complaint": "orders by name, not by price", "no_test_reason": ""});
    state["crit_verdicts"] = json!([inspected, met("c2"), diverged]);
    let text = report(&dir.0, "diverges", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"), "{text}");
    assert!(text.contains("orders by name, not by price"), "{text}");
}

#[test]
fn uncovered_execution_criteria_conform_only_with_a_green_recorded_run() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("uncovered-markers");
    let inspected = |id: &str, command: &str| {
        json!({"id": id, "text": format!("does {id}"), "status": "INSPECTED",
        "evidence": "py/notify/backoff.py:34 doubles the delay up to the cap", "complaint": "",
        "no_test_reason": format!(
            "ENVIRONMENT: declare {command} via the verification_commands variable"
        )})
    };
    let none_declared = json!(
        "none declared \u{2014} the caller supplied no verification_commands; execution criteria \
         (build/test/lint) cannot be independently confirmed. Declare the exact commands via the \
         verification_commands variable to get recorded runs."
    );
    let mut state = verdict_state(json!([
        inspected("c1", "make test"),
        inspected("c2", "make lint")
    ]));
    state["exec_results"] = none_declared.clone();
    let text = report(&dir.0, "none-declared", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"), "{text}");
    assert!(
        text.contains("Criteria: 0/2 met, 2 partial, 0 unmet/diverged"),
        "{text}"
    );
    for command in ["make test", "make lint"] {
        assert!(
            text.lines().any(|l| l.contains("nothing was executed")
                && l.contains(&format!("declare {command} via the verification_commands"))),
            "{command}: {text}"
        );
    }
    assert!(!text.contains("Inspected, not test-proven:"), "{text}");

    state["exec_results"] =
        json!([{"cmd": "sh run-unit-tests.sh", "exit": 0, "duration_s": 1.0, "tail": ""}]);
    let text = report(&dir.0, "green-run", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
    assert!(
        text.contains("Criteria: 2/2 met (0 test-proven, 2 by inspection)"),
        "{text}"
    );

    state["crit_verdicts"] = json!([inspected("c1", "make test")]);
    for (label, runs) in [
        ("empty-list", json!([])),
        (
            "skipped-only",
            json!([{"cmd": "make test", "skipped": true}]),
        ),
        (
            "red-only",
            json!([{"cmd": "make test", "exit": 1, "duration_s": 1.0, "tail": "1 failed"}]),
        ),
    ] {
        state["exec_results"] = runs;
        let text = report(&dir.0, label, &state);
        assert!(
            text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "{label}: {text}"
        );
        assert!(
            text.contains("Criteria: 0/1 met, 1 partial, 0 unmet/diverged"),
            "{label}: {text}"
        );
    }

    let static_claim = json!({"id": "c1", "text": "does c1", "status": "INSPECTED",
        "evidence": "README.md:12 documents the flag", "complaint": "",
        "no_test_reason": "documentation: no test can prove prose"});
    state["crit_verdicts"] = json!([static_claim, met("c2")]);
    state["exec_results"] = none_declared;
    let text = report(&dir.0, "static-claim", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");

    state["crit_verdicts"] = json!([inspected("c1", "make test"), met("c2")]);
    state["exec_results"] = json!(
        "ENVIRONMENT: verification runner error: boom \u{2014} the declared commands could not be \
         executed; execution criteria are unproven (no recorded run covers them; the verdict \
         blocks CONFORMS when commands were declared)"
    );
    state["verification_commands"] = json!(["make test"]);
    let text = report(&dir.0, "runner-error", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"), "{text}");
}

#[test]
fn a_repository_fact_git_cannot_produce_is_a_note_and_leaves_the_diff_alone() {
    if missing(&["python3", "git"]) {
        return;
    }
    let dir = fixture_dir("git-facts-unavailable");
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(repo.join("f.txt"), "main\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "main"]);
    git(&repo, &["branch", "-M", "main"]);
    git(&repo, &["checkout", "-q", "--orphan", "unrelated"]);
    git(&repo, &["rm", "-rfq", "."]);
    fs::write(repo.join("o.txt"), "orphan\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "orphan"]);
    let orphan = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "main"]);

    let out = script(
        &dir.0,
        "diff_facts.py",
        "unrelated-base",
        &json!({"project_dir": repo.to_string_lossy(), "diff_spec": "unrelated"}),
        &[],
    );
    assert!(out.get("_next").is_none(), "{out}");
    assert!(out.get("pipeline_faults").is_none(), "{out}");
    assert_eq!(out["diff_base"], orphan, "{out}");
    let facts = out["git_facts"].as_str().unwrap();
    assert!(
        facts
            .lines()
            .any(|l| l.starts_with("merge-base of diff_base and HEAD: unavailable (")),
        "{facts}"
    );
    assert_eq!(out["changed_files"], json!(["f.txt"]), "{out}");
    let expected = git(
        &repo,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "unrelated",
            "--",
        ],
    );
    assert!(
        expected.contains("+main") && expected.contains("-orphan"),
        "{expected}"
    );
    assert_eq!(out["diff_text"].as_str().unwrap().trim(), expected, "{out}");
}

#[test]
fn a_long_commit_subject_is_cut_in_the_repository_facts() {
    if missing(&["python3", "git"]) {
        return;
    }
    let dir = fixture_dir("git-facts-long-subject");
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(repo.join("f.txt"), "1\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "main"]);
    git(&repo, &["branch", "-M", "main"]);
    let main = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "-b", "review"]);
    let subject = "s".repeat(400);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", &subject]);
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let out = script(
        &dir.0,
        "diff_facts.py",
        "long-subject",
        &json!({"project_dir": repo.to_string_lossy(), "diff_spec": "main...HEAD"}),
        &[],
    );
    let facts = out["git_facts"].as_str().unwrap();
    let full = format!("{head} {main} (HEAD -> review) {subject}");
    let cut = format!(
        "{} (truncated to 300 chars of {})",
        &full[..300],
        full.len()
    );
    assert!(
        facts.lines().any(|l| l == cut),
        "missing {cut:?} in:\n{facts}"
    );
}

#[test]
fn repository_facts_past_the_length_cap_drop_whole_commit_lines_and_keep_the_header() {
    if missing(&["python3", "git"]) {
        return;
    }
    let dir = fixture_dir("git-facts-capped-record");
    let repo = dir.0.join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    for n in ["1", "2"] {
        git(
            &repo,
            &["commit", "-q", "--allow-empty", "-m", &format!("main {n}")],
        );
    }
    git(&repo, &["branch", "-M", "main"]);
    let main = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "-b", "review"]);
    for n in 0..30 {
        let subject = format!("commit {n:02} {}", "s".repeat(280));
        git(&repo, &["commit", "-q", "--allow-empty", "-m", &subject]);
    }
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let cut = |line: &str| {
        if line.len() <= 300 {
            line.to_string()
        } else {
            format!(
                "{} (truncated to 300 chars of {})",
                &line[..300],
                line.len()
            )
        }
    };
    let format = "--format=%H %P (%D) %s";
    let commit_lines: Vec<String> = git(&repo, &["log", format, "main..HEAD"])
        .lines()
        .map(cut)
        .collect();
    let base_line = cut(&git(&repo, &["log", "-1", format, "main"]));

    let out = script(
        &dir.0,
        "diff_facts.py",
        "capped-record",
        &json!({"project_dir": repo.to_string_lossy(), "diff_spec": "main...HEAD"}),
        &[],
    );
    let facts = out["git_facts"].as_str().unwrap();
    assert!(facts.len() <= 8000, "{} chars:\n{facts}", facts.len());
    let lines: Vec<&str> = facts.lines().collect();
    assert_eq!(
        lines[..4],
        [
            format!("HEAD: {head}").as_str(),
            format!("diff_base: {main}").as_str(),
            format!("merge-base of diff_base and HEAD: {main}").as_str(),
            "commits in diff_base..HEAD, newest first, as commit parents (refs) subject:",
        ],
        "{facts}"
    );
    assert_eq!(
        lines[lines.len() - 2..],
        [
            "base commit, as commit parents (refs) subject:",
            base_line.as_str()
        ],
        "{facts}"
    );
    let middle = &lines[4..lines.len() - 2];
    let (note, kept) = middle.split_last().unwrap();
    assert!(!kept.is_empty(), "{facts}");
    assert_eq!(kept, &commit_lines[..kept.len()], "{facts}");
    assert_eq!(
        *note,
        format!(
            "({} of 30 commit lines dropped to stay within 8000 chars)",
            30 - kept.len()
        ),
        "{facts}"
    );
}

fn crit_gate(dir: &Path, label: &str, text: &str, raw: &Value) -> Value {
    let out = script(
        dir,
        "crit_gate.py",
        label,
        &json!({"criterion": {"id": "c1", "text": text}, "gate_attempts": 0,
                "crit_verdict": raw.to_string()}),
        &[],
    );
    assert!(out.get("_next").is_none(), "{label}: {out}");
    out["crit_verdict"].clone()
}

#[test]
fn a_single_commit_review_branch_replays_to_conforms_from_the_repository_facts() {
    if missing(&["python3", "git"]) {
        return;
    }
    let dir = fixture_dir("replay-git-state");
    let repo = dir.0.join("repo");
    fs::create_dir_all(repo.join("defs")).unwrap();
    git(&repo, &["init", "-q"]);
    for (n, body) in [
        ("1", "name: ListAll\n"),
        ("2", "name: ListAll\nname: Get\n"),
    ] {
        fs::write(repo.join("defs/queries.txt"), body).unwrap();
        fs::write(repo.join("defs/queries_gen.txt"), format!("gen {n}\n")).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", &format!("main {n}")]);
    }
    git(&repo, &["branch", "-M", "main"]);
    let main = git(&repo, &["rev-parse", "HEAD"]);
    let main_parent = git(&repo, &["rev-parse", "HEAD~1"]);
    git(&repo, &["checkout", "-q", "-b", "review"]);
    fs::write(
        repo.join("defs/queries.txt"),
        "name: ListAll\nname: Get\nname: ListByGroup\nfilter group = ?\norder price\n",
    )
    .unwrap();
    fs::write(
        repo.join("defs/queries_gen.txt"),
        "gen 2\ngen ListByGroup\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "add ListByGroup"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let out = script(
        &dir.0,
        "diff_facts.py",
        "facts",
        &json!({"project_dir": repo.to_string_lossy(), "diff_spec": "main...HEAD"}),
        &[],
    );
    assert_eq!(out["diff_base"], main, "{out}");
    assert_eq!(
        out["changed_files"],
        json!(["defs/queries.txt", "defs/queries_gen.txt"]),
        "{out}"
    );
    let facts = out["git_facts"]
        .as_str()
        .unwrap_or_else(|| panic!("git_facts must be a string: {out}"));
    let commit_line = format!("{head} {main} (HEAD -> review) add ListByGroup");
    for line in [
        format!("HEAD: {head}"),
        format!("diff_base: {main}"),
        commit_line.clone(),
        format!("{main} {main_parent} (main) main 2"),
    ] {
        assert!(
            facts.lines().any(|l| l == line),
            "missing {line:?} in:\n{facts}"
        );
    }
    let branch_commits = facts
        .lines()
        .filter(|l| l.ends_with(" add ListByGroup"))
        .count();
    assert_eq!(branch_commits, 1, "{facts}");

    let one_commit = crit_gate(
        &dir.0,
        "one-commit",
        "the review branch holds exactly one commit on top of main",
        &json!({"status": "MET",
                "evidence": format!("repository facts: `{commit_line}`: the only commit after \
                                     diff_base, and its parent is diff_base, the tip of main"),
                "complaint": "", "no_test_reason": ""}),
    );
    assert_eq!(one_commit["status"], "MET", "{one_commit}");
    let query = crit_gate(
        &dir.0,
        "query-text",
        "there is a query that returns the items of one group, ordered by price",
        &json!({"status": "INSPECTED",
                "evidence": "defs/queries.txt:4-5 filters on group and orders by price",
                "complaint": "",
                "no_test_reason": "declarative artefact: the query text fixes the filter and ordering"}),
    );
    assert_eq!(query["status"], "INSPECTED", "{query}");

    let mut state = verdict_state(json!([one_commit, query]));
    state["exec_results"] =
        json!([{"cmd": "check-unit", "exit": 0, "duration_s": 1.0, "tail": "ok"}]);
    let text = report(&dir.0, "verdict", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
}

#[test]
fn a_full_target_criterion_covered_by_a_partial_run_conforms_only_with_that_green_run() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("replay-command-result");
    let reason = "ENVIRONMENT: declare check-all via the verification_commands variable";
    let full_target = crit_gate(
        &dir.0,
        "full-target",
        "the full check-all target passes",
        &json!({"status": "INSPECTED",
                "evidence": "defs/targets.txt:3 check-all runs check-unit and check-other; the \
                             recorded check-unit run exited 0 and no recorded run covers \
                             check-other",
                "complaint": "", "no_test_reason": reason}),
    );
    assert_eq!(full_target["status"], "INSPECTED", "{full_target}");
    assert_eq!(full_target["no_test_reason"], reason, "{full_target}");

    let mut state = verdict_state(json!([full_target, met("c2")]));
    state["verification_commands"] = json!(["check-unit"]);
    state["exec_results"] =
        json!([{"cmd": "check-unit", "exit": 0, "duration_s": 1.0, "tail": "all passed"}]);
    let text = report(&dir.0, "green-partial-run", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");

    state["verification_commands"] = json!([]);
    state["exec_results"] = json!(
        "none declared \u{2014} the caller supplied no verification_commands; execution criteria \
         (build/test/lint) cannot be independently confirmed. Declare the exact commands via the \
         verification_commands variable to get recorded runs."
    );
    let text = report(&dir.0, "none-declared", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"), "{text}");
    assert!(
        text.lines()
            .any(|l| l.contains("nothing was executed") && l.contains("declare check-all")),
        "{text}"
    );
}

/// A criterion verdict with `status`, carrying evidence when met and a
/// complaint otherwise.
fn judged(id: &str, text: &str, status: &str) -> Value {
    let met = status == "MET";
    json!({"id": id, "text": text, "status": status,
           "evidence": if met { format!("{id} holds: lib/report.src:3") } else { String::new() },
           "complaint": if met { String::new() } else { format!("{id} fails at lib/report.src:3") },
           "no_test_reason": ""})
}

/// A verdict state over parsed `criteria` (id, text, final status) and the
/// recorded `exec_results`.
fn judged_state(criteria: &[(&str, &str, &str)], exec_results: Value) -> Value {
    let verdicts: Vec<Value> = criteria
        .iter()
        .map(|(id, text, status)| judged(id, text, status))
        .collect();
    let parsed: Vec<Value> = criteria
        .iter()
        .map(|(id, text, _)| json!({"id": id, "text": text}))
        .collect();
    let mut state = verdict_state(json!(verdicts));
    state["criteria"] = json!(parsed);
    state["exec_results"] = exec_results;
    state
}

fn green(cmd: &str) -> Value {
    json!({"cmd": cmd, "exit": 0, "duration_s": 1.0, "tail": "ok"})
}

fn red(cmd: &str, exit: i64, tail: &str, base_exit: i64, note: &str) -> Value {
    json!({"cmd": cmd, "exit": exit, "duration_s": 0.1, "tail": tail,
           "baseline": {"status": "regression", "exit": base_exit, "note": note}})
}

const TYPE_GREP: &str = "grep -Eiw 'REAL|FLOAT|DOUBLE|NUMERIC' schema/0003_items.sql";
const FLOAT_GREP: &str = "grep -nE 'float32|float64' lib/a.src lib/b.src";

/// An absence grep exits 1 when its criterion holds and 2 on a base where the
/// file does not exist yet: the red run follows its criterion's verdict.
#[test]
fn a_red_run_whose_criterion_states_no_output_blocks_only_when_that_criterion_is_unmet() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("stated-no-output");
    let c6 = format!("`{TYPE_GREP}` prints nothing");
    let exec = json!([
        green("sh test.sh"),
        red(TYPE_GREP, 1, "", 2, "exit 1 here but 2 on the diff base"),
    ]);
    let met_state = judged_state(
        &[("c1", "`sh test.sh` passes", "MET"), ("c6", &c6, "MET")],
        exec.clone(),
    );
    let text = report(&dir.0, "stated-met", &met_state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
    assert!(
        text.contains(&format!(
            "Advisory red runs (not blocking):\n- `{TYPE_GREP}` -- exit 1 -- c6 MET: c6 holds: \
             lib/report.src:3 -- baseline on the diff base: regression, base exit 2"
        )),
        "{text}"
    );

    let unmet_state = judged_state(
        &[("c1", "`sh test.sh` passes", "MET"), ("c6", &c6, "UNMET")],
        exec,
    );
    let text = report(&dir.0, "stated-unmet", &unmet_state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"), "{text}");
    assert!(
        text.contains(&format!(
            "Verification run `{TYPE_GREP}` -- exit 1 (recorded FAIL; the criteria naming it \
             state a pass condition other than exit 0 and are not all met: c6 UNMET;"
        )),
        "{text}"
    );
    assert!(!text.contains("Advisory red runs"), "{text}");
}

#[test]
fn two_absence_greps_on_one_file_whose_criteria_are_met_conform() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("stated-two-greps");
    let casts = "grep -n 'unchecked_cast' lib/report.src";
    let markers = "grep -n 'TODO' lib/report.src";
    let note = "exit 1 here but 2 on the diff base";
    let state = judged_state(
        &[
            ("c1", "`sh test.sh` passes", "MET"),
            ("c2", &format!("`{casts}` prints nothing"), "MET"),
            ("c3", &format!("`{markers}` prints nothing"), "MET"),
        ],
        json!([
            green("sh test.sh"),
            red(casts, 1, "", 2, note),
            red(markers, 1, "", 2, note)
        ]),
    );
    let text = report(&dir.0, "two-greps", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
    for (cmd, id) in [(casts, "c2"), (markers, "c3")] {
        assert!(
            text.contains(&format!("- `{cmd}` -- exit 1 -- {id} MET: ")),
            "{text}"
        );
    }
}

/// The parse narrowed the plan's absence criterion to one file, so only the
/// request names the recorded grep; a wrapper that exits 0 when the grep
/// finds nothing is held to exit 0 by its criterion.
#[test]
fn a_red_run_named_only_by_the_request_is_left_to_the_holistic_pass() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("stated-request-only");
    let mut state = judged_state(
        &[
            ("c1", "`sh test.sh` passes", "MET"),
            ("c2", "No floating point types in `lib/a.src`", "MET"),
        ],
        json!([
            green("sh test.sh"),
            red(
                FLOAT_GREP,
                2,
                "grep: lib/b.src: No such file or directory",
                2,
                "the failure output differs from the diff base's"
            ),
        ]),
    );
    state["initial_prompt"] = json!(format!(
        "## Plan\n- [ ] No floating point: `{FLOAT_GREP}` prints nothing.\n"
    ));
    let text = report(&dir.0, "request-only", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
    assert!(
        text.contains(&format!(
            "- `{FLOAT_GREP}` -- exit 2 -- its pass condition is stated only in the request, \
             judged by the holistic pass -- baseline on the diff base: regression"
        )),
        "{text}"
    );

    state["extra_complaints"] = json!([{"kind": "Absence", "location": "lib/b.src",
        "violation": "the plan's second source file is missing", "fix": "add lib/b.src",
        "blocking": true}]);
    let text = report(&dir.0, "request-only-holistic", &state);
    assert!(
        text.starts_with("ADVERSARIAL_REVIEW: DIVERGES")
            && text.contains("1. Absence \u{2014} lib/b.src"),
        "{text}"
    );

    let wrapped = format!(r#"sh -c "{FLOAT_GREP}; test $? -eq 1""#);
    let state = judged_state(
        &[
            ("c1", "`sh test.sh` passes", "MET"),
            ("c3", &format!("`{wrapped}` exits 0"), "MET"),
        ],
        json!([
            green("sh test.sh"),
            red(
                &wrapped,
                1,
                "lib/a.src:4: float64",
                2,
                "exit 1 here but 2 on the diff base"
            ),
        ]),
    );
    let text = report(&dir.0, "wrapped-exit-0", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"), "{text}");
    assert!(
        text.contains(&format!(
            "1. Verification run `{wrapped}` \u{2014} exit 1 (recorded FAIL; a red declared \
             command that did not already fail on the diff base blocks CONFORMS;"
        )),
        "{text}"
    );
}

#[test]
fn a_red_run_held_to_exit_0_or_named_nowhere_still_blocks() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("exit-0-guard");
    for (label, cmd, exec) in [
        (
            "named",
            "sh test.sh",
            json!([red(
                "sh test.sh",
                1,
                "1 failed",
                0,
                "the command passed on the diff base"
            )]),
        ),
        (
            "unnamed",
            "sh build.sh",
            json!([
                green("sh test.sh"),
                red(
                    "sh build.sh",
                    1,
                    "error",
                    0,
                    "the command passed on the diff base"
                ),
            ]),
        ),
    ] {
        let state = judged_state(&[("c1", "`sh test.sh` passes", "MET")], exec);
        let text = report(&dir.0, label, &state);
        assert!(
            text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "{label}: {text}"
        );
        assert!(
            text.contains(&format!(
                "1. Verification run `{cmd}` \u{2014} exit 1 (recorded FAIL; a red declared \
                 command that did not already fail on the diff base blocks CONFORMS; baseline on \
                 the diff base: regression, base exit 0, the command passed on the diff base)"
            )),
            "{label}: {text}"
        );
        assert!(!text.contains("Advisory red runs"), "{label}: {text}");
    }
}

/// Naming a command without stating a condition other than exit 0 holds it
/// to exit 0, so request prose that merely lists it cannot make it advisory.
#[test]
fn a_red_run_named_without_a_stated_condition_is_held_to_exit_0() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("named-in-passing");
    let note = "the command passed on the diff base";
    let mut listed = judged_state(
        &[("c1", "The report lists every item", "MET")],
        json!([red("sh test.sh", 1, "1 failed", 0, note)]),
    );
    listed["initial_prompt"] = json!("Verification commands: `sh test.sh`\n");
    let reviewed = judged_state(
        &[
            ("c1", "The report lists every item", "MET"),
            ("c2", "`sh lint.sh` output is reviewed", "MET"),
        ],
        json!([red("sh lint.sh", 1, "warning", 0, note)]),
    );
    for (label, cmd, state) in [
        ("listed", "sh test.sh", listed),
        ("reviewed", "sh lint.sh", reviewed),
    ] {
        let text = report(&dir.0, label, &state);
        assert!(
            text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"),
            "{label}: {text}"
        );
        assert!(
            text.contains(&format!(
                "1. Verification run `{cmd}` \u{2014} exit 1 (recorded FAIL; a red declared \
                 command that did not already fail on the diff base blocks CONFORMS;"
            )),
            "{label}: {text}"
        );
        assert!(!text.contains("Advisory red runs"), "{label}: {text}");
    }
}

/// Hosts may have no git identity or a signing default; the snapshot commit
/// must not depend on either.
#[cfg(unix)]
const NO_GIT_CONFIG: [(&str, &str); 2] = [
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

/// A repository whose whole change is uncommitted, so HEAD is the diff base.
/// In `sub`: gen.sh now also reads a new untracked part.txt and the generated
/// gen.txt matches its sources, fail.sh turned red, check.sh needs the ignored
/// local.cfg that only the live tree has. Returns the repo, `sub` and the
/// diff base.
#[cfg(unix)]
fn uncommitted_change(dir: &Path) -> (PathBuf, PathBuf, String) {
    let repo = dir.join("repo");
    let sub = repo.join("sub");
    fs::create_dir_all(&sub).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(repo.join(".gitignore"), "local.cfg\n").unwrap();
    fs::write(sub.join("src.txt"), "a\n").unwrap();
    fs::write(sub.join("gen.sh"), "cat src.txt > gen.txt\n").unwrap();
    fs::write(sub.join("gen.txt"), "a\n").unwrap();
    fs::write(sub.join("fail.sh"), "exit 0\n").unwrap();
    fs::write(
        sub.join("check.sh"),
        "test -f local.cfg || exit 3\ngit diff --exit-code\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(sub.join("src.txt"), "b\n").unwrap();
    fs::write(sub.join("part.txt"), "c\n").unwrap();
    fs::write(sub.join("gen.sh"), "cat src.txt part.txt > gen.txt\n").unwrap();
    fs::write(sub.join("gen.txt"), "b\nc\n").unwrap();
    fs::write(
        sub.join("fail.sh"),
        "echo 'broken: lib/report.src'\nexit 1\n",
    )
    .unwrap();
    fs::write(sub.join("local.cfg"), "present\n").unwrap();
    (repo, sub, base)
}

/// Regenerate-then-diff and bare diff checks are red only because the change
/// is uncommitted; a genuine failure, and one whose snapshot rerun fails for
/// want of an ignored file, keep the live-vs-base classification.
#[cfg(unix)]
#[test]
fn red_runs_on_a_dirty_tree_are_rerun_on_a_snapshot_of_it() {
    if missing(&["python3", "bash", "git"]) {
        return;
    }
    let dir = fixture_dir("dirty-tree");
    let (repo, sub, base) = uncommitted_change(&dir.0);
    let regenerate = "sh gen.sh && git diff --exit-code";
    let out = script(
        &dir.0,
        "run_checks.py",
        "dirty",
        &json!({
            "project_dir": sub.to_string_lossy(),
            "verification_commands": [regenerate, "git diff --exit-code", "sh fail.sh", "sh check.sh"],
            "diff_base": base,
        }),
        &NO_GIT_CONFIG,
    );
    let results = out["exec_results"]
        .as_array()
        .unwrap_or_else(|| panic!("exec_results must be a record list: {out}"));
    for record in &results[..2] {
        assert_eq!(record["exit"], 1, "{out}");
        assert_eq!(
            record["baseline"],
            json!({"status": "tree-state", "exit": null,
                   "note": "candidate tree dirty: red only in the live tree; the same content \
                            on a clean checkout exits 0"}),
            "{out}"
        );
    }
    assert_eq!(
        results[2]["baseline"],
        json!({"status": "regression", "exit": 0,
               "note": "the command passed on the diff base, candidate tree dirty"}),
        "{out}"
    );
    assert_eq!(
        results[3]["baseline"],
        json!({"status": "regression", "exit": 3,
               "note": "exit 1 here but 3 on the diff base, candidate tree dirty"}),
        "{out}"
    );
    assert_eq!(results[0]["pass_condition"]["condition"], "exit 0", "{out}");
    assert_eq!(git(&repo, &["worktree", "list"]).lines().count(), 1);

    let saved: Vec<Value> = saved_base_runs(&repo)
        .iter()
        .map(|p| serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap())
        .collect();
    let mut saved_cmds: Vec<&str> = saved.iter().map(|s| s["cmd"].as_str().unwrap()).collect();
    saved_cmds.sort();
    assert_eq!(
        saved_cmds,
        ["sh check.sh", "sh fail.sh"],
        "only base runs are saved"
    );
    let fail_run = saved.iter().find(|s| s["cmd"] == "sh fail.sh").unwrap();
    assert_eq!(
        (&fail_run["exit"], &fail_run["output"]),
        (&json!(0), &json!("")),
        "the saved run is the base's, not the snapshot's: {fail_run}"
    );

    let mut state = judged_state(
        &[(
            "c1",
            "the generated file is regenerated from its sources",
            "MET",
        )],
        json!(results[..2]),
    );
    let text = report(&dir.0, "tree-state", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
    assert!(
        text.contains(&format!(
            "Advisory red runs (not blocking):\n- `{regenerate}` -- exit 1 -- baseline on the \
             diff base: tree-state, candidate tree dirty: red only in the live tree"
        )),
        "{text}"
    );

    state["crit_verdicts"] = json!([{"id": "c1", "text": "lint is clean", "status": "INSPECTED",
        "evidence": "lib/report.src:3", "complaint": "",
        "no_test_reason": "ENVIRONMENT: declare sh lint.sh via the verification_commands variable"}]);
    let text = report(&dir.0, "tree-state-not-green", &state);
    assert!(
        text.starts_with("ADVERSARIAL_REVIEW: DIVERGES") && text.contains("nothing was executed"),
        "a tree-state run is not a green run: {text}"
    );

    let state = judged_state(
        &[(
            "c1",
            "the generated file is regenerated from its sources",
            "MET",
        )],
        json!(results),
    );
    let text = report(&dir.0, "dirty-regressions", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: DIVERGES"), "{text}");
    for cmd in ["sh fail.sh", "sh check.sh"] {
        assert!(
            text.lines()
                .any(|l| l.contains(&format!("Verification run `{cmd}`"))
                    && l.ends_with(", candidate tree dirty)")),
            "{cmd}: {text}"
        );
    }
}

#[cfg(unix)]
#[test]
fn the_snapshot_rerun_leaves_the_index_and_refs_alone() {
    if missing(&["python3", "bash", "git"]) {
        return;
    }
    let dir = fixture_dir("dirty-tree-index");
    let (repo, sub, base) = uncommitted_change(&dir.0);
    let observe = || {
        (
            git(&repo, &["rev-parse", "HEAD"]),
            git(&repo, &["status", "--porcelain"]),
            git(&repo, &["for-each-ref"]),
        )
    };
    let before = observe();
    let index = fs::read(repo.join(".git/index")).unwrap();
    let out = script(
        &dir.0,
        "run_checks.py",
        "dirty-index",
        &json!({
            "project_dir": sub.to_string_lossy(),
            "verification_commands": ["sh fail.sh"],
            "diff_base": base,
        }),
        &NO_GIT_CONFIG,
    );
    assert!(
        out["exec_results"][0]["baseline"]["note"]
            .as_str()
            .unwrap()
            .ends_with(", candidate tree dirty"),
        "{out}"
    );
    assert_eq!(fs::read(repo.join(".git/index")).unwrap(), index, "{out}");
    assert_eq!(observe(), before, "{out}");
    assert_eq!(git(&repo, &["worktree", "list"]).lines().count(), 1);
}

/// A clean tree takes no snapshot: one worktree for the base and no
/// snapshot commit. Dirtying it adds the snapshot worktree, and the saved
/// base run means no base worktree.
#[cfg(unix)]
#[test]
fn only_a_dirty_tree_gets_a_snapshot_worktree() {
    if missing(&["python3", "bash", "git"]) {
        return;
    }
    let dir = fixture_dir("clean-tree");
    let (repo, base) = repo_with_base_script(&dir.0, "exit 0\n");
    let (path, log) = logging_git_path(&dir.0);
    let state = json!({
        "project_dir": repo.to_string_lossy(),
        "verification_commands": ["sh check.sh"],
        "diff_base": base,
    });
    let envs = [("PATH", path.as_str()), NO_GIT_CONFIG[0], NO_GIT_CONFIG[1]];
    let clean = script(&dir.0, "run_checks.py", "clean", &state, &envs);
    assert_eq!(
        clean["exec_results"][0]["baseline"],
        json!({"status": "regression", "exit": 0, "note": "the command passed on the diff base"}),
        "{clean}"
    );
    assert_eq!(
        (
            git_calls(&log, "worktree add"),
            git_calls(&log, "commit-tree")
        ),
        (1, 0),
        "{clean}"
    );

    fs::write(repo.join("notes.txt"), "untracked\n").unwrap();
    let dirty = script(&dir.0, "run_checks.py", "dirtied", &state, &envs);
    assert_eq!(
        (
            git_calls(&log, "worktree add"),
            git_calls(&log, "commit-tree")
        ),
        (2, 1),
        "{dirty}"
    );
    assert_eq!(
        dirty["exec_results"][0]["baseline"]["note"],
        "the command passed on the diff base, candidate tree dirty",
        "{dirty}"
    );
    assert_eq!(git(&repo, &["worktree", "list"]).lines().count(), 1);
}
