//! Pins for the coder's `verify_tier` variable. The round tier runs the base
//! suite as a background job in a detached worktree of the base commit,
//! verifies each lap with the commands select_checks picked, and writes a
//! verification record keyed by the worktree tree; the finalize gate runs the
//! whole suite against the base record. The default full tier stays as it
//! was. On unix `bash`, `jq`, `git` and `python3` must be on PATH (asserted,
//! so a missing tool cannot hide the pins); elsewhere the pins skip without
//! them.
//!
//! The review side is pinned here too: review-gauntlet's and the adversary's
//! `verification_tier` (a final-tier review needs a green finalize record for
//! the reviewed tree and never re-runs the suite), and step-runner seating its
//! coder at the round tier with its own gates as the step's finalize gate.

use serde_json::{Value, json};
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BASELINE: &str = "coder/scripts/baseline_tests.sh";
const VERIFY_TESTS: &str = "coder/scripts/verify_tests.sh";
const SELECT_FAULT: &str = "coder/scripts/select_fault.sh";
const TOOLS: [&str; 3] = ["jq", "git", "python3"];
const CACHE: &str = r#"{"type":"unknown","build":"","test":"","check":"","lint":"","fmt":""}"#;

/// Every variable the coder scripts read, cleared so the host's own run
/// cannot leak into a pin.
const CLEARED: [&str; 16] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GRAPH_STATE",
    "BUILD_CMD",
    "TEST_CMD",
    "TEST_TIMEOUT",
    "COYOTE_GATE_OUTPUT_MAX_BYTES",
    "LLM_AGENT_VAR_BUILD_LOCK",
    "LLM_AGENT_VAR_PROJECT_DIR",
    "LLM_AGENT_VAR_TEST_COMMAND",
    "LLM_AGENT_VAR_BUILD_TIMEOUT",
    "LLM_AGENT_VAR_TEST_TIMEOUT",
    "LLM_AGENT_VAR_BASELINE_FLOOR",
    "LLM_AGENT_VAR_VERIFY_TIER",
];

/// The first bash on PATH outside System32, whose bash is the WSL launcher.
fn bash_program() -> Option<PathBuf> {
    which::which_all("bash").ok()?.find(|path| {
        !path
            .to_string_lossy()
            .split(['/', '\\'])
            .any(|part| part.eq_ignore_ascii_case("system32"))
    })
}

fn tools_missing() -> bool {
    if cfg!(unix) {
        assert!(bash_program().is_some(), "bash must resolve on unix");
        for tool in TOOLS {
            assert!(which::which(tool).is_ok(), "{tool} must be on PATH on unix");
        }
    }
    let missing = bash_program().is_none() || TOOLS.iter().any(|t| which::which(t).is_err());
    if missing {
        eprintln!("skipping: bash/jq/git/python3 not available");
    }
    missing
}

fn asset(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/agents")
        .join(rel)
}

/// A temporary directory whose fixture project is `proj`. Dropping it kills
/// any base suite job the pin left running and removes its worktree.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let jobs = self.0.join("proj/.git/coyote-baselines/jobs");
        for entry in fs::read_dir(jobs).into_iter().flatten().flatten() {
            let Ok(text) = fs::read_to_string(entry.path().join("status.json")) else {
                continue;
            };
            let Ok(status) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            for key in ["pgid", "cmd_pgid"] {
                if let Some(pgid) = status[key].as_i64() {
                    kill_group(pgid);
                }
            }
            if let Some(worktree) = status["worktree"].as_str() {
                let _ = fs::remove_dir_all(worktree);
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(label: &str) -> TempDirGuard {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!("coyote-tiers-{label}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    TempDirGuard(dir)
}

fn kill_group(pgid: i64) {
    let _ = Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .output();
}

fn alive(pid: i64) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Runs git in `dir` without the host's system or global config, so a
/// fixture commit has an author and never tries to sign.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=verify-tiers",
            "-c",
            "user.email=verify-tiers@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", dir)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A committed project at `root/proj` with a seeded detection cache, so
/// detect_project never reaches the LLM detector. Its own path is committed
/// too, so no two fixtures share a commit and a base worktree path.
fn committed(root: &Path) -> PathBuf {
    let dir = root.join("proj");
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join(".coyote-project.json"), CACHE).unwrap();
    fs::write(dir.join("where.txt"), dir.to_string_lossy().as_bytes()).unwrap();
    fs::write(dir.join("src/widget.txt"), "widget\n").unwrap();
    fs::write(dir.join(".gitignore"), "state-*.json\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "fixture"]);
    dir
}

fn commit_file(dir: &Path, file: &str) -> String {
    fs::write(dir.join(file), file).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", file]);
    git(dir, &["rev-parse", "HEAD"])
}

fn scrubbed(program: &Path) -> Command {
    let mut cmd = Command::new(program);
    cmd.env("GIT_CONFIG_NOSYSTEM", "1");
    for name in CLEARED {
        cmd.env_remove(name);
    }
    cmd
}

fn parse(label: &str, output: &std::process::Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("{label} emitted invalid JSON ({e}): {stdout}\nstderr: {stderr}")
    })
}

fn run_script(rel: &str, dir: &Path, state: &Value, envs: &[(&str, &str)]) -> Value {
    let state_file = dir.join(format!("state-{}.json", rel.replace('/', "-")));
    fs::write(&state_file, state.to_string()).unwrap();
    let output = scrubbed(&bash_program().expect("bash resolved by tools_missing"))
        .arg(asset(rel))
        .current_dir(dir)
        .env("GRAPH_STATE_FILE", &state_file)
        .envs(envs.iter().copied())
        .output()
        .expect("spawn bash");
    assert!(
        output.status.success(),
        "{rel} exited {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    parse(rel, &output)
}

/// Runs a `.shared` python script and returns its exit code and JSON.
fn shared_py(script: &str, args: &[&str]) -> (Option<i32>, Value) {
    let output = scrubbed(Path::new("python3"))
        .arg(asset(&format!(".shared/{script}")))
        .args(args)
        .output()
        .expect("spawn python3");
    (output.status.code(), parse(script, &output))
}

fn finalize(dir: &Path, envs: &[(&str, &str)]) -> Value {
    let output = scrubbed(&bash_program().expect("bash resolved by tools_missing"))
        .arg(asset(".shared/finalize_gate.sh"))
        .arg(dir)
        .current_dir(dir)
        .envs(envs.iter().copied())
        .output()
        .expect("spawn bash");
    assert!(
        output.status.success(),
        "finalize_gate.sh exited {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    parse("finalize_gate.sh", &output)
}

fn str_field<'a>(out: &'a Value, key: &str) -> &'a str {
    out.get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string field '{key}' in {out}"))
}

fn graph() -> serde_yaml::Value {
    serde_yaml::from_str(&fs::read_to_string(asset("coder/graph.yaml")).unwrap()).unwrap()
}

fn path_str(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn project_state(dir: &Path) -> Value {
    json!({ "project_dir": path_str(dir) })
}

fn round<'a>(cmd: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![("LLM_AGENT_VAR_VERIFY_TIER", "round"), ("TEST_CMD", cmd)]
}

/// Verify state for a lap that selected `targeted` and `impacted`, carrying
/// what baseline_tests returned.
fn lap_state(dir: &Path, baseline: &Value, targeted: &[&str], impacted: &[&str]) -> Value {
    let mut state = json!({
        "project_dir": path_str(dir),
        "targeted": targeted,
        "impacted": impacted,
        "reason": "pinned selection",
    });
    for key in [
        "baseline_status",
        "baseline_mode",
        "baseline_key",
        "baseline_base",
    ] {
        if let Some(value) = baseline.get(key) {
            state[key] = value.clone();
        }
    }
    state
}

fn store(dir: &Path) -> PathBuf {
    dir.join(".git/coyote-baselines")
}

fn job_status(dir: &Path, key: &str) -> Value {
    let text = fs::read_to_string(store(dir).join("jobs").join(key).join("status.json"))
        .unwrap_or_else(|e| panic!("status.json of job {key}: {e}"));
    serde_json::from_str(&text).unwrap()
}

/// Waits for the base suite job `key` of `dir` to store its record and
/// returns the record.
fn await_base(dir: &Path, key: &str) -> Value {
    let until = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;
    let (code, out) = shared_py(
        "base_suite.py",
        &[
            "await",
            "--root",
            path_str(dir),
            "--key",
            key,
            "--until",
            &until.to_string(),
        ],
    );
    assert_eq!(code, Some(0), "{out}");
    serde_json::from_str(&fs::read_to_string(str_field(&out, "record")).unwrap()).unwrap()
}

fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn worktree_count(dir: &Path) -> usize {
    git(dir, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .count()
}

fn round_lookup(dir: &Path) -> Value {
    shared_py(
        "verify_record.py",
        &["lookup", "--root", path_str(dir), "--tier", "round"],
    )
    .1
}

/// The tree hash `verify_record.py tree` prints for `dir`.
fn worktree_tree(dir: &Path) -> String {
    let output = scrubbed(Path::new("python3"))
        .arg(asset(".shared/verify_record.py"))
        .args(["tree", "--root", path_str(dir)])
        .output()
        .expect("spawn python3");
    assert!(
        output.status.success(),
        "verify_record.py tree exited {:?}",
        output.status.code()
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn last_line(text: &str) -> &str {
    text.lines().last().unwrap_or("")
}

/// A green lap reads `Tests: passed` and `Tier: round: ...` in the final
/// report, and leaves a green round record for the tree it verified.
#[test]
fn round_green_lap_reports_passed_and_its_tier_and_records_the_tree() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("green-lap");
    let dir = committed(&root.0);
    let envs = round("true");
    let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    let state = lap_state(&dir, &base, &["true"], &["echo impacted"]);
    let out = run_script(VERIFY_TESTS, &dir, &state, &envs);
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(str_field(&out, "tests_verdict"), "passed", "{out}");
    assert_eq!(str_field(&out, "_next"), "self_review", "{out}");
    let tier = str_field(&out, "tests_tier");
    assert!(
        tier.starts_with("round: targeted `true`; impacted `echo impacted`;"),
        "{tier}"
    );
    let report = str_field(&out, "baseline_report");
    let rendered = graph()["nodes"]["end_success"]["output"]
        .as_str()
        .unwrap()
        .replace("{{tests_verdict}}", str_field(&out, "tests_verdict"))
        .replace("{{baseline_report}}", report)
        .replace("{{tests_tier}}", tier);
    assert!(rendered.contains("\nTests: passed\n"), "{rendered}");
    assert!(
        rendered.contains(&format!("{report}\nTier: {tier}\nReview: ")),
        "{rendered}"
    );
    let lookup = round_lookup(&dir);
    assert_eq!(str_field(&lookup, "status"), "green", "{lookup}");
    assert!(Path::new(str_field(&lookup, "path")).is_file(), "{lookup}");
    await_base(&dir, str_field(&base, "baseline_key"));
}

/// A red targeted command that the green base does not share stops the lap:
/// the impacted commands are not run.
#[test]
fn round_red_targeted_command_leaves_the_impacted_commands_unrun() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("targeted-red");
    let dir = committed(&root.0);
    let envs = round("true");
    let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    await_base(&dir, str_field(&base, "baseline_key"));
    let red = "printf 'test fresh ... FAILED\\n'; exit 101";
    let state = lap_state(&dir, &base, &[red], &["touch impacted.txt"]);
    let out = run_script(VERIFY_TESTS, &dir, &state, &envs);
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert_eq!(
        str_field(&out, "tests_verdict"),
        "failed (exit 101)",
        "{out}"
    );
    assert_eq!(str_field(&out, "_next"), "fix_loop_gate", "{out}");
    assert!(
        str_field(&out, "tests_tier").contains("impacted `touch impacted.txt` (not run)"),
        "{out}"
    );
    assert!(
        str_field(&out, "tests_output").contains("was green, so these failures are new"),
        "{out}"
    );
    assert!(
        !dir.join("impacted.txt").exists(),
        "an impacted command ran"
    );
    assert_eq!(str_field(&round_lookup(&dir), "status"), "red");
}

/// A command narrower than the full suite passes red only when it names
/// its failing tests and every one failed in the base suite.
#[test]
fn round_narrow_command_passes_only_on_identified_base_failures() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("narrow-run");
    let dir = committed(&root.0);
    let full = "printf 'test known_a ... FAILED\\ntest known_b ... FAILED\\n'; exit 101";
    let envs = round(full);
    let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    await_base(&dir, str_field(&base, "baseline_key"));

    let subset = "printf 'test known_a ... FAILED\\n'; exit 1";
    let out = run_script(
        VERIFY_TESTS,
        &dir,
        &lap_state(&dir, &base, &[subset], &[]),
        &envs,
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(
        str_field(&out, "tests_verdict"),
        "red, matches the pre-run baseline (exit 1); accepted pre-existing failures: known_a",
        "{out}"
    );

    let outside = "printf 'test known_a ... FAILED\\ntest other ... FAILED\\n'; exit 101";
    let out = run_script(
        VERIFY_TESTS,
        &dir,
        &lap_state(&dir, &base, &[outside], &[]),
        &envs,
    );
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert!(
        str_field(&out, "tests_output").contains("New failures: other"),
        "{out}"
    );

    let unidentified = "echo boom; exit 101";
    let out = run_script(
        VERIFY_TESTS,
        &dir,
        &lap_state(&dir, &base, &[unidentified], &[]),
        &envs,
    );
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert!(
        str_field(&out, "tests_output").contains("No failing test could be identified"),
        "{out}"
    );
}

/// When impact_facts or select_checks dies, select_fault selects the full
/// test command and the lap says why.
#[test]
fn select_fault_runs_the_full_suite_and_says_why() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("select-fault");
    let dir = committed(&root.0);
    let full = "printf 'test known ... FAILED\\n'; exit 101";
    let envs = round(full);
    let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);

    let mut state = project_state(&dir);
    state["selection_output"] = json!("select_checks failed: provider error\nsecond line");
    let picked = run_script(SELECT_FAULT, &dir, &state, &envs);
    assert_eq!(picked["targeted"], json!([full]), "{picked}");
    assert_eq!(picked["impacted"], json!([]), "{picked}");
    assert_eq!(str_field(&picked, "_next"), "verify_tests", "{picked}");
    let reason = str_field(&picked, "reason");
    assert!(
        reason.contains("(select_checks failed: provider error second line)"),
        "{reason}"
    );
    assert!(
        reason.contains("the full test command runs instead"),
        "{reason}"
    );

    state["last_script_error"] = json!("script 'impact_facts' failed: exit 1");
    let crashed = run_script(SELECT_FAULT, &dir, &state, &envs);
    assert!(
        str_field(&crashed, "reason").contains("(script 'impact_facts' failed: exit 1)"),
        "{crashed}"
    );
    assert_eq!(str_field(&crashed, "last_script_error"), "", "{crashed}");

    let mut lap = lap_state(&dir, &base, &[], &[]);
    for key in ["targeted", "impacted", "reason"] {
        lap[key] = picked[key].clone();
    }
    let out = run_script(VERIFY_TESTS, &dir, &lap, &envs);
    assert!(
        str_field(&out, "tests_output").starts_with(&format!("Ran: {full} (targeted)\n")),
        "{out}"
    );
    assert!(
        str_field(&out, "tests_tier").ends_with(&format!("selection: {reason}")),
        "{out}"
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
}

/// An empty selection runs nothing and reports NOT RUN, never passed.
#[test]
fn round_empty_selection_is_not_run() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("empty-selection");
    let dir = committed(&root.0);
    let envs = round("touch ran.txt");
    let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    let mut state = lap_state(&dir, &base, &[], &[]);
    state["reason"] = json!("only docs changed");
    let out = run_script(VERIFY_TESTS, &dir, &state, &envs);
    assert_eq!(
        str_field(&out, "tests_verdict"),
        "NOT RUN: no targeted or impacted tests selected: only docs changed",
        "{out}"
    );
    assert!(
        str_field(&out, "tests_output").contains("GATE NOT RUN"),
        "{out}"
    );
    assert_eq!(str_field(&out, "_next"), "self_review", "{out}");
    assert!(
        last_line(str_field(&out, "baseline_report")).starts_with("Baseline mode: "),
        "{out}"
    );
    assert!(!dir.join("ran.txt").exists(), "a test ran in the live tree");
    await_base(&dir, str_field(&base, "baseline_key"));
}

/// verify_tier defaults to full; any other value than round runs the full
/// tier and resolve_paths says so on the Tier line. The round route and
/// its LLM node are declared as the tier needs them.
#[test]
fn verify_tier_defaults_to_full_and_declares_the_round_route() {
    let graph = graph();
    let tier = graph["variables"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|v| v["name"].as_str() == Some("verify_tier"))
        .expect("verify_tier variable");
    assert_eq!(tier["default"].as_str(), Some("full"));
    let initial = &graph["initial_state"];
    assert_eq!(initial["tests_tier"].as_str(), Some("full"));
    for key in ["baseline_mode", "baseline_key", "baseline_base"] {
        assert_eq!(initial[key].as_str(), Some(""), "initial_state.{key}");
    }
    let output = graph["nodes"]["end_success"]["output"].as_str().unwrap();
    assert!(
        output.contains(
            "\nTests: {{tests_verdict}}{{baseline_report}}\nTier: {{tests_tier}}\nReview: "
        ),
        "{output}"
    );
    let nodes = &graph["nodes"];
    assert_eq!(
        nodes["impact_facts"]["next"].as_str(),
        Some("select_checks")
    );
    assert_eq!(
        nodes["impact_facts"]["fallback"].as_str(),
        Some("select_fault")
    );
    let select = &nodes["select_checks"];
    assert_eq!(select["type"].as_str(), Some("llm"));
    assert_eq!(select["max_iterations"].as_u64(), Some(30));
    assert_eq!(select["timeout"].as_u64(), Some(600));
    assert_eq!(select["fallback"].as_str(), Some("select_fault"));
    assert_eq!(select["next"].as_str(), Some("verify_tests"));
    let tools: Vec<&str> = select["tools"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert_eq!(tools, ["fs_cat", "fs_ls", "execute_command"]);
    let required: Vec<&str> = select["output_schema"]["required"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert_eq!(required, ["targeted", "impacted", "reason"]);

    if tools_missing() {
        return;
    }
    let root = temp_dir("default-tier");
    let dir = committed(&root.0);
    for value in [None, Some("full"), Some("round")] {
        let envs: Vec<(&str, &str)> = value
            .map(|v| vec![("LLM_AGENT_VAR_VERIFY_TIER", v)])
            .unwrap_or_default();
        let out = run_script("coder/scripts/resolve_paths.sh", &dir, &Value::Null, &envs);
        assert!(out.get("tests_tier").is_none(), "{value:?}: {out}");
    }
    let out = run_script(
        "coder/scripts/resolve_paths.sh",
        &dir,
        &Value::Null,
        &[("LLM_AGENT_VAR_VERIFY_TIER", "rounds")],
    );
    assert_eq!(
        str_field(&out, "tests_tier"),
        "full (verify_tier \"rounds\" is neither round nor full, so the full tier ran)",
        "{out}"
    );

    let build = |tier: &str| {
        run_script(
            "coder/scripts/verify_build.sh",
            &dir,
            &project_state(&dir),
            &[("BUILD_CMD", "true"), ("LLM_AGENT_VAR_VERIFY_TIER", tier)],
        )
    };
    assert_eq!(str_field(&build(""), "_next"), "verify_tests");
    assert_eq!(str_field(&build("rounds"), "_next"), "verify_tests");
    assert_eq!(str_field(&build("round"), "_next"), "impact_facts");

    let plain = run_script(
        VERIFY_TESTS,
        &dir,
        &project_state(&dir),
        &[("TEST_CMD", "true")],
    );
    let unknown = run_script(
        VERIFY_TESTS,
        &dir,
        &project_state(&dir),
        &[
            ("TEST_CMD", "true"),
            ("LLM_AGENT_VAR_VERIFY_TIER", "rounds"),
        ],
    );
    assert_eq!(plain, unknown);
    assert!(plain.get("tests_tier").is_none(), "{plain}");
}

/// The round baseline returns before the base suite finishes, with the job
/// recorded in status.json, and the suite never runs in the live tree.
#[test]
fn round_baseline_returns_at_once_and_leaves_the_live_tree_alone() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("returns-at-once");
    let dir = committed(&root.0);
    let envs = round("echo ran > ran.txt; sleep 20");
    let head = git(&dir, &["rev-parse", "HEAD"]);
    let status_before = git(&dir, &["status", "--porcelain"]);
    let tree_before = worktree_tree(&dir);
    let started = Instant::now();
    let out = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(
        str_field(&out, "baseline_mode"),
        "background (pending)",
        "{out}"
    );
    assert_eq!(str_field(&out, "baseline_status"), "pending", "{out}");
    assert_eq!(str_field(&out, "baseline_base"), head, "{out}");
    let key = str_field(&out, "baseline_key");
    assert_eq!(key.len(), 16, "{out}");
    let status = job_status(&dir, key);
    assert_eq!(str_field(&status, "base"), head, "{status}");

    let record = await_base(&dir, key);
    assert_eq!(str_field(&record, "status"), "green", "{record}");
    assert!(
        !dir.join("ran.txt").exists(),
        "the suite ran in the live tree"
    );
    assert_eq!(git(&dir, &["status", "--porcelain"]), status_before);
    let tree_after = worktree_tree(&dir);
    assert_eq!(tree_before, tree_after);
}

/// An edit already in the tree does not reach the base suite: it runs in a
/// detached worktree of HEAD, which the record names.
#[test]
fn round_baseline_on_a_dirty_tree_records_the_green_base() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("dirty-tree");
    let dir = committed(&root.0);
    let head = git(&dir, &["rev-parse", "HEAD"]);
    let envs = round("if [ -e broken.txt ]; then printf 'test edited ... FAILED\\n'; exit 101; fi");
    fs::write(dir.join("broken.txt"), "edit").unwrap();
    let out = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    assert_eq!(
        str_field(&out, "baseline_mode"),
        "background (pending)",
        "{out}"
    );
    let record = await_base(&dir, str_field(&out, "baseline_key"));
    assert_eq!(str_field(&record, "status"), "green", "{record}");
    assert_eq!(
        str_field(&record, "ran_in"),
        format!("detached worktree at {}", &head[..7]),
        "{record}"
    );
}

/// A second start while the job runs attaches to it; once it is done the
/// record is reused.
#[test]
fn round_baseline_attaches_to_a_running_job_then_reuses_its_record() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("attach");
    let dir = committed(&root.0);
    let envs = round("sleep 3");
    let first = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    let key = str_field(&first, "baseline_key").to_string();
    let pid = job_status(&dir, &key)["pid"].clone();
    let second = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    assert_eq!(str_field(&second, "baseline_key"), key, "{second}");
    assert_eq!(
        str_field(&second, "baseline_mode"),
        "background (pending)",
        "{second}"
    );
    assert_eq!(fs::read_dir(store(&dir).join("jobs")).unwrap().count(), 1);
    assert_eq!(job_status(&dir, &key)["pid"], pid);
    await_base(&dir, &key);
    let third = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    assert_eq!(str_field(&third, "baseline_mode"), "reused", "{third}");
    assert_eq!(str_field(&third, "baseline_status"), "green", "{third}");
}

/// A red command while the base suite still runs waits for it, and a
/// failure the base shares is pre-existing.
#[test]
fn round_red_run_while_pending_waits_and_classifies_pre_existing() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("pending-wait");
    let dir = committed(&root.0);
    let full =
        "if [ ! -e live.txt ]; then sleep 15; fi; printf 'test known ... FAILED\\n'; exit 101";
    let envs = round(full);
    fs::write(dir.join("live.txt"), "live").unwrap();
    let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    let out = run_script(
        VERIFY_TESTS,
        &dir,
        &lap_state(&dir, &base, &[full], &[]),
        &envs,
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(
        str_field(&out, "tests_verdict"),
        "red, matches the pre-run baseline (exit 101); accepted pre-existing failures: known",
        "{out}"
    );
    assert_eq!(
        str_field(&out, "baseline_report"),
        "\nPre-existing failures: known\nBaseline mode: background",
        "{out}"
    );
}

/// A job killed before it stored a record is never restarted: only the red
/// command is re-run at the base, and the report says the job was killed.
#[test]
fn round_killed_job_reruns_only_the_red_command_at_the_base() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("killed-job");
    let dir = committed(&root.0);
    let full_runs = root.0.join("full-runs.txt");
    let narrow_runs = root.0.join("narrow-runs.txt");
    let full = format!("echo f >> '{}'; sleep 30", full_runs.display());
    let narrow = format!(
        "echo n >> '{}'; printf 'test known ... FAILED\\n'; exit 101",
        narrow_runs.display()
    );
    let envs = round(&full);
    let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    let key = str_field(&base, "baseline_key").to_string();
    wait_for("the base command to start", || {
        job_status(&dir, &key)["cmd_pgid"].is_i64()
    });
    let status = job_status(&dir, &key);
    let pid = status["pid"].as_i64().unwrap();
    kill_group(status["pgid"].as_i64().unwrap());
    wait_for("the job to die", || !alive(pid));

    let out = run_script(
        VERIFY_TESTS,
        &dir,
        &lap_state(&dir, &base, &[&narrow], &[]),
        &envs,
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert!(
        last_line(str_field(&out, "baseline_report"))
            .starts_with("Baseline mode: lazy (background job killed"),
        "{out}"
    );
    assert_eq!(lines(&full_runs).len(), 1, "the full suite ran again");
    assert_eq!(lines(&narrow_runs).len(), 2, "live run plus one base run");
    assert_eq!(str_field(&job_status(&dir, &key), "state"), "killed");
}

/// A base suite still running at the lap's deadline leaves a red command
/// unclassified, which is red, never passed.
#[test]
fn round_deadline_while_pending_leaves_red_unclassified() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("deadline");
    let dir = committed(&root.0);
    let full =
        "if [ ! -e live.txt ]; then sleep 30; fi; printf 'test known ... FAILED\\n'; exit 101";
    let mut envs = round(full);
    envs.push(("LLM_AGENT_VAR_TEST_TIMEOUT", "33"));
    fs::write(dir.join("live.txt"), "live").unwrap();
    let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    let out = run_script(
        VERIFY_TESTS,
        &dir,
        &lap_state(&dir, &base, &[full], &[]),
        &envs,
    );
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert_eq!(
        str_field(&out, "tests_verdict"),
        "failed (exit 101)",
        "{out}"
    );
    assert!(
        str_field(&out, "tests_output").contains("Baseline still running; failures not classified"),
        "{out}"
    );
    assert_eq!(
        last_line(str_field(&out, "baseline_report")),
        "Baseline mode: background (pending)",
        "{out}"
    );
}

/// The job keeps running after the node that started it has exited.
#[test]
fn round_base_job_outlives_the_node_that_started_it() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("outlives-node");
    let dir = committed(&root.0);
    let out = run_script(BASELINE, &dir, &project_state(&dir), &round("sleep 3"));
    let key = str_field(&out, "baseline_key");
    let pid = job_status(&dir, key)["pid"].as_i64().unwrap();
    assert!(alive(pid), "the job died with baseline_tests");
    await_base(&dir, key);
    wait_for("the job to mark itself done", || {
        job_status(&dir, key)["state"] == "done"
    });
}

/// A finished job leaves no base worktree behind, and a sweep after a dead
/// job removes the one it left.
#[test]
fn round_base_worktrees_are_removed_and_swept_after_a_dead_job() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("sweep");
    let dir = committed(&root.0);
    let out = run_script(BASELINE, &dir, &project_state(&dir), &round("true"));
    let key = str_field(&out, "baseline_key").to_string();
    await_base(&dir, &key);
    wait_for("the job to mark itself done", || {
        job_status(&dir, &key)["state"] == "done"
    });
    let worktree = str_field(&job_status(&dir, &key), "worktree").to_string();
    assert!(worktree.contains("coyote-base-"), "{worktree}");
    assert!(!Path::new(&worktree).exists(), "{worktree} left behind");
    assert_eq!(worktree_count(&dir), 1);

    commit_file(&dir, "next.txt");
    let out = run_script(BASELINE, &dir, &project_state(&dir), &round("sleep 30"));
    let key = str_field(&out, "baseline_key").to_string();
    wait_for("the base command to start", || {
        job_status(&dir, &key)["cmd_pgid"].is_i64()
    });
    let status = job_status(&dir, &key);
    let pid = status["pid"].as_i64().unwrap();
    kill_group(status["pgid"].as_i64().unwrap());
    kill_group(status["cmd_pgid"].as_i64().unwrap());
    wait_for("the job to die", || !alive(pid));
    let worktree = str_field(&status, "worktree").to_string();
    assert!(Path::new(&worktree).exists(), "{worktree}");

    let (code, swept) = shared_py("base_suite.py", &["sweep", "--root", path_str(&dir)]);
    assert_eq!(code, Some(0), "{swept}");
    assert_eq!(swept["swept"], json!([key]), "{swept}");
    assert_eq!(str_field(&job_status(&dir, &key), "state"), "killed");
    assert!(!Path::new(&worktree).exists(), "{worktree} not swept");
    assert_eq!(worktree_count(&dir), 1);
}

/// Under build_lock the job starts only after the lap's selected commands,
/// and its command runs under the same prefix.
#[test]
fn round_base_job_waits_for_the_selected_commands_under_build_lock() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("build-lock");
    let dir = committed(&root.0);
    let log = root.0.join("lock.log");
    let wrapper = root.0.join("wrap.sh");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$PWD\" >> '{}'\nexec \"$@\"\n",
            log.display()
        ),
    )
    .unwrap();
    Command::new("chmod")
        .args(["+x", path_str(&wrapper)])
        .status()
        .unwrap();
    let mut envs = round("true");
    envs.push(("LLM_AGENT_VAR_BUILD_LOCK", path_str(&wrapper)));
    let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    assert_eq!(
        str_field(&base, "baseline_mode"),
        "background (deferred: build_lock)",
        "{base}"
    );
    assert!(!store(&dir).join("jobs").exists(), "the job started early");
    assert!(!log.exists(), "baseline_tests ran under the lock");

    let out = run_script(
        VERIFY_TESTS,
        &dir,
        &lap_state(&dir, &base, &["echo targeted"], &[]),
        &envs,
    );
    assert_eq!(str_field(&out, "tests_verdict"), "passed", "{out}");
    let report = str_field(&out, "baseline_report");
    assert!(
        report.contains(&format!(
            "Base suite runs under build_lock `{}`",
            wrapper.display()
        )),
        "{report}"
    );
    assert_eq!(
        last_line(report),
        "Baseline mode: background (deferred: build_lock)",
        "{report}"
    );
    let key = fs::read_dir(store(&dir).join("jobs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name()
        .into_string()
        .unwrap();
    let record = await_base(&dir, &key);
    assert_eq!(
        str_field(&record, "lock_prefix"),
        path_str(&wrapper),
        "{record}"
    );
    let ran = lines(&log);
    assert_eq!(ran.len(), 2, "{ran:?}");
    assert!(!ran[0].contains("coyote-base-"), "{ran:?}");
    assert!(ran[1].contains("coyote-base-"), "{ran:?}");
}

/// A record the job of a linked worktree stored is in the common git dir,
/// so the main worktree reuses it.
#[test]
fn round_record_from_a_linked_worktree_is_reused_by_the_main_one() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("linked-worktree");
    let main = committed(&root.0);
    let linked = root.0.join("linked");
    git(
        &main,
        &["worktree", "add", "-q", "--detach", path_str(&linked)],
    );
    let envs = round("true");
    let out = run_script(BASELINE, &linked, &project_state(&linked), &envs);
    let key = str_field(&out, "baseline_key").to_string();
    let until = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60)
        .to_string();
    let (code, awaited) = shared_py(
        "base_suite.py",
        &[
            "await",
            "--root",
            path_str(&linked),
            "--key",
            &key,
            "--until",
            &until,
        ],
    );
    assert_eq!(code, Some(0), "{awaited}");
    let common = fs::canonicalize(store(&main)).unwrap();
    assert!(
        Path::new(str_field(&awaited, "record")).starts_with(&common),
        "{awaited}"
    );
    let reused = run_script(BASELINE, &main, &project_state(&main), &envs);
    assert_eq!(str_field(&reused, "baseline_mode"), "reused", "{reused}");
    assert_eq!(str_field(&reused, "baseline_key"), key, "{reused}");
}

/// A record a synchronous full-tier baseline stored at the floor is reused
/// by the round tier, also from the per-worktree directory older runs wrote
/// to, which both tiers read and neither writes.
#[test]
fn round_baseline_reuses_a_synchronous_record_at_the_floor() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("floor-reuse");
    let dir = committed(&root.0);
    let cmd = "printf 'test known ... FAILED\\n'; exit 101";
    let floor = git(&dir, &["rev-parse", "HEAD"]);
    let sync = run_script(BASELINE, &dir, &project_state(&dir), &[("TEST_CMD", cmd)]);
    assert_eq!(str_field(&sync, "baseline_status"), "red", "{sync}");
    commit_file(&dir, "later.txt");
    let mut envs = round(cmd);
    envs.push(("LLM_AGENT_VAR_BASELINE_FLOOR", &floor));
    let out = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    assert_eq!(str_field(&out, "baseline_mode"), "reused", "{out}");
    assert_eq!(str_field(&out, "baseline_status"), "red", "{out}");
    assert_eq!(str_field(&out, "baseline_base"), floor, "{out}");
    assert_eq!(out["baseline_failures"], json!(["known"]), "{out}");
    assert!(!store(&dir).join("jobs").exists(), "a job started");

    let linked = root.0.join("linked");
    git(
        &dir,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            path_str(&linked),
            &floor,
        ],
    );
    let legacy = PathBuf::from(git(
        &linked,
        &["rev-parse", "--git-path", "coyote-baselines"],
    ));
    let legacy = if legacy.is_absolute() {
        legacy
    } else {
        linked.join(legacy)
    };
    fs::create_dir_all(&legacy).unwrap();
    let records: Vec<PathBuf> = fs::read_dir(store(&dir))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .collect();
    assert_eq!(records.len(), 1, "{records:?}");
    let moved = legacy.join(records[0].file_name().unwrap());
    fs::rename(&records[0], &moved).unwrap();

    let full = run_script(
        BASELINE,
        &linked,
        &project_state(&linked),
        &[("TEST_CMD", cmd)],
    );
    assert_eq!(str_field(&full, "baseline_status"), "red", "{full}");
    assert!(
        str_field(&full, "baseline_note").contains("reused"),
        "{full}"
    );
    let round_out = run_script(BASELINE, &linked, &project_state(&linked), &round(cmd));
    assert_eq!(
        str_field(&round_out, "baseline_mode"),
        "reused",
        "{round_out}"
    );
    assert_eq!(fs::read_dir(&legacy).unwrap().count(), 1);
    assert!(
        !records[0].exists(),
        "a reused legacy record was written to the shared store"
    );
}

/// The key base_suite.py stores HEAD's record of `cmd` in `dir` under.
fn head_key(dir: &Path, cmd: &str) -> String {
    let head = git(dir, &["rev-parse", "HEAD"]);
    let output = Command::new("python3")
        .args([
            "-c",
            "import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest()[:16])",
            &format!("{head}\n.\n{cmd}"),
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A job status or base record that cannot be read gives a lazy or no
/// baseline, and an unreadable verification record counts as missing;
/// none of them ever passes a red run.
#[test]
fn round_unreadable_status_or_record_is_never_green() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("corrupt");
    let dir = committed(&root.0);
    let cmd = "if [ -e broken.txt ]; then printf 'test edited ... FAILED\\n'; exit 101; fi";
    let envs = round(cmd);
    let key = head_key(&dir, cmd);
    let job = store(&dir).join("jobs").join(&key);
    fs::create_dir_all(&job).unwrap();
    fs::write(job.join("status.json"), "{not json").unwrap();
    let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    assert_eq!(str_field(&base, "baseline_mode"), "lazy", "{base}");
    fs::write(dir.join("broken.txt"), "edit").unwrap();
    let out = run_script(
        VERIFY_TESTS,
        &dir,
        &lap_state(&dir, &base, &[cmd], &[]),
        &envs,
    );
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert!(
        last_line(str_field(&out, "baseline_report")).starts_with("Baseline mode: lazy"),
        "{out}"
    );

    let round_record = str_field(&round_lookup(&dir), "path").to_string();
    fs::write(&round_record, "{").unwrap();
    assert_eq!(str_field(&round_lookup(&dir), "status"), "missing");

    fs::remove_file(dir.join("broken.txt")).unwrap();
    commit_file(&dir, "next.txt");
    let key = head_key(&dir, cmd);
    fs::write(store(&dir).join(format!("tests-{key}.json")), "garbage").unwrap();
    let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);
    assert_eq!(str_field(&base, "baseline_mode"), "none", "{base}");
    assert!(
        str_field(&base, "baseline_note").contains("cannot be read"),
        "{base}"
    );
    fs::write(dir.join("broken.txt"), "edit").unwrap();
    let out = run_script(
        VERIFY_TESTS,
        &dir,
        &lap_state(&dir, &base, &[cmd], &[]),
        &envs,
    );
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert!(
        str_field(&out, "tests_output").contains("No baseline to compare with"),
        "{out}"
    );
}

/// The full tier, by default or by name, keeps its synchronous baseline and
/// its output: no job, no tier fields, no verification record.
#[test]
fn full_tier_is_unchanged() {
    if tools_missing() {
        return;
    }
    let baseline_keys = [
        "_next",
        "baseline_exit_code",
        "baseline_failure_hash",
        "baseline_failures",
        "baseline_note",
        "baseline_reused_from",
        "baseline_status",
    ];
    for tier in ["", "full"] {
        let root = temp_dir("full-tier");
        let dir = committed(&root.0);
        let envs = [
            ("TEST_CMD", "printf 'test known ... FAILED\\n'; exit 101"),
            ("LLM_AGENT_VAR_VERIFY_TIER", tier),
        ];
        let base = run_script(BASELINE, &dir, &project_state(&dir), &envs);
        let mut keys: Vec<&str> = base
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, baseline_keys, "{base}");
        assert_eq!(str_field(&base, "baseline_status"), "red", "{base}");
        let mut state = base.clone();
        state["project_dir"] = json!(path_str(&dir));
        let out = run_script(VERIFY_TESTS, &dir, &state, &envs);
        assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
        assert!(out.get("tests_tier").is_none(), "{out}");
        assert!(out.get("baseline_mode").is_none(), "{out}");
        assert!(!dir.join(".git/coyote-verification").exists());
        assert!(!store(&dir).join("jobs").exists());
        assert_eq!(fs::read_dir(store(&dir)).unwrap().count(), 1);
    }
}

/// The finalize gate accepts the failures the base suite already had,
/// fails a new one, and is unavailable with no base to compare with.
#[test]
fn finalize_gate_judges_the_full_suite_against_the_base() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("finalize");
    let dir = committed(&root.0);
    let cmd = "if [ -e new.txt ]; then printf 'test fresh ... FAILED\\n'; fi; printf 'test known ... FAILED\\n'; exit 101";
    let sync = run_script(BASELINE, &dir, &project_state(&dir), &[("TEST_CMD", cmd)]);
    assert_eq!(str_field(&sync, "baseline_status"), "red", "{sync}");
    let envs = [("TEST_CMD", cmd), ("TEST_TIMEOUT", "60")];
    let test_item = |out: &Value| -> Value {
        out["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["role"] == "test")
            .cloned()
            .unwrap_or_else(|| panic!("no test item: {out}"))
    };

    fs::write(dir.join("src/widget.txt"), "edited\n").unwrap();
    let out = finalize(&dir, &envs);
    assert_eq!(str_field(&out, "status"), "green", "{out}");
    assert_eq!(
        str_field(&test_item(&out), "verdict"),
        "pre-existing",
        "{out}"
    );
    let lookup = shared_py(
        "verify_record.py",
        &["lookup", "--root", path_str(&dir), "--tier", "finalize"],
    )
    .1;
    assert_eq!(str_field(&lookup, "status"), "green", "{lookup}");
    assert_eq!(lookup["record"]["tree"], out["tree"], "{lookup}");

    fs::write(dir.join("new.txt"), "new").unwrap();
    let out = finalize(&dir, &envs);
    assert_eq!(str_field(&out, "status"), "red", "{out}");
    assert!(
        str_field(&test_item(&out), "note").contains("new failures: fresh"),
        "{out}"
    );

    fs::remove_file(dir.join("new.txt")).unwrap();
    commit_file(&dir, "later.txt");
    let out = finalize(&dir, &envs);
    assert_eq!(
        str_field(&out, "status"),
        "green",
        "the parent's record: {out}"
    );

    let other = [
        ("TEST_CMD", "printf 'test other ... FAILED\\n'; exit 1"),
        ("TEST_TIMEOUT", "60"),
    ];
    let out = finalize(&dir, &other);
    assert_eq!(str_field(&out, "status"), "unavailable", "{out}");
    assert_eq!(str_field(&test_item(&out), "verdict"), "red", "{out}");
}

const RUN_CHECKS: &str = "adversary/scripts/run_checks.py";
const ADVERSARY_VERDICT: &str = "adversary/scripts/verdict.py";
const GAUNTLET_GATE: &str = "review-gauntlet/scripts/verdict_gate.py";
const FINALIZE_CHECK: &str = "review-gauntlet/scripts/finalize_check.sh";
const STEP_VERIFY_TESTS: &str = "step-runner/scripts/verify_tests.sh";
const SUITE_CRITERION: &str = "the full test suite passes";
const DEFERRED: &str = "DEFERRED: full suite runs at the final gate";

fn run_py(rel: &str, dir: &Path, state: &Value, envs: &[(&str, &str)]) -> Value {
    let state_file = dir.join(format!("state-{}.json", rel.replace('/', "-")));
    fs::write(&state_file, state.to_string()).unwrap();
    let output = scrubbed(Path::new("python3"))
        .arg(asset(rel))
        .current_dir(dir)
        .env("GRAPH_STATE_FILE", &state_file)
        .envs(envs.iter().copied())
        .output()
        .expect("spawn python3");
    assert!(
        output.status.success(),
        "{rel} exited {:?}\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    parse(rel, &output)
}

fn agent_graph(rel: &str) -> serde_yaml::Value {
    serde_yaml::from_str(&fs::read_to_string(asset(rel)).unwrap()).unwrap()
}

/// Writes the finalize record of the worktree tree of `dir` holding `results`.
fn write_finalize(dir: &Path, results: Value) {
    let mut child = scrubbed(Path::new("python3"))
        .arg(asset(".shared/verify_record.py"))
        .args([
            "write",
            "--root",
            path_str(dir),
            "--tier",
            "finalize",
            "--writer",
            "verify-tiers",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn python3");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(results.to_string().as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success(), "verify_record.py write");
}

fn finalize_lookup(dir: &Path) -> Value {
    shared_py(
        "verify_record.py",
        &["lookup", "--root", path_str(dir), "--tier", "finalize"],
    )
    .1
}

fn test_result(cmd: &str, exit: i64, verdict: &str, note: &str) -> Value {
    json!([{
        "role": "test",
        "cmd": cmd,
        "exit": exit,
        "duration_s": 1,
        "verdict": verdict,
        "note": note,
        "tail": "",
    }])
}

/// What run_checks.py records for `dir` at `tier`, with `declared` as the
/// verification_commands.
fn adversary_checks(dir: &Path, tier: &str, declared: &[&str]) -> Value {
    let state = json!({
        "project_dir": path_str(dir),
        "verification_tier": tier,
        "verification_commands": json!(declared).to_string(),
        "criteria": [{"id": "c1", "text": SUITE_CRITERION}],
    });
    let lock = dir.with_file_name("review-stack.lock");
    run_py(
        RUN_CHECKS,
        dir,
        &state,
        &[("ADVERSARY_STACK_LOCK_PATH", path_str(&lock))],
    )
}

fn verdict_of(status: &str, evidence: &str, no_test_reason: &str) -> Value {
    json!([{
        "id": "c1",
        "text": SUITE_CRITERION,
        "status": status,
        "evidence": evidence,
        "complaint": "",
        "no_test_reason": no_test_reason,
    }])
}

/// The adversary report for one criterion judged `verdicts`, over the
/// exec_results and full_suite_record that `checks` recorded.
fn adversary_report(dir: &Path, checks: &Value, verdicts: Value, tier: &str) -> String {
    let mut state = json!({
        "criteria": [{"id": "c1", "text": SUITE_CRITERION}],
        "crit_verdicts": verdicts,
        "verification_tier": tier,
        "verification_commands": "[]",
        "debris_summary": "debris: clean",
    });
    for key in ["exec_results", "full_suite_record"] {
        if let Some(value) = checks.get(key) {
            state[key] = value.clone();
        }
    }
    str_field(&run_py(ADVERSARY_VERDICT, dir, &state, &[]), "adv_report").to_string()
}

fn report_line(report: &str, n: usize) -> &str {
    report.lines().nth(n).unwrap_or("")
}

/// At the final tier a worktree tree with no finalize record diverges, even
/// with every criterion met, and nothing declared runs.
#[test]
fn final_tier_adversary_without_a_finalize_record_diverges_with_every_criterion_met() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("adversary-no-record");
    let dir = committed(&root.0);
    let checks = adversary_checks(&dir, "final", &["touch sentinel.txt"]);
    let tree = worktree_tree(&dir);
    assert_eq!(checks["full_suite_record"]["status"], "missing", "{checks}");
    let line = str_field(&checks["full_suite_record"], "line");
    assert_eq!(
        line,
        format!("FULL-SUITE RECORD MISSING: no finalize record for the worktree tree {tree}"),
        "{checks}"
    );
    assert!(!dir.join("sentinel.txt").exists(), "a declared command ran");

    let met = verdict_of("MET", "the recorded run exits 0", "");
    let report = adversary_report(&dir, &checks, met.clone(), "final");
    assert_eq!(
        report_line(&report, 0),
        "ADVERSARIAL_REVIEW: DIVERGES",
        "{report}"
    );
    assert_eq!(report_line(&report, 2), line, "{report}");
    assert!(report.contains(&format!("\n1. {line}\n")), "{report}");

    let unchecked = adversary_report(&dir, &json!({}), met, "final");
    assert!(
        unchecked
            .contains("FULL-SUITE RECORD MISSING: run_checks recorded no finalize record lookup"),
        "{unchecked}"
    );
}

/// A finalize record for another tree is stale: once the worktree changes,
/// the review diverges.
#[test]
fn final_tier_adversary_with_a_stale_tree_record_diverges() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("adversary-stale");
    let dir = committed(&root.0);
    write_finalize(&dir, test_result("run-suite", 0, "green", ""));
    let met = verdict_of("MET", "finalize record run exits 0", "");
    let fresh = adversary_checks(&dir, "final", &[]);
    assert_eq!(fresh["full_suite_record"]["status"], "green", "{fresh}");
    let report = adversary_report(&dir, &fresh, met.clone(), "final");
    assert_eq!(
        report_line(&report, 0),
        "ADVERSARIAL_REVIEW: CONFORMS",
        "{report}"
    );

    fs::write(dir.join("src/widget.txt"), "changed after the gate\n").unwrap();
    let stale = adversary_checks(&dir, "final", &[]);
    assert_eq!(stale["full_suite_record"]["status"], "missing", "{stale}");
    let report = adversary_report(&dir, &stale, met, "final");
    assert_eq!(
        report_line(&report, 0),
        "ADVERSARIAL_REVIEW: DIVERGES",
        "{report}"
    );
    assert!(
        report_line(&report, 2).starts_with("FULL-SUITE RECORD MISSING: "),
        "{report}"
    );
}

/// With the reviewed tree named, a file another lane writes after it does
/// not hide that tree's record, and a tree with no record is missing.
#[test]
fn final_tier_adversary_looks_up_the_named_review_tree() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("adversary-review-tree");
    let dir = committed(&root.0);
    write_finalize(&dir, test_result("run-suite", 0, "green", ""));
    let reviewed = worktree_tree(&dir);
    fs::write(dir.join("src/probe_test.txt"), "written by another lane\n").unwrap();
    let lock = dir.with_file_name("review-stack.lock");
    let checks_for = |tree: &str| {
        let state = json!({
            "project_dir": path_str(&dir),
            "verification_tier": "final",
            "review_tree": tree,
            "verification_commands": "[]",
            "criteria": [{"id": "c1", "text": SUITE_CRITERION}],
        });
        run_py(
            RUN_CHECKS,
            &dir,
            &state,
            &[("ADVERSARY_STACK_LOCK_PATH", path_str(&lock))],
        )
    };

    let named = checks_for(&reviewed);
    assert_eq!(named["full_suite_record"]["status"], "green", "{named}");
    assert_eq!(str_field(&named["full_suite_record"], "tree"), reviewed);
    let met = verdict_of("MET", "finalize record run exits 0", "");
    let report = adversary_report(&dir, &named, met, "final");
    assert_eq!(
        report_line(&report, 0),
        "ADVERSARIAL_REVIEW: CONFORMS",
        "{report}"
    );

    let live = checks_for("");
    assert_eq!(live["full_suite_record"]["status"], "missing", "{live}");

    let other = "0123456789abcdef0123456789abcdef01234567";
    let unknown = checks_for(other);
    assert_eq!(
        str_field(&unknown["full_suite_record"], "line"),
        format!("FULL-SUITE RECORD MISSING: no finalize record for the reviewed tree {other}"),
        "{unknown}"
    );

    let gauntlet = agent_graph("review-gauntlet/graph.yaml");
    assert_eq!(
        gauntlet["nodes"]["run_adversary"]["inputs"]["review_tree"].as_str(),
        Some("{{review_tree_before}}")
    );
}

/// A record with a new failure diverges with the RED line; one whose only
/// failures already failed on the base passes.
#[test]
fn final_tier_adversary_diverges_on_a_regression_and_passes_pre_existing_failures() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("adversary-red");
    let dir = committed(&root.0);
    let met = verdict_of("MET", "finalize record run", "");

    write_finalize(
        &dir,
        test_result("run-suite", 101, "red", "new failures: fresh"),
    );
    let red = adversary_checks(&dir, "final", &[]);
    assert_eq!(red["full_suite_record"]["status"], "red", "{red}");
    let line = str_field(&red["full_suite_record"], "line");
    assert!(
        line.starts_with("FULL-SUITE RECORD RED: finalize record "),
        "{line}"
    );
    assert!(
        line.ends_with(": test `run-suite` red (new failures: fresh)"),
        "{line}"
    );
    let report = adversary_report(&dir, &red, met.clone(), "final");
    assert_eq!(
        report_line(&report, 0),
        "ADVERSARIAL_REVIEW: DIVERGES",
        "{report}"
    );
    assert_eq!(report_line(&report, 2), line, "{report}");

    write_finalize(
        &dir,
        test_result(
            "run-suite",
            101,
            "pre-existing",
            "pre-existing failures: known",
        ),
    );
    let old = adversary_checks(&dir, "final", &[]);
    assert_eq!(old["full_suite_record"]["status"], "green", "{old}");
    assert_eq!(
        old["exec_results"][0]["baseline"]["status"], "pre-existing",
        "{old}"
    );
    let report = adversary_report(&dir, &old, met, "final");
    assert_eq!(
        report_line(&report, 0),
        "ADVERSARIAL_REVIEW: CONFORMS",
        "{report}"
    );
    assert!(
        report.contains("Pre-existing failures (not blocking):\n- `run-suite` -- exit 101"),
        "{report}"
    );
}

/// A green record is the execution evidence: its runs reach exec_results
/// naming the record, and the declared command does not run; at the empty
/// tier the same declaration does run.
#[test]
fn final_tier_adversary_cites_a_green_record_without_running_the_command() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("adversary-green");
    let dir = committed(&root.0);
    let sentinel = "touch sentinel.txt";
    write_finalize(&dir, test_result(sentinel, 0, "green", ""));
    let record = finalize_lookup(&dir)["record"].clone();
    let checks = adversary_checks(&dir, "final", &[sentinel]);
    assert!(!dir.join("sentinel.txt").exists(), "the full command ran");
    let run = &checks["exec_results"][0];
    assert_eq!(str_field(run, "cmd"), sentinel, "{checks}");
    assert_eq!(run["exit"], 0, "{checks}");
    assert_eq!(
        str_field(run, "source"),
        format!(
            "finalize record {} @ {}",
            str_field(&record, "tree"),
            str_field(&record, "recorded_at")
        ),
        "{checks}"
    );
    assert!(
        str_field(&checks, "tier_note").contains("finalize record of the reviewed tree"),
        "{checks}"
    );
    let evidence = format!(
        "recorded run `{sentinel}` exit 0 ({})",
        str_field(run, "source")
    );
    let report = adversary_report(&dir, &checks, verdict_of("MET", &evidence, ""), "final");
    assert_eq!(
        report_line(&report, 0),
        "ADVERSARIAL_REVIEW: CONFORMS",
        "{report}"
    );
    assert!(report.contains(&evidence), "{report}");

    let plain = adversary_checks(&dir, "", &[sentinel]);
    assert!(dir.join("sentinel.txt").exists(), "{plain}");
    assert!(plain.get("full_suite_record").is_none(), "{plain}");
}

/// A criterion deferred to the final gate conforms at the round tier beside
/// a green recorded run, is partial at the round tier with none, and is
/// always partial at the final tier.
#[test]
fn deferred_criterion_conforms_at_round_and_diverges_at_final() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("adversary-deferred");
    let dir = committed(&root.0);
    let reason = format!("{DEFERRED}; the targeted tests cover the change");
    let deferred = verdict_of("INSPECTED", "src/widget.txt:1 holds the change", &reason);
    let green = json!({
        "exec_results": [{"cmd": "true", "exit": 0, "duration_s": 0.1, "tail": ""}],
        "full_suite_record": {"status": "green", "tree": "t", "line": ""},
    });

    let round = adversary_report(&dir, &green, deferred.clone(), "round");
    assert_eq!(
        report_line(&round, 0),
        "ADVERSARIAL_REVIEW: CONFORMS",
        "{round}"
    );

    let nothing_ran = json!({"exec_results": "none declared"});
    let unproven = adversary_report(&dir, &nothing_ran, deferred.clone(), "round");
    assert_eq!(
        report_line(&unproven, 0),
        "ADVERSARIAL_REVIEW: DIVERGES",
        "{unproven}"
    );
    assert!(
        unproven.contains("1 partial")
            && unproven.contains("deferred to the final gate and judged from the diff only"),
        "{unproven}"
    );

    let fin = adversary_report(&dir, &green, deferred, "final");
    assert_eq!(
        report_line(&fin, 0),
        "ADVERSARIAL_REVIEW: DIVERGES",
        "{fin}"
    );
    assert!(
        fin.contains("1 partial")
            && fin.contains("deferred to the final gate, but this review is the final gate"),
        "{fin}"
    );

    let round_note = str_field(&adversary_checks(&dir, "round", &[]), "tier_note").to_string();
    assert!(round_note.contains(DEFERRED), "{round_note}");
}

/// verdict_gate.py state for a review whose every lane is green.
fn green_lanes(dir: &Path, before: &str, tier: &str) -> Value {
    json!({
        "project_dir": path_str(dir),
        "review_tree_before": before,
        "verification_tier": tier,
        "debris_summary": "debris: clean",
        "code_review_results": ["**Verdict: MERGE-READY**\n"],
        "adversary_results": ["ADVERSARIAL_REVIEW: CONFORMS\nCriteria: 1/1 met.\n"],
    })
}

fn gauntlet_gate(dir: &Path, state: &Value) -> (String, String) {
    let out = run_py(GAUNTLET_GATE, dir, state, &[]);
    (
        str_field(&out, "gauntlet_verdict").to_string(),
        str_field(&out, "gauntlet_report").to_string(),
    )
}

/// A final-tier gauntlet routes signals to finalize_check, which runs the
/// finalize gate on a miss and writes the record, reuses it on a hit, and
/// reports a red record or an unclassified red suite; the verdict gate blocks
/// on anything but a green record for the reviewed tree, lanes green or not.
#[test]
fn final_tier_gauntlet_runs_the_finalize_gate_once_and_blocks_without_a_green_record() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("gauntlet-final");
    let dir = committed(&root.0);
    fs::write(dir.join("src/widget.txt"), "under review\n").unwrap();

    let signals_state = json!({"project_dir": path_str(&dir), "diff_spec": "worktree"});
    let plain = run_py(
        "review-gauntlet/scripts/signals.py",
        &dir,
        &signals_state,
        &[],
    );
    assert!(plain.get("_next").is_none(), "{plain}");
    let mut final_state = signals_state.clone();
    final_state["verification_tier"] = json!("final");
    let signals = run_py(
        "review-gauntlet/scripts/signals.py",
        &dir,
        &final_state,
        &[],
    );
    assert_eq!(str_field(&signals, "_next"), "finalize_check", "{signals}");
    let before = str_field(&signals, "review_tree_before").to_string();
    assert_eq!(before, worktree_tree(&dir));

    let runs = root.0.join("suite-runs.txt");
    let suite = format!("echo ran >> '{}'", runs.display());
    let check_state = json!({"project_dir": path_str(&dir), "review_tree_before": before});
    let miss = run_script(FINALIZE_CHECK, &dir, &check_state, &[("TEST_CMD", &suite)]);
    let record = &miss["full_suite_record"];
    assert_eq!(str_field(record, "status"), "green", "{miss}");
    assert_eq!(str_field(record, "tree"), before, "{miss}");
    assert_eq!(
        lines(&runs).len(),
        1,
        "the finalize gate did not run the suite"
    );
    assert_eq!(str_field(&finalize_lookup(&dir), "status"), "green");

    let hit = run_script(FINALIZE_CHECK, &dir, &check_state, &[("TEST_CMD", &suite)]);
    assert_eq!(
        str_field(&hit["full_suite_record"], "status"),
        "green",
        "{hit}"
    );
    assert!(
        str_field(&hit["full_suite_record"], "detail").ends_with(", reused"),
        "{hit}"
    );
    assert_eq!(lines(&runs).len(), 1, "a record hit ran the suite again");

    let mut state = green_lanes(&dir, &before, "final");
    state["full_suite_record"] = hit["full_suite_record"].clone();
    let (verdict, report) = gauntlet_gate(&dir, &state);
    assert_eq!(verdict, "PASS", "{report}");
    assert!(
        report.contains("| full-suite | GREEN | finalize record "),
        "{report}"
    );

    let (verdict, report) = gauntlet_gate(&dir, &green_lanes(&dir, &before, "final"));
    assert_eq!(verdict, "BLOCKED", "{report}");
    assert!(
        report.contains(
            "\n- FULL-SUITE RECORD MISSING: the finalize check did not finish, so no record was looked up or written"
        ),
        "{report}"
    );
    let mut moved = green_lanes(&dir, "0123456789abcdef0123456789abcdef01234567", "final");
    moved["full_suite_record"] = hit["full_suite_record"].clone();
    let (verdict, report) = gauntlet_gate(&dir, &moved);
    assert_eq!(verdict, "BLOCKED", "{report}");
    assert!(
        report.contains("not the reviewed tree 0123456789abcdef0123456789abcdef01234567"),
        "{report}"
    );

    fs::write(dir.join("src/widget.txt"), "red under review\n").unwrap();
    write_finalize(
        &dir,
        test_result("run-suite", 101, "red", "new failures: fresh"),
    );
    let red_before = worktree_tree(&dir);
    let red_state = json!({"project_dir": path_str(&dir), "review_tree_before": red_before});
    let red = run_script(FINALIZE_CHECK, &dir, &red_state, &[("TEST_CMD", &suite)]);
    assert_eq!(
        str_field(&red["full_suite_record"], "status"),
        "red",
        "{red}"
    );
    assert_eq!(
        lines(&runs).len(),
        1,
        "a red record hit ran the suite again"
    );
    let mut state = green_lanes(&dir, &red_before, "final");
    state["full_suite_record"] = red["full_suite_record"].clone();
    let (verdict, report) = gauntlet_gate(&dir, &state);
    assert_eq!(verdict, "BLOCKED", "{report}");
    assert!(
        report.contains("\n- FULL-SUITE RECORD RED: finalize record "),
        "{report}"
    );
    assert!(
        report.contains("test `run-suite` red (new failures: fresh)"),
        "{report}"
    );

    fs::write(dir.join("src/widget.txt"), "unclassified under review\n").unwrap();
    let odd_before = worktree_tree(&dir);
    let odd_state = json!({"project_dir": path_str(&dir), "review_tree_before": odd_before});
    let odd = run_script(
        FINALIZE_CHECK,
        &dir,
        &odd_state,
        &[("TEST_CMD", "printf 'test other ... FAILED\\n'; exit 1")],
    );
    assert_eq!(
        str_field(&odd["full_suite_record"], "status"),
        "unavailable",
        "{odd}"
    );
    let mut state = green_lanes(&dir, &odd_before, "final");
    state["full_suite_record"] = odd["full_suite_record"].clone();
    let (verdict, report) = gauntlet_gate(&dir, &state);
    assert_eq!(verdict, "BLOCKED", "{report}");
    assert!(
        report.contains("\n- FULL-SUITE RECORD UNAVAILABLE: "),
        "{report}"
    );

    let gauntlet = agent_graph("review-gauntlet/graph.yaml");
    let node = &gauntlet["nodes"]["finalize_check"];
    assert_eq!(node["script"].as_str(), Some("scripts/finalize_check.sh"));
    assert_eq!(node["timeout"].as_u64(), Some(3600));
    assert_eq!(node["next"].as_str(), Some("debris_lint"));
    assert_eq!(node["fallback"].as_str(), Some("debris_lint"));
    assert_eq!(
        gauntlet["nodes"]["run_adversary"]["inputs"]["verification_tier"].as_str(),
        Some("{{verification_tier}}")
    );
    let script = fs::read_to_string(asset(FINALIZE_CHECK)).unwrap();
    assert!(script.contains("DEADLINE_SECS=3300\n"), "{script}");
}

/// A finalize gate whose fmt step rewrites a reviewed file is reported red
/// with the rewritten path, not hidden behind a record for another tree.
#[test]
fn final_tier_gauntlet_reports_a_tree_the_finalize_gate_rewrote() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("gauntlet-rewrite");
    let dir = committed(&root.0);
    fs::write(
        dir.join(".coyote-project.json"),
        r#"{"type":"unknown","build":"","test":"","check":"","lint":"","fmt":"printf 'formatted\\n' > src/widget.txt"}"#,
    )
    .unwrap();
    fs::write(dir.join("src/widget.txt"), "unformatted\n").unwrap();
    let before = worktree_tree(&dir);
    let state = json!({"project_dir": path_str(&dir), "review_tree_before": before});
    let out = run_script(FINALIZE_CHECK, &dir, &state, &[("TEST_CMD", "true")]);
    let record = &out["full_suite_record"];
    assert_eq!(str_field(record, "status"), "red", "{out}");
    assert_ne!(str_field(record, "tree"), before, "{out}");
    assert!(
        str_field(record, "detail").contains(&format!(
            "; the gate rewrote the reviewed tree {before}: src/widget.txt; format the change and review again"
        )),
        "{out}"
    );

    let mut lanes = green_lanes(&dir, &before, "final");
    lanes["full_suite_record"] = record.clone();
    let (verdict, report) = gauntlet_gate(&dir, &lanes);
    assert_eq!(verdict, "BLOCKED", "{report}");
    assert!(
        report.contains("\n- FULL-SUITE RECORD RED: finalize gate ran on tree "),
        "{report}"
    );
}

/// An empty verification_tier leaves the gauntlet verdict, the adversary's
/// runs and its verdict exactly as without the variable; round adds only the
/// criterion checks' note, and an unknown tier fails closed.
#[test]
fn empty_verification_tier_changes_nothing() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("empty-tier");
    let dir = committed(&root.0);
    let before = worktree_tree(&dir);

    let mut absent = green_lanes(&dir, &before, "");
    absent.as_object_mut().unwrap().remove("verification_tier");
    let (verdict, report) = gauntlet_gate(&dir, &absent);
    assert_eq!(verdict, "PASS", "{report}");
    assert!(!report.contains("full-suite"), "{report}");
    for tier in ["", "round"] {
        assert_eq!(
            gauntlet_gate(&dir, &green_lanes(&dir, &before, tier)),
            (verdict.clone(), report.clone()),
            "verification_tier {tier:?}"
        );
    }
    let (verdict, report) = gauntlet_gate(&dir, &green_lanes(&dir, &before, "Final"));
    assert_eq!(verdict, "BLOCKED", "{report}");
    assert!(
        report.contains("verification_tier 'Final' is not"),
        "{report}"
    );

    let mut runs_state = json!({
        "project_dir": path_str(&dir),
        "verification_commands": "[]",
    });
    let without = run_py(RUN_CHECKS, &dir, &runs_state, &[]);
    runs_state["verification_tier"] = json!("");
    assert_eq!(run_py(RUN_CHECKS, &dir, &runs_state, &[]), without);
    assert!(without.get("tier_note").is_none(), "{without}");
    runs_state["verification_tier"] = json!("round");
    let round = run_py(RUN_CHECKS, &dir, &runs_state, &[]);
    assert_eq!(round["exec_results"], without["exec_results"], "{round}");
    assert!(
        round["tier_note"]
            .as_str()
            .is_some_and(|n| n.contains(DEFERRED))
    );
    runs_state["verification_tier"] = json!("rounds");
    let bad = run_py(RUN_CHECKS, &dir, &runs_state, &[]);
    assert!(
        str_field(&bad, "exec_results")
            .starts_with("PIPELINE-FAULT: verification_tier declaration invalid"),
        "{bad}"
    );

    let met = verdict_of("MET", "a test proves it", "");
    let checks = json!({"exec_results": "none declared"});
    let mut state = json!({
        "criteria": [{"id": "c1", "text": SUITE_CRITERION}],
        "crit_verdicts": met,
        "exec_results": "none declared",
        "verification_commands": "[]",
        "debris_summary": "debris: clean",
    });
    let plain = str_field(&run_py(ADVERSARY_VERDICT, &dir, &state, &[]), "adv_report").to_string();
    assert_eq!(adversary_report(&dir, &checks, met.clone(), ""), plain);
    assert_eq!(adversary_report(&dir, &checks, met, "round"), plain);
    state["verification_tier"] = json!("");
    assert_eq!(
        str_field(&run_py(ADVERSARY_VERDICT, &dir, &state, &[]), "adv_report"),
        plain
    );

    let adversary = agent_graph("adversary/graph.yaml");
    let gauntlet = agent_graph("review-gauntlet/graph.yaml");
    for graph in [&adversary, &gauntlet] {
        let tier = graph["variables"]
            .as_sequence()
            .unwrap()
            .iter()
            .find(|v| v["name"].as_str() == Some("verification_tier"))
            .expect("verification_tier variable");
        assert_eq!(tier["default"].as_str(), Some(""));
    }
    assert_eq!(adversary["initial_state"]["tier_note"].as_str(), Some(""));
    for node in ["check_criterion", "holistic"] {
        let prompt = adversary["nodes"][node]["prompt"].as_str().unwrap();
        assert!(
            prompt.contains("{{exec_results}}{{tier_note}}\n"),
            "{node}: {prompt}"
        );
    }
}

/// step-runner seats its coder at the round tier with the commit the step
/// started from as baseline_floor, and its verify_tests gate writes a
/// finalize record after a green suite and reuses it for the same tree.
#[test]
fn step_runner_seats_its_coder_at_round_and_reuses_its_finalize_record() {
    let graph = agent_graph("step-runner/graph.yaml");
    let inputs = &graph["nodes"]["implement"]["inputs"];
    assert_eq!(inputs["verify_tier"].as_str(), Some("round"));
    assert_eq!(
        inputs["baseline_floor"].as_str(),
        Some("{{baseline_floor}}")
    );
    assert_eq!(graph["initial_state"]["baseline_floor"].as_str(), Some(""));
    let output = graph["nodes"]["end_success"]["output"].as_str().unwrap();
    assert!(
        output.contains(
            "\nBuild: passed | Tests: passed (full suite and lint at the step gate; the coder ran \
             targeted and impacted tests per lap)"
        ),
        "{output}"
    );

    if tools_missing() {
        return;
    }
    let root = temp_dir("step-runner-tier");
    let dir = committed(&root.0);
    let head = git(&dir, &["rev-parse", "HEAD"]);
    fs::create_dir_all(dir.join("plans/steps")).unwrap();
    fs::write(
        dir.join("plans/steps/01-first.md"),
        "---\ntitle: first\nstatus: pending\n---\nfirst\n",
    )
    .unwrap();
    let resolved = run_script(
        "step-runner/scripts/resolve_step.sh",
        &dir,
        &json!({}),
        &[("LLM_AGENT_VAR_PROJECT_DIR", path_str(&dir))],
    );
    assert_eq!(str_field(&resolved, "baseline_floor"), head, "{resolved}");

    fs::write(dir.join("src/widget.txt"), "step edit\n").unwrap();
    let runs = root.0.join("suite-runs.txt");
    let suite = format!("echo ran >> '{}'", runs.display());
    let state = json!({
        "project_dir": path_str(&dir),
        "build_ok": true,
        "build_output": "Ran: make check\n\nok",
        "lint_ok": true,
        "lint_output": "(GATE NOT RUN: no lint command configured or detected.)",
    });
    let first = run_script(STEP_VERIFY_TESTS, &dir, &state, &[("TEST_CMD", &suite)]);
    assert_eq!(first["tests_ok"], Value::Bool(true), "{first}");
    assert_eq!(str_field(&first, "_next"), "debt_scan", "{first}");
    assert!(
        str_field(&first, "tests_output").starts_with("Ran: "),
        "{first}"
    );
    assert_eq!(lines(&runs).len(), 1);
    let lookup = finalize_lookup(&dir);
    assert_eq!(str_field(&lookup, "status"), "green", "{lookup}");
    let record = &lookup["record"];
    assert_eq!(str_field(record, "writer"), "step-runner", "{lookup}");
    let results: Vec<(&str, &str, &str)> = record["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                str_field(r, "role"),
                str_field(r, "cmd"),
                str_field(r, "verdict"),
            )
        })
        .collect();
    assert_eq!(
        results,
        [
            ("check", "make check", "green"),
            ("lint", "", "not_run"),
            ("test", suite.as_str(), "green"),
        ]
    );

    let second = run_script(STEP_VERIFY_TESTS, &dir, &state, &[("TEST_CMD", &suite)]);
    assert_eq!(second["tests_ok"], Value::Bool(true), "{second}");
    assert_eq!(str_field(&second, "_next"), "debt_scan", "{second}");
    assert!(
        str_field(&second, "tests_output").starts_with("Reused the finalize record "),
        "{second}"
    );
    assert_eq!(lines(&runs).len(), 1, "a record hit ran the suite again");

    fs::write(dir.join("src/widget.txt"), "known failure\n").unwrap();
    write_finalize(
        &dir,
        test_result(
            "run-suite",
            101,
            "pre-existing",
            "pre-existing failures: known",
        ),
    );
    let failing = format!("{suite}; exit 1");
    let known = run_script(STEP_VERIFY_TESTS, &dir, &state, &[("TEST_CMD", &failing)]);
    assert_eq!(known["tests_ok"], Value::Bool(false), "{known}");
    assert_eq!(str_field(&known, "_next"), "fix_loop_gate", "{known}");
    assert_eq!(
        lines(&runs).len(),
        2,
        "a record green only through pre-existing failures was reused"
    );
}
