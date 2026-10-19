//! Binary-level contract for the graph.yaml `session:` block, driven from a
//! cold config dir the way an agent author would set it up: a dry-run model,
//! an `echo-graph` agent whose graph ends immediately, and `-e hello` run
//! through the real binary. A graph with a valid block (or none) prints the
//! prompt back; every malformed block fails at graph load with the documented
//! error text and never panics. Every run uses a fresh fake HOME and a
//! scrubbed sandbox/provider env so host state cannot leak in.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process};

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const AGENT: &str = "echo-graph";

/// The block-less graph from the local-run recipe; `session_block` is
/// appended verbatim so each case controls exactly the top-level block.
const BASE_GRAPH: &str = "name: echo-graph\n\
    version: \"1.0\"\n\
    description: Echoes the prompt.\n\
    initial_state: { said: \"\" }\n\
    start: done\n\
    nodes:\n\
    \x20 done: { type: end, output: \"{{initial_prompt}}\" }\n";

fn fresh_config_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-graph-session-{label}-{}-{unique}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("agents").join(AGENT)).unwrap();
    dir
}

/// Host knobs the session-block validator reads at engage: the dry-run
/// model's function-calling flag and the app-level `memory` / `skills_enabled`
/// switches. The recipe defaults are FC on, memory off, skills on.
#[derive(Clone, Copy)]
struct Host {
    function_calling: bool,
    memory: bool,
    skills_enabled: bool,
}

const RECIPE_HOST: Host = Host {
    function_calling: true,
    memory: false,
    skills_enabled: true,
};

fn write_fixture(dir: &Path, session_block: &str) {
    write_fixture_on(dir, session_block, RECIPE_HOST);
}

fn write_fixture_on(dir: &Path, session_block: &str, host: Host) {
    // Without IS_SANDBOX (scrubbed below) a run insists on a vault password
    // file, so the fixture provides one.
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
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
             \x20       supports_function_calling: {fc}\n\
             save: false\n\
             memory: {memory}\n\
             skills_enabled: {skills}\n\
             stream: false\n",
            vault_pass = vault_pass.display(),
            fc = host.function_calling,
            memory = host.memory,
            skills = host.skills_enabled,
        ),
    )
    .unwrap();
    fs::write(
        dir.join("agents").join(AGENT).join("graph.yaml"),
        format!("{BASE_GRAPH}{session_block}"),
    )
    .unwrap();
    // The one global tool a block may name, written where the binary loads
    // global tools from. Built-in install skips files that already exist, so
    // this fixture is the `fs_read` the run sees; `chat.tools` entries must
    // be declared through `global_tools` or the agent is refused at engage.
    let tools_dir = dir.join("functions").join("tools");
    fs::create_dir_all(&tools_dir).unwrap();
    fs::write(
        tools_dir.join("fs_read.sh"),
        "#!/usr/bin/env bash\n# @describe Read a file.\n# @option --path! The file\nmain() { cat \"$argc_path\"; }\n",
    )
    .unwrap();
}

fn run_echo(dir: &Path) -> Output {
    let home = dir.join("home");
    Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(["-a", AGENT, "-e", "hello"])
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

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The dry-run model echoes its prompt, so the last non-empty stdout line
/// of a run that reached the `end` node is the `-e` text.
fn assert_echoes_hello(label: &str, output: &Output) {
    let stdout = stdout_of(output);
    let last = stdout.lines().rev().find(|l| !l.trim().is_empty());
    assert!(
        output.status.success() && last == Some("hello"),
        "{label}: expected a successful run ending in `hello`, got exit {:?}\nstdout tail: {:?}\nstderr: {}",
        output.status,
        stdout.lines().rev().take(3).collect::<Vec<_>>(),
        stderr_of(output)
    );
}

/// A malformed block must stop the run at graph load: non-zero exit, the
/// load-failure frame naming the agent, the documented reason, and no panic.
fn assert_fails_at_graph_load(label: &str, output: &Output, expected: &[&str]) {
    let stderr = stderr_of(output);
    let combined = format!("{stderr}\n{}", stdout_of(output));
    assert!(
        !output.status.success(),
        "{label}: expected graph load to fail, got exit {:?}\n{combined}",
        output.status
    );
    assert!(
        !combined.contains("panicked"),
        "{label}: the binary panicked:\n{combined}"
    );
    assert!(
        stderr.contains("Failed to load graph.yaml for agent 'echo-graph'"),
        "{label}: missing load-failure frame:\n{combined}"
    );
    for needle in expected {
        assert!(
            stderr.contains(needle),
            "{label}: stderr lacks {needle:?}:\n{combined}"
        );
    }
    assert!(
        stderr.is_ascii(),
        "{label}: non-ASCII bytes in the error text:\n{stderr}"
    );
}

#[test]
fn a_graph_with_a_valid_session_block_runs_like_one_without() {
    for (label, block) in [
        ("no block", ""),
        (
            "recipe block",
            "session:\n  expose: [said]\n  chat:\n    instructions: Answer questions about the last run from its output.\n",
        ),
        (
            "minimal block",
            "session:\n  chat:\n    instructions: Answer.\n",
        ),
        (
            "full block",
            "global_tools: [fs_read.sh]\nsession:\n  expose: [said]\n  route:\n    model: dryrun:dry-model\n    instructions: Prefer chat.\n  chat:\n    model: dryrun:dry-model\n    instructions: Answer.\n    memory: true\n    tools: [fs_read]\n",
        ),
        ("null block", "session:\n"),
    ] {
        let dir = fresh_config_dir("ok");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);

        let output = run_echo(&dir);
        assert_echoes_hello(label, &output);
        if label == "full block" {
            // The fixture config sets `memory: false`, so the engage-time
            // validator must have run and warned exactly once.
            let stderr = stderr_of(&output);
            let needle = "Warning: session.chat.memory: memory is disabled";
            assert_eq!(
                stderr.matches(needle).count(),
                1,
                "{label}: expected one {needle:?} line in stderr:\n{stderr}"
            );
        }
    }
}

#[test]
fn every_documented_session_block_mistake_fails_at_graph_load() {
    const HINT: &str = "Optional top-level block: `session` (keys: expose, route, chat).";
    let cases: &[(&str, &str, &[&str])] = &[
        (
            "unknown key in session",
            "session:\n  exposed: [said]\n  chat:\n    instructions: Answer.\n",
            &["unknown field `exposed`", HINT],
        ),
        (
            "unknown key in route",
            "session:\n  route:\n    prompt: Route.\n  chat:\n    instructions: Answer.\n",
            &["unknown field `prompt`", HINT],
        ),
        (
            "unknown key in chat",
            "session:\n  chat:\n    instructions: Answer.\n    temperature: 0.5\n",
            &["unknown field `temperature`", HINT],
        ),
        (
            "missing chat",
            "session:\n  expose: [said]\n",
            &["missing field `chat`"],
        ),
        (
            "empty chat.instructions",
            "session:\n  chat:\n    instructions: \"\"\n",
            &["session.chat.instructions must be non-empty"],
        ),
        (
            "whitespace chat.instructions",
            "session:\n  chat:\n    instructions: \"   \"\n",
            &["session.chat.instructions must be non-empty"],
        ),
        (
            "__INPUT__ in chat.instructions",
            "session:\n  chat:\n    instructions: Answer __INPUT__ now.\n",
            &["session.chat.instructions must not contain '__INPUT__'"],
        ),
        (
            "__INPUT__ in route.instructions",
            "session:\n  route:\n    instructions: Route __INPUT__ now.\n  chat:\n    instructions: Answer.\n",
            &["session.route.instructions must not contain '__INPUT__'"],
        ),
        (
            "undeclared expose key",
            "session:\n  expose: [nope]\n  chat:\n    instructions: Answer.\n",
            &[
                "session.expose key 'nope' is not declared anywhere (add it to initial_state, or have a node write it via state_updates / output_schema)",
            ],
        ),
        (
            "duplicate expose key",
            "session:\n  expose: [said, said]\n  chat:\n    instructions: Answer.\n",
            &["session.expose key 'said' is listed more than once"],
        ),
        (
            "blank expose entry",
            "session:\n  expose: [\"  \"]\n  chat:\n    instructions: Answer.\n",
            &["session.expose entry #0 is blank"],
        ),
        (
            "session is a scalar",
            "session: yes\n",
            &["session: invalid type"],
        ),
        (
            "session is an empty map",
            "session: {}\n",
            &["missing field `chat`"],
        ),
        (
            "expose is a scalar",
            "session:\n  expose: said\n  chat:\n    instructions: Answer.\n",
            &["session.expose: invalid type"],
        ),
        (
            "chat.memory is not a bool",
            "session:\n  chat:\n    instructions: Answer.\n    memory: maybe\n",
            &["session.chat.memory: invalid type"],
        ),
        (
            "denied tool in chat.tools",
            "session:\n  chat:\n    instructions: Answer.\n    tools: [agent__spawn]\n",
            &[
                "session.chat.tools entry 'agent__spawn' is not allowed in the chat turn (the agent__, job__, todo__, user__ and memory__ built-in families are refused)",
            ],
        ),
        (
            "chat.tools is a scalar",
            "session:\n  chat:\n    instructions: Answer.\n    tools: fs_read\n",
            &["session.chat.tools: invalid type"],
        ),
        (
            "chat.tools entry is a map",
            "session:\n  chat:\n    instructions: Answer.\n    tools: [{a: 1}]\n",
            &["session.chat.tools[0]: invalid type"],
        ),
        (
            "chat is a scalar",
            "session:\n  chat: 3\n",
            &["session.chat: invalid type"],
        ),
    ];

    for (label, block, expected) in cases {
        let dir = fresh_config_dir("err");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);

        assert_fails_at_graph_load(label, &run_echo(&dir), expected);
    }
}

/// An engage-time validator error stops the run before the prompt reaches the
/// graph: non-zero exit, the validation frame, the documented reason, no
/// `hello` on stdout, no panic, ASCII only.
fn assert_fails_at_engage(label: &str, output: &Output, expected: &[&str]) {
    let stderr = stderr_of(output);
    let stdout = stdout_of(output);
    let combined = format!("{stderr}\n{stdout}");
    assert!(
        !output.status.success(),
        "{label}: expected engage to fail, got exit {:?}\n{combined}",
        output.status
    );
    assert!(
        !combined.contains("panicked"),
        "{label}: the binary panicked:\n{combined}"
    );
    assert!(
        stderr.contains("Graph validation failed with"),
        "{label}: missing the validation frame:\n{combined}"
    );
    for needle in expected {
        assert!(
            stderr.contains(needle),
            "{label}: stderr lacks {needle:?}:\n{combined}"
        );
    }
    assert!(
        !stdout.lines().any(|l| l.trim() == "hello"),
        "{label}: the prompt reached the graph after a validation error:\n{combined}"
    );
    assert!(
        stderr.is_ascii(),
        "{label}: non-ASCII bytes in the error text:\n{stderr}"
    );
}

fn count_lines_starting_with(text: &str, prefix: &str) -> usize {
    text.lines().filter(|l| l.starts_with(prefix)).count()
}

/// One sample per denied prefix is refused at graph load with the one
/// documented sentence, the entry echoed verbatim, and nothing printed to
/// stdout.
#[test]
fn usage_probe_every_denied_chat_tool_is_refused_before_any_prompt() {
    const DENIED: &[&str] = &[
        "agent__spawn",
        "job__start",
        "todo__add",
        "user__select",
        "memory__read",
    ];
    for tool in DENIED {
        let dir = fresh_config_dir("deny");
        let _guard = TempDirGuard(dir.clone());
        // A denied name placed second: the sweep must not stop at an allowed
        // first entry.
        write_fixture(
            &dir,
            &format!(
                "global_tools: [fs_read.sh]\nsession:\n  chat:\n    instructions: Answer.\n    tools: [fs_read, {tool}]\n"
            ),
        );
        let output = run_echo(&dir);
        assert_fails_at_graph_load(
            tool,
            &output,
            &[&format!(
                "session.chat.tools entry '{tool}' is not allowed in the chat turn (the agent__, job__, todo__, user__ and memory__ built-in families are refused)"
            )],
        );
        assert!(
            stdout_of(&output).trim().is_empty(),
            "{tool}: a refused engage must print nothing to stdout:\n{}",
            stdout_of(&output)
        );
    }
}

/// A block may name the bundled write tool once the agent declares it as a
/// global; the built-in install puts `fs_write.sh` beside the fixture's
/// `fs_read.sh` on a cold start, and the run completes.
#[test]
fn usage_probe_a_block_naming_a_write_tool_declared_as_a_global_engages_cleanly() {
    let dir = fresh_config_dir("write");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(
        &dir,
        "global_tools: [fs_read.sh, fs_write.sh]\nsession:\n  expose: [said]\n  route:\n    model: dryrun:dry-model\n    instructions: Prefer chat.\n  chat:\n    model: dryrun:dry-model\n    instructions: Answer.\n    memory: true\n    tools: [fs_read, fs_write]\n",
    );

    let output = run_echo(&dir);

    assert_echoes_hello("write tool declared as a global", &output);
    assert!(
        dir.join("functions")
            .join("tools")
            .join("fs_write.sh")
            .is_file(),
        "the built-in install must have written fs_write.sh"
    );
}

/// The validator's own errors (unknown tool, tool not global for this agent,
/// unknown route/chat model, tools on a non-FC chat model, a YAML integer or
/// blank entry) stop the run at engage with the documented reason.
#[test]
fn usage_probe_engage_time_validator_errors_stop_the_run_before_the_graph() {
    const GLOBAL: &str = "must name a GLOBAL tool declared for this agent (agent-local tools.* subcommands are not allowed)";
    let cases: &[(&str, Host, &str, &[&str])] = &[
        (
            "unknown tool name",
            RECIPE_HOST,
            "session:\n  chat:\n    instructions: Answer.\n    tools: [nonexistent]\n",
            &["session.chat.tools entry 'nonexistent' must name a GLOBAL tool"],
        ),
        (
            "tool exists on disk but is not in global_tools",
            RECIPE_HOST,
            "session:\n  chat:\n    instructions: Answer.\n    tools: [fs_read]\n",
            &[
                "session.chat.tools entry 'fs_read' must name a GLOBAL tool",
                GLOBAL,
            ],
        ),
        (
            "write tool that is not in global_tools",
            RECIPE_HOST,
            "global_tools: [fs_read.sh]\nsession:\n  chat:\n    instructions: Answer.\n    tools: [fs_read, fs_write]\n",
            &[
                "session.chat.tools entry 'fs_write' must name a GLOBAL tool",
                GLOBAL,
            ],
        ),
        (
            "integer entry",
            RECIPE_HOST,
            "session:\n  chat:\n    instructions: Answer.\n    tools: [1]\n",
            &["session.chat.tools entry '1' must name a GLOBAL tool"],
        ),
        (
            "blank entry",
            RECIPE_HOST,
            "session:\n  chat:\n    instructions: Answer.\n    tools: [\"\"]\n",
            &["session.chat.tools entry '' must name a GLOBAL tool"],
        ),
        (
            "unknown chat.model",
            RECIPE_HOST,
            "session:\n  chat:\n    instructions: Answer.\n    model: nope:x\n",
            &["session.chat.model references unknown model 'nope:x'"],
        ),
        (
            "unknown route.model",
            RECIPE_HOST,
            "session:\n  route:\n    model: nope:x\n  chat:\n    instructions: Answer.\n",
            &["session.route.model references unknown model 'nope:x'"],
        ),
        (
            "empty chat.model",
            RECIPE_HOST,
            "session:\n  chat:\n    instructions: Answer.\n    model: \"\"\n",
            &["session.chat.model references unknown model ''"],
        ),
        (
            "tools on a non-FC chat model",
            Host {
                function_calling: false,
                ..RECIPE_HOST
            },
            "global_tools: [fs_read.sh]\nsession:\n  chat:\n    instructions: Answer.\n    tools: [fs_read]\n",
            &["session.chat.tools: tools unavailable on 'dryrun:dry-model' (no function calling)"],
        ),
        (
            "skill tool that is not declared while skills are on",
            RECIPE_HOST,
            "session:\n  chat:\n    instructions: Answer.\n    tools: [skill__bogus]\n",
            &["session.chat.tools entry 'skill__bogus' must name a GLOBAL tool"],
        ),
    ];
    for (label, host, block, expected) in cases {
        let dir = fresh_config_dir("engage");
        let _guard = TempDirGuard(dir.clone());
        write_fixture_on(&dir, block, *host);
        assert_fails_at_engage(label, &run_echo(&dir), expected);
    }
}

/// `chat.memory: true` on a non-FC chat model warns exactly once at engage
/// (with the inlined-section tail when memory is on, without it when the app
/// has memory off -- then the memory-disabled line fires too), and the run
/// still completes.
#[test]
fn usage_probe_non_fc_chat_model_memory_warning_fires_once_and_the_run_completes() {
    const UNAVAILABLE: &str = "Warning: session.chat.memory: memory tools unavailable on 'dryrun:dry-model' (no function calling)";
    const TAIL: &str = "; the MEMORY.md section is inlined with the without-tools cap";
    const DISABLED: &str = "Warning: session.chat.memory: memory is disabled";
    let block = "session:\n  chat:\n    instructions: Answer.\n    memory: true\n";

    for (label, memory, want_tail, want_disabled) in [
        ("memory on", true, true, 0usize),
        ("memory off", false, false, 1usize),
    ] {
        let dir = fresh_config_dir("nonfc");
        let _guard = TempDirGuard(dir.clone());
        write_fixture_on(
            &dir,
            block,
            Host {
                function_calling: false,
                memory,
                ..RECIPE_HOST
            },
        );
        let output = run_echo(&dir);
        assert_echoes_hello(label, &output);
        let stderr = stderr_of(&output);
        let lines: Vec<&str> = stderr
            .lines()
            .filter(|l| l.starts_with(UNAVAILABLE))
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "{label}: expected exactly one memory-unavailable warning:\n{stderr}"
        );
        assert_eq!(
            lines[0].ends_with(TAIL),
            want_tail,
            "{label}: tail presence mismatch on {:?}",
            lines[0]
        );
        assert_eq!(
            count_lines_starting_with(&stderr, DISABLED),
            want_disabled,
            "{label}: memory-disabled line count:\n{stderr}"
        );
        assert!(
            stderr.is_ascii(),
            "{label}: non-ASCII warning text:\n{stderr}"
        );
    }
}

/// A `skill__*` entry is a warning (not an error) only when the agent
/// declares no skill tools; the run still completes.
#[test]
fn usage_probe_skill_tools_warn_instead_of_erroring_when_skills_are_off() {
    let dir = fresh_config_dir("skills-off");
    let _guard = TempDirGuard(dir.clone());
    write_fixture_on(
        &dir,
        "session:\n  chat:\n    instructions: Answer.\n    tools: [skill__load]\n",
        Host {
            skills_enabled: false,
            ..RECIPE_HOST
        },
    );
    let output = run_echo(&dir);
    assert_echoes_hello("skills off", &output);
    let stderr = stderr_of(&output);
    assert_eq!(
        count_lines_starting_with(
            &stderr,
            "Warning: session.chat.tools entry 'skill__load' names a skill tool"
        ),
        1,
        "skills off: expected one skill-tool warning:\n{stderr}"
    );
    assert!(
        !stderr.contains("Graph validation failed"),
        "skills off: a warning must not become an error:\n{stderr}"
    );

    // With skills on the same entry is a declared function: no line at all.
    let dir = fresh_config_dir("skills-on");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(
        &dir,
        "session:\n  chat:\n    instructions: Answer.\n    tools: [skill__load]\n",
    );
    let output = run_echo(&dir);
    assert_echoes_hello("skills on", &output);
    assert!(
        !stderr_of(&output).contains("session.chat.tools"),
        "skills on: unexpected session.chat.tools line:\n{}",
        stderr_of(&output)
    );
}

/// Agents without a session block -- a block-less graph agent and a config
/// agent -- never see the new validator, even on a non-FC model with memory
/// on: no `session.` line of any kind.
#[test]
fn usage_probe_block_less_graph_and_config_agents_print_no_session_lines() {
    let host = Host {
        function_calling: false,
        memory: true,
        ..RECIPE_HOST
    };

    let dir = fresh_config_dir("blockless");
    let _guard = TempDirGuard(dir.clone());
    write_fixture_on(&dir, "", host);
    let output = run_echo(&dir);
    assert_echoes_hello("block-less graph agent", &output);
    assert!(
        !stderr_of(&output).contains("session."),
        "block-less graph agent: unexpected session line:\n{}",
        stderr_of(&output)
    );

    let dir = fresh_config_dir("config-agent");
    let _guard = TempDirGuard(dir.clone());
    write_fixture_on(&dir, "", host);
    let agent_dir = dir.join("agents").join(AGENT);
    fs::remove_file(agent_dir.join("graph.yaml")).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        "name: echo-graph\ndescription: A config agent.\ninstructions: Be brief.\n",
    )
    .unwrap();
    let output = run_echo(&dir);
    assert!(
        output.status.success(),
        "config agent: expected a successful run, got {:?}\n{}",
        output.status,
        stderr_of(&output)
    );
    assert!(
        !stderr_of(&output).contains("session."),
        "config agent: unexpected session line:\n{}",
        stderr_of(&output)
    );
}
