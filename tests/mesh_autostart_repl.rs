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
    let notice_line = startup
        .lines()
        .find(|line| line.contains(AUTOSTART_NOTICE))
        .unwrap();
    assert!(
        !notice_line.contains(".mesh off"),
        "the notice carries no pointer of its own; the summary's `.mesh off` is the one: {notice_line:?}"
    );
    assert!(
        startup.contains("Turn it off with `.mesh off`."),
        "the summary names the off command once the notice no longer does: {startup:?}"
    );
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

/// Sends `line`, waits for the regex `expected`, and returns the escape-stripped
/// row the match landed on together with the whole transcript through the next
/// prompt (the row first), so a test can pin one printed line exactly and still
/// sweep everything the command printed.
fn command_row_and_transcript(
    session: &mut Target,
    line: &str,
    expected: &str,
    prompt: &str,
) -> (String, String) {
    session.send(format!("{line}\r")).expect("type the command");
    let (before, matched) = wait_for_match(session, expected);
    let row = row_of(&before, &matched);
    let mut transcript = strip_escapes(&before) + &strip_escapes(&matched);
    transcript.push_str(&strip_escapes(&wait_for(session, prompt)));
    (row, transcript)
}

/// A regex that matches `text` literally.
fn literally(text: &str) -> String {
    let mut out = String::new();
    for ch in text.chars() {
        if r"\.+*?()[]{}|^$".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Cold start, mesh off, no session: the verbs renamed before the release answer
/// at the first prompt with the words the docs quote, and the old spellings are
/// refused outright rather than aliased. `.mesh reply` is an unknown verb whose
/// error names `.mesh message` and nothing is previewed or sent; `.mesh brief
/// note` sets and clears the note with the documented confirmations while `brief
/// set` and `brief clear` are refused with the new spelling named; the `.mesh`
/// help list carries the `message` row and no `reply` row. No node starts.
#[test]
fn renamed_verbs_answer_at_a_cold_prompt_and_the_retired_spellings_are_refused() {
    const REPLY_REFUSED: &str = "Unknown .mesh command 'reply'. To message a peer, use `.mesh message <destination> [--yes] \"text\" [--attach <path> [--force]]`. Type `.mesh` for the list.";
    const NOTE_SET: &str = "Note set; your envoy includes it the next time it answers a peer (with the card and, in auto mode, the digest). Run `.mesh brief` to see what a peer is served.";
    const NOTE_CLEARED: &str = "Note cleared; your envoy answers peers without it from now on. `.mesh brief note \"text\"` sets a new one.";
    fn retired(word: &str) -> String {
        format!(
            ".mesh brief has no '{word}' argument. Set the note with `.mesh brief note \"text\"` and remove it with `.mesh brief note clear`."
        )
    }

    let relay = SilentRelay::start();
    let probe = Probe::new("renamed-verbs", false, &[relay.port]);
    let mut session = spawn_repl(probe.command());
    let startup = strip_escapes(&wait_for(&mut session, BARE_PROMPT));
    assert!(!startup.contains("joining the mesh"), "{startup:?}");

    // The old verb is unknown on a cold node: the error alone names the new one.
    let hash = "ab".repeat(16);
    let (row, transcript) = command_row_and_transcript(
        &mut session,
        &format!(".mesh reply {hash} --yes hello"),
        &literally(REPLY_REFUSED),
        BARE_PROMPT,
    );
    // A refusal is a command error, so the REPL paints it under its `Error:` prefix;
    // the sentence is the whole of what the user reads on that row.
    assert_eq!(row.trim(), format!("Error: {REPLY_REFUSED}"));
    for sent in [
        "This sends your text",
        "Send it?",
        "Sent ",
        "Nothing was sent",
    ] {
        assert!(
            !transcript.contains(sent),
            "`.mesh reply` must not reach the send path: found {sent:?} in {transcript:?}"
        );
    }

    // The note is set and cleared with the documented confirmations, on or off.
    let (row, _) = command_row_and_transcript(
        &mut session,
        ".mesh brief note \"we ship on Friday\"",
        &literally(NOTE_SET),
        BARE_PROMPT,
    );
    assert_eq!(row.trim(), NOTE_SET);
    for (line, word) in [
        (".mesh brief set \"other\"", "set"),
        (".mesh brief clear", "clear"),
    ] {
        let expected = retired(word);
        let (row, transcript) =
            command_row_and_transcript(&mut session, line, &literally(&expected), BARE_PROMPT);
        assert_eq!(row.trim(), format!("Error: {expected}"));
        assert!(
            !transcript.contains("Note set") && !transcript.contains("Note cleared"),
            "{line} is refused, never an alias: {transcript:?}"
        );
    }
    let (row, _) = command_row_and_transcript(
        &mut session,
        ".mesh brief note clear",
        &literally(NOTE_CLEARED),
        BARE_PROMPT,
    );
    assert_eq!(row.trim(), NOTE_CLEARED);

    // The help list names `message`, with its usage line, and has no `reply` row.
    let (_, help) = command_row_and_transcript(
        &mut session,
        ".mesh",
        r"\.mesh message\s+Send your own text to one peer, bypassing the model",
        BARE_PROMPT,
    );
    assert!(
        help.contains(
            "e.g. .mesh message <destination> [--yes] \"text\" [--attach <path> [--force]]"
        ),
        "{help}"
    );
    assert!(
        help.contains("e.g. .mesh brief [note \"text\"|note clear|auto|manual|off]"),
        "{help}"
    );
    assert!(
        !help
            .lines()
            .any(|line| line.trim_start().starts_with(".mesh reply")),
        "the help list must not carry a reply row:\n{help}"
    );
    assert!(
        !help.contains("brief set") && !help.contains("brief clear"),
        "{help}"
    );

    let status = exit_clean(&mut session);
    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
    assert_eq!(probe.marker_count("MESH_STARTED"), 0, "no node was started");
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
/// other built-in and the dot form is ordinary chat: `/mesh peers` is refused
/// because the mesh is off, `.mesh peers` comes back from the dry-run model
/// instead of the verb, and a `/mesh peers` after that chat turn is refused
/// again. `/help` lists the mesh row under the configured sigil and never
/// under the other, and `/exit` ends the run cleanly.
#[test]
fn repl_under_slash_dispatches_slash_mesh_and_sends_dot_mesh_to_chat() {
    /// `command_prints` drops the matched bytes; this keeps them, so the
    /// transcript can be counted for the matched text.
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

    // The dot form is chat under slash: the dry-run model echoes the line back
    // on a row of its own, which the typed-line repaint never does (that one
    // follows the prompt on the same row). Waiting for that row and the prompt
    // after it also keeps the next command from being typed into the running
    // turn, where the REPL reads keys only to spot Ctrl-C.
    session
        .send(".mesh peers\r")
        .expect("type the dot form, which is chat under slash");
    let (before, matched) = wait_for_match(
        &mut session,
        r"\n(\x1b\[[0-9;?]*[A-Za-z])*\.mesh peers|Mesh is off\.",
    );
    let mut chat = strip_escapes(&before) + &strip_escapes(&matched);
    assert!(
        !chat.contains("Mesh is off"),
        "`.mesh peers` reached the verb under slash instead of the model:\n{chat}"
    );
    chat.push_str(&strip_escapes(&wait_for(&mut session, BARE_PROMPT)));
    assert!(
        !chat.contains("Unknown command"),
        "a dot line is chat under slash, not an unknown command:\n{chat}"
    );

    // The verb still answers after the chat turn.
    let text = command_transcript(&mut session, "/mesh peers", r"Mesh is off\.", BARE_PROMPT);
    assert_eq!(text.matches("Mesh is off").count(), 1, "{text:?}");

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

/// The hashes under one section of `<config_dir>/mesh/trust.yaml`, as the node wrote
/// it; a missing file is an empty list, the shape a cold node starts from.
fn trust_file_keys(config_dir: &Path, section: &str) -> Vec<String> {
    let path = config_dir.join("mesh").join("trust.yaml");
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let file: serde_yaml::Value = serde_yaml::from_str(&text).expect("trust.yaml parses");
    assert_eq!(file["version"], serde_yaml::Value::from(2), "{text}");
    file[section]
        .as_mapping()
        .map(|map| {
            map.keys()
                .map(|key| key.as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Sends `line`, waits for the regex `expected` and returns the escape-stripped
/// transcript through the next prompt; `expected` is the one line the step pins.
fn trust_verb(session: &mut Target, line: &str, expected: &str) -> String {
    let (_, transcript) =
        command_row_and_transcript(session, line, &literally(expected), SESSION_PROMPT);
    transcript
}

/// Cold start with a node on and nobody heard: the trust verbs, typed against a
/// destination and an identity the node has never seen, write exactly the entry each
/// one names into `trust.yaml` and say so in the documented words. `forget` is not an
/// alias of `untrust` any more: its destination form is refused before anything is
/// read or written, `block <destination>` works on a never-heard hash and `.mesh peers`
/// lists it as denied, `untrust` on that deny-only hash names `unblock`, a second
/// `block` changes nothing, `unblock` lifts the deny and trusts nothing, `block
/// --identity` writes the block, `forget --identity` is a dry run until its own token
/// comes back (never `--yes`) and then erases the block, after which the identity is
/// a stranger the verb refuses. The identity-wide `untrust` of an unknown identity
/// writes nothing.
#[test]
fn trust_verbs_at_a_cold_prompt_write_only_what_they_name_and_forget_is_no_alias() {
    let relay = SilentRelay::start();
    let probe = Probe::new("trust-verbs", true, &[relay.port]);
    let mut command = probe.command();
    command.args(["--session", "probe"]);
    let mut session = spawn_repl(command);
    let startup = strip_escapes(&wait_for(&mut session, SESSION_PROMPT));
    assert!(startup.contains(MESH_ON_SUMMARY), "{startup:?}");
    probe.wait_for_marker("MESH_STARTED");
    let dir = probe.config_dir.clone();
    let dest = "cd".repeat(16);
    let id = "ef".repeat(16);
    let no_entries = |label: &str| {
        for section in [
            "identities",
            "destinations",
            "denied_destinations",
            "blocked_identities",
        ] {
            assert!(
                trust_file_keys(&dir, section).is_empty(),
                "{label}: {section} must be empty"
            );
        }
    };

    // `forget` takes `--identity` only; the destination form teaches the two verbs that
    // act on one and reaches neither the untrust path nor the file.
    let expected = format!(
        ".mesh forget takes --identity <id>: a destination is untrusted with .mesh untrust {dest} or refused with .mesh block {dest}."
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh forget {dest} --yes"),
        &expected,
    );
    assert!(out.contains(&format!("Error: {expected}")), "{out:?}");
    assert!(
        !out.contains("Untrust") && !out.contains("untrusts instance") && !out.contains("Forget "),
        "`.mesh forget <destination>` must not act as untrust: {out:?}"
    );
    no_entries("after forget <destination>");

    // `untrust` of a hash the list holds nothing of names both verbs that act on it.
    let expected = format!(
        "Destination {dest} is not in the trust list, so there is nothing to untrust; .mesh block {dest} refuses it, .mesh trust {dest} serves it."
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh untrust {dest} --yes"),
        &expected,
    );
    assert!(out.contains(&format!("Error: {expected}")), "{out:?}");
    no_entries("after untrust of an unknown destination");

    // `block <destination>` works on a never-heard hash: the deny alone is written and
    // the result names `unblock`; with no identity to name, the clause is left out.
    let expected =
        format!("Blocked cdcdcdcd: it is refused without a knock. .mesh unblock {dest} lifts it.");
    let out = trust_verb(
        &mut session,
        &format!(".mesh block {dest} --yes"),
        &expected,
    );
    assert!(
        out.contains("This blocks instance cdcdcdcd: it is refused without a knock and its trust record, if any, is removed."),
        "{out:?}"
    );
    assert!(!out.contains("Removed its trust record."), "{out:?}");
    assert_eq!(
        trust_file_keys(&dir, "denied_destinations"),
        vec![dest.clone()]
    );
    assert!(trust_file_keys(&dir, "destinations").is_empty());
    assert!(trust_file_keys(&dir, "identities").is_empty());
    assert!(trust_file_keys(&dir, "blocked_identities").is_empty());

    // The deny-only row shows in `.mesh peers` as denied, never heard.
    let peers = trust_verb(&mut session, ".mesh peers", "cdcdcdcd");
    let row = peers
        .lines()
        .find(|line| line.contains("cdcdcdcd"))
        .unwrap_or_else(|| panic!("no peers row for the denied destination: {peers:?}"));
    assert!(row.contains("denied"), "{row:?}");
    assert!(row.contains("never"), "{row:?}");
    assert!(!peers.contains("No peers heard yet"), "{peers:?}");

    // `untrust` on the deny-only hash names the block and the verb that lifts it.
    let expected = format!(
        "Destination {dest} is blocked and has no trust record, so there is nothing to untrust; .mesh unblock {dest} lifts the block."
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh untrust {dest} --yes"),
        &expected,
    );
    assert!(out.contains(&format!("Error: {expected}")), "{out:?}");
    assert_eq!(
        trust_file_keys(&dir, "denied_destinations"),
        vec![dest.clone()]
    );

    // A second `block` of the same hash changes nothing and says so.
    let out = trust_verb(
        &mut session,
        &format!(".mesh block {dest} --yes"),
        "cdcdcdcd is already blocked.",
    );
    assert!(out.contains("Nothing was changed."), "{out:?}");
    assert!(!out.contains("Blocked cdcdcdcd:"), "{out:?}");

    // `unblock <destination>` lifts the deny and trusts nothing; with no record and no
    // peer row for the hash, nothing binds it to an identity, so what serves it is
    // known only once it announces.
    let standing = format!(
        "no trust record names its identity any more; an identity trusted for all destinations serves it once it announces, otherwise .mesh trust {dest} serves it."
    );
    let expected = format!("Unblocked cdcdcdcd; {standing}");
    let out = trust_verb(
        &mut session,
        &format!(".mesh unblock {dest} --yes"),
        &expected,
    );
    assert!(
        out.contains(&format!(
            "This lifts the block on instance cdcdcdcd; afterwards {standing}"
        )),
        "{out:?}"
    );
    no_entries("after unblock <destination>");

    // Lifting a deny that is not there is refused, naming both next verbs, and nothing
    // is written.
    let expected = format!(
        "Destination {dest} is not blocked, so there is nothing to unblock; .mesh block {dest} refuses it, .mesh trust {dest} serves it."
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh unblock {dest} --yes"),
        &expected,
    );
    assert!(out.contains(&format!("Error: {expected}")), "{out:?}");
    no_entries("after a second unblock");

    // `untrust --identity` of an identity the list has never held writes nothing.
    let expected = format!(
        "Identity {id} is not in the trust list, so there is nothing to untrust; .mesh trust --identity {id} grants it, .mesh block --identity {id} silences it."
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh untrust --identity {id}"),
        &expected,
    );
    assert!(out.contains(&format!("Error: {expected}")), "{out:?}");
    no_entries("after untrust --identity of an unknown identity");

    // `block --identity` works on an identity the list has never seen.
    let out = trust_verb(
        &mut session,
        &format!(".mesh block --identity {id} --yes"),
        "Blocked efefefef.",
    );
    assert!(
        out.contains("This blocks identity efefefef: its knocks are dropped without a word, and every trust record for it, identity and instances alike, is removed."),
        "{out:?}"
    );
    assert_eq!(
        trust_file_keys(&dir, "blocked_identities"),
        vec![id.clone()]
    );
    assert!(trust_file_keys(&dir, "identities").is_empty());
    assert!(trust_file_keys(&dir, "denied_destinations").is_empty());

    // `forget --identity` is a dry run first: it lists the block and prints its own
    // token; `--yes` is refused; nothing is written by either.
    let token_line = format!(
        "This was a dry run; nothing changed. To erase everything this node knows of this identity, run: .mesh forget --identity {id} --confirm forget-efefefef"
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh forget --identity {id}"),
        &token_line,
    );
    assert!(
        out.contains(&format!("Forgetting identity {id} erases:")),
        "{out:?}"
    );
    assert!(out.contains("  block on identity efefefef"), "{out:?}");
    assert!(!out.contains("Forgot identity"), "{out:?}");
    let expected = "`.mesh forget --identity` takes `--confirm forget-<identity-short>` from a dry run, not `--yes`.";
    let out = trust_verb(
        &mut session,
        &format!(".mesh forget --identity {id} --yes"),
        expected,
    );
    assert!(out.contains(&format!("Error: {expected}")), "{out:?}");
    assert_eq!(
        trust_file_keys(&dir, "blocked_identities"),
        vec![id.clone()]
    );

    // A wrong token is refused and names the right one; the right one erases the block.
    let expected = format!(
        "'forget-00000000' is not the token for identity efefefef; run .mesh forget --identity {id} --confirm forget-efefefef"
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh forget --identity {id} --confirm forget-00000000"),
        &expected,
    );
    assert!(out.contains(&format!("Error: {expected}")), "{out:?}");
    assert_eq!(
        trust_file_keys(&dir, "blocked_identities"),
        vec![id.clone()]
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh forget --identity {id} --confirm forget-efefefef"),
        "Forgot identity efefefef: grants, blocks, knocks and envoy memory erased; it is a stranger again.",
    );
    assert!(out.contains("  block on identity efefefef"), "{out:?}");
    no_entries("after forget --identity");

    // A stranger has nothing to forget.
    let expected = format!(
        "Identity {id} is a stranger already: no grant, block, knock or envoy memory is recorded for it, so there is nothing to forget. .mesh memory forget {id} --dry-run lists what the envoy memory on disk holds of it."
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh forget --identity {id}"),
        &expected,
    );
    assert!(out.contains(&format!("Error: {expected}")), "{out:?}");

    // The verb list carries the split: `forget` takes `--identity` only, `block` and
    // `unblock` take a destination or `--identity`.
    let (_, help) = command_row_and_transcript(
        &mut session,
        ".mesh",
        r"\.mesh forget\s+Erase everything this node knows of an identity",
        SESSION_PROMPT,
    );
    assert!(
        help.contains(
            "e.g. .mesh forget --identity <identity> [--dry-run|--confirm forget-<identity-short>]"
        ),
        "{help}"
    );
    assert!(
        help.contains("e.g. .mesh block <destination> [--yes] [--note \"text\"] | --identity <identity> [--yes] [--note \"text\"]"),
        "{help}"
    );
    assert!(
        help.contains("e.g. .mesh unblock <destination> [--yes] | --identity <identity> [--yes]"),
        "{help}"
    );
    assert!(!help.contains("alias of"), "{help}");

    let status = exit_clean(&mut session);
    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
}

/// Cold start over a `trust.yaml` an earlier run left: an identity trusted for all
/// destinations with one instance record bound to it. `untrust <destination>` is
/// refused in the documented sentence and the file is byte-identical afterwards,
/// dry run or not; `block <destination>` is the verb that stops serving that one
/// instance, and it leaves the identity grant standing while the deny is written and
/// the record removed; `unblock` then lifts the deny alone, the grant still standing.
#[test]
fn untrust_of_an_instance_under_an_identity_grant_refuses_at_a_cold_prompt_and_block_is_the_remedy()
{
    let relay = SilentRelay::start();
    let probe = Probe::new("trust-grant", true, &[relay.port]);
    let dest = "ab".repeat(16);
    let id = "12".repeat(16);
    let mesh_dir = probe.config_dir.join("mesh");
    fs::create_dir_all(&mesh_dir).unwrap();
    let trust_path = mesh_dir.join("trust.yaml");
    fs::write(
        &trust_path,
        format!(
            "version: 2\n\
             identities:\n\
             \x20 {id}:\n\
             \x20   added_at: '2026-01-01T00:00:00Z'\n\
             \x20   last_seen_at: '2026-01-01T00:00:00Z'\n\
             \x20   label: Ida\n\
             \x20   note: null\n\
             \x20   all_destinations: true\n\
             destinations:\n\
             \x20 {dest}:\n\
             \x20   identity: {id}\n\
             \x20   added_at: '2026-01-01T00:00:00Z'\n\
             \x20   last_seen_at: '2026-01-01T00:00:00Z'\n\
             \x20   label: null\n\
             \x20   note: null\n\
             denied_destinations: {{}}\n\
             blocked_identities: {{}}\n"
        ),
    )
    .unwrap();
    let seeded = fs::read(&trust_path).unwrap();
    let mut command = probe.command();
    command.args(["--session", "probe"]);
    let mut session = spawn_repl(command);
    let startup = strip_escapes(&wait_for(&mut session, SESSION_PROMPT));
    assert!(startup.contains(MESH_ON_SUMMARY), "{startup:?}");
    probe.wait_for_marker("MESH_STARTED");
    let dir = probe.config_dir.clone();

    let refusal = format!(
        "Destination abababab belongs to identity 12121212, which this node trusts fully, so untrusting the destination alone would change nothing. To stop serving just this instance, run .mesh block {dest}. To stop trusting the whole identity, run .mesh untrust --identity {id}."
    );
    for line in [
        format!(".mesh untrust {dest}"),
        format!(".mesh untrust {dest} --dry-run"),
        format!(".mesh untrust {dest} --yes"),
    ] {
        let out = trust_verb(&mut session, &line, &refusal);
        assert!(
            out.contains(&format!("Error: {refusal}")),
            "{line}: {out:?}"
        );
        assert!(
            !out.contains("This untrusts instance") && !out.contains("Untrust abababab?"),
            "{line}: refused before any preview or prompt: {out:?}"
        );
        assert_eq!(
            fs::read(&trust_path).unwrap(),
            seeded,
            "{line}: the refusal writes nothing"
        );
    }

    // `block <destination>` is the named remedy: the deny is written, the record goes,
    // the identity grant stands, and the line names the identity it leaves unchanged.
    let expected = format!(
        "Blocked abababab: it is refused without a knock (identity 12121212 otherwise unchanged). .mesh unblock {dest} lifts it."
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh block {dest} --yes"),
        &expected,
    );
    assert!(
        out.contains("This blocks instance abababab: it is refused without a knock and its trust record, if any, is removed; identity 12121212 is otherwise unchanged."),
        "{out:?}"
    );
    assert!(out.contains("Removed its trust record."), "{out:?}");
    assert_eq!(
        trust_file_keys(&dir, "denied_destinations"),
        vec![dest.clone()]
    );
    assert!(trust_file_keys(&dir, "destinations").is_empty());
    assert_eq!(trust_file_keys(&dir, "identities"), vec![id.clone()]);
    assert!(trust_file_keys(&dir, "blocked_identities").is_empty());

    // `unblock <destination>` lifts the deny only: the grant still stands, no record
    // returns, and with the record gone and the peer unheard nothing binds the hash to the
    // identity, so the line says the grant serves it once it announces.
    let expected = format!(
        "Unblocked abababab; no trust record names its identity any more; an identity trusted for all destinations serves it once it announces, otherwise .mesh trust {dest} serves it."
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh unblock {dest} --yes"),
        &expected,
    );
    assert!(!out.contains("Error:"), "{out:?}");
    assert!(trust_file_keys(&dir, "denied_destinations").is_empty());
    assert!(trust_file_keys(&dir, "destinations").is_empty());
    assert_eq!(trust_file_keys(&dir, "identities"), vec![id.clone()]);

    let status = exit_clean(&mut session);
    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
}

/// Whether `trust.yaml` grants identity `id` every destination (`None` when the
/// identity is not on file).
fn identity_grants_all(config_dir: &Path, id: &str) -> Option<bool> {
    let text = fs::read_to_string(config_dir.join("mesh").join("trust.yaml")).ok()?;
    let file: serde_yaml::Value = serde_yaml::from_str(&text).expect("trust.yaml parses");
    file["identities"][id]["all_destinations"].as_bool()
}

/// Cold start over a `trust.yaml` an earlier run left: an identity trusted for all
/// destinations with two instance records bound to it, one of them denied. `untrust
/// --identity` is a dry run until its own token comes back, and then withdraws the
/// grant ALONE: both records stay, the deny stays, the identity stays on file as a
/// known identity granting nothing, and the line says so. With no grant left to
/// withdraw a second `untrust --identity` is refused in the documented sentence and
/// writes nothing; `forget --identity` is what erases the identity with its instances
/// and denies.
#[test]
fn usage_probe_untrust_identity_at_a_cold_prompt_withdraws_the_grant_and_leaves_the_bound_records()
{
    let relay = SilentRelay::start();
    let probe = Probe::new("trust-grant-only", true, &[relay.port]);
    let dest_a = "ab".repeat(16);
    let dest_b = "ba".repeat(16);
    let id = "12".repeat(16);
    let mesh_dir = probe.config_dir.join("mesh");
    fs::create_dir_all(&mesh_dir).unwrap();
    let trust_path = mesh_dir.join("trust.yaml");
    fs::write(
        &trust_path,
        format!(
            "version: 2\n\
             identities:\n\
             \x20 {id}:\n\
             \x20   added_at: '2026-01-01T00:00:00Z'\n\
             \x20   last_seen_at: '2026-01-01T00:00:00Z'\n\
             \x20   label: Ida\n\
             \x20   note: null\n\
             \x20   all_destinations: true\n\
             destinations:\n\
             \x20 {dest_a}:\n\
             \x20   identity: {id}\n\
             \x20   added_at: '2026-01-01T00:00:00Z'\n\
             \x20   last_seen_at: '2026-01-01T00:00:00Z'\n\
             \x20   label: null\n\
             \x20   note: null\n\
             \x20 {dest_b}:\n\
             \x20   identity: {id}\n\
             \x20   added_at: '2026-01-01T00:00:00Z'\n\
             \x20   last_seen_at: '2026-01-01T00:00:00Z'\n\
             \x20   label: null\n\
             \x20   note: null\n\
             denied_destinations:\n\
             \x20 {dest_b}:\n\
             \x20   added_at: '2026-01-01T00:00:00Z'\n\
             \x20   note: null\n\
             blocked_identities: {{}}\n"
        ),
    )
    .unwrap();
    let seeded = fs::read(&trust_path).unwrap();
    let mut command = probe.command();
    command.args(["--session", "probe"]);
    let mut session = spawn_repl(command);
    let startup = strip_escapes(&wait_for(&mut session, SESSION_PROMPT));
    assert!(startup.contains(MESH_ON_SUMMARY), "{startup:?}");
    probe.wait_for_marker("MESH_STARTED");
    let dir = probe.config_dir.clone();
    let token = "untrust-12121212";
    let bound_both = |label: &str| {
        let mut destinations = trust_file_keys(&dir, "destinations");
        destinations.sort();
        let mut expected = vec![dest_a.clone(), dest_b.clone()];
        expected.sort();
        assert_eq!(destinations, expected, "{label}: both records stay");
        assert_eq!(
            trust_file_keys(&dir, "denied_destinations"),
            vec![dest_b.clone()],
            "{label}: the deny stays"
        );
        assert!(
            trust_file_keys(&dir, "blocked_identities").is_empty(),
            "{label}"
        );
    };

    // The dry run (bare and `--dry-run`) previews the grant and the two instances that
    // stay, names the token, and writes nothing.
    let dry_tail = format!(
        "This was a dry run; nothing changed. To withdraw the grant, run: .mesh untrust --identity {id} --confirm {token}. To erase the identity with its instances, denies and knocks, run .mesh forget --identity {id}."
    );
    for line in [
        format!(".mesh untrust --identity {id}"),
        format!(".mesh untrust --identity {id} --dry-run"),
    ] {
        let out = trust_verb(&mut session, &line, &dry_tail);
        assert!(
            out.contains(&format!(
                "Untrusting identity {id} withdraws its grant for all destinations:"
            )),
            "{line}: {out:?}"
        );
        assert!(
            out.contains("2 instance(s) stay trusted on their own:"),
            "{line}: {out:?}"
        );
        assert!(
            out.contains(&format!("instance {dest_b}")) && out.contains("refused"),
            "{line}: the denied instance is listed with its refusal: {out:?}"
        );
        assert!(!out.contains("Error:"), "{line}: {out:?}");
        assert_eq!(
            fs::read(&trust_path).unwrap(),
            seeded,
            "{line}: a dry run writes nothing"
        );
    }

    // `--yes` is not the token for an identity-wide verb; the wrong token is refused too.
    for (line, expected) in [
        (
            format!(".mesh untrust --identity {id} --yes"),
            "`.mesh untrust --identity` takes `--confirm untrust-<identity-short>` from a dry run, not `--yes`.".to_string(),
        ),
        (
            format!(".mesh untrust --identity {id} --confirm untrust-nope"),
            format!(
                "'untrust-nope' is not the token for identity 12121212; run .mesh untrust --identity {id} --confirm {token}"
            ),
        ),
    ] {
        let out = trust_verb(&mut session, &line, &expected);
        assert!(out.contains("Error:"), "{line}: {out:?}");
        assert_eq!(fs::read(&trust_path).unwrap(), seeded, "{line}: a refusal writes nothing");
    }

    // The token withdraws the grant alone.
    let expected = format!(
        "Untrusted identity 12121212: its grant for all destinations is withdrawn; 2 instance(s) stay trusted on their own. .mesh forget --identity {id} erases them too."
    );
    let out = trust_verb(
        &mut session,
        &format!(".mesh untrust --identity {id} --confirm {token}"),
        &expected,
    );
    assert!(!out.contains("Error:"), "{out:?}");
    assert_eq!(
        identity_grants_all(&dir, &id),
        Some(false),
        "the identity stays on file as a known identity granting nothing"
    );
    bound_both("after untrust --identity");
    let after_untrust = fs::read(&trust_path).unwrap();
    assert_ne!(after_untrust, seeded);

    // With no grant left, the identity-wide untrust is refused in the documented
    // sentence before any prompt, and the file is byte-identical afterwards.
    let refusal = format!(
        "Identity {id} holds no grant for all destinations, so there is no grant to withdraw; its 2 instance(s) are trusted on their own. .mesh untrust <destination> stops serving one, .mesh forget --identity {id} erases them all."
    );
    for line in [
        format!(".mesh untrust --identity {id}"),
        format!(".mesh untrust --identity {id} --confirm {token}"),
    ] {
        let out = trust_verb(&mut session, &line, &refusal);
        assert!(
            out.contains(&format!("Error: {refusal}")),
            "{line}: {out:?}"
        );
        assert!(
            !out.contains("Untrusting identity"),
            "{line}: refused before any preview: {out:?}"
        );
        assert_eq!(
            fs::read(&trust_path).unwrap(),
            after_untrust,
            "{line}: the refusal writes nothing"
        );
    }

    // `forget --identity` is the verb that erases the identity with its instances and
    // the deny; it takes its own token.
    let out = trust_verb(
        &mut session,
        &format!(".mesh forget --identity {id} --confirm forget-12121212"),
        "Forgot identity 12121212",
    );
    assert!(!out.contains("Error:"), "{out:?}");
    for section in [
        "identities",
        "destinations",
        "denied_destinations",
        "blocked_identities",
    ] {
        assert!(
            trust_file_keys(&dir, section).is_empty(),
            "after forget --identity: {section} must be empty"
        );
    }

    let status = exit_clean(&mut session);
    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
}

/// Cold start with a seeded list that trusts one instance on its own: the identity entry
/// grants nothing and exists only to bind the record. `untrust <destination>` removes
/// that record alone and says the identity stays known; the entry it leaves behind
/// holds no grant and names no instance, and `untrust --identity` then previews that
/// entry as the thing it removes (no instance rows, the token named), writes nothing
/// until the token comes back, removes the entry with the token and refuses a repeat
/// as an identity the list does not hold.
#[test]
fn untrust_of_the_last_instance_leaves_a_binding_only_entry_that_untrust_identity_removes() {
    let relay = SilentRelay::start();
    let probe = Probe::new("trust-binding-only", true, &[relay.port]);
    let dest = "ab".repeat(16);
    let id = "12".repeat(16);
    let mesh_dir = probe.config_dir.join("mesh");
    fs::create_dir_all(&mesh_dir).unwrap();
    let trust_path = mesh_dir.join("trust.yaml");
    fs::write(
        &trust_path,
        format!(
            "version: 2\n\
             identities:\n\
             \x20 {id}:\n\
             \x20   added_at: '2026-01-01T00:00:00Z'\n\
             \x20   last_seen_at: '2026-01-01T00:00:00Z'\n\
             \x20   label: Ida\n\
             \x20   note: null\n\
             \x20   all_destinations: false\n\
             destinations:\n\
             \x20 {dest}:\n\
             \x20   identity: {id}\n\
             \x20   added_at: '2026-01-01T00:00:00Z'\n\
             \x20   last_seen_at: '2026-01-01T00:00:00Z'\n\
             \x20   label: null\n\
             \x20   note: null\n\
             denied_destinations: {{}}\n\
             blocked_identities: {{}}\n"
        ),
    )
    .unwrap();
    let seeded = fs::read(&trust_path).unwrap();
    let mut command = probe.command();
    command.args(["--session", "probe"]);
    let mut session = spawn_repl(command);
    let startup = strip_escapes(&wait_for(&mut session, SESSION_PROMPT));
    assert!(startup.contains(MESH_ON_SUMMARY), "{startup:?}");
    probe.wait_for_marker("MESH_STARTED");
    let dir = probe.config_dir.clone();
    let token = "untrust-12121212";

    // The dry run of the instance untrust previews the record going and writes nothing.
    let preview = "This untrusts instance abababab: its destination record goes; the identity 12121212 stays known. It is served no longer; a knock from it will be filed again.";
    let out = trust_verb(
        &mut session,
        &format!(".mesh untrust {dest} --dry-run"),
        preview,
    );
    assert!(
        out.contains("This was a dry run; nothing changed."),
        "{out:?}"
    );
    assert_eq!(
        fs::read(&trust_path).unwrap(),
        seeded,
        "a dry run writes nothing"
    );

    // `--yes` removes the record alone: the identity entry stays, granting nothing.
    let out = trust_verb(
        &mut session,
        &format!(".mesh untrust {dest} --yes"),
        "Untrusted Ida (abababab): its destination record is gone; the identity 12121212 stays known. It is served no longer; a knock from it will be filed again.",
    );
    assert!(!out.contains("Error:"), "{out:?}");
    assert!(
        trust_file_keys(&dir, "destinations").is_empty(),
        "the destination record is gone"
    );
    assert_eq!(trust_file_keys(&dir, "identities"), vec![id.clone()]);
    assert_eq!(identity_grants_all(&dir, &id), Some(false));
    assert!(trust_file_keys(&dir, "denied_destinations").is_empty());
    let after_untrust = fs::read(&trust_path).unwrap();

    // A second untrust of the same hash is the teaching error for a hash the list does
    // not hold, naming block and trust; nothing is written.
    let out = trust_verb(
        &mut session,
        &format!(".mesh untrust {dest}"),
        &format!(
            "Destination {dest} is not in the trust list, so there is nothing to untrust; .mesh block {dest} refuses it, .mesh trust {dest} serves it."
        ),
    );
    assert!(out.contains("Error:"), "{out:?}");
    assert_eq!(fs::read(&trust_path).unwrap(), after_untrust);

    // The identity-wide untrust previews the binding-only entry as what it removes: no
    // grant, no instance rows, its own token; a dry run writes nothing.
    let out = trust_verb(
        &mut session,
        &format!(".mesh untrust --identity {id}"),
        &format!(
            "This was a dry run; nothing changed. To remove the entry, run: .mesh untrust --identity {id} --confirm {token}. To erase the identity with its instances, denies and knocks, run .mesh forget --identity {id}."
        ),
    );
    assert!(
        out.contains(&format!(
            "Untrusting identity {id} removes its entry from the trust list, which holds no grant and names no instance of it:"
        )),
        "{out:?}"
    );
    assert!(
        out.contains("identity 12121212") && out.contains("listed instances only"),
        "{out:?}"
    );
    assert!(
        !out.contains("instance(s) stay trusted") && !out.contains(&format!("instance {dest}")),
        "no instance is bound, so none is listed: {out:?}"
    );
    assert!(!out.contains("Error:"), "{out:?}");
    assert_eq!(
        fs::read(&trust_path).unwrap(),
        after_untrust,
        "a dry run writes nothing"
    );

    // The token removes the entry; the identity is a stranger and the line says what
    // grants it again.
    let out = trust_verb(
        &mut session,
        &format!(".mesh untrust --identity {id} --confirm {token}"),
        &format!(
            "Untrusted identity 12121212: the trust list no longer names it; no instance of it is trusted and the envoy's memory of it is gone. .mesh trust --identity {id} grants it again."
        ),
    );
    assert!(!out.contains("Error:"), "{out:?}");
    for section in [
        "identities",
        "destinations",
        "denied_destinations",
        "blocked_identities",
    ] {
        assert!(
            trust_file_keys(&dir, section).is_empty(),
            "after untrust --identity: {section} must be empty"
        );
    }
    let emptied = fs::read(&trust_path).unwrap();

    // A repeat is refused as an identity the list does not hold, and writes nothing.
    let out = trust_verb(
        &mut session,
        &format!(".mesh untrust --identity {id} --confirm {token}"),
        &format!(
            "Identity {id} is not in the trust list, so there is nothing to untrust; .mesh trust --identity {id} grants it, .mesh block --identity {id} silences it."
        ),
    );
    assert!(out.contains("Error:"), "{out:?}");
    assert_eq!(fs::read(&trust_path).unwrap(), emptied);

    let status = exit_clean(&mut session);
    assert!(matches!(status, WaitStatus::Exited(_, 0)), "{status:?}");
}
