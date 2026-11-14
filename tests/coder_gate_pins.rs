//! Pins for where and how long the coder graph's verify gates run: the
//! directory they run in when the caller left `project_dir` unset, the
//! `test_command` variable, and the `build_timeout` / `test_timeout`
//! deadlines whose expiry is reported apart from a red run (by the gate
//! itself, or by fix_loop_gate when the engine killed the node), and the
//! baseline of tests that already failed before `implement`, against which
//! verify_tests tells pre-existing failures from new ones. On unix
//! `bash`, `jq` and `git` must be on PATH (asserted, so a missing tool cannot
//! hide the pins); elsewhere the script pins skip without them.

use serde_json::Value;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const GATES: [Gate; 2] = [
    Gate {
        rel: "coder/scripts/verify_build.sh",
        node: "verify_build",
        cmd_env: "BUILD_CMD",
        timeout_var: "LLM_AGENT_VAR_BUILD_TIMEOUT",
        timeout_name: "build_timeout",
        ok_key: "build_ok",
        output_key: "build_output",
    },
    Gate {
        rel: "coder/scripts/verify_tests.sh",
        node: "verify_tests",
        cmd_env: "TEST_CMD",
        timeout_var: "LLM_AGENT_VAR_TEST_TIMEOUT",
        timeout_name: "test_timeout",
        ok_key: "tests_ok",
        output_key: "tests_output",
    },
];

struct Gate {
    rel: &'static str,
    node: &'static str,
    cmd_env: &'static str,
    timeout_var: &'static str,
    timeout_name: &'static str,
    ok_key: &'static str,
    output_key: &'static str,
}

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
        for tool in ["jq", "git"] {
            assert!(which::which(tool).is_ok(), "{tool} must be on PATH on unix");
        }
    }
    let missing =
        bash_program().is_none() || which::which("jq").is_err() || which::which("git").is_err();
    if missing {
        eprintln!("skipping: bash/jq/git not available");
    }
    missing
}

fn asset(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/agents")
        .join(rel)
}

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(label: &str) -> TempDirGuard {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!("coyote-coder-gates-{label}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    TempDirGuard(dir)
}

/// A directory with a pre-seeded detection cache (so detect_project never
/// reaches the LLM detector) and a `where.txt` naming it.
fn project(dir: &Path, name: &str) {
    fs::create_dir_all(dir).unwrap();
    fs::write(
        dir.join(".coyote-project.json"),
        r#"{"type":"unknown","build":"","test":"","check":"","lint":"","fmt":""}"#,
    )
    .unwrap();
    fs::write(dir.join("where.txt"), name).unwrap();
}

/// Runs git in `dir` without the host's system or global config, so a
/// fixture commit has an author and never tries to sign.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args([
            "-c",
            "user.name=coder-gates",
            "-c",
            "user.email=coder-gates@example.invalid",
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

fn git_init(dir: &Path) {
    git(dir, &["init", "-q"]);
}

/// Commits everything under work tree `dir`, with the state and transcript
/// files run_script and run_transcript write ignored, so the tree stays
/// clean for baseline_tests.
fn commit_all(dir: &Path) {
    fs::write(dir.join(".gitignore"), "state-*.json\ntranscript-*.txt\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "fixture"]);
}

/// A `project` that is also a git work tree with everything committed, which
/// is the only kind of tree baseline_tests runs the suite in.
fn committed_project(dir: &Path, name: &str) {
    project(dir, name);
    git_init(dir);
    commit_all(dir);
}

fn run_script(rel: &str, dir: &Path, state: &Value, envs: &[(&str, &str)]) -> Value {
    let state_file = dir.join(format!("state-{}.json", rel.replace('/', "-")));
    fs::write(&state_file, state.to_string()).unwrap();
    let mut cmd = Command::new(bash_program().expect("bash resolved by tools_missing"));
    cmd.arg(asset(rel))
        .current_dir(dir)
        .env("GRAPH_STATE_FILE", &state_file)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GRAPH_STATE")
        .env_remove("CODER_AUTOAPPROVE")
        .env_remove("COYOTE_GATE_OUTPUT_MAX_BYTES")
        .env_remove("BUILD_CMD")
        .env_remove("TEST_CMD")
        .env_remove("LLM_AGENT_VAR_BUILD_LOCK")
        .env_remove("LLM_AGENT_VAR_PROJECT_DIR")
        .env_remove("LLM_AGENT_VAR_TEST_COMMAND")
        .env_remove("LLM_AGENT_VAR_BUILD_TIMEOUT")
        .env_remove("LLM_AGENT_VAR_TEST_TIMEOUT");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("spawn bash");
    let stdout = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "{rel} exited {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("{rel} emitted invalid JSON ({e}): {stdout}\nstderr: {stderr}"))
}

fn str_field<'a>(out: &'a Value, key: &str) -> &'a str {
    out.get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string field '{key}' in {out}"))
}

fn graph() -> serde_yaml::Value {
    serde_yaml::from_str(&fs::read_to_string(asset("coder/graph.yaml")).unwrap()).unwrap()
}

fn variable<'a>(graph: &'a serde_yaml::Value, name: &str) -> &'a serde_yaml::Value {
    graph["variables"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|v| v["name"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("coder/graph.yaml declares no `{name}` variable"))
}

/// resolve_paths records whether the caller left project_dir at its default,
/// which is the only case the gates may look past it.
#[test]
fn resolve_paths_records_whether_project_dir_was_defaulted() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("resolve");
    for (value, defaulted) in [(None, true), (Some(""), true), (Some("."), true)] {
        let mut envs = Vec::new();
        if let Some(v) = value {
            envs.push(("LLM_AGENT_VAR_PROJECT_DIR", v));
        }
        let out = run_script(
            "coder/scripts/resolve_paths.sh",
            &dir.0,
            &Value::Null,
            &envs,
        );
        assert_eq!(
            out["project_dir_defaulted"],
            Value::Bool(defaulted),
            "{value:?}: {out}"
        );
    }
    let explicit = dir.0.to_string_lossy().into_owned();
    let out = run_script(
        "coder/scripts/resolve_paths.sh",
        &dir.0,
        &Value::Null,
        &[("LLM_AGENT_VAR_PROJECT_DIR", explicit.as_str())],
    );
    assert_eq!(out["project_dir_defaulted"], Value::Bool(false), "{out}");
}

/// A parent graph's `inputs: project_dir` seeds the coder's state; that
/// value wins over the inherited agent variable, which may name the parent's
/// tree. With no seeded value (the graph's own empty seed, or no key at all)
/// the variable is used as before.
#[test]
fn resolve_paths_prefers_a_seeded_state_project_dir_over_the_variable() {
    if tools_missing() {
        return;
    }
    let parent = temp_dir("resolve-parent");
    let worktree = temp_dir("resolve-worktree");
    let parent_dir = parent.0.canonicalize().unwrap();
    let worktree_dir = worktree.0.canonicalize().unwrap();
    let env_value = parent_dir.to_string_lossy().into_owned();
    let envs = [("LLM_AGENT_VAR_PROJECT_DIR", env_value.as_str())];

    let seeded = serde_json::json!({ "project_dir": worktree_dir.to_string_lossy() });
    let out = run_script("coder/scripts/resolve_paths.sh", &parent.0, &seeded, &envs);
    assert_eq!(
        Path::new(str_field(&out, "project_dir"))
            .canonicalize()
            .unwrap(),
        worktree_dir,
        "the seeded state project_dir must win over the variable: {out}"
    );
    assert_eq!(out["project_dir_defaulted"], Value::Bool(false), "{out}");

    for state in [
        serde_json::json!({ "project_dir": "" }),
        serde_json::json!({}),
        Value::Null,
    ] {
        let out = run_script("coder/scripts/resolve_paths.sh", &worktree.0, &state, &envs);
        assert_eq!(
            Path::new(str_field(&out, "project_dir"))
                .canonicalize()
                .unwrap(),
            parent_dir,
            "{state}: with no seeded project_dir the variable is used: {out}"
        );
    }
}

/// A spawned coder whose project_dir defaulted to the engine's working
/// directory (the orchestrator's checkout) runs its gates in the work tree
/// the plan edits. An explicit project_dir, or a working directory already
/// inside that tree, is kept as given.
#[test]
fn verify_gates_run_in_the_planned_work_tree_when_project_dir_defaulted() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("workdir");
    let orch = root.0.join("orchestrator");
    let tree = root.0.join("worktree");
    let sub = tree.join("crates/inner");
    project(&orch, "in-orchestrator");
    project(&tree, "in-worktree");
    project(&sub, "in-subdir");
    git_init(&orch);
    commit_all(&orch);
    git_init(&tree);
    commit_all(&tree);
    let planned = tree.join("src/not/created/yet.rs");
    let planned = planned.to_string_lossy();

    let legs: [(&Path, bool, &str); 3] = [
        (&orch, true, "in-worktree"),
        (&orch, false, "in-orchestrator"),
        (&sub, true, "in-subdir"),
    ];
    for (project_dir, defaulted, expected) in legs {
        let state = serde_json::json!({
            "project_dir": project_dir.to_string_lossy(),
            "project_dir_defaulted": defaulted,
            "files_to_modify": [],
            "files_to_create": [planned],
        });
        for gate in &GATES {
            let out = run_script(
                gate.rel,
                &root.0,
                &state,
                &[(gate.cmd_env, "cat where.txt")],
            );
            assert_eq!(out[gate.ok_key], Value::Bool(true), "{}: {out}", gate.rel);
            let output = str_field(&out, gate.output_key);
            assert!(
                output.ends_with(expected),
                "{} with project_dir {} (defaulted {defaulted}) must run {expected}: {output}",
                gate.rel,
                project_dir.display()
            );
            assert!(output.contains("\nRan in: "), "{}: {output}", gate.rel);
        }
        assert_baseline_ran_in(&root.0, &state, &[("TEST_CMD", WHERE_FAILS)], expected);
    }
}

/// Planned paths and project_dir are compared only after resolution, never
/// as strings. A Windows drive path (`X:\...`, which is what the engine hands
/// a Git Bash script) must still find its tree: on unix a `cygpath` shim
/// maps `X:\` to `/`, on Windows the native path and the real cygpath are
/// used. Paths with `.` and `..` segments, on either side, name the same
/// directories as their plain spelling.
#[test]
fn verify_gates_resolve_planned_paths_and_project_dir_before_comparing() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("workdir-forms");
    let orch = root.0.join("orchestrator");
    let tree = root.0.join("worktree");
    project(&orch, "in-orchestrator");
    project(&tree, "in-worktree");
    project(&tree.join("crates/inner"), "in-subdir");
    git_init(&tree);
    commit_all(&tree);

    let bin = root.0.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let path_env = format!("{}:{}", bin.display(), env::var("PATH").unwrap_or_default());
    let planned_file = tree.join("src").join("new.rs");
    let drive_form = if cfg!(windows) {
        planned_file.to_string_lossy().into_owned()
    } else {
        let shim = bin.join("cygpath");
        fs::write(
            &shim,
            "#!/bin/sh\n[ \"$1\" = -u ] || exit 1\nprintf '%s\\n' \"$2\" | sed -e 's/^X://' -e 's/\\\\/\\//g'\n",
        )
        .unwrap();
        let status = Command::new("chmod").arg("+x").arg(&shim).status().unwrap();
        assert!(status.success(), "chmod the cygpath shim");
        format!("X:{}", planned_file.to_string_lossy().replace('/', "\\"))
    };
    // Neither spelling starts with the tree's own path, so only resolving
    // them shows they lie inside it.
    let dotted_planned = format!(
        "{}/orchestrator/../worktree/./src/new.rs",
        root.0.to_string_lossy()
    );
    let dotted_sub = format!(
        "{}/orchestrator/../worktree/./crates/inner",
        root.0.to_string_lossy()
    );
    let orch_dir = orch.to_string_lossy().into_owned();

    let legs: [(&str, &str, &str); 4] = [
        (orch_dir.as_str(), drive_form.as_str(), "in-worktree"),
        (orch_dir.as_str(), dotted_planned.as_str(), "in-worktree"),
        (dotted_sub.as_str(), dotted_planned.as_str(), "in-subdir"),
        (dotted_sub.as_str(), drive_form.as_str(), "in-subdir"),
    ];
    for (project_dir, planned, expected) in legs {
        let state = serde_json::json!({
            "project_dir": project_dir,
            "project_dir_defaulted": true,
            "files_to_modify": [planned],
            "files_to_create": [],
        });
        for gate in &GATES {
            let mut envs = vec![(gate.cmd_env, "cat where.txt")];
            if !cfg!(windows) {
                envs.push(("PATH", path_env.as_str()));
            }
            let out = run_script(gate.rel, &root.0, &state, &envs);
            assert_eq!(out[gate.ok_key], Value::Bool(true), "{}: {out}", gate.rel);
            let output = str_field(&out, gate.output_key);
            assert!(
                output.ends_with(expected),
                "{} with project_dir {project_dir} and planned {planned} must run {expected}: {output}",
                gate.rel
            );
        }
        let mut envs = vec![("TEST_CMD", WHERE_FAILS)];
        if !cfg!(windows) {
            envs.push(("PATH", path_env.as_str()));
        }
        assert_baseline_ran_in(&root.0, &state, &envs, expected);
    }
}

/// The test_command variable is the command verify_tests and baseline_tests
/// run, ahead of TEST_CMD and detection.
#[test]
fn verify_tests_runs_the_test_command_variable_first() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("test-command");
    committed_project(&dir.0, "here");
    let state = serde_json::json!({ "project_dir": dir.0.to_string_lossy() });
    let out = run_script(
        "coder/scripts/verify_tests.sh",
        &dir.0,
        &state,
        &[
            ("LLM_AGENT_VAR_TEST_COMMAND", "echo from-variable"),
            ("TEST_CMD", "echo from-env"),
        ],
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    let output = str_field(&out, "tests_output");
    assert!(output.starts_with("Ran: echo from-variable\n"), "{output}");
    assert!(output.ends_with("from-variable"), "{output}");
    assert_baseline_ran_in(
        &dir.0,
        &state,
        &[
            (
                "LLM_AGENT_VAR_TEST_COMMAND",
                "printf 'test from-variable ... FAILED\\n'; exit 101",
            ),
            ("TEST_CMD", "printf 'test from-env ... FAILED\\n'; exit 101"),
        ],
        "from-variable",
    );
}

/// A gate that outlives its deadline is killed with its whole process group
/// and ends the run with a TIMED OUT gate_error: not a red run, so no fix
/// attempt is spent and no `Exit code:` verdict is reported. The grandchild
/// that would write `late.txt` after the deadline must die with the gate.
#[test]
fn verify_gates_report_a_deadline_kill_apart_from_a_red_run() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("deadline");
    committed_project(&dir.0, "here");
    let state = serde_json::json!({ "project_dir": dir.0.to_string_lossy() });
    let late = dir.0.join("late.txt");
    let cmd = "sh -c 'sleep 4; echo late > late.txt'; echo after > late.txt";
    for gate in &GATES {
        let started = Instant::now();
        let out = run_script(
            gate.rel,
            &dir.0,
            &state,
            &[(gate.cmd_env, cmd), (gate.timeout_var, "1")],
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{}: the deadline did not cut the gate short ({:?})",
            gate.rel,
            started.elapsed()
        );
        assert_eq!(out[gate.ok_key], Value::Bool(false), "{}: {out}", gate.rel);
        assert_eq!(str_field(&out, "_next"), "end_failure", "{}", gate.rel);
        let gate_error = str_field(&out, "gate_error");
        let headline = format!("{} TIMED OUT after 1 s ({})", gate.node, gate.timeout_name);
        assert!(
            gate_error.starts_with(&headline),
            "{}: {gate_error}",
            gate.rel
        );
        assert!(
            gate_error.contains("not a red run"),
            "{}: {gate_error}",
            gate.rel
        );
        let output = str_field(&out, gate.output_key);
        assert!(
            output.contains("\nTIMED OUT after 1 s"),
            "{}: {output}",
            gate.rel
        );
        assert!(!output.contains("Exit code:"), "{}: {output}", gate.rel);
    }
    let started = Instant::now();
    let out = run_script(
        BASELINE,
        &dir.0,
        &state,
        &[("TEST_CMD", cmd), ("LLM_AGENT_VAR_TEST_TIMEOUT", "1")],
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{BASELINE}: the deadline did not cut the gate short ({:?})",
        started.elapsed()
    );
    assert_eq!(str_field(&out, "baseline_status"), "timed_out", "{out}");
    assert_eq!(str_field(&out, "_next"), "implement", "{out}");
    assert!(
        str_field(&out, "baseline_note").contains("timed out after 1 s"),
        "{out}"
    );
    thread::sleep(Duration::from_secs(5));
    assert!(
        !late.exists(),
        "a child of a timed-out gate outlived the kill and wrote {}",
        late.display()
    );
}

/// Within the deadline (or with it disabled) the verdict is the command's
/// own; a deadline that is not whole seconds stops the run before the gate
/// runs anything.
#[test]
fn verify_gate_deadlines_keep_red_and_green_verdicts_and_refuse_bad_values() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("deadline-values");
    project(&dir.0, "here");
    let state = serde_json::json!({ "project_dir": dir.0.to_string_lossy() });
    for gate in &GATES {
        for limit in ["0", "30"] {
            let out = run_script(
                gate.rel,
                &dir.0,
                &state,
                &[(gate.cmd_env, "echo fine"), (gate.timeout_var, limit)],
            );
            assert_eq!(
                out[gate.ok_key],
                Value::Bool(true),
                "{} at {limit}: {out}",
                gate.rel
            );
            let out = run_script(
                gate.rel,
                &dir.0,
                &state,
                &[(gate.cmd_env, "exit 3"), (gate.timeout_var, limit)],
            );
            assert_eq!(
                out[gate.ok_key],
                Value::Bool(false),
                "{} at {limit}: {out}",
                gate.rel
            );
            assert_eq!(str_field(&out, "_next"), "fix_loop_gate", "{}", gate.rel);
            assert!(str_field(&out, gate.output_key).contains("Exit code: 3"));
        }
        let out = run_script(
            gate.rel,
            &dir.0,
            &state,
            &[(gate.cmd_env, "touch ran.txt"), (gate.timeout_var, "ten")],
        );
        assert_eq!(out[gate.ok_key], Value::Bool(false), "{}: {out}", gate.rel);
        assert_eq!(str_field(&out, "_next"), "end_failure", "{}", gate.rel);
        assert!(
            str_field(&out, "gate_error").contains(gate.timeout_name),
            "{}: {out}",
            gate.rel
        );
        assert!(
            !dir.0.join("ran.txt").exists(),
            "{} ran with a bad deadline",
            gate.rel
        );
    }
}

/// The engine's own node timeout (it kills the script, then takes the
/// fallback edge) is a timeout too: fix_loop_gate ends the run with a
/// TIMED OUT gate_error instead of re-running the gate as a crash.
#[test]
fn fix_loop_gate_ends_an_engine_timeout_as_timed_out_not_a_crash() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("node-timeout");
    for node in ["verify_build", "verify_tests"] {
        let error = format!(
            "script '{node}' failed: Script '/agents/coder/scripts/{node}.sh' timed out after 3600s"
        );
        let state = serde_json::json!({
            "fix_attempts": 0,
            "max_fix_attempts": 5,
            "build_ok": true,
            "tests_ok": true,
            "gate_retries": 0,
            "last_script_error": error,
        });
        let out = run_script("coder/scripts/fix_loop_gate.sh", &dir.0, &state, &[]);
        assert_eq!(str_field(&out, "_next"), "end_failure", "{node}: {out}");
        let gate_error = str_field(&out, "gate_error");
        assert!(gate_error.starts_with("TIMED OUT"), "{node}: {gate_error}");
        assert!(gate_error.ends_with(&error), "{node}: {gate_error}");
        assert_eq!(str_field(&out, "last_script_error"), "", "{node}");
    }
}

/// The deadline variables are declared, default below their node's timeout
/// (the gate's own kill must come first, or the engine kills only the shell
/// and leaves its children running), and the prose quotes the figures.
#[test]
fn coder_graph_declares_deadlines_below_the_gate_node_timeouts() {
    let graph = graph();
    let readme = fs::read_to_string(asset("coder/README.md")).unwrap();
    for (node, var) in [
        ("verify_build", "build_timeout"),
        ("verify_tests", "test_timeout"),
    ] {
        let node_timeout = graph["nodes"][node]["timeout"]
            .as_u64()
            .unwrap_or_else(|| panic!("nodes.{node}.timeout"));
        let declared = variable(&graph, var);
        let default: u64 = declared["default"]
            .as_str()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("{var}: default must be a quoted whole number"));
        assert!(
            0 < default && default < node_timeout,
            "{var} default {default} must sit below nodes.{node}.timeout {node_timeout}"
        );
        let figure = format!("{node_timeout} s");
        assert!(
            declared["description"].as_str().unwrap().contains(&figure),
            "{var} description must quote {figure}"
        );
        assert!(
            readme.contains(&format!("`{var}` (default `{default}`)")),
            "coder/README.md must give the {var} default {default}"
        );
    }
    assert_eq!(
        variable(&graph, "test_command")["default"].as_str(),
        Some("")
    );
    assert_eq!(
        graph["initial_state"]["project_dir_defaulted"],
        serde_yaml::Value::Bool(false)
    );
}

const BASELINE: &str = "coder/scripts/baseline_tests.sh";
const VERIFY_TESTS: &str = "coder/scripts/verify_tests.sh";

/// The keys baseline_tests may write: ids, hash and note, never a transcript
/// and never the implementer's `last_node_output`.
const BASELINE_KEYS: [&str; 6] = [
    "_next",
    "baseline_exit_code",
    "baseline_failure_hash",
    "baseline_failures",
    "baseline_note",
    "baseline_status",
];

/// Runs `rel` with a TEST_CMD that prints `transcript` and exits `rc`. Each
/// call reads its own file, so the command differs per call and a baseline
/// stored for an earlier transcript is never reused for a later one.
fn run_transcript(rel: &str, dir: &Path, state: &Value, transcript: &str, rc: i32) -> Value {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let name = format!("transcript-{}.txt", CALLS.fetch_add(1, Ordering::Relaxed));
    fs::write(dir.join(&name), transcript).unwrap();
    let cmd = format!("cat {name}; exit {rc}");
    run_script(rel, dir, state, &[("TEST_CMD", cmd.as_str())])
}

fn project_state(dir: &Path) -> Value {
    serde_json::json!({ "project_dir": dir.to_string_lossy() })
}

fn assert_baseline_shape(out: &Value) {
    let mut keys: Vec<&str> = out
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, BASELINE_KEYS, "{out}");
    assert_eq!(str_field(out, "_next"), "implement", "{out}");
}

fn baseline_ids(out: &Value) -> Vec<&str> {
    out["baseline_failures"]
        .as_array()
        .unwrap_or_else(|| panic!("baseline_failures must be an array: {out}"))
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}

/// A test command whose one failing cargo test id is the `where.txt` of the
/// directory it ran in: baseline_tests keeps no transcript, so the id is
/// what shows where it ran.
const WHERE_FAILS: &str = "printf 'test %s ... FAILED\\n' \"$(cat where.txt)\"; exit 101";

fn assert_baseline_ran_in(dir: &Path, state: &Value, envs: &[(&str, &str)], expected: &str) {
    let out = run_script(BASELINE, dir, state, envs);
    assert_eq!(str_field(&out, "baseline_status"), "red", "{out}");
    assert_eq!(baseline_ids(&out), [expected], "{out}");
}

/// Verify state carrying a red baseline of `ids` at exit code `rc`.
fn red_baseline(dir: &Path, ids: &[&str], rc: i32, hash: &str) -> Value {
    serde_json::json!({
        "project_dir": dir.to_string_lossy(),
        "baseline_status": "red",
        "baseline_exit_code": rc,
        "baseline_failures": ids,
        "baseline_failure_hash": hash,
    })
}

/// A pytest short summary failing `ids`.
fn pytest_failing(ids: &[&str]) -> String {
    let mut text = String::from("==== short test summary info ====\n");
    for id in ids {
        text.push_str(&format!("FAILED {id} - AssertionError: assert 1 == 2\n"));
    }
    text.push_str(&format!("==== {} failed in 0.12s ====\n", ids.len()));
    text
}

/// Without a test command there is nothing to run: no baseline, the run
/// goes on to implement, and verify_tests keeps its plain verdict. A
/// test_timeout that is not whole seconds records no baseline either.
#[test]
fn baseline_tests_without_a_test_command_records_no_baseline() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("baseline-none");
    project(&dir.0, "here");
    let out = run_script(BASELINE, &dir.0, &project_state(&dir.0), &[]);
    assert_baseline_shape(&out);
    assert_eq!(str_field(&out, "baseline_status"), "none", "{out}");
    assert!(baseline_ids(&out).is_empty(), "{out}");

    let out = run_script(
        BASELINE,
        &dir.0,
        &project_state(&dir.0),
        &[
            ("TEST_CMD", "touch ran.txt"),
            ("LLM_AGENT_VAR_TEST_TIMEOUT", "ten"),
        ],
    );
    assert_baseline_shape(&out);
    assert_eq!(str_field(&out, "baseline_status"), "none", "{out}");
    assert!(
        str_field(&out, "baseline_note").contains("test_timeout"),
        "{out}"
    );
    assert!(!dir.0.join("ran.txt").exists(), "ran with a bad deadline");
}

/// A red baseline records the exit code, the sorted unique failing test
/// ids each supported runner prints (ANSI colour and CRLF stripped), a
/// failure hash and a note naming the ids.
#[test]
fn baseline_tests_records_the_failing_test_ids_per_runner() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("baseline-ids");
    committed_project(&dir.0, "here");
    let jest = "\u{25cf}";
    let sep = "\u{203a}";
    let cases: Vec<(&str, String, Vec<&str>)> = vec![
        (
            "unittest",
            "\x1b[31mFAIL: test_tier_boundary_is_inclusive (tests.test_invoicer.TotalsTests.test_tier_boundary_is_inclusive)\x1b[0m\r\n\
             ----------------------------------------------------------------------\r\n\
             Traceback (most recent call last):\r\n\
             AssertionError: 10 != 11\r\n\
             ERROR: test_old (tests.test_legacy.LegacyTests)\r\n\
             Ran 2 tests in 0.003s\r\n\
             \r\n\
             FAILED (failures=1, errors=1)\r\n"
                .to_string(),
            vec![
                "tests.test_invoicer.TotalsTests.test_tier_boundary_is_inclusive",
                "tests.test_legacy.LegacyTests.test_old",
            ],
        ),
        (
            "pytest",
            "==== short test summary info ====\n\
             FAILED tests/test_a.py::test_one - AssertionError: assert 1 == 2\n\
             FAILED tests/test_a.py::TestK::test_two[param-1]\n\
             ERROR tests/test_b.py::test_three - RuntimeError: boom\n\
             FAILED tests/test_a.py::test_one - AssertionError: assert 1 == 2\n\
             ==== 2 failed, 1 error in 0.12s ====\n"
                .to_string(),
            vec![
                "tests/test_a.py::TestK::test_two[param-1]",
                "tests/test_a.py::test_one",
                "tests/test_b.py::test_three",
            ],
        ),
        (
            "cargo",
            "running 3 tests\n\
             test parser::tests::ok ... ok\n\
             test parser::tests::bad_input ... FAILED\n\
             test cli::it_runs ... FAILED\n\
             \n\
             failures:\n\
             ---- parser::tests::bad_input stdout ----\n\
             thread 'parser::tests::bad_input' panicked at src/parser.rs:10:5:\n\
             failures:\n    cli::it_runs\n    parser::tests::bad_input\n\
             \n\
             test result: FAILED. 1 passed; 2 failed; 0 ignored; finished in 0.01s\n"
                .to_string(),
            vec!["cli::it_runs", "parser::tests::bad_input"],
        ),
        (
            "go",
            "=== RUN   TestAdd\n\
             --- FAIL: TestAdd (0.00s)\n\
             \x20   math_test.go:9: got 3 want 4\n\
             === RUN   TestSub/negative\n\
             \x20   --- FAIL: TestSub/negative (0.00s)\n\
             FAIL\n\
             FAIL\texample.com/m/math\t0.002s\n\
             FAIL\texample.com/m/broken [build failed]\n\
             FAIL\n"
                .to_string(),
            vec![
                "TestAdd",
                "TestSub/negative",
                "pkg:example.com/m/broken",
                "pkg:example.com/m/math",
            ],
        ),
        (
            "jest",
            format!(
                " FAIL  src/cart.test.js\n\
                 \x20 Cart\n\
                 \x20   x adds items (5 ms)\n\
                 \x20 {jest} Cart {sep} adds items\n\
                 \n\
                 \x20   expect(received).toBe(expected)\n\
                 \n\
                 \x20 {jest} Cart {sep} totals {sep} rounds down\n\
                 \n\
                 \x20 {jest} Console\n\
                 \n\
                 \x20   console.log\n"
            ),
            vec![
                "src/cart.test.js > Cart \u{203a} adds items",
                "src/cart.test.js > Cart \u{203a} totals \u{203a} rounds down",
            ],
        ),
        (
            "vitest",
            "\x1b[31m FAIL \x1b[39m src/a.test.ts > math > adds\n\
             \x20FAIL  src/b.test.ts\n\
             AssertionError: expected 1 to be 2\n\
             \x20FAIL  src/a.test.ts > math > nested > subtracts\n"
                .to_string(),
            vec![
                "src/a.test.ts > math > adds",
                "src/a.test.ts > math > nested > subtracts",
                "src/b.test.ts",
            ],
        ),
    ];
    for (runner, transcript, expected) in &cases {
        let out = run_transcript(BASELINE, &dir.0, &project_state(&dir.0), transcript, 1);
        assert_baseline_shape(&out);
        assert_eq!(str_field(&out, "baseline_status"), "red", "{runner}: {out}");
        assert_eq!(out["baseline_exit_code"], Value::from(1), "{runner}: {out}");
        assert_eq!(&baseline_ids(&out), expected, "{runner}: {out}");
        assert!(
            !str_field(&out, "baseline_failure_hash").is_empty(),
            "{runner}: {out}"
        );
        let note = str_field(&out, "baseline_note");
        assert!(note.contains("(exit 1)"), "{runner}: {note}");
        for id in expected {
            assert!(note.lines().any(|l| l == *id), "{runner}: {note}");
        }
    }
}

/// A green baseline records no failures; a baseline killed at test_timeout
/// records none either, is not stored, and still hands over to implement.
#[test]
fn baseline_tests_records_green_and_timed_out_runs_without_failures() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("baseline-green");
    committed_project(&dir.0, "here");
    let out = run_transcript(BASELINE, &dir.0, &project_state(&dir.0), "ok\n", 0);
    assert_baseline_shape(&out);
    assert_eq!(str_field(&out, "baseline_status"), "green", "{out}");
    assert!(baseline_ids(&out).is_empty(), "{out}");

    let started = Instant::now();
    let out = run_script(
        BASELINE,
        &dir.0,
        &project_state(&dir.0),
        &[("TEST_CMD", "sleep 5"), ("LLM_AGENT_VAR_TEST_TIMEOUT", "1")],
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "deadline ignored"
    );
    assert_baseline_shape(&out);
    assert_eq!(str_field(&out, "baseline_status"), "timed_out", "{out}");
    assert!(baseline_ids(&out).is_empty(), "{out}");
    assert_eq!(
        stored_records(&dir.0).len(),
        1,
        "only the green run is stored"
    );
}

/// Failures that are a subset of the baseline at the baseline's exit code
/// pass straight to self_review (no fix attempt) and are reported as
/// pre-existing, ahead of the transcript.
#[test]
fn verify_tests_passes_a_run_whose_failures_are_all_in_the_baseline() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-subset");
    project(&dir.0, "here");
    let a = "tests/test_a.py::test_one";
    let state = red_baseline(&dir.0, &[a], 1, "");
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, &pytest_failing(&[a]), 1);
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(str_field(&out, "_next"), "self_review", "{out}");
    let report = str_field(&out, "baseline_report");
    assert_eq!(report, format!("\nPre-existing failures: {a}"), "{out}");
    assert!(
        str_field(&out, "tests_output")
            .starts_with(&format!("Pre-existing failures: {a}\n\nRan: ")),
        "{out}"
    );
}

/// A failure the baseline did not have fails the run, and the transcript
/// names which failures are new and which were there before.
#[test]
fn verify_tests_fails_a_new_failure_on_top_of_the_baseline() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-new");
    project(&dir.0, "here");
    let (a, c) = ("tests/test_a.py::test_one", "tests/test_c.py::test_new");
    let state = red_baseline(&dir.0, &[a], 1, "");
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, &pytest_failing(&[a, c]), 1);
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert_eq!(str_field(&out, "_next"), "fix_loop_gate", "{out}");
    let output = str_field(&out, "tests_output");
    assert!(output.contains("\nExit code: 1\n"), "{output}");
    assert!(
        output.contains(&format!("\nNew failures: {c}\n")),
        "{output}"
    );
    assert!(
        output.contains(&format!("\nPre-existing failures: {a}\n")),
        "{output}"
    );
}

/// Output with no identifiable test ids is matched by its failure hash:
/// the same failures (timings and addresses aside) pass with the
/// unidentified report line, different ones fail.
#[test]
fn verify_tests_matches_unidentified_failures_by_their_hash() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-hash");
    committed_project(&dir.0, "here");
    let before = "compiling\nerror: linker failed at 0x7ffd1234 after 0.52s\nsomething else\n";
    let base = run_transcript(BASELINE, &dir.0, &project_state(&dir.0), before, 1);
    assert_eq!(str_field(&base, "baseline_status"), "red", "{base}");
    assert!(baseline_ids(&base).is_empty(), "{base}");
    let state = red_baseline(&dir.0, &[], 1, str_field(&base, "baseline_failure_hash"));

    let same = "error: linker failed at 0x7ffe9999 after 1.03s\nother noise\n";
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, same, 1);
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(str_field(&out, "_next"), "self_review", "{out}");
    assert_eq!(
        str_field(&out, "baseline_report"),
        "\nPre-existing failures: unidentified (no test ids could be parsed), output matches the baseline (exit 1)",
        "{out}"
    );

    let changed = "error: linker failed: missing symbol foo\n";
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, changed, 1);
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert_eq!(str_field(&out, "_next"), "fix_loop_gate", "{out}");
}

/// Baseline failures that now pass are reported as fixed: all of them on a
/// green run, and on a run that still fails a subset the ones it shows
/// passing. One that is merely absent from a red run is only no longer
/// reported failing.
#[test]
fn verify_tests_reports_baseline_failures_that_now_pass_as_fixed() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-fixed");
    project(&dir.0, "here");
    let (a, b) = ("tests/test_a.py::test_one", "tests/test_b.py::test_two");
    let out = run_transcript(
        VERIFY_TESTS,
        &dir.0,
        &red_baseline(&dir.0, &[a], 1, ""),
        "all good\n",
        0,
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(
        str_field(&out, "baseline_report"),
        format!("\nFixed pre-existing failures: {a}"),
        "{out}"
    );

    let out = run_transcript(
        VERIFY_TESTS,
        &dir.0,
        &red_baseline(&dir.0, &[], 1, "unmatched"),
        "all good\n",
        0,
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(
        str_field(&out, "baseline_report"),
        "\nFixed pre-existing failures: unidentified, the suite now passes",
        "{out}"
    );

    let out = run_transcript(
        VERIFY_TESTS,
        &dir.0,
        &red_baseline(&dir.0, &[a, b], 1, ""),
        &pytest_failing(&[a]),
        1,
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(str_field(&out, "_next"), "self_review", "{out}");
    assert_eq!(
        str_field(&out, "baseline_report"),
        format!("\nPre-existing failures: {a}\nNo longer reported failing: {b}"),
        "{out}"
    );

    let shown_passing = format!("{b} PASSED [ 50%]\n{}", pytest_failing(&[a]));
    let out = run_transcript(
        VERIFY_TESTS,
        &dir.0,
        &red_baseline(&dir.0, &[a, b], 1, ""),
        &shown_passing,
        1,
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(
        str_field(&out, "baseline_report"),
        format!("\nPre-existing failures: {a}\nFixed pre-existing failures: {b}"),
        "{out}"
    );
}

/// The same ids at a different exit code are not the baseline's failures
/// (a crash on top of them, say): the run fails.
#[test]
fn verify_tests_fails_when_the_exit_code_differs_from_the_baseline() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-exit");
    project(&dir.0, "here");
    let a = "tests/test_a.py::test_one";
    let state = red_baseline(&dir.0, &[a], 1, "");
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, &pytest_failing(&[a]), 2);
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert_eq!(str_field(&out, "_next"), "fix_loop_gate", "{out}");
    assert!(
        str_field(&out, "tests_output").contains("\nBaseline exit code: 1"),
        "{out}"
    );
}

/// A baseline with known ids never passes a run whose failures cannot be
/// identified: unknown means fail.
#[test]
fn verify_tests_fails_an_unidentified_run_against_an_identified_baseline() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-unknown");
    project(&dir.0, "here");
    let state = red_baseline(&dir.0, &["tests/test_a.py::test_one"], 1, "");
    let compile_error = "error[E0425]: cannot find value `x` in this scope\n";
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, compile_error, 1);
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert_eq!(str_field(&out, "_next"), "fix_loop_gate", "{out}");
    assert!(
        str_field(&out, "tests_output").contains("No failing test could be identified"),
        "{out}"
    );
}

/// On a red test run the fix instructions name the baseline failures as
/// pre-existing, and say nothing of a baseline when there is none.
#[test]
fn fix_loop_gate_lists_pre_existing_failures_in_fix_instructions() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("fix-baseline");
    let mut state = serde_json::json!({
        "fix_attempts": 0,
        "max_fix_attempts": 5,
        "build_ok": true,
        "tests_ok": false,
        "tests_output": "Exit code: 1",
        "baseline_failures": ["tests/a.py::one", "tests/b.py::two"],
    });
    let out = run_script("coder/scripts/fix_loop_gate.sh", &dir.0, &state, &[]);
    assert_eq!(str_field(&out, "_next"), "implement", "{out}");
    let instructions = str_field(&out, "fix_instructions");
    assert!(
        instructions
            .contains("already failing before you started: tests/a.py::one, tests/b.py::two."),
        "{instructions}"
    );

    state["baseline_failures"] = serde_json::json!([]);
    let out = run_script("coder/scripts/fix_loop_gate.sh", &dir.0, &state, &[]);
    let instructions = str_field(&out, "fix_instructions");
    assert!(
        !instructions.contains("before you started"),
        "{instructions}"
    );

    let many: Vec<String> = (1..=23).map(|n| format!("t{n:02}")).collect();
    state["baseline_failures"] = serde_json::json!(many);
    let out = run_script("coder/scripts/fix_loop_gate.sh", &dir.0, &state, &[]);
    let instructions = str_field(&out, "fix_instructions");
    assert!(
        instructions.contains("t19, t20, (+3 more)."),
        "{instructions}"
    );
    assert!(!instructions.contains("t21"), "{instructions}");
}

/// route_complexity sends every plan that skips approval to the baseline.
#[test]
fn route_complexity_routes_into_the_baseline() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("baseline-wiring");
    for (score, autoapprove, expected) in [
        (2, "0", "baseline_tests"),
        (9, "1", "baseline_tests"),
        (9, "0", "gate_approval"),
    ] {
        let state = serde_json::json!({ "complexity_score": score });
        let out = run_script(
            "coder/scripts/route_complexity.sh",
            &dir.0,
            &state,
            &[("CODER_AUTOAPPROVE", autoapprove)],
        );
        assert_eq!(str_field(&out, "_next"), expected, "score {score}");
    }
}

/// The baseline runs once, between approval and implement: every route
/// into implement from the planning side goes through it, a crash of it
/// falls through to implement, and its outputs reach the prompts.
#[test]
fn coder_graph_runs_the_baseline_once_before_implement() {
    let graph = graph();
    let nodes = &graph["nodes"];
    assert_eq!(
        nodes["gate_approval"]["routes"]["yes"].as_str(),
        Some("baseline_tests")
    );
    let node = &nodes["baseline_tests"];
    assert_eq!(node["type"].as_str(), Some("script"));
    assert_eq!(node["script"].as_str(), Some("scripts/baseline_tests.sh"));
    assert_eq!(node["fallback"].as_str(), Some("implement"));
    assert_eq!(
        node["timeout"].as_u64(),
        nodes["verify_tests"]["timeout"].as_u64()
    );
    assert!(
        nodes["implement"]["prompt"]
            .as_str()
            .unwrap()
            .contains("{{baseline_note}}")
    );
    assert!(
        nodes["end_success"]["output"]
            .as_str()
            .unwrap()
            .contains("Tests: {{tests_verdict}}{{baseline_report}}\nReview: ")
    );
    for key in [
        "baseline_status",
        "baseline_exit_code",
        "baseline_failures",
        "baseline_failure_hash",
        "baseline_note",
        "baseline_report",
    ] {
        assert!(
            !graph["initial_state"][key].is_null(),
            "initial_state lacks {key}"
        );
    }
    assert_eq!(
        graph["initial_state"]["baseline_note"].as_str(),
        Some("No baseline was recorded.")
    );
}

/// Only gate_approval (by its routes), and route_complexity and
/// parse_approval (by the `_next` they print) lead into baseline_tests, and
/// baseline_tests declares no `next` or `routes` of its own: it runs once,
/// before implement.
#[test]
fn only_the_approval_side_routes_into_the_baseline() {
    let graph = graph();
    let mut into = Vec::new();
    for (name, node) in graph["nodes"].as_mapping().unwrap() {
        let mut targets: Vec<&serde_yaml::Value> = ["next", "fallback", "on_other"]
            .iter()
            .map(|key| &node[*key])
            .collect();
        if let Some(routes) = node["routes"].as_mapping() {
            targets.extend(routes.values());
        }
        if targets.iter().any(|t| t.as_str() == Some("baseline_tests")) {
            into.push(name.as_str().unwrap().to_string());
        }
    }
    assert_eq!(into, ["gate_approval"]);

    let mut scripts = Vec::new();
    for entry in fs::read_dir(asset("coder/scripts")).unwrap() {
        let path = entry.unwrap().path();
        let text: String = fs::read_to_string(&path)
            .unwrap()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if text.contains("\"_next\":\"baseline_tests\"") {
            scripts.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    scripts.sort_unstable();
    assert_eq!(scripts, ["parse_approval.sh", "route_complexity.sh"]);

    let node = &graph["nodes"]["baseline_tests"];
    assert!(node["next"].is_null(), "baseline_tests declares a next");
    assert!(node["routes"].is_null(), "baseline_tests declares routes");
}

/// With no report the `Tests:` line is followed by the `Review:` line; a
/// report lands on its own lines between them.
#[test]
fn end_success_renders_the_baseline_report_after_tests_passed() {
    let template = graph()["nodes"]["end_success"]["output"]
        .as_str()
        .unwrap()
        .replace("{{tests_verdict}}", "passed");
    let plain = template.replace("{{baseline_report}}", "");
    assert!(plain.contains("\nTests: passed\nReview: "), "{plain}");
    let reported = template.replace(
        "{{baseline_report}}",
        "\nPre-existing failures: tests/a.py::one",
    );
    assert!(
        reported.contains("Tests: passed\nPre-existing failures: tests/a.py::one\nReview: "),
        "{reported}"
    );
}

/// Without a red baseline verify_tests reports nothing, so end_success
/// renders exactly the layout it had before baselines existed.
#[test]
fn verify_tests_reports_nothing_without_a_baseline() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-no-baseline");
    project(&dir.0, "here");
    for (cmd, ok) in [("echo fine", true), ("echo boom; exit 1", false)] {
        let out = run_script(
            VERIFY_TESTS,
            &dir.0,
            &project_state(&dir.0),
            &[("TEST_CMD", cmd)],
        );
        assert_eq!(out["tests_ok"], Value::Bool(ok), "{cmd}: {out}");
        if ok {
            assert_eq!(str_field(&out, "baseline_report"), "", "{cmd}: {out}");
            assert!(
                str_field(&out, "tests_output").starts_with("Ran: "),
                "{out}"
            );
        } else {
            assert!(out.get("baseline_report").is_none(), "{cmd}: {out}");
        }
    }
}

/// A crash of baseline_tests is not a verify gate crash: with a red test run
/// fix_loop_gate still spends a fix attempt, and with both gates green it
/// takes the same "no error recorded" retry it takes without any error.
#[test]
fn fix_loop_gate_does_not_treat_a_baseline_tests_crash_as_a_gate_crash() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("fix-baseline-crash");
    let mut state = serde_json::json!({
        "fix_attempts": 0,
        "max_fix_attempts": 5,
        "build_ok": true,
        "tests_ok": false,
        "tests_output": "Exit code: 1",
        "gate_retries": 0,
        "last_script_error": "script 'baseline_tests' failed: boom",
    });
    let out = run_script("coder/scripts/fix_loop_gate.sh", &dir.0, &state, &[]);
    assert_eq!(str_field(&out, "_next"), "implement", "{out}");
    assert_eq!(out["fix_attempts"], Value::from(1), "{out}");
    assert_eq!(str_field(&out, "gate_error"), "", "{out}");
    assert_eq!(out["gate_retries"], Value::from(0), "{out}");

    state["tests_ok"] = Value::Bool(true);
    let out = run_script("coder/scripts/fix_loop_gate.sh", &dir.0, &state, &[]);
    assert_eq!(str_field(&out, "_next"), "verify_build", "{out}");
    assert_eq!(
        str_field(&out, "gate_error"),
        "no error recorded in state",
        "{out}"
    );
    assert_eq!(out["gate_retries"], Value::from(1), "{out}");
}

/// Cargo ids carry their target (backslashes as `/`), jest ids their file,
/// a jest or vitest file that failed without a failing test of its own is
/// recorded by name, and pytest ids keep their spaces up to the ` - `
/// message.
#[test]
fn baseline_tests_qualifies_ids_by_target_and_file() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("baseline-qualified");
    committed_project(&dir.0, "here");
    let jest = "\u{25cf}";
    let sep = "\u{203a}";
    let cases: Vec<(&str, String, Vec<&str>)> = vec![
        (
            "cargo",
            "Running unittests src/lib.rs (target/debug/deps/x-1)\n\
             test a::bad ... FAILED\n\
             test result: FAILED. 0 passed; 1 failed; 0 ignored\n\
             Running tests\\foo.rs (target\\debug\\deps\\foo-2)\n\
             test bad ... FAILED\n\
             test result: FAILED. 0 passed; 1 failed; 0 ignored\n\
             Doc-tests x\n\
             test src/lib.rs - f (line 3) ... FAILED\n\
             test result: FAILED. 0 passed; 1 failed; 0 ignored\n"
                .to_string(),
            vec![
                "Doc-tests x::src/lib.rs - f (line 3)",
                "tests/foo.rs::bad",
                "unittests src/lib.rs::a::bad",
            ],
        ),
        (
            "jest",
            format!(
                " FAIL  src/a.test.js (5.2 s)\n\
                 \x20 {jest} suite {sep} breaks\n\
                 \x20FAIL  src/b.test.js\n\
                 \x20 {jest} Test suite failed to run\n\
                 Test Suites: 2 failed, 2 total\n\
                 Tests:       1 failed, 1 total\n"
            ),
            vec!["src/a.test.js > suite \u{203a} breaks", "src/b.test.js"],
        ),
        (
            "vitest",
            " FAIL  src/a.test.ts > m > adds\n\
             \x20FAIL  src/a.test.ts [ src/a.test.ts ]\n\
             \x20FAIL  src/c.test.ts [ src/c.test.ts ]\n\
             \x20Test Files  2 failed | 1 passed (3)\n\
             \x20     Tests  1 failed | 4 passed (5)\n"
                .to_string(),
            vec!["src/a.test.ts > m > adds", "src/c.test.ts"],
        ),
        (
            "pytest",
            "FAILED tests/t.py::test_x[a b] - assert 1 == 2\n\
             FAILED tests/t.py::test_z[x y]\n\
             ==== 2 failed in 0.10s ====\n"
                .to_string(),
            vec!["tests/t.py::test_x[a b]", "tests/t.py::test_z[x y]"],
        ),
    ];
    for (runner, transcript, expected) in &cases {
        let out = run_transcript(BASELINE, &dir.0, &project_state(&dir.0), transcript, 1);
        assert_eq!(str_field(&out, "baseline_status"), "red", "{runner}: {out}");
        assert_eq!(&baseline_ids(&out), expected, "{runner}: {out}");
    }
}

/// A cargo test target killed by a signal (its remaining tests never ran).
const SIGNAL_CRASH: &str = "Running tests/foo.rs (target/debug/deps/foo-1)\n\
     test a ... FAILED\n\
     error: test failed, to rerun pass `--test foo`\n\
     \n\
     Caused by:\n\
     \x20 process didn't exit successfully: `/x/foo-1` (signal: 6, SIGABRT: process abort signal)\n";

/// Failures are unidentified, with the reason in the note, when no id was
/// read, when the runner's own failure total exceeds the ids read, and
/// when a test target was killed by a signal. Only the hash is kept.
#[test]
fn baseline_tests_records_why_failures_are_unidentified() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("baseline-unidentified");
    committed_project(&dir.0, "here");
    let short = "the runner reported 2 failures but only 1 ids were parsed";
    let cases: [(&str, &str, &str); 9] = [
        ("none", "boom error\n", "no test ids could be parsed"),
        (
            "pytest",
            "FAILED tests/t.py::a - x\n==== 2 failed in 0.10s ====\n",
            short,
        ),
        (
            "cargo",
            "test a ... FAILED\n\
             test result: FAILED. 0 passed; 1 failed;\n\
             test result: FAILED. 0 passed; 1 failed;\n",
            short,
        ),
        (
            "jest",
            " FAIL  src/a.test.js\n  \u{25cf} a\nTests:       2 failed, 2 total\n",
            short,
        ),
        (
            "jest suites",
            " FAIL  src/a.test.js\n  \u{25cf} a\nTest Suites: 2 failed, 2 total\n",
            short,
        ),
        (
            "vitest",
            " FAIL  src/a.test.ts > m > adds\n      Tests  2 failed | 1 passed (3)\n",
            short,
        ),
        (
            "vitest files",
            " FAIL  src/a.test.ts > m > adds\n Test Files  2 failed | 1 passed (3)\n",
            short,
        ),
        (
            "unittest",
            "FAIL: test_a (tests.T)\nFAILED (failures=2)\n",
            short,
        ),
        (
            "signal",
            SIGNAL_CRASH,
            "a test target was killed by a signal",
        ),
    ];
    for (runner, transcript, reason) in cases {
        let out = run_transcript(BASELINE, &dir.0, &project_state(&dir.0), transcript, 1);
        assert_eq!(str_field(&out, "baseline_status"), "red", "{runner}: {out}");
        assert!(baseline_ids(&out).is_empty(), "{runner}: {out}");
        assert!(
            !str_field(&out, "baseline_failure_hash").is_empty(),
            "{runner}: {out}"
        );
        assert!(
            str_field(&out, "baseline_note")
                .contains(&format!("could not be identified ({reason})")),
            "{runner}: {out}"
        );
    }
}

/// A run in which a test target was killed by a signal never passes, even
/// when every failure it printed was in the baseline. Without a baseline it
/// fails on its exit code alone, with no sentence about the baseline.
#[test]
fn verify_tests_never_passes_a_test_target_killed_by_a_signal() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-signal");
    project(&dir.0, "here");
    let state = red_baseline(&dir.0, &["tests/foo.rs::a"], 101, "");
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, SIGNAL_CRASH, 101);
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert_eq!(str_field(&out, "_next"), "fix_loop_gate", "{out}");
    assert!(
        str_field(&out, "tests_output").contains("\nA test target was killed by a signal"),
        "{out}"
    );
    let out = run_transcript(
        VERIFY_TESTS,
        &dir.0,
        &project_state(&dir.0),
        SIGNAL_CRASH,
        101,
    );
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert!(
        !str_field(&out, "tests_output").contains("A test target was killed by a signal"),
        "{out}"
    );
}

/// Both gates run every `cargo test` and `cargo t` with --no-fail-fast
/// before any `--`, so later targets still run after one fails; the `Ran:`
/// line and the transcript show the command as run, and a command that has
/// the flag already, or another subcommand such as `testx`, is unchanged.
#[test]
fn test_gates_run_cargo_test_with_no_fail_fast() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("no-fail-fast");
    committed_project(&dir.0, "here");
    let fake = "cargo() { printf 'test %s ... FAILED\\n' \"$*\"; return 101; }; ";
    let cases: [(&str, &str, &[&str]); 6] = [
        (
            "cargo test -p a -- --nocapture",
            "cargo test --no-fail-fast -p a -- --nocapture",
            &["test --no-fail-fast -p a -- --nocapture"],
        ),
        (
            "cargo +nightly test",
            "cargo +nightly test --no-fail-fast",
            &["+nightly test --no-fail-fast"],
        ),
        (
            "cargo test --no-fail-fast",
            "cargo test --no-fail-fast",
            &["test --no-fail-fast"],
        ),
        ("cargo t", "cargo t --no-fail-fast", &["t --no-fail-fast"]),
        ("cargo testx", "cargo testx", &["testx"]),
        (
            "cargo test -p a; cargo test -p b",
            "cargo test --no-fail-fast -p a; cargo test --no-fail-fast -p b",
            &["test --no-fail-fast -p a", "test --no-fail-fast -p b"],
        ),
    ];
    for (cmd, ran, ids) in cases {
        let full = format!("{fake}{cmd}");
        let envs = [("TEST_CMD", full.as_str())];
        let out = run_script(BASELINE, &dir.0, &project_state(&dir.0), &envs);
        assert_eq!(baseline_ids(&out), ids, "{cmd}: {out}");
        let out = run_script(VERIFY_TESTS, &dir.0, &project_state(&dir.0), &envs);
        let output = str_field(&out, "tests_output");
        assert!(
            output.starts_with(&format!("Ran: {fake}{ran}\n")),
            "{cmd}: {out}"
        );
        for id in ids {
            let line = format!("test {id} ... FAILED");
            assert!(output.lines().any(|l| l == line), "{cmd}: {out}");
        }
    }
}

/// A cargo test target that failed under fail-fast: cargo printed its rerun
/// hint and stopped, so later targets never ran.
const CARGO_FAIL_FAST: &str = "     Running tests/a.rs (target/debug/deps/a-123)\n\
     test a1 ... FAILED\n\
     test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\
     error: test failed, to rerun pass `--test a`\n";

/// A wrapper that runs fail-fast cargo hides it from the --no-fail-fast
/// rewrite, so a transcript showing cargo stopped at its first failing
/// target is unidentified in both gates. With the closing `1 target failed:`
/// summary that --no-fail-fast prints, the same failure is identified.
#[test]
fn test_gates_treat_cargo_stopped_at_the_first_failing_target_as_unidentified() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("cargo-fail-fast");
    committed_project(&dir.0, "here");
    let out = run_transcript(
        BASELINE,
        &dir.0,
        &project_state(&dir.0),
        CARGO_FAIL_FAST,
        101,
    );
    assert_eq!(str_field(&out, "baseline_status"), "red", "{out}");
    assert!(baseline_ids(&out).is_empty(), "{out}");
    assert!(
        str_field(&out, "baseline_note")
            .contains("could not be identified (cargo stopped at the first failing target)"),
        "{out}"
    );

    let no_fail_fast = format!("{CARGO_FAIL_FAST}error: 1 target failed:\n    `--test a`\n");
    let out = run_transcript(BASELINE, &dir.0, &project_state(&dir.0), &no_fail_fast, 101);
    assert_eq!(baseline_ids(&out), ["tests/a.rs::a1"], "{out}");

    let state = red_baseline(&dir.0, &["tests/a.rs::a1"], 101, "");
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, CARGO_FAIL_FAST, 101);
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert!(
        str_field(&out, "tests_output").contains("cargo stopped at the first failing target"),
        "{out}"
    );
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, &no_fail_fast, 101);
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
}

/// With no test command neither gate runs anything: baseline_tests records
/// no baseline, and verify_tests emits only its GATE NOT RUN verdict, with
/// no baseline report.
#[test]
fn test_gates_without_a_test_command_run_nothing() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("no-test-command");
    project(&dir.0, "here");
    let out = run_script(BASELINE, &dir.0, &project_state(&dir.0), &[]);
    assert_baseline_shape(&out);
    assert_eq!(str_field(&out, "baseline_status"), "none", "{out}");
    assert!(baseline_ids(&out).is_empty(), "{out}");
    assert_eq!(str_field(&out, "baseline_failure_hash"), "", "{out}");
    assert_eq!(
        str_field(&out, "baseline_note"),
        "No test command was configured or detected, so no baseline was recorded.",
        "{out}"
    );

    let out = run_script(VERIFY_TESTS, &dir.0, &project_state(&dir.0), &[]);
    let mut keys: Vec<&str> = out
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["_next", "tests_ok", "tests_output", "tests_verdict"],
        "{out}"
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(str_field(&out, "_next"), "self_review", "{out}");
    assert!(
        str_field(&out, "tests_output")
            .starts_with("(GATE NOT RUN: no test command configured or detected."),
        "{out}"
    );
}

/// A cargo baseline failure that a red run shows passing in the same target
/// is reported fixed under its target-qualified id.
#[test]
fn verify_tests_reports_a_target_qualified_cargo_failure_as_fixed() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-cargo-fixed");
    project(&dir.0, "here");
    let state = red_baseline(&dir.0, &["tests/foo.rs::a", "tests/foo.rs::b"], 101, "");
    let transcript = "     Running tests/foo.rs (target/debug/deps/foo-1)\n\
         test a ... FAILED\n\
         test b ... ok\n\
         test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n\
         error: test failed, to rerun pass `--test foo`\n\
         error: 1 target failed:\n\
         \x20   `--test foo`\n";
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, transcript, 101);
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(
        str_field(&out, "baseline_report"),
        "\nPre-existing failures: tests/foo.rs::a\nFixed pre-existing failures: tests/foo.rs::b",
        "{out}"
    );
}

/// Output without a failure line hashes to nothing, and nothing matches
/// nothing: two unrelated failing runs are not the same failure.
#[test]
fn verify_tests_never_matches_an_empty_failure_hash() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-empty-hash");
    committed_project(&dir.0, "here");
    let base = run_script(
        BASELINE,
        &dir.0,
        &project_state(&dir.0),
        &[("TEST_CMD", "echo boom; exit 1")],
    );
    assert_eq!(str_field(&base, "baseline_failure_hash"), "", "{base}");
    let state = red_baseline(&dir.0, &[], 1, "");
    let out = run_script(
        VERIFY_TESTS,
        &dir.0,
        &state,
        &[("TEST_CMD", "echo other; exit 1")],
    );
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    assert_eq!(str_field(&out, "_next"), "fix_loop_gate", "{out}");
}

/// A baseline whose failures could not be identified never passes a run
/// whose failures can: it can only match by hash.
#[test]
fn verify_tests_fails_an_identified_run_against_an_unidentified_baseline() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-unknown-base");
    committed_project(&dir.0, "here");
    let a = "tests/test_a.py::test_one";
    let transcript = pytest_failing(&[a]);
    let base = run_transcript(BASELINE, &dir.0, &project_state(&dir.0), &transcript, 1);
    let hash = str_field(&base, "baseline_failure_hash");
    assert!(!hash.is_empty(), "{base}");
    // Same exit code and same hash: only the identified ids can fail it.
    let state = red_baseline(&dir.0, &[], 1, hash);
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, &transcript, 1);
    assert_eq!(out["tests_ok"], Value::Bool(false), "{out}");
    let output = str_field(&out, "tests_output");
    assert!(
        output.contains(&format!(
            "\nFailing tests (the baseline's failures could not be identified, so none can be compared): {a}\n"
        )),
        "{out}"
    );
    assert!(!output.contains("New failures:"), "{out}");
}

fn baseline_store(dir: &Path) -> PathBuf {
    dir.join(".git").join("coyote-baselines")
}

/// The baseline records stored for work tree `dir`, sorted.
fn stored_records(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(baseline_store(dir)) else {
        return Vec::new();
    };
    let mut records: Vec<PathBuf> = entries.map(|e| e.unwrap().path()).collect();
    records.sort();
    records
}

/// The hex sha256 of `text`, through the same tools baseline_tests uses.
fn sha256_hex(text: &str) -> String {
    let mut child = Command::new(bash_program().expect("bash resolved by tools_missing"))
        .args([
            "-c",
            "if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn bash");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "sha256 of {text:?}");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap()
        .to_string()
}

/// A baseline taken on a clean committed tree is stored under the git dir,
/// keyed by HEAD, directory and command, and reused as is on later runs at
/// that commit, even once the tree holds an edit that breaks the suite. A
/// stored record is never rewritten: a planted one wins over the suite.
#[test]
fn baseline_tests_reuses_the_record_stored_on_a_clean_tree() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("baseline-store");
    committed_project(&dir.0, "here");
    let cmd = "if [ -e broken.txt ]; then printf 'test edited ... FAILED\\n'; exit 101; fi";
    let envs = [("TEST_CMD", cmd)];
    let state = project_state(&dir.0);

    let out = run_script(BASELINE, &dir.0, &state, &envs);
    assert_baseline_shape(&out);
    assert_eq!(str_field(&out, "baseline_status"), "green", "{out}");

    let head = git(&dir.0, &["rev-parse", "HEAD"]);
    let key = sha256_hex(&format!("{head}\n.\n{cmd}"));
    let record = baseline_store(&dir.0).join(format!("tests-{}.json", &key[..16]));
    assert_eq!(stored_records(&dir.0), std::slice::from_ref(&record));
    let stored: Value = serde_json::from_str(&fs::read_to_string(&record).unwrap()).unwrap();
    let workdir = str_field(&stored, "workdir");
    assert!(
        Path::new(workdir).file_name() == dir.0.file_name(),
        "{stored}"
    );
    let recorded_at = str_field(&stored, "recorded_at");
    assert!(
        recorded_at.len() == 20 && recorded_at.ends_with('Z'),
        "{stored}"
    );
    assert_eq!(
        stored,
        serde_json::json!({
            "version": 1,
            "head": head,
            "workdir": workdir,
            "command": cmd,
            "recorded_at": recorded_at,
            "status": "green",
            "exit_code": 0,
            "failures": [],
            "failure_hash": "",
            "unidentified_reason": "",
        })
    );

    fs::write(dir.0.join("broken.txt"), "edit").unwrap();
    let red = run_script(VERIFY_TESTS, &dir.0, &state, &envs);
    assert_eq!(red["tests_ok"], Value::Bool(false), "the edit fails: {red}");
    let out = run_script(BASELINE, &dir.0, &state, &envs);
    assert_baseline_shape(&out);
    assert_eq!(str_field(&out, "baseline_status"), "green", "{out}");
    assert!(baseline_ids(&out).is_empty(), "{out}");
    let note = str_field(&out, "baseline_note");
    assert!(
        note.contains(&format!("recorded on HEAD {} before any edit", &head[..7])),
        "{note}"
    );
    assert!(note.contains("reused"), "{note}");

    let planted = serde_json::json!({
        "version": 1,
        "head": head,
        "workdir": workdir,
        "command": cmd,
        "recorded_at": "2000-01-01T00:00:00Z",
        "status": "red",
        "exit_code": 101,
        "failures": ["planted"],
        "failure_hash": "",
        "unidentified_reason": "",
    })
    .to_string();
    fs::write(&record, &planted).unwrap();
    fs::remove_file(dir.0.join("broken.txt")).unwrap();
    let out = run_script(BASELINE, &dir.0, &state, &envs);
    assert_eq!(str_field(&out, "baseline_status"), "red", "{out}");
    assert_eq!(out["baseline_exit_code"], Value::from(101), "{out}");
    assert_eq!(baseline_ids(&out), ["planted"], "{out}");
    assert_eq!(fs::read_to_string(&record).unwrap(), planted);
    assert_eq!(stored_records(&dir.0), [record]);
}

/// With no stored record, a tree that already holds changes (an untracked
/// file is enough) gets no baseline and the suite does not run; neither
/// does a directory outside git or a repository with no commit yet.
#[test]
fn baseline_tests_runs_nothing_on_a_tree_it_cannot_show_is_untouched() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("baseline-unproven");
    let dirty = root.0.join("dirty");
    committed_project(&dirty, "here");
    fs::write(dirty.join("edit.txt"), "edit").unwrap();
    let unborn = root.0.join("unborn");
    project(&unborn, "here");
    git_init(&unborn);
    let plain = root.0.join("plain");
    project(&plain, "here");
    let head = git(&dirty, &["rev-parse", "--short=7", "HEAD"]);

    let cmd = "touch marker.txt; printf 'test a ... FAILED\\n'; exit 101";
    let legs: [(&Path, &str, String); 3] = [
        (
            &dirty,
            "dirty",
            format!(
                "uncommitted changes at the start of this run and no baseline is stored for HEAD {head}"
            ),
        ),
        (
            &unborn,
            "none",
            "cannot be told from an edited one".to_string(),
        ),
        (
            &plain,
            "none",
            "cannot be told from an edited one".to_string(),
        ),
    ];
    for (dir, status, note) in legs {
        let out = run_script(BASELINE, &root.0, &project_state(dir), &[("TEST_CMD", cmd)]);
        assert_baseline_shape(&out);
        assert_eq!(str_field(&out, "baseline_status"), status, "{out}");
        assert!(baseline_ids(&out).is_empty(), "{out}");
        assert_eq!(out["baseline_exit_code"], Value::from(0), "{out}");
        assert!(str_field(&out, "baseline_note").contains(&note), "{out}");
        assert!(
            !dir.join("marker.txt").exists(),
            "{status}: the suite ran in {}",
            dir.display()
        );
        assert!(stored_records(dir).is_empty(), "{status}: stored a record");
    }
}

/// A git status that fails prints nothing, which must not pass for a clean
/// tree, and a stored record that cannot be read is neither trusted nor
/// silently replaced: both get no baseline, the suite does not run, and the
/// note says why (naming the record to delete).
#[test]
fn baseline_tests_trusts_neither_a_failed_status_nor_an_unreadable_record() {
    if tools_missing() {
        return;
    }
    let root = temp_dir("baseline-unreadable");
    let cmd = "touch marker.txt; printf 'test a ... FAILED\\n'; exit 101";

    let broken_index = root.0.join("broken-index");
    committed_project(&broken_index, "here");
    fs::write(broken_index.join(".git").join("index"), "not an index").unwrap();

    let bad_record = root.0.join("bad-record");
    committed_project(&bad_record, "here");
    let head = git(&bad_record, &["rev-parse", "HEAD"]);
    let key = sha256_hex(&format!("{head}\n.\n{cmd}"))[..16].to_string();
    fs::create_dir_all(baseline_store(&bad_record)).unwrap();
    let record = baseline_store(&bad_record).join(format!("tests-{key}.json"));
    fs::write(&record, "{\"version\":1,\"status\":\"re").unwrap();

    for (dir, note) in [
        (&broken_index, "git status failed".to_string()),
        (&bad_record, format!("tests-{key}.json cannot be read")),
    ] {
        let out = run_script(BASELINE, &root.0, &project_state(dir), &[("TEST_CMD", cmd)]);
        assert_baseline_shape(&out);
        assert_eq!(str_field(&out, "baseline_status"), "none", "{out}");
        assert!(baseline_ids(&out).is_empty(), "{out}");
        let got = str_field(&out, "baseline_note");
        assert!(got.contains(&note), "{note:?} not in {out}");
        assert!(
            !dir.join("marker.txt").exists(),
            "the suite ran in {}",
            dir.display()
        );
    }
    assert_eq!(
        fs::read_to_string(&record).unwrap(),
        "{\"version\":1,\"status\":\"re"
    );
}

/// end_success reports what each check did instead of a literal pass,
/// seeded `not run` for a run that never reached it, with the `Review:` line
/// after `Tests:` and the Design block last.
#[test]
fn end_success_renders_the_gate_and_review_verdicts() {
    let graph = graph();
    let output = graph["nodes"]["end_success"]["output"].as_str().unwrap();
    assert!(!output.contains("Build: passed"), "{output}");
    assert!(!output.contains("Tests: passed"), "{output}");
    assert!(
        output.ends_with(
            "\nBuild: {{build_verdict}}\nTests: {{tests_verdict}}{{baseline_report}}\nReview: {{review_verdict}}\nDesign: {{design_notes}}\n"
        ),
        "{output}"
    );
    for key in ["build_verdict", "tests_verdict", "review_verdict"] {
        assert_eq!(
            graph["initial_state"][key].as_str(),
            Some("not run"),
            "initial_state.{key}"
        );
    }
}

/// Each verify gate states its verdict for the report: `passed` on a green
/// run, and NOT RUN when there is no command to run.
#[test]
fn verify_gates_emit_passed_and_not_run_verdicts() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("gate-verdicts");
    project(&dir.0, "here");
    let state = project_state(&dir.0);
    let legs = [
        (
            &GATES[0],
            "build_verdict",
            "NOT RUN: no build/check command configured or detected",
        ),
        (
            &GATES[1],
            "tests_verdict",
            "NOT RUN: no test command configured or detected",
        ),
    ];
    for (gate, key, not_run) in legs {
        let out = run_script(gate.rel, &dir.0, &state, &[]);
        assert_eq!(str_field(&out, key), not_run, "{}: {out}", gate.rel);
        let out = run_script(gate.rel, &dir.0, &state, &[(gate.cmd_env, "true")]);
        assert_eq!(str_field(&out, key), "passed", "{}: {out}", gate.rel);
        let out = run_script(gate.rel, &dir.0, &state, &[(gate.cmd_env, "exit 3")]);
        assert_eq!(
            str_field(&out, key),
            "failed (exit 3)",
            "{}: {out}",
            gate.rel
        );
    }
}

/// A red run passed against the baseline says so in its one-line verdict,
/// naming the accepted failures (capped like every id list), or the reason
/// they could not be named when the output matched by hash.
#[test]
fn verify_tests_verdict_names_the_accepted_baseline_failures() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("verify-verdict");
    committed_project(&dir.0, "here");
    let prefix = "red, matches the pre-run baseline (exit 1); accepted pre-existing failures: ";
    let (a, b) = ("tests/test_a.py::test_one", "tests/test_b.py::test_two");
    let state = red_baseline(&dir.0, &[a, b], 1, "");
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, &pytest_failing(&[a, b]), 1);
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(
        str_field(&out, "tests_verdict"),
        format!("{prefix}{a}, {b}"),
        "{out}"
    );

    let many: Vec<String> = (1..=23).map(|n| format!("tests/t.py::t{n:02}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let state = red_baseline(&dir.0, &many, 1, "");
    let out = run_transcript(VERIFY_TESTS, &dir.0, &state, &pytest_failing(&many), 1);
    let verdict = str_field(&out, "tests_verdict");
    assert!(
        verdict.starts_with(prefix) && verdict.ends_with("tests/t.py::t20, (+3 more)"),
        "{out}"
    );

    let base = run_transcript(
        BASELINE,
        &dir.0,
        &project_state(&dir.0),
        "error: linker failed at 0x7ffd1234 after 0.52s\n",
        1,
    );
    let state = red_baseline(&dir.0, &[], 1, str_field(&base, "baseline_failure_hash"));
    let out = run_transcript(
        VERIFY_TESTS,
        &dir.0,
        &state,
        "error: linker failed at 0x7ffe9999 after 1.03s\n",
        1,
    );
    assert_eq!(out["tests_ok"], Value::Bool(true), "{out}");
    assert_eq!(
        str_field(&out, "tests_verdict"),
        format!("{prefix}unidentified (no test ids could be parsed)"),
        "{out}"
    );
}

/// self_review sees the gate verdicts and the test output, and is told that
/// a pass resting on baseline failures is a scope call it must make, and
/// that a test reported NOT RUN is not verified.
#[test]
fn self_review_sees_the_test_evidence() {
    let graph = graph();
    let node = &graph["nodes"]["self_review"];
    let prompt = node["prompt"].as_str().unwrap();
    for section in [
        "## Build result\n{{build_verdict}}\n",
        "## Test result\n{{tests_verdict}}{{baseline_report}}\n",
        "## Test output\n{{tests_output}}\n",
    ] {
        assert!(prompt.contains(section), "missing {section:?}: {prompt}");
    }
    let instructions = node["instructions"]
        .as_str()
        .unwrap()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for needle in [
        "passed only because their failures match the pre-run baseline",
        "if any accepted failure is within what the request asked for, set review_clean=false and name it in review_notes",
        "reported as NOT RUN was not verified",
    ] {
        assert!(instructions.contains(needle), "missing {needle:?}");
    }
}

/// The review outcome reaches the report: `clean`, the notes left unresolved
/// when the review budget ran out (verbatim), or NOT RUN with any earlier
/// notes when the self-review died.
#[test]
fn review_routes_emit_the_review_verdict() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("review-verdict");
    let route = "coder/scripts/route_review_result.sh";
    let fault = "coder/scripts/self_review_fault.sh";
    let notes = "x.rs:1 - y\n  z.rs:2 - w  \n";
    let indented = "  x.rs:1 - y\n    z.rs:2 - w  \n  ";
    let not_run = "NOT RUN: self-review ended without a verdict (iteration cap, timeout or provider failure); the diff was not self-reviewed";

    let out = run_script(
        route,
        &dir.0,
        &serde_json::json!({"review_clean": true}),
        &[],
    );
    assert_eq!(str_field(&out, "review_verdict"), "clean", "{out}");
    let out = run_script(route, &dir.0, &serde_json::json!({}), &[]);
    assert_eq!(
        str_field(&out, "review_verdict"),
        "NOT RUN: self-review returned no verdict",
        "{out}"
    );

    let exhausted = serde_json::json!({
        "review_clean": false,
        "review_notes": notes,
        "review_attempts": 1,
        "max_review_attempts": 1,
    });
    let out = run_script(route, &dir.0, &exhausted, &[]);
    assert_eq!(str_field(&out, "_next"), "end_success", "{out}");
    assert_eq!(
        str_field(&out, "review_verdict"),
        format!("unresolved notes (review budget exhausted):\n{indented}"),
        "{out}"
    );
    let mut empty = exhausted.clone();
    empty["review_notes"] = Value::from("");
    let out = run_script(route, &dir.0, &empty, &[]);
    assert_eq!(
        str_field(&out, "review_verdict"),
        "unresolved notes (review budget exhausted):\n  (no notes given)",
        "{out}"
    );

    let out = run_script(fault, &dir.0, &serde_json::json!({"review_notes": ""}), &[]);
    assert_eq!(str_field(&out, "review_verdict"), not_run, "{out}");
    let out = run_script(
        fault,
        &dir.0,
        &serde_json::json!({"review_notes": notes}),
        &[],
    );
    assert_eq!(
        str_field(&out, "review_verdict"),
        format!("{not_run}\n  Earlier review notes:\n{indented}"),
        "{out}"
    );
}

/// An answer other than exactly yes or no goes to parse_approval, never
/// straight to end_rejected. A crash of it fails closed, never as a
/// rejection, and the answer and
/// any re-ask line travel through state.
#[test]
fn gate_approval_sends_other_answers_to_parse_approval() {
    let graph = graph();
    let gate = &graph["nodes"]["gate_approval"];
    assert_eq!(gate["on_other"].as_str(), Some("parse_approval"));
    assert_eq!(
        gate["state_updates"]["approval_answer"].as_str(),
        Some("{{choice}}")
    );
    assert!(
        gate["question"]
            .as_str()
            .unwrap()
            .trim_end()
            .ends_with("{{approval_reask}}")
    );
    let node = &graph["nodes"]["parse_approval"];
    assert_eq!(node["type"].as_str(), Some("script"));
    assert_eq!(node["script"].as_str(), Some("scripts/parse_approval.sh"));
    assert_eq!(node["timeout"].as_u64(), Some(5));
    assert_eq!(node["fallback"].as_str(), Some("end_failure"));
    assert_eq!(graph["initial_state"]["approval_reask"].as_str(), Some(""));
}

/// parse_approval honours a reworded yes or no and asks again, saying why,
/// on anything else, a yes with a negation in it included.
#[test]
fn parse_approval_routes_reworded_answers() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("parse-approval");
    let rel = "coder/scripts/parse_approval.sh";
    for (answer, next) in [
        ("Yes", "baseline_tests"),
        ("approve.", "baseline_tests"),
        ("  yes, go ahead!  ", "baseline_tests"),
        ("LGTM", "baseline_tests"),
        ("ok", "baseline_tests"),
        ("I approve the plan", "baseline_tests"),
        ("No", "end_rejected"),
        ("reject.", "end_rejected"),
        ("no thanks", "end_rejected"),
        ("maybe later", "gate_approval"),
        ("yes, but not now", "gate_approval"),
        ("yes, but rename the module first", "gate_approval"),
        ("Yes, but don\u{2019}t touch it", "gate_approval"),
        ("go back and revise the plan", "gate_approval"),
        ("go", "gate_approval"),
        ("No problem, go ahead", "gate_approval"),
        ("no objections", "gate_approval"),
        ("no wait, yes", "gate_approval"),
        ("", "gate_approval"),
    ] {
        let state = serde_json::json!({ "approval_answer": answer });
        let out = run_script(rel, &dir.0, &state, &[]);
        assert_eq!(str_field(&out, "_next"), next, "{answer:?}: {out}");
        let reask = str_field(&out, "approval_reask");
        if next == "gate_approval" {
            assert_eq!(
                reask,
                format!(
                    "Your previous answer \"{}\" was not understood. Answer exactly yes or no.",
                    answer.trim()
                ),
                "{out}"
            );
        } else {
            assert_eq!(reask, "", "{answer:?}: {out}");
        }
    }
}
