//! Real-binary checks of what a `--headless` run promises a consumer: a
//! user-interaction tool answers with the `needs_human` JSON, a tool's
//! question through the prompt channel is answered `null` (directly and
//! from a background job), a malformed request line is passed on as output,
//! and the shell hooks of tool, job, turn and LLM events get the variables
//! the hooks page lists. Every case goes through bash tools and `sh -c`
//! hooks, so the file is unix-only.
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process, thread};

use common::fake_llm::{FakeLlm, Reply};
use serde_json::Value;

const RUN_DEADLINE: Duration = Duration::from_secs(120);
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);
/// Hooks are fire-and-forget, so their snapshots can land after the run
/// exits; a loaded host can take a while to get them all written.
const HOOK_DEADLINE: Duration = Duration::from_secs(30);

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fresh_config_dir(scenario: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-headless-{scenario}-{}-{unique}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("hook-out")).unwrap();
    fs::create_dir_all(dir.join("functions").join("tools")).unwrap();
    dir.canonicalize().unwrap()
}

/// Writes a config whose only model is served by `fake`, with
/// `config_extra` spliced in verbatim.
fn write_config(dir: &Path, fake: &FakeLlm, config_extra: &str) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: fake:fake-model\n\
             function_calling_support: true\n\
             stream: false\n\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai-compatible\n\
             \x20   name: fake\n\
             \x20   api_base: '{api_base}'\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: fake-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             save: false\n\
             workspace_instructions: false\n\
             memory: false\n\
             {config_extra}",
            vault_pass = vault_pass.display(),
            api_base = fake.api_base(),
        ),
    )
    .unwrap();
}

/// A `probe` agent that may call the global tools in `tools`; global hooks
/// reach an agent only through its whitelist, so it lets every one through.
fn write_agent(dir: &Path, tools: &[&str]) {
    let agent = dir.join("agents").join("probe");
    fs::create_dir_all(&agent).unwrap();
    let global_tools: String = tools.iter().map(|t| format!("  - {t}\n")).collect();
    fs::write(
        agent.join("config.yaml"),
        format!(
            "name: probe\n\
             description: a probe agent\n\
             version: 0.1.0\n\
             instructions: |\n\
             \x20 Do what the user asks.\n\
             global_hooks:\n\
             \x20 - job.started.*\n\
             \x20 - job.completed.*\n\
             \x20 - agent.started.*\n\
             \x20 - agent.completed.*\n\
             global_tools:\n{global_tools}"
        ),
    )
    .unwrap();
}

fn write_tool(dir: &Path, name: &str, describe: &str, main_body: &str) {
    fs::write(
        dir.join("functions/tools").join(format!("{name}.sh")),
        format!(
            "#!/usr/bin/env bash\n\
             set -e\n\
             \n\
             # @describe {describe}\n\
             \n\
             # @env LLM_OUTPUT=/dev/stdout The output path\n\
             \n\
             main() {{\n{main_body}\n}}\n"
        ),
    )
    .unwrap();
}

/// A tool that asks `request` (one raw protocol line) and records, in
/// `<dir>/<name>.out`, what the host told it: `COYOTE_UI`, whether a reply
/// channel was set, and the reply line (or `no reply` after two seconds).
fn write_asking_tool(dir: &Path, name: &str, request: &str) {
    let out = dir.join(format!("{name}.out"));
    write_tool(
        dir,
        name,
        "Ask one question through the prompt channel.",
        &format!(
            "    local out='{out}'\n\
             \x20   echo \"ui=${{COYOTE_UI:-unset}}\" > \"$out\"\n\
             \x20   if [ -z \"${{COYOTE_PROMPT_REPLY:-}}\" ]; then echo 'channel=unset' >> \"$out\"; return 0; fi\n\
             \x20   echo 'channel=set' >> \"$out\"\n\
             \x20   exec 9<\"$COYOTE_PROMPT_REPLY\"\n\
             \x20   printf '%s\\n' '{request}'\n\
             \x20   local line\n\
             \x20   if read -r -t 2 -u 9 line; then echo \"reply=$line\" >> \"$out\"; else echo 'reply=none' >> \"$out\"; fi\n\
             \x20   echo 'tool done' >> \"$LLM_OUTPUT\"",
            out = out.display(),
        ),
    );
}

/// What an asking tool recorded, as `key -> value`.
fn asked(dir: &Path, name: &str) -> BTreeMap<String, String> {
    let path = dir.join(format!("{name}.out"));
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("{} was not written: {err}", path.display()));
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn run_coyote(dir: &Path, args: &[&str]) -> Output {
    let home = dir.join("home");
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
    let path = env::join_paths(
        iter::once(fixture_bin).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_coyote"));
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            cmd.env_remove(&key);
        }
    }
    cmd.args(args)
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("COLUMNS", "100")
        .env("LINES", "40")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .env("PATH", path)
        .env_remove("COLORTERM")
        .env_remove("IS_SANDBOX")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = common::pty::detached(&mut cmd).spawn().unwrap();
    let pid = child.id();
    let stdout = drain(child.stdout.take().unwrap());
    let stderr = drain(child.stderr.take().unwrap());
    let deadline = Instant::now() + RUN_DEADLINE;
    let status = loop {
        match child.try_wait().unwrap() {
            Some(status) => break Some(status),
            None if Instant::now() >= deadline => {
                kill_session_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => thread::sleep(Duration::from_millis(50)),
        }
    };
    let stdout = stdout.recv_timeout(DRAIN_DEADLINE).unwrap_or_default();
    let stderr = stderr.recv_timeout(DRAIN_DEADLINE).unwrap_or_default();
    let Some(status) = status else {
        panic!(
            "coyote {args:?} did not exit within {RUN_DEADLINE:?}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
    };
    Output {
        status,
        stdout,
        stderr,
    }
}

fn kill_session_group(pid: u32) {
    // SAFETY: kill with a negative pid signals the process group `detached`
    // created for this child and touches no memory.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

fn drain(mut stream: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stream.read_to_end(&mut bytes);
        let _ = tx.send(bytes);
    });
    rx
}

fn transcript(output: &Output) -> String {
    format!(
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The `content` of every `tool` message in the last request the fake
/// answered, which carries the whole conversation, in order.
fn tool_results(fake: &FakeLlm) -> Vec<String> {
    let Some(last) = fake.bodies().pop() else {
        return Vec::new();
    };
    let body: Value = serde_json::from_str(&last).unwrap();
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "tool")
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect()
}

/// The hook map line that snapshots every `COYOTE_*` variable of `event`
/// (and a copy of its payload file, when it has one) into `hook-out`.
fn snapshot_hook(dir: &Path, event: &str) -> String {
    let out = dir.join("hook-out");
    format!(
        "  {event}:\n\
         \x20   - name: snap\n\
         \x20     command: 'f=\"{out}/$COYOTE_EVENT.$$\"; env | grep ^COYOTE_ | sort > \"$f.tmp\"; if [ -n \"$COYOTE_HOOK_PAYLOAD_FILE\" ]; then cp \"$COYOTE_HOOK_PAYLOAD_FILE\" \"$f.payload\" 2>/dev/null || echo gone > \"$f.payload\"; fi; mv \"$f.tmp\" \"$f.env\"'\n",
        out = out.display(),
    )
}

#[derive(Debug)]
struct Snapshot {
    env: BTreeMap<String, String>,
    payload: Option<String>,
}

/// Waits until `hook-out` holds `want` snapshots of each named event and
/// returns every snapshot by event.
fn hook_snapshots(dir: &Path, want: &[(&str, usize)]) -> BTreeMap<String, Vec<Snapshot>> {
    let out = dir.join("hook-out");
    let deadline = Instant::now() + HOOK_DEADLINE;
    loop {
        let mut by_event: BTreeMap<String, Vec<Snapshot>> = BTreeMap::new();
        for entry in fs::read_dir(&out).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".env") else {
                continue;
            };
            let event = stem.rsplit_once('.').unwrap().0.to_string();
            let env = fs::read_to_string(&path)
                .unwrap()
                .lines()
                .filter_map(|line| line.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let payload = fs::read_to_string(out.join(format!("{stem}.payload"))).ok();
            by_event
                .entry(event)
                .or_default()
                .push(Snapshot { env, payload });
        }
        let complete = want
            .iter()
            .all(|(event, n)| by_event.get(*event).map_or(0, Vec::len) >= *n);
        if complete || Instant::now() >= deadline {
            return by_event;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn only<'a>(snaps: &'a BTreeMap<String, Vec<Snapshot>>, event: &str) -> &'a Snapshot {
    let found = snaps.get(event).map(Vec::as_slice).unwrap_or_default();
    assert_eq!(found.len(), 1, "{event} fired once: {snaps:#?}");
    &found[0]
}

fn var<'a>(snap: &'a Snapshot, key: &str) -> Option<&'a str> {
    snap.env.get(key).map(String::as_str)
}

/// "User-interaction tool calls ... return the structured JSON ... with
/// `"needs_human": true`" under `--headless`, and the run still finishes.
#[test]
fn headless_user_confirm_returns_the_needs_human_json() {
    let dir = fresh_config_dir("confirm");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "user__confirm".to_string(),
            arguments: r#"{"question":"Deploy to production?"}"#.to_string(),
        },
        Reply::Text("fixture done".to_string()),
    ]);
    write_config(&dir, &fake, "");
    write_agent(&dir, &[]);

    let output = run_coyote(
        &dir,
        &["--headless", "--agent", "probe", "Ask me before deploying"],
    );
    let text = transcript(&output);
    assert!(output.status.success(), "{text}");
    assert_eq!(fake.served(), 2, "{text}");
    let results = tool_results(&fake);
    assert_eq!(results.len(), 1, "{results:?}\n{text}");
    let answer: Value = serde_json::from_str(&results[0])
        .unwrap_or_else(|err| panic!("the tool result is not JSON ({err}): {}", results[0]));
    assert_eq!(answer["needs_human"], Value::Bool(true), "{answer}");
    assert!(text.contains("fixture done"), "{text}");
}

/// "Questions from tools through the prompt channel are answered `null`, a
/// cancel ... Tools also see `COYOTE_UI=headless`", with the channel "set
/// for every tool call".
#[test]
fn headless_tool_question_is_answered_null() {
    let dir = fresh_config_dir("channel");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_ask".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::Text("fixture done".to_string()),
    ]);
    write_config(&dir, &fake, "");
    write_asking_tool(
        &dir,
        "fixture_ask",
        r#"{"coyote_prompt": {"id": "p-direct-1", "action": "select", "question": "Deploy which environment?", "options": ["staging", "production"]}}"#,
    );

    let output = run_coyote(&dir, &["--headless", "Ask through the tool"]);
    let text = transcript(&output);
    assert!(output.status.success(), "{text}");
    let seen = asked(&dir, "fixture_ask");
    assert_eq!(
        seen.get("ui").map(String::as_str),
        Some("headless"),
        "{seen:?}"
    );
    assert_eq!(
        seen.get("channel").map(String::as_str),
        Some("set"),
        "{seen:?}"
    );
    let reply: Value = serde_json::from_str(&seen["reply"])
        .unwrap_or_else(|err| panic!("reply is not JSON ({err}): {seen:?}"));
    assert_eq!(reply["id"], "p-direct-1", "{reply}");
    assert_eq!(reply["answer"], Value::Null, "{reply}");
    assert!(reply.as_object().unwrap().contains_key("answer"), "{reply}");
    assert!(
        !text.contains("coyote_prompt"),
        "a request line leaked into the output\n{text}"
    );
}

/// "A malformed line with a readable `id` is passed on as output as well
/// as answered `null`", and "a line that carries the marker but no
/// readable `id` gets no reply and is passed on as ordinary output".
#[test]
fn headless_malformed_requests_are_passed_on_and_only_an_id_gets_a_reply() {
    let dir = fresh_config_dir("malformed");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_bad_id".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::ToolCall {
            name: "fixture_no_id".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::Text("fixture done".to_string()),
    ]);
    write_config(&dir, &fake, "");
    write_asking_tool(
        &dir,
        "fixture_bad_id",
        r#"{"coyote_prompt": {"id": "p-bad-1", "action": "juggle", "question": "Juggle?"}}"#,
    );
    write_asking_tool(
        &dir,
        "fixture_no_id",
        r#"{"coyote_prompt": {"action": "confirm", "question": "No id here?"}}"#,
    );

    let output = run_coyote(&dir, &["--headless", "Ask badly"]);
    let text = transcript(&output);
    assert!(output.status.success(), "{text}");
    assert_eq!(fake.served(), 3, "{text}");

    let bad = asked(&dir, "fixture_bad_id");
    let reply: Value = serde_json::from_str(&bad["reply"])
        .unwrap_or_else(|err| panic!("no JSON reply for a readable id ({err}): {bad:?}"));
    assert_eq!(reply["id"], "p-bad-1", "{reply}");
    assert_eq!(reply["answer"], Value::Null, "{reply}");
    let no_id = asked(&dir, "fixture_no_id");
    assert_eq!(
        no_id.get("reply").map(String::as_str),
        Some("none"),
        "{no_id:?}"
    );

    let results = tool_results(&fake);
    let shown = format!("{text}\n{results:?}");
    assert!(
        shown.contains("p-bad-1") && shown.contains("juggle"),
        "the malformed line with an id was not passed on as output:\n{shown}"
    );
    assert!(
        shown.contains("No id here?"),
        "the id-less line was not passed on as output:\n{shown}"
    );
}

/// The channel is "set for every tool call and background job": a job's
/// question is answered `null` under `--headless` too, and its
/// `job.started` / `job.completed` hooks carry `COYOTE_JOB_ID` and
/// `COYOTE_TOOL_NAME`.
#[test]
fn headless_background_job_question_is_answered_null_and_its_hooks_fire() {
    let dir = fresh_config_dir("job");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "job__start".to_string(),
            arguments: r#"{"tool":"fixture_job_ask","arguments":{}}"#.to_string(),
        },
        Reply::ToolCall {
            name: "job__wait".to_string(),
            arguments: r#"{"timeout_secs":30}"#.to_string(),
        },
        Reply::Text("fixture done".to_string()),
    ]);
    let hooks = format!(
        "hooks:\n{}{}{}{}",
        snapshot_hook(&dir, "job.started"),
        snapshot_hook(&dir, "job.completed"),
        snapshot_hook(&dir, "agent.started"),
        snapshot_hook(&dir, "agent.completed")
    );
    write_config(&dir, &fake, &hooks);
    write_asking_tool(
        &dir,
        "fixture_job_ask",
        r#"{"coyote_prompt": {"id": "p-job-1", "action": "confirm", "question": "Go ahead from the job?"}}"#,
    );

    write_agent(&dir, &["fixture_job_ask.sh"]);

    let output = run_coyote(
        &dir,
        &["--headless", "--agent", "probe", "Run it in the background"],
    );
    let text = transcript(&output);
    assert!(output.status.success(), "{text}");
    // The fake never collects the job (its id is random), so the run's
    // uncollected-job guardrail asks again before the turn ends.
    assert!(fake.served() >= 3, "{text}\n{:?}", tool_results(&fake));
    let seen = asked(&dir, "fixture_job_ask");
    assert_eq!(
        seen.get("ui").map(String::as_str),
        Some("headless"),
        "{seen:?}"
    );
    assert_eq!(
        seen.get("channel").map(String::as_str),
        Some("set"),
        "{seen:?}"
    );
    let reply: Value = serde_json::from_str(&seen["reply"])
        .unwrap_or_else(|err| panic!("reply is not JSON ({err}): {seen:?}"));
    assert_eq!(reply["id"], "p-job-1", "{reply}");
    assert_eq!(reply["answer"], Value::Null, "{reply}");

    let snaps = hook_snapshots(
        &dir,
        &[
            ("job.started", 1),
            ("job.completed", 1),
            ("agent.started", 1),
            ("agent.completed", 1),
        ],
    );
    let started = only(&snaps, "job.started");
    let completed = only(&snaps, "job.completed");
    for snap in [started, completed] {
        assert_eq!(
            var(snap, "COYOTE_TOOL_NAME"),
            Some("fixture_job_ask"),
            "{snap:?}"
        );
        assert!(
            var(snap, "COYOTE_JOB_ID").is_some_and(|id| id.starts_with("job_")),
            "{snap:?}"
        );
        assert_eq!(var(snap, "COYOTE_HOOK_NAME"), Some("snap"), "{snap:?}");
    }
    assert_eq!(
        var(started, "COYOTE_JOB_ID"),
        var(completed, "COYOTE_JOB_ID"),
        "one job id pairs the two events"
    );
    for event in ["agent.started", "agent.completed"] {
        let snap = only(&snaps, event);
        assert_eq!(var(snap, "COYOTE_AGENT_NAME"), Some("probe"), "{snap:?}");
        assert!(var(snap, "COYOTE_AGENT_ID").is_some(), "{snap:?}");
    }
}

/// The hooks page's event catalog for one headless turn with a good tool
/// call and an unknown one: `tool.started` / `tool.completed` carry the
/// tool name, the model's call id and a payload file, `tool.completed`
/// adds the duration, the unknown tool's `tool.failed` carries the error
/// but no payload file, and the turn and LLM events carry their own.
#[test]
fn headless_turn_hooks_carry_the_documented_variables() {
    let dir = fresh_config_dir("hooks");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_echo".to_string(),
            arguments: r#"{"text":"hello"}"#.to_string(),
        },
        Reply::ToolCall {
            name: "fixture_missing".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::Text("fixture done".to_string()),
    ]);
    let events = [
        "turn.started",
        "turn.completed",
        "tool.started",
        "tool.completed",
        "tool.failed",
        "llm.request.started",
        "llm.request.completed",
    ];
    let hooks: String = events.iter().map(|e| snapshot_hook(&dir, e)).collect();
    write_config(&dir, &fake, &format!("hooks:\n{hooks}"));
    write_tool(
        &dir,
        "fixture_echo",
        "Echo a fixed line for the harness.\n# @option --text! The text to echo",
        "    echo \"fixture result: $argc_text\" >> \"$LLM_OUTPUT\"",
    );

    let output = run_coyote(&dir, &["--headless", "Call the fixture tool"]);
    let text = transcript(&output);
    assert!(output.status.success(), "{text}");
    assert_eq!(fake.served(), 3, "{text}");

    let snaps = hook_snapshots(
        &dir,
        &[
            ("turn.started", 1),
            ("turn.completed", 1),
            ("tool.started", 1),
            ("tool.completed", 1),
            ("tool.failed", 1),
            ("llm.request.started", 3),
            ("llm.request.completed", 3),
        ],
    );
    let config_dir = dir.display().to_string();
    for (event, list) in &snaps {
        for snap in list {
            assert_eq!(var(snap, "COYOTE_EVENT"), Some(event.as_str()), "{snap:?}");
            assert_eq!(var(snap, "COYOTE_HOOK_NAME"), Some("snap"), "{snap:?}");
            assert_eq!(
                var(snap, "COYOTE_CONFIG_DIR"),
                Some(config_dir.as_str()),
                "{snap:?}"
            );
            assert!(var(snap, "COYOTE_EVENT_TIMESTAMP").is_some(), "{snap:?}");
        }
    }

    let started = only(&snaps, "tool.started");
    let completed = only(&snaps, "tool.completed");
    for snap in [started, completed] {
        assert_eq!(
            var(snap, "COYOTE_TOOL_NAME"),
            Some("fixture_echo"),
            "{snap:?}"
        );
        assert_eq!(var(snap, "COYOTE_TOOL_CALL_ID"), Some("call_1"), "{snap:?}");
        assert!(var(snap, "COYOTE_HOOK_PAYLOAD_FILE").is_some(), "{snap:?}");
        assert!(
            snap.payload.as_deref().is_some_and(|p| p.contains("hello")),
            "the payload file carries the call's arguments: {snap:?}"
        );
    }
    assert!(
        var(started, "COYOTE_TOOL_DURATION_MS").is_none(),
        "{started:?}"
    );
    assert!(
        var(completed, "COYOTE_TOOL_DURATION_MS").is_some_and(|ms| ms.parse::<u64>().is_ok()),
        "{completed:?}"
    );

    let failed = only(&snaps, "tool.failed");
    assert_eq!(
        var(failed, "COYOTE_TOOL_NAME"),
        Some("fixture_missing"),
        "{failed:?}"
    );
    assert_eq!(
        var(failed, "COYOTE_TOOL_CALL_ID"),
        Some("call_1"),
        "{failed:?}"
    );
    assert!(var(failed, "COYOTE_TOOL_ERROR").is_some(), "{failed:?}");
    assert!(
        var(failed, "COYOTE_HOOK_PAYLOAD_FILE").is_none(),
        "{failed:?}"
    );

    for snap in &snaps["llm.request.started"] {
        assert_eq!(var(snap, "COYOTE_LLM_PROVIDER"), Some("fake"), "{snap:?}");
        assert!(
            var(snap, "COYOTE_LLM_MODEL").is_some_and(|m| m.ends_with("fake-model")),
            "{snap:?}"
        );
    }
    for snap in &snaps["llm.request.completed"] {
        assert!(var(snap, "COYOTE_LLM_DURATION_MS").is_some(), "{snap:?}");
    }
    for event in ["turn.started", "turn.completed"] {
        let snap = only(&snaps, event);
        assert!(var(snap, "COYOTE_TOOL_NAME").is_none(), "{snap:?}");
    }
}
