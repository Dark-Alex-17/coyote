use super::contract::{WAIT, read_line_bytewise, reply_line};
use super::*;
use crate::function::prompt_channel::{MARKER, OUT_FD_ENV, REPLY_ENV};
use std::fs::File;
use std::io::{self, BufRead, Read};
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const HELPER: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/functions/utils/prompt-utils.sh"
);

/// Opens the pipe and reads one line on a helper thread.
fn read_one_line(path: &str) -> mpsc::Receiver<io::Result<Option<String>>> {
    let path = path.to_owned();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = File::open(&path).and_then(|mut file| read_line_bytewise(&mut file));
        tx.send(result).unwrap();
    });
    rx
}

#[test]
fn env_value_names_a_local_pipe_unique_per_server() {
    let first = Server::open().unwrap();
    let second = Server::open().unwrap();
    for server in [&first, &second] {
        let path = server.env_value();
        assert!(
            path.starts_with(r"\\.\pipe\coyote-prompt-"),
            "unexpected pipe path {path}"
        );
    }
    assert_ne!(first.env_value(), second.env_value());
}

/// bash's `read <"$path"` opens the pipe anew for every line: a client
/// that takes one line and leaves must not swallow the next one, which
/// has to reach the client that opens the pipe after it.
#[test]
fn each_reopen_of_the_pipe_receives_the_next_queued_line() {
    let server = Server::open().unwrap();
    server.reply("stray", &json!(null));
    server.reply("mine", &json!(false));
    let first = read_one_line(&server.env_value())
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(first, Some(reply_line("stray", &json!(null))));
    let second = read_one_line(&server.env_value())
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(second, Some(reply_line("mine", &json!(false))));
}

/// Two prompts in a row, as a tool makes them: the first client takes
/// its line and leaves with the queue empty, so the delivery thread
/// is parked on a connection nobody reads any more; the next client
/// opens the instance created ahead of it and waits; only then is the
/// second line written. It has to reach that waiting client: the write
/// to the dead connection fails, the thread moves on to the instance
/// the client already holds, and `ConnectNamedPipe` reports it as
/// connected.
#[test]
fn a_line_written_while_the_next_client_waits_reaches_it() {
    let server = Server::open().unwrap();
    server.reply("a", &json!(1));
    let first = read_one_line(&server.env_value())
        .recv_timeout(WAIT)
        .unwrap()
        .unwrap();
    assert_eq!(first, Some(reply_line("a", &json!(1))));

    let rx = read_one_line(&server.env_value());
    thread::sleep(Duration::from_millis(200));
    server.reply("b", &json!(2));
    let second = rx.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(second, Some(reply_line("b", &json!(2))));
}

/// A client that stays connected across lines (the python and
/// typescript helpers skip stray replies on one open) sees them in
/// order on the same connection.
#[test]
fn one_connection_receives_successive_lines() {
    let server = Server::open().unwrap();
    let path = server.env_value();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut file = File::open(&path).unwrap();
        let first = read_line_bytewise(&mut file);
        let second = read_line_bytewise(&mut file);
        tx.send((first, second)).unwrap();
    });
    thread::sleep(Duration::from_millis(200));
    server.reply("a", &json!(1));
    server.reply("b", &json!(2));
    let (first, second) = rx.recv_timeout(WAIT).unwrap();
    assert_eq!(first.unwrap(), Some(reply_line("a", &json!(1))));
    assert_eq!(second.unwrap(), Some(reply_line("b", &json!(2))));
}

/// Dropping the server with lines nobody collected must end the
/// delivery thread: afterwards the path no longer opens.
#[test]
fn dropping_the_server_with_pending_lines_closes_the_pipe() {
    let server = Server::open().unwrap();
    let path = server.env_value();
    server.reply("unread", &json!(true));
    server.reply("also-unread", &json!(true));
    drop(server);
    let deadline = Instant::now() + WAIT;
    loop {
        match File::open(&path) {
            Err(_) => break,
            Ok(_) if Instant::now() > deadline => panic!("pipe still open after drop"),
            Ok(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// The first instance claimed afresh: a client still holding a
/// previous instance keeps the name alive, so claiming it is denied
/// until that client lets go. The acceptor outwaits it instead of
/// giving the channel up, and the next client still gets its line.
/// The scenario is built by hand: the server always starts with an
/// instance in hand.
#[test]
fn a_lingering_client_only_delays_the_replacement_instance() {
    let path = format!(
        r"\\.\pipe\coyote-prompt-test-{}",
        uuid::Uuid::new_v4().simple()
    );
    let name = win::wide(&path);
    let descriptor = Arc::new(win::OwnerOnlyDescriptor::new().unwrap());
    let previous = win::create_instance(&name, true, &descriptor).unwrap();
    let lingering = File::open(&path).unwrap();
    drop(previous);
    let shared = Arc::new(Shared::new());
    start_threads(&name, descriptor, None, &shared).unwrap();
    let server = Server {
        name,
        path: path.clone(),
        shared,
    };
    server.reply("late", &json!(true));
    thread::sleep(Duration::from_millis(300));
    assert!(
        !server.shared.delivery.lock().unwrap().closed,
        "the thread gave the channel up while a client lingered"
    );
    drop(lingering);

    let deadline = Instant::now() + WAIT;
    let mut next = loop {
        match File::open(&path) {
            Ok(file) => break file,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Err(err) => panic!("no replacement instance appeared: {err}"),
        }
    };
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || tx.send(read_line_bytewise(&mut next)).unwrap());
    let line = rx.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(line, Some(reply_line("late", &json!(true))));
}

/// Git Bash may open the path once to probe it and close it again
/// before it opens it to read, so clients that come and go with no
/// line queued must never use the listening instances up: every open
/// that follows still finds one, and a line written afterwards reaches
/// the client that waits for it. Each open waits until the previous
/// client is accepted (which is counted only once its replacement
/// listens), so the sequence is the same on every run; a server that
/// stops listening while it waits for a line refuses the third open.
#[test]
fn clients_that_open_and_leave_at_once_never_find_the_pipe_busy() {
    let server = Server::open().unwrap();
    let path = server.env_value();
    for round in 1..=20 {
        let probe = File::open(&path)
            .unwrap_or_else(|err| panic!("open {round} found no listening instance: {err}"));
        drop(probe);
        let deadline = Instant::now() + WAIT;
        while server.shared.delivery.lock().unwrap().accepted < round {
            assert!(Instant::now() < deadline, "open {round} was never accepted");
            thread::sleep(Duration::from_millis(5));
        }
    }
    let rx = read_one_line(&path);
    thread::sleep(Duration::from_millis(200));
    server.reply("after-probes", &json!(true));
    let line = rx.recv_timeout(WAIT).unwrap().unwrap();
    assert_eq!(line, Some(reply_line("after-probes", &json!(true))));
}

/// The bash helper end to end under Git Bash: the pipe is opened by its
/// Windows name, each `read` opens it anew, and the second prompt is
/// answered after the first open has gone away.
/// Request lines are answered from the stdout pump the way the tool
/// runner answers them; a stall kills the shell so a regression fails
/// the test instead of holding the run. Both tools are asserted, as
/// the unix contract cases do: a silent skip would hide the harness.
#[test]
fn git_bash_confirms_twice_through_the_named_pipe() {
    let bash = crate::testing::bash_program()
        .expect("bash must resolve on windows; a silent skip would hide the harness");
    assert!(
        which::which("jq").is_ok(),
        "jq must resolve on windows; a silent skip would hide the harness"
    );
    let mut server = Server::open().unwrap();
    let mut cmd = Command::new(bash);
    cmd.arg("-c")
        .arg(concat!(
            "source \"$PROMPT_UTILS\"; ",
            "a=\"$(confirm \"First? e2e-x7x\")\"; echo \"first=$a\"; ",
            "b=\"$(confirm \"Second? e2e-x7x\")\"; echo \"second=$b\""
        ))
        .env_remove("AUTO_CONFIRM")
        .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
        .env_remove("COYOTE_UI")
        .env_remove(OUT_FD_ENV)
        .env(REPLY_ENV, server.env_value())
        .env("TERM", "xterm-256color")
        .env("PROMPT_UTILS", HELPER.replace('\\', "/"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    server.configure(&mut cmd);
    let mut child = cmd.spawn().expect("spawn bash");
    server.spawned();
    let mut stderr = child.stderr.take().unwrap();
    let stderr_thread = thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).unwrap();
        text
    });
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut answers = [json!(true), json!(false)].into_iter();
        let mut printed = Vec::new();
        for line in io::BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if line.trim_start().as_bytes().starts_with(MARKER) {
                let req: Value = serde_json::from_str(&line).unwrap();
                let id = req["coyote_prompt"]["id"].as_str().unwrap();
                server.reply(id, &answers.next().unwrap_or(Value::Null));
            } else {
                printed.push(line);
            }
        }
        tx.send(printed).unwrap();
    });
    let printed = match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(printed) => printed,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            panic!("bash did not get through both prompts in time");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = child.kill();
            panic!("the stdout pump died before bash finished");
        }
    };
    let status = child.wait().unwrap();
    let stderr = stderr_thread.join().unwrap();
    assert!(status.success(), "{status}: {stderr}");
    assert_eq!(printed, ["first=1", "second=0"], "{stderr}");
    assert_eq!(stderr, "");
}
