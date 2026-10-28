//! Black-box contract for the `agent__list_sessions` tool through the real
//! binary: a cold config dir, an agent allowed to spawn children, and a
//! loopback chat-completions server that plays the model, answering each
//! request by its position with either a tool call or plain text. The
//! probes read what the binary sent back to the "model" on the next request
//! (the tool result rides along as a `role: tool` message) and what the
//! cache dir looks like before and after, so the tool is judged the way a
//! model and an operator would see it. Every run uses a fresh fake HOME and
//! a scrubbed sandbox/provider env so host state cannot leak in.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{env, process};

use serde_json::{Value, json};

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const AGENT: &str = "lister";
const SESSION: &str = "s";
/// The saved session's child-session identity, written by hand into the
/// session file; its directory is the uuid in simple (32-hex) form.
const UID: &str = "0f4a3c2e-5b6d-4e7f-8a9b-0c1d2e3f4a5b";
const UID_DIR: &str = "0f4a3c2e5b6d4e7f8a9b0c1d2e3f4a5b";
const TOOL: &str = "agent__list_sessions";

fn fresh_config_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-list-sessions-{label}-{}-{unique}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    dir
}

/// One scripted reply: a tool call with these arguments, or plain text.
enum Reply {
    Call(Value),
    Text(&'static str),
}

/// A loopback chat-completions server that answers request `i` with
/// `script[i]` (the last entry repeats past the end) and keeps every
/// request body it saw. One connection per request.
struct ScriptedModel {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

impl ScriptedModel {
    fn playing(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = Arc::clone(&requests);
        let replies: Vec<String> = script
            .iter()
            .enumerate()
            .map(|(index, reply)| {
                let (message, finish) = match reply {
                    Reply::Call(arguments) => (
                        json!({
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": format!("call_{index}"),
                                "type": "function",
                                "function": {
                                    "name": TOOL,
                                    "arguments": arguments.to_string(),
                                }
                            }]
                        }),
                        "tool_calls",
                    ),
                    Reply::Text(text) => (json!({ "role": "assistant", "content": text }), "stop"),
                };
                json!({
                    "id": "stub",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "stub-model",
                    "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
                    "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
                })
                .to_string()
            })
            .collect();
        std::thread::spawn(move || {
            let mut served = 0usize;
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
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&raw[header_end..]).into_owned());
                let reply = &replies[served.min(replies.len() - 1)];
                served += 1;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self { port, requests }
    }

    fn requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|body| serde_json::from_str(body).expect("request body is JSON"))
            .collect()
    }
}

/// A live (non-dry-run) config dir whose only model is the stub and whose
/// one agent may spawn children, so the `agent__*` tools are declared.
/// `save_session` decides whether a turn flushes the session (a save
/// records the parent in the child root under its lock, so a turn that
/// must leave the cache untouched runs with it off).
fn write_fixture(dir: &Path, port: u16, save_session: bool) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: stub:stub-model\n\
             dry_run: false\n\
             save_session: {save_session}\n\
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
             stream: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
    let agent_dir = dir.join("agents").join(AGENT);
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

/// One REPL turn as a macro: the prompt, then (when `save`) `.save session`
/// so the headless macro arm (which saves nothing on its own) leaves the
/// session file.
fn run_turn(dir: &Path, prompt: &str, save: bool) -> Output {
    fs::create_dir_all(dir.join("macros")).unwrap();
    let mut macro_text = format!("isolated: false\nsteps:\n  - \"{prompt}\"\n");
    if save {
        macro_text.push_str("  - \".save session\"\n");
    }
    fs::write(dir.join("macros/turn.yaml"), macro_text).unwrap();
    let home = dir.join("home");
    Command::new(env!("CARGO_BIN_EXE_coyote"))
        .args(["-a", AGENT, "--session", SESSION, "--macro", "turn"])
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
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn combined(output: &Output) -> String {
    format!(
        "exit {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The tool results the binary handed back to the model, in call order,
/// read from the LAST request (which carries the whole exchange).
fn tool_results(requests: &[Value]) -> Vec<Value> {
    let last = requests.last().expect("the model was asked at least once");
    last["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| {
            let content = m["content"].as_str().expect("tool content is a string");
            serde_json::from_str(content).unwrap_or_else(|_| json!({ "text": content }))
        })
        .collect()
}

fn ids(page: &Value) -> Vec<&str> {
    page["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("not a sessions page (a refused call?): {page}"))
        .iter()
        .map(|row| row["session_id"].as_str().unwrap())
        .collect()
}

/// Every path under `root` with its bytes (directories map to `None`), so
/// two snapshots compare by both the entry set and the file contents.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                out.insert(relative, None);
                walk(&path, root, out);
            } else {
                out.insert(relative, Some(fs::read(&path).unwrap_or_default()));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

/// The session file the binary wrote for `-a lister --session s`.
fn session_file(dir: &Path) -> PathBuf {
    fn find(dir: &Path) -> Option<PathBuf> {
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|n| n == "cache" || n == "home")
                {
                    continue;
                }
                if let Some(found) = find(&path) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|n| n == "s.yaml")
                && path.parent().is_some_and(|p| p.ends_with("sessions"))
            {
                return Some(path);
            }
        }
        None
    }
    find(dir).expect("run 0 saved the agent session file")
}

/// A sidecar as the binary writes it (version 1, every field spelled), with
/// `runs` left to the caller: `None` writes a sidecar from before run
/// history was kept, `Some(n)` writes `n` closed runs.
#[allow(clippy::too_many_arguments)]
fn seed_sidecar(
    children: &Path,
    session_id: &str,
    status: &str,
    created_at: &str,
    pid: u32,
    pid_start: u64,
    prompt_head: &str,
    runs: Option<usize>,
) {
    let mut text = format!(
        "version: 1\n\
         session_id: {session_id}\n\
         agent: worker\n\
         status: {status}\n\
         agent_id: agent-{session_id}\n\
         pid: {pid}\n\
         pid_start: {pid_start}\n\
         created_at: {created_at}\n\
         updated_at: {created_at}\n\
         prompt_head: {prompt_head}\n"
    );
    if let Some(runs) = runs {
        text.push_str("runs:\n");
        for run in 0..runs {
            text.push_str(&format!(
                "  - agent_id: agent-{session_id}-run{run}\n    pid: {pid}\n    pid_start: {pid_start}\n    started_at: {created_at}\n    finished_at: {created_at}\n    status: {status}\n"
            ));
        }
    }
    fs::write(children.join(format!("{session_id}.meta.yaml")), text).unwrap();
    // The child's own transcript sits beside the sidecar; the listing must
    // not need to open it.
    fs::write(
        children.join(format!("{session_id}.yaml")),
        "messages: []\n",
    )
    .unwrap();
}

/// A process id no live process carries on any supported platform.
const DEAD_PID: u32 = 4_000_000_000;

const NEWEST: &str = "sess_worker_cccccccccccc";
const MIDDLE: &str = "sess_worker_bbbbbbbbbbbb";
const OLDEST: &str = "sess_worker_aaaaaaaaaaaa";

/// A cold start with no saved session and no spawn: the tool answers the
/// empty page with its note and the cache keeps no `sessions/` directory,
/// so listing never mints a child-session root.
#[test]
fn list_sessions_on_a_cold_start_is_the_empty_page_and_mints_no_root() {
    let dir = fresh_config_dir("cold");
    let _guard = TempDirGuard(dir.clone());
    let model = ScriptedModel::playing(vec![Reply::Call(json!({})), Reply::Text("done")]);
    write_fixture(&dir, model.port, false);

    let output = run_turn(&dir, "what children do I have", false);
    assert!(output.status.success(), "{}", combined(&output));

    let requests = model.requests();
    assert_eq!(requests.len(), 2, "one tool round then the answer");
    let declared = requests[0]["tools"]
        .as_array()
        .expect("the first request declares tools")
        .iter()
        .any(|t| t["function"]["name"] == TOOL);
    assert!(declared, "the agent declares {TOOL} to the model");

    let results = tool_results(&requests);
    assert_eq!(results.len(), 1, "{results:?}");
    let page = &results[0];
    assert_eq!(page["sessions"], json!([]), "{page}");
    assert_eq!(page["total"], json!(0), "{page}");
    assert_eq!(page["next_cursor"], Value::Null, "{page}");
    let note = page["note"].as_str().unwrap_or_default();
    assert!(
        note.contains("No child sessions yet") && note.is_ascii(),
        "the empty page carries the no-root note: {page}"
    );
    assert!(
        !dir.join("cache").join("sessions").exists(),
        "listing minted a child-session root: {:?}",
        snapshot(&dir.join("cache")).keys().collect::<Vec<_>>()
    );
}

/// A saved session whose uid directory already holds children lists them
/// after a cold restart with no spawn in this process: newest first by
/// `created_at`, a dead `running` sidecar shown as `crashed`, `runs` as
/// recorded (0 for a sidecar from before run history), the documented
/// shape on every row; five identical calls in one turn all answer (the
/// tool is exempt from loop detection), `page_size: 2` pages with the
/// `cursor` naming the row to continue after; and the cache directory is
/// byte-for-byte what it was before the turn.
#[test]
fn list_sessions_through_the_binary_lists_a_saved_sessions_children_read_only() {
    let dir = fresh_config_dir("saved");
    let _guard = TempDirGuard(dir.clone());
    // Run 0: let the binary mint the session file with a plain answer.
    let mint = ScriptedModel::playing(vec![Reply::Text("hello")]);
    write_fixture(&dir, mint.port, true);
    let output = run_turn(&dir, "hi", true);
    assert!(output.status.success(), "{}", combined(&output));
    let session = session_file(&dir);
    let mut text = fs::read_to_string(&session).unwrap();
    assert!(!text.contains("uid:"), "a plain turn mints no uid: {text}");
    // The minted file carries the knob it was saved under; a session that
    // says `save_session: true` flushes after every turn (and a flush
    // records the parent in the child root under its lock). Dropping the
    // line leaves the knob to the config, which the turns below set off,
    // so nothing but the listing can reach the root.
    assert!(
        text.contains("save_session: true\n"),
        "run 0 saved the session under save_session: true: {text}"
    );
    text = text.replace("save_session: true\n", "");
    text.push_str(&format!("uid: {UID}\n"));
    fs::write(&session, text).unwrap();

    // The uid root as a spawner would have left it after the parent's last
    // save: an identity file naming this session file as a parent (so the
    // start-up sweep keeps the dir) with a `last_saved_at` later than every
    // child, and three children, seeded out of order on disk.
    let root = dir.join("cache").join("sessions").join(UID_DIR);
    let children = root.join("children");
    fs::create_dir_all(&children).unwrap();
    let parent = fs::canonicalize(&session).unwrap();
    fs::write(
        root.join("root.yaml"),
        format!(
            "version: 1\ncreated_at: 2026-01-01T00:00:00Z\nparent_paths:\n  - '{}'\nlast_pid: {DEAD_PID}\nlast_pid_start: 1\nlast_saved_at: 2026-01-01T00:10:00Z\n",
            parent.display()
        ),
    )
    .unwrap();
    seed_sidecar(
        &children,
        MIDDLE,
        "completed",
        "2026-01-01T00:02:00Z",
        DEAD_PID,
        1,
        "middle task",
        Some(2),
    );
    seed_sidecar(
        &children,
        NEWEST,
        "running",
        "2026-01-01T00:03:00Z",
        DEAD_PID,
        1,
        "newest task",
        Some(1),
    );
    seed_sidecar(
        &children,
        OLDEST,
        "completed",
        "2026-01-01T00:01:00Z",
        DEAD_PID,
        1,
        "oldest task",
        None,
    );
    let seeded = snapshot(&dir.join("cache"));

    // Control turn: the same session, no tool call. A plain turn on a
    // session that is not flushing leaves the seeded root exactly as it
    // was, which is the baseline the listing turn below is held to.
    let control = ScriptedModel::playing(vec![Reply::Text("hello again")]);
    write_fixture(&dir, control.port, false);
    let output = run_turn(&dir, "hi again", false);
    assert!(output.status.success(), "{}", combined(&output));
    assert_eq!(control.requests().len(), 1, "the control turn asks once");
    let before = snapshot(&dir.join("cache"));
    let sessions_dir = PathBuf::from("sessions");
    let under_sessions = |snap: &BTreeMap<PathBuf, Option<Vec<u8>>>| {
        snap.iter()
            .filter(|(path, _)| path.starts_with(&sessions_dir))
            .map(|(path, bytes)| (path.clone(), bytes.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    let log = fs::read_to_string(dir.join("cache").join("coyote.log")).unwrap_or_default();
    assert_eq!(
        under_sessions(&seeded),
        under_sessions(&before),
        "a plain turn changed the seeded root; the binary's log says:\n{}",
        log.lines()
            .filter(|l| l.contains(UID_DIR) || l.contains("child session"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        !log.contains("Skipping unreadable child session sidecar"),
        "the seeded sidecars must all parse: {log}"
    );

    // Listing turn: five identical calls, then two paged calls, then the
    // answer. The cache dir afterwards must match the control turn's end
    // state byte for byte, apart from the operator log, which every run
    // appends to; in particular no root lock file appears.
    let mut script: Vec<Reply> = (0..5).map(|_| Reply::Call(json!({}))).collect();
    script.push(Reply::Call(json!({ "page_size": 2 })));
    script.push(Reply::Call(json!({ "page_size": 2, "cursor": MIDDLE })));
    script.push(Reply::Text("done"));
    let model = ScriptedModel::playing(script);
    write_fixture(&dir, model.port, false);
    let output = run_turn(&dir, "list my children", false);
    assert!(output.status.success(), "{}", combined(&output));

    let requests = model.requests();
    assert_eq!(requests.len(), 8, "seven tool rounds then the answer");
    let results = tool_results(&requests);
    assert_eq!(results.len(), 7, "{results:?}");

    for (round, page) in results[..5].iter().enumerate() {
        assert_eq!(
            ids(page),
            vec![NEWEST, MIDDLE, OLDEST],
            "round {round}: newest first by created_at: {page}"
        );
        assert_eq!(page["total"], json!(3), "round {round}: {page}");
        assert_eq!(page["next_cursor"], Value::Null, "round {round}: {page}");
        assert!(
            page.get("note").is_none(),
            "round {round}: no note on a full page: {page}"
        );
    }
    let first = &results[0];
    let rows = first["sessions"].as_array().unwrap();
    let keys: Vec<Vec<&str>> = rows
        .iter()
        .map(|row| {
            let mut keys: Vec<&str> = row
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            keys
        })
        .collect();
    let documented = vec![
        "agent",
        "created_at",
        "prompt_head",
        "runs",
        "session_id",
        "status",
        "updated_at",
    ];
    assert!(
        keys.iter().all(|k| *k == documented),
        "every row carries exactly the documented keys: {keys:?}"
    );
    assert_eq!(rows[0]["status"], json!("crashed"), "{first}");
    assert_eq!(rows[1]["status"], json!("completed"), "{first}");
    assert_eq!(rows[2]["status"], json!("completed"), "{first}");
    assert_eq!(rows[0]["runs"], json!(1), "{first}");
    assert_eq!(rows[1]["runs"], json!(2), "{first}");
    assert_eq!(
        rows[2]["runs"],
        json!(0),
        "an old sidecar lists 0 runs: {first}"
    );
    assert_eq!(rows[0]["agent"], json!("worker"), "{first}");
    assert_eq!(rows[0]["prompt_head"], json!("newest task"), "{first}");
    assert_eq!(
        rows[0]["created_at"],
        json!("2026-01-01T00:03:00+00:00"),
        "{first}"
    );

    let page_one = &results[5];
    assert_eq!(ids(page_one), vec![NEWEST, MIDDLE], "{page_one}");
    assert_eq!(page_one["total"], json!(3), "{page_one}");
    assert_eq!(page_one["next_cursor"], json!(MIDDLE), "{page_one}");
    let page_two = &results[6];
    assert_eq!(ids(page_two), vec![OLDEST], "{page_two}");
    assert_eq!(page_two["total"], json!(3), "{page_two}");
    assert_eq!(page_two["next_cursor"], Value::Null, "{page_two}");

    let after = snapshot(&dir.join("cache"));
    let log = fs::read_to_string(dir.join("cache").join("coyote.log")).unwrap_or_default();
    let root_lines: Vec<&str> = log
        .lines()
        .filter(|l| l.contains(UID_DIR) || l.contains("child session"))
        .collect();
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "listing added or removed an entry under the cache dir; the binary's log says:\n{}",
        root_lines.join("\n")
    );
    for (path, bytes) in &before {
        if path == Path::new("coyote.log") {
            continue;
        }
        assert_eq!(bytes, &after[path], "listing rewrote {}", path.display());
    }
    assert!(
        !dir.join("cache")
            .join("sessions")
            .join(UID_DIR)
            .join(".lock")
            .exists(),
        "listing took the root lock (no bookkeeping path ran in this turn): {}",
        root_lines.join("\n")
    );
    assert!(
        !log.contains("Skipping unreadable child session sidecar"),
        "{log}"
    );
}
