//! Binary-level contract for agent variable inheritance: a parent graph
//! agent started from the CLI with `--agent-variable` runs a child graph
//! agent through an agent node, and the child is seated with the parent's
//! values, both as its script env (`LLM_AGENT_VAR_<NAME>`) and in its graph
//! state. An agent node's `inputs:` for a variable the child declares
//! outranks the inherited value in both places and is what the child's own
//! children inherit. A value the parent only holds as its own declared
//! default is not passed on. A config child is seated the same way, seen
//! through its instructions, which the dry-run client echoes back.
//!
//! Unix only: the child's script node runs through `bash`.
#![cfg(unix)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const PARENT: &str = "inherit-parent";
const MID: &str = "inherit-mid";
const CHILD: &str = "inherit-child";

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fresh_dir(label: &str) -> TempDirGuard {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!("coyote-var-inherit-{label}-{unique}"));
    fs::create_dir_all(dir.join("cache")).unwrap();
    TempDirGuard(dir)
}

/// A graph agent declaring `build_lock` with `default` whose one agent node
/// runs `target`, with `inputs` (a YAML line) spliced under it when given.
fn caller_graph(name: &str, default: &str, target: &str, inputs: Option<&str>) -> String {
    let inputs = inputs
        .map(|body| format!("\x20   inputs:\n\x20     {body}\n"))
        .unwrap_or_default();
    format!(
        "name: {name}\n\
         variables:\n\
         \x20 - name: build_lock\n\
         \x20   description: gate prefix\n\
         \x20   default: {default}\n\
         start: call\n\
         nodes:\n\
         \x20 call:\n\
         \x20   type: agent\n\
         \x20   agent: {target}\n\
         \x20   prompt: go\n\
         {inputs}\
         \x20   state_updates:\n\
         \x20     child_out: '{{{{output}}}}'\n\
         \x20   next: done\n\
         \x20 done:\n\
         \x20   type: end\n\
         \x20   output: \"{{{{child_out}}}}\"\n"
    )
}

/// A dry-run config dir with the parent, the reporting child and, when
/// `via_mid`, a middle graph agent between them that passes no `inputs:`.
fn write_fixture(dir: &Path, inputs: Option<&str>, via_mid: bool) {
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

    let child_dir = dir.join("agents").join(CHILD);
    fs::create_dir_all(&child_dir).unwrap();
    fs::write(
        child_dir.join("probe.sh"),
        "#!/usr/bin/env bash\nprintf '{\"seen\": \"%s\"}' \"${LLM_AGENT_VAR_BUILD_LOCK-unset}\"\n",
    )
    .unwrap();
    fs::write(
        child_dir.join("graph.yaml"),
        format!(
            "name: {CHILD}\n\
             variables:\n\
             \x20 - name: build_lock\n\
             \x20   description: gate prefix\n\
             \x20   default: child-default\n\
             start: probe\n\
             nodes:\n\
             \x20 probe:\n\
             \x20   type: script\n\
             \x20   script: probe.sh\n\
             \x20   state_updates: {{}}\n\
             \x20   next: done\n\
             \x20 done:\n\
             \x20   type: end\n\
             \x20   output: \"CHILD env=[{{{{seen}}}}] state=[{{{{build_lock}}}}]\"\n"
        ),
    )
    .unwrap();

    let first_hop = if via_mid {
        let mid_dir = dir.join("agents").join(MID);
        fs::create_dir_all(&mid_dir).unwrap();
        fs::write(
            mid_dir.join("graph.yaml"),
            caller_graph(MID, "mid-default", CHILD, None),
        )
        .unwrap();
        MID
    } else {
        CHILD
    };
    let parent_dir = dir.join("agents").join(PARENT);
    fs::create_dir_all(&parent_dir).unwrap();
    fs::write(
        parent_dir.join("graph.yaml"),
        caller_graph(PARENT, "parent-default", first_hop, inputs),
    )
    .unwrap();
}

fn run_parent(dir: &Path, cli_vars: &[&str]) -> Output {
    let mut args = vec!["--agent", PARENT];
    args.extend_from_slice(cli_vars);
    args.push("go");
    run_coyote(dir, &args)
}

/// Removes every inherited `COYOTE_*` variable, so a developer's own
/// override (`COYOTE_CONFIG_FILE` beats the scratch config dir) never
/// reaches the child; the harness then sets the ones it needs.
fn scrub_coyote_env(cmd: &mut Command) -> &mut Command {
    for (key, _) in env::vars_os() {
        if key.to_str().is_some_and(|k| k.starts_with("COYOTE_")) {
            cmd.env_remove(key);
        }
    }
    cmd
}

fn run_coyote(dir: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_coyote"));
    scrub_coyote_env(&mut cmd)
        .args(args)
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", dir)
        .env("NO_COLOR", "1")
        .env_remove("IS_SANDBOX")
        .env_remove("LLM_AGENT_VAR_BUILD_LOCK")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn child_line(label: &str, inputs: Option<&str>, via_mid: bool, cli_vars: &[&str]) -> String {
    let dir = fresh_dir(label);
    write_fixture(&dir.0, inputs, via_mid);
    let out = run_parent(&dir.0, cli_vars);
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{label}: exit {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    stdout
        .lines()
        .find(|line| line.starts_with("CHILD "))
        .unwrap_or_else(|| panic!("{label}: no CHILD line\nstdout: {stdout}\nstderr: {stderr}"))
        .to_string()
}

#[test]
fn cli_started_parent_hands_its_variables_to_a_graph_child() {
    let line = child_line(
        "cli",
        None,
        false,
        &["--agent-variable", "build_lock", "flock -w 9 lock"],
    );

    assert_eq!(line, "CHILD env=[flock -w 9 lock] state=[flock -w 9 lock]");
}

#[test]
fn cli_started_parents_variables_reach_a_grandchild() {
    let line = child_line(
        "grandchild",
        None,
        true,
        &["--agent-variable", "build_lock", "from-cli"],
    );

    assert_eq!(line, "CHILD env=[from-cli] state=[from-cli]");
}

#[test]
fn agent_node_inputs_outrank_the_inherited_value() {
    let line = child_line(
        "inputs",
        Some("build_lock: from-inputs"),
        false,
        &["--agent-variable", "build_lock", "from-cli"],
    );

    assert_eq!(line, "CHILD env=[from-inputs] state=[from-inputs]");
}

#[test]
fn agent_node_inputs_are_what_the_childs_own_children_inherit() {
    let line = child_line(
        "inputs-down",
        Some("build_lock: from-inputs"),
        true,
        &["--agent-variable", "build_lock", "from-cli"],
    );

    assert_eq!(line, "CHILD env=[from-inputs] state=[from-inputs]");
}

#[test]
fn a_parents_own_default_is_not_passed_to_the_child() {
    let line = child_line("default", None, false, &[]);

    assert_eq!(line, "CHILD env=[child-default] state=[child-default]");
}

#[test]
fn agent_node_inputs_seat_a_config_childs_declared_variable() {
    let dir = fresh_dir("config-child");
    write_fixture(&dir.0, Some("build_lock: from-inputs"), false);
    let child_dir = dir.0.join("agents").join(CHILD);
    fs::remove_file(child_dir.join("graph.yaml")).unwrap();
    fs::write(
        child_dir.join("config.yaml"),
        format!(
            "name: {CHILD}\n\
             instructions: 'CHILD var=[{{{{build_lock}}}}]'\n\
             variables:\n\
             \x20 - name: build_lock\n\
             \x20   description: gate prefix\n\
             \x20   default: child-default\n"
        ),
    )
    .unwrap();

    let out = run_parent(&dir.0, &["--agent-variable", "build_lock", "from-cli"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(child_lines(&out.stdout), ["CHILD var=[from-inputs]"]);
}

fn child_lines(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .replace("\r\n", "\n")
        .lines()
        .filter(|line| line.starts_with("CHILD "))
        .map(str::to_string)
        .collect()
}

#[test]
fn an_input_the_child_does_not_declare_stays_graph_state_only() {
    let dir = fresh_dir("undeclared");
    write_fixture(&dir.0, Some("review_cadence: per-task"), false);
    let child_dir = dir.0.join("agents").join(CHILD);
    fs::write(
        child_dir.join("probe.sh"),
        "#!/usr/bin/env bash\nprintf '{\"seen\": \"%s\"}' \"${LLM_AGENT_VAR_REVIEW_CADENCE-unset}\"\n",
    )
    .unwrap();
    let graph = fs::read_to_string(child_dir.join("graph.yaml"))
        .unwrap()
        .replace("state=[{{build_lock}}]", "state=[{{review_cadence}}]");
    fs::write(child_dir.join("graph.yaml"), graph).unwrap();

    let out = run_parent(&dir.0, &["--agent-variable", "build_lock", "from-cli"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        child_lines(&out.stdout),
        ["CHILD env=[unset] state=[per-task]"]
    );
}

#[test]
fn repl_agent_values_do_not_leak_into_a_later_agent() {
    let dir = fresh_dir("repl");
    write_fixture(&dir.0, None, false);
    fs::create_dir_all(dir.0.join("macros")).unwrap();
    fs::write(
        dir.0.join("macros").join("reenter.yaml"),
        format!(
            "isolated: false\nsteps:\n  - \".agent {PARENT} build_lock=from-repl\"\n  - \"go\"\n  - \".exit agent\"\n  - \".agent {PARENT}\"\n  - \"go\"\n"
        ),
    )
    .unwrap();

    let out = run_coyote(&dir.0, &["--macro", "reenter"]);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        child_lines(&out.stdout),
        [
            "CHILD env=[from-repl] state=[from-repl]",
            "CHILD env=[child-default] state=[child-default]",
        ]
    );
}
