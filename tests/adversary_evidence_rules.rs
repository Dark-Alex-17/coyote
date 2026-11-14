//! Evidence and default rules of the adversary graph's deterministic scripts:
//! criteria met by inspection, red verification runs compared with a
//! baseline run on the diff base, the run_checks deadline starting after the
//! review-stack lock, holistic complaint severity plus the criterion
//! format-failure fallback, and the diff base diff_facts.py resolves. Each
//! test spawns the real script with a JSON state file and skips when python3
//! (or bash and git, where needed) is missing.

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
