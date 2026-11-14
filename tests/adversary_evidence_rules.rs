//! Evidence and default rules of the adversary graph's deterministic scripts:
//! criteria met by inspection, red verification runs compared with a
//! baseline run on the diff base, the run_checks deadline starting after the
//! review-stack lock, and holistic complaint severity plus the criterion
//! format-failure fallback. Each test spawns the real script with a JSON
//! state file and skips when python3 (or bash and git, where needed) is
//! missing.

use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
    let state_file = dir.join(format!("state-{label}.json"));
    fs::write(&state_file, state.to_string()).unwrap();
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

#[cfg(unix)]
#[test]
fn run_checks_deadline_starts_after_the_stack_lock_and_skips_are_not_red() {
    if missing(&["python3"]) {
        return;
    }
    let dir = fixture_dir("lock-deadline");
    let lock = dir.0.join("stack.lock");
    let mut holder = Command::new("python3")
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
    let mut line = String::new();
    BufReader::new(holder.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "locked");

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
    let _ = holder.wait();
    let record = &out["exec_results"][0];
    assert!(
        record.get("skipped").is_none(),
        "the lock wait must not spend the deadline: {out}"
    );
    assert_eq!(record["exit"], 0, "{out}");

    let mut state = verdict_state(json!([met("c1")]));
    state["exec_results"] = json!([{"cmd": "echo late", "skipped": "deadline"}]);
    let text = report(&dir.0, "skipped", &state);
    assert!(text.starts_with("ADVERSARIAL_REVIEW: CONFORMS"), "{text}");
    assert!(text.contains("- [NOT RUN] `echo late`"), "{text}");
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
