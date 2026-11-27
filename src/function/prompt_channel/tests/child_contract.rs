use super::*;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

fn bash_child(server: &Server, script: &str) -> Child {
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(script)
        .env(REPLY_ENV, server.env_value())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    server.configure(&mut cmd);
    cmd.spawn().expect("spawn bash")
}

/// Waits for the child with a deadline so a regression fails instead
/// of hanging the suite.
fn wait_with_deadline(mut child: Child) -> std::process::Output {
    let (tx, rx) = mpsc::channel();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    thread::spawn(move || {
        let mut out = Vec::new();
        let _ = stdout.read_to_end(&mut out);
        let mut err = Vec::new();
        let _ = stderr.read_to_end(&mut err);
        let _ = tx.send((out, err));
    });
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("bash child did not finish within {WAIT:?}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let (stdout, stderr) = rx.recv_timeout(WAIT).expect("child pipes drained");
    std::process::Output {
        status,
        stdout,
        stderr,
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn helper_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/functions/utils/prompt-utils.sh")
}

/// A `tput` that answers the two size queries the helper makes without
/// a terminfo lookup, so the child does not depend on the host's TERM.
fn fixture_bin() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("coyote-child-contract-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let tput = dir.join("tput");
    std::fs::write(
        &tput,
        "#!/usr/bin/env sh\ncase \"$1\" in\n  cols) echo 100 ;;\n  lines) echo 40 ;;\n  *) exit 1 ;;\nesac\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tput, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

fn helper_env(cmd: &mut Command) {
    let path = std::env::var("PATH").unwrap_or_default();
    cmd.env("PATH", format!("{}:{path}", fixture_bin().display()))
        .env("TERM", "xterm-256color")
        .env_remove("COYOTE_UI")
        .env_remove(OUT_FD_ENV);
}

// The env value is a path the child can open; unix spells it /dev/fd/N
// with N clear of the std fds and the helper's saved stdout (fd 9).
#[test]
fn env_value_is_a_dev_fd_path_clear_of_the_low_range() {
    let server = Server::open().unwrap();
    let value = server.env_value();
    let n: i32 = value
        .strip_prefix("/dev/fd/")
        .unwrap_or_else(|| panic!("env value {value:?} is not /dev/fd/N"))
        .parse()
        .unwrap();
    assert!(
        n >= 10,
        "{value}: fd must sit above 0-2 and the helper's fd 9"
    );
}

// A bash `read` blocked on the path is woken by a reply written
// after it blocked, and sees exactly one JSON line.
#[test]
fn a_reply_written_after_the_child_blocks_reaches_its_read() {
    let mut server = Server::open().unwrap();
    let child = bash_child(
        &server,
        r#"IFS= read -r line <"$COYOTE_PROMPT_REPLY"; rc=$?; printf '%s' "$line"; exit $rc"#,
    );
    server.spawned();
    thread::sleep(Duration::from_millis(250));
    server.reply("p1", &json!(true));
    let out = wait_with_deadline(child);
    assert!(out.status.success(), "stderr: {}", text(&out.stderr));
    let got: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("not one JSON object: {e}: {:?}", text(&out.stdout)));
    assert_eq!(got["id"], json!("p1"));
    assert_eq!(got["answer"], json!(true));
    assert!(
        !out.stdout.contains(&b'\n'),
        "read -r must strip the single newline"
    );
}

// A reply queued before the child ever opens the path is delivered
// once it does.
#[test]
fn a_reply_queued_before_the_child_opens_is_delivered() {
    let mut server = Server::open().unwrap();
    server.reply("p1", &json!(["a", "c"]));
    let child = bash_child(
        &server,
        r#"sleep 0.2; IFS= read -r line <"$COYOTE_PROMPT_REPLY"; printf '%s' "$line""#,
    );
    server.spawned();
    let out = wait_with_deadline(child);
    assert!(out.status.success(), "stderr: {}", text(&out.stderr));
    let got: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(got, json!({"id": "p1", "answer": ["a", "c"]}));
}

// Dropping the server while the sourced helper waits for an answer
// releases it: `read` sees EOF, the helper reports the closed channel and
// the tool gets return code 1 (the helper's own documented contract).
#[test]
fn dropping_the_server_releases_a_helper_blocked_on_its_answer() {
    let mut server = Server::open().unwrap();
    let script = format!(
        r#"source "{}"; confirm "Go?"; rc=$?; printf 'rc=%s\n' "$rc" >&2; exit $rc"#,
        helper_path().display()
    );
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(&script)
        .env(REPLY_ENV, server.env_value())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    helper_env(&mut cmd);
    server.configure(&mut cmd);
    let mut child = cmd.spawn().expect("spawn bash");
    server.spawned();

    // The request line on the child's stdout proves it is now inside
    // the reply read.
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest);
    });
    let request = rx.recv_timeout(WAIT).expect("request line");
    assert!(
        request.starts_with(r#"{"coyote_prompt":{"id":"#),
        "request line: {request:?}"
    );
    thread::sleep(Duration::from_millis(150));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the helper must still be waiting before the server goes away"
    );

    drop(server);

    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("dropping the server did not release the blocked helper");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    let stderr = text(&stderr);
    assert_eq!(status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("prompt reply channel closed"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("rc=1"), "stderr: {stderr}");
}

// One tool process asks twice; each `confirm` gets its own answer by
// id (the helper reopens the path per prompt), so a true then a false
// print `1` then `0` (the helper's documented values). The request
// lines travel on the helper's saved stdout (fd 9), so a command
// substitution captures only the answer, the way a tool uses it.
#[test]
fn two_prompts_in_one_child_each_receive_their_own_answer() {
    let mut server = Server::open().unwrap();
    let script = format!(
        r#"source "{}"; a=$(confirm "First?"); b=$(confirm "Second?"); printf 'a=%s b=%s\n' "$a" "$b" >&2"#,
        helper_path().display()
    );
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(&script)
        .env(REPLY_ENV, server.env_value())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    helper_env(&mut cmd);
    server.configure(&mut cmd);
    let mut child = cmd.spawn().expect("spawn bash");
    server.spawned();

    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut ids = Vec::new();
    for (n, answer) in [true, false].into_iter().enumerate() {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(line.trim_end())
            .unwrap_or_else(|e| panic!("request {n}: {e}: {line:?}"));
        let id = request["coyote_prompt"]["id"].as_str().unwrap().to_owned();
        assert_eq!(request["coyote_prompt"]["action"], json!("confirm"));
        ids.push(id.clone());
        server.reply(&id, &json!(answer));
    }
    assert_ne!(ids[0], ids[1], "each prompt carries a fresh id");

    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "child did not finish");
        thread::sleep(Duration::from_millis(10));
    };
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    let stderr = text(&stderr);
    assert!(status.success(), "stderr: {stderr}");
    assert!(stderr.contains("a=1 b=0"), "stderr: {stderr}");
}

// A non-ASCII answer crosses the pipe byte-identical.
#[test]
fn a_non_ascii_answer_round_trips_byte_identical() {
    let mut server = Server::open().unwrap();
    let child = bash_child(
        &server,
        r#"IFS= read -r line <"$COYOTE_PROMPT_REPLY"; printf '%s' "$line""#,
    );
    server.spawned();
    let answer = "caf\u{e9} \u{2713} \u{1F389} \\ \"q\"";
    server.reply("p1", &json!(answer));
    let out = wait_with_deadline(child);
    assert!(out.status.success(), "stderr: {}", text(&out.stderr));
    let got: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(got["answer"].as_str(), Some(answer));
}

// A server nobody connected to, and a reply after the tool is gone,
// are both quiet (no block, no panic): the writer must not take the
// pump down with it.
#[test]
fn an_unused_server_and_a_reply_after_exit_are_quiet() {
    let server = Server::open().unwrap();
    drop(server);

    let mut server = Server::open().unwrap();
    let child = bash_child(&server, "exit 0");
    server.spawned();
    let out = wait_with_deadline(child);
    assert!(out.status.success());
    // The read end died with the child; this write hits a broken pipe.
    server.reply("p1", &json!(true));
    server.reply("p2", &json!("still quiet"));
}

// The reply never lands on the child's stdout stream; the only
// bytes there are the child's own.
#[test]
fn the_reply_does_not_leak_into_the_child_stdout() {
    let mut server = Server::open().unwrap();
    let child = bash_child(
        &server,
        r#"IFS= read -r line <"$COYOTE_PROMPT_REPLY"; printf 'own-output\n'"#,
    );
    server.spawned();
    server.reply("p1", &json!(false));
    let out = wait_with_deadline(child);
    assert_eq!(text(&out.stdout), "own-output\n");
    assert!(out.stderr.is_empty(), "stderr: {}", text(&out.stderr));
}
