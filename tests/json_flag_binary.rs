//! Black-box contract for `--json` on the plain one-shot path: a cold config
//! dir and a loopback chat-completions server that plays the model. Each
//! probe reads the binary's stdout as the caller of `--json` would, so a
//! stray byte of rendered markdown or a second object fails the parse.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process};

use serde_json::{Value, json};

const REPLY: &str = "**hello** from the stub";

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
}

/// A loopback chat-completions server that answers every request the same
/// way: `REPLY` as a plain or SSE completion (matching the request's
/// `stream` field), or an HTTP 500. One connection per request.
fn serve(behaviour: Behaviour) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Some(body) = read_request_body(&mut stream) else {
                continue;
            };
            let streaming = serde_json::from_str::<Value>(&body)
                .is_ok_and(|request| request["stream"] == json!(true));
            let (status, content_type, payload) = match behaviour {
                Behaviour::ServerError => (
                    "500 Internal Server Error",
                    "application/json",
                    json!({ "error": { "message": "stub exploded", "type": "server_error" } })
                        .to_string(),
                ),
                Behaviour::Answer if streaming => ("200 OK", "text/event-stream", sse_completion()),
                Behaviour::Answer => ("200 OK", "application/json", plain_completion()),
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
             save: false\n\
             memory: false\n\
             stream: {stream}\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

fn run(dir: &Path, args: &[&str]) -> Output {
    run_with_stdin(dir, args, None)
}

fn run_with_stdin(dir: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let home = dir.join("home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(args)
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        // Loopback must not be sent through a host proxy.
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("COYOTE_LOG_PATH")
        .env_remove("COYOTE_WORKSPACE_CONFIG_DIR")
        .env_remove("IS_SANDBOX")
        .env_remove("COYOTE_PROVIDER")
        .env_remove("COYOTE_PLATFORM")
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
    child.wait_with_output().unwrap()
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

#[test]
fn a_non_streamed_reply_is_the_only_stdout_and_skips_markdown() {
    let output = json_run("plain", Behaviour::Answer, false, &["--json", "hi"]);

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
    assert!(!output.stderr.is_empty(), "{}", combined(&output));
}

#[test]
fn a_failed_model_call_prints_the_error_wrapper_and_exits_1() {
    let output = json_run("500", Behaviour::ServerError, false, &["--json", "hi"]);

    assert_eq!(output.status.code(), Some(1), "{}", combined(&output));
    let report = the_only_object(&output);
    assert_eq!(report["is_error"], json!(true), "{report}");
    assert_eq!(report["result"], Value::Null, "{report}");
    let error = report["error"].as_str().unwrap_or_default();
    assert!(!error.is_empty(), "{report}");
    assert_eq!(report["model"], json!("stub:stub-model"), "{report}");
    assert!(!output.stderr.is_empty(), "{}", combined(&output));
}

// ---- usage-probe additions (spec-first, TASK-431 D12 / R1-10) ----

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
    // The stub model has no pricing, so the cost cannot be computed (R1-10).
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
    let output = run_with_stdin(&dir.0, &["--json"], Some("hi from stdin\n"));

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
    let keys: Vec<&str> = report
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, KEYS, "{report}");
    assert!(report["num_turns"].as_u64().unwrap_or(0) >= 1, "{report}");
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
    ];
    for flag in flags {
        let mut args = vec!["--json"];
        args.extend_from_slice(flag);
        args.push("hi");
        let output = run(&dir.0, &args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "{flag:?}\n{}",
            combined(&output)
        );
        assert!(output.stdout.is_empty(), "{flag:?}\n{}", combined(&output));
    }
    assert!(!dir.0.join("COYOTE.md").exists(), "--init-instructions ran");
}
