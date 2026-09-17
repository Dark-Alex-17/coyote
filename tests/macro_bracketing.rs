//! Binary-level contract for --macro bracket symmetry: a committed macro
//! run fires agent.started plus EXACTLY ONE terminal agent event
//! (agent.completed / agent.failed / agent.interrupted), whether the agent
//! arrives via the --agent flag or an in-macro `.agent` step, in both
//! isolation modes, on success and failure alike. The markers ride on
//! global agent.* hooks admitted through the agent's `global_hooks: ["*"]`
//! wildcard, so the fixture also exercises the whitelist grammar
//! end-to-end — and stays observable even if a failing step leaves the
//! context without a loaded agent.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn fresh_config_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let tmp_dir = env::temp_dir().join(format!("coyote-macro-bracketing-{label}-{unique}"));
    fs::create_dir_all(&tmp_dir).unwrap();
    tmp_dir
}

/// Lays out a config dir with a dry_run model, global agent.* hooks that
/// append one marker line per event to `log`, a `probe-macro` agent that
/// whitelists everything, and a `probe` macro with the given YAML body.
fn write_fixture(dir: &Path, log: &Path, macro_yaml: &str) {
    let log = log.display();
    // Without IS_SANDBOX (scrubbed below) a run insists on a vault password
    // file, so the fixture provides one.
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             vault_password_file: {vault_pass}\n\
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
             stream: false\n\
             hooks:\n\
             \x20 agent.started:\n\
             \x20   - name: mark\n\
             \x20     command: \"echo STARTED >> '{log}'\"\n\
             \x20 agent.completed:\n\
             \x20   - name: mark\n\
             \x20     command: \"echo COMPLETED >> '{log}'\"\n\
             \x20 agent.failed:\n\
             \x20   - name: mark\n\
             \x20     command: \"echo FAILED >> '{log}'\"\n\
             \x20 agent.interrupted:\n\
             \x20   - name: mark\n\
             \x20     command: \"echo INTERRUPTED >> '{log}'\"\n",
            vault_pass = vault_pass.display()
        ),
    )
    .unwrap();

    let agent_dir = dir.join("agents/probe-macro");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        "name: probe-macro\n\
         description: macro bracketing probe agent\n\
         version: 0.1.0\n\
         instructions: |\n\
         \x20 You are a probe agent. Reply briefly.\n\
         global_hooks:\n\
         \x20 - \"*\"\n",
    )
    .unwrap();

    let macros_dir = dir.join("macros");
    fs::create_dir_all(&macros_dir).unwrap();
    fs::write(macros_dir.join("probe.yaml"), macro_yaml).unwrap();
}

fn run_coyote(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(args)
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env_remove("IS_SANDBOX")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Bounded poll for the expected terminal marker. The binary drains pending
/// hooks before exiting, so the log is normally complete once `output()`
/// returns; the poll only absorbs filesystem latency, never paces the run.
fn wait_for_marker(log: &Path, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let contents = fs::read_to_string(log).unwrap_or_default();
        if contents.lines().any(|line| line == needle) {
            return contents;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for '{needle}'; log so far: {contents:?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn assert_marker_count(contents: &str, marker: &str, expected: usize) {
    let count = contents.lines().filter(|line| *line == marker).count();
    assert_eq!(
        count, expected,
        "expected {expected}x {marker}: {contents:?}"
    );
}

fn assert_started_and_single_terminal(contents: &str, terminal: &str) {
    assert_marker_count(contents, "STARTED", 1);
    for marker in ["COMPLETED", "FAILED", "INTERRUPTED"] {
        let expected = usize::from(marker == terminal);
        assert_marker_count(contents, marker, expected);
    }
}

fn probe_macro_run(label: &str, macro_yaml: &str, args: &[&str], expect_success: bool) -> String {
    let dir = fresh_config_dir(label);
    let log = dir.join("agent-events.log");
    write_fixture(&dir, &log, macro_yaml);

    let output = run_coyote(&dir, args);
    assert_eq!(
        output.status.success(),
        expect_success,
        "{label}: unexpected exit {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let terminal = if expect_success {
        "COMPLETED"
    } else {
        "FAILED"
    };
    let contents = wait_for_marker(&log, terminal);
    let _ = fs::remove_dir_all(&dir);
    assert_started_and_single_terminal(&contents, terminal);
    contents
}

#[test]
fn agent_macro_run_fires_started_and_exactly_one_completed() {
    probe_macro_run(
        "agent-flag-success",
        "steps:\n  - \".set temperature 0.5\"\n",
        &["--agent", "probe-macro", "--macro", "probe"],
        true,
    );
}

#[test]
fn agent_macro_failure_closes_the_bracket_with_exactly_one_failed() {
    probe_macro_run(
        "agent-flag-failure",
        "steps:\n  - \".agent no-such-agent-zz\"\n",
        &["--agent", "probe-macro", "--macro", "probe"],
        false,
    );
}

#[test]
fn isolated_in_macro_agent_step_fires_started_and_exactly_one_completed() {
    probe_macro_run(
        "isolated-step-success",
        "steps:\n  - \".agent probe-macro\"\n  - \".set temperature 0.5\"\n",
        &["--macro", "probe"],
        true,
    );
}

#[test]
fn isolated_in_macro_agent_step_failure_closes_the_bracket() {
    probe_macro_run(
        "isolated-step-failure",
        "steps:\n  - \".agent probe-macro\"\n  - \".agent no-such-agent-zz\"\n",
        &["--macro", "probe"],
        false,
    );
}

#[test]
fn non_isolated_in_macro_agent_step_fires_started_and_exactly_one_completed() {
    probe_macro_run(
        "non-isolated-step-success",
        "isolated: false\nsteps:\n  - \".agent probe-macro\"\n  - \".set temperature 0.5\"\n",
        &["--macro", "probe"],
        true,
    );
}

#[test]
fn non_isolated_in_macro_agent_step_failure_closes_the_bracket() {
    probe_macro_run(
        "non-isolated-step-failure",
        "isolated: false\nsteps:\n  - \".agent probe-macro\"\n  - \".agent no-such-agent-zz\"\n",
        &["--macro", "probe"],
        false,
    );
}
