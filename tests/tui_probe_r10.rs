//! Cold-start cases for the full-screen UI, written from its documented
//! contract before the implementation was read. Each case boots the real
//! binary cold (fresh config and cache dirs, no model) and exercises a
//! usage pattern the lifecycle suite does not pin:
//! non-interactive modes under `COYOTE_UI=tui`, malformed input bytes,
//! wide graphemes, over-width drafts, repeated commands, resize bursts and
//! a cold start on a tiny terminal. Unix-only for the same reasons as the
//! lifecycle suite; cases run one at a time because each takes a
//! controlling tty.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process};

use common::pty::{Pty, strip_ansi};
use serial_test::serial;

/// Covers the keyboard-enhancement probe, which waits two seconds for an
/// answer a pseudo-terminal never gives.
const PTY_DEADLINE: Duration = Duration::from_secs(20);
/// Hotkeys count once typing has been idle for a second; settle before
/// sending a quit chord after other keys.
const HOTKEY_GAP: Duration = Duration::from_millis(1200);

const ENTER_ALT: &[u8] = b"\x1b[?1049h";
const LEAVE_ALT: &[u8] = b"\x1b[?1049l";

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fresh_config_dir(scenario: &str) -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = env::temp_dir().join(format!(
        "coyote-tui-probe-{scenario}-{}-{unique}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    dir
}

/// The documented model-free recipe: a dry-run model, streaming on, nothing
/// that could prompt on exit.
fn write_dry_run_config(dir: &Path) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             stream: true\n\
             save_session: false\n\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai\n\
             \x20   name: dryrun\n\
             \x20   auth: none\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: dry-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             workspace_instructions: false\n\
             memory: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

fn built_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_coyote"))
}

fn fixture_path() -> std::ffi::OsString {
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
    env::join_paths(
        iter::once(fixture_bin).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap()
}

/// Pins `COYOTE_CONFIG_DIR`/`COYOTE_CACHE_DIR`, strips every inherited
/// `COYOTE_*`, selects the TUI through the provisional env branch.
fn base_env(cmd: &mut Command, dir: &Path) {
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            cmd.env_remove(&key);
        }
    }
    cmd.current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("COYOTE_UI", "tui")
        .env("HOME", dir.join("home"))
        .env("TERM", "xterm-256color")
        .env("PATH", fixture_path())
        .env_remove("IS_SANDBOX")
        .env_remove("COLORTERM")
        .env_remove("NO_COLOR");
}

const TTY_REPORT: &str = "echo; echo \"exit=$rc\"; \
     echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done";

const WAIT_WRAPPER: &str =
    "'{coyote}' </dev/tty & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {report}";

/// The binary on a fresh pty under a bash wrapper that reports the exit
/// code and the tty flags once it is gone.
fn spawn_tui(dir: &Path) -> Pty {
    let mut cmd = Command::new("bash");
    base_env(&mut cmd, dir);
    cmd.arg("-c").arg(
        WAIT_WRAPPER
            .replace("{coyote}", built_binary().to_str().unwrap())
            .replace("{report}", TTY_REPORT),
    );
    Pty::spawn(cmd).unwrap()
}

/// The binary with no terminal at all: stdin from /dev/null, stdout and
/// stderr piped, the way a script or a pipe runs it.
fn run_piped(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(built_binary());
    base_env(&mut cmd, dir);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().unwrap()
}

fn assert_tty_restored(transcript: &str) {
    let flags = transcript
        .lines()
        .find_map(|line| line.strip_prefix("tty-flags: "))
        .unwrap_or_else(|| panic!("no tty-flags line in the transcript:\n{transcript}"));
    let tokens: Vec<&str> = flags
        .split(|c: char| c.is_whitespace() || c == ';')
        .collect();
    for flag in ["echo", "icanon", "isig", "opost"] {
        assert!(
            tokens.contains(&flag),
            "stty did not report {flag} after the UI left:\n{transcript}"
        );
        let off = format!("-{flag}");
        assert!(
            !tokens.contains(&off.as_str()),
            "the terminal was left with {flag} off:\n{transcript}"
        );
    }
}

fn exit_code(transcript: &str) -> i32 {
    transcript
        .lines()
        .find_map(|line| line.strip_prefix("exit="))
        .unwrap_or_else(|| panic!("no exit= line in the transcript:\n{transcript}"))
        .trim()
        .parse()
        .unwrap()
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack[from.min(haystack.len())..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|at| from + at)
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    let mut n = 0;
    let mut from = 0;
    while let Some(at) = find(haystack, needle, from) {
        n += 1;
        from = at + needle.len();
    }
    n
}

fn squeezed(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Waits until the whitespace-squeezed screen text contains `marker` at
/// or after raw offset `from`.
fn wait_for_squeezed_after(pty: &mut Pty, marker: &str, from: usize, what: &str) -> String {
    let wanted = squeezed(marker);
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    loop {
        let text = pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(
            &pty.raw()[from.min(pty.raw().len())..],
        ));
        if squeezed(&since).contains(&wanted) {
            return since;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: {marker:?} never appeared on screen:\n{text}"
        );
    }
}

/// The hint, the model and the session tokens must all be on screen
/// again after `what`, without waiting for anything else to change.
fn assert_repainted_after(pty: &mut Pty, from: usize, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let repainted = strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]));
        let missing: Vec<&str> = ["Type a prompt", "dryrun:dry-model", "no session"]
            .into_iter()
            .filter(|token| !repainted.contains(token))
            .collect();
        if missing.is_empty() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "after {what} the screen was not repainted; still missing {missing:?}. \
             Printable bytes since: {repainted:?}"
        );
        pty.read_for(Duration::from_millis(200));
    }
}

/// Quits from an idle, empty composer and checks the common exit
/// promises: exit 0, cooked tty, the alternate screen left exactly once.
fn quit_and_check(mut pty: Pty, what: &str) -> String {
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{what}:\n{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        1,
        "{what}: the alternate screen was left more than once (a panic restore?):\n{transcript}"
    );
    transcript
}

// ---------------------------------------------------------------------
// Non-interactive modes under the provisional env branch. The plan routes
// `COYOTE_UI=tui` to `tui::run` only for the interactive mode; a positional
// prompt or an inspection flag is the plain command path, which has no
// business with the alternate screen and must work without a tty.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn a_positional_prompt_under_coyote_ui_tui_is_the_plain_command_path() {
    let dir = fresh_config_dir("cmd-mode");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);

    let out = run_piped(&dir, &["zebra quartz probe-marker"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        find(&out.stdout, ENTER_ALT, 0).is_none() && find(&out.stderr, ENTER_ALT, 0).is_none(),
        "a command-mode run entered the alternate screen:\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        out.status.success(),
        "command mode under COYOTE_UI=tui did not exit 0 ({:?}):\nstdout={stdout}\nstderr={stderr}",
        out.status
    );
    assert!(
        stdout.contains("probe-marker"),
        "the dry-run reply did not reach stdout:\nstdout={stdout}\nstderr={stderr}"
    );
}

#[test]
#[serial]
fn the_info_flag_under_coyote_ui_tui_prints_the_table_without_a_tty() {
    let dir = fresh_config_dir("info-flag");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);

    let out = run_piped(&dir, &["--info"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        find(&out.stdout, ENTER_ALT, 0).is_none() && find(&out.stderr, ENTER_ALT, 0).is_none(),
        "--info entered the alternate screen:\nstdout={stdout}\nstderr={stderr}"
    );
    assert!(
        out.status.success(),
        "--info under COYOTE_UI=tui did not exit 0 ({:?}):\nstdout={stdout}\nstderr={stderr}",
        out.status
    );
    assert!(
        stdout.contains("dryrun:dry-model"),
        "--info did not print the model row:\nstdout={stdout}\nstderr={stderr}"
    );
}

// ---------------------------------------------------------------------
// Robustness of the one stdin reader: bytes a terminal may legitimately
// deliver but no key produces (a bare ESC, a truncated CSI, an absurd
// parameter, invalid UTF-8, NUL, C1 controls, DEL) must neither panic the
// UI nor leave it deaf.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn malformed_input_bytes_neither_panic_the_ui_nor_leave_it_deaf() {
    let dir = fresh_config_dir("garbage");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    let bursts: [&[u8]; 7] = [
        b"\x1b",                            // bare ESC, nothing follows for a while
        b"\x1b[",                           // truncated CSI
        b"\x1b[99999999999999999m",         // parameter that overflows u16/u32
        b"\xff\xfe\xc3",                    // invalid UTF-8 and a dangling lead byte
        b"\x00\x7f",                        // NUL and DEL
        b"\x9b\x85\xc2\x9b",                // C1 CSI raw and as UTF-8
        b"\x1b[<35;1;1M\x1b[200~\x1b[201~", // SGR mouse report, empty paste
    ];
    for burst in bursts {
        pty.send(burst).unwrap();
        pty.read_for(Duration::from_millis(250));
    }

    // Still alive and still taking keys: a command runs and paints. A
    // typed letter plus Ctrl+C clears whatever the garbage left in the
    // draft (Ctrl+C on an empty composer would quit instead).
    let before = pty.raw().len();
    pty.send(b"z").unwrap();
    pty.read_for(Duration::from_millis(200));
    pty.send(b"\x03").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", before, PTY_DEADLINE)
        .expect("keys were not handled after the malformed bytes");
    assert!(
        find(pty.raw(), LEAVE_ALT, 0).is_none(),
        "the alternate screen was left while the UI should still be up"
    );

    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after malformed input bytes");
}

// ---------------------------------------------------------------------
// Wide graphemes and over-width drafts: the composer and the transcript
// have to carry text a terminal paints two cells wide (emoji, CJK, VS16)
// and a draft longer than the row, then render the dry-run echo of both.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn wide_graphemes_typed_into_the_composer_come_back_in_the_rendered_reply() {
    let dir = fresh_config_dir("wide");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    // The asterisks are stripped by rendering, so the plain form can only
    // come from the assistant cell, not from the typed draft or user cell.
    pty.send(
        "**\u{1F98A} fox** \u{65E5}\u{672C} \u{2714}\u{FE0F} h\u{E9}llo wide-marker\r".as_bytes(),
    )
    .unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "\u{1F98A} fox \u{65E5}\u{672C}",
        before,
        "a wide-grapheme prompt",
    );
    let since = wait_for_squeezed_after(&mut pty, "wide-marker", before, "a wide-grapheme prompt");
    assert!(
        since.contains("h\u{E9}llo"),
        "the composed character did not survive the round trip:\n{since}"
    );

    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after a wide-grapheme prompt");
}

#[test]
#[serial]
fn a_draft_longer_than_the_row_submits_and_its_reply_renders() {
    let dir = fresh_config_dir("long-draft");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    let filler: String = (0..280)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let prompt = format!("{filler} **longtail** end-marker\r");
    pty.send(prompt.as_bytes()).unwrap();
    let since = wait_for_squeezed_after(
        &mut pty,
        "longtail end-marker",
        before,
        "an over-width draft",
    );
    // The filler itself (280 columns on a 100-column screen) reached the
    // transcript in one piece, wrapped but not truncated.
    assert!(
        squeezed(&since).contains(&filler),
        "the over-width draft was truncated on its way to the transcript:\n{since}"
    );

    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after an over-width draft");
}

// ---------------------------------------------------------------------
// Repeating a command turn: each `.info` yields its own Command cell with
// its own copy of the table; the second never replaces or empties the
// first and the UI stays live between them.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn two_dot_info_turns_yield_two_tables_and_the_ui_stays_live() {
    let dir = fresh_config_dir("info-twice");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(500));

    let second_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", second_at, PTY_DEADLINE)
        .expect("the second .info painted no table");
    pty.read_for(Duration::from_millis(500));
    // A full repaint (resize round trip) shows the transcript as it
    // stands: the newest table is on screen with its own prompt row, and
    // the screen is not blank or stuck on the first turn.
    let repaint_at = pty.raw().len();
    pty.resize(39, 100).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.resize(40, 100).unwrap();
    // The table is taller than the 40-row screen, so only its tail and the
    // footer are visible after a full repaint; `.info`'s own row has
    // scrolled off, which is the transcript's job, not a loss.
    pty.wait_for_after("log_path", repaint_at, PTY_DEADLINE)
        .expect("the transcript did not repaint after the second .info");

    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after two .info turns");
}

// ---------------------------------------------------------------------
// Resize handling: a burst of window-size changes (a user dragging the
// edge) is coalesced into frames rather than drawn once per event, and the
// UI is live and clean afterwards. A terminal that is tiny from the very
// first frame (started inside a small split) comes up and leaves clean too.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn a_burst_of_resizes_leaves_the_ui_live_and_painted_once_per_size_at_most() {
    let dir = fresh_config_dir("resize-burst");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    // A run of sizes with no pause between them, the way SIGWINCH arrives
    // while an edge is dragged; the last one differs from the start size,
    // so a repaint is owed.
    let sizes: Vec<u16> = (50..=100)
        .step_by(5)
        .rev()
        .chain((55..=90).step_by(5))
        .collect();
    for &cols in &sizes {
        pty.resize(30, cols).unwrap();
    }
    pty.read_for(Duration::from_millis(1500));
    assert_repainted_after(&mut pty, before, "a burst of resizes ending at 90x30");
    // Frames are coalesced: each full repaint paints the footer hint once,
    // and a renderer that painted every event would paint once per size.
    // The burst lands as one repaint on an idle host; a second is allowed
    // for a frame that starts before the last size arrives under load.
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    let repaints = since.matches("Type a prompt").count();
    assert!(
        repaints <= 2,
        "{repaints} full repaints for {} resize events: frames were not coalesced",
        sizes.len()
    );

    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", before, PTY_DEADLINE)
        .expect("keys were not handled after the resize burst");
    pty.read_for(Duration::from_millis(300));

    // A second burst that ends back at the size it started from: a diff
    // renderer may owe no bytes for it, but the UI must still take keys.
    let same_size_at = pty.raw().len();
    for cols in [80u16, 60, 40, 60, 80, 90] {
        pty.resize(30, cols).unwrap();
    }
    pty.read_for(Duration::from_millis(800));
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", same_size_at, PTY_DEADLINE)
        .expect("keys were not handled after a resize burst back to the same size");
    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after a resize burst");
}

#[test]
#[serial]
fn a_cold_start_on_a_tiny_terminal_comes_up_and_leaves_clean() {
    let dir = fresh_config_dir("tiny-start");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir);
    // Shrink before the first frame: startup probes the keyboard for two
    // seconds, so the resize lands well before anything is drawn.
    pty.resize(4, 20).unwrap();

    pty.wait_for_raw(ENTER_ALT, 0, PTY_DEADLINE)
        .expect("the alternate screen was never entered on a 20x4 terminal");
    pty.read_for(Duration::from_secs(3));
    assert!(
        find(pty.raw(), LEAVE_ALT, 0).is_none(),
        "the UI left the alternate screen on its own on a 20x4 terminal (a panic?)"
    );

    let before = pty.raw().len();
    pty.resize(40, 100).unwrap();
    assert_repainted_after(&mut pty, before, "growing a 20x4 cold start to 100x40");
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", before, PTY_DEADLINE)
        .expect("keys were not handled after a tiny cold start");
    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after a tiny cold start");
}
