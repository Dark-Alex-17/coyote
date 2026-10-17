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

fn write_fixture(dir: &Path, session_block: &str) {
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
             \x20       supports_function_calling: true\n\
             save: false\n\
             memory: false\n\
             stream: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
    fs::write(
        dir.join("agents").join(AGENT).join("graph.yaml"),
        format!("{BASE_GRAPH}{session_block}"),
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
            "session:\n  expose: [said]\n  route:\n    model: dryrun:dry-model\n    instructions: Prefer chat.\n  chat:\n    model: dryrun:dry-model\n    instructions: Answer.\n    memory: true\n    tools: [fs_read]\n",
        ),
        ("null block", "session:\n"),
    ] {
        let dir = fresh_config_dir("ok");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);

        assert_echoes_hello(label, &run_echo(&dir));
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
    ];

    for (label, block, expected) in cases {
        let dir = fresh_config_dir("err");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);

        assert_fails_at_graph_load(label, &run_echo(&dir), expected);
    }
}
