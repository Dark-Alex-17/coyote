//! Binary-level contract for the graph.yaml `session:` block, driven from a
//! cold config dir the way an agent author would set it up: a dry-run model,
//! an `echo-graph` agent whose graph ends immediately, and `-e hello` run
//! through the real binary. A graph with a valid block (or none) prints the
//! prompt back; every malformed block fails at graph load with the documented
//! error text and never panics. Every run uses a fresh fake HOME and a
//! scrubbed sandbox/provider env so host state cannot leak in.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process};

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const AGENT: &str = "echo-graph";

/// The built binary every run here spawns; the one place the harness
/// names it, so a test that needs its directory reads it from here.
const COYOTE_BIN: &str = env!("CARGO_BIN_EXE_coyote");

/// The workspace-memory root override the unit-test guards set; it mirrors
/// `WORKSPACE_MEMORY_ROOT_TEST_ENV` in `src/config/memory.rs`, which an
/// integration test cannot import from the binary crate.
const TEST_WORKSPACE_MEMORY_ROOT_ENV: &str = "COYOTE_TEST_WORKSPACE_MEMORY_ROOT";

/// The `name:` the fixture graph declares; the run-time logger keys its
/// `[graph:<name>]` prefix on `graph.name`, not on the agent directory.
const GRAPH_NAME: &str = "echo-graph";

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
    run_with(dir, &["-a", AGENT, "-e", "hello"])
}

/// The real binary over `dir` with `args`, from a scrubbed env and no TTY.
fn run_with(dir: &Path, args: &[&str]) -> Output {
    run_with_env(dir, args, &[])
}

/// `run_with` plus extra environment pairs applied after the scrub, for
/// cases that pin a host knob (the log level) rather than inherit it.
fn run_with_env(dir: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    run_from(dir, dir, args, extra_env)
}

/// `run_with_env` with the process started in `cwd` instead of the config
/// dir, for cases where the directory the binary runs from is the lever
/// (workspace discovery walks up from it).
fn run_from(cwd: &Path, dir: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let home = dir.join("home");
    let binary = Path::new(COYOTE_BIN);
    Command::new(binary)
        .args(args)
        .current_dir(cwd)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        .env("PATH", pinned_path(binary.parent().unwrap()))
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("COYOTE_WORKSPACE_CONFIG_DIR")
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
        // The unit-test guard's workspace-memory pin; a shipped binary
        // ignores it, and a run here starts without it unless a case sets it.
        .env_remove(TEST_WORKSPACE_MEMORY_ROOT_ENV)
        .envs(extra_env.iter().copied())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// A PATH holding only `bin_dir` (the built binary's own directory) and the
/// system shell directories, so the fixture's `sh`/`bash` hooks and tool
/// scripts resolve the same way on every host and nothing else on the
/// developer's PATH can leak into a run.
fn pinned_path(bin_dir: &Path) -> std::ffi::OsString {
    let system_dirs: Vec<PathBuf> = if cfg!(windows) {
        let system_root = env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        vec![Path::new(&system_root).join("System32")]
    } else {
        vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
    };
    env::join_paths(std::iter::once(bin_dir.to_path_buf()).chain(system_dirs)).unwrap()
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

/// Reads the echoed request a dry-run teammate returned (a YAML message
/// list) into `(role, content)` pairs.
fn echoed_messages(echo: &str) -> Vec<(String, String)> {
    let doc: serde_yaml::Value = serde_yaml::from_str(echo)
        .unwrap_or_else(|e| panic!("a teammate's output is not the echoed request ({e}):\n{echo}"));
    doc.as_sequence()
        .unwrap_or_else(|| panic!("the echoed request is not a message list:\n{echo}"))
        .iter()
        .map(|message| {
            let role = message["role"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let content = match &message["content"] {
                serde_yaml::Value::String(text) => text.clone(),
                other => serde_yaml::to_string(other).unwrap_or_default(),
            };
            (role, content)
        })
        .collect()
}

/// A config agent that runs on its own named session (`agent_session`), so
/// a dry-run call echoes the full message list it would have sent rather
/// than the flattened role text.
fn write_sessioned_config_agent(dir: &Path, name: &str) {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        format!(
            "name: {name}\ndescription: A helper.\ninstructions: Be brief.\nagent_session: t\n"
        ),
    )
    .unwrap();
}

/// Two config agents fanned out in one super-step with `teammates: true`
/// each run as a teammate of the other. Under dry run every model call
/// echoes the request it would have sent, so the state the fan-out leaves
/// behind shows exactly what each teammate was told: the roster rides the
/// System message (after the agent's own instructions), names the agent's
/// own seat by the node id and the other node as the peer, and the User
/// message is the bare `prompt:` of the node with no roster in it. Without
/// `teammates: true` neither request carries a roster. A script node dumps
/// the raw state to a file so the oracle does not depend on how the end
/// node's markdown is rendered.
#[test]
fn usage_probe_config_teammates_in_a_fan_out_get_the_roster_in_the_system_prompt_only() {
    if cfg!(windows) {
        eprintln!("skipping: the fan-out joins through a bash script");
        return;
    }
    for teammates in [true, false] {
        let dir = fresh_config_dir("teammate-roster");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, "");
        write_sessioned_config_agent(&dir, "alpha");
        write_sessioned_config_agent(&dir, "beta");
        let agent_dir = dir.join("agents").join(AGENT);
        let scripts = agent_dir.join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        fs::write(scripts.join("fan.sh"), "#!/usr/bin/env bash\nprintf '{}'\n").unwrap();
        let dump = dir.join("state.json");
        fs::write(
            scripts.join("dump.sh"),
            format!(
                "#!/usr/bin/env bash\n\
                 out='{}'\n\
                 if [ -n \"${{GRAPH_STATE_FILE:-}}\" ]; then cp \"$GRAPH_STATE_FILE\" \"$out\"; \
                 else printf '%s' \"$GRAPH_STATE\" > \"$out\"; fi\n\
                 printf '{{}}'\n",
                dump.display()
            ),
        )
        .unwrap();
        let flag = if teammates { "teammates: true, " } else { "" };
        fs::write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: echo-graph\n\
                 version: \"1.0\"\n\
                 initial_state: {{ a: \"\", b: \"\" }}\n\
                 start: fan\n\
                 nodes:\n\
                 \x20 fan: {{ type: script, script: scripts/fan.sh, next: [worker_a, worker_b] }}\n\
                 \x20 worker_a: {{ type: agent, agent: alpha, prompt: \"hi from a\", {flag}state_updates: {{ a: \"{{{{output}}}}\" }}, next: join }}\n\
                 \x20 worker_b: {{ type: agent, agent: beta, prompt: \"hi from b\", {flag}state_updates: {{ b: \"{{{{output}}}}\" }}, next: join }}\n\
                 \x20 join: {{ type: script, script: scripts/dump.sh, next: done }}\n\
                 \x20 done: {{ type: end, output: \"done\" }}\n"
            ),
        )
        .unwrap();

        let output = run_with(&dir, &["-a", AGENT, "go"]);
        let text = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
        assert!(
            output.status.success()
                && !text.contains("panicked")
                && stdout_of(&output).lines().any(|l| l == "done"),
            "teammates={teammates}: the fan-out completes: exit {:?}\n{text}",
            output.status
        );
        let state = fs::read_to_string(&dump).unwrap_or_else(|e| {
            panic!("teammates={teammates}: the join dumped no state ({e}):\n{text}")
        });
        let state: serde_json::Value = serde_json::from_str(&state).unwrap();

        for (key, own, peer, prompt) in [
            ("a", "worker_a", "worker_b", "hi from a"),
            ("b", "worker_b", "worker_a", "hi from b"),
        ] {
            let echo = state[key].as_str().unwrap_or_else(|| {
                panic!("teammates={teammates}: state.{key} is not the echo:\n{state}")
            });
            let messages = echoed_messages(echo);
            let (first_role, system) = &messages[0];
            assert_eq!(first_role, "system", "teammates={teammates} {key}:\n{echo}");
            let users: Vec<&String> = messages
                .iter()
                .filter(|(role, _)| role == "user")
                .map(|(_, content)| content)
                .collect();
            assert_eq!(
                users,
                vec![prompt],
                "teammates={teammates} {key}: the User message is the bare node prompt:\n{echo}"
            );
            if teammates {
                assert!(
                    system.starts_with("Be brief.\n\n## Teammate roster\n"),
                    "{key}: the roster follows the agent's own instructions in the System message:\n{system}"
                );
                assert!(
                    system.contains(&format!("({own})")) && system.contains("You are `"),
                    "{key}: the roster names this teammate's own seat by node id:\n{system}"
                );
                assert!(
                    system.contains(peer),
                    "{key}: the roster names the other node as a peer:\n{system}"
                );
                assert_eq!(
                    echo.matches("## Teammate roster").count(),
                    1,
                    "{key}: exactly one roster in the whole request:\n{echo}"
                );
            } else {
                assert!(
                    !echo.contains("Teammate roster"),
                    "{key}: no roster without `teammates: true`:\n{echo}"
                );
            }
        }
    }
}

/// The fan-out graph the roster probes share: `fan` splits into two
/// `teammates: true` agent nodes on config agents `alpha` / `beta`, `join`
/// dumps the raw state (each teammate's echoed request) to `dump`.
fn write_roster_fan_out(dir: &Path, dump: &Path) {
    let agent_dir = dir.join("agents").join(AGENT);
    let scripts = agent_dir.join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    fs::write(scripts.join("fan.sh"), "#!/usr/bin/env bash\nprintf '{}'\n").unwrap();
    fs::write(
        scripts.join("dump.sh"),
        format!(
            "#!/usr/bin/env bash\n\
             out='{}'\n\
             if [ -n \"${{GRAPH_STATE_FILE:-}}\" ]; then cp \"$GRAPH_STATE_FILE\" \"$out\"; \
             else printf '%s' \"$GRAPH_STATE\" > \"$out\"; fi\n\
             printf '{{}}'\n",
            dump.display()
        ),
    )
    .unwrap();
    fs::write(
        agent_dir.join("graph.yaml"),
        "name: echo-graph\n\
         version: \"1.0\"\n\
         initial_state: { a: \"\", b: \"\" }\n\
         start: fan\n\
         nodes:\n\
         \x20 fan: { type: script, script: scripts/fan.sh, next: [worker_a, worker_b] }\n\
         \x20 worker_a: { type: agent, agent: alpha, prompt: \"hi from a\", teammates: true, state_updates: { a: \"{{output}}\" }, next: join }\n\
         \x20 worker_b: { type: agent, agent: beta, prompt: \"hi from b\", teammates: true, state_updates: { b: \"{{output}}\" }, next: join }\n\
         \x20 join: { type: script, script: scripts/dump.sh, next: done }\n\
         \x20 done: { type: end, output: \"done\" }\n",
    )
    .unwrap();
}

/// Runs the roster fan-out and returns the echoed requests keyed `a` / `b`.
fn run_roster_fan_out(label: &str, dir: &Path, dump: &Path) -> serde_json::Value {
    let output = run_with(dir, &["-a", AGENT, "go"]);
    let text = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
    assert!(
        output.status.success()
            && !text.contains("panicked")
            && stdout_of(&output).lines().any(|l| l == "done"),
        "{label}: the fan-out completes: exit {:?}\n{text}",
        output.status
    );
    let state = fs::read_to_string(dump)
        .unwrap_or_else(|e| panic!("{label}: the join dumped no state ({e}):\n{text}"));
    serde_json::from_str(&state).unwrap()
}

/// A teammate that keeps an `agent_session` is RESUMED on the next run:
/// the model sees the stored message list, including the System row saved
/// under the previous run. That row carried the previous run's roster, whose
/// seat ids are minted per run, so a resumed teammate must be re-told the
/// current roster: exactly one `## Teammate roster`, naming this run's node
/// ids, with the stale seat gone, while the earlier exchange survives in
/// order and the new User message is still the bare node prompt. From a cold
/// start the previous run is a hand-written session file: the stored System
/// row is the agent's instructions plus a roster for a seat that no longer
/// exists.
#[test]
fn usage_probe_a_resumed_agent_session_teammate_is_re_told_the_current_roster() {
    if cfg!(windows) {
        eprintln!("skipping: the fan-out joins through a bash script");
        return;
    }
    const STALE_SEAT: &str = "graph_agent_alpha_deadbeef";
    let dir = fresh_config_dir("teammate-resumed-roster");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");
    write_sessioned_config_agent(&dir, "alpha");
    write_sessioned_config_agent(&dir, "beta");
    let dump = dir.join("state.json");
    write_roster_fan_out(&dir, &dump);

    // alpha's `agent_session: t` from an earlier run: the stored System row
    // is the instructions plus that run's roster, followed by one exchange.
    let stale_system = format!(
        "Be brief.\n\n## Teammate roster\nYou are `{STALE_SEAT}` (worker_a). Peers: `graph_agent_beta_cafef00d` (worker_b)."
    );
    let sessions = dir.join("agents").join("alpha").join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("t.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             agent_instructions: {system:?}\n\
             messages:\n\
             \x20 - role: system\n\
             \x20   content: {system:?}\n\
             \x20 - role: user\n\
             \x20   content: earlier\n\
             \x20 - role: assistant\n\
             \x20   content: noted\n",
            system = stale_system
        ),
    )
    .unwrap();

    let state = run_roster_fan_out("resumed", &dir, &dump);
    let echo = state["a"]
        .as_str()
        .unwrap_or_else(|| panic!("state.a is not the echo:\n{state}"));
    let messages = echoed_messages(echo);
    let (first_role, system) = &messages[0];
    assert_eq!(first_role, "system", "{echo}");
    assert!(
        system.starts_with("Be brief.\n\n## Teammate roster\n"),
        "the resumed System row is the instructions plus a roster:\n{system}"
    );
    assert!(
        system.contains("(worker_a)") && system.contains("worker_b"),
        "the roster names this run's own seat and peer:\n{system}"
    );
    assert!(
        !echo.contains(STALE_SEAT) && !echo.contains("cafef00d"),
        "the previous run's seat ids are gone from the whole request:\n{echo}"
    );
    assert_eq!(
        echo.matches("## Teammate roster").count(),
        1,
        "exactly one roster after the re-adoption:\n{echo}"
    );
    let roles: Vec<&str> = messages.iter().map(|(role, _)| role.as_str()).collect();
    assert_eq!(
        roles,
        ["system", "user", "assistant", "user"],
        "the earlier exchange survives in order ahead of the new prompt:\n{echo}"
    );
    let users: Vec<&String> = messages
        .iter()
        .filter(|(role, _)| role == "user")
        .map(|(_, content)| content)
        .collect();
    assert_eq!(
        users,
        vec!["earlier", "hi from a"],
        "the stored User text is untouched and the new one is the bare prompt:\n{echo}"
    );

    // The fresh teammate in the same run is unaffected by its peer's resume.
    let echo_b = state["b"].as_str().unwrap();
    assert_eq!(echo_b.matches("## Teammate roster").count(), 1, "{echo_b}");
    assert!(!echo_b.contains(STALE_SEAT), "{echo_b}");
}

/// A teammate whose instructions come from its `_instructions` hook every
/// turn (`dynamic_instructions: true`) still carries the roster: the roster
/// is appended after whatever text the hook produced, so the System row
/// starts with the hook's text and ends with exactly one roster naming this
/// run's seats. The hook's text differs from the static `instructions:`
/// line on purpose, proving the roster rode the dynamic branch and not a
/// stale static copy.
#[test]
fn usage_probe_a_dynamic_instructions_teammate_carries_the_roster_after_the_rendered_text() {
    if cfg!(windows) {
        eprintln!("skipping: the fan-out joins through a bash script");
        return;
    }
    let dir = fresh_config_dir("teammate-dynamic-roster");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");
    write_sessioned_config_agent(&dir, "beta");
    let alpha_dir = dir.join("agents").join("alpha");
    fs::create_dir_all(&alpha_dir).unwrap();
    fs::write(
        alpha_dir.join("config.yaml"),
        "name: alpha\n\
         description: A helper.\n\
         instructions: Be brief.\n\
         dynamic_instructions: true\n\
         agent_session: t\n",
    )
    .unwrap();
    fs::write(
        alpha_dir.join("tools.sh"),
        "#!/usr/bin/env bash\n\
         set -eo pipefail\n\
         # @env LLM_OUTPUT=/dev/stdout\n\
         # @describe alpha tools\n\
         \n\
         # @cmd Dynamic instructions hook.\n\
         _instructions() { printf 'Be brief about cats.' >> \"$LLM_OUTPUT\"; }\n",
    )
    .unwrap();
    let dump = dir.join("state.json");
    write_roster_fan_out(&dir, &dump);

    let state = run_roster_fan_out("dynamic", &dir, &dump);
    let echo = state["a"]
        .as_str()
        .unwrap_or_else(|| panic!("state.a is not the echo:\n{state}"));
    let messages = echoed_messages(echo);
    let (first_role, system) = &messages[0];
    assert_eq!(first_role, "system", "{echo}");
    assert!(
        system.starts_with("Be brief about cats.\n\n## Teammate roster\n"),
        "the roster follows the hook's dynamic instructions:\n{system}"
    );
    assert!(
        !system.starts_with("Be brief.\n"),
        "the static `instructions:` line did not win over the dynamic hook:\n{system}"
    );
    assert!(
        system.contains("(worker_a)") && system.contains("worker_b"),
        "the roster names this run's own seat and peer:\n{system}"
    );
    assert_eq!(
        echo.matches("## Teammate roster").count(),
        1,
        "exactly one roster in the whole request:\n{echo}"
    );
    let users: Vec<&String> = messages
        .iter()
        .filter(|(role, _)| role == "user")
        .map(|(_, content)| content)
        .collect();
    assert_eq!(
        users,
        vec!["hi from a"],
        "the User message is the bare node prompt:\n{echo}"
    );
}

/// A single PLAIN agent node (no `teammates: true`) on config agent `alpha`,
/// followed by the same state dump the roster fan-out uses: `worker` is the
/// start node, `join` dumps the raw state to `dump`.
fn write_plain_resume_graph(dir: &Path, dump: &Path) {
    write_roster_fan_out(dir, dump);
    fs::write(
        dir.join("agents").join(AGENT).join("graph.yaml"),
        "name: echo-graph\n\
         version: \"1.0\"\n\
         initial_state: { a: \"\" }\n\
         start: worker\n\
         nodes:\n\
         \x20 worker: { type: agent, agent: alpha, prompt: \"hi from a\", state_updates: { a: \"{{output}}\" }, next: join }\n\
         \x20 join: { type: script, script: scripts/dump.sh, next: done }\n\
         \x20 done: { type: end, output: \"done\" }\n",
    )
    .unwrap();
}

/// Writes alpha's stored `agent_session: t` from an earlier run: `system`
/// is both the System row and `agent_instructions`, followed by one
/// exchange; `extra` is spliced in as further top-level session fields.
fn write_alpha_session(dir: &Path, system: &str, extra: &str) {
    let sessions = dir.join("agents").join("alpha").join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("t.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             agent_instructions: {system:?}\n\
             {extra}\
             messages:\n\
             \x20 - role: system\n\
             \x20   content: {system:?}\n\
             \x20 - role: user\n\
             \x20   content: earlier\n\
             \x20 - role: assistant\n\
             \x20   content: noted\n",
        ),
    )
    .unwrap();
}

/// Resuming a stored `agent_session` from a PLAIN agent node (no
/// `teammates: true`) is the unseated shape: this run has no roster to
/// append, yet the stored System row may carry the previous run's roster
/// for seats that no longer exist. That row is re-adopted from the agent's
/// own instructions, so the model sees exactly `Be brief.` with no roster
/// text anywhere, the earlier exchange in order and the bare node prompt.
/// A stored session that never carried a roster is the ordinary resume: it
/// keeps speaking with the System row it was saved under, even though the
/// configured instructions differ, so nothing re-adopts it.
#[test]
fn usage_probe_an_unseated_resume_drops_a_stored_roster_and_leaves_a_plain_row_alone() {
    if cfg!(windows) {
        eprintln!("skipping: the state dump goes through a bash script");
        return;
    }
    const STALE_SEAT: &str = "graph_agent_alpha_deadbeef";
    for (label, stored_system, expected_system) in [
        (
            "stale roster",
            format!(
                "Be brief.\n\n## Teammate roster\nYou are `{STALE_SEAT}` (worker_a). Peers: `graph_agent_beta_cafef00d` (worker_b)."
            ),
            "Be brief.".to_string(),
        ),
        (
            "no roster",
            "Old prompt from the stored run.".to_string(),
            "Old prompt from the stored run.".to_string(),
        ),
    ] {
        let dir = fresh_config_dir("unseated-resume");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, "");
        write_sessioned_config_agent(&dir, "alpha");
        let dump = dir.join("state.json");
        write_plain_resume_graph(&dir, &dump);
        write_alpha_session(&dir, &stored_system, "");

        let state = run_roster_fan_out(label, &dir, &dump);
        let echo = state["a"]
            .as_str()
            .unwrap_or_else(|| panic!("{label}: state.a is not the echo:\n{state}"));
        let messages = echoed_messages(echo);
        let (first_role, system) = &messages[0];
        assert_eq!(first_role, "system", "{label}:\n{echo}");
        // A re-adopted row is the agent's prompt, which the built-in tool
        // guidance follows; a row left alone is sent exactly as stored.
        assert!(
            system.starts_with(&expected_system),
            "{label}: the System row the resumed agent speaks with:\n{echo}"
        );
        if label == "no roster" {
            assert_eq!(
                system, &expected_system,
                "{label}: nothing re-adopts a roster-less row:\n{echo}"
            );
        }
        assert!(
            !echo.contains("Teammate roster") && !echo.contains(STALE_SEAT),
            "{label}: no roster text anywhere in the request:\n{echo}"
        );
        let roles: Vec<&str> = messages.iter().map(|(role, _)| role.as_str()).collect();
        assert_eq!(
            roles,
            ["system", "user", "assistant", "user"],
            "{label}:\n{echo}"
        );
        let users: Vec<&String> = messages
            .iter()
            .filter(|(role, _)| role == "user")
            .map(|(_, content)| content)
            .collect();
        assert_eq!(users, vec!["earlier", "hi from a"], "{label}:\n{echo}");
    }
}

/// The same unseated resume for a `dynamic_instructions` agent: the stored
/// session re-enters as the session's dynamic text, so the model sees the
/// text the earlier run stored (not what the hook says today) with its
/// trailing roster cut, and nothing else.
#[test]
fn usage_probe_an_unseated_dynamic_instructions_resume_keeps_the_stored_text_minus_its_roster() {
    if cfg!(windows) {
        eprintln!("skipping: the state dump goes through a bash script");
        return;
    }
    let dir = fresh_config_dir("unseated-dynamic-resume");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");
    let alpha_dir = dir.join("agents").join("alpha");
    fs::create_dir_all(&alpha_dir).unwrap();
    fs::write(
        alpha_dir.join("config.yaml"),
        "name: alpha\n\
         description: A helper.\n\
         instructions: Be brief.\n\
         dynamic_instructions: true\n\
         agent_session: t\n",
    )
    .unwrap();
    fs::write(
        alpha_dir.join("tools.sh"),
        "#!/usr/bin/env bash\n\
         set -eo pipefail\n\
         # @env LLM_OUTPUT=/dev/stdout\n\
         # @describe alpha tools\n\
         \n\
         # @cmd Dynamic instructions hook.\n\
         _instructions() { printf 'Be brief about dogs.' >> \"$LLM_OUTPUT\"; }\n",
    )
    .unwrap();
    let dump = dir.join("state.json");
    write_plain_resume_graph(&dir, &dump);
    write_alpha_session(
        &dir,
        "Be brief about cats.\n\n## Teammate roster\nYou are `graph_agent_alpha_deadbeef` (worker_a).",
        "",
    );

    let state = run_roster_fan_out("dynamic unseated", &dir, &dump);
    let echo = state["a"]
        .as_str()
        .unwrap_or_else(|| panic!("state.a is not the echo:\n{state}"));
    let messages = echoed_messages(echo);
    let (first_role, system) = &messages[0];
    assert_eq!(first_role, "system", "{echo}");
    assert!(
        system.starts_with("Be brief about cats."),
        "the stored dynamic text, roster cut, hook text not re-run:\n{echo}"
    );
    assert!(
        !echo.contains("Teammate roster") && !echo.contains("deadbeef"),
        "{echo}"
    );
    assert!(
        !echo.contains("dogs"),
        "the stored session is not re-rendered by the hook:\n{echo}"
    );
    let users: Vec<&String> = messages
        .iter()
        .filter(|(role, _)| role == "user")
        .map(|(_, content)| content)
        .collect();
    assert_eq!(users, vec!["earlier", "hi from a"], "{echo}");
}

/// A teammate resumed on a COMPRESSED `agent_session`: the stored System row
/// is the prompt of the run that compressed it followed by the summary, and
/// `compressed_system_prompt` holds that prompt. Re-adoption replaces only
/// the prompt at the head of the row, so the summary survives behind this
/// run's prompt and roster. When the row no longer starts with the stored
/// prompt, nothing is rewritten: the row is sent as stored and the run log
/// says why.
#[test]
fn usage_probe_a_compressed_resume_keeps_its_summary_and_a_drifted_row_is_left_alone() {
    if cfg!(windows) {
        eprintln!("skipping: the fan-out joins through a bash script");
        return;
    }
    const STALE_SEAT: &str = "graph_agent_alpha_deadbeef";
    const SUMMARY: &str = "Summary: earlier we settled on cats.";
    let stale_system = format!(
        "Be brief.\n\n## Teammate roster\nYou are `{STALE_SEAT}` (worker_a). Peers: `graph_agent_beta_cafef00d` (worker_b)."
    );
    for (label, stored_prompt, row) in [
        (
            "prefix hit",
            stale_system.clone(),
            format!("{stale_system}\n\n{SUMMARY}"),
        ),
        (
            "prefix miss",
            "Be brief.".to_string(),
            format!("Something else entirely.\n\n{stale_system}\n\n{SUMMARY}"),
        ),
    ] {
        let dir = fresh_config_dir("compressed-resume");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, "");
        write_sessioned_config_agent(&dir, "alpha");
        write_sessioned_config_agent(&dir, "beta");
        let dump = dir.join("state.json");
        write_roster_fan_out(&dir, &dump);
        let sessions = dir.join("agents").join("alpha").join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join("t.yaml"),
            format!(
                "model: dryrun:dry-model\n\
                 agent_instructions: {stale_system:?}\n\
                 compressed_system_prompt: {stored_prompt:?}\n\
                 messages:\n\
                 \x20 - role: system\n\
                 \x20   content: {row:?}\n\
                 \x20 - role: user\n\
                 \x20   content: earlier\n\
                 \x20 - role: assistant\n\
                 \x20   content: noted\n",
            ),
        )
        .unwrap();

        let output = run_with_env(&dir, &["-a", AGENT, "go"], &[("COYOTE_LOG_LEVEL", "warn")]);
        let text = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
        assert!(
            output.status.success()
                && !text.contains("panicked")
                && stdout_of(&output).lines().any(|l| l == "done"),
            "{label}: the fan-out completes: exit {:?}\n{text}",
            output.status
        );
        let state: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&dump).unwrap()).unwrap();
        let echo = state["a"]
            .as_str()
            .unwrap_or_else(|| panic!("{label}: state.a is not the echo:\n{state}"));
        let messages = echoed_messages(echo);
        let (first_role, system) = &messages[0];
        assert_eq!(first_role, "system", "{label}:\n{echo}");
        let log = fs::read_to_string(dir.join("cache").join("coyote.log")).unwrap_or_default();
        let kept_line = "keeps its summary row: it does not start with the stored prompt";
        if label == "prefix hit" {
            assert!(
                system.starts_with("Be brief.\n\n## Teammate roster\n"),
                "{label}: this run's prompt and roster lead the row:\n{system}"
            );
            assert!(
                system.ends_with(&format!("\n\n{SUMMARY}")),
                "{label}: the summary survives behind the fresh prompt:\n{system}"
            );
            assert!(
                system.contains("(worker_a)") && system.contains("worker_b"),
                "{label}: the roster names this run's seats:\n{system}"
            );
            assert!(
                !echo.contains(STALE_SEAT) && !echo.contains("cafef00d"),
                "{label}: the stale seats are gone:\n{echo}"
            );
            assert_eq!(
                echo.matches("## Teammate roster").count(),
                1,
                "{label}:\n{echo}"
            );
            assert!(
                !log.contains(kept_line),
                "{label}: nothing to warn about:\n{log}"
            );
        } else {
            assert_eq!(
                system, &row,
                "{label}: a row that drifted from the stored prompt is sent as stored:\n{echo}"
            );
            assert_eq!(
                log.lines()
                    .filter(|l| l.contains("[WARN]") && l.contains(kept_line))
                    .count(),
                1,
                "{label}: the run log says once why the row was left alone:\n{log}"
            );
        }
        let users: Vec<&String> = messages
            .iter()
            .filter(|(role, _)| role == "user")
            .map(|(_, content)| content)
            .collect();
        assert_eq!(users, vec!["earlier", "hi from a"], "{label}:\n{echo}");
    }
}

/// Declaring `carry:` lifts the engage-time validator pass onto a block-less
/// graph, but not the session rule: a graph agent without a `session:` block
/// is still refused a `--session`, with the teaching sentence, and no session
/// file (and so no carried state) is written. The same graph takes the
/// session the moment a block exists.
#[test]
fn usage_probe_a_carry_only_graph_is_still_refused_a_session() {
    const TEACHING: &str = "Graph-based agent 'echo-graph' has no `session:` block in its graph.yaml, \
         so it cannot hold a session. Add a `session:` block (see the Graph Agents \
         wiki page, \"Sessions\") to enable one.";
    let dir = fresh_config_dir("carry-no-session");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "carry: [said]\n");
    let config = dir.join("config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replace("dry_run: true\n", "dry_run: false\nsave_session: true\n"),
    )
    .unwrap();

    let output = run_with(&dir, &["-a", AGENT, "--session", "s", "hello"]);
    let text = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
    assert!(
        !output.status.success() && !text.contains("panicked"),
        "a carry-only graph cannot take a session: exit {:?}\n{text}",
        output.status
    );
    assert!(
        text.contains(TEACHING),
        "the refusal carries the teaching sentence:\n{text}"
    );
    let sessions = dir.join("agents").join(AGENT).join("sessions");
    assert!(
        !sessions.join("s.yaml").exists(),
        "a refused session writes no file:\n{text}"
    );

    // The same graph with a block holds the session and carries `said`.
    let dir = fresh_config_dir("carry-with-session");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");
    let config = dir.join("config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replace("dry_run: true\n", "dry_run: false\nsave_session: true\n"),
    )
    .unwrap();
    fs::write(
        dir.join("agents").join(AGENT).join("graph.yaml"),
        "name: echo-graph\n\
         version: \"1.0\"\n\
         initial_state: { said: \"\" }\n\
         carry: [said]\n\
         start: done\n\
         nodes:\n\
         \x20 done: { type: end, output: \"{{initial_prompt}}\", state_updates: { said: \"{{initial_prompt}}\" } }\n\
         session:\n\
         \x20 chat:\n\
         \x20   instructions: Answer.\n",
    )
    .unwrap();
    let output = run_with(&dir, &["-a", AGENT, "--session", "s", "hello"]);
    let text = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
    assert!(
        output.status.success() && !text.contains("panicked"),
        "the same graph with a block takes the session: exit {:?}\n{text}",
        output.status
    );
    let yaml = fs::read_to_string(
        dir.join("agents")
            .join(AGENT)
            .join("sessions")
            .join("s.yaml"),
    )
    .unwrap_or_else(|e| panic!("the session file was not written ({e}):\n{text}"));
    let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(
        doc["last_run"]["carried"]["said"].as_str(),
        Some("hello"),
        "the carried key is recorded on the session:\n{yaml}"
    );
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
            "reserved expose key",
            "session:\n  expose: [previous_turns]\n  chat:\n    instructions: Answer.\n",
            &["session.expose key 'previous_turns' is reserved (the dispatcher seeds it)"],
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

/// The deny is a prefix rule, not a name registry: a name under a denied
/// family that no built-in carries is still refused at graph load with the
/// deny sentence (never the GLOBAL-tool error), and the load-time deny wins
/// over every engage-time check even when an undeclared plain name sits
/// ahead of it in the list.
#[test]
fn usage_probe_denied_prefix_wins_at_load_whatever_else_the_list_holds() {
    const GLOBAL: &str = "must name a GLOBAL tool declared for this agent";
    for (label, block, denied) in [
        (
            "unknown name under a denied family",
            "global_tools: [fs_read.sh]\nsession:\n  chat:\n    instructions: Answer.\n    tools: [agent__does_not_exist]\n",
            "agent__does_not_exist",
        ),
        (
            "undeclared plain name ahead of a denied one",
            "session:\n  chat:\n    instructions: Answer.\n    tools: [not_a_tool, user__anything]\n",
            "user__anything",
        ),
        (
            "bare family prefix as the whole name",
            "global_tools: [fs_read.sh]\nsession:\n  chat:\n    instructions: Answer.\n    tools: [fs_read, memory__]\n",
            "memory__",
        ),
    ] {
        let dir = fresh_config_dir("deny-prefix");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);
        let output = run_echo(&dir);
        assert_fails_at_graph_load(
            label,
            &output,
            &[&format!(
                "session.chat.tools entry '{denied}' is not allowed in the chat turn (the agent__, job__, todo__, user__ and memory__ built-in families are refused)"
            )],
        );
        let stderr = stderr_of(&output);
        assert!(
            !stderr.contains(GLOBAL) && !stderr.contains("Graph validation failed with"),
            "{label}: the load-time deny must come before any engage-time check:\n{stderr}"
        );
        assert!(
            !stderr.contains("Warning:"),
            "{label}: nothing is warned about when the block is refused at load:\n{stderr}"
        );
        assert!(
            stdout_of(&output).trim().is_empty(),
            "{label}: a refused load must print nothing to stdout:\n{}",
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

/// No global tool is refused by NAME: a block listing the bundled write AND
/// exec tools (both declared as globals, memory left off) engages with no
/// `session.chat.tools` line and no warning at all, and the run completes.
/// The same two names fail with the unchanged undeclared-global error once
/// the agent stops declaring them, so the only gate left is the declaration.
#[test]
fn usage_probe_declared_write_and_exec_globals_engage_silently_and_undeclared_ones_are_refused() {
    const GLOBAL: &str = "must name a GLOBAL tool declared for this agent (agent-local tools.* subcommands are not allowed)";
    let dir = fresh_config_dir("write-exec");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(
        &dir,
        "global_tools: [fs_write.sh, execute_command.sh, web_search_coyote.sh]\nsession:\n  chat:\n    instructions: Answer.\n    tools: [fs_write, execute_command]\n",
    );
    let output = run_echo(&dir);
    assert_echoes_hello("declared write + exec", &output);
    let stderr = stderr_of(&output);
    assert!(
        !stderr.contains("session.chat.tools"),
        "declared write + exec: no session.chat.tools line of any kind:\n{stderr}"
    );
    assert!(
        !stderr.contains("Warning:") && !stderr.contains("not allowed"),
        "declared write + exec: no warning or refusal:\n{stderr}"
    );
    for tool in ["fs_write.sh", "execute_command.sh"] {
        assert!(
            dir.join("functions").join("tools").join(tool).is_file(),
            "the built-in install must have written {tool}"
        );
    }

    // Every listed name with no declaration: both refused, each named, the
    // text unchanged, and the count on the frame matches.
    let dir = fresh_config_dir("write-exec-undeclared");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(
        &dir,
        "session:\n  chat:\n    instructions: Answer.\n    tools: [fs_write, execute_command]\n",
    );
    let output = run_echo(&dir);
    assert_fails_at_engage(
        "undeclared write + exec",
        &output,
        &[
            "session.chat.tools entry 'fs_write' must name a GLOBAL tool",
            "session.chat.tools entry 'execute_command' must name a GLOBAL tool",
            GLOBAL,
            "Graph validation failed with 2 error(s)",
        ],
    );
    assert!(
        !stderr_of(&output).contains("not allowed in the chat turn"),
        "undeclared write + exec: must be the declaration error, not a deny:\n{}",
        stderr_of(&output)
    );
}

/// An agent-local `tools.*` subcommand is never listable, declared global or
/// not: the cold-start run stops at engage with the unchanged global-only
/// sentence, while the same block with the local name removed engages.
#[test]
fn usage_probe_an_agent_local_subcommand_in_chat_tools_is_refused_at_engage() {
    const LOCAL_TOOLS: &str = "#!/usr/bin/env bash\n# @describe Agent-local tools\n\n# @cmd Say hello from the agent\nlocal_hello() { echo hello; }\n";
    let dir = fresh_config_dir("local-tool");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(
        &dir,
        "global_tools: [fs_read.sh, fs_write.sh]\nsession:\n  chat:\n    instructions: Answer.\n    tools: [fs_write, local_hello]\n",
    );
    fs::write(dir.join("agents").join(AGENT).join("tools.sh"), LOCAL_TOOLS).unwrap();
    let output = run_echo(&dir);
    assert_fails_at_engage(
        "agent-local subcommand",
        &output,
        &[
            "session.chat.tools entry 'local_hello' must name a GLOBAL tool declared for this agent (agent-local tools.* subcommands are not allowed)",
            "Graph validation failed with 1 error(s)",
        ],
    );
    let stderr = stderr_of(&output);
    assert!(
        !stderr.contains("entry 'fs_write'"),
        "agent-local subcommand: the declared global beside it is not an error:\n{stderr}"
    );
    assert!(
        !stderr.contains("not allowed in the chat turn"),
        "agent-local subcommand: a local tool is a declaration error, not a deny:\n{stderr}"
    );

    // Control: the same agent (local tools.sh still present) with only the
    // declared global listed engages and runs.
    let dir = fresh_config_dir("local-tool-control");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(
        &dir,
        "global_tools: [fs_read.sh, fs_write.sh]\nsession:\n  chat:\n    instructions: Answer.\n    tools: [fs_write]\n",
    );
    fs::write(dir.join("agents").join(AGENT).join("tools.sh"), LOCAL_TOOLS).unwrap();
    let output = run_echo(&dir);
    assert_echoes_hello("agent-local control", &output);
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

/// The config agent a delegating block may name: `agents/<name>/config.yaml`
/// with the given declared variable names (metadata only, no defaults).
fn write_delegate_config_agent(dir: &Path, name: &str, variables: &[&str]) {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    let declared = variables
        .iter()
        .map(|v| format!("  - name: {v}\n    description: {v}\n"))
        .collect::<String>();
    let variables = if declared.is_empty() {
        String::new()
    } else {
        format!("variables:\n{declared}")
    };
    fs::write(
        agent_dir.join("config.yaml"),
        format!("name: {name}\ndescription: A helper.\ninstructions: Be brief.\n{variables}"),
    )
    .unwrap();
}

/// A second graph agent (`agents/<name>/graph.yaml`) for the "must name a
/// config agent" arm.
fn write_delegate_graph_agent(dir: &Path, name: &str) {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("graph.yaml"),
        BASE_GRAPH.replacen("name: echo-graph", &format!("name: {name}"), 1),
    )
    .unwrap();
}

/// Every pure-YAML rule of a delegating `chat:` block fails at graph load
/// with its documented text: `agent` beside any
/// non-default inline key, `agent` naming the graph itself, `variables`
/// without `agent`, and an inline block whose `instructions` key is absent.
#[test]
fn usage_probe_every_delegating_block_parser_mistake_fails_at_graph_load() {
    const EXCLUSIVE: &str =
        "session.chat.agent is exclusive with instructions, model, memory and tools";
    let cases: &[(&str, &str, &str)] = &[
        (
            "agent beside instructions",
            "session:\n  chat:\n    agent: helper\n    instructions: Answer.\n",
            EXCLUSIVE,
        ),
        (
            "agent beside model",
            "session:\n  chat:\n    agent: helper\n    model: dryrun:dry-model\n",
            EXCLUSIVE,
        ),
        (
            "agent beside memory: true",
            "session:\n  chat:\n    agent: helper\n    memory: true\n",
            EXCLUSIVE,
        ),
        (
            "agent beside a non-empty tools list",
            "global_tools: [fs_read.sh]\nsession:\n  chat:\n    agent: helper\n    tools: [fs_read]\n",
            EXCLUSIVE,
        ),
        (
            "agent names the graph itself",
            "session:\n  chat:\n    agent: echo-graph\n",
            "session.chat.agent must not name the graph agent itself",
        ),
        (
            "variables without agent",
            "session:\n  chat:\n    instructions: Answer.\n    variables: { tone: terse }\n",
            "session.chat.variables requires session.chat.agent",
        ),
        (
            "inline block with the instructions key absent",
            "session:\n  chat:\n    memory: true\n",
            "session.chat.instructions must be non-empty",
        ),
    ];
    for (label, block, expected) in cases {
        let dir = fresh_config_dir("delegate-load");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);
        // The delegate exists and is valid, so only the parser can refuse.
        write_delegate_config_agent(&dir, "helper", &["tone"]);

        assert_fails_at_graph_load(label, &run_echo(&dir), &[expected]);
    }
}

/// The `validate_session_chat_agent` pass runs at engage, graph-first:
/// a delegate that is a graph agent (even one that also has a config.yaml,
/// and even the graph's own directory under a different `name:`) is refused
/// as a graph agent; one with no config.yaml is refused for that; a
/// `variables` key the delegate does not declare is refused with the declared
/// list, or with "declares no variables" when there are none.
#[test]
fn usage_probe_delegating_block_validator_errors_stop_the_run_at_engage() {
    enum Delegate {
        Missing,
        GraphOnly,
        GraphAndConfig,
        Config(&'static [&'static str]),
    }
    let cases: &[(&str, Delegate, &str, &str)] = &[
        (
            "delegate has no agent directory",
            Delegate::Missing,
            "session:\n  chat:\n    agent: helper\n",
            "session.chat.agent must name a config agent (config.yaml); 'helper' has no config.yaml",
        ),
        (
            "delegate is a graph agent",
            Delegate::GraphOnly,
            "session:\n  chat:\n    agent: helper\n",
            "session.chat.agent must name a config agent (config.yaml); 'helper' is a graph agent",
        ),
        (
            "delegate has both graph.yaml and config.yaml",
            Delegate::GraphAndConfig,
            "session:\n  chat:\n    agent: helper\n",
            "session.chat.agent must name a config agent (config.yaml); 'helper' is a graph agent",
        ),
        (
            "undeclared variable with a declared list",
            Delegate::Config(&["tone", "depth"]),
            "session:\n  chat:\n    agent: helper\n    variables: { tone: x, verbose: y }\n",
            "session.chat.variables names 'verbose', which agent 'helper' does not declare (declared: tone, depth)",
        ),
        (
            "undeclared variable when the delegate declares none",
            Delegate::Config(&[]),
            "session:\n  chat:\n    agent: helper\n    variables: { tone: x }\n",
            "session.chat.variables names 'tone', which agent 'helper' does not declare (declares no variables)",
        ),
    ];
    for (label, delegate, block, expected) in cases {
        let dir = fresh_config_dir("delegate-engage");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);
        match delegate {
            Delegate::Missing => {}
            Delegate::GraphOnly => write_delegate_graph_agent(&dir, "helper"),
            Delegate::GraphAndConfig => {
                write_delegate_graph_agent(&dir, "helper");
                write_delegate_config_agent(&dir, "helper", &[]);
            }
            Delegate::Config(vars) => write_delegate_config_agent(&dir, "helper", vars),
        }

        assert_fails_at_engage(label, &run_echo(&dir), &[expected]);
    }

    // The parser compares `agent` against `graph.name`, not the directory:
    // a graph under `agents/echo-graph/` declaring `name: other-echo` and
    // delegating to `echo-graph` passes load and is caught by the validator
    // as a graph agent.
    let dir = fresh_config_dir("delegate-self-dir");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "session:\n  chat:\n    agent: echo-graph\n");
    let graph = dir.join("agents").join(AGENT).join("graph.yaml");
    let renamed =
        fs::read_to_string(&graph)
            .unwrap()
            .replacen("name: echo-graph", "name: other-echo", 1);
    fs::write(&graph, renamed).unwrap();
    let output = run_echo(&dir);
    assert!(
        !stderr_of(&output).contains("must not name the graph agent itself"),
        "directory self-reference: the parser rule is name-based:\n{}",
        stderr_of(&output)
    );
    assert_fails_at_engage(
        "directory self-reference",
        &output,
        &[
            "session.chat.agent must name a config agent (config.yaml); 'echo-graph' is a graph agent",
        ],
    );
}

/// A well-formed delegating block engages like an inline one and, with no
/// session attached, runs the graph exactly as before (the block is inert
/// without a session): bare `agent:`, `agent:` beside the
/// default-valued `memory: false` / `tools: []`, and `agent:` with every
/// `variables` key declared. No `session.` line of any kind is printed.
#[test]
fn usage_probe_a_well_formed_delegating_block_engages_and_echoes_without_a_session() {
    let cases: &[(&str, &[&str], &str)] = &[
        ("bare agent", &[], "session:\n  chat:\n    agent: helper\n"),
        (
            "agent beside default-valued inline keys",
            &[],
            "session:\n  chat:\n    agent: helper\n    memory: false\n    tools: []\n",
        ),
        (
            "agent with every variable declared",
            &["tone", "depth"],
            "session:\n  expose: [said]\n  chat:\n    agent: helper\n    variables: { tone: terse, depth: full }\n",
        ),
    ];
    for (label, declared, block) in cases {
        let dir = fresh_config_dir("delegate-ok");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);
        write_delegate_config_agent(&dir, "helper", declared);

        let output = run_echo(&dir);
        assert_echoes_hello(label, &output);
        let stderr = stderr_of(&output);
        assert!(
            !stderr.contains("session."),
            "{label}: a valid delegating block prints no session line:\n{stderr}"
        );
    }
}

/// The shape rules on `chat.agent` and on `chat.variables` values are
/// checked as the file loads, before any agent directory is consulted: an
/// empty name, a path-shaped name, a name with a trailing space and a
/// variable value carrying the input placeholder each stop the run at graph
/// load with the documented text, even though a valid `helper` exists.
#[test]
fn usage_probe_delegate_name_and_variable_value_shapes_fail_at_graph_load() {
    const PLAIN: &str =
        "session.chat.agent must be a plain agent name (no path separators or reserved names)";
    let cases: &[(&str, &str, &str)] = &[
        (
            "empty agent name",
            "session:\n  chat:\n    agent: \"\"\n",
            "session.chat.agent must be non-empty",
        ),
        (
            "agent name with a path separator",
            "session:\n  chat:\n    agent: ../helper\n",
            PLAIN,
        ),
        (
            "agent name that is the parent directory",
            "session:\n  chat:\n    agent: ..\n",
            PLAIN,
        ),
        (
            "agent name with a trailing space",
            "session:\n  chat:\n    agent: \"helper \"\n",
            PLAIN,
        ),
        (
            "variable value carrying the input placeholder",
            "session:\n  chat:\n    agent: helper\n    variables: { tone: \"say __INPUT__\" }\n",
            "session.chat.variables value for 'tone' must not contain '__INPUT__'",
        ),
    ];
    for (label, block, expected) in cases {
        let dir = fresh_config_dir("delegate-shape");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);
        write_delegate_config_agent(&dir, "helper", &["tone"]);

        let output = run_echo(&dir);
        assert_fails_at_graph_load(label, &output, &[expected]);
        assert!(
            !stderr_of(&output).contains("has no config.yaml")
                && !stderr_of(&output).contains("Failed to engage"),
            "{label}: a shape error is refused by the loader, not by the engage pass:\n{}",
            stderr_of(&output)
        );
    }
}

/// `--info` on a delegating graph agent runs the same engage-time pass the
/// prompt paths do: a delegate without a `config.yaml` fails the inspection
/// with the validator text and a non-zero exit, while a valid delegate
/// inspects cleanly with no `session.` line and no engage failure.
#[test]
fn usage_probe_info_runs_the_delegate_pass_like_an_engage() {
    let missing = fresh_config_dir("delegate-info-missing");
    let _guard = TempDirGuard(missing.clone());
    write_fixture(&missing, "session:\n  chat:\n    agent: nope\n");
    write_delegate_config_agent(&missing, "helper", &[]);
    let output = run_with(&missing, &["-a", AGENT, "--info"]);
    let combined = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
    assert!(
        !output.status.success() && !combined.contains("panicked"),
        "--info with a missing delegate: expected a refusal, got exit {:?}\n{combined}",
        output.status
    );
    assert!(
        stderr_of(&output).contains(
            "session.chat.agent must name a config agent (config.yaml); 'nope' has no config.yaml"
        ),
        "--info with a missing delegate does not carry the validator text:\n{combined}"
    );

    let valid = fresh_config_dir("delegate-info-valid");
    let _guard = TempDirGuard(valid.clone());
    write_fixture(
        &valid,
        "session:\n  chat:\n    agent: helper\n    variables: { tone: terse }\n",
    );
    write_delegate_config_agent(&valid, "helper", &["tone"]);
    let output = run_with(&valid, &["-a", AGENT, "--info"]);
    let combined = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
    assert!(
        output.status.success() && !combined.contains("panicked"),
        "--info with a valid delegate: expected exit 0, got {:?}\n{combined}",
        output.status
    );
    for needle in ["session.", "Failed to engage", "was born with"] {
        assert!(
            !combined.contains(needle),
            "--info with a valid delegate prints {needle:?}:\n{combined}"
        );
    }
    assert!(
        stdout_of(&output).contains(AGENT),
        "--info names the inspected graph agent:\n{combined}"
    );
}

/// The top-level `carry:` list from a cold start: a key declared through
/// `initial_state` loads and the graph still echoes; every documented
/// mistake fails at graph load with its sentence; a mistyped `cary:` is a
/// plain unknown field and is ignored; `carry` nested under `session:` is
/// an unknown field whose hint names both the session block and the carry
/// list, the session sentence byte-for-byte as before.
#[test]
fn usage_probe_carry_list_rules_hold_from_a_cold_start() {
    const SESSION_HINT: &str = "Optional top-level block: `session` (keys: expose, route, chat).";
    const CARRY_HINT: &str =
        "Optional top-level list: `carry` (state keys remembered into the next run of a session).";

    for (label, block) in [
        ("declared key", "carry: [said]\n"),
        (
            "declared key with a session block",
            "carry: [said]\nsession:\n  expose: [said]\n  chat:\n    instructions: Answer.\n",
        ),
        ("empty list", "carry: []\n"),
        ("mistyped top-level name is ignored", "cary: [nope]\n"),
        (
            "a variable and a carried key may coexist under different names",
            "variables:\n  - name: tone\n    description: a knob\n    default: terse\ncarry: [said]\n",
        ),
    ] {
        let dir = fresh_config_dir("carry-ok");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);

        assert_echoes_hello(label, &run_echo(&dir));
    }

    let cases: &[(&str, &str, &[&str])] = &[
        (
            "undeclared carry key",
            "carry: [nope]\n",
            &[
                "carry key 'nope' is not declared anywhere (add it to initial_state, or have a node write it via state_updates / output_schema)",
            ],
        ),
        (
            "reserved initial_prompt",
            "carry: [initial_prompt]\n",
            &["carry key 'initial_prompt' is reserved"],
        ),
        (
            "reserved previous_turns",
            "carry: [previous_turns]\n",
            &["carry key 'previous_turns' is reserved"],
        ),
        (
            "duplicate carry key",
            "carry: [said, said]\n",
            &["carry key 'said' is listed more than once"],
        ),
        (
            "blank carry key",
            "carry: [\" \"]\n",
            &["carry key ' ' must be non-empty"],
        ),
        (
            "carry key that is also a variable",
            "variables:\n  - name: said\n    description: a knob\n    default: x\ncarry: [said]\n",
            &["carry key 'said' is also a variable; carried values would out-rank --variables"],
        ),
        (
            "carry is a scalar",
            "carry: said\n",
            &["carry: invalid type"],
        ),
        (
            "carry nested under session",
            "session:\n  carry: [said]\n  chat:\n    instructions: Answer.\n",
            &["unknown field `carry`", SESSION_HINT, CARRY_HINT],
        ),
    ];
    for (label, block, expected) in cases {
        let dir = fresh_config_dir("carry-err");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, block);

        let output = run_echo(&dir);
        assert_fails_at_graph_load(label, &output, expected);
        if *label == "carry nested under session" {
            // The added carry sentence rides on its own line after the
            // session sentence, so the pinned sentence stays byte-intact.
            let stderr = stderr_of(&output);
            let session_at = stderr.find(SESSION_HINT).unwrap();
            let carry_at = stderr.find(CARRY_HINT).unwrap();
            // (anyhow indents the cause chain, so only the line break counts.)
            let between = &stderr[session_at + SESSION_HINT.len()..carry_at];
            assert!(
                between.trim().is_empty() && between.matches('\n').count() == 1,
                "the carry sentence follows the session sentence on the next line:\n{stderr}"
            );
        }
    }
}

/// A `carry:` list with no `session:` block loads, but the agent is told at
/// engage that nothing is carried between REPL turns until a block exists:
/// one warning line on stderr. `-e hello` is shell-execute mode, so this
/// pins only the engage half; the run-time copy in the log is pinned by
/// `usage_probe_carry_without_a_session_block_is_warned_in_the_run_log_too`.
#[test]
fn usage_probe_carry_without_a_session_block_warns_once_at_engage() {
    const WARNING: &str = "Warning: carry: is declared but the graph has no session: block; nothing is carried between REPL turns until one exists (a child resumed by session_id still carries)";
    let dir = fresh_config_dir("carry-warn");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "carry: [said]\n");

    let output = run_echo(&dir);

    assert_echoes_hello("carry without a session block", &output);
    let stderr = stderr_of(&output);
    assert_eq!(
        stderr.lines().filter(|line| *line == WARNING).count(),
        1,
        "expected exactly one carry warning:\n{stderr}"
    );
    assert!(stderr.is_ascii(), "non-ASCII warning text:\n{stderr}");

    let dir = fresh_config_dir("carry-block");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(
        &dir,
        "carry: [said]\nsession:\n  chat:\n    instructions: Answer.\n",
    );
    let output = run_echo(&dir);
    assert_echoes_hello("carry with a session block", &output);
    assert!(
        !stderr_of(&output).contains("carry:"),
        "a graph with a session block carries, so no warning:\n{}",
        stderr_of(&output)
    );
}

/// A one-shot run under `--session` records the exchange and the run on the
/// session the way a REPL turn does, so the next turn on that session reads
/// the one-shot back through `previous_turns`. The graph is end-only, so no
/// model is called and the fixture can leave dry run (which records nothing
/// by contract); `save_session: true` saves the one-shot's session without a
/// prompt.
#[test]
fn usage_probe_a_one_shot_run_under_a_session_is_read_back_by_the_next_turn() {
    let dir = fresh_config_dir("one-shot");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");
    let config = dir.join("config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    assert!(
        text.contains("dry_run: true\n"),
        "the shared fixture is expected to opt into dry run:\n{text}"
    );
    fs::write(
        &config,
        text.replace("dry_run: true\n", "dry_run: false\nsave_session: true\n"),
    )
    .unwrap();
    let agent_dir = dir.join("agents").join(AGENT);
    fs::write(
        agent_dir.join("graph.yaml"),
        "name: echo-graph\n\
         version: \"1.0\"\n\
         initial_state: { said: \"\" }\n\
         start: done\n\
         nodes:\n\
         \x20 done: { type: end, output: \"prev=[{{previous_turns}}] now={{initial_prompt}}\" }\n\
         session:\n\
         \x20 chat:\n\
         \x20   instructions: Answer.\n",
    )
    .unwrap();

    let first = run_with(&dir, &["-a", AGENT, "--session", "s", "hello"]);
    let first_text = format!("{}\n{}", stderr_of(&first), stdout_of(&first));
    assert!(
        first.status.success() && !first_text.contains("panicked"),
        "one-shot: exit {:?}\n{first_text}",
        first.status
    );
    assert!(
        stdout_of(&first)
            .lines()
            .any(|line| line == "prev=[] now=hello"),
        "the first run sees no earlier turns:\n{first_text}"
    );
    let saved = agent_dir.join("sessions").join("s.yaml");
    let yaml = fs::read_to_string(&saved)
        .unwrap_or_else(|e| panic!("the session file was not written ({e}):\n{first_text}"));
    let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(
        doc["last_run"]["prompt"].as_str(),
        Some("hello"),
        "last_run.prompt is the one-shot prompt:\n{yaml}"
    );
    assert_eq!(
        doc["last_run"]["output"].as_str(),
        Some("prev=[] now=hello"),
        "last_run.output is the one-shot output:\n{yaml}"
    );

    fs::create_dir_all(dir.join("macros")).unwrap();
    fs::write(
        dir.join("macros/resume.yaml"),
        "isolated: false\nsteps:\n  - \".run again\"\n",
    )
    .unwrap();
    let second = run_with(&dir, &["-a", AGENT, "--session", "s", "--macro", "resume"]);
    let second_text = format!("{}\n{}", stderr_of(&second), stdout_of(&second));
    assert!(
        second.status.success() && !second_text.contains("panicked"),
        "resume: exit {:?}\n{second_text}",
        second.status
    );
    let stdout = stdout_of(&second);
    assert!(
        stdout.contains("prev=[user: hello\nassistant: prev=[] now=hello] now=again"),
        "the next turn reads the one-shot exchange back:\n{second_text}"
    );
}

/// The engage-time stderr line is only half of the carry warning: a graph
/// RUN re-validates the graph and sends the same sentence to the log file
/// (the executor's validation warnings go to the logger, not the terminal).
/// `-e hello` is shell-execute mode and never runs the graph, so this
/// drives a real run with a positional prompt; an operator reading
/// `<cache>/coyote.log` afterwards sees the engage copy and exactly one
/// run-time copy at WARN under the graph's own prefix, in the same words.
/// A graph with a session block logs nothing about `carry:`.
#[test]
fn usage_probe_carry_without_a_session_block_is_warned_in_the_run_log_too() {
    const SENTENCE: &str = "carry: is declared but the graph has no session: block; nothing is carried between REPL turns until one exists (a child resumed by session_id still carries)";
    assert!(BASE_GRAPH.starts_with(&format!("name: {GRAPH_NAME}\n")));
    let dir = fresh_config_dir("carry-log");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "carry: [said]\n");

    let output = run_with_env(
        &dir,
        &["-a", AGENT, "hello"],
        &[("COYOTE_LOG_LEVEL", "warn")],
    );
    let text = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
    assert!(
        output.status.success()
            && !text.contains("panicked")
            && stdout_of(&output).lines().any(|l| l == "hello"),
        "the graph run completes and the end node echoes the prompt: exit {:?}\n{text}",
        output.status
    );
    let stderr = stderr_of(&output);
    assert_eq!(
        stderr
            .lines()
            .filter(|l| *l == format!("Warning: {SENTENCE}"))
            .count(),
        1,
        "the engage line is printed once:\n{stderr}"
    );

    let log_path = dir.join("cache").join("coyote.log");
    let log = fs::read_to_string(&log_path).unwrap_or_else(|e| {
        panic!(
            "the run writes {} ({e}); stderr:\n{}",
            log_path.display(),
            stderr_of(&output)
        )
    });
    let run_time: Vec<&str> = log
        .lines()
        .filter(|l| l.contains(&format!("[graph:{GRAPH_NAME}] {SENTENCE}")))
        .collect();
    assert_eq!(
        run_time.len(),
        1,
        "one graph run logs the run-time carry warning exactly once:\n{log}"
    );
    assert!(
        run_time[0].contains("[WARN]") && run_time[0].is_ascii(),
        "the run-time copy is a WARN line in ASCII:\n{}",
        run_time[0]
    );
    assert!(
        log.lines()
            .any(|l| l.contains("[WARN]") && l.contains(&format!("Warning: {SENTENCE}"))),
        "the engage copy is logged at WARN as well:\n{log}"
    );

    let dir = fresh_config_dir("carry-log-block");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(
        &dir,
        "carry: [said]\nsession:\n  chat:\n    instructions: Answer.\n",
    );
    let output = run_with_env(
        &dir,
        &["-a", AGENT, "hello"],
        &[("COYOTE_LOG_LEVEL", "warn")],
    );
    assert!(
        output.status.success() && stdout_of(&output).lines().any(|l| l == "hello"),
        "carry with a session block runs: exit {:?}\n{}",
        output.status,
        stderr_of(&output)
    );
    let log = fs::read_to_string(dir.join("cache").join("coyote.log")).unwrap_or_default();
    assert!(
        !log.contains("carry:"),
        "a graph with a session block carries, so the run logs nothing about carry:\n{log}"
    );
}

/// `previous_turns` has a byte budget. Once a session holds more one-shot
/// exchanges than fit, the next turn sees the newest ones whole, oldest
/// first, behind one `[N earlier turn(s) omitted]` line whose count equals
/// the exchanges that fell off; no dropped prompt leaks into the text. The
/// one-shots run a graph whose end node prints a fixed word, so each
/// exchange has a known size; the graph is rewritten before the reading
/// turn (the session file is keyed by agent, not by graph content).
#[test]
fn usage_probe_a_session_past_the_turn_budget_drops_the_oldest_exchanges_whole() {
    let dir = fresh_config_dir("turn-budget");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");
    let config = dir.join("config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replace("dry_run: true\n", "dry_run: false\nsave_session: true\n"),
    )
    .unwrap();
    let agent_dir = dir.join("agents").join(AGENT);
    let graph = |output: &str| {
        format!(
            "name: echo-graph\n\
             version: \"1.0\"\n\
             initial_state: {{ said: \"\" }}\n\
             start: done\n\
             nodes:\n\
             \x20 done: {{ type: end, output: \"{output}\" }}\n\
             session:\n\
             \x20 chat:\n\
             \x20   instructions: Answer.\n"
        )
    };
    fs::write(agent_dir.join("graph.yaml"), graph("ok")).unwrap();

    // Four exchanges of roughly 8 KiB each: more than the 24 KiB budget, so
    // at least one must fall off and at least one must stay whole.
    let markers = ["one", "two", "three", "four"];
    let pad = "a".repeat(8 * 1024);
    for marker in markers {
        let prompt = format!("{marker} {pad}");
        let output = run_with(&dir, &["-a", AGENT, "--session", "s", &prompt]);
        assert!(
            output.status.success() && stdout_of(&output).lines().any(|l| l == "ok"),
            "one-shot {marker}: exit {:?}\n{}",
            output.status,
            stderr_of(&output)
        );
    }

    fs::write(
        agent_dir.join("graph.yaml"),
        graph("prev=[{{previous_turns}}] now={{initial_prompt}}"),
    )
    .unwrap();
    fs::create_dir_all(dir.join("macros")).unwrap();
    fs::write(
        dir.join("macros/resume.yaml"),
        "isolated: false\nsteps:\n  - \".run tail\"\n",
    )
    .unwrap();
    let output = run_with(&dir, &["-a", AGENT, "--session", "s", "--macro", "resume"]);
    let text = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
    assert!(
        output.status.success() && !text.contains("panicked"),
        "reading turn: exit {:?}\n{text}",
        output.status
    );
    let stdout = stdout_of(&output);
    let start = stdout
        .find("prev=[")
        .unwrap_or_else(|| panic!("the reading turn rendered previous_turns:\n{text}"));
    let end = stdout[start..]
        .find("] now=tail")
        .map(|i| start + i)
        .unwrap_or_else(|| panic!("the rendered turns close before `now=tail`:\n{text}"));
    let rendered = &stdout[start + "prev=[".len()..end];

    let omitted_line = rendered
        .lines()
        .next()
        .unwrap_or_else(|| panic!("previous_turns is not empty:\n{text}"));
    let dropped: usize = omitted_line
        .strip_prefix('[')
        .and_then(|l| l.strip_suffix(" earlier turn(s) omitted]"))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("the first line counts the dropped turns, got {omitted_line:?}"));
    assert!(
        dropped >= 1 && dropped < markers.len(),
        "some but not all exchanges fell off the budget (dropped {dropped})"
    );
    let kept: Vec<usize> = markers
        .iter()
        .filter_map(|m| rendered.find(&format!("user: {m} ")))
        .collect();
    assert_eq!(
        kept.len(),
        markers.len() - dropped,
        "the kept exchanges plus the omitted count add up to every one-shot:\n{rendered}"
    );
    assert!(
        kept.windows(2).all(|w| w[0] < w[1]),
        "kept exchanges render oldest first:\n{rendered}"
    );
    for dropped_marker in &markers[..dropped] {
        assert!(
            !rendered.contains(&format!("user: {dropped_marker} ")),
            "the oldest exchange `{dropped_marker}` is dropped whole, not cut:\n{rendered}"
        );
    }
    assert_eq!(
        rendered.matches("assistant: ok").count(),
        markers.len() - dropped,
        "every kept exchange carries its whole reply:\n{rendered}"
    );
}

/// A state too large for the environment is handed to a script node as a
/// temp file named by `GRAPH_STATE_FILE`. From the script's point of view
/// that file is owner-only (0600): nothing else on the host reads the graph
/// state while the node runs, and the file is gone once the run ends. The
/// script reports the mode and the path back through its JSON stdout, the
/// end node echoes the mode, and the test checks the path after exit. A
/// positional prompt drives the graph (`-e` is shell-execute mode).
#[cfg(unix)]
#[test]
fn usage_probe_the_graph_state_spill_file_is_owner_only_and_removed_after_the_run() {
    let dir = fresh_config_dir("spill-mode");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");
    let agent_dir = dir.join("agents").join(AGENT);
    let scripts = agent_dir.join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    // `stat -c` is GNU, `stat -f` is BSD/macOS; whichever answers wins.
    fs::write(
        scripts.join("probe.sh"),
        "#!/usr/bin/env bash\n\
         if [ -z \"${GRAPH_STATE_FILE:-}\" ]; then\n\
         \x20 printf '{\"mode\":\"inline\",\"spill\":\"\"}'\n\
         \x20 exit 0\n\
         fi\n\
         m=$(stat -c %a \"$GRAPH_STATE_FILE\" 2>/dev/null || stat -f %Lp \"$GRAPH_STATE_FILE\")\n\
         printf '{\"mode\":\"%s\",\"spill\":\"%s\"}' \"$m\" \"$GRAPH_STATE_FILE\"\n",
    )
    .unwrap();
    // 48 KiB of payload: past the 32 KiB inline ceiling by a margin that
    // does not depend on the JSON framing around it.
    let blob = "x".repeat(48 * 1024);
    fs::write(
        agent_dir.join("graph.yaml"),
        format!(
            "name: echo-graph\n\
             version: \"1.0\"\n\
             initial_state: {{ said: \"\", mode: \"\", spill: \"\", blob: \"{blob}\" }}\n\
             start: probe\n\
             nodes:\n\
             \x20 probe: {{ type: script, script: scripts/probe.sh, next: done }}\n\
             \x20 done: {{ type: end, output: \"mode={{{{mode}}}} spill={{{{spill}}}}\" }}\n"
        ),
    )
    .unwrap();

    let output = run_with(&dir, &["-a", AGENT, "hello"]);
    let text = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
    assert!(
        output.status.success() && !text.contains("panicked"),
        "spill run: exit {:?}\n{text}",
        output.status
    );
    let line = stdout_of(&output)
        .lines()
        .rev()
        .find(|l| l.starts_with("mode="))
        .unwrap_or_else(|| panic!("the end node reports the probe's findings:\n{text}"))
        .to_string();
    let spill = line
        .split_once(" spill=")
        .map(|(_, p)| p.trim().to_string())
        .unwrap_or_default();
    assert!(
        line.starts_with("mode=600 "),
        "the spill file is owner-only while the script runs (got {line:?}):\n{text}"
    );
    assert!(
        !spill.is_empty() && spill.ends_with(".json"),
        "the script saw a real spill path (got {spill:?}):\n{text}"
    );
    assert!(
        !Path::new(&spill).exists(),
        "the spill file {spill} is removed once the run ends"
    );
}

/// `graph.example.yaml` enables the `ddg-search` MCP server, which the
/// bundled `mcp.json` launches through `uvx`. That launcher is not on every
/// host (the CI runners lack it), and a cold start of the example tries to
/// spawn it before `--info` prints anything. The carry test is about the
/// `carry:` block, so the copy it boots drops every MCP reference: the
/// graph-level `mcp_servers:` and `mcp_tools:` blocks, the node-level
/// `mcp_tools:` block, and each `- mcp:<server>` tool entry. A block is its
/// key line plus every following non-blank line indented deeper than it.
fn without_mcp_references(yaml: &str) -> String {
    let mut kept = String::new();
    let mut block_indent: Option<usize> = None;
    for line in yaml.lines() {
        let body = line.trim_start();
        let indent = line.len() - body.len();
        if let Some(opened_at) = block_indent {
            if !body.is_empty() && indent > opened_at {
                continue;
            }
            block_indent = None;
        }
        if body.starts_with("mcp_servers:") || body.starts_with("mcp_tools:") {
            block_indent = Some(indent);
            continue;
        }
        if body.starts_with("- mcp:") {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept
}

/// Fold CRLF to LF so a checkout with either line ending reads the same
/// example text. `.gitattributes` pins no `eol` for `graph.example.yaml`,
/// so a Windows checkout hands the test CRLF bytes and every LF needle
/// below would miss without this.
fn lf_text(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// The commented `carry:` block as the shipped example spells it, and the
/// two uncommented lines a consumer ends up with.
const COMMENTED_CARRY: &str = "\n# carry:\n#   - final_report\n";
const UNCOMMENTED_CARRY: &str = "\ncarry:\n  - final_report\n";

/// Read the shipped `graph.example.yaml` as LF text.
fn example_graph_text() -> String {
    lf_text(
        &fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("graph.example.yaml"))
            .unwrap(),
    )
}

/// The copy of the example the carry test boots: the commented `carry:`
/// block uncommented, the example's own model knobs swapped for the dry-run
/// model, and its MCP wiring dropped, so the only thing under test is the
/// uncommented list and no MCP launcher has to exist on the host. `None`
/// when the example no longer carries the commented block. Folds CRLF first
/// so a CRLF copy of the example takes the same path as the LF copy.
fn example_carry_fixture(example: &str) -> Option<String> {
    let example = lf_text(example);
    if !example.contains(COMMENTED_CARRY) {
        return None;
    }
    Some(
        without_mcp_references(&example.replace(COMMENTED_CARRY, UNCOMMENTED_CARRY))
            .replace("claude:claude-sonnet-4-6", "dryrun:dry-model")
            .replace("claude:claude-haiku-4-5", "dryrun:dry-model"),
    )
}

/// A CRLF copy of the example misses every LF needle the carry test uses
/// until it is folded; folded, the LF and CRLF copies derive byte-identical
/// fixtures. This is the Windows-checkout shape the real binary test sees.
#[test]
fn usage_probe_example_carry_fixture_is_line_ending_independent() {
    let lf = example_graph_text();
    assert!(
        !lf.contains('\r'),
        "LF fixture must carry no carriage returns"
    );
    let crlf = lf.replace('\n', "\r\n");
    assert!(
        crlf.contains(&COMMENTED_CARRY.replace('\n', "\r\n")),
        "CRLF fixture must present the carry block with CRLF bytes"
    );
    assert!(
        !crlf.contains(COMMENTED_CARRY),
        "the unfolded CRLF copy must miss the LF needle, or this pin proves nothing"
    );

    assert_eq!(lf_text(&crlf), lf);
    assert_eq!(lf_text(&lf), lf);

    let from_lf = example_carry_fixture(&lf).expect("LF example carries the commented block");
    let from_crlf = example_carry_fixture(&crlf).expect("CRLF example carries the commented block");
    assert_eq!(from_crlf, from_lf);
    assert!(from_crlf.contains(UNCOMMENTED_CARRY));
    assert!(!from_crlf.contains('\r'));
}

/// The shipped `graph.example.yaml` documents `carry:` as a commented block
/// ending in a ready-to-use `carry: [final_report]`. A consumer who
/// uncomments exactly those two lines gets a graph the real binary still
/// loads from a cold start (`final_report` is written by a node, is not a
/// variable, and is not reserved), inspected end to end with `--info`. The
/// same block naming an undeclared key is refused at graph load with the
/// carry sentence, so the positive leg really did parse the list.
#[test]
fn usage_probe_uncommenting_the_example_carry_block_loads_from_a_cold_start() {
    let example = example_graph_text();
    let uncommented = example_carry_fixture(&example)
        .expect("graph.example.yaml no longer carries the commented `carry:` block");
    assert!(
        !uncommented.to_ascii_lowercase().contains("mcp") && !uncommented.contains("ddg-search"),
        "the booted copy still references an MCP server:\n{uncommented}"
    );
    assert!(
        uncommented.contains(UNCOMMENTED_CARRY),
        "the MCP strip must leave the uncommented carry block in place"
    );

    for (label, graph, expect_ok) in [
        ("as documented", uncommented.clone(), true),
        (
            "undeclared key in the same block",
            uncommented.replace(UNCOMMENTED_CARRY, "\ncarry:\n  - nope\n"),
            false,
        ),
    ] {
        let dir = fresh_config_dir("carry-example");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, "");
        let agent_dir = dir.join("agents").join(AGENT);
        fs::write(agent_dir.join("graph.yaml"), &graph).unwrap();
        // The example's synthesize script only has to exist for the parser.
        fs::create_dir_all(agent_dir.join("scripts")).unwrap();
        fs::write(agent_dir.join("scripts").join("synthesize.py"), "").unwrap();

        let output = run_with(&dir, &["-a", AGENT, "--info"]);
        let combined = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
        if expect_ok {
            assert!(
                output.status.success() && !combined.contains("panicked"),
                "{label}: expected `--info` to inspect the example, got exit {:?}\n{combined}",
                output.status
            );
            assert!(
                !combined.contains("Failed to load graph.yaml"),
                "{label}: the uncommented carry block failed to load:\n{combined}"
            );
            assert!(
                stdout_of(&output).contains("config_file:"),
                "{label}: `--info` did not reach the agent summary:\n{combined}"
            );
        } else {
            assert_fails_at_graph_load(
                label,
                &output,
                &["carry key 'nope' is not declared anywhere"],
            );
        }
    }
}

/// Graph-Agents.md says `previous_turns` is reserved like `initial_prompt`:
/// declaring it in `initial_state` or as a `variables[]` name fails at graph
/// load with the quoted text, from a cold start, before any model or script
/// runs. The same graph with the declaration dropped loads and runs.
#[test]
fn usage_probe_a_reserved_previous_turns_declaration_fails_at_graph_load() {
    const GRAPH_TAIL: &str = "start: done\n\
        nodes:\n\
        \x20 done: { type: end, output: \"{{initial_prompt}}\" }\n";
    let cases: &[(&str, &str, &str)] = &[
        (
            "previous_turns in initial_state",
            "initial_state: { said: \"\", previous_turns: x }\n",
            "initial_state key 'previous_turns' is reserved (the dispatcher seeds it)",
        ),
        (
            "previous_turns as a variable name",
            "initial_state: { said: \"\" }\n\
             variables:\n\
             \x20 - name: previous_turns\n\
             \x20   description: the earlier turns\n\
             \x20   default: none\n",
            "variables[] name 'previous_turns' is reserved",
        ),
    ];
    for (label, head, expected) in cases {
        let dir = fresh_config_dir("reserved-turns");
        let _guard = TempDirGuard(dir.clone());
        write_fixture(&dir, "");
        let graph = dir.join("agents").join(AGENT).join("graph.yaml");
        fs::write(
            &graph,
            format!(
                "name: echo-graph\nversion: \"1.0\"\ndescription: Echoes the prompt.\n{head}{GRAPH_TAIL}"
            ),
        )
        .unwrap();
        let output = run_echo(&dir);
        assert_fails_at_graph_load(label, &output, &[expected]);
        // The run never reached the graph: the dry-run echo is absent.
        assert!(
            !stdout_of(&output).lines().any(|line| line == "hello"),
            "{label}: the graph ran despite the reserved key:\n{}",
            stdout_of(&output)
        );

        // Control: the same graph without the reserved declaration runs.
        let clean = head
            .replace(", previous_turns: x", "")
            .replace("previous_turns", "earlier_turns");
        fs::write(
            &graph,
            format!(
                "name: echo-graph\nversion: \"1.0\"\ndescription: Echoes the prompt.\n{clean}{GRAPH_TAIL}"
            ),
        )
        .unwrap();
        assert_echoes_hello(&format!("{label} (control)"), &run_echo(&dir));
    }
}

/// A loopback chat-completions server that answers every request with one
/// fixed assistant message and keeps every request body it saw. It stands
/// in for the model so a plain prompt can cross the route classifier in the
/// real binary without a live provider; the end-only graph never asks for a
/// second completion. One connection per request (`Connection: close`); a
/// request asking for `stream: true` is answered as an event stream.
struct StubModel {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

impl StubModel {
    fn answering(content: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&requests);
        let whole = serde_json::json!({
            "id": "stub",
            "object": "chat.completion",
            "created": 0,
            "model": "stub-model",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
        })
        .to_string();
        let delta = serde_json::json!({
            "id": "stub",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "stub-model",
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant", "content": content },
                "finish_reason": "stop"
            }]
        })
        .to_string();
        let streamed = format!("data: {delta}\n\ndata: [DONE]\n\n");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut raw = Vec::new();
                let mut chunk = [0u8; 4096];
                let (header_end, content_length) = loop {
                    let Ok(n) = stream.read(&mut chunk) else {
                        break (None, 0);
                    };
                    if n == 0 {
                        break (None, 0);
                    }
                    raw.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&raw[..pos]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (Some(pos + 4), length);
                    }
                };
                let Some(header_end) = header_end else {
                    continue;
                };
                while raw.len() < header_end + content_length {
                    let Ok(n) = stream.read(&mut chunk) else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&chunk[..n]);
                }
                let body = String::from_utf8_lossy(&raw[header_end..]).into_owned();
                seen.lock().unwrap().push(body);
                let compact: String = raw[header_end..]
                    .iter()
                    .filter(|b| !b.is_ascii_whitespace())
                    .map(|&b| b as char)
                    .collect();
                let (content_type, reply) = if compact.contains("\"stream\":true") {
                    ("text/event-stream", &streamed)
                } else {
                    ("application/json", &whole)
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self { port, requests }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// A live (non-dry-run) fixture whose only model is the stub: a graph agent
/// with a session block whose end node prints `ran=<prompt>`, `save_session`
/// on so the exit saves `s` without a prompt, and a `helper` role for the
/// `.role <name> <text>` form.
fn write_stub_model_fixture(dir: &Path, stub: &StubModel) {
    write_fixture(dir, "session:\n  chat:\n    instructions: Answer.\n");
    let vault_pass = dir.join("vault-pass");
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: stub:stub-model\n\
             dry_run: false\n\
             save_session: true\n\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai\n\
             \x20   name: stub\n\
             \x20   auth: none\n\
             \x20   api_key: 'unused'\n\
             \x20   api_base: http://127.0.0.1:{port}/v1\n\
             \x20   models:\n\
             \x20     - name: stub-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             save: false\n\
             memory: false\n\
             skills_enabled: true\n\
             stream: false\n",
            vault_pass = vault_pass.display(),
            port = stub.port,
        ),
    )
    .unwrap();
    let agent_dir = dir.join("agents").join(AGENT);
    fs::write(
        agent_dir.join("graph.yaml"),
        "name: echo-graph\n\
         version: \"1.0\"\n\
         initial_state: { said: \"\" }\n\
         start: done\n\
         nodes:\n\
         \x20 done: { type: end, output: \"ran={{initial_prompt}}\" }\n\
         session:\n\
         \x20 chat:\n\
         \x20   instructions: Answer.\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("roles")).unwrap();
    fs::write(dir.join("roles").join("helper.md"), "You are terse.\n").unwrap();
}

/// One REPL turn as a macro, then `.save session` so the headless macro
/// arm (which saves nothing on its own) leaves `s.yaml` on disk for the
/// test to read; a turn that recorded nothing still writes the file.
fn run_macro_against_stub(dir: &Path, step: &str) -> Output {
    fs::create_dir_all(dir.join("macros")).unwrap();
    fs::write(
        dir.join("macros/turn.yaml"),
        format!("isolated: false\nsteps:\n  - {step}\n  - \".save session\"\n"),
    )
    .unwrap();
    // Loopback must not be sent through a host proxy.
    run_with_env(
        dir,
        &["-a", AGENT, "--session", "s", "--macro", "turn"],
        &[
            ("NO_PROXY", "127.0.0.1,localhost"),
            ("no_proxy", "127.0.0.1,localhost"),
        ],
    )
}

/// A plain prompt on a graph agent with a session block crosses the route
/// classifier (one model call, answered here by the stub with `run`), the
/// graph runs, and the turn is recorded: `last_run` names the prompt and
/// the output, and the exchange sits in `messages`. The `.role <name>
/// <text>` form of the same prompt is detached from the session: it crosses
/// the same classifier and runs the same graph, but records NOTHING - no
/// `last_run`, no exchange - so the next turn on `s` has no earlier turn to
/// read. Both halves run the real binary from a cold config dir.
#[test]
fn usage_probe_a_role_prefixed_prompt_runs_the_graph_unrecorded_while_a_plain_one_is_recorded() {
    let stub = StubModel::answering("{\"decision\":\"run\"}");

    // Plain prompt: attached, classified, run, recorded.
    let dir = fresh_config_dir("routed-plain");
    let _guard = TempDirGuard(dir.clone());
    write_stub_model_fixture(&dir, &stub);
    let plain = run_macro_against_stub(&dir, "\"hi there\"");
    let plain_text = format!("{}\n{}", stderr_of(&plain), stdout_of(&plain));
    assert!(
        plain.status.success() && !plain_text.contains("panicked"),
        "plain prompt: exit {:?}\n{plain_text}",
        plain.status
    );
    assert!(
        stdout_of(&plain).lines().any(|l| l == "ran=hi there"),
        "the classifier routed the plain prompt to a run:\n{plain_text}"
    );
    let classifier_calls = stub.requests();
    assert_eq!(
        classifier_calls.len(),
        1,
        "one plain prompt costs exactly one model call (the classifier):\n{classifier_calls:#?}"
    );
    assert!(
        classifier_calls[0].contains("New message: hi there")
            && classifier_calls[0].contains("has_last_run: false"),
        "the classifier saw the typed text and an empty session:\n{}",
        classifier_calls[0]
    );
    let saved = dir
        .join("agents")
        .join(AGENT)
        .join("sessions")
        .join("s.yaml");
    let yaml = fs::read_to_string(&saved)
        .unwrap_or_else(|e| panic!("the session file was not written ({e}):\n{plain_text}"));
    let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(
        doc["last_run"]["prompt"].as_str(),
        Some("hi there"),
        "last_run.prompt is the bare typed prompt:\n{yaml}"
    );
    assert_eq!(
        doc["last_run"]["output"].as_str(),
        Some("ran=hi there"),
        "last_run.output is the graph output:\n{yaml}"
    );
    let roles: Vec<&str> = doc["messages"]
        .as_sequence()
        .map(|m| m.iter().filter_map(|m| m["role"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        roles,
        vec!["system", "user", "assistant"],
        "the routed run stores the block's prompt row and one exchange:\n{yaml}"
    );

    // `.role helper <text>`: detached, classified, run, NOT recorded.
    let dir = fresh_config_dir("routed-role");
    let _guard = TempDirGuard(dir.clone());
    write_stub_model_fixture(&dir, &stub);
    let detached = run_macro_against_stub(&dir, "\".role helper hi there\"");
    let detached_text = format!("{}\n{}", stderr_of(&detached), stdout_of(&detached));
    assert!(
        detached.status.success() && !detached_text.contains("panicked"),
        "role-prefixed prompt: exit {:?}\n{detached_text}",
        detached.status
    );
    assert!(
        stdout_of(&detached).lines().any(|l| l == "ran=hi there"),
        "the role-prefixed prompt still runs the graph:\n{detached_text}"
    );
    let calls = stub.requests();
    assert_eq!(
        calls.len(),
        2,
        "the detached prompt crossed the classifier too (one more model call):\n{calls:#?}"
    );
    assert!(
        calls[1].contains("New message: hi there"),
        "the classifier saw the role-prefixed text:\n{}",
        calls[1]
    );
    let saved = dir
        .join("agents")
        .join(AGENT)
        .join("sessions")
        .join("s.yaml");
    let yaml = fs::read_to_string(&saved)
        .unwrap_or_else(|e| panic!("`.save session` wrote no file ({e}):\n{detached_text}"));
    let doc: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
    assert!(
        doc.get("last_run").is_none_or(|run| run.is_null()),
        "a detached run records no last_run:\n{yaml}"
    );
    assert!(
        !yaml.contains("hi there"),
        "a detached run stores no exchange:\n{yaml}"
    );

    // The next turn on `s` has nothing to read back.
    let graph = dir.join("agents").join(AGENT).join("graph.yaml");
    let text = fs::read_to_string(&graph).unwrap();
    fs::write(
        &graph,
        text.replace(
            "output: \"ran={{initial_prompt}}\"",
            "output: \"prev=[{{previous_turns}}] now={{initial_prompt}}\"",
        ),
    )
    .unwrap();
    let next = run_macro_against_stub(&dir, "\".run later\"");
    let next_text = format!("{}\n{}", stderr_of(&next), stdout_of(&next));
    assert!(
        next.status.success() && !next_text.contains("panicked"),
        "next turn: exit {:?}\n{next_text}",
        next.status
    );
    assert!(
        stdout_of(&next).lines().any(|l| l == "prev=[] now=later"),
        "nothing from the detached turn reaches previous_turns:\n{next_text}"
    );
    assert_eq!(
        stub.requests().len(),
        2,
        "`.run` is forced and never asks the classifier"
    );
}

/// Every run of the real binary here gets a PATH of exactly the built
/// binary's own directory followed by the system shell directories, so a
/// script node (which the engine runs through the `bash` on PATH) sees that
/// value and nothing from the developer's PATH. The only other entries are
/// the ones the engine itself prepends from inside the fixture's config dir
/// (its tool shim directories). The script reports the PATH it ran with
/// through its JSON stdout and the end node echoes it; the expectation is
/// spelled from the documented shape, not read back from the fixture helper.
#[cfg(unix)]
#[test]
fn usage_probe_a_script_node_runs_with_only_the_binary_dir_and_the_system_shells_on_path() {
    let dir = fresh_config_dir("pinned-path");
    let _guard = TempDirGuard(dir.clone());
    write_fixture(&dir, "");
    let agent_dir = dir.join("agents").join(AGENT);
    let scripts = agent_dir.join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    fs::write(
        scripts.join("path.sh"),
        "#!/usr/bin/env bash\n\
         printf '{\"seen\":\"%s\"}' \"$PATH\"\n",
    )
    .unwrap();
    fs::write(
        agent_dir.join("graph.yaml"),
        "name: echo-graph\n\
         version: \"1.0\"\n\
         initial_state: { said: \"\", seen: \"\" }\n\
         start: probe\n\
         nodes:\n\
         \x20 probe: { type: script, script: scripts/path.sh, next: done }\n\
         \x20 done: { type: end, output: \"path=<{{seen}}>\" }\n",
    )
    .unwrap();

    let output = run_with(&dir, &["-a", AGENT, "hello"]);
    let text = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
    assert!(
        output.status.success() && !text.contains("panicked"),
        "path run: exit {:?}\n{text}",
        output.status
    );
    let bin_dir = Path::new(COYOTE_BIN).parent().unwrap();
    let line = stdout_of(&output)
        .lines()
        .rev()
        .find(|l| l.starts_with("path=<"))
        .unwrap_or_else(|| panic!("the end node reports the script's PATH:\n{text}"))
        .to_string();
    let seen = line
        .strip_prefix("path=<")
        .and_then(|rest| rest.strip_suffix('>'))
        .unwrap_or_else(|| panic!("unexpected report shape {line:?}"));
    let from_outside_the_fixture: Vec<&str> = seen
        .split(':')
        .filter(|entry| !Path::new(entry).starts_with(&dir))
        .collect();
    assert_eq!(
        from_outside_the_fixture,
        [bin_dir.to_str().unwrap(), "/usr/bin", "/bin"],
        "beyond the engine's own shim dirs under the fixture, the child PATH is the binary dir plus /usr/bin:/bin:\n{text}"
    );
    assert!(
        seen.ends_with("/usr/bin:/bin"),
        "the system shell directories come last and in order:\n{line}"
    );
}

/// `TEST_WORKSPACE_MEMORY_ROOT_ENV` is a copy of the source const, so a
/// rename there would leave the probe below setting (and the harness
/// scrubbing) a variable the binary never reads; this keeps the two equal.
#[test]
fn the_test_memory_root_env_name_matches_the_source_const() {
    let source_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config/memory.rs");
    let source = fs::read_to_string(&source_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", source_path.display()));
    let needle =
        format!("WORKSPACE_MEMORY_ROOT_TEST_ENV: &str = \"{TEST_WORKSPACE_MEMORY_ROOT_ENV}\"");
    assert!(
        source.contains(&needle),
        "{} no longer declares {needle}",
        source_path.display()
    );
}

/// The shipped binary discovers workspace memory by walking up from the
/// directory it runs in, and the `TEST_WORKSPACE_MEMORY_ROOT_ENV` pin
/// the unit-test guards set is a test-build seam: a real run that carries
/// it still reads the index above its cwd. The dry-run model echoes the
/// whole system prompt, so the workspace index lines under `<facts>` are
/// observable on stdout.
#[test]
fn usage_probe_the_shipped_binary_walks_up_from_its_cwd_for_workspace_memory_and_ignores_the_test_root_pin()
 {
    const GLOBAL_LINE: &str = "- global fact from the fixture";
    const WORKSPACE_HEADER: &str = "# Workspace Memory Index";
    const WORKSPACE_LINE: &str = "- workspace fact planted by the probe";
    let dir = fresh_config_dir("workspace-memory");
    let _guard = TempDirGuard(dir.clone());
    write_fixture_on(
        &dir,
        "",
        Host {
            memory: true,
            ..RECIPE_HOST
        },
    );
    fs::create_dir_all(dir.join("memory")).unwrap();
    fs::write(
        dir.join("memory").join("MEMORY.md"),
        format!("{GLOBAL_LINE}\n"),
    )
    .unwrap();
    // The index sits one level above the cwd so the walk has to climb.
    let workspace = dir.join("ws");
    let nested = workspace.join("nested");
    let index_dir = workspace.join(".coyote").join("memory");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&index_dir).unwrap();
    fs::write(
        index_dir.join("MEMORY.md"),
        format!("{WORKSPACE_HEADER}\n{WORKSPACE_LINE}\n"),
    )
    .unwrap();
    // A sibling with no index anywhere above it short of the temp root.
    let elsewhere = dir.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();

    let memory_block = |output: &Output, label: &str| -> String {
        let text = format!("{}\n{}", stderr_of(output), stdout_of(output));
        assert!(
            output.status.success() && !text.contains("panicked"),
            "{label}: exit {:?}\n{text}",
            output.status
        );
        let stdout = stdout_of(output);
        let start = stdout.find("<memory>").unwrap_or_else(|| {
            panic!("{label}: the dry-run echo carries the memory section:\n{text}")
        });
        let end = stdout[start..]
            .find("</memory>")
            .unwrap_or_else(|| panic!("{label}: the memory section closes:\n{text}"));
        stdout[start..start + end].to_string()
    };

    // 1. From under the workspace: both indexes ride the prompt.
    let from_workspace = memory_block(
        &run_from(&nested, &dir, &["-e", "hello"], &[]),
        "walk from the nested cwd",
    );
    assert!(from_workspace.contains("<facts>"), "{from_workspace}");
    assert!(from_workspace.contains(GLOBAL_LINE), "{from_workspace}");
    assert!(
        from_workspace.contains(WORKSPACE_HEADER),
        "{from_workspace}"
    );
    assert!(from_workspace.contains(WORKSPACE_LINE), "{from_workspace}");

    // 2. Same cwd with the test pin aimed at an index-less dir: the shipped
    //    binary renders the very same block, so the pin changed nothing.
    let with_pin = memory_block(
        &run_from(
            &nested,
            &dir,
            &["-e", "hello"],
            &[(TEST_WORKSPACE_MEMORY_ROOT_ENV, elsewhere.to_str().unwrap())],
        ),
        "the test pin on a real run",
    );
    assert_eq!(
        with_pin, from_workspace,
        "the pin is inert outside test builds"
    );

    // 3. From the index-less sibling: the global index alone.
    let from_elsewhere = memory_block(
        &run_from(&elsewhere, &dir, &["-e", "hello"], &[]),
        "walk from a cwd with no index above it",
    );
    assert!(from_elsewhere.contains(GLOBAL_LINE), "{from_elsewhere}");
    assert!(
        !from_elsewhere.contains(WORKSPACE_HEADER),
        "{from_elsewhere}"
    );
    assert!(!from_elsewhere.contains(WORKSPACE_LINE), "{from_elsewhere}");
}

/// The converse of the probe above: a pin aimed AT a workspace that holds
/// an index cannot pull that index into a real run whose cwd has none.
#[test]
fn usage_probe_the_shipped_binary_does_not_load_workspace_memory_from_a_pinned_root() {
    const GLOBAL_LINE: &str = "- global fact for the converse probe";
    const WORKSPACE_LINE: &str = "- workspace fact only the pin points at";
    let dir = fresh_config_dir("workspace-memory-pin");
    let _guard = TempDirGuard(dir.clone());
    write_fixture_on(
        &dir,
        "",
        Host {
            memory: true,
            ..RECIPE_HOST
        },
    );
    fs::create_dir_all(dir.join("memory")).unwrap();
    fs::write(
        dir.join("memory").join("MEMORY.md"),
        format!("{GLOBAL_LINE}\n"),
    )
    .unwrap();
    let pinned = dir.join("pinned");
    let index_dir = pinned.join(".coyote").join("memory");
    fs::create_dir_all(&index_dir).unwrap();
    fs::write(
        index_dir.join("MEMORY.md"),
        format!("# Workspace Memory Index\n{WORKSPACE_LINE}\n"),
    )
    .unwrap();
    let cwd = dir.join("plain");
    fs::create_dir_all(&cwd).unwrap();

    let output = run_from(
        &cwd,
        &dir,
        &["-e", "hello"],
        &[(TEST_WORKSPACE_MEMORY_ROOT_ENV, pinned.to_str().unwrap())],
    );
    let text = format!("{}\n{}", stderr_of(&output), stdout_of(&output));
    assert!(
        output.status.success() && !text.contains("panicked"),
        "exit {:?}\n{text}",
        output.status
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains(GLOBAL_LINE),
        "the global index still rides:\n{stdout}"
    );
    assert!(
        !stdout.contains(WORKSPACE_LINE),
        "the pinned root's index leaked into a real run:\n{stdout}"
    );
    assert!(
        !stdout.contains("# Workspace Memory Index"),
        "no workspace index header without one above the cwd:\n{stdout}"
    );
}
