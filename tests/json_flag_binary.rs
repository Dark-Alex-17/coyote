//! Black-box contract for `--json` on the plain one-shot path: a cold config
//! dir and a loopback chat-completions server that plays the model. Each
//! probe reads the binary's stdout as the caller of `--json` would, so a
//! stray byte of rendered markdown or a second object fails the parse.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, process};

use serde_json::{Value, json};

const REPLY: &str = "**hello** from the stub";
const TOOL: &str = "agent__list_sessions";
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!(
            "coyote-json-flag-{label}-{}-{unique}",
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

#[derive(Clone, Copy)]
enum Behaviour {
    Answer,
    ServerError,
    /// Calls `TOOL` until a request carries a tool result, then answers.
    CallToolThenAnswer,
    /// Calls the named tool with identical arguments until the request
    /// carries that many tool results, then answers.
    CallNamedToolThenAnswer(&'static str, usize),
}

/// A loopback chat-completions server that answers each request by its
/// `behaviour`: `REPLY` as a plain or SSE completion (matching the request's
/// `stream` field), an HTTP 500, or a plain tool call. One connection per
/// request.
fn serve(behaviour: Behaviour) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(body) = read_request_body(&mut stream) else {
                continue;
            };
            let request = serde_json::from_str::<Value>(&body).unwrap_or_default();
            let streaming = request["stream"] == json!(true);
            let tool_results = request["messages"].as_array().map_or(0, |messages| {
                messages.iter().filter(|m| m["role"] == "tool").count()
            });
            let has_tool_result = tool_results > 0;
            let (status, content_type, payload) = match behaviour {
                Behaviour::ServerError => (
                    "500 Internal Server Error",
                    "application/json",
                    json!({ "error": { "message": "stub exploded", "type": "server_error" } })
                        .to_string(),
                ),
                Behaviour::CallToolThenAnswer if !has_tool_result => {
                    ("200 OK", "application/json", tool_call_completion(TOOL, 0))
                }
                Behaviour::CallNamedToolThenAnswer(name, calls) if tool_results < calls => (
                    "200 OK",
                    "application/json",
                    tool_call_completion(name, tool_results),
                ),
                Behaviour::Answer
                | Behaviour::CallToolThenAnswer
                | Behaviour::CallNamedToolThenAnswer(..)
                    if streaming =>
                {
                    ("200 OK", "text/event-stream", sse_completion())
                }
                Behaviour::Answer
                | Behaviour::CallToolThenAnswer
                | Behaviour::CallNamedToolThenAnswer(..) => {
                    ("200 OK", "application/json", plain_completion())
                }
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
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

fn plain_completion() -> String {
    json!({
        "id": "stub",
        "object": "chat.completion",
        "created": 0,
        "model": "stub-model",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": REPLY },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
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
        "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
    })
    .to_string()
}

fn sse_completion() -> String {
    let chunk = |delta: Value, finish: Value| {
        json!({
            "id": "stub",
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "stub-model",
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
        })
    };
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        chunk(
            json!({ "role": "assistant", "content": REPLY }),
            Value::Null
        ),
        chunk(json!({}), json!("stop")),
    )
}

fn write_config(dir: &Path, port: u16, stream: bool) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
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
             save: false\n\
             memory: false\n\
             stream: {stream}\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

fn run(dir: &Path, args: &[&str]) -> Output {
    run_with_stdin(dir, args, None, &[])
}

/// Runs the binary with a scrubbed environment: every inherited `COYOTE_*`
/// variable is dropped before the test's own are set, and a run that
/// outlives `RUN_TIMEOUT` is killed.
fn run_with_stdin(dir: &Path, args: &[&str], stdin: Option<&str>, envs: &[(&str, &str)]) -> Output {
    let home = dir.join("home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_coyote"));
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            command.env_remove(key);
        }
    }
    let mut child = command
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
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(text) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(text.as_bytes()).unwrap();
    }
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
    let value: Value = serde_json::from_str(line)
        .unwrap_or_else(|err| panic!("stdout is not JSON ({err}):\n{}", combined(output)));
    assert!(value.is_object(), "{}", combined(output));
    value
}

fn json_run(label: &str, behaviour: Behaviour, stream: bool, args: &[&str]) -> Output {
    let dir = TempDir::new(label);
    write_config(&dir.0, serve(behaviour), stream);
    run(&dir.0, args)
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

fn assert_keys_in_order(report: &Value) {
    let keys: Vec<&str> = report
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, KEYS, "{report}");
}

#[test]
fn a_non_streamed_reply_is_the_only_stdout_and_skips_markdown() {
    let output = json_run("plain", Behaviour::Answer, false, &["--json", "hi"]);

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_keys_in_order(&report);
    assert_eq!(report["result"], json!(REPLY), "{report}");
    assert_eq!(report["is_error"], json!(false), "{report}");
    assert_eq!(report["error"], Value::Null, "{report}");
    assert_eq!(report["model"], json!("stub:stub-model"), "{report}");
    assert_eq!(report["session_id"], Value::Null, "{report}");
    assert_eq!(report["num_turns"], json!(1), "{report}");
    assert!(report["duration_ms"].is_u64(), "{report}");
}

#[test]
fn a_streamed_reply_renders_nothing_besides_the_object() {
    let output = json_run("stream", Behaviour::Answer, true, &["--json", "hi"]);

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(report["result"], json!(REPLY), "{report}");
    assert_eq!(report["num_turns"], json!(1), "{report}");
}

#[test]
fn a_named_session_is_reported_as_the_session_id() {
    let output = json_run(
        "session",
        Behaviour::Answer,
        false,
        &["--json", "-S", "--headless", "--session", "s", "hi"],
    );

    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(the_only_object(&output)["session_id"], json!("s"));
}

#[test]
fn a_missing_prompt_fails_on_stderr_with_empty_stdout() {
    let output = json_run("no-prompt", Behaviour::Answer, false, &["--json"]);

    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    assert!(output.stdout.is_empty(), "{}", combined(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--json requires a prompt (argument, -f, or stdin)"),
        "{}",
        combined(&output)
    );
}

// ---- first-time RAG build reached by an agent load ----

const RAG_INIT_LINE: &str = "Initializing RAG";

/// Like `serve(Behaviour::Answer)`, but answers an embeddings request (a
/// body carrying `input`) with one fixed vector per input.
fn serve_with_embeddings() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(body) = read_request_body(&mut stream) else {
                continue;
            };
            let request = serde_json::from_str::<Value>(&body).unwrap_or_default();
            let (content_type, payload) = if request.get("input").is_some() {
                let count = request["input"].as_array().map_or(1, Vec::len);
                ("application/json", json!({
                    "object": "list",
                    "data": (0..count)
                        .map(|i| json!({ "object": "embedding", "index": i, "embedding": [0.1, 0.2, 0.3] }))
                        .collect::<Vec<_>>(),
                    "model": "stub-embed",
                    "usage": { "prompt_tokens": 1, "total_tokens": 1 }
                })
                .to_string())
            } else if request["stream"] == json!(true) {
                ("text/event-stream", sse_completion())
            } else {
                ("application/json", plain_completion())
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

/// A cold config dir with a chat and an embedding model, plus a graph agent
/// whose rag node builds its knowledge base on first load with no prompts.
fn rag_graph_dir(label: &str) -> TempDir {
    let dir = TempDir::new(label);
    let port = serve_with_embeddings();
    write_config(&dir.0, port, false);
    let config = fs::read_to_string(dir.0.join("config.yaml")).unwrap();
    let config = config.replace(
        "       supports_function_calling: true\n",
        "       supports_function_calling: true\n\
         \x20     - name: stub-embed\n\
         \x20       type: embedding\n\
         \x20       max_tokens_per_chunk: 8000\n\
         \x20       default_chunk_size: 1000\n\
         \x20       max_batch_size: 10\n",
    );
    fs::write(dir.0.join("config.yaml"), config).unwrap();
    let agent_dir = dir.0.join("agents").join("rag-graph");
    fs::create_dir_all(agent_dir.join("knowledge")).unwrap();
    fs::write(
        agent_dir.join("knowledge").join("notes.md"),
        "The stub knows one fact.\n",
    )
    .unwrap();
    fs::write(
        agent_dir.join("graph.yaml"),
        "name: rag-graph\n\
         version: \"1.0\"\n\
         description: Retrieves, then asks.\n\
         initial_state: { context: \"\", reply: \"\" }\n\
         start: retrieve\n\
         nodes:\n\
         \x20 retrieve:\n\
         \x20   type: rag\n\
         \x20   documents: [ ./knowledge/ ]\n\
         \x20   embedding_model: stub:stub-embed\n\
         \x20   chunk_size: 1000\n\
         \x20   chunk_overlap: 100\n\
         \x20   driver: yaml\n\
         \x20   state_updates: { context: \"{{output.context}}\" }\n\
         \x20   next: ask\n\
         \x20 ask: { type: llm, prompt: \"{{initial_prompt}}\", state_updates: { reply: \"{{output}}\" }, next: done }\n\
         \x20 done: { type: end, output: \"{{reply}}\" }\n",
    )
    .unwrap();
    dir
}

#[test]
fn a_first_time_agent_rag_build_goes_to_stderr_under_json() {
    let plain_dir = rag_graph_dir("rag-build-plain");
    let plain = run(&plain_dir.0, &["--agent", "rag-graph", "hi"]);
    assert!(plain.status.success(), "{}", combined(&plain));
    assert!(
        String::from_utf8_lossy(&plain.stdout).contains(RAG_INIT_LINE),
        "without --json the RAG build progress is on stdout:\n{}",
        combined(&plain)
    );

    let dir = rag_graph_dir("rag-build-json");
    let output = run(&dir.0, &["--json", "--agent", "rag-graph", "hi"]);
    assert!(output.status.success(), "{}", combined(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in [
        RAG_INIT_LINE,
        "Embedding model",
        "Chunk size",
        "Chunk overlap",
        "Saved RAG",
        "Load ",
    ] {
        assert!(
            !stdout.contains(line),
            "`{line}` reached stdout under --json:\n{}",
            combined(&output)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(RAG_INIT_LINE),
        "under --json the RAG build progress moves to stderr:\n{}",
        combined(&output)
    );
    let report = the_only_object(&output);
    assert_eq!(report["result"], json!(REPLY), "{report}");
    assert_eq!(report["is_error"], json!(false), "{report}");
}

#[test]
fn a_setup_error_before_the_run_stays_plain() {
    let output = json_run(
        "agent",
        Behaviour::Answer,
        false,
        &["--json", "--agent", "no-such-agent", "hi"],
    );

    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    assert!(output.stdout.is_empty(), "{}", combined(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no-such-agent"),
        "{}",
        combined(&output)
    );
}

#[test]
fn a_failed_model_call_prints_the_error_wrapper_and_exits_1() {
    let output = json_run("500", Behaviour::ServerError, false, &["--json", "hi"]);

    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_keys_in_order(&report);
    assert_eq!(report["is_error"], json!(true), "{report}");
    assert_eq!(report["result"], Value::Null, "{report}");
    let error = report["error"].as_str().unwrap_or_default();
    assert!(!error.is_empty(), "{report}");
    assert_eq!(report["model"], json!("stub:stub-model"), "{report}");
    assert!(!output.stderr.is_empty(), "{}", combined(&output));
}

#[test]
fn a_graph_agent_run_prints_its_output_as_the_only_object() {
    let dir = TempDir::new("graph");
    write_config(&dir.0, serve(Behaviour::Answer), false);
    let agent_dir = dir.0.join("agents").join("ask-graph");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("graph.yaml"),
        "name: ask-graph\n\
         version: \"1.0\"\n\
         description: Asks the model once.\n\
         initial_state: { reply: \"\" }\n\
         start: ask\n\
         nodes:\n\
         \x20 ask: { type: llm, prompt: \"{{initial_prompt}}\", state_updates: { reply: \"{{output}}\" }, next: done }\n\
         \x20 done: { type: end, output: \"{{reply}}\" }\n",
    )
    .unwrap();

    let output = run(&dir.0, &["--json", "--agent", "ask-graph", "hi"]);

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_keys_in_order(&report);
    assert_eq!(report["result"], json!(REPLY), "{report}");
    assert_eq!(report["is_error"], json!(false), "{report}");
    assert_eq!(report["num_turns"], json!(1), "{report}");
}

// ---- usage block, stdin and -f prompts, failed turns, rejected flags ----

const USAGE_KEYS: [&str; 4] = [
    "input_tokens",
    "output_tokens",
    "cache_read_input_tokens",
    "cache_creation_input_tokens",
];

fn usage_keys(report: &Value) -> Vec<String> {
    report["usage"]
        .as_object()
        .unwrap_or_else(|| panic!("usage must be an object: {report}"))
        .keys()
        .cloned()
        .collect()
}

#[test]
fn reported_token_counts_land_in_usage_and_unreported_ones_are_null() {
    let output = json_run("usage", Behaviour::Answer, false, &["--json", "hi"]);

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(usage_keys(&report), USAGE_KEYS, "{report}");
    assert_eq!(report["usage"]["input_tokens"], json!(1), "{report}");
    assert_eq!(report["usage"]["output_tokens"], json!(1), "{report}");
    assert_eq!(
        report["usage"]["cache_read_input_tokens"],
        Value::Null,
        "{report}"
    );
    assert_eq!(
        report["usage"]["cache_creation_input_tokens"],
        Value::Null,
        "{report}"
    );
    // The stub model has no pricing, so the cost cannot be computed.
    assert_eq!(report["total_cost_usd"], Value::Null, "{report}");
}

#[test]
fn a_stream_without_usage_reports_every_usage_field_as_null() {
    let output = json_run("stream-usage", Behaviour::Answer, true, &["--json", "hi"]);

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(usage_keys(&report), USAGE_KEYS, "{report}");
    for key in USAGE_KEYS {
        assert_eq!(report["usage"][key], Value::Null, "{key}: {report}");
    }
}

#[test]
fn a_prompt_piped_on_stdin_is_enough() {
    let dir = TempDir::new("stdin");
    write_config(&dir.0, serve(Behaviour::Answer), false);
    let output = run_with_stdin(&dir.0, &["--json"], Some("hi from stdin\n"), &[]);

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(report["result"], json!(REPLY), "{report}");
    assert_eq!(report["is_error"], json!(false), "{report}");
}

#[test]
fn a_file_alone_is_enough_of_a_prompt() {
    let dir = TempDir::new("file");
    write_config(&dir.0, serve(Behaviour::Answer), false);
    let file = dir.0.join("prompt.txt");
    fs::write(&file, "summarise me\n").unwrap();
    let output = run(&dir.0, &["--json", "-f", file.to_str().unwrap()]);

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(report["result"], json!(REPLY), "{report}");
}

#[test]
fn a_failed_call_counts_as_a_turn_and_adds_no_usage() {
    let output = json_run(
        "500-usage",
        Behaviour::ServerError,
        false,
        &["--json", "hi"],
    );

    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_keys_in_order(&report);
    // The plain path sends the request once; a 500 is not retried.
    assert_eq!(report["num_turns"], json!(1), "{report}");
    assert_eq!(usage_keys(&report), USAGE_KEYS, "{report}");
    for key in USAGE_KEYS {
        assert_eq!(report["usage"][key], Value::Null, "{key}: {report}");
    }
    assert_eq!(report["session_id"], Value::Null, "{report}");
}

#[test]
fn stdout_printing_flags_are_rejected_alongside_json_before_anything_runs() {
    let dir = TempDir::new("conflicts");
    write_config(&dir.0, serve(Behaviour::Answer), false);
    let flags: &[&[&str]] = &[
        &["-e"],
        &["-c"],
        &["--dry-run"],
        &["--raw-markdown"],
        &["--macro", "x"],
        &["--acp-server"],
        &["--memory-migrate", "global"],
        &["--list-models"],
        &["--list-roles"],
        &["--list-sessions"],
        &["--list-agents"],
        &["--list-rags"],
        &["--list-macros"],
        &["--list-skills"],
        &["--info"],
        &["--init-memory", "global"],
        &["--init-instructions"],
        &["--sync-models"],
        &["--build-tools"],
        &["--completions", "bash"],
        &["--tail-logs"],
        &["--rebuild-rag"],
    ];
    for flag in flags {
        let mut args = vec!["--json"];
        args.extend_from_slice(flag);
        // `--memory-migrate` also rejects a prompt, which would mask the
        // `--json` conflict, so that row runs without one.
        if flag[0] != "--memory-migrate" {
            args.push("hi");
        }
        let output = run(&dir.0, &args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{flag:?}\n{}",
            combined(&output)
        );
        assert!(output.stdout.is_empty(), "{flag:?}\n{}", combined(&output));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--json"),
            "{flag:?}\n{}",
            combined(&output)
        );
    }
    assert!(!dir.0.join("COYOTE.md").exists(), "--init-instructions ran");
}

// ---- stray writers: tool calls, first-time setup, skills ----

/// An agent allowed to spawn children, so `TOOL` is declared to the model.
fn write_tool_agent(dir: &Path) {
    let agent_dir = dir.join("agents").join("lister");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        "name: lister\n\
         model: stub:stub-model\n\
         instructions: You keep an eye on child sessions.\n\
         can_spawn_agents: true\n",
    )
    .unwrap();
}

#[test]
fn a_tool_call_log_never_reaches_stdout() {
    let dir = TempDir::new("tool-call");
    write_config(&dir.0, serve(Behaviour::CallToolThenAnswer), false);
    write_tool_agent(&dir.0);
    let show_tool_calls = [("COYOTE_SHOW_TOOL_CALLS", "true")];

    let plain = run_with_stdin(&dir.0, &["-a", "lister", "hi"], None, &show_tool_calls);
    assert!(plain.status.success(), "{}", combined(&plain));
    assert!(
        String::from_utf8_lossy(&plain.stdout).contains(&format!("Call {TOOL}")),
        "without --json the call log is on stdout:\n{}",
        combined(&plain)
    );

    let output = run_with_stdin(
        &dir.0,
        &["--json", "-a", "lister", "hi"],
        None,
        &show_tool_calls,
    );

    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(report["result"], json!(REPLY), "{report}");
    assert_eq!(report["num_turns"], json!(2), "{report}");
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains(&format!("Call {TOOL}")),
        "under --json the call log is dropped, not moved to stderr:\n{}",
        combined(&output)
    );
}

#[test]
fn an_unknown_rag_fails_before_the_run_without_setting_one_up() {
    let dir = TempDir::new("new-rag");
    write_config(&dir.0, serve(Behaviour::Answer), false);

    let output = run(&dir.0, &["--json", "--rag", "fresh", "hi"]);

    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    assert!(output.stdout.is_empty(), "{}", combined(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Unknown RAG 'fresh'"),
        "{}",
        combined(&output)
    );
    assert!(!dir.0.join("rags").join("fresh.yaml").exists());
}

#[test]
fn a_missing_skill_fails_on_stderr_without_creating_it() {
    let dir = TempDir::new("missing-skill");
    write_config(&dir.0, serve(Behaviour::Answer), false);

    let output = run(&dir.0, &["--json", "--skill", "absent", "hi"]);

    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    assert!(output.stdout.is_empty(), "{}", combined(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Skill 'absent' is not installed"),
        "{}",
        combined(&output)
    );
    assert!(!dir.0.join("skills").join("absent").exists());
}

#[test]
fn loading_an_installed_skill_leaves_stdout_to_the_object() {
    let dir = TempDir::new("skill");
    write_config(&dir.0, serve(Behaviour::Answer), false);
    let skill_dir = dir.0.join("skills").join("helper");
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: Helps.\n---\nBe helpful.\n",
    )
    .unwrap();

    let output = run(&dir.0, &["--json", "--skill", "helper", "hi"]);

    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(the_only_object(&output)["result"], json!(REPLY));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Loaded skill 'helper'"),
        "{}",
        combined(&output)
    );
}

// ---- script-tool relay and the loop warning ----

const SCRIPT_TOOL: &str = "shout";
const RELAYED: &str = "RELAYED-FROM-THE-TOOL";
const LOOP_WARNING: &str = "Tool-call loop detected";

/// An agent whose one script tool prints `RELAYED` to its own stdout (the
/// depth-0 relay carries it) and returns a JSON result through `LLM_OUTPUT`.
fn write_script_tool_agent(dir: &Path) {
    let agent_dir = dir.join("agents").join("shouter");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        "name: shouter\n\
         model: stub:stub-model\n\
         instructions: You shout.\n",
    )
    .unwrap();
    fs::write(
        agent_dir.join("tools.sh"),
        format!(
            "#!/usr/bin/env bash\n\
             set -eo pipefail\n\
             # @env LLM_OUTPUT=/dev/stdout\n\
             # @describe shouter tools\n\
             \n\
             # @cmd Shout once.\n\
             {SCRIPT_TOOL}() {{ echo '{RELAYED}'; printf '{{\"ok\":true}}' >> \"$LLM_OUTPUT\"; }}\n"
        ),
    )
    .unwrap();
}

fn script_tool_run(label: &str, calls: usize, json_flag: bool) -> Output {
    let dir = TempDir::new(label);
    write_config(
        &dir.0,
        serve(Behaviour::CallNamedToolThenAnswer(SCRIPT_TOOL, calls)),
        false,
    );
    write_script_tool_agent(&dir.0);
    let mut args = vec!["-a", "shouter", "hi"];
    if json_flag {
        args.insert(0, "--json");
    }
    run(&dir.0, &args)
}

#[test]
fn a_script_tool_relay_goes_to_stderr_under_json() {
    if cfg!(windows) {
        eprintln!("skipping: the tool is a bash script");
        return;
    }
    let plain = script_tool_run("relay-plain", 1, false);
    assert!(plain.status.success(), "{}", combined(&plain));
    assert!(
        String::from_utf8_lossy(&plain.stdout).contains(RELAYED),
        "without --json the relay is on stdout:\n{}",
        combined(&plain)
    );

    let output = script_tool_run("relay-json", 1, true);
    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(report["result"], json!(REPLY), "{report}");
    assert_eq!(report["num_turns"], json!(2), "{report}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(RELAYED),
        "under --json the relay moves to stderr:\n{}",
        combined(&output)
    );
}

#[test]
fn the_loop_warning_goes_to_stderr_under_json() {
    if cfg!(windows) {
        eprintln!("skipping: the tool is a bash script");
        return;
    }
    let plain = script_tool_run("loop-plain", 4, false);
    assert!(plain.status.success(), "{}", combined(&plain));
    assert!(
        String::from_utf8_lossy(&plain.stdout).contains(LOOP_WARNING),
        "without --json the loop warning is on stdout:\n{}",
        combined(&plain)
    );

    let output = script_tool_run("loop-json", 4, true);
    assert!(output.status.success(), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(report["result"], json!(REPLY), "{report}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(LOOP_WARNING),
        "under --json the loop warning moves to stderr:\n{}",
        combined(&output)
    );
}
