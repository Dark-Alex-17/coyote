//! End-to-end contract for the `--json` wrapper through the real binary,
//! on the plain one-shot path and the graph-agent path. A loopback
//! chat-completions server plays the model, answering each request by its
//! content and keeping every request body, so a probe can check that
//! `num_turns` is exactly the number of model calls the server saw.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, process};

use serde_json::{Value, json};

const REPLY: &str = "**hello** from the stub";
const TOOL: &str = "agent__list_sessions";
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// Every successful reply reports this usage: 7 prompt tokens of which 2
/// were cache reads, so the run sees 5 input, 3 output and 2 cache-read
/// tokens per call, and never a cache write.
fn usage() -> Value {
    json!({
        "prompt_tokens": 7,
        "completion_tokens": 3,
        "total_tokens": 10,
        "prompt_tokens_details": { "cached_tokens": 2 }
    })
}

fn assert_usage_of(report: &Value, calls: u64) {
    let usage = &report["usage"];
    assert_eq!(usage["input_tokens"], json!(5 * calls), "{report}");
    assert_eq!(usage["output_tokens"], json!(3 * calls), "{report}");
    assert_eq!(
        usage["cache_read_input_tokens"],
        json!(2 * calls),
        "{report}"
    );
    assert_eq!(
        usage["cache_creation_input_tokens"],
        Value::Null,
        "{report}"
    );
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "coyote-json-output-{label}-{}-{unique}",
            process::id()
        ));
        fs::create_dir_all(dir.join("home")).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One scripted answer. `Text` is sent as an SSE stream ending in a usage
/// chunk when the request asks for `stream: true`, and as a plain
/// completion otherwise.
enum Reply {
    Text(String),
    Call(&'static str),
    Status(u16, Value),
}

/// A loopback chat-completions server that answers each request with
/// `respond(request, earlier_requests)` and keeps every request body it
/// saw. One connection per request.
struct ScriptedModel {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl ScriptedModel {
    fn answering(respond: impl Fn(&Value, &[Value]) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
        let seen = Arc::clone(&requests);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some(body) = read_request_body(&mut stream) else {
                    continue;
                };
                let request = serde_json::from_str::<Value>(&body).unwrap_or_default();
                let reply = {
                    let mut seen = seen.lock().unwrap();
                    let reply = respond(&request, &seen);
                    seen.push(request.clone());
                    reply
                };
                let (status, content_type, payload) = match reply {
                    Reply::Text(text) if request["stream"] == json!(true) => {
                        (200, "text/event-stream", sse_completion(&text))
                    }
                    Reply::Text(text) => (200, "application/json", text_completion(&text)),
                    Reply::Call(name) => (
                        200,
                        "application/json",
                        tool_call_completion(name, seen.lock().unwrap().len()),
                    ),
                    Reply::Status(code, body) => (code, "application/json", body.to_string()),
                };
                let reason = match status {
                    200 => "OK",
                    400 => "Bad Request",
                    _ => "Internal Server Error",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self { port, requests }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

fn read_request_body(stream: &mut impl Read) -> Option<String> {
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let (header_end, content_length) = loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&chunk[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..pos]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (pos + 4, length);
        }
    };
    while raw.len() < header_end + content_length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
    }
    Some(String::from_utf8_lossy(&raw[header_end..]).into_owned())
}

fn text_completion(text: &str) -> String {
    json!({
        "id": "stub",
        "object": "chat.completion",
        "created": 0,
        "model": "stub-model",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": usage()
    })
    .to_string()
}

fn tool_call_completion(name: &str, seq: usize) -> String {
    json!({
        "id": "stub",
        "object": "chat.completion",
        "created": 0,
        "model": "stub-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": format!("call_{seq}"),
                    "type": "function",
                    "function": { "name": name, "arguments": "{}" }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": usage()
    })
    .to_string()
}

/// The reply as one content delta and a finish chunk, then the trailing
/// `choices: []` chunk that carries the usage.
fn sse_completion(text: &str) -> String {
    let chunk = |choices: Value, usage: Value| {
        json!({
            "id": "stub",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "stub-model",
            "choices": choices,
            "usage": usage
        })
    };
    format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            json!([{ "index": 0, "delta": { "role": "assistant", "content": text }, "finish_reason": null }]),
            Value::Null
        ),
        chunk(
            json!([{ "index": 0, "delta": {}, "finish_reason": "stop" }]),
            Value::Null
        ),
        chunk(json!([]), usage()),
    )
}

fn has_response_format(request: &Value) -> bool {
    request.get("response_format").is_some()
}

fn mentions(request: &Value, needle: &str) -> bool {
    request["messages"].to_string().contains(needle)
}

/// `structured` marks the stub model as supporting native structured
/// output and never streaming, the shape graph llm nodes need here.
fn write_config(dir: &Path, port: u16, stream: bool, structured: bool) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    let model_keys = if structured {
        "\x20       supports_structured_output: true\n\
         \x20       no_stream: true\n"
    } else {
        ""
    };
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: stub:stub-model\n\
             dry_run: false\n\
             save_session: false\n\
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
             {model_keys}\
             save: false\n\
             memory: false\n\
             stream: {stream}\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

fn write_graph_agent(dir: &Path, name: &str, graph: &str) {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(agent_dir.join("graph.yaml"), graph).unwrap();
}

fn write_config_agent(dir: &Path, name: &str, config: &str) {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(agent_dir.join("config.yaml"), config).unwrap();
}

fn run(dir: &Path, args: &[&str]) -> Output {
    run_with_env(dir, args, &[])
}

/// Runs the binary with a scrubbed environment: every inherited `COYOTE_*`
/// variable is dropped before the test's own are set, and a run that
/// outlives `RUN_TIMEOUT` is killed.
fn run_with_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let home = dir.join("home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_coyote"));
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            command.env_remove(key);
        }
    }
    let child = command
        .args(args)
        .current_dir(dir)
        .envs(envs.iter().copied())
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        // Loopback must not be sent through a host proxy.
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("IS_SANDBOX")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_bounded(child, args)
}

fn wait_bounded(mut child: Child, args: &[&str]) -> Output {
    let drain = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = pipe.read_to_end(&mut buf);
            buf
        })
    };
    let stdout = drain(Box::new(child.stdout.take().unwrap()));
    let stderr = drain(Box::new(child.stderr.take().unwrap()));
    let deadline = Instant::now() + RUN_TIMEOUT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "coyote {args:?} did not exit within {}s",
                RUN_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

fn combined(output: &Output) -> String {
    format!(
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Parses stdout as exactly one JSON object followed by one newline.
fn the_only_object(output: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .strip_suffix('\n')
        .unwrap_or_else(|| panic!("stdout must end in one newline:\n{}", combined(output)));
    assert!(
        !line.contains('\n'),
        "more than one line:\n{}",
        combined(output)
    );
    let value: Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|err| panic!("stdout is not JSON ({err}):\n{}", combined(output)));
    assert!(value.is_object(), "{}", combined(output));
    value
}

fn assert_turns_match_requests(report: &Value, model: &ScriptedModel, expected: u64) {
    let requests = model.requests().len() as u64;
    assert_eq!(
        requests, expected,
        "server saw {requests} requests: {report}"
    );
    assert_eq!(report["num_turns"], json!(expected), "{report}");
}

const KEYS: [&str; 9] = [
    "result",
    "is_error",
    "error",
    "model",
    "session_id",
    "num_turns",
    "duration_ms",
    "usage",
    "total_cost_usd",
];

// ---- plain one-shot ----

#[test]
fn a_plain_one_shot_prints_one_object_whose_usage_is_the_reported_usage() {
    let dir = TempDir::new("plain");
    let model = ScriptedModel::answering(|_, _| Reply::Text(REPLY.into()));
    write_config(&dir.0, model.port, false, false);

    let output = run(&dir.0, &["--json", "hi"]);

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    let keys: Vec<&str> = report
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, KEYS, "{report}");
    assert_eq!(report["result"], json!(REPLY), "{report}");
    assert_eq!(report["is_error"], json!(false), "{report}");
    assert_eq!(report["model"], json!("stub:stub-model"), "{report}");
    assert_turns_match_requests(&report, &model, 1);
    assert_usage_of(&report, 1);
}

#[test]
fn a_tool_call_then_an_answer_is_two_turns_with_both_usages_summed() {
    let dir = TempDir::new("tool-call");
    let model = ScriptedModel::answering(|request, _| {
        if mentions(request, "\"role\":\"tool\"") {
            Reply::Text(REPLY.into())
        } else {
            Reply::Call(TOOL)
        }
    });
    write_config(&dir.0, model.port, false, false);
    write_config_agent(
        &dir.0,
        "lister",
        "name: lister\n\
         model: stub:stub-model\n\
         instructions: You keep an eye on child sessions.\n\
         can_spawn_agents: true\n",
    );

    let output = run_with_env(
        &dir.0,
        &["--json", "-a", "lister", "hi"],
        &[("COYOTE_SHOW_TOOL_CALLS", "true")],
    );

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(report["result"], json!(REPLY), "{report}");
    assert_turns_match_requests(&report, &model, 2);
    assert_usage_of(&report, 2);
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains(TOOL),
        "{}",
        combined(&output)
    );
}

#[test]
fn a_streamed_reply_reports_the_usage_from_its_final_chunk() {
    let dir = TempDir::new("stream");
    let model = ScriptedModel::answering(|_, _| Reply::Text(REPLY.into()));
    write_config(&dir.0, model.port, true, false);

    let output = run(&dir.0, &["--json", "hi"]);

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(model.requests()[0]["stream"], json!(true), "{report}");
    assert_eq!(report["result"], json!(REPLY), "{report}");
    assert_turns_match_requests(&report, &model, 1);
    assert_usage_of(&report, 1);
}

#[test]
fn a_leading_think_block_is_stripped_from_a_plain_result() {
    let dir = TempDir::new("think-plain");
    let model = ScriptedModel::answering(|_, _| {
        Reply::Text("<think>mulling it over</think>\nfinal".into())
    });
    write_config(&dir.0, model.port, false, false);

    let output = run(&dir.0, &["--json", "hi"]);

    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(the_only_object(&output)["result"], json!("final"));
}

#[test]
fn a_provider_error_prints_the_error_wrapper_counting_the_failed_call() {
    let dir = TempDir::new("500");
    let model = ScriptedModel::answering(|_, _| {
        Reply::Status(
            500,
            json!({ "error": { "message": "stub exploded", "type": "server_error" } }),
        )
    });
    write_config(&dir.0, model.port, false, false);

    let output = run(&dir.0, &["--json", "hi"]);

    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(report["is_error"], json!(true), "{report}");
    assert_eq!(report["result"], Value::Null, "{report}");
    assert!(
        report["error"]
            .as_str()
            .is_some_and(|e| e.contains("stub exploded")),
        "{report}"
    );
    assert_turns_match_requests(&report, &model, 1);
    for key in ["input_tokens", "output_tokens", "cache_read_input_tokens"] {
        assert_eq!(report["usage"][key], Value::Null, "{key}: {report}");
    }
}

// ---- rejected invocations ----

#[test]
fn json_with_execute_or_a_macro_is_rejected_by_the_parser_with_empty_stdout() {
    let dir = TempDir::new("conflicts");
    let model = ScriptedModel::answering(|_, _| Reply::Text(REPLY.into()));
    write_config(&dir.0, model.port, false, false);

    for args in [
        &["--json", "-e", "hi"][..],
        &["--json", "--macro", "x", "hi"],
    ] {
        let output = run(&dir.0, args);

        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}\n{}",
            combined(&output)
        );
        assert!(output.stdout.is_empty(), "{args:?}\n{}", combined(&output));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--json"),
            "{args:?}\n{}",
            combined(&output)
        );
    }
    assert!(model.requests().is_empty());
}

#[test]
fn json_without_any_prompt_fails_plainly_on_stderr() {
    let dir = TempDir::new("no-prompt");
    let model = ScriptedModel::answering(|_, _| Reply::Text(REPLY.into()));
    write_config(&dir.0, model.port, false, false);

    let output = run(&dir.0, &["--json"]);

    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    assert!(output.stdout.is_empty(), "{}", combined(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--json requires a prompt"),
        "{}",
        combined(&output)
    );
    assert!(model.requests().is_empty());
}

// ---- graph agents ----

const ANSWER_JSON: &str = r#"{"answer":"forty-two"}"#;
const CHILD_MARKER: &str = "CHILD-MARKER";

const SCHEMA_NODE: &str = "\x20 ask:\n\
     \x20   type: llm\n\
     \x20   prompt: \"{{initial_prompt}}\"\n\
     \x20   output_schema: { type: object, properties: { answer: { type: string } }, required: [answer] }\n\
     \x20   state_updates: { answer: \"{{output.answer}}\" }\n";

/// An llm node with an `output_schema` whose answer is handed to a config
/// agent; the graph's output is the child's reply.
fn write_schema_then_child_graph(dir: &Path) {
    write_config_agent(dir, "helper", "name: helper\ninstructions: You help.\n");
    write_graph_agent(
        dir,
        "schema-graph",
        &format!(
            "name: schema-graph\n\
             version: \"1.0\"\n\
             description: Asks for a structured answer, then delegates.\n\
             initial_state: {{ answer: \"\", reply: \"\" }}\n\
             start: ask\n\
             nodes:\n\
             {SCHEMA_NODE}\
             \x20   next: delegate\n\
             \x20 delegate: {{ type: agent, agent: helper, prompt: \"{CHILD_MARKER} {{{{answer}}}}\", state_updates: {{ reply: \"{{{{output}}}}\" }}, next: done }}\n\
             \x20 done: {{ type: end, output: \"{{{{reply}}}}\" }}\n"
        ),
    );
}

#[test]
fn a_graph_with_a_schema_node_and_a_child_agent_counts_every_call() {
    let dir = TempDir::new("graph-child");
    let model = ScriptedModel::answering(|request, _| {
        if mentions(request, CHILD_MARKER) {
            Reply::Text("<think>delegated thoughts</think>\nfinal from the child".into())
        } else {
            Reply::Text(ANSWER_JSON.into())
        }
    });
    write_config(&dir.0, model.port, false, true);
    write_schema_then_child_graph(&dir.0);

    let output = run(&dir.0, &["--json", "--agent", "schema-graph", "hi"]);

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(report["is_error"], json!(false), "{report}");
    assert_eq!(report["result"], json!("final from the child"), "{report}");
    let requests = model.requests();
    let schema_call = requests
        .iter()
        .find(|r| !mentions(r, CHILD_MARKER))
        .unwrap_or_else(|| panic!("no llm node request: {requests:?}"));
    assert_eq!(
        schema_call["response_format"]["type"],
        json!("json_schema"),
        "{schema_call}"
    );
    assert!(
        requests
            .iter()
            .any(|r| mentions(r, &format!("{CHILD_MARKER} forty-two"))),
        "the child never got the structured answer: {requests:?}"
    );
    assert_turns_match_requests(&report, &model, 2);
    assert_usage_of(&report, 2);
}

#[test]
fn a_rejected_native_format_is_retried_without_it_and_the_rejection_counts() {
    let dir = TempDir::new("graph-fallback");
    let model = ScriptedModel::answering(|request, earlier| {
        if has_response_format(request) && !earlier.iter().any(has_response_format) {
            Reply::Status(
                400,
                json!({ "error": { "message": "response_format is not supported", "type": "invalid_request_error" } }),
            )
        } else {
            Reply::Text(ANSWER_JSON.into())
        }
    });
    write_config(&dir.0, model.port, false, true);
    write_graph_agent(
        &dir.0,
        "fallback-graph",
        &format!(
            "name: fallback-graph\n\
             version: \"1.0\"\n\
             description: Asks for a structured answer.\n\
             initial_state: {{ answer: \"\" }}\n\
             start: ask\n\
             nodes:\n\
             {SCHEMA_NODE}\
             \x20   next: done\n\
             \x20 done: {{ type: end, output: \"{{{{answer}}}}\" }}\n"
        ),
    );

    let output = run(&dir.0, &["--json", "--agent", "fallback-graph", "hi"]);

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(report["result"], json!("forty-two"), "{report}");
    let requests = model.requests();
    assert!(has_response_format(&requests[0]), "{requests:?}");
    assert!(!has_response_format(&requests[1]), "{}", requests[1]);
    assert_turns_match_requests(&report, &model, 2);
    assert_usage_of(&report, 1);
}
