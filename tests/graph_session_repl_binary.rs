//! Binary-level contract for the REPL-side graph turn gate, driven from a
//! cold config dir the way an operator would set it up (the local-run
//! recipe's dry-run model and `echo-graph` agent) through `--macro`, the
//! only non-interactive path that executes REPL lines through the real
//! binary. What is reachable without a session today: a prompt on a graph
//! agent runs the graph unrouted (no `routing:` line) whether or not the
//! graph carries a `session:` block, `.session` is still refused for a graph
//! agent, and the turn hooks of a config agent and of a session-less graph
//! agent carry no `COYOTE_GRAPH_*` variables. Every run uses a fresh fake
//! HOME and a scrubbed sandbox/provider env so host state cannot leak in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, process};

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const GRAPH_AGENT: &str = "echo-graph";
const CONFIG_AGENT: &str = "probe";
const TURN_EVENTS: [&str; 2] = ["turn.started", "turn.completed"];

/// The block-less graph from the local-run recipe; `session_block` is
/// appended verbatim. Every agent admits the global hooks so the turn
/// events reach the capture commands.
const BASE_GRAPH: &str = "name: echo-graph\n\
    version: \"1.0\"\n\
    description: Echoes the prompt.\n\
    global_hooks: [\"*\"]\n\
    initial_state: { said: \"\" }\n\
    start: done\n\
    nodes:\n\
    \x20 done: { type: end, output: \"{{initial_prompt}}\" }\n";

const RECIPE_BLOCK: &str = "session:\n  expose: [said]\n  chat:\n    instructions: Answer questions about the last run from its output.\n";

fn fresh_config_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-graph-repl-{label}-{}-{unique}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("agents").join(GRAPH_AGENT)).unwrap();
    fs::create_dir_all(dir.join("agents").join(CONFIG_AGENT)).unwrap();
    fs::create_dir_all(dir.join("macros")).unwrap();
    dir
}

/// Path of the file `event`'s hook dumps its environment into.
fn env_log(dir: &Path, event: &str) -> PathBuf {
    dir.join(format!("env-{event}.log"))
}

/// A hook command that writes the child's whole environment to the
/// event's own file, in the dialect of the shell the hook engine dispatches
/// through (`sh -c` elsewhere, `cmd /C` on Windows).
fn env_capture_command(dir: &Path, event: &str) -> String {
    let log = env_log(dir, event);
    let log = log.display();
    if cfg!(windows) {
        format!(r#"set >> "{log}""#)
    } else {
        format!(r#"env >> "{log}""#)
    }
}

fn write_fixture(dir: &Path, session_block: &str) {
    // Without IS_SANDBOX (scrubbed below) a run insists on a vault password
    // file, so the fixture provides one.
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    let hooks = TURN_EVENTS
        .iter()
        .map(|event| {
            format!(
                "\x20 {event}:\n\x20   - name: capture\n\x20     command: '{}'\n",
                env_capture_command(dir, event)
            )
        })
        .collect::<String>();
    // Paths land in single-quoted YAML scalars: double-quoted ones would read
    // the backslashes in a Windows temp path (`C:\Users\...`) as escapes.
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
             stream: false\n\
             hooks:\n\
             {hooks}",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
    fs::write(
        dir.join("agents").join(GRAPH_AGENT).join("graph.yaml"),
        format!("{BASE_GRAPH}{session_block}"),
    )
    .unwrap();
    fs::write(
        dir.join("agents").join(CONFIG_AGENT).join("config.yaml"),
        "name: probe\n\
         description: a plain config agent\n\
         version: 0.1.0\n\
         instructions: |\n\
         \x20 You are a probe agent. Reply briefly.\n\
         global_hooks:\n\
         \x20 - \"*\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("macros/say.yaml"),
        "isolated: false\nsteps:\n  - \"hello\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("macros/engage.yaml"),
        "isolated: false\nsteps:\n  - \".session demo\"\n  - \"hello\"\n",
    )
    .unwrap();
}

fn run_coyote(dir: &Path, args: &[&str]) -> Output {
    let home = dir.join("home");
    Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(args)
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("COYOTE_WORKSPACE_CONFIG_DIR")
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn combined(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        stdout_of(output),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The dry-run graph echoes its prompt from the end node, so a plain
/// prompt step prints exactly `hello` on a line of its own and, with no
/// session to attach to, never prints the classifier's `routing:` line.
fn assert_unrouted_echo(label: &str, output: &Output) -> String {
    let text = combined(output);
    assert!(
        output.status.success(),
        "{label}: exit {:?}\n{text}",
        output.status
    );
    assert!(
        !text.contains("panicked"),
        "{label}: the binary panicked:\n{text}"
    );
    let stdout = stdout_of(output);
    assert!(
        stdout.lines().any(|line| line == "hello"),
        "{label}: the graph's echo is missing:\n{text}"
    );
    assert!(
        !stdout.lines().any(|line| line.contains("routing:")),
        "{label}: a session-less turn is never routed:\n{text}"
    );
    stdout
}

/// Bounded poll until the event's capture file carries the hook's own
/// COYOTE_EVENT line: the binary's exit drain only acknowledges that each
/// hook process spawned, the dump itself lands a moment later.
fn wait_for_env_dump(dir: &Path, event: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let dump = fs::read_to_string(env_log(dir, event)).unwrap_or_default();
        if dump
            .lines()
            .any(|line| line.trim_end() == format!("COYOTE_EVENT={event}"))
        {
            return dump;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the {event} hook's environment dump; so far:\n{dump}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn assert_no_graph_vars(label: &str, event: &str, dump: &str) {
    for key in ["COYOTE_GRAPH_BRANCH", "COYOTE_GRAPH_ROUTE"] {
        assert!(
            !dump
                .lines()
                .any(|line| line.starts_with(&format!("{key}="))),
            "{label}: {event} must not carry {key}:\n{dump}"
        );
    }
}

/// A prompt on a graph agent with no session engaged runs the graph as it
/// always has, whether or not the graph declares a `session:` block, and
/// the two print the same thing.
#[test]
fn a_session_less_prompt_runs_the_graph_unrouted_with_or_without_a_session_block() {
    let mut outputs = Vec::new();
    for (label, block) in [("no block", ""), ("recipe block", RECIPE_BLOCK)] {
        let dir = fresh_config_dir("unrouted");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);

        let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "say"]);
        outputs.push(assert_unrouted_echo(label, &output));
    }
    assert_eq!(
        outputs[0], outputs[1],
        "a session block changes nothing about a session-less turn"
    );
}

/// `.session` on a graph agent is still refused (the session surface for
/// graph agents is not open yet), the refusal names the agent, and nothing
/// panics.
#[test]
fn session_is_still_refused_for_a_graph_agent() {
    let dir = fresh_config_dir("refused");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_BLOCK);

    let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "engage"]);

    let text = combined(&output);
    assert!(
        !output.status.success(),
        "a refused macro step fails the run:\n{text}"
    );
    assert!(!text.contains("panicked"), "the binary panicked:\n{text}");
    assert!(
        text.contains("Graph-based agent 'echo-graph' does not support sessions"),
        "the refusal must name the agent:\n{text}"
    );
    assert!(
        !text.contains("routing:"),
        "no turn is routed when the session step is refused:\n{text}"
    );
}

/// Turn hooks fired by a config agent's turn, and by a session-less graph
/// agent's turn, carry no graph variables (started never does; completed
/// only when a session-attached graph turn was routed).
#[test]
fn turn_hooks_carry_no_graph_vars_for_a_config_agent_or_a_session_less_graph_agent() {
    for (label, agent) in [("config agent", CONFIG_AGENT), ("graph agent", GRAPH_AGENT)] {
        let dir = fresh_config_dir("hooks");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, RECIPE_BLOCK);

        let output = run_coyote(&dir, &["-a", agent, "--macro", "say"]);
        let text = combined(&output);
        assert!(
            output.status.success(),
            "{label}: exit {:?}\n{text}",
            output.status
        );
        assert!(
            !text.contains("panicked"),
            "{label}: the binary panicked:\n{text}"
        );

        for event in TURN_EVENTS {
            let dump = wait_for_env_dump(&dir, event);
            assert_no_graph_vars(label, event, &dump);
        }
    }
}
