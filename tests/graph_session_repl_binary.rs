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

/// A graph that carries three keys: `said` grows by one word per run, `big`
/// is rewritten to a 17 KiB string and `list` to a 9000-element array each
/// run, so the record shows the cap at work on every kind of value.
const CARRY_GRAPH: &str = "name: echo-graph\n\
    version: \"1.0\"\n\
    description: Accumulates what it was told across runs.\n\
    global_hooks: [\"*\"]\n\
    initial_state: { said: \"\", big: \"\", list: [] }\n\
    carry: [said, big, list]\n\
    start: add\n\
    nodes:\n\
    \x20 add: { type: script, script: add.sh, state_updates: { said: \"{{said}}+{{initial_prompt}}\" }, next: done }\n\
    \x20 done: { type: end, output: \"said={{said}}\" }\n";

const ADD_SCRIPT: &str = "#!/bin/bash\n\
    big=$(head -c 17408 /dev/zero | tr '\\0' x)\n\
    list=$(printf '1,%.0s' $(seq 1 9000))\n\
    printf '{\"big\":\"%s\",\"list\":[%s]}\\n' \"$big\" \"${list%,}\"\n";

/// Carried state survives a cold restart through the saved session file: the
/// first `.run` leaves `said` in `last_run.carried`, a fresh process that
/// resumes the session seeds the next run from it and the file then holds
/// the grown value. On disk a carried string over 16 KiB is kept from its
/// tail behind `...` at exactly 16 KiB and an oversized array is left out of
/// the record, while the exposed copy stays whole. A `.run` with no session
/// engaged starts from the YAML default and leaves the saved record alone.
#[test]
fn usage_probe_carried_state_round_trips_through_the_saved_session_across_cold_starts() {
    if cfg!(windows) {
        eprintln!("skipping: the accumulating node is a bash script");
        return;
    }
    const CAP: usize = 16 * 1024;
    let dir = fresh_config_dir("carry");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_BLOCK);
    // The recipe's dry run records no last_run by contract; the graph is
    // script-only and `.run` skips the classifier, so no model is called.
    let config = dir.join("config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(&config, text.replace("dry_run: true\n", "dry_run: false\n")).unwrap();
    let agent_dir = dir.join("agents").join(GRAPH_AGENT);
    fs::write(
        agent_dir.join("graph.yaml"),
        format!("{CARRY_GRAPH}{RECIPE_BLOCK}"),
    )
    .unwrap();
    fs::write(agent_dir.join("add.sh"), ADD_SCRIPT).unwrap();
    let saved = agent_dir.join("sessions").join("demo.yaml");

    let run_saving = |label: &str, prompt: &str| -> (String, serde_yaml::Value) {
        write_macro(
            &dir,
            label,
            &[
                ".session demo",
                &format!(".run {prompt}"),
                ".save session",
                ".exit session",
            ],
        );
        let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", label]);
        let text = combined(&output);
        assert!(
            output.status.success() && !text.contains("panicked"),
            "{label}: exit {:?}\n{text}",
            output.status
        );
        assert!(
            stdout_of(&output)
                .lines()
                .any(|line| line.contains("routing: run")),
            "{label}: the forced run prints its routing line:\n{text}"
        );
        let yaml = fs::read_to_string(&saved)
            .unwrap_or_else(|e| panic!("{label}: the session file was not written ({e}):\n{text}"));
        (text, serde_yaml::from_str(&yaml).unwrap())
    };
    // The resumed session replays the earlier exchange above the new turn,
    // so the current run's echo is the last `said=` line.
    let said_line = |text: &str| -> String {
        text.lines()
            .rev()
            .find(|line| line.starts_with("said="))
            .unwrap_or_else(|| panic!("no said= line:\n{text}"))
            .to_string()
    };

    let (text, doc) = run_saving("first", "one");
    assert_eq!(said_line(&text), "said=+one", "{text}");
    let carried = &doc["last_run"]["carried"];
    assert_eq!(
        carried["said"].as_str(),
        Some("+one"),
        "last_run.carried.said after the first run:\n{doc:?}"
    );
    let big = carried["big"]
        .as_str()
        .unwrap_or_else(|| panic!("carried.big is a string:\n{doc:?}"));
    assert_eq!(big.len(), CAP, "the carried string is cut to the cap");
    assert!(
        big.starts_with("...") && big.ends_with("xxx"),
        "the carried string is cut from the front behind the mark: {:?}...",
        &big[..8]
    );
    let keys: Vec<&str> = carried
        .as_mapping()
        .unwrap_or_else(|| panic!("carried is a map:\n{doc:?}"))
        .keys()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert_eq!(
        keys,
        vec!["said", "big"],
        "the oversized array is left out, the rest keep the carry order"
    );
    assert_eq!(
        doc["last_run"]["exposed"]["said"].as_str(),
        Some("+one"),
        "exposed state is recorded alongside:\n{doc:?}"
    );

    let (text, doc) = run_saving("second", "two");
    assert_eq!(
        said_line(&text),
        "said=+one+two",
        "the resumed run starts from the carried value:\n{text}"
    );
    assert_eq!(
        doc["last_run"]["carried"]["said"].as_str(),
        Some("+one+two"),
        "the record now carries the grown value:\n{doc:?}"
    );
    let before = fs::read_to_string(&saved).unwrap();

    write_macro(&dir, "loose", &[".run three"]);
    let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "loose"]);
    let text = combined(&output);
    assert!(
        output.status.success() && !text.contains("panicked"),
        "loose: exit {:?}\n{text}",
        output.status
    );
    assert_eq!(
        said_line(&text),
        "said=+three",
        "without a session nothing is carried in:\n{text}"
    );
    assert_eq!(
        fs::read_to_string(&saved).unwrap(),
        before,
        "a session-less run leaves the saved record alone"
    );

    let (text, _) = run_saving("third", "four");
    assert_eq!(
        said_line(&text),
        "said=+one+two+four",
        "the session-less run in between did not touch what the session carries:\n{text}"
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

/// The recipe's delegating block: the graph agent hands every chat turn to
/// the `helper` config agent, pre-setting its one variable from the block.
const RECIPE_DELEGATING_BLOCK: &str =
    "session:\n  expose: [said]\n  chat:\n    agent: helper\n    variables: { tone: terse }\n";

/// The recipe's `helper` config agent: a required variable `tone` with no
/// default, rendered into its instructions.
const HELPER_CONFIG: &str = "name: helper\n\
    model: dryrun:dry-model\n\
    instructions: \"Speak {{tone}}.\"\n\
    global_hooks: [\"*\"]\n\
    variables:\n\
    \x20 - name: tone\n\
    \x20   description: t\n";

fn write_config_agent(dir: &Path, name: &str, config: &str) {
    let agent = dir.join("agents").join(name);
    fs::create_dir_all(&agent).unwrap();
    fs::write(agent.join("config.yaml"), config).unwrap();
}

/// Dry run records nothing by contract; a saved session needs the run for
/// real (the forced branch skips the classifier, so no model is called).
fn record_for_real(dir: &Path) {
    let config = dir.join("config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.contains("dry_run: true\n"), "{text}");
    fs::write(&config, text.replace("dry_run: true\n", "dry_run: false\n")).unwrap();
}

/// Adds a `turn.failed` capture to the fixture's hooks, which is the last
/// key in the config, so appending keeps the mapping well-formed.
fn capture_turn_failed(dir: &Path) {
    let config = dir.join("config.yaml");
    let mut text = fs::read_to_string(&config).unwrap();
    text.push_str(&format!(
        "\x20 turn.failed:\n\x20   - name: capture\n\x20     command: '{}'\n",
        env_capture_command(dir, "turn.failed")
    ));
    fs::write(config, text).unwrap();
}

/// With the block's `variables:` dropped, the helper's required `tone` has
/// no value and, without a TTY, engaging fails before anything runs. All
/// headless shapes refuse the same way: nothing is echoed, no turn hook
/// fires, the error names the chat agent and the graph agent (`Failed to
/// engage chat agent 'helper' for graph agent 'echo-graph'`) and carries the
/// variables bail as its cause. With `--session s` the delegate's variables
/// are seated only when the session is, so the wrapper has to come from the
/// seating stage rather than from engage. The refusal lands before any model
/// call. The `--session s -e hello` arm is the operator's shape; its session
/// twin is a `.run hello` then `.save session` macro, because a headless `-e`
/// under the session would route through the classifier and fail without a
/// model even after a successful engage, whereas the forced run echoes and
/// the save writes `s.yaml`, so the missing session file tells a refused
/// engage apart from one that succeeded.
#[test]
fn usage_probe_a_missing_delegate_variable_refuses_every_headless_engage_path_naming_the_chat_agent()
 {
    const BLOCK_WITHOUT_VARIABLES: &str =
        "session:\n  expose: [said]\n  chat:\n    agent: helper\n";
    const WRAPPER: &str = "Failed to engage chat agent 'helper' for graph agent 'echo-graph'";
    const REQUIRED: &str = "The following agent variables are required:";
    for (label, args, routed) in [
        (
            "session-less -e",
            &["-a", GRAPH_AGENT, "-e", "hello"][..],
            false,
        ),
        (
            "--session s -e",
            &["-a", GRAPH_AGENT, "--session", "s", "-e", "hello"][..],
            false,
        ),
        (
            "--session s .run",
            &["-a", GRAPH_AGENT, "--session", "s", "--macro", "forced"][..],
            true,
        ),
    ] {
        let dir = fresh_config_dir("delegate-required-var");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, BLOCK_WITHOUT_VARIABLES);
        write_config_agent(&dir, "helper", HELPER_CONFIG);
        if routed {
            record_for_real(&dir);
            write_macro(&dir, "forced", &[".run hello", ".save session"]);
        }

        let output = run_coyote(&dir, args);
        let text = combined(&output);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && !text.contains("panicked"),
            "{label}: the engage is refused without a TTY: exit {:?}\n{text}",
            output.status
        );
        assert!(
            !stdout_of(&output).lines().any(|line| line == "hello") && !has_routing_line(&output),
            "{label}: nothing runs after a refused engage:\n{text}"
        );
        assert!(
            stderr.contains(REQUIRED) && stderr.contains("- tone: t"),
            "{label}: the refusal on stderr is the variables bail naming `tone`:\n{text}"
        );
        for event in TURN_EVENTS {
            assert!(
                !env_log(&dir, event).exists(),
                "{label}: {event} must not fire for a refused engage"
            );
        }
        assert!(
            stderr.contains(WRAPPER),
            "{label}: stderr names the chat agent and the graph agent (`{WRAPPER}` + `{REQUIRED}`):\n{text}"
        );
        let sessions = dir.join("agents").join(GRAPH_AGENT).join("sessions");
        assert!(
            !sessions.join("s.yaml").exists(),
            "{label}: a refused engage writes no session file"
        );
    }
}

/// With the block's `variables:` setting the helper's one required `tone`,
/// both headless cold starts engage the delegate without a prompt and
/// without a TTY: the session-less prompt echoes unrouted, and the forced run
/// under `--session s` echoes routed. Neither prints a re-seating warning,
/// an engage failure or any `session.chat` text.
#[test]
fn a_delegating_block_with_its_variables_set_engages_cold_on_both_headless_paths() {
    for (label, args, routed) in [
        (
            "session-less -e",
            &["-a", GRAPH_AGENT, "-e", "hello"][..],
            false,
        ),
        (
            "--session s .run",
            &["-a", GRAPH_AGENT, "--session", "s", "--macro", "forced"][..],
            true,
        ),
    ] {
        let dir = fresh_config_dir("delegate-satisfied-var");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, RECIPE_DELEGATING_BLOCK);
        write_config_agent(&dir, "helper", HELPER_CONFIG);
        if routed {
            write_macro(&dir, "forced", &[".run hello"]);
        }

        let output = run_coyote(&dir, args);
        let text = combined(&output);
        assert!(
            output.status.success() && !text.contains("panicked"),
            "{label}: exit {:?}\n{text}",
            output.status
        );
        assert!(
            stdout_of(&output).lines().any(|line| line == "hello"),
            "{label}: the graph's echo is missing:\n{text}"
        );
        assert_eq!(
            has_routing_line(&output),
            routed,
            "{label}: routed only under the engaged session:\n{text}"
        );
        for needle in ["was born with", "Failed to engage", "session.chat"] {
            assert!(
                !text.contains(needle),
                "{label}: a satisfied delegate engages silently, but the output says {needle:?}:\n{text}"
            );
        }
    }
}

/// A prompt that cannot be classified under a delegating block fails the
/// turn with the classifier's error while the delegate is engaged for the
/// session (the save before the prompt records `chat_agent: helper`), and
/// `turn.failed` is the graph agent's own: `COYOTE_AGENT_NAME=echo-graph`,
/// no branch, no route and no `COYOTE_GRAPH_CHAT_AGENT`, which rides only a
/// turn whose branch was decided as chat.
#[test]
fn usage_probe_a_classifier_failure_under_a_delegating_block_names_no_chat_agent_in_turn_failed() {
    let dir = fresh_config_dir("delegate-classifier");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_DELEGATING_BLOCK);
    write_config_agent(&dir, "helper", HELPER_CONFIG);
    capture_turn_failed(&dir);
    write_macro(
        &dir,
        "routed",
        &[".session demo", ".save session", "hello there"],
    );

    let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "routed"]);
    let text = combined(&output);
    assert!(
        !output.status.success() && !text.contains("panicked"),
        "the unclassifiable prompt fails the run: exit {:?}\n{text}",
        output.status
    );
    assert!(
        text.contains("Graph route classifier could not read the decision")
            && !has_routing_line(&output)
            && !text.contains("graph: echo-graph"),
        "the failure is the classifier's, no branch is chosen and the graph never starts:\n{text}"
    );
    assert!(
        !text.contains("was born with") && !text.contains("Failed to engage"),
        "the delegate engaged cleanly for the fresh session:\n{text}"
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
        "the chat agent was seated for the session before the prompt:\n{saved}"
    );

    let failed = wait_for_env_dump(&dir, "turn.failed");
    assert!(
        failed
            .lines()
            .any(|line| line.trim_end() == format!("COYOTE_AGENT_NAME={GRAPH_AGENT}")),
        "turn.failed is the graph agent's turn:\n{failed}"
    );
    assert_no_graph_vars("delegating classifier failure", "turn.failed", &failed);
    let started = wait_for_env_dump(&dir, "turn.started");
    assert_no_graph_vars("delegating classifier failure", "turn.started", &started);
    assert!(
        !env_log(&dir, "turn.completed").exists(),
        "a failed turn completes nothing"
    );
}

/// `graph.yaml` is re-read on every turn. Renaming `chat.agent` while a
/// session is engaged leaves forced runs alone (the run arm never swaps
/// agents, so the renamed delegate is never built) and the session keeps
/// the chat agent it was ENGAGED with; a rename to an agent that does not
/// declare the block's variable fails the very next `.run` at graph
/// validation with the engage-time text, before the graph starts.
#[test]
fn usage_probe_a_chat_agent_renamed_mid_session_leaves_forced_runs_alone_and_keeps_the_engaged_one()
{
    if cfg!(windows) {
        eprintln!("skipping: the rename step is a `cp` shell command");
        return;
    }
    const OTHER_WITH_TONE: &str = "name: other\n\
        model: dryrun:dry-model\n\
        instructions: Be other.\n\
        variables:\n\
        \x20 - name: tone\n\
        \x20   description: t\n\
        \x20   default: x\n";
    const OTHER_WITHOUT_TONE: &str =
        "name: other\nmodel: dryrun:dry-model\ninstructions: Be other.\n";
    for (label, other_config, runs_on) in [
        (
            "renamed to a delegate declaring tone",
            OTHER_WITH_TONE,
            true,
        ),
        (
            "renamed to a delegate without tone",
            OTHER_WITHOUT_TONE,
            false,
        ),
    ] {
        let dir = fresh_config_dir("delegate-rename");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, RECIPE_DELEGATING_BLOCK);
        record_for_real(&dir);
        write_config_agent(&dir, "helper", HELPER_CONFIG);
        write_config_agent(&dir, "other", other_config);
        let graph = dir.join("agents").join(GRAPH_AGENT).join("graph.yaml");
        let renamed = dir.join("agents").join(GRAPH_AGENT).join("renamed.yaml");
        fs::write(
            &renamed,
            fs::read_to_string(&graph)
                .unwrap()
                .replace("agent: helper", "agent: other"),
        )
        .unwrap();
        let rename = format!("!cp {} {}", renamed.display(), graph.display());
        write_macro(
            &dir,
            "rename",
            &[
                ".session demo",
                ".run one",
                ".save session",
                rename.as_str(),
                ".run two",
                ".save session",
                ".exit session",
            ],
        );

        let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--macro", "rename"]);
        let text = combined(&output);
        assert!(!text.contains("panicked"), "{label}: panicked:\n{text}");
        let stdout = stdout_of(&output);
        assert!(
            stdout.lines().any(|line| line == "one"),
            "{label}: the run before the rename echoes:\n{text}"
        );
        assert!(
            fs::read_to_string(&graph).unwrap().contains("agent: other"),
            "{label}: the rename landed on disk"
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
            "{label}: the session keeps the chat agent it was engaged with:\n{saved}"
        );
        if runs_on {
            assert!(
                output.status.success(),
                "{label}: exit {:?}\n{text}",
                output.status
            );
            assert!(
                stdout.lines().any(|line| line == "two")
                    && stdout
                        .lines()
                        .filter(|line| line.contains("routing: run"))
                        .count()
                        == 2,
                "{label}: the forced run after the rename is routed and echoes:\n{text}"
            );
            assert!(
                !text.contains("was born with") && !text.contains("Failed to engage"),
                "{label}: the renamed delegate is never engaged on a run:\n{text}"
            );
            assert_eq!(
                doc["last_run"]["prompt"].as_str(),
                Some("two"),
                "{label}: the second run was recorded:\n{saved}"
            );
        } else {
            assert!(
                !output.status.success(),
                "{label}: the next run fails at validation:\n{text}"
            );
            assert!(
                text.contains(
                    "session.chat.variables names 'tone', which agent 'other' does not declare (declares no variables)"
                ),
                "{label}: the per-turn re-read reports the validator text:\n{text}"
            );
            // The forced branch's `routing: run` line prints before the
            // graph is re-read, so it still appears; the graph itself
            // starts only once.
            assert!(
                !stdout.lines().any(|line| line == "two")
                    && text
                        .lines()
                        .filter(|line| line.contains("graph: echo-graph"))
                        .count()
                        == 1,
                "{label}: the second run never starts the graph:\n{text}"
            );
            assert_eq!(
                doc["last_run"]["prompt"].as_str(),
                Some("one"),
                "{label}: only the first run was recorded:\n{saved}"
            );
        }
    }
}

/// `.agent <graph> <session> key=value` extras feed the graph agent only:
/// the block's `chat.variables` is the sole pre-set path for the delegate.
/// An extra spelled like the delegate's variable neither reaches the
/// delegate (the session records the block's `tone: terse` and the system
/// message is rendered from it) nor leaks anywhere into the saved session.
#[test]
fn usage_probe_agent_command_extras_never_reach_the_delegate() {
    let dir = fresh_config_dir("delegate-extras");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_DELEGATING_BLOCK);
    record_for_real(&dir);
    write_config_agent(&dir, "helper", HELPER_CONFIG);
    write_macro(
        &dir,
        "extras",
        &[
            ".agent echo-graph demo tone=loud",
            ".run hello",
            ".save session",
            ".exit session",
        ],
    );

    let output = run_coyote(&dir, &["--macro", "extras"]);
    let text = combined(&output);
    assert!(
        output.status.success() && !text.contains("panicked"),
        "exit {:?}\n{text}",
        output.status
    );
    assert!(
        stdout_of(&output).lines().any(|line| line == "hello") && has_routing_line(&output),
        "the forced run under the engaged session is routed and echoes:\n{text}"
    );
    let saved = fs::read_to_string(
        dir.join("agents")
            .join(GRAPH_AGENT)
            .join("sessions/demo.yaml"),
    )
    .unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&saved).unwrap();
    assert_eq!(doc["chat_agent"].as_str(), Some("helper"), "{saved}");
    assert_eq!(
        doc["chat_agent_variables"]["tone"].as_str(),
        Some("terse"),
        "the delegate's variable is the block's, not the extra's:\n{saved}"
    );
    let system_prompt = doc["messages"][0]["content"].as_str().unwrap_or_default();
    assert!(
        system_prompt.starts_with("Speak terse."),
        "the session's system message is rendered from the block's value:\n{saved}"
    );
    assert!(
        !saved.contains("loud"),
        "the extra never reaches the saved session:\n{saved}"
    );
}

/// A session file born under `helper` with its own session variable, which
/// differs from anything a block could pre-set.
const LOUD_DELEGATE_SESSION: &str = "model: dryrun:dry-model\n\
    messages:\n\
    - role: system\n\
    \x20 content: Speak loud.\n\
    - role: user\n\
    \x20 content: hi\n\
    - role: assistant\n\
    \x20 content: ok\n\
    chat_agent: helper\n\
    chat_agent_variables:\n\
    \x20 tone: loud\n\
    chat_agent_instructions: Speak loud.\n";

/// A session saved before chat agents existed: messages, no `chat_agent*`
/// fields.
const INLINE_BORN_SESSION: &str = "model: dryrun:dry-model\n\
    messages:\n\
    - role: system\n\
    \x20 content: Be terse.\n\
    - role: user\n\
    \x20 content: hi\n\
    - role: assistant\n\
    \x20 content: ok\n";

/// With no `chat.variables` in the block, the delegate has no shared value
/// for its required `tone`, so every seating that cannot fall back on a
/// stored session value refuses as `.agent helper` would without a TTY, and
/// names both agents. A session born under the delegate carries `tone`, so
/// resuming it seats the stored value without asking; a session born inline
/// carries none and is refused at the resume, its file left byte-identical;
/// and leaving the delegate's session hands it back its (empty) shared
/// variables, which refuses the same way.
#[test]
fn usage_probe_without_shared_variables_the_delegate_lives_on_a_sessions_stored_ones_and_refuses_without_them()
 {
    const BLOCK_WITHOUT_VARIABLES: &str =
        "session:\n  expose: [said]\n  chat:\n    agent: helper\n";
    const WRAPPER: &str = "Failed to engage chat agent 'helper' for graph agent 'echo-graph'";
    const REQUIRED: &str = "The following agent variables are required:";

    // A resumed session born inline: nothing stored to seat, refused.
    let dir = fresh_config_dir("delegate-inline-born-no-vars");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, BLOCK_WITHOUT_VARIABLES);
    record_for_real(&dir);
    write_config_agent(&dir, "helper", HELPER_CONFIG);
    let sessions = dir.join("agents").join(GRAPH_AGENT).join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let saved = sessions.join("s.yaml");
    fs::write(&saved, INLINE_BORN_SESSION).unwrap();
    let output = run_coyote(&dir, &["-a", GRAPH_AGENT, "--session", "s", "-e", "hello"]);
    let text = combined(&output);
    assert!(
        !output.status.success() && !text.contains("panicked"),
        "inline-born resume: refused without a TTY: exit {:?}\n{text}",
        output.status
    );
    assert!(
        text.contains(WRAPPER) && text.contains(REQUIRED) && text.contains("- tone: t"),
        "inline-born resume: the refusal names both agents and the variable:\n{text}"
    );
    assert!(
        !stdout_of(&output).lines().any(|line| line == "hello") && !has_routing_line(&output),
        "inline-born resume: nothing runs after the refused seating:\n{text}"
    );
    assert!(
        !text.contains("was born with"),
        "inline-born resume: a refused seating prints no re-seating warning:\n{text}"
    );
    assert_eq!(
        fs::read_to_string(&saved).unwrap(),
        INLINE_BORN_SESSION,
        "inline-born resume: the refused seating leaves the session file as it was"
    );

    // A resumed session born under the delegate: its stored `tone` seats
    // the delegate; leaving the session falls back on the empty shared set.
    let dir = fresh_config_dir("delegate-stored-vars");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, BLOCK_WITHOUT_VARIABLES);
    record_for_real(&dir);
    write_config_agent(&dir, "helper", HELPER_CONFIG);
    let sessions = dir.join("agents").join(GRAPH_AGENT).join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let saved = sessions.join("s.yaml");
    fs::write(&saved, LOUD_DELEGATE_SESSION).unwrap();
    write_macro(
        &dir,
        "leave",
        &[".run hello", ".save session", ".exit session", ".run two"],
    );
    let output = run_coyote(
        &dir,
        &["-a", GRAPH_AGENT, "--session", "s", "--macro", "leave"],
    );
    let text = combined(&output);
    let stdout = stdout_of(&output);
    assert!(
        stdout.lines().any(|line| line == "hello") && has_routing_line(&output),
        "stored vars: the resume seats the stored `tone` and the forced run echoes:\n{text}"
    );
    assert!(
        text.contains("Saved the session") && !text.contains("was born with"),
        "stored vars: the session saves without a re-seating warning:\n{text}"
    );
    let yaml = fs::read_to_string(&saved).unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(doc["chat_agent"].as_str(), Some("helper"), "{yaml}");
    assert_eq!(
        doc["chat_agent_variables"]["tone"].as_str(),
        Some("loud"),
        "stored vars: the session's own value is what seats the delegate:\n{yaml}"
    );
    assert_eq!(
        doc["chat_agent_instructions"].as_str(),
        Some("Speak loud."),
        "stored vars: the recorded delegate instructions survive the save:\n{yaml}"
    );
    assert_eq!(doc["last_run"]["prompt"].as_str(), Some("hello"), "{yaml}");
    assert!(
        !output.status.success() && !text.contains("panicked"),
        "stored vars: leaving the session hands the delegate its empty shared set, refused without a TTY: exit {:?}\n{text}",
        output.status
    );
    assert!(
        text.contains(WRAPPER) && text.contains(REQUIRED) && text.contains("- tone: t"),
        "stored vars: the refusal on leaving the session names both agents:\n{text}"
    );
    assert!(
        !stdout.lines().any(|line| line == "two"),
        "stored vars: nothing runs after the refused leave:\n{text}"
    );
}

/// Leaving a session hands the delegate its shared variables back: a fresh
/// session opened afterwards is born from the block's `tone: terse`, not
/// from the `tone: loud` the previous session had stored, while that
/// previous session keeps its own value.
#[test]
fn usage_probe_leaving_a_session_returns_the_delegate_to_its_shared_variables_for_the_next_one() {
    let dir = fresh_config_dir("delegate-exit-session-shared");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_DELEGATING_BLOCK);
    record_for_real(&dir);
    write_config_agent(&dir, "helper", HELPER_CONFIG);
    let sessions = dir.join("agents").join(GRAPH_AGENT).join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let loud = sessions.join("s.yaml");
    fs::write(&loud, LOUD_DELEGATE_SESSION).unwrap();
    write_macro(
        &dir,
        "rotate",
        &[
            ".run one",
            ".save session",
            ".exit session",
            ".session fresh",
            ".run two",
            ".save session",
            ".exit session",
        ],
    );

    let output = run_coyote(
        &dir,
        &["-a", GRAPH_AGENT, "--session", "s", "--macro", "rotate"],
    );
    let text = combined(&output);
    assert!(
        output.status.success() && !text.contains("panicked"),
        "exit {:?}\n{text}",
        output.status
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.lines().any(|line| line == "one") && stdout.lines().any(|line| line == "two"),
        "both forced runs echo:\n{text}"
    );
    assert!(
        !text.contains("was born with") && !text.contains("variables are required"),
        "neither session draws a warning or a prompt:\n{text}"
    );

    let fresh = fs::read_to_string(sessions.join("fresh.yaml")).unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&fresh).unwrap();
    assert_eq!(doc["chat_agent"].as_str(), Some("helper"), "{fresh}");
    assert_eq!(
        doc["chat_agent_variables"]["tone"].as_str(),
        Some("terse"),
        "the fresh session is born from the block's shared value:\n{fresh}"
    );
    assert!(
        doc["chat_agent_instructions"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Speak terse."),
        "the fresh session's delegate instructions render the shared value:\n{fresh}"
    );
    assert!(
        doc["messages"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Speak terse."),
        "the fresh session's system message is the delegate's, rendered from the shared value:\n{fresh}"
    );
    assert!(
        !fresh.contains("loud"),
        "the previous session's stored value never reaches the fresh one:\n{fresh}"
    );
    let loud = fs::read_to_string(&loud).unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&loud).unwrap();
    assert_eq!(
        doc["chat_agent_variables"]["tone"].as_str(),
        Some("loud"),
        "the resumed session keeps its own value:\n{loud}"
    );
    assert_eq!(doc["last_run"]["prompt"].as_str(), Some("one"), "{loud}");
}

/// Gives the fixture's dry-run client a second model, so a delegate that
/// names it is told apart from the graph agent's `dryrun:dry-model`.
fn add_other_model(dir: &Path) {
    let config = dir.join("config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    let anchor = "\x20       supports_function_calling: true\n";
    assert!(text.contains(anchor), "{text}");
    fs::write(
        &config,
        text.replacen(
            anchor,
            "\x20       supports_function_calling: true\n\
             \x20     - name: other-model\n\
             \x20       max_input_tokens: 50000\n\
             \x20       supports_function_calling: true\n",
            1,
        ),
    )
    .unwrap();
}

/// The session is born from the chat agent, not switched to it: a delegate
/// on its own model with its own compression settings and temperature
/// gives the session its rendered instructions and its compression
/// settings, while the session's `model` stays the graph agent's and the
/// delegate's temperature and tool set are never written into the file.
#[test]
fn usage_probe_a_session_born_from_the_delegate_takes_its_prompt_and_compression_but_not_its_model()
{
    const HELPER_ON_ITS_OWN_MODEL: &str = "name: helper\n\
        model: dryrun:other-model\n\
        temperature: 0.9\n\
        compression_threshold: 4321\n\
        compression_keep_last: 7\n\
        instructions: \"Speak {{tone}}.\"\n\
        variables:\n\
        \x20 - name: tone\n\
        \x20   description: t\n";
    let dir = fresh_config_dir("delegate-born-model");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_DELEGATING_BLOCK);
    record_for_real(&dir);
    add_other_model(&dir);
    write_config_agent(&dir, "helper", HELPER_ON_ITS_OWN_MODEL);
    write_macro(
        &dir,
        "born",
        &[
            ".agent echo-graph demo",
            ".run hello",
            ".save session",
            ".exit session",
        ],
    );

    let output = run_coyote(&dir, &["--macro", "born"]);
    let text = combined(&output);
    assert!(
        output.status.success() && !text.contains("panicked"),
        "exit {:?}\n{text}",
        output.status
    );
    assert!(
        stdout_of(&output).lines().any(|line| line == "hello") && has_routing_line(&output),
        "the forced run under the engaged session echoes:\n{text}"
    );
    let saved = fs::read_to_string(
        dir.join("agents")
            .join(GRAPH_AGENT)
            .join("sessions/demo.yaml"),
    )
    .unwrap();
    let doc: serde_yaml::Value = serde_yaml::from_str(&saved).unwrap();
    assert_eq!(
        doc["model"].as_str(),
        Some("dryrun:dry-model"),
        "the session keeps the graph agent's model, not the delegate's:\n{saved}"
    );
    assert!(
        !saved.contains("other-model"),
        "the delegate's model is never written into the session:\n{saved}"
    );
    assert_eq!(
        doc["compression_threshold"].as_u64(),
        Some(4321),
        "the delegate's compression threshold governs the session:\n{saved}"
    );
    assert_eq!(
        doc["compression_keep_last"].as_u64(),
        Some(7),
        "the delegate's compression keep-last governs the session:\n{saved}"
    );
    for key in ["temperature", "enabled_tools"] {
        assert!(
            doc.get(key).is_none(),
            "the delegate's {key} is a per-request setting, never a session one:\n{saved}"
        );
    }
    assert!(
        doc["messages"][0]["content"]
            .as_str()
            .unwrap_or_default()
            .starts_with("Speak terse."),
        "the recorded system prompt is the delegate's rendered instructions:\n{saved}"
    );
    assert_eq!(doc["chat_agent"].as_str(), Some("helper"), "{saved}");
}

/// Under `save_session: true` a session engaged on a delegating block is
/// flushed at the end of every turn, not at exit: a snapshot of the file
/// taken between two forced runs already carries the first run's exchange
/// and record and not the second's. The file keeps the graph agent's model
/// while the parked chat agent's model (set on the delegate's own config)
/// never reaches it, and the clean exit confirms the file once. The helper
/// is given a model of its own so the two can be told apart on disk.
#[test]
fn usage_probe_an_autosaving_session_under_a_delegating_block_lands_each_run_before_the_next() {
    if cfg!(windows) {
        eprintln!("skipping: the snapshot steps are `cp` shell commands");
        return;
    }
    const HELPER_WITH_OWN_MODEL: &str = "name: helper\n\
        model: dryrun:dry-helper\n\
        instructions: \"Speak {{tone}}.\"\n\
        global_hooks: [\"*\"]\n\
        variables:\n\
        \x20 - name: tone\n\
        \x20   description: t\n";
    const SAVED_PREFIX: &str = "Saved the session to '";
    let dir = fresh_config_dir("delegate-autosave");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, RECIPE_DELEGATING_BLOCK);
    record_for_real(&dir);
    write_config_agent(&dir, "helper", HELPER_WITH_OWN_MODEL);
    // The app autosaves every session, and the dry-run client also knows the
    // helper's model so the delegate engages.
    let config = dir.join("config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.contains("dry_run: false\n"), "{text}");
    let text = text
        .replace("dry_run: false\n", "dry_run: false\nsave_session: true\n")
        .replace(
            "       supports_function_calling: true\n",
            "       supports_function_calling: true\n\
            \x20     - name: dry-helper\n\
            \x20       max_input_tokens: 100000\n\
            \x20       supports_function_calling: true\n",
        );
    assert!(text.contains("dry-helper"), "{text}");
    fs::write(&config, text).unwrap();
    // A saved session gives the flush a file to land in; it was born under
    // this delegate, so the engage re-seats it silently. A loaded session
    // autosaves by its own `save_session` key (the app's seeds only new
    // sessions), so the file carries it.
    let sessions = dir.join("agents").join(GRAPH_AGENT).join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let session_file = sessions.join("demo.yaml");
    fs::write(
        &session_file,
        "model: dryrun:dry-model\n\
         save_session: true\n\
         messages:\n\
         - role: system\n\
         \x20 content: Speak terse.\n\
         - role: user\n\
         \x20 content: hi\n\
         - role: assistant\n\
         \x20 content: ok\n\
         chat_agent: helper\n\
         chat_agent_variables:\n\
         \x20 tone: terse\n\
         chat_agent_instructions: Speak terse.\n",
    )
    .unwrap();
    let after_one = dir.join("after-one.yaml");
    let after_two = dir.join("after-two.yaml");
    let snap_one = format!("!cp {} {}", session_file.display(), after_one.display());
    let snap_two = format!("!cp {} {}", session_file.display(), after_two.display());
    write_macro(
        &dir,
        "autosave",
        &[
            ".run first-run",
            snap_one.as_str(),
            ".run second-run",
            snap_two.as_str(),
            ".exit session",
        ],
    );

    let output = run_coyote(
        &dir,
        &[
            "-a",
            GRAPH_AGENT,
            "--session",
            "demo",
            "--macro",
            "autosave",
        ],
    );
    let text = combined(&output);
    assert!(
        output.status.success() && !text.contains("panicked"),
        "exit {:?}\n{text}",
        output.status
    );
    for needle in ["was born with", "Failed to engage", "Warning"] {
        assert!(
            !text.contains(needle),
            "the saved delegate session re-seats silently, but the output says {needle:?}:\n{text}"
        );
    }
    let stdout = stdout_of(&output);
    for echo in ["first-run", "second-run"] {
        assert!(
            stdout.lines().any(|line| line == echo),
            "the forced run echoes {echo}:\n{text}"
        );
    }

    let one = fs::read_to_string(&after_one).expect("the file exists after the first run");
    let one_doc: serde_yaml::Value = serde_yaml::from_str(&one).unwrap();
    assert!(
        one.contains("first-run") && !one.contains("second-run"),
        "the first run is on disk before the second starts:\n{one}"
    );
    assert_eq!(
        one_doc["last_run"]["prompt"].as_str(),
        Some("first-run"),
        "the run record lands with the exchange:\n{one}"
    );
    let two = fs::read_to_string(&after_two).unwrap();
    let two_doc: serde_yaml::Value = serde_yaml::from_str(&two).unwrap();
    assert!(
        two.contains("first-run") && two.contains("second-run"),
        "the second run is on disk before the exit:\n{two}"
    );
    assert_eq!(two_doc["last_run"]["prompt"].as_str(), Some("second-run"));
    for (label, doc, raw) in [("after one", &one_doc, &one), ("after two", &two_doc, &two)] {
        assert_eq!(
            doc["model"].as_str(),
            Some("dryrun:dry-model"),
            "{label}: the file keeps the graph agent's model:\n{raw}"
        );
        assert!(
            !raw.contains("dry-helper"),
            "{label}: the parked chat agent's model never reaches the file:\n{raw}"
        );
        assert_eq!(
            doc["chat_agent"].as_str(),
            Some("helper"),
            "{label}:\n{raw}"
        );
        assert_eq!(
            doc["chat_agent_variables"]["tone"].as_str(),
            Some("terse"),
            "{label}:\n{raw}"
        );
    }
    assert_eq!(
        fs::read_to_string(&session_file).unwrap(),
        two,
        "the clean exit rewrites nothing after the last flush"
    );
    let saved_lines: Vec<&str> = stdout
        .lines()
        .filter(|line| line.contains(SAVED_PREFIX))
        .collect();
    assert_eq!(
        saved_lines.len(),
        1,
        "the clean exit confirms the autosaved file once:\n{text}"
    );
    assert!(
        saved_lines[0].ends_with(&format!("{SAVED_PREFIX}{}'.", session_file.display())),
        "{:?}",
        saved_lines[0]
    );
    let mut entries: Vec<String> = fs::read_dir(&sessions)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec!["demo.yaml".to_string()],
        "no temp file, no second session"
    );
}
