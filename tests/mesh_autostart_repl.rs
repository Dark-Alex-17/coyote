//! Black-box coverage of the mesh autostart through the interactive REPL:
//! `mesh.enabled: true` joins the mesh when the REPL starts, before the first
//! prompt, without asking, and again when a session is entered with `.session`
//! or `.agent <name> <session>` unless `.mesh off` was typed in this run;
//! `.exit session` and `.exit agent` with a session held leave the mesh; a
//! failed `.mesh on` precondition prints that refusal and leaves the mesh off
//! with a usable prompt; `mesh.enabled: false` never
//! mentions joining; an interface that connects after the first prompt prints
//! its idle line and leaves the prompt intact. The one-shot side (`--macro`
//! never autostarts) lives in `macro_bracketing.rs`.
//!
//! Drives the real `coyote` binary through a pty like `envoy_repl.rs`, against
//! a private-relay interface pointing at a loopback listener this test owns
//! (accepted and held silent), so nothing leaves the machine and no fixed
//! port is taken. Every wait is an `expect` bounded by `EXPECT_TIMEOUT`.
//!
//! **Unix only.** expectrl has a Windows backend (ConPTY), but this harness
//! drives a raw pty: it sizes the window through ptyprocess via
//! `get_process_mut()`, answers the cursor position handshake itself, and
//! asserts on the raw VT byte stream reedline paints. ConPTY re-renders that
//! stream through its own emulator, so those assertions would not transfer.
//! The dev-dependency is declared under `[target.'cfg(unix)'.dev-dependencies]`
//! and this file is compiled out on Windows.

#![cfg(unix)]

use std::env;
use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use expectrl::process::unix::WaitStatus;
use expectrl::session::{OsProcess, OsStream};
use expectrl::{Any, Eof, Expect, Regex, Session};
use tokio::net::TcpSocket;

/// The binary is a cold debug build that opens a vault, resolves a model and,
/// here, starts a Reticulum transport before its first prompt.
const EXPECT_TIMEOUT: Duration = Duration::from_secs(60);

const TERMINAL_COLUMNS: u16 = 160;
const TERMINAL_ROWS: u16 = 50;

const CURSOR_POSITION_QUERY: &str = r"\x1b\[6n";
/// Reply to a query that no column move preceded; see
/// [`cursor_position_reply`] for the queries that follow one.
const CURSOR_POSITION_REPLY: &str = "\x1b[2;1R";

/// Prompt tails, as regexes: reedline colours the pieces separately. With a
/// session the left prompt reads `dry-model)probe)`, without one `dry-model)>`.
const SESSION_PROMPT: &str = r"dry-model\)(\x1b\[[0-9;]*m)*probe(\x1b\[[0-9;]*m)*\)";
const BARE_PROMPT: &str = r"dry-model\)(\x1b\[[0-9;]*m)*>";

/// `SESSION_PROMPT` for a session entered with `.session <name>`.
fn session_prompt(name: &str) -> String {
    format!(r"dry-model\)(\x1b\[[0-9;]*m)*{name}(\x1b\[[0-9;]*m)*\)")
}

/// The left prompt inside agent `agent` with session `name` held: `dry-model)plain>a)`.
fn agent_session_prompt(agent: &str, name: &str) -> String {
    format!(r"dry-model\)(\x1b\[[0-9;]*m)*{agent}>(\x1b\[[0-9;]*m)*{name}(\x1b\[[0-9;]*m)*\)")
}

/// The left prompt inside agent `agent` with no session held: `dry-model)plain>>`.
fn agent_prompt(agent: &str) -> String {
    format!(r"dry-model\)(\x1b\[[0-9;]*m)*{agent}>(\x1b\[[0-9;]*m)*>")
}

/// The one line `mesh::autostart` adds over `.mesh on --yes`.
const AUTOSTART_NOTICE: &str =
    "mesh.enabled is true in config.yaml: joining the mesh for this session";
const MESH_ON_SUMMARY: &str = "Mesh is on for this session";
/// The clause a typed `.mesh off` adds under `mesh.enabled: true`.
const STAYS_OFF: &str = "it stays off for the rest of this run";
const STAYS_OFF_PATTERN: &str = r"it stays off for the rest of this run";
/// What `.exit session` prints while the node runs.
const LEAVING: &str =
    "Leaving the mesh: the session that owns this node's instance id is being exited.";
const LEAVING_PATTERN: &str =
    r"Leaving the mesh: the session that owns this node's instance id is being exited\.";
const NEEDS_SESSION: &str = "Mesh needs a session";
const MESH_OFF: &str = "Mesh is off. Run `.mesh on` first.";
/// `MESH_OFF` as a regex, for waiting on it.
const MESH_OFF_PATTERN: &str = r"Mesh is off\. Run `\.mesh on` first\.";
/// What `.mesh peers` says on a node that is on but has heard nobody.
const NO_PEERS_PATTERN: &str = r"No peers heard yet\.";

type Target = Session<OsProcess, OsStream>;

fn fresh_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!("coyote-mesh-autostart-{label}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A loopback TCP listener that accepts every connection and holds it open,
/// silently, so a `private` interface has something to connect to.
struct SilentRelay {
    port: u16,
}

impl SilentRelay {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        hold_connections(listener);
        Self { port }
    }
}

/// Accepts every connection on `listener` and holds it open, silently.
fn hold_connections(listener: TcpListener) {
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut stream = stream;
                let mut sink = [0u8; 1024];
                while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
            });
        }
    });
}

/// A loopback port that is bound but not listening, so every connection
/// attempt is refused at once and the node keeps retrying it; `open` turns it
/// into the same silent sink as `SilentRelay`.
struct HeldPort {
    port: u16,
    socket: Option<TcpSocket>,
}

impl HeldPort {
    fn hold() -> Self {
        let socket = TcpSocket::new_v4().unwrap();
        socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = socket.local_addr().unwrap().port();
        Self {
            port,
            socket: Some(socket),
        }
    }

    /// `TcpSocket::listen` registers the listener with a reactor, so it runs
    /// on a throwaway runtime; the std listener it hands back needs none.
    fn open(&mut self) {
        let socket = self.socket.take().expect("a held port opens once");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let listener = runtime
            .block_on(async { socket.listen(16).unwrap().into_std() })
            .unwrap();
        listener.set_nonblocking(false).unwrap();
        hold_connections(listener);
    }
}

struct Probe {
    config_dir: PathBuf,
    home_dir: PathBuf,
    temp_root: PathBuf,
}

impl Probe {
    fn new(label: &str, mesh_enabled: bool, relay_ports: &[u16]) -> Self {
        let config_dir = fresh_dir(&format!("{label}-cfg"));
        let home_dir = fresh_dir(&format!("{label}-home"));
        let temp_root = fresh_dir(&format!("{label}-tmp"));
        let vault_pass = config_dir.join("vault-pass");
        fs::write(&vault_pass, "test-password\n").unwrap();
        let started = marker_command("MESH_STARTED", &config_dir);
        let stopped = marker_command("MESH_STOPPED", &config_dir);
        let interfaces: String = relay_ports
            .iter()
            .map(|port| format!("    - {{type: private, host: 127.0.0.1, port: {port}}}\n"))
            .collect();
        fs::write(
            config_dir.join("config.yaml"),
            format!(
                "model: dryrun:dry-model\n\
                 dry_run: true\n\
                 vault_password_file: '{vault_pass}'\n\
                 function_calling_support: true\n\
                 clients:\n\
                 \x20 - type: openai\n\
                 \x20   name: dryrun\n\
                 \x20   auth: none\n\
                 \x20   api_key: unused\n\
                 \x20   models:\n\
                 \x20     - name: dry-model\n\
                 \x20       max_input_tokens: 100000\n\
                 \x20       supports_function_calling: true\n\
                 save: false\n\
                 save_session: false\n\
                 hooks:\n\
                 \x20 mesh.started:\n\
                 \x20   - name: mark\n\
                 \x20     command: '{started}'\n\
                 \x20 mesh.stopped:\n\
                 \x20   - name: mark\n\
                 \x20     command: '{stopped}'\n\
                 mesh:\n\
                 \x20 enabled: {mesh_enabled}\n\
                 \x20 announce: false\n\
                 \x20 interfaces:\n\
                 {interfaces}",
                vault_pass = vault_pass.display(),
            ),
        )
        .unwrap();
        Self {
            config_dir,
            home_dir,
            temp_root,
        }
    }

    /// A plain user agent under `<config_dir>/agents/<name>/`, loadable offline: a
    /// name and instructions, no tools, no sessions default.
    fn with_agent(self, name: &str) -> Self {
        let dir = self.config_dir.join("agents").join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("config.yaml"),
            format!("name: {name}\ninstructions: {name}-agent-instructions\n"),
        )
        .unwrap();
        self
    }

    /// Appends main's `command_prefix` key to the config the mesh block
    /// already lives in, the way an operator would set both.
    fn with_command_prefix(self, prefix: &str) -> Self {
        let path = self.config_dir.join("config.yaml");
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(&format!("command_prefix: {prefix}\n"));
        fs::write(&path, text).unwrap();
        self
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_coyote"));
        cmd.env("COYOTE_CONFIG_DIR", &self.config_dir)
            .env("HOME", &self.home_dir)
            .env("USERPROFILE", &self.home_dir)
            .env("TMPDIR", &self.temp_root)
            .env("TMP", &self.temp_root)
            .env("TEMP", &self.temp_root)
            .env("XDG_CACHE_HOME", &self.temp_root)
            .env("TERM", "xterm-256color")
            .env_remove("IS_SANDBOX")
            .env_remove("COYOTE_PROVIDER")
            .env_remove("COYOTE_PLATFORM")
            .env_remove("COYOTE_ENV_FILE")
            .env_remove("COYOTE_CACHE_DIR")
            .env_remove("COYOTE_LEFT_PROMPT")
            .env_remove("COYOTE_RIGHT_PROMPT")
            .env_remove("COYOTE_SESSIONS_DIR")
            .current_dir(&self.home_dir);
        cmd
    }

    fn marker_count(&self, marker: &str) -> usize {
        fs::read_to_string(marker_log(&self.config_dir, marker))
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == marker)
            .count()
    }

    /// Hooks are spawned, not awaited, by the binary; bounded poll for the marker.
    fn wait_for_marker(&self, marker: &str) {
        self.wait_for_marker_count(marker, 1);
    }

    /// Bounded poll until `marker` has fired at least `count` times.
    fn wait_for_marker_count(&self, marker: &str, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.marker_count(marker) < count {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the {marker} hook marker to reach {count} (at {})",
                self.marker_count(marker)
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Removed on drop, so a failed run does not leave fixture dirs behind.
impl Drop for Probe {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.config_dir);
        let _ = fs::remove_dir_all(&self.home_dir);
        let _ = fs::remove_dir_all(&self.temp_root);
    }
}

fn marker_log(dir: &Path, marker: &str) -> PathBuf {
    dir.join(format!("mesh-events-{marker}.log"))
}

fn marker_command(marker: &str, dir: &Path) -> String {
    let log = marker_log(dir, marker);
    format!(r#"echo {marker} >> "{}""#, log.display())
}

fn spawn_repl(command: Command) -> Target {
    let mut session = Session::spawn(command).expect("spawn coyote in a pty");
    session
        .get_process_mut()
        .set_window_size(TERMINAL_COLUMNS, TERMINAL_ROWS)
        .expect("set pty window size");
    session.set_expect_timeout(Some(EXPECT_TIMEOUT));
    session
}

/// Waits for the regex `pattern`, answering every cursor position query that
/// arrives first, and returns the bytes that came before the match.
fn wait_for(session: &mut Target, pattern: &str) -> Vec<u8> {
    wait_for_match(session, pattern).0
}

/// Like `wait_for`, but returns the escape-stripped text of the row the match
/// landed on, through the end of the match.
fn wait_for_row(session: &mut Target, pattern: &str) -> String {
    let (before, matched) = wait_for_match(session, pattern);
    row_of(&before, &matched)
}

/// Waits for the regex `pattern`, answering every cursor position query that
/// arrives first, and returns the bytes before the match and the match itself.
fn wait_for_match(session: &mut Target, pattern: &str) -> (Vec<u8>, Vec<u8>) {
    let mut before = Vec::new();
    loop {
        let captures = session
            .expect(Any([Regex(pattern), Regex(CURSOR_POSITION_QUERY)]))
            .unwrap_or_else(|err| panic!("waiting for {pattern:?}: {err}"));
        before.extend_from_slice(captures.before());
        let matched = captures.get(0).unwrap_or_default().to_vec();
        if matched == b"\x1b[6n" {
            session
                .send(cursor_position_reply(captures.before()))
                .expect("reply to the cursor position query");
            continue;
        }
        return (before, matched);
    }
}

/// The escape-stripped text of the row `matched` landed on: everything after
/// the last line break before it, with a trailing `\r` trimmed.
fn row_of(before: &[u8], matched: &[u8]) -> String {
    let text = strip_escapes(before) + &strip_escapes(matched);
    text.rsplit('\n')
        .next()
        .unwrap_or_default()
        .trim_end_matches('\r')
        .to_string()
}

/// The reply to a cursor position query that arrived after `before`. Before
/// every prompt the REPL moves the cursor to a sentinel column (`ESC[<n>G`),
/// queries, and trusts the answer only when the column matches, so a query
/// preceded by a column move is answered with that column verbatim (the
/// terminal reports the same 1-based number the move named). A query with no
/// move before it gets the fixed reply.
fn cursor_position_reply(before: &[u8]) -> String {
    match last_column_move(before) {
        Some(column) => format!("\x1b[2;{column}R"),
        None => CURSOR_POSITION_REPLY.to_string(),
    }
}

/// The parameter of the last `ESC[<n>G` in `bytes`, verbatim.
fn last_column_move(bytes: &[u8]) -> Option<&str> {
    let mut found = None;
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let start = i + 2;
            let mut end = start;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > start && bytes.get(end) == Some(&b'G') {
                found = std::str::from_utf8(&bytes[start..end]).ok();
            }
        }
        i += 1;
    }
    found
}

/// Drops CSI and OSC sequences and two-byte ESC pairs.
fn strip_escapes(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            match bytes.get(i + 1) {
                Some(b'[') => {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                Some(b']') => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != 0x07 {
                        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                Some(_) => {
                    i += 2;
                    continue;
                }
                None => break,
            }
        }
        out.push(char::from(bytes[i]));
        i += 1;
    }
    out
}

/// Sends `line` and waits for `expected` (a regex) and then the next prompt;
/// waiting on the output first keeps the prompt repaint reedline paints while
/// echoing the typed line from being mistaken for the command's end.
fn command_prints(session: &mut Target, line: &str, expected: &str, prompt: &str) -> String {
    session.send(format!("{line}\r")).expect("type the command");
    let mut text = strip_escapes(&wait_for(session, expected));
    text.push_str(&strip_escapes(&wait_for(session, prompt)));
    text
}

fn exit_clean(session: &mut Target) -> WaitStatus {
    session.send(".exit\r").expect("leave the REPL");
    session.expect(Eof).expect("the REPL exits after .exit");
    session
        .get_process()
        .wait()
        .expect("collect the REPL exit status")
}

/// The harness answers the REPL's sentinel query with the column it moved to,
/// the last move winning, and falls back to the fixed reply when nothing
/// moved the cursor (reedline anchoring its prompt, or plain painting).
#[test]
fn cursor_queries_are_answered_with_the_last_column_moved_to() {
    assert_eq!(cursor_position_reply(b"prompt text"), "\x1b[2;1R");
    assert_eq!(cursor_position_reply(b"\x1b[0m\x1b[2J\x1b[H"), "\x1b[2;1R");
    assert_eq!(cursor_position_reply(b"\x1b7\x1b[6G"), "\x1b[2;6R");
    assert_eq!(
        cursor_position_reply(b"\x1b[3G text \x1b[12G"),
        "\x1b[2;12R"
    );
    assert_eq!(cursor_position_reply(b"\x1b[G"), "\x1b[2;1R");
}

/// Positive: with a session held, `mesh.enabled: true` joins before the
/// first prompt with the autostart notice and the `.mesh on` summary, asks
/// nothing, fires `mesh.started`, and a node verb works at the first prompt.
/// `.exit` stops the node (`mesh.stopped`) and exits clean.
#[test]
fn repl_with_mesh_enabled_joins_before_the_first_prompt_without_asking() {
    let relay = SilentRelay::start();
    let probe = Probe::new("joins", true, &[relay.port]);
    let mut command = probe.command();
    command.args(["--session", "probe"]);
    let mut session = spawn_repl(command);

    let startup = strip_escapes(&wait_for(&mut session, SESSION_PROMPT));
    let notice_at = startup
        .find(AUTOSTART_NOTICE)
        .unwrap_or_else(|| panic!("autostart notice missing before the first prompt: {startup:?}"));
    let summary_at = startup.find(MESH_ON_SUMMARY).unwrap_or_else(|| {
        panic!("`.mesh on` summary missing before the first prompt: {startup:?}")
    });
    assert!(notice_at < summary_at, "{startup:?}");
    assert!(
        !startup.contains("Join the mesh on")
            && !startup.contains("Join anyway?")
            && !startup.contains("(y/N)"),
        "autostart must not ask: {startup:?}"
    );
    assert!(
        !startup.contains(NEEDS_SESSION) && !startup.contains(MESH_OFF),
        "{startup:?}"
    );
    probe.wait_for_marker("MESH_STARTED");

    // A node verb works at the first prompt: the node is on and has heard nobody
    // (announce is off and the relay is silent). Waiting on that text is the
    // assertion; a `MESH_OFF` answer would time out here instead.
    let peers = command_prints(
        &mut session,
        ".mesh peers",
        NO_PEERS_PATTERN,
        SESSION_PROMPT,
    );
    assert!(!peers.contains(MESH_OFF), "{peers:?}");

    let status = exit_clean(&mut session);
    probe.wait_for_marker("MESH_STOPPED");
    let started = probe.marker_count("MESH_STARTED");
    let stopped = probe.marker_count("MESH_STOPPED");

    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        ".exit: expected a clean exit, got {status:?}"
    );
    assert_eq!(started, 1, "exactly one join at startup");
    assert_eq!(stopped, 1, "exactly one stop at exit");
}

/// Failed precondition: `mesh.enabled: true` with no session prints the
/// autostart notice and the very `.mesh on` refusal, then goes on to a usable
/// prompt with the mesh off; no node is started (no `mesh.started`).
#[test]
fn repl_with_mesh_enabled_but_no_session_prints_the_mesh_on_refusal_and_stays_off() {
    let relay = SilentRelay::start();
    let probe = Probe::new("no-session", true, &[relay.port]);
    let mut session = spawn_repl(probe.command());

    let startup = strip_escapes(&wait_for(&mut session, BARE_PROMPT));
    let notice_at = startup
        .find(AUTOSTART_NOTICE)
        .unwrap_or_else(|| panic!("autostart notice missing: {startup:?}"));
    let refusal_at = startup
        .find(NEEDS_SESSION)
        .unwrap_or_else(|| panic!("the `.mesh on` refusal must be printed: {startup:?}"));
    assert!(notice_at < refusal_at, "{startup:?}");
    assert!(
        startup.contains("Run `.session <name>` first."),
        "the refusal teaches the fix: {startup:?}"
    );
    assert!(!startup.contains(MESH_ON_SUMMARY), "{startup:?}");

    // The mesh stays off: the node verb answers with the off refusal.
    command_prints(&mut session, ".mesh peers", MESH_OFF_PATTERN, BARE_PROMPT);

    let status = exit_clean(&mut session);
    let started = probe.marker_count("MESH_STARTED");

    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        "a refused autostart must not break the REPL: {status:?}"
    );
    assert_eq!(started, 0, "no node was started");
}

/// Unchanged: `mesh.enabled: false` with a session held never mentions
/// joining and the mesh is off at the first prompt.
#[test]
fn repl_with_mesh_disabled_never_mentions_joining() {
    let relay = SilentRelay::start();
    let probe = Probe::new("disabled", false, &[relay.port]);
    let mut command = probe.command();
    command.args(["--session", "probe"]);
    let mut session = spawn_repl(command);

    let startup = strip_escapes(&wait_for(&mut session, SESSION_PROMPT));
    assert!(
        !startup.contains("joining the mesh") && !startup.contains(MESH_ON_SUMMARY),
        "{startup:?}"
    );

    command_prints(
        &mut session,
        ".mesh peers",
        MESH_OFF_PATTERN,
        SESSION_PROMPT,
    );

    let status = exit_clean(&mut session);
    let started = probe.marker_count("MESH_STARTED");

    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert_eq!(started, 0);
}

/// Under `command_prefix: slash` the mesh verbs answer to `/mesh` like every
/// other built-in and the dot form is ordinary chat: typed back to back, a
/// `.mesh peers` (chat, echoed by the dry-run model) and a `/mesh peers`
/// (the verb) produce exactly one off-refusal. `/help` lists the mesh row
/// under the configured sigil and never under the other, and `/exit` ends
/// the run cleanly.
#[test]
fn repl_under_slash_dispatches_slash_mesh_and_sends_dot_mesh_to_chat() {
    /// `command_prints` keeps the bytes around the match; this keeps the
    /// match too, so the transcript can be counted for the matched text.
    fn command_transcript(
        session: &mut Target,
        line: &str,
        expected: &str,
        prompt: &str,
    ) -> String {
        session.send(format!("{line}\r")).expect("type the command");
        let (before, matched) = wait_for_match(session, expected);
        let mut text = strip_escapes(&before) + &strip_escapes(&matched);
        text.push_str(&strip_escapes(&wait_for(session, prompt)));
        text
    }
    let relay = SilentRelay::start();
    let probe = Probe::new("slash-prefix", false, &[relay.port]).with_command_prefix("slash");
    let mut session = spawn_repl(probe.command());

    let startup = strip_escapes(&wait_for(&mut session, BARE_PROMPT));
    assert!(
        !startup.contains("joining the mesh") && !startup.contains(MESH_ON_SUMMARY),
        "{startup:?}"
    );

    // The verb under the configured sigil answers like every built-in.
    let text = command_transcript(&mut session, "/mesh peers", r"Mesh is off\.", BARE_PROMPT);
    assert_eq!(text.matches("Mesh is off").count(), 1, "{text:?}");

    // The dot form is chat under slash. Queued ahead of a second `/mesh peers`,
    // the transcript up to the first refusal holds exactly one; had the dot
    // form reached the verb, its refusal would be the one matched here and the
    // verb's own refusal would surface in the `/help` window below instead.
    session
        .send(".mesh peers\r")
        .expect("type the dot form, which is chat under slash");
    let text = command_transcript(&mut session, "/mesh peers", r"Mesh is off\.", BARE_PROMPT);
    assert_eq!(
        text.matches("Mesh is off").count(),
        1,
        "`.mesh peers` must not reach the verb under slash:\n{text}"
    );
    assert!(
        !text.contains("Unknown command"),
        "a dot line is chat under slash, not an unknown command:\n{text}"
    );

    // Help lists the mesh row under the configured sigil only. A refusal left
    // over from the previous window would land here, so none may.
    let help = command_transcript(&mut session, "/help", r"/mesh ", BARE_PROMPT);
    assert!(
        !help.contains("Mesh is off"),
        "`.mesh peers` reached the verb under slash; its refusal spilled into the help window:\n{help}"
    );
    assert!(
        help.lines().any(|l| l.trim_start().starts_with("/mesh ")),
        "{help}"
    );
    assert!(
        !help.lines().any(|l| l.trim_start().starts_with(".mesh ")),
        "help must not list the dot row under slash:\n{help}"
    );

    session.send("/exit\r").expect("leave the REPL under slash");
    session
        .expect(Eof)
        .expect("the REPL exits after /exit under slash");
    let status = session
        .get_process()
        .wait()
        .expect("collect the REPL exit status");
    let started = probe.marker_count("MESH_STARTED");

    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert_eq!(started, 0, "no node was started");
}

/// Entering a session is the other moment `mesh.enabled: true` joins: started without
/// `--session` the REPL prints the refusal, then `.session a` joins with the same notice
/// and no `.mesh on`. A typed `.mesh off` holds for the rest of the run (`.session b`
/// stays off) until `.mesh on`; `.exit session` while on leaves the mesh without that
/// decision, so `.session c` joins again. Every node start and stop fires its hook.
#[test]
fn entering_a_session_joins_and_mesh_off_holds_until_mesh_on_while_exit_session_leaves() {
    let relay = SilentRelay::start();
    let probe = Probe::new("enter-session", true, &[relay.port]);
    let mut session = spawn_repl(probe.command());
    let prompt_a = session_prompt("a");
    let prompt_b = session_prompt("b");
    let prompt_c = session_prompt("c");

    let startup = strip_escapes(&wait_for(&mut session, BARE_PROMPT));
    assert!(
        startup.contains(AUTOSTART_NOTICE) && startup.contains(NEEDS_SESSION),
        "{startup:?}"
    );
    assert_eq!(probe.marker_count("MESH_STARTED"), 0);

    // `.session a` joins by itself: the notice, the summary, no question.
    let entered = command_prints(&mut session, ".session a", MESH_ON_SUMMARY, &prompt_a);
    assert!(entered.contains(AUTOSTART_NOTICE), "{entered:?}");
    assert!(
        !entered.contains("Join the mesh on")
            && !entered.contains("Join anyway?")
            && !entered.contains("(y/N)")
            && !entered.contains(NEEDS_SESSION),
        "entering a session must join without asking: {entered:?}"
    );
    probe.wait_for_marker_count("MESH_STARTED", 1);
    let peers = command_prints(&mut session, ".mesh peers", NO_PEERS_PATTERN, &prompt_a);
    assert!(!peers.contains(MESH_OFF), "{peers:?}");

    // A typed `.mesh off` says it holds for the run; waiting on that clause is the
    // assertion (`command_prints` hands back the bytes around the match, not the match).
    let off = command_prints(
        &mut session,
        ".mesh off --yes",
        STAYS_OFF_PATTERN,
        &prompt_a,
    );
    assert!(off.contains("Mesh is off for this session"), "{off:?}");
    probe.wait_for_marker_count("MESH_STOPPED", 1);

    // Leaving a session with the mesh already off says nothing about leaving it.
    session.send(".exit session\r").expect("leave session a");
    let exited = strip_escapes(&wait_for(&mut session, BARE_PROMPT));
    assert!(!exited.contains(LEAVING), "{exited:?}");

    // `.session b` stays off: no notice, no summary, and the node verb answers with the
    // off refusal at the next prompt.
    session.send(".session b\r").expect("enter session b");
    let entered_b = strip_escapes(&wait_for(&mut session, &prompt_b));
    assert!(
        !entered_b.contains(AUTOSTART_NOTICE) && !entered_b.contains(MESH_ON_SUMMARY),
        "a typed `.mesh off` keeps the next session off: {entered_b:?}"
    );
    let peers = command_prints(&mut session, ".mesh peers", MESH_OFF_PATTERN, &prompt_b);
    assert!(
        !peers.contains(AUTOSTART_NOTICE) && !peers.contains(MESH_ON_SUMMARY),
        "{peers:?}"
    );
    assert_eq!(
        probe.marker_count("MESH_STARTED"),
        1,
        "no join for session b"
    );

    // `.mesh on` lifts the decision ...
    let on = command_prints(&mut session, ".mesh on --yes", MESH_ON_SUMMARY, &prompt_b);
    assert!(!on.contains(AUTOSTART_NOTICE), "{on:?}");
    probe.wait_for_marker_count("MESH_STARTED", 2);

    // ... and `.exit session` while on leaves the mesh without taking it again.
    let left = command_prints(&mut session, ".exit session", LEAVING_PATTERN, BARE_PROMPT);
    assert!(
        !left.contains(STAYS_OFF) && !left.contains("Run `.mesh on` to join again"),
        "leaving on `.exit session` is not a typed `.mesh off`: {left:?}"
    );
    probe.wait_for_marker_count("MESH_STOPPED", 2);
    let peers = command_prints(&mut session, ".mesh peers", MESH_OFF_PATTERN, BARE_PROMPT);
    assert!(!peers.contains(LEAVING), "{peers:?}");

    // `.session c` joins again.
    let entered_c = command_prints(&mut session, ".session c", MESH_ON_SUMMARY, &prompt_c);
    assert!(entered_c.contains(AUTOSTART_NOTICE), "{entered_c:?}");
    probe.wait_for_marker_count("MESH_STARTED", 3);
    let peers = command_prints(&mut session, ".mesh peers", NO_PEERS_PATTERN, &prompt_c);
    assert!(!peers.contains(MESH_OFF), "{peers:?}");

    let status = exit_clean(&mut session);
    probe.wait_for_marker_count("MESH_STOPPED", 3);
    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        ".exit: expected a clean exit, got {status:?}"
    );
    assert_eq!(
        probe.marker_count("MESH_STARTED"),
        3,
        "a, the `.mesh on` in b, and c"
    );
    assert_eq!(
        probe.marker_count("MESH_STOPPED"),
        3,
        "the `.mesh off` in a, the `.exit session` from b, and `.exit`"
    );
}

/// `mesh.enabled: false` with a session entered later never joins and `.exit session`
/// says nothing about the mesh: nothing changes for a config with the mesh off.
#[test]
fn entering_a_session_with_mesh_disabled_never_joins() {
    let relay = SilentRelay::start();
    let probe = Probe::new("enter-disabled", false, &[relay.port]);
    let mut session = spawn_repl(probe.command());
    let prompt_a = session_prompt("a");

    let startup = strip_escapes(&wait_for(&mut session, BARE_PROMPT));
    assert!(!startup.contains("joining the mesh"), "{startup:?}");

    session.send(".session a\r").expect("enter session a");
    let entered = strip_escapes(&wait_for(&mut session, &prompt_a));
    assert!(
        !entered.contains("joining the mesh") && !entered.contains(MESH_ON_SUMMARY),
        "{entered:?}"
    );
    command_prints(&mut session, ".mesh peers", MESH_OFF_PATTERN, &prompt_a);
    session.send(".exit session\r").expect("leave session a");
    let exited = strip_escapes(&wait_for(&mut session, BARE_PROMPT));
    assert!(!exited.contains(LEAVING), "{exited:?}");

    let status = exit_clean(&mut session);
    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert_eq!(probe.marker_count("MESH_STARTED"), 0);
}

/// A session is a session in any context: `.agent plain a` joins with the same notice
/// `.session` prints, `.exit agent` with that session held leaves the mesh, a typed
/// `.mesh off` holds across `.exit session` and the next `.agent plain c` until
/// `.mesh on`, and `.exit session` inside the agent leaves the mesh while the agent
/// stays. Every node start and stop fires its hook.
#[test]
fn entering_an_agent_session_joins_and_leaving_it_by_any_path_leaves_the_mesh() {
    let relay = SilentRelay::start();
    let probe = Probe::new("agent-session", true, &[relay.port]).with_agent("plain");
    let mut session = spawn_repl(probe.command());
    let prompt_plain_a = agent_session_prompt("plain", "a");
    let prompt_b = session_prompt("b");
    let prompt_plain_c = agent_session_prompt("plain", "c");
    let prompt_plain = agent_prompt("plain");

    let startup = strip_escapes(&wait_for(&mut session, BARE_PROMPT));
    assert!(
        startup.contains(AUTOSTART_NOTICE) && startup.contains(NEEDS_SESSION),
        "{startup:?}"
    );
    assert_eq!(probe.marker_count("MESH_STARTED"), 0);

    // `.agent plain a` enters a session and joins by itself: notice, summary, no question.
    let entered = command_prints(
        &mut session,
        ".agent plain a",
        MESH_ON_SUMMARY,
        &prompt_plain_a,
    );
    assert!(entered.contains(AUTOSTART_NOTICE), "{entered:?}");
    assert!(
        !entered.contains("Join the mesh on")
            && !entered.contains("Join anyway?")
            && !entered.contains("(y/N)")
            && !entered.contains(NEEDS_SESSION),
        "entering an agent session must join without asking: {entered:?}"
    );
    probe.wait_for_marker_count("MESH_STARTED", 1);
    let peers = command_prints(
        &mut session,
        ".mesh peers",
        NO_PEERS_PATTERN,
        &prompt_plain_a,
    );
    assert!(!peers.contains(MESH_OFF), "{peers:?}");

    // `.exit agent` takes the session with it, so the mesh is left first.
    let left = command_prints(&mut session, ".exit agent", LEAVING_PATTERN, BARE_PROMPT);
    assert!(
        !left.contains(STAYS_OFF) && !left.contains("Run `.mesh on` to join again"),
        "leaving on `.exit agent` is not a typed `.mesh off`: {left:?}"
    );
    probe.wait_for_marker_count("MESH_STOPPED", 1);

    // `.session b` joins again: the leave took no decision for the run.
    let entered_b = command_prints(&mut session, ".session b", MESH_ON_SUMMARY, &prompt_b);
    assert!(entered_b.contains(AUTOSTART_NOTICE), "{entered_b:?}");
    probe.wait_for_marker_count("MESH_STARTED", 2);

    // A typed `.mesh off` holds for the run ...
    let off = command_prints(
        &mut session,
        ".mesh off --yes",
        STAYS_OFF_PATTERN,
        &prompt_b,
    );
    assert!(off.contains("Mesh is off for this session"), "{off:?}");
    probe.wait_for_marker_count("MESH_STOPPED", 2);
    session.send(".exit session\r").expect("leave session b");
    let exited = strip_escapes(&wait_for(&mut session, BARE_PROMPT));
    assert!(!exited.contains(LEAVING), "{exited:?}");

    // ... across `.agent plain c`, which stays off: no notice, no summary, off refusal.
    session
        .send(".agent plain c\r")
        .expect("enter agent session c");
    let entered_c = strip_escapes(&wait_for(&mut session, &prompt_plain_c));
    assert!(
        !entered_c.contains(AUTOSTART_NOTICE) && !entered_c.contains(MESH_ON_SUMMARY),
        "a typed `.mesh off` keeps the next agent session off: {entered_c:?}"
    );
    let peers = command_prints(
        &mut session,
        ".mesh peers",
        MESH_OFF_PATTERN,
        &prompt_plain_c,
    );
    assert!(!peers.contains(AUTOSTART_NOTICE), "{peers:?}");
    assert_eq!(
        probe.marker_count("MESH_STARTED"),
        2,
        "no join for agent session c"
    );

    // `.mesh on` lifts the decision ...
    let on = command_prints(
        &mut session,
        ".mesh on --yes",
        MESH_ON_SUMMARY,
        &prompt_plain_c,
    );
    assert!(!on.contains(AUTOSTART_NOTICE), "{on:?}");
    probe.wait_for_marker_count("MESH_STARTED", 3);

    // ... and `.exit session` inside the agent leaves the mesh while the agent stays.
    let left = command_prints(
        &mut session,
        ".exit session",
        LEAVING_PATTERN,
        &prompt_plain,
    );
    assert!(!left.contains(STAYS_OFF), "{left:?}");
    probe.wait_for_marker_count("MESH_STOPPED", 3);
    let peers = command_prints(&mut session, ".mesh peers", MESH_OFF_PATTERN, &prompt_plain);
    assert!(!peers.contains(LEAVING), "{peers:?}");

    // `.exit agent` with no session held says nothing about the mesh.
    session.send(".exit agent\r").expect("leave the agent");
    let exited = strip_escapes(&wait_for(&mut session, BARE_PROMPT));
    assert!(!exited.contains(LEAVING), "{exited:?}");

    let status = exit_clean(&mut session);
    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        ".exit: expected a clean exit, got {status:?}"
    );
    assert_eq!(
        probe.marker_count("MESH_STARTED"),
        3,
        "plain a, b, and the `.mesh on` in plain c"
    );
    assert_eq!(
        probe.marker_count("MESH_STOPPED"),
        3,
        "the `.exit agent` from plain a, the `.mesh off` in b, and the `.exit session` from plain c"
    );
}

/// Connect-later: the node comes up with its first interface connected and its
/// second refused (bound, not listening), warns about the second on startup,
/// and once that port starts listening the idle line it pushes reaches the
/// prompt printer, which repaints the prompt identically under it.
#[test]
fn an_interface_that_connects_later_prints_its_idle_line_and_leaves_the_prompt_intact() {
    let sink = SilentRelay::start();
    let mut held = HeldPort::hold();
    let probe = Probe::new("connect-later", true, &[sink.port, held.port]);
    let mut command = probe.command();
    command.args(["--session", "probe"]);
    let mut session = spawn_repl(command);

    let (before, matched) = wait_for_match(&mut session, SESSION_PROMPT);
    let startup = strip_escapes(&before);
    assert!(startup.contains(MESH_ON_SUMMARY), "{startup:?}");
    let warning = format!("WARNING: private 127.0.0.1:{} is unreachable (", held.port);
    assert!(
        startup.contains(&warning),
        "the pending interface is warned about at startup: {startup:?}"
    );
    probe.wait_for_marker("MESH_STARTED");
    let first_prompt = row_of(&before, &matched);
    assert!(
        first_prompt.contains("dry-model)probe)"),
        "{first_prompt:?}"
    );

    held.open();

    wait_for(
        &mut session,
        &format!(r"\[mesh\] private 127\.0\.0\.1:{} connected", held.port),
    );
    let second_prompt = wait_for_row(&mut session, SESSION_PROMPT);
    assert!(!second_prompt.contains("[mesh]"), "{second_prompt:?}");
    assert_eq!(
        second_prompt, first_prompt,
        "the prompt repainted under the idle line must be byte-identical once escapes are stripped"
    );

    let peers = command_prints(
        &mut session,
        ".mesh peers",
        NO_PEERS_PATTERN,
        SESSION_PROMPT,
    );
    assert!(!peers.contains(MESH_OFF), "{peers:?}");

    let status = exit_clean(&mut session);
    probe.wait_for_marker("MESH_STOPPED");
    let started = probe.marker_count("MESH_STARTED");
    let stopped = probe.marker_count("MESH_STOPPED");

    assert!(
        matches!(status, WaitStatus::Exited(_, 0)),
        ".exit: expected a clean exit, got {status:?}"
    );
    assert_eq!(started, 1, "exactly one join at startup");
    assert_eq!(stopped, 1, "exactly one stop at exit");
}
