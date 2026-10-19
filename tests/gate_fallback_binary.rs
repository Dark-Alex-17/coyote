//! Binary-level contract for the fail-closed routing gates: the shipped
//! `route_review` (step-runner) and `route_review_result` (coder) node
//! definitions, with the shipped scripts behind them, run through the real
//! `coyote` binary. A gate crash lands on `end_failure`, the trace line
//! names the crash as ` -> end_failure (fallback)`, and a healthy gate still
//! takes its clean route without the marker. The 40 KB case drives the
//! state past the inline GRAPH_STATE budget so the script reads the spilled
//! file and hands the report to jq on stdin.
//!
//! Unix only: the engine spawns the scripts through `bash` on PATH with a
//! `#!/usr/bin/env bash` shebang and the fixture relies on `jq`; the Windows
//! arm of that spawn is exercised by the windows-latest lane's executor
//! tests, not here.
#![cfg(unix)]

use serde_json::{Value, json};
use serial_test::serial;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const AGENT: &str = "probe-gates";

/// Removes the fixture dir even when an assertion panics mid-run.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Unix hosts must have both tools, so a missing one is a harness defect
/// rather than a reason to skip: a silent skip would pass this suite with
/// nothing exercised.
fn tools_missing() -> bool {
    let bash = which::which("bash");
    let jq = which::which("jq");
    assert!(
        bash.is_ok(),
        "bash must be on PATH on unix; a silent skip would hide the harness"
    );
    assert!(
        jq.is_ok(),
        "jq must be on PATH on unix; a silent skip would hide the harness"
    );
    let missing = bash.is_err() || jq.is_err();
    if missing {
        eprintln!("skipping: bash/jq not available");
    }
    missing
}

fn fresh_dir(label: &str) -> TempDirGuard {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!("coyote-gate-fallback-{label}-{unique}"));
    fs::create_dir_all(dir.join("cache")).unwrap();
    TempDirGuard(dir)
}

fn asset(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("assets/agents")
        .join(rel)
}

/// The node `id` lifted verbatim from a shipped graph.yaml, so the fixture
/// runs the exact `fallback:` a consumer gets.
fn shipped_node(graph_rel: &str, id: &str) -> Value {
    let text = fs::read_to_string(asset(graph_rel)).unwrap();
    let graph: Value = serde_yaml::from_str(&text).unwrap();
    let node = &graph["nodes"][id];
    assert!(node.is_object(), "{graph_rel} must define node {id}");
    node.clone()
}

/// A config dir with a dry_run model and one graph agent: the shipped gate
/// node plus one end node per route it can take, each printing a marker.
fn write_fixture(
    dir: &Path,
    graph_rel: &str,
    gate: &str,
    script_rel: &str,
    routes: &[&str],
    initial_state: &Value,
) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai\n\
             \x20   name: dryrun\n\
             \x20   auth: none\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: dry-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             save: false\n\
             memory: false\n\
             stream: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();

    let agent_dir = dir.join("agents").join(AGENT);
    fs::create_dir_all(agent_dir.join("scripts")).unwrap();
    let script = asset(script_rel);
    let target = agent_dir.join("scripts").join(script.file_name().unwrap());
    fs::copy(&script, &target).unwrap();

    let mut nodes = serde_yaml::Mapping::new();
    nodes.insert(
        serde_yaml::Value::String(gate.to_string()),
        serde_yaml::to_value(shipped_node(graph_rel, gate)).unwrap(),
    );
    for route in routes {
        let mut end = serde_yaml::Mapping::new();
        end.insert("id".into(), serde_yaml::Value::String(route.to_string()));
        end.insert("type".into(), "end".into());
        end.insert(
            "output".into(),
            serde_yaml::Value::String(format!("PROBE_ROUTE={route}")),
        );
        nodes.insert(
            serde_yaml::Value::String(route.to_string()),
            serde_yaml::Value::Mapping(end),
        );
    }
    let mut graph = serde_yaml::Mapping::new();
    graph.insert("name".into(), "probe_gates".into());
    graph.insert("start".into(), serde_yaml::Value::String(gate.to_string()));
    graph.insert(
        "initial_state".into(),
        serde_yaml::to_value(initial_state).unwrap(),
    );
    graph.insert("nodes".into(), serde_yaml::Value::Mapping(nodes));
    fs::write(
        agent_dir.join("graph.yaml"),
        serde_yaml::to_string(&serde_yaml::Value::Mapping(graph)).unwrap(),
    )
    .unwrap();
}

fn run_coyote_with(dir: &Path, envs: &[(&str, &str)]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(["--agent", AGENT, "probe"])
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("IS_SANDBOX")
        .env_remove("CODER_AUTOAPPROVE")
        .env_remove("STEP_AUTOAPPROVE")
        .env_remove("STEP_SKIP_REVIEW")
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn run_coyote(dir: &Path) -> Output {
    run_coyote_with(dir, &[])
}

/// Runs the fixture and returns (stdout, stderr) with CRLF folded.
fn run_gate(
    label: &str,
    graph_rel: &str,
    gate: &str,
    script_rel: &str,
    routes: &[&str],
    initial_state: &Value,
) -> (String, String) {
    let dir = fresh_dir(label);
    write_fixture(&dir.0, graph_rel, gate, script_rel, routes, initial_state);
    let out = run_coyote(&dir.0);
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n");
    assert!(
        out.status.success(),
        "{label}: exit {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    (stdout, stderr)
}

/// The trace line for `node`, as the consumer reads it on stderr.
fn done_line<'a>(stderr: &'a str, node: &str) -> &'a str {
    let needle = format!("{node} (script) done in");
    stderr
        .lines()
        .find(|line| line.contains(&needle))
        .unwrap_or_else(|| panic!("no completion line for {node} in:\n{stderr}"))
}

const STEP_RUNNER_GRAPH: &str = "step-runner/graph.yaml";
const STEP_RUNNER_SCRIPT: &str = "step-runner/scripts/route_review.sh";
const STEP_RUNNER_ROUTES: &[&str] = &["end_failure", "write_handoff", "implement"];

/// A gate whose script dies (a non-numeric attempt counter trips bash
/// arithmetic under `set -e`) fails the step closed: the run ends on
/// end_failure and the trace marks the hop as a fallback.
#[test]
// Each test boots the real binary, which forks the gate scripts under it;
// run one at a time so the suite never trips the host's process budget.
#[serial]
fn step_runner_route_review_crash_fails_closed_and_is_traced_as_a_fallback() {
    if tools_missing() {
        return;
    }
    let (stdout, stderr) = run_gate(
        "step-runner-crash",
        STEP_RUNNER_GRAPH,
        "route_review",
        STEP_RUNNER_SCRIPT,
        STEP_RUNNER_ROUTES,
        &json!({
            "review_report": "- \u{1F534} CRITICAL: broken",
            "review_attempts": "9x",
            "max_review_attempts": 2,
        }),
    );
    assert_eq!(stdout.trim(), "PROBE_ROUTE=end_failure", "stderr: {stderr}");
    let line = done_line(&stderr, "route_review");
    assert!(
        line.ends_with(" -> end_failure (fallback)"),
        "trace must name the fallback hop: {line}"
    );
}

/// A clean report still takes the gate's own route, and the trace carries
/// no fallback marker.
#[test]
#[serial]
fn step_runner_route_review_clean_report_routes_to_write_handoff_without_a_marker() {
    if tools_missing() {
        return;
    }
    let (stdout, stderr) = run_gate(
        "step-runner-clean",
        STEP_RUNNER_GRAPH,
        "route_review",
        STEP_RUNNER_SCRIPT,
        STEP_RUNNER_ROUTES,
        &json!({
            "review_report": "all clean \u{1F7E2}",
            "review_attempts": 0,
            "max_review_attempts": 2,
        }),
    );
    assert_eq!(
        stdout.trim(),
        "PROBE_ROUTE=write_handoff",
        "stderr: {stderr}"
    );
    let line = done_line(&stderr, "route_review");
    assert!(line.ends_with(" -> write_handoff"), "{line}");
    assert!(!stderr.contains("(fallback)"), "{stderr}");
}

/// A 40 KB critical report pushes the state past the inline budget; the
/// shipped script reads the spilled state file, hands the report to jq on
/// stdin, and the run loops to implement. The end template proves the
/// counter the script wrote landed in state.
#[test]
#[serial]
fn step_runner_route_review_carries_a_spilled_40kb_report_through_the_engine() {
    if tools_missing() {
        return;
    }
    let unit = "- \u{1F534} CRITICAL: \"quoted\" \\back\\slash\ttab\n";
    let mut report = String::new();
    while report.len() < 40_000 {
        report.push_str(unit);
    }
    assert!(report.len() > 32_767);
    let dir = fresh_dir("step-runner-spill");
    write_fixture(
        &dir.0,
        STEP_RUNNER_GRAPH,
        "route_review",
        STEP_RUNNER_SCRIPT,
        STEP_RUNNER_ROUTES,
        &json!({
            "review_report": report,
            "review_attempts": 0,
            "max_review_attempts": 2,
        }),
    );
    // The implement end node renders what the gate wrote.
    let graph_path = dir.0.join("agents").join(AGENT).join("graph.yaml");
    let graph = fs::read_to_string(&graph_path).unwrap().replace(
        "PROBE_ROUTE=implement",
        "PROBE_ROUTE=implement attempts={{review_attempts}} fix={{fix_instructions}}",
    );
    fs::write(&graph_path, graph).unwrap();

    let out = run_coyote(&dir.0);
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n");
    assert!(out.status.success(), "exit {:?}\n{stderr}", out.status);
    assert!(
        stdout.starts_with(
            "PROBE_ROUTE=implement attempts=1 fix=## Independent review findings (attempt 1 of 2)"
        ),
        "{}",
        stdout.chars().take(200).collect::<String>()
    );
    assert!(
        stdout.trim_end().ends_with(report.trim_end()),
        "report tail missing from the rendered fix_instructions"
    );
    let line = done_line(&stderr, "route_review");
    assert!(line.ends_with(" -> implement"), "{line}");
}

const CODER_GRAPH: &str = "coder/graph.yaml";
const CODER_SCRIPT: &str = "coder/scripts/route_review_result.sh";
const CODER_ROUTES: &[&str] = &["end_failure", "end_success", "implement"];

/// The coder twin: a non-boolean review_clean makes the shipped script exit
/// 1, and the run ends on end_failure (it used to fall open to
/// end_success) with the fallback marker on the trace line.
#[test]
#[serial]
fn coder_route_review_result_crash_fails_closed_and_is_traced_as_a_fallback() {
    if tools_missing() {
        return;
    }
    let (stdout, stderr) = run_gate(
        "coder-crash",
        CODER_GRAPH,
        "route_review_result",
        CODER_SCRIPT,
        CODER_ROUTES,
        &json!({
            "review_clean": "maybe",
            "review_attempts": 0,
            "max_review_attempts": 1,
        }),
    );
    assert_eq!(stdout.trim(), "PROBE_ROUTE=end_failure", "stderr: {stderr}");
    let line = done_line(&stderr, "route_review_result");
    assert!(
        line.ends_with(" -> end_failure (fallback)"),
        "trace must name the fallback hop: {line}"
    );
}

/// The coder gate's clean route is untouched by the fallback change.
#[test]
#[serial]
fn coder_route_review_result_clean_review_ends_success_without_a_marker() {
    if tools_missing() {
        return;
    }
    let (stdout, stderr) = run_gate(
        "coder-clean",
        CODER_GRAPH,
        "route_review_result",
        CODER_SCRIPT,
        CODER_ROUTES,
        &json!({
            "review_clean": true,
            "review_attempts": 0,
            "max_review_attempts": 1,
        }),
    );
    assert_eq!(stdout.trim(), "PROBE_ROUTE=end_success", "stderr: {stderr}");
    let line = done_line(&stderr, "route_review_result");
    assert!(line.ends_with(" -> end_success"), "{line}");
    assert!(!stderr.contains("(fallback)"), "{stderr}");
}

const COMPLEXITY_SCRIPT: &str = "coder/scripts/route_complexity.sh";
const COMPLEXITY_ROUTES: &[&str] = &["gate_approval", "implement"];

/// The one gate that fails toward the human: a route_complexity script that
/// dies (jq fails under it; a malformed score alone does not crash the
/// script, its `if (( ... ))` swallows the arithmetic error) lands on
/// gate_approval through the fallback edge, so a crashed scorer asks for
/// plan approval instead of skipping it. The healthy routes on either side
/// of the threshold carry no marker, so the fallback hop is told apart from
/// a genuinely high score.
#[test]
#[serial]
fn coder_route_complexity_crash_asks_for_approval_and_is_traced_as_a_fallback() {
    if tools_missing() {
        return;
    }
    let dir = fresh_dir("coder-complexity-crash");
    write_fixture(
        &dir.0,
        CODER_GRAPH,
        "route_complexity",
        COMPLEXITY_SCRIPT,
        COMPLEXITY_ROUTES,
        &json!({ "complexity_score": 2 }),
    );
    let path = format!(
        "{}:{}",
        dead_jq_bin(&dir.0).display(),
        env::var("PATH").unwrap_or_default()
    );
    let out = run_coyote_with(&dir.0, &[("PATH", path.as_str())]);
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n");
    assert!(
        out.status.success(),
        "crash run: exit {:?}\n{stderr}",
        out.status
    );
    assert_eq!(
        stdout.trim(),
        "PROBE_ROUTE=gate_approval",
        "stderr: {stderr}"
    );
    let line = done_line(&stderr, "route_complexity");
    assert!(
        line.ends_with(" -> gate_approval (fallback)"),
        "trace must name the fallback hop: {line}"
    );

    for (label, score, route) in [
        ("coder-complexity-high", 9, "gate_approval"),
        ("coder-complexity-low", 2, "implement"),
    ] {
        let (stdout, stderr) = run_gate(
            label,
            CODER_GRAPH,
            "route_complexity",
            COMPLEXITY_SCRIPT,
            COMPLEXITY_ROUTES,
            &json!({ "complexity_score": score }),
        );
        assert_eq!(
            stdout.trim(),
            format!("PROBE_ROUTE={route}"),
            "{label}: {stderr}"
        );
        let line = done_line(&stderr, "route_complexity");
        assert!(line.ends_with(&format!(" -> {route}")), "{label}: {line}");
        assert!(!stderr.contains("(fallback)"), "{label}: {stderr}");
    }
}

/// A `jq` for the front of PATH that fails every call: the environment
/// failure (missing or broken jq) that can crash a routing gate whose
/// inputs are otherwise unremarkable.
fn dead_jq_bin(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("dead-jq-bin");
    fs::create_dir_all(&bin).unwrap();
    let shim = bin.join("jq");
    fs::write(
        &shim,
        "#!/bin/sh\necho 'jq: simulated outage' >&2\nexit 7\n",
    )
    .unwrap();
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// The remaining fail-closed gates (route_sweep, route_staleness,
/// self_review_fault) cannot be crashed from state, so the crash is the
/// one a consumer actually meets: jq fails under the script. Each gate
/// then ends on end_failure with the fallback marker and the failure
/// template's `Last script error:` names the dead gate; with a working jq
/// the same fixture takes the gate's own route without a marker.
#[test]
#[serial]
fn remaining_routing_gates_fail_closed_when_jq_dies_under_them() {
    if tools_missing() {
        return;
    }
    /// One shipped gate with an unremarkable state and the route it takes
    /// when its script runs to completion.
    struct GateCase {
        label: &'static str,
        graph_rel: &'static str,
        gate: &'static str,
        script_rel: &'static str,
        routes: &'static [&'static str],
        state: Value,
        own_route: &'static str,
    }
    let cases = [
        GateCase {
            label: "step-runner-sweep",
            graph_rel: STEP_RUNNER_GRAPH,
            gate: "route_sweep",
            script_rel: "step-runner/scripts/route_sweep.sh",
            routes: &["end_failure", "write_handoff", "independent_review"],
            state: json!({ "needs_independent_review": false }),
            own_route: "write_handoff",
        },
        GateCase {
            label: "step-runner-staleness",
            graph_rel: STEP_RUNNER_GRAPH,
            gate: "route_staleness",
            script_rel: "step-runner/scripts/route_staleness.sh",
            routes: &["end_failure", "implement", "gate_deviation"],
            state: json!({ "has_major_deviation": false }),
            own_route: "implement",
        },
        GateCase {
            label: "coder-self-review-fault",
            graph_rel: CODER_GRAPH,
            gate: "self_review_fault",
            script_rel: "coder/scripts/self_review_fault.sh",
            routes: &["end_failure", "end_success"],
            state: json!({ "design_trail": "", "review_notes": "" }),
            own_route: "end_success",
        },
    ];
    for case in &cases {
        let GateCase {
            label,
            graph_rel,
            gate,
            script_rel,
            routes,
            state,
            own_route,
        } = case;
        let dir = fresh_dir(label);
        write_fixture(&dir.0, graph_rel, gate, script_rel, routes, state);
        // The failure end node renders what the engine recorded about the
        // crash, as the shipped STEP_FAILED / CODER_FAILED templates do.
        let graph_path = dir.0.join("agents").join(AGENT).join("graph.yaml");
        let graph = fs::read_to_string(&graph_path).unwrap();
        assert!(
            graph.contains("output: PROBE_ROUTE=end_failure\n"),
            "{graph}"
        );
        let graph = graph.replace(
            "output: PROBE_ROUTE=end_failure\n",
            "output: 'PROBE_ROUTE=end_failure Last script error: {{last_script_error}}'\n",
        );
        fs::write(&graph_path, graph).unwrap();

        let out = run_coyote(&dir.0);
        let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        let stderr = String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n");
        assert!(
            out.status.success(),
            "{label}: exit {:?}\n{stderr}",
            out.status
        );
        assert_eq!(
            stdout.trim(),
            format!("PROBE_ROUTE={own_route}"),
            "{label} healthy: {stderr}"
        );
        assert!(
            done_line(&stderr, gate).ends_with(&format!(" -> {own_route}")),
            "{label} healthy: {stderr}"
        );
        assert!(!stderr.contains("(fallback)"), "{label} healthy: {stderr}");

        let path = format!(
            "{}:{}",
            dead_jq_bin(&dir.0).display(),
            env::var("PATH").unwrap_or_default()
        );
        let out = run_coyote_with(&dir.0, &[("PATH", path.as_str())]);
        let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
        let stderr = String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n");
        assert!(
            out.status.success(),
            "{label} dead jq: exit {:?}\n{stderr}",
            out.status
        );
        let (route, last) = stdout
            .trim()
            .split_once(" Last script error: ")
            .unwrap_or_else(|| panic!("{label} dead jq: unexpected stdout {stdout:?}\n{stderr}"));
        assert_eq!(
            route, "PROBE_ROUTE=end_failure",
            "{label} dead jq: {stderr}"
        );
        assert!(
            last.starts_with("script '") && last.contains(gate),
            "{label} dead jq: failure template must name the crashed gate, got {last:?}\nstderr: {stderr}"
        );
        let line = done_line(&stderr, gate);
        assert!(
            line.ends_with(" -> end_failure (fallback)"),
            "{label} dead jq: trace must name the fallback hop: {line}"
        );
    }
}

/// The coder README promises that `CODER_AUTOAPPROVE=1` bypasses the
/// approval gate but does not cover a crash of `route_complexity`: the
/// flag is honoured inside the script, after it has already asked jq for
/// the score, so a dead jq crashes the gate before the flag is read and
/// the fallback still asks the human. With a working jq the same flag
/// sends a high score straight to implement, so the two runs tell the
/// bypass apart from the crash.
#[test]
#[serial]
fn coder_autoapprove_bypasses_the_gate_but_not_a_route_complexity_crash() {
    if tools_missing() {
        return;
    }
    let dir = fresh_dir("coder-autoapprove-healthy");
    write_fixture(
        &dir.0,
        CODER_GRAPH,
        "route_complexity",
        COMPLEXITY_SCRIPT,
        COMPLEXITY_ROUTES,
        &json!({ "complexity_score": 9 }),
    );
    let out = run_coyote_with(&dir.0, &[("CODER_AUTOAPPROVE", "1")]);
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n");
    assert!(
        out.status.success(),
        "healthy run: exit {:?}\n{stderr}",
        out.status
    );
    assert_eq!(
        stdout.trim(),
        "PROBE_ROUTE=implement",
        "CODER_AUTOAPPROVE=1 must bypass the gate for a high score: {stderr}"
    );
    let line = done_line(&stderr, "route_complexity");
    assert!(line.ends_with(" -> implement"), "{line}");
    assert!(!stderr.contains("(fallback)"), "{stderr}");

    let dir = fresh_dir("coder-autoapprove-crash");
    write_fixture(
        &dir.0,
        CODER_GRAPH,
        "route_complexity",
        COMPLEXITY_SCRIPT,
        COMPLEXITY_ROUTES,
        &json!({ "complexity_score": 2 }),
    );
    let path = format!(
        "{}:{}",
        dead_jq_bin(&dir.0).display(),
        env::var("PATH").unwrap_or_default()
    );
    let out = run_coyote_with(
        &dir.0,
        &[("PATH", path.as_str()), ("CODER_AUTOAPPROVE", "1")],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr).replace("\r\n", "\n");
    assert!(
        out.status.success(),
        "crash run: exit {:?}\n{stderr}",
        out.status
    );
    assert_eq!(
        stdout.trim(),
        "PROBE_ROUTE=gate_approval",
        "a crashed gate must still ask the human even under CODER_AUTOAPPROVE=1: {stderr}"
    );
    let line = done_line(&stderr, "route_complexity");
    assert!(
        line.ends_with(" -> gate_approval (fallback)"),
        "trace must name the fallback hop: {line}"
    );
}
