//! Binary-level contract for the REPL-side graph turn gate, driven from a
//! cold config dir the way an operator would set it up (the local-run
//! recipe's dry-run model and `echo-graph` agent) through `--macro`, the
//! only non-interactive path that executes REPL lines through the real
//! binary. What is reachable without a session today: a prompt on a graph
//! agent runs the graph unrouted (no `routing:` line) whether or not the
//! graph carries a `session:` block, `.session` engages on a graph agent
//! whose graph carries a `session:` block and is refused with the teaching
//! text on one that does not, and the turn hooks of a config agent and of a
//! session-less graph agent carry no `COYOTE_GRAPH_*` variables. Every run
//! uses a fresh fake HOME and a scrubbed sandbox/provider env so host state
//! cannot leak in.

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
    // The dry-run model echoes the classifier's own prompt back, so an
    // unforced prompt under an engaged session can never route; `.run`
    // forces the branch and still prints the routing line.
    fs::write(
        dir.join("macros/engage.yaml"),
        "isolated: false\nsteps:\n  - \".session demo\"\n  - \".run hello\"\n",
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
    for key in [
        "COYOTE_GRAPH_BRANCH",
        "COYOTE_GRAPH_ROUTE",
        "COYOTE_GRAPH_CHAT_AGENT",
    ] {
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

/// `.session` on a graph agent whose graph carries a `session:` block
/// engages, and the forced run that follows runs the graph through the
/// routed path.
#[test]
fn session_engages_for_a_graph_agent_with_a_session_block() {
    let dir = fresh_config_dir("engaged");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_BLOCK);

    let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "engage"]);

    let text = combined(&output);
    assert!(
        output.status.success(),
        "engaging a session on a block-bearing graph succeeds: exit {:?}\n{text}",
        output.status
    );
    assert!(!text.contains("panicked"), "the binary panicked:\n{text}");
    let stdout = stdout_of(&output);
    assert!(
        stdout.lines().any(|line| line == "hello"),
        "the graph's echo is missing:\n{text}"
    );
    assert!(
        stdout.lines().any(|line| line.contains("routing: run")),
        "a turn under an engaged session is routed:\n{text}"
    );
}

/// `.session` on a graph agent whose graph has no `session:` block is
/// refused with the teaching text, and no turn is routed.
#[test]
fn session_is_refused_for_a_graph_agent_without_a_session_block() {
    let dir = fresh_config_dir("refused");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");

    let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "engage"]);

    let text = combined(&output);
    assert!(
        !output.status.success(),
        "a refused macro step fails the run:\n{text}"
    );
    assert!(!text.contains("panicked"), "the binary panicked:\n{text}");
    assert!(
        text.contains(TEACHING_TEXT),
        "the refusal must carry the teaching text:\n{text}"
    );
    assert!(
        !has_routing_line(&output),
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

/// Writes `macros/<name>.yaml` with the given REPL lines as its steps.
fn write_macro(dir: &Path, name: &str, steps: &[&str]) {
    let steps = steps
        .iter()
        .map(|step| format!("  - {step:?}\n"))
        .collect::<String>();
    fs::write(
        dir.join("macros").join(format!("{name}.yaml")),
        format!("isolated: false\nsteps:\n{steps}"),
    )
    .unwrap();
}

/// Switches the fixture's config to the slash command prefix.
fn use_slash_prefix(dir: &Path) {
    let config = dir.join("config.yaml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str("command_prefix: slash\n");
    fs::write(config, text).unwrap();
}

const TEACHING_TEXT: &str = "Graph-based agent 'echo-graph' has no `session:` block in its graph.yaml, \
     so it cannot hold a session. Add a `session:` block (see the Graph Agents \
     wiki page, \"Sessions\") to enable one.";

/// Scans both streams: the routing line is printed to stdout, but a negative
/// check must not pass merely because the line landed on the other stream.
fn has_routing_line(output: &Output) -> bool {
    combined(output)
        .lines()
        .any(|line| line.contains("routing:"))
}

/// `.run` from a cold start, under both command prefixes: a bare `.run`
/// prints the usage hint spelled with the active prefix and runs nothing,
/// `.run x` on a config agent is refused with the rendered text, and `.run
/// hello` on a block-less graph agent with no session runs the graph
/// unrouted. Every command-naming string follows the configured prefix.
#[test]
fn run_command_cold_start_paths_follow_the_command_prefix() {
    for (prefix, slash) in [(".", false), ("/", true)] {
        let dir = fresh_config_dir("run-cmd");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, "");
        if slash {
            use_slash_prefix(&dir);
        }
        write_macro(&dir, "bare", &[&format!("{prefix}run")]);
        write_macro(&dir, "on-config", &[&format!("{prefix}run x")]);
        write_macro(&dir, "forced", &[&format!("{prefix}run hello")]);

        let bare = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "bare"]);
        let text = combined(&bare);
        assert!(
            bare.status.success(),
            "prefix {prefix}: a bare run command is a hint, not an error: exit {:?}\n{text}",
            bare.status
        );
        assert!(
            stdout_of(&bare)
                .lines()
                .any(|line| line == format!("Usage: {prefix}run <prompt>")),
            "prefix {prefix}: the usage hint is spelled with the active prefix:\n{text}"
        );
        assert!(
            !stdout_of(&bare).lines().any(|line| line == "hello") && !has_routing_line(&bare),
            "prefix {prefix}: a bare run command runs nothing:\n{text}"
        );

        let on_config = run_coyote(&dir, &["-a", CONFIG_AGENT, "--macro", "on-config"]);
        let text = combined(&on_config);
        assert!(
            !on_config.status.success(),
            "prefix {prefix}: the run command is refused on a config agent:\n{text}"
        );
        assert!(
            text.contains(&format!(
                "{prefix}run only applies to graph-based agents; '{CONFIG_AGENT}' is a config agent"
            )),
            "prefix {prefix}: the refusal names the command with the active prefix and the agent:\n{text}"
        );
        assert!(
            !text.contains("panicked"),
            "prefix {prefix}: the binary panicked:\n{text}"
        );

        let forced = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "forced"]);
        assert_unrouted_echo(
            &format!("prefix {prefix}: forced run without a session"),
            &forced,
        );
    }
}

/// The rest of a `.run` line is the prompt, even when it is spelled like a
/// REPL command: `.run .session demo` runs the graph with `.session demo`
/// as its prompt instead of engaging a session.
#[test]
fn run_command_takes_a_command_shaped_prompt_literally() {
    let dir = fresh_config_dir("run-literal");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_BLOCK);
    write_macro(&dir, "literal", &[".run .session demo"]);

    let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "literal"]);

    let text = combined(&output);
    assert!(output.status.success(), "exit {:?}\n{text}", output.status);
    assert!(
        stdout_of(&output)
            .lines()
            .any(|line| line == ".session demo"),
        "the graph echoes the command-shaped prompt verbatim:\n{text}"
    );
    assert!(
        !has_routing_line(&output),
        "no session was engaged by the prompt, so the turn is unrouted:\n{text}"
    );
}

/// `-a <graph> --session <name>` (the explicit-session path of `use_agent`)
/// follows the same predicate as the `.session` command: refused with the
/// teaching text on a block-less graph, engaged on one with a block so the
/// forced run that follows is routed.
#[test]
fn explicit_session_flag_follows_the_session_block() {
    for (label, block, expect_engaged) in [
        ("no block", "", false),
        ("recipe block", RECIPE_BLOCK, true),
    ] {
        let dir = fresh_config_dir("session-flag");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);
        write_macro(&dir, "forced", &[".run hello"]);

        let output = run_coyote(
            &dir,
            &["-a", GRAPH_AGENT, "--session", "demo", "--macro", "forced"],
        );
        let text = combined(&output);
        assert!(
            !text.contains("panicked"),
            "{label}: the binary panicked:\n{text}"
        );
        if expect_engaged {
            assert!(
                output.status.success(),
                "{label}: exit {:?}\n{text}",
                output.status
            );
            assert!(
                stdout_of(&output).lines().any(|line| line == "hello"),
                "{label}: the graph's echo is missing:\n{text}"
            );
            assert!(
                stdout_of(&output)
                    .lines()
                    .any(|line| line.contains("routing: run")),
                "{label}: a forced run under the flag-engaged session is routed:\n{text}"
            );
        } else {
            assert!(
                !output.status.success(),
                "{label}: the explicit session is refused:\n{text}"
            );
            assert!(
                text.contains(TEACHING_TEXT),
                "{label}: the refusal carries the teaching text:\n{text}"
            );
            assert!(
                !has_routing_line(&output),
                "{label}: nothing is routed after the refusal:\n{text}"
            );
        }
    }
}

/// A session saved while the graph carried a `session:` block can be
/// resumed by name on a later cold start; once the block is removed from
/// graph.yaml the same saved session is refused with the teaching text and
/// no turn is routed.
#[test]
fn a_saved_session_is_refused_once_the_block_is_removed() {
    let dir = fresh_config_dir("saved");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_BLOCK);
    write_macro(
        &dir,
        "record",
        &[
            ".session demo",
            ".run hello",
            ".save session",
            ".exit session",
        ],
    );
    write_macro(&dir, "resume", &[".session demo", ".run again"]);

    let record = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "record"]);
    let text = combined(&record);
    assert!(
        record.status.success(),
        "record: exit {:?}\n{text}",
        record.status
    );
    let saved = dir
        .join("agents")
        .join(GRAPH_AGENT)
        .join("sessions")
        .join("demo.yaml");
    assert!(saved.is_file(), "the session file was not written:\n{text}");

    let resume = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "resume"]);
    let text = combined(&resume);
    assert!(
        resume.status.success(),
        "resume: exit {:?}\n{text}",
        resume.status
    );
    assert!(
        stdout_of(&resume).lines().any(|line| line == "again")
            && stdout_of(&resume)
                .lines()
                .any(|line| line.contains("routing: run")),
        "resume: the saved session engages and the forced run is routed:\n{text}"
    );

    let graph = dir.join("agents").join(GRAPH_AGENT).join("graph.yaml");
    fs::write(&graph, BASE_GRAPH).unwrap();
    let refused = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "resume"]);
    let text = combined(&refused);
    assert!(
        !refused.status.success(),
        "block removed: resuming the saved session is refused:\n{text}"
    );
    assert!(
        text.contains(TEACHING_TEXT),
        "block removed: the refusal carries the teaching text:\n{text}"
    );
    assert!(
        !has_routing_line(&refused) && !text.contains("panicked"),
        "block removed: nothing is routed and nothing panics:\n{text}"
    );
}

/// `.undo` on a block-less graph agent is refused with the teaching text
/// from a cold start, before any session state is consulted.
#[test]
fn undo_on_a_blockless_graph_agent_is_refused_with_the_teaching_text() {
    let dir = fresh_config_dir("undo");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");
    write_macro(&dir, "undo", &[".undo"]);

    let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "undo"]);

    let text = combined(&output);
    assert!(!output.status.success(), "undo is refused:\n{text}");
    assert!(
        text.contains(TEACHING_TEXT),
        "the refusal carries the teaching text:\n{text}"
    );
    assert!(
        !text.contains("Nothing to undo") && !text.contains("No session"),
        "the session-block gate precedes every session-state message:\n{text}"
    );
}

/// A forced run under an engaged session marks its `turn.completed` hook
/// with `COYOTE_GRAPH_BRANCH=run` and `COYOTE_GRAPH_ROUTE=forced`; the
/// `turn.started` hook still carries neither variable.
#[test]
fn a_forced_run_under_an_engaged_session_marks_its_completed_hook_forced() {
    let dir = fresh_config_dir("forced-hooks");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_BLOCK);

    let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "engage"]);
    let text = combined(&output);
    assert!(output.status.success(), "exit {:?}\n{text}", output.status);

    let started = wait_for_env_dump(&dir, "turn.started");
    assert_no_graph_vars("forced run", "turn.started", &started);

    let completed = wait_for_env_dump(&dir, "turn.completed");
    for (key, value) in [
        ("COYOTE_GRAPH_BRANCH", "run"),
        ("COYOTE_GRAPH_ROUTE", "forced"),
    ] {
        assert!(
            completed
                .lines()
                .any(|line| line.trim_end() == format!("{key}={value}")),
            "turn.completed must carry {key}={value}:\n{completed}"
        );
    }
}

/// A saved session that carries a paused auto-continue from an earlier
/// escalation, with one item still open.
const PAUSED_SESSION: &str = "model: dryrun:dry-model\n\
    messages: []\n\
    todo_list:\n\
    \x20 goal: Ship feature\n\
    \x20 todos:\n\
    \x20   - id: 1\n\
    \x20     desc: Write code\n\
    \x20     done: false\n\
    auto_continue_paused: Which database?\n";

/// `.run <prompt>` opens a fresh turn the way a plain prompt does: the pause
/// a resumed session carried is lifted and the next save no longer records
/// it, while the bare `.run` hint runs no turn and leaves the pause in place.
/// The open todo item survives either way. (An unforced prompt cannot stand
/// in for the plain-prompt side here: under the dry-run model the route
/// classifier never reaches a decision, so that turn fails before saving.)
#[test]
fn a_run_lifts_a_saved_auto_continue_pause_as_a_plain_prompt_does() {
    for (label, step, expect_paused) in [
        ("forced run", ".run hello", false),
        ("bare run", ".run", true),
    ] {
        let dir = fresh_config_dir("run-pause");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, RECIPE_BLOCK);
        let sessions = dir.join("agents").join(GRAPH_AGENT).join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let saved = sessions.join("paused.yaml");
        fs::write(&saved, PAUSED_SESSION).unwrap();
        write_macro(
            &dir,
            "turn",
            &[".session paused", step, ".save session", ".exit session"],
        );

        let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "turn"]);
        let text = combined(&output);
        assert!(
            output.status.success() && !text.contains("panicked"),
            "{label}: exit {:?}\n{text}",
            output.status
        );

        let yaml = fs::read_to_string(&saved).unwrap();
        assert_eq!(
            yaml.contains("auto_continue_paused: Which database?"),
            expect_paused,
            "{label}: the saved session's pause after the step:\n{yaml}\n{text}"
        );
        assert!(
            yaml.contains("desc: Write code"),
            "{label}: the open todo item must survive the save:\n{yaml}"
        );
    }
}

/// `.run` is a built-in the operator can discover: the real binary's help
/// listing carries its row, spelled with the active command prefix and
/// described as skipping intent routing, from a cold start without any
/// agent engaged.
#[test]
fn usage_probe_help_lists_the_run_command_under_both_prefixes() {
    for (prefix, slash) in [(".", false), ("/", true)] {
        let dir = fresh_config_dir("help-run");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, "");
        if slash {
            use_slash_prefix(&dir);
        }
        write_macro(&dir, "help", &[&format!("{prefix}help")]);

        let output = run_coyote(&dir, &["--macro", "help"]);

        let text = combined(&output);
        assert!(
            output.status.success() && !text.contains("panicked"),
            "prefix {prefix}: exit {:?}\n{text}",
            output.status
        );
        let rows: Vec<String> = stdout_of(&output)
            .lines()
            .filter(|line| line.starts_with(&format!("{prefix}run ")))
            .map(|line| line.trim_end().to_string())
            .collect();
        assert_eq!(
            rows.len(),
            1,
            "prefix {prefix}: exactly one help row names the run command:\n{text}"
        );
        assert!(
            rows[0].contains("Run the graph with this prompt, skipping intent routing"),
            "prefix {prefix}: the run row carries its description:\n{text}"
        );
        let other = if slash { ".run " } else { "/run " };
        assert!(
            !stdout_of(&output)
                .lines()
                .any(|line| line.starts_with(other)),
            "prefix {prefix}: the row is spelled only with the active prefix:\n{text}"
        );
    }
}

/// The loader checks the keys inside `session`, `route` and `chat`, not the
/// block's own name: a graph whose block is spelled `sesion:` loads without
/// a word of complaint, runs its graph on a bare prompt like any block-less
/// graph, and turns `.session` down with the teaching text.
#[test]
fn usage_probe_a_mistyped_top_level_block_name_silently_disables_sessions() {
    let dir = fresh_config_dir("sesion");
    let _guard = TempDirGuard(dir.clone());
    let typo = RECIPE_BLOCK.replacen("session:", "sesion:", 1);
    assert_ne!(typo, RECIPE_BLOCK);
    write_fixture(&dir, &typo);

    let said = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "say"]);
    assert_unrouted_echo("sesion: bare prompt", &said);
    let text = combined(&said);
    assert!(
        !text.contains("unknown field") && !text.contains("sesion"),
        "a mistyped block name draws no load-time complaint:\n{text}"
    );

    let engaged = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "engage"]);
    let text = combined(&engaged);
    assert!(
        !engaged.status.success() && text.contains(TEACHING_TEXT),
        "with the block misspelled the agent holds no session:\n{text}"
    );
    assert!(
        !has_routing_line(&engaged) && !text.contains("panicked"),
        "nothing is routed and nothing panics:\n{text}"
    );
}

/// `.run` is a built-in command: `.run hello` runs the graph even when the
/// operator has a macro named `run`, and that macro is reached through
/// `.macro run` (from an isolated macro here, the only way a `--macro`
/// cold start can nest one).
#[test]
fn usage_probe_run_is_a_builtin_so_a_user_macro_named_run_needs_dot_macro() {
    let dir = fresh_config_dir("macro-run");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_BLOCK);
    write_macro(&dir, "run", &["MACRO_RAN"]);
    write_macro(&dir, "direct", &[".run hello"]);
    fs::write(
        dir.join("macros").join("viamacro.yaml"),
        "isolated: true\nsteps:\n  - \".macro run\"\n",
    )
    .unwrap();

    let direct = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "direct"]);
    let stdout = assert_unrouted_echo(".run with a macro named run present", &direct);
    assert!(
        !stdout.contains("MACRO_RAN"),
        ".run must not fall through to the user macro:\n{}",
        combined(&direct)
    );

    let via = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "viamacro"]);
    let text = combined(&via);
    assert!(
        via.status.success() && !text.contains("panicked"),
        ".macro run: exit {:?}\n{text}",
        via.status
    );
    assert!(
        stdout_of(&via).lines().any(|line| line == "MACRO_RAN"),
        ".macro run reaches the user macro named run:\n{text}"
    );
}

/// Under `dry_run` the router's model echoes the prompt instead of a
/// decision, so a bare prompt typed into an engaged session fails the turn
/// with no silent default, no `routing:` line and no graph run; `.run` on
/// the same session still works.
#[test]
fn usage_probe_a_routed_prompt_under_dry_run_fails_at_the_classifier_while_run_still_works() {
    let dir = fresh_config_dir("classifier");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_BLOCK);
    write_macro(&dir, "routed", &[".session demo", "hello"]);

    let routed = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "routed"]);
    let text = combined(&routed);
    assert!(
        !routed.status.success(),
        "a routed turn that cannot be classified fails the run:\n{text}"
    );
    assert!(
        text.contains("Graph route classifier could not read the decision"),
        "the failure names the classifier:\n{text}"
    );
    assert!(
        !has_routing_line(&routed) && !text.contains("panicked"),
        "no branch is chosen, so no routing line prints and nothing panics:\n{text}"
    );
    assert!(
        !text.contains("graph: echo-graph"),
        "the graph never starts when the router fails:\n{text}"
    );

    let forced = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "engage"]);
    let text = combined(&forced);
    assert!(
        forced.status.success()
            && stdout_of(&forced).lines().any(|line| line == "hello")
            && stdout_of(&forced)
                .lines()
                .any(|line| line.contains("routing: run")),
        ".run on the same session still runs the graph:\n{text}"
    );
}

/// The graph whose first node crashes and is absorbed by `fallback:`; the
/// script lives next to graph.yaml because a graph agent's scripts resolve
/// against the agent's own directory.
const FALLBACK_GRAPH: &str = "name: echo-graph\n\
    version: \"1.0\"\n\
    description: Crashes, then recovers over the fallback edge.\n\
    global_hooks: [\"*\"]\n\
    initial_state: { said: \"\" }\n\
    start: crash\n\
    nodes:\n\
    \x20 crash: { type: script, script: crash.sh, fallback: recover, next: done }\n\
    \x20 recover: { type: end, output: recovered }\n\
    \x20 done: { type: end, output: done }\n";

/// A forced run whose first node crashes under `fallback:` completes from
/// the fallback node, and the session saved afterwards records that hop in
/// its trace as a route to the fallback target (not the declared `next`)
/// carrying `fell_back: true`, then the fallback node's own end step; a
/// cold-start resume still engages the session and runs again.
#[test]
fn usage_probe_a_fallback_absorbed_crash_is_saved_in_the_session_trace_as_routed() {
    if cfg!(windows) {
        eprintln!("skipping: the crashing node is a bash script");
        return;
    }
    let dir = fresh_config_dir("fallback");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_BLOCK);
    // The recipe's dry run records no exchange and no last_run by contract,
    // so this fixture runs for real: the graph is script-only and the forced
    // branch skips the classifier, so no model is ever called.
    let config = dir.join("config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    assert!(
        text.contains("dry_run: true\n"),
        "the shared fixture is expected to opt into dry run:\n{text}"
    );
    fs::write(&config, text.replace("dry_run: true\n", "dry_run: false\n")).unwrap();
    let agent_dir = dir.join("agents").join(GRAPH_AGENT);
    fs::write(
        agent_dir.join("graph.yaml"),
        format!("{FALLBACK_GRAPH}{RECIPE_BLOCK}"),
    )
    .unwrap();
    fs::write(agent_dir.join("crash.sh"), "#!/bin/bash\nexit 3\n").unwrap();
    write_macro(
        &dir,
        "record",
        &[
            ".session demo",
            ".run hello",
            ".save session",
            ".exit session",
        ],
    );

    let record = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "record"]);
    let text = combined(&record);
    assert!(
        record.status.success(),
        "record: exit {:?}\n{text}",
        record.status
    );
    let stdout = stdout_of(&record);
    assert!(
        stdout.lines().any(|line| line == "recovered")
            && !stdout.lines().any(|line| line == "done"),
        "the run completes from the fallback node, not the declared next:\n{text}"
    );
    assert!(
        stdout.lines().any(|line| line.contains("routing: run")),
        "the forced run prints its routing line:\n{text}"
    );

    let saved = agent_dir.join("sessions").join("demo.yaml");
    let yaml = fs::read_to_string(&saved)
        .unwrap_or_else(|e| panic!("the session file was not written ({e}):\n{text}"));
    let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    let last_run = &doc["last_run"];
    assert_eq!(
        last_run["output"].as_str(),
        Some("recovered"),
        "last_run.output is the fallback node's output:\n{yaml}"
    );
    assert_eq!(
        last_run["prompt"].as_str(),
        Some("hello"),
        "last_run.prompt is the bare prompt:\n{yaml}"
    );
    let trace = last_run["trace"]
        .as_sequence()
        .unwrap_or_else(|| panic!("last_run.trace is a sequence:\n{yaml}"));
    let steps: Vec<(&str, &str, u64, serde_yaml::Value, bool)> = trace
        .iter()
        .map(|step| {
            (
                step["node_id"].as_str().unwrap(),
                step["node_type"].as_str().unwrap(),
                step["visit"].as_u64().unwrap(),
                step["outcome"].clone(),
                step["fell_back"].as_bool().unwrap(),
            )
        })
        .collect();
    // serde_yaml writes the enum as a YAML tag, so the saved file reads
    // `outcome: !routed { to: recover }` plus `fell_back: true` for the
    // absorbed hop.
    let routed_to = |id: &str| {
        serde_yaml::from_str::<serde_yaml::Value>(&format!("!routed {{ to: {id} }}")).unwrap()
    };
    let end = serde_yaml::Value::String("end".into());
    assert_eq!(
        steps,
        vec![
            ("crash", "script", 1, routed_to("recover"), true),
            ("recover", "end", 1, end, false),
        ],
        "the absorbed crash is traced as a fallback route to its target:\n{yaml}"
    );

    write_macro(&dir, "resume", &[".session demo", ".run again"]);
    let resume = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "resume"]);
    let text = combined(&resume);
    assert!(
        resume.status.success()
            && stdout_of(&resume).lines().any(|line| line == "recovered")
            && stdout_of(&resume)
                .lines()
                .any(|line| line.contains("routing: run")),
        "resume: the saved session engages and the forced run recovers again:\n{text}"
    );
}

/// A delegating `chat:` block (`agent: helper`, a config agent) engages a
/// session on a cold start and resumes saved ones whatever their vintage:
/// an empty file saved before the `chat_agent*` keys existed is seated as a
/// new session and records the delegate it was seated with, and a file with
/// messages that already carries `chat_agent`, `chat_agent_variables` and
/// `chat_agent_instructions` for that same delegate keeps them byte-for-byte
/// across a `.save session`, its recorded variable winning over the block's.
/// A file with messages but no `chat_agent*` keys, or one born under another
/// chat agent, is re-seated under the block's delegate with a warning while
/// its recorded system message stays as it was. The forced run is routed and
/// the graph still answers as itself.
#[test]
fn usage_probe_a_delegating_block_resumes_sessions_of_every_vintage_and_seats_the_delegate() {
    const DELEGATING_BLOCK: &str =
        "session:\n  expose: [said]\n  chat:\n    agent: helper\n    variables: { tone: terse }\n";
    const LEGACY_SESSION: &str = "model: dryrun:dry-model\nmessages: []\n";
    const INLINE_SESSION: &str = "model: dryrun:dry-model\n\
        messages:\n\
        - role: system\n\
        \x20 content: Be terse.\n\
        - role: user\n\
        \x20 content: hi\n\
        - role: assistant\n\
        \x20 content: ok\n";
    const DELEGATE_SESSION: &str = "model: dryrun:dry-model\n\
        messages:\n\
        - role: system\n\
        \x20 content: Be terse.\n\
        - role: user\n\
        \x20 content: hi\n\
        - role: assistant\n\
        \x20 content: ok\n\
        chat_agent: helper\n\
        chat_agent_variables:\n\
        \x20 tone: curt\n\
        chat_agent_instructions: Be terse.\n";
    const OTHER_DELEGATE_SESSION: &str = "model: dryrun:dry-model\n\
        messages:\n\
        - role: system\n\
        \x20 content: Be other.\n\
        - role: user\n\
        \x20 content: hi\n\
        - role: assistant\n\
        \x20 content: ok\n\
        chat_agent: other\n\
        chat_agent_variables:\n\
        \x20 tone: curt\n\
        chat_agent_instructions: Be other.\n";
    const INLINE_WARNING: &str = "Warning: session 'demo' was born with inline chat instructions; its recorded system prompt stays as is";
    const OTHER_WARNING: &str = "Warning: session 'demo' was born with chat agent 'other'; its recorded system prompt stays as is";

    struct Row {
        label: &'static str,
        saved_yaml: &'static str,
        /// The delegate variable the save records: the file's own when it
        /// was born under this delegate, the block's otherwise.
        tone: &'static str,
        /// Whether `chat_agent_instructions` survive the save as recorded
        /// rather than being re-rendered from the delegate.
        keeps_recorded_instructions: bool,
        /// The system message the file carried, which no re-seating rewrites.
        recorded_prompt: Option<&'static str>,
        warning: Option<&'static str>,
    }
    let rows = [
        Row {
            label: "empty session file without chat_agent fields",
            saved_yaml: LEGACY_SESSION,
            tone: "terse",
            keeps_recorded_instructions: false,
            recorded_prompt: None,
            warning: None,
        },
        Row {
            label: "session file with messages and chat_agent fields",
            saved_yaml: DELEGATE_SESSION,
            tone: "curt",
            keeps_recorded_instructions: true,
            recorded_prompt: Some("Be terse."),
            warning: None,
        },
        Row {
            label: "session file with messages and no chat_agent fields",
            saved_yaml: INLINE_SESSION,
            tone: "terse",
            keeps_recorded_instructions: false,
            recorded_prompt: Some("Be terse."),
            warning: Some(INLINE_WARNING),
        },
        Row {
            label: "session file born under another chat agent",
            saved_yaml: OTHER_DELEGATE_SESSION,
            tone: "terse",
            keeps_recorded_instructions: false,
            recorded_prompt: Some("Be other."),
            warning: Some(OTHER_WARNING),
        },
    ];
    for Row {
        label,
        saved_yaml,
        tone,
        keeps_recorded_instructions,
        recorded_prompt,
        warning,
    } in rows
    {
        let dir = fresh_config_dir("delegate-session");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, DELEGATING_BLOCK);
        // Dry run records no exchange and no last_run by contract; the graph
        // is end-only and the forced branch skips the classifier, so running
        // for real never calls a model and the save has something to write.
        let config = dir.join("config.yaml");
        let text = fs::read_to_string(&config).unwrap();
        assert!(text.contains("dry_run: true\n"), "{text}");
        fs::write(&config, text.replace("dry_run: true\n", "dry_run: false\n")).unwrap();
        let helper = dir.join("agents").join("helper");
        fs::create_dir_all(&helper).unwrap();
        fs::write(
            helper.join("config.yaml"),
            "name: helper\ndescription: A helper.\ninstructions: Be brief.\nvariables:\n  - name: tone\n    description: t\n",
        )
        .unwrap();
        let sessions = dir.join("agents").join(GRAPH_AGENT).join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        let saved = sessions.join("demo.yaml");
        fs::write(&saved, saved_yaml).unwrap();
        write_macro(
            &dir,
            "resume",
            &[
                ".session demo",
                ".run hello",
                ".save session",
                ".exit session",
            ],
        );

        let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "resume"]);
        let text = combined(&output);
        assert!(
            output.status.success() && !text.contains("panicked"),
            "{label}: exit {:?}\n{text}",
            output.status
        );
        let stdout = stdout_of(&output);
        assert!(
            stdout.lines().any(|line| line == "hello")
                && stdout.lines().any(|line| line.contains("routing: run")),
            "{label}: the forced run under the resumed session is routed and echoes:\n{text}"
        );
        assert!(
            !text.contains("session.chat"),
            "{label}: a valid delegating block prints no session.chat line:\n{text}"
        );
        match warning {
            Some(warning) => assert!(
                text.contains(warning),
                "{label}: the re-seating warning is printed:\n{text}"
            ),
            None => assert!(
                !text.contains("was born with"),
                "{label}: a session seated under this delegate draws no warning:\n{text}"
            ),
        }

        let yaml = fs::read_to_string(&saved).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            doc["last_run"]["prompt"].as_str(),
            Some("hello"),
            "{label}: the run was recorded on the resumed session:\n{yaml}"
        );
        assert_eq!(
            doc["chat_agent"].as_str(),
            Some("helper"),
            "{label}: chat_agent is recorded:\n{yaml}"
        );
        assert_eq!(
            doc["chat_agent_variables"]["tone"].as_str(),
            Some(tone),
            "{label}: chat_agent_variables are recorded:\n{yaml}"
        );
        let instructions = doc["chat_agent_instructions"].as_str().unwrap_or_default();
        if keeps_recorded_instructions {
            assert_eq!(
                instructions, "Be terse.",
                "{label}: chat_agent_instructions survive the save:\n{yaml}"
            );
        } else {
            assert!(
                instructions.starts_with("Be brief."),
                "{label}: chat_agent_instructions are the delegate's rendered prompt:\n{yaml}"
            );
        }
        let system_prompt = doc["messages"][0]["content"].as_str().unwrap_or_default();
        match recorded_prompt {
            Some(recorded_prompt) => assert_eq!(
                system_prompt, recorded_prompt,
                "{label}: the recorded system prompt stays as is:\n{yaml}"
            ),
            // The system message carries the rendered role, which may append
            // the skill guidance after the agent's own instructions.
            None => {
                assert!(
                    system_prompt.starts_with("Be brief."),
                    "{label}: the recorded system prompt is the delegate's:\n{yaml}"
                );
                assert!(
                    system_prompt.starts_with(instructions),
                    "{label}: the recorded system prompt opens with the delegate's instructions:\n{yaml}"
                );
            }
        }
    }

    // The same block refuses a delegate that is not a config agent at engage,
    // from the `.session` command path as from `-e`.
    let dir = fresh_config_dir("delegate-session-refused");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "session:\n  chat:\n    agent: missing-helper\n");
    write_macro(&dir, "engage", &[".session demo", ".run hello"]);
    let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "engage"]);
    let text = combined(&output);
    assert!(
        !output.status.success() && !text.contains("panicked"),
        "missing delegate: engage is refused:\n{text}"
    );
    assert!(
        text.contains(
            "session.chat.agent must name a config agent (config.yaml); 'missing-helper' has no config.yaml"
        ) && !has_routing_line(&output),
        "missing delegate: the validator text is printed and nothing is routed:\n{text}"
    );
}

/// `COYOTE_GRAPH_CHAT_AGENT` names the block's chat agent only on a chat
/// branch. With a delegating block engaged, a forced run's turn events are
/// the graph agent's own, carry the run branch, and name no chat agent,
/// while the chat agent is still the one parked for the session (its
/// recorded `chat_agent`).
#[test]
fn usage_probe_a_forced_run_under_a_delegating_block_names_no_chat_agent_in_its_hooks() {
    const DELEGATING_BLOCK: &str = "session:\n  expose: [said]\n  chat:\n    agent: helper\n";
    let dir = fresh_config_dir("delegate-run-hooks");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, DELEGATING_BLOCK);
    let config = dir.join("config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(&config, text.replace("dry_run: true\n", "dry_run: false\n")).unwrap();
    let helper = dir.join("agents").join("helper");
    fs::create_dir_all(&helper).unwrap();
    fs::write(
        helper.join("config.yaml"),
        "name: helper\ndescription: A helper.\ninstructions: Be brief.\n",
    )
    .unwrap();
    write_macro(
        &dir,
        "engage",
        &[
            ".session demo",
            ".run hello",
            ".save session",
            ".exit session",
        ],
    );

    let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "engage"]);
    let text = combined(&output);
    assert!(
        output.status.success() && !text.contains("panicked"),
        "exit {:?}\n{text}",
        output.status
    );
    assert!(has_routing_line(&output), "{text}");

    let started = wait_for_env_dump(&dir, "turn.started");
    assert_no_graph_vars("delegating forced run", "turn.started", &started);

    let completed = wait_for_env_dump(&dir, "turn.completed");
    for (key, value) in [
        ("COYOTE_AGENT_NAME", GRAPH_AGENT),
        ("COYOTE_GRAPH_BRANCH", "run"),
        ("COYOTE_GRAPH_ROUTE", "forced"),
    ] {
        assert!(
            completed
                .lines()
                .any(|line| line.trim_end() == format!("{key}={value}")),
            "turn.completed must carry {key}={value}:\n{completed}"
        );
    }
    assert!(
        !completed
            .lines()
            .any(|line| line.starts_with("COYOTE_GRAPH_CHAT_AGENT=")),
        "a run branch names no chat agent:\n{completed}"
    );

    let saved = fs::read_to_string(
        dir.join("agents")
            .join(GRAPH_AGENT)
            .join("sessions/demo.yaml"),
    )
    .unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&saved).unwrap();
    assert_eq!(
        doc["chat_agent"].as_str(),
        Some("helper"),
        "the chat agent stays seated for the session the run was recorded on:\n{saved}"
    );
}
