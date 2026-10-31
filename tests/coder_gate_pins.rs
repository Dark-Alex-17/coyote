//! Pins for where and how long the coder graph's verify gates run: the
//! directory they run in when the caller left `project_dir` unset, the
//! `test_command` variable, and the `build_timeout` / `test_timeout`
//! deadlines whose expiry is reported apart from a red run (by the gate
//! itself, or by fix_loop_gate when the engine killed the node). On unix
//! `bash`, `jq` and `git` must be on PATH (asserted, so a missing tool cannot
//! hide the pins); elsewhere the script pins skip without them.

use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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

fn git_init(dir: &Path) {
    let status = Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git init in {}", dir.display());
}

fn run_script(rel: &str, dir: &Path, state: &Value, envs: &[(&str, &str)]) -> Value {
    let state_file = dir.join(format!("state-{}.json", rel.replace('/', "-")));
    fs::write(&state_file, state.to_string()).unwrap();
    let mut cmd = Command::new(bash_program().expect("bash resolved by tools_missing"));
    cmd.arg(asset(rel))
        .current_dir(dir)
        .env("GRAPH_STATE_FILE", &state_file)
        .env_remove("GRAPH_STATE")
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
    git_init(&tree);
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
    }
}

/// The test_command variable is the command verify_tests runs, ahead of
/// TEST_CMD and detection.
#[test]
fn verify_tests_runs_the_test_command_variable_first() {
    if tools_missing() {
        return;
    }
    let dir = temp_dir("test-command");
    project(&dir.0, "here");
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
    project(&dir.0, "here");
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
