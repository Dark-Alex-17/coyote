//! Black-box coverage of the mesh autostart through the interactive REPL
//! (TASK-100 criterion (e)), spec-first: `mesh.enabled: true` joins the mesh
//! when the REPL starts, before the first prompt, without asking; a failed
//! `.mesh on` precondition prints that refusal and leaves the mesh off with a
//! usable prompt; `mesh.enabled: false` never mentions joining. The one-shot
//! side of (e) (`--macro` never autostarts) lives in `macro_bracketing.rs`.
//!
//! Drives the real `coyote` binary through a pty like `envoy_repl.rs`, against
//! a private-relay interface pointing at a loopback listener this test owns
//! (accepted and held silent), so nothing leaves the machine and no fixed
//! port is taken. Every wait is an `expect` bounded by `EXPECT_TIMEOUT`.
//!
//! **Unix only**, for the same reasons as `pty_repl.rs`.

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

/// The binary is a cold debug build that opens a vault, resolves a model and,
/// here, starts a Reticulum transport before its first prompt.
const EXPECT_TIMEOUT: Duration = Duration::from_secs(60);

const TERMINAL_COLUMNS: u16 = 160;
const TERMINAL_ROWS: u16 = 50;

const CURSOR_POSITION_QUERY: &str = r"\x1b\[6n";
const CURSOR_POSITION_REPLY: &str = "\x1b[2;1R";

/// Prompt tails, as regexes: reedline colours the pieces separately. With a
/// session the left prompt reads `dry-model)probe)`, without one `dry-model)>`.
const SESSION_PROMPT: &str = r"dry-model\)(\x1b\[[0-9;]*m)*probe(\x1b\[[0-9;]*m)*\)";
const BARE_PROMPT: &str = r"dry-model\)(\x1b\[[0-9;]*m)*>";

/// The one line `mesh::autostart` adds over `.mesh on --yes`.
const AUTOSTART_NOTICE: &str =
    "mesh.enabled is true in config.yaml: joining the mesh for this session";
const MESH_ON_SUMMARY: &str = "Mesh is on for this session";
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
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || {
                    let mut stream = stream;
                    let mut sink = [0u8; 1024];
                    while matches!(stream.read(&mut sink), Ok(n) if n > 0) {}
                });
            }
        });
        Self { port }
    }
}

struct Probe {
    config_dir: PathBuf,
    home_dir: PathBuf,
    temp_root: PathBuf,
}

impl Probe {
    fn new(label: &str, mesh_enabled: bool, relay_port: u16) -> Self {
        let config_dir = fresh_dir(&format!("{label}-cfg"));
        let home_dir = fresh_dir(&format!("{label}-home"));
        let temp_root = fresh_dir(&format!("{label}-tmp"));
        let vault_pass = config_dir.join("vault-pass");
        fs::write(&vault_pass, "test-password\n").unwrap();
        let started = marker_command("MESH_STARTED", &config_dir);
        let stopped = marker_command("MESH_STOPPED", &config_dir);
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
                 \x20   - {{type: private, host: 127.0.0.1, port: {relay_port}}}\n",
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
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.marker_count(marker) == 0 {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the {marker} hook marker"
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
    let mut before = Vec::new();
    loop {
        let captures = session
            .expect(Any([Regex(pattern), Regex(CURSOR_POSITION_QUERY)]))
            .unwrap_or_else(|err| panic!("waiting for {pattern:?}: {err}"));
        before.extend_from_slice(captures.before());
        if captures.get(0) == Some(b"\x1b[6n".as_slice()) {
            session
                .send(CURSOR_POSITION_REPLY)
                .expect("reply to the cursor position query");
            continue;
        }
        return before;
    }
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

/// (e) positive: with a session held, `mesh.enabled: true` joins before the
/// first prompt with the autostart notice and the `.mesh on` summary, asks
/// nothing, fires `mesh.started`, and a node verb works at the first prompt.
/// `.exit` stops the node (`mesh.stopped`) and exits clean.
#[test]
fn repl_with_mesh_enabled_joins_before_the_first_prompt_without_asking() {
    let relay = SilentRelay::start();
    let probe = Probe::new("joins", true, relay.port);
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
        !startup.contains("Turn mesh on?") && !startup.contains("[y/n]"),
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

/// (e) failed precondition: `mesh.enabled: true` with no session prints the
/// autostart notice and the very `.mesh on` refusal, then goes on to a usable
/// prompt with the mesh off; no node is started (no `mesh.started`).
#[test]
fn repl_with_mesh_enabled_but_no_session_prints_the_mesh_on_refusal_and_stays_off() {
    let relay = SilentRelay::start();
    let probe = Probe::new("no-session", true, relay.port);
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

/// (e) unchanged: `mesh.enabled: false` with a session held never mentions
/// joining and the mesh is off at the first prompt.
#[test]
fn repl_with_mesh_disabled_never_mentions_joining() {
    let relay = SilentRelay::start();
    let probe = Probe::new("disabled", false, relay.port);
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
