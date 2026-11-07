//! Transcript cases for the full-screen UI: what the real binary shows a
//! first-time user in the scrollback, exercised on a pseudo-terminal from
//! a cold start (fresh config and cache dirs, a dry-run model, or the
//! scripted endpoint when a turn has to be recorded). Unix-only for the
//! same reasons as the lifecycle suite; cases run one at a time because
//! each takes a controlling tty.
//!
//! Covered here and nowhere else in `tests/`:
//! - the welcome block leaves for the first reply, not for typing, and
//!   names the model;
//! - the welcome block carries no colour under `NO_COLOR`;
//! - `.undo` removes the last exchange's cells back to the previous user
//!   cell (against a recording session: a dry-run turn is never saved);
//! - `.undo` with nothing to undo, a terminal shrunk to a few cells, and
//!   an emoji/VS16 prompt leave the UI alive and quitting cleanly;
//! - undoing the only exchange stops at the command cell before it and
//!   leaves that cell in place;
//! - the "Thinking for Ns" status row is live while a reasoning delta is
//!   held;
//! - `F2` shows and hides the Todo panel;
//! - scrolling up raises the follow chip, `End` and a submitted line
//!   clear it;
//! - a tool call is one cell that finishes in place (its start and end
//!   events pair up), never a running cell plus a finished one;
//! - a finished tool row reads `name(args)  elapsed  ok`, folds its output
//!   away, and opens/closes on `Enter` once `Ctrl+Up` focused it; `Esc`
//!   drops the focus and a draft makes `Enter` submit instead;
//! - a failed tool row's header ends in `failed: <first line>` of the
//!   error; the opened row carries the rest;
//! - cells that land while the view is scrolled up count on the follow
//!   chip and `End` brings them into view;
//! - `todo__*` calls never take a transcript row; the Todo panel shows them;
//! - Shift+Up and a mouse wheel notch detach the view like PgUp does; PgDn
//!   walks back to the bottom, and Shift+G jumps there only while the
//!   composer is empty;
//! - a streamed `<think>` span never shows in the assistant cell, during
//!   the stream or after it;
//! - a folded header cuts a long argument to its budget;
//! - a bookkeeping call (`job__list`) takes one line with no status;
//! - a mouse click on a folded tool row opens it and a second click folds
//!   it again;
//! - a narrower terminal re-wraps the cells in place without duplicating
//!   a row;
//! - Shift+G with the view at the bottom is plain typing, before a scroll
//!   and right after a jump;
//! - a think span whose `</think>` arrives late (after a hold, or only at
//!   the end of a reasoning-only reply) never shows;
//! - Esc over a focused row mid-turn interrupts the turn;
//! - F2, PgUp and End keep working under a tool's Confirm modal and the
//!   Input modal, and leave the question up;
//! - a folded tool header at forty columns is one row ending in `ok`.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process};

use common::fake_llm::{Chunk, FakeLlm, HOLD_LIMIT, Reply};
use common::pty::{Pty, strip_ansi};
use serial_test::serial;

/// Covers the keyboard-enhancement probe, which waits two seconds for an
/// answer a pseudo-terminal never gives.
const PTY_DEADLINE: Duration = Duration::from_secs(20);
/// Hotkeys count only once the last keystroke is at least HOTKEY_IDLE
/// (1 s) old; 1200 ms settles it.
const HOTKEY_GAP: Duration = Duration::from_millis(1200);

const F2: &[u8] = b"\x1bOQ";
const PAGE_UP: &[u8] = b"\x1b[5~";
const PAGE_DOWN: &[u8] = b"\x1b[6~";
const END: &[u8] = b"\x1b[F";
const SHIFT_UP: &[u8] = b"\x1b[1;2A";
const CTRL_UP: &[u8] = b"\x1b[1;5A";
const ESC: &[u8] = b"\x1b";
const CTRL_L: &[u8] = b"\x0c";
const CTRL_D: &[u8] = b"\x04";
const CTRL_C: &[u8] = b"\x03";
/// One SGR-encoded wheel-up notch at column 50, row 5.
const WHEEL_UP: &[u8] = b"\x1b[<64;50;5M";

const CHIP: &str = "End=follow";

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
        "coyote-tui-transcript-{scenario}-{}-{unique}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    dir
}

/// A model-free config: a dry-run model, streaming on, nothing that could
/// prompt on exit.
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

/// The dry-run config with its only client pointed at `fake` instead, so
/// a turn is answered by the scripted endpoint and recorded in the session.
fn write_fake_llm_config(dir: &Path, fake: &FakeLlm) {
    write_fake_llm_config_with(dir, fake, "");
}

/// [`write_fake_llm_config`] with `extra` top-level YAML lines appended
/// (each line newline-terminated).
fn write_fake_llm_config_with(dir: &Path, fake: &FakeLlm, extra: &str) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: fake:fake-model\n\
             function_calling_support: true\n\
             stream: true\n\
             save_session: false\n\
             vault_password_file: '{vault_pass}'\n\
             clients:\n\
             \x20 - type: openai-compatible\n\
             \x20   name: fake\n\
             \x20   api_base: '{api_base}'\n\
             \x20   api_key: 'unused'\n\
             \x20   models:\n\
             \x20     - name: fake-model\n\
             \x20       max_input_tokens: 100000\n\
             \x20       supports_function_calling: true\n\
             workspace_instructions: false\n\
             memory: false\n{extra}",
            vault_pass = vault_pass.display(),
            api_base = fake.api_base(),
            extra = extra,
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
fn spawn_tui(dir: &Path, extra_env: &[(&str, &str)]) -> Pty {
    let mut cmd = Command::new("bash");
    base_env(&mut cmd, dir);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    cmd.arg("-c").arg(
        WAIT_WRAPPER
            .replace("{coyote}", built_binary().to_str().unwrap())
            .replace("{report}", TTY_REPORT),
    );
    Pty::spawn(cmd).unwrap()
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
            "{what}: {marker:?} never appeared on screen; searched since offset {from}:\n{since}\nlast read:\n{text}"
        );
    }
}

/// Ctrl+L clears the screen and repaints everything, so the bytes after it
/// are the whole visible frame as it stands. Returns that frame squeezed.
fn repaint(pty: &mut Pty) -> String {
    let at = pty.raw().len();
    pty.send(CTRL_L).unwrap();
    pty.read_for(Duration::from_secs(1));
    squeezed(&strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..])))
}

/// Ctrl+L, then the frame split into screen rows by following the
/// cursor-position sequences the repaint carries. Rows are 1-based as a
/// terminal reports them; each is the squeezed text written on it.
fn repaint_rows(pty: &mut Pty) -> Vec<(u16, String)> {
    let at = pty.raw().len();
    pty.send(CTRL_L).unwrap();
    pty.read_for(Duration::from_secs(1));
    let text = String::from_utf8_lossy(&pty.raw()[at..]).into_owned();
    let chars: Vec<char> = text.chars().collect();
    let mut rows: std::collections::BTreeMap<u16, String> = Default::default();
    let mut row: u16 = 1;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '\x1b' {
            if !chars[i].is_whitespace() && !chars[i].is_control() {
                rows.entry(row).or_default().push(chars[i]);
            }
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('[') => {
                let start = i + 2;
                let mut j = start;
                while matches!(chars.get(j), Some(c) if ('\x30'..='\x3f').contains(c)) {
                    j += 1;
                }
                while matches!(chars.get(j), Some(c) if ('\x20'..='\x2f').contains(c)) {
                    j += 1;
                }
                let fin = chars.get(j).copied();
                if matches!(fin, Some('H') | Some('f')) {
                    let params: String = chars[start..j].iter().collect();
                    row = params
                        .split(';')
                        .next()
                        .and_then(|r| r.parse().ok())
                        .unwrap_or(1)
                        .max(1);
                }
                i = if fin.is_some() { j + 1 } else { j };
            }
            Some(']' | 'P' | '_' | '^' | 'X') => {
                i += 2;
                while i < chars.len() && chars[i] != '\x07' && chars[i] != '\x1b' {
                    i += 1;
                }
                i += 1;
            }
            Some(_) => i += 2,
            None => i += 1,
        }
    }
    rows.into_iter().collect()
}

fn boot(scenario: &str, extra_env: &[(&str, &str)]) -> (TempDirGuard, Pty) {
    let dir = fresh_config_dir(scenario);
    let guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, extra_env);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    (guard, pty)
}

fn boot_with_fake(scenario: &str, fake: &FakeLlm) -> (TempDirGuard, Pty) {
    let dir = fresh_config_dir(scenario);
    let guard = TempDirGuard(dir.clone());
    write_fake_llm_config(&dir, fake);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    (guard, pty)
}

/// Ctrl+D on the empty composer; the UI has to leave cleanly and hand the
/// terminal back as it found it.
fn quit(mut pty: Pty) {
    pty.read_for(HOTKEY_GAP);
    pty.send(CTRL_D).unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// The first SGR sequence in `raw` that sets a foreground or background
/// colour (basic, bright, 256 or truecolour). Bold, dim, italic,
/// underline and reset are not colours and stay allowed.
fn has_colour_sgr(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    let mut rest: &str = &text;
    while let Some(at) = rest.find("\x1b[") {
        let after = &rest[at + 2..];
        let end = after
            .find(|c: char| ('\x40'..='\x7e').contains(&c))
            .unwrap_or(after.len());
        let (params, fin) = (&after[..end], after.get(end..end + 1).unwrap_or(""));
        if fin == "m" {
            for p in params.split(';') {
                if let Ok(n) = p.parse::<u16>()
                    && matches!(n, 30..=38 | 40..=48 | 90..=97 | 100..=107)
                {
                    return Some(format!("\\x1b[{params}m"));
                }
            }
        }
        rest = &after[end.min(after.len())..];
    }
    None
}

// ---------------------------------------------------------------------
// The welcome block fills the empty transcript on boot and leaves for
// good once the first reply is in. The dry-run echo renders the prompt's
// markdown, so the asterisk-free form of the prompt can only come from
// the assistant cell, never from the composer echo or the user cell.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn the_welcome_block_leaves_after_the_first_reply() {
    let dir = fresh_config_dir("welcome");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    let booted = pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let booted = squeezed(&strip_ansi(&booted));
    assert!(
        booted.contains(&squeezed("Tip: Press Ctrl-C")),
        "the welcome block did not show the interrupt hint on boot:\n{booted}"
    );
    assert!(
        booted.contains(&squeezed(concat!("coyote ", env!("CARGO_PKG_VERSION")))),
        "the welcome block did not show the wordmark and version on boot:\n{booted}"
    );

    let before = pty.raw().len();
    pty.send(b"**bye** welcome-marker\r").unwrap();
    wait_for_squeezed_after(&mut pty, "bye welcome-marker", before, "the first reply");
    pty.read_for(Duration::from_millis(500));

    let repainted = repaint(&mut pty);
    assert!(
        repainted.contains(&squeezed("bye welcome-marker")),
        "the repaint after the first reply does not show the reply:\n{repainted}"
    );
    assert!(
        !repainted.contains(&squeezed("Tip: Press Ctrl-C")),
        "the welcome block is still on screen after the first reply:\n{repainted}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// The welcome block carries the same facts the REPL banner prints (name
// and version, model, interrupt hint, .help pointer) and typing alone
// does not dismiss it.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn the_welcome_block_names_the_model_and_survives_typing() {
    let (_guard, mut pty) = boot("welcome-typing", &[]);

    let frame = repaint(&mut pty);
    for fact in [
        concat!("coyote ", env!("CARGO_PKG_VERSION")),
        "model: dryrun:dry-model",
        "Tip: Press Ctrl-C",
        "\".help\"",
    ] {
        assert!(
            frame.contains(&squeezed(fact)),
            "the welcome block lacks {fact:?} on boot:\n{frame}"
        );
    }

    pty.send(b"typed-but-not-sent").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains(&squeezed("typed-but-not-sent")),
        "the composer did not echo the typed text:\n{frame}"
    );
    assert!(
        frame.contains(&squeezed("Tip: Press Ctrl-C")),
        "typing (without Enter) dismissed the welcome block:\n{frame}"
    );

    let before = pty.raw().len();
    pty.send(b"\r").unwrap();
    wait_for_squeezed_after(&mut pty, "typed-but-not-sent", before, "the first prompt");
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains(&squeezed("Tip: Press Ctrl-C")),
        "the welcome block is still on screen after the first submitted line:\n{frame}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// Under NO_COLOR the welcome frame carries no colour SGR.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn the_welcome_block_has_no_colour_under_no_color() {
    let (_guard, mut pty) = boot("welcome-no-color", &[("NO_COLOR", "1")]);

    let at = pty.raw().len();
    pty.send(CTRL_L).unwrap();
    pty.read_for(Duration::from_secs(1));
    let raw = &pty.raw()[at..];
    let text = strip_ansi(&String::from_utf8_lossy(raw));
    assert!(
        squeezed(&text).contains(&squeezed("Tip: Press Ctrl-C")),
        "the welcome block is missing under NO_COLOR:\n{text}"
    );
    if let Some(seq) = has_colour_sgr(raw) {
        panic!("NO_COLOR is set but the frame carries a colour SGR {seq}:\n{text}");
    }

    quit(pty);
}

// ---------------------------------------------------------------------
// Inside a session, `.undo` takes the second exchange (its `> ` user cell
// and its reply) out of the transcript and leaves the first. The undone
// prompt comes back as composer prefill, the REPL's long-standing `.undo`
// semantics, so only the `> `-prefixed cell is asserted gone. A dry-run
// turn is never saved to the session (`record_chat_completion` skips
// it), so the scripted endpoint answers.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn undo_removes_the_last_exchange_and_keeps_the_one_before() {
    let fake = FakeLlm::start(vec![
        Reply::Text("reply-alpha-xx".to_string()),
        Reply::Text("reply-bravo-yy".to_string()),
    ]);
    let (_guard, mut pty) = boot_with_fake("undo", &fake);

    pty.send(b".session scratch\r").unwrap();
    pty.read_for(Duration::from_secs(1));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains(".scratch."),
        "the status row does not name the scratch session:\n{frame}"
    );

    let before = pty.raw().len();
    pty.send(b"alphamarker one\r").unwrap();
    wait_for_squeezed_after(&mut pty, "reply-alpha-xx", before, "the first reply");
    pty.read_for(Duration::from_millis(500));

    let before = pty.raw().len();
    pty.send(b"bravomarker two\r").unwrap();
    wait_for_squeezed_after(&mut pty, "reply-bravo-yy", before, "the second reply");
    pty.read_for(Duration::from_millis(500));

    let frame = repaint(&mut pty);
    for cell in [
        ">alphamarkerone",
        "reply-alpha-xx",
        ">bravomarkertwo",
        "reply-bravo-yy",
    ] {
        assert!(
            frame.contains(cell),
            "{cell} should be on screen before .undo:\n{frame}"
        );
    }

    pty.send(b".undo\r").unwrap();
    pty.read_for(Duration::from_secs(1));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains(">bravomarkertwo"),
        ".undo left the second exchange's user cell on screen:\n{frame}"
    );
    assert!(
        !frame.contains("reply-bravo-yy"),
        ".undo left the second exchange's reply cell on screen:\n{frame}"
    );
    assert!(
        frame.contains(">alphamarkerone") && frame.contains("reply-alpha-xx"),
        ".undo removed more than the last exchange:\n{frame}"
    );
    assert!(
        frame.contains("bravomarkertwo"),
        ".undo did not hand the undone prompt back to the composer:\n{frame}"
    );

    // Ctrl+C clears the undone prompt out of the composer; Ctrl+D would
    // not quit over it.
    pty.send(CTRL_C).unwrap();
    pty.read_for(Duration::from_millis(300));
    quit(pty);
}

// =====================================================================
// The scroll paths the page keys do not cover, the think span of a
// streamed reply, the folded header's argument budget, a bookkeeping
// call's single line, a mouse click on a folded row, and a resize that
// must re-wrap without duplicating rows.
// =====================================================================

// ---------------------------------------------------------------------
// Shift+Up detaches the view like PgUp does and raises the chip; enough
// PgDn presses walk back to the bottom and clear it without End.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn shift_up_raises_the_chip_and_page_down_walks_back_to_the_bottom() {
    let (_guard, mut pty) = boot("shift-up", &[]);
    overflow_a_short_terminal(&mut pty);

    pty.read_for(HOTKEY_GAP);
    let before = pty.raw().len();
    pty.send(SHIFT_UP).unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "End = follow",
        before,
        "the follow chip after Shift+Up",
    );

    let mut frame = String::new();
    for _ in 0..6 {
        pty.send(PAGE_DOWN).unwrap();
        pty.read_for(Duration::from_millis(400));
        frame = repaint(&mut pty);
        if !frame.contains(CHIP) {
            break;
        }
    }
    assert!(
        !frame.contains(CHIP),
        "six PgDn presses did not bring the view back to the bottom:\n{frame}"
    );
    assert!(
        frame.contains("fillerline05"),
        "the bottom of the transcript is not in view after PgDn:\n{frame}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// A mouse wheel notch upward scrolls the view and raises the chip; End
// clears it again.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn the_mouse_wheel_raises_the_chip_and_end_clears_it() {
    let (_guard, mut pty) = boot("wheel", &[]);
    overflow_a_short_terminal(&mut pty);

    pty.read_for(HOTKEY_GAP);
    let before = pty.raw().len();
    pty.send(WHEEL_UP).unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "End = follow",
        before,
        "the follow chip after a wheel-up notch",
    );

    pty.send(END).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains(CHIP),
        "End did not clear the follow chip after a wheel scroll:\n{frame}"
    );
    assert!(
        frame.contains("fillerline05"),
        "End did not bring the newest output back into view:\n{frame}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// Shift+G is the keyboard's other way back to the bottom, but only from
// an empty composer: with a draft the letter is typed and the view stays
// where it is; once the draft is gone the same key clears the chip.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn shift_g_follows_the_bottom_only_from_an_empty_composer() {
    let (_guard, mut pty) = boot("shift-g", &[]);
    overflow_a_short_terminal(&mut pty);

    let before = pty.raw().len();
    pty.send(PAGE_UP).unwrap();
    wait_for_squeezed_after(&mut pty, "End = follow", before, "the chip after PgUp");

    pty.send(b"draftG").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains(CHIP),
        "G typed into a draft moved the view:\n{frame}"
    );
    assert!(
        frame.contains("draftG"),
        "G typed into a draft did not reach the composer:\n{frame}"
    );

    pty.send(CTRL_C).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"G").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains(CHIP),
        "Shift+G on the empty composer did not follow the bottom (the chip is still up):\n{frame}"
    );
    assert!(
        frame.contains("fillerline05"),
        "Shift+G did not bring the newest output back into view:\n{frame}"
    );
    assert!(
        !frame.contains("draftG"),
        "the cleared draft is still on screen:\n{frame}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// A reasoning model streams its thinking as a `<think>` span ahead of
// the answer. The assistant cell shows the answer alone: no tag and no
// word of the reasoning, during the stream or once the reply is done.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn a_streamed_think_span_never_shows_in_the_assistant_cell() {
    let fake = FakeLlm::start(vec![Reply::Streamed(vec![
        Chunk::Reasoning("weighing the options v4w".to_string()),
        Chunk::Text("the answer n7p".to_string()),
    ])]);
    let (_guard, mut pty) = boot_with_fake("think-span", &fake);

    let before = pty.raw().len();
    pty.send(b"think first\r").unwrap();
    let streamed = wait_for_squeezed_after(
        &mut pty,
        "the answer n7p",
        before,
        "the answer after the think span",
    );
    assert!(
        !streamed.contains("<think>") && !streamed.contains("</think>"),
        "a think tag was painted while the reply streamed:\n{streamed}"
    );
    assert!(
        !squeezed(&streamed).contains(&squeezed("weighing the options")),
        "the reasoning was painted while the reply streamed:\n{streamed}"
    );

    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains(&squeezed("the answer n7p")),
        "the answer is not on screen:\n{frame}"
    );
    assert!(
        !frame.contains("<think>") && !frame.contains("</think>"),
        "a think tag is on screen after the reply finished:\n{frame}"
    );
    assert!(
        !frame.contains(&squeezed("weighing the options")),
        "the reasoning is on screen after the reply finished:\n{frame}"
    );

    quit(pty);
}

/// A tool that takes a free-text note besides its marker and echoes a
/// fixed line, so the folded header has long arguments to cut.
const NOTE_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Take a note.\n\
    # @option --note! The note text.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   echo \"note taken t6y\" >> \"$LLM_OUTPUT\"\n\
    }\n";

// ---------------------------------------------------------------------
// The folded header carries `name(short args)`: a long argument is cut
// to the header's budget, never spilled whole across the row, and the
// row stays one folded line that ends in its status.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn long_tool_args_are_cut_short_in_the_folded_header() {
    let dir = fresh_config_dir("short-args");
    let _guard = TempDirGuard(dir.clone());
    let note: String = (0..40).map(|i| format!("seg{i:02}xx")).collect();
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_note".to_string(),
            arguments: format!("{{\"note\": \"{note}\"}}"),
        },
        Reply::Text("note reply h3d".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_note.sh", NOTE_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    pty.send(b"take a long note\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "note reply h3d",
        0,
        "the reply after the note tool",
    );
    pty.read_for(Duration::from_millis(500));

    let rows = repaint_rows(&mut pty);
    let header = folded_header_row(&rows, "fixture_note");
    assert!(
        header.contains("seg00xx"),
        "the folded header shows none of the arguments: {header:?}"
    );
    assert!(
        !header.contains("seg39xx"),
        "the folded header spilled the whole 320-character argument: {header:?}"
    );
    assert_header_is_name_args_elapsed(
        header_before_status(header, "fixture_note", "ok"),
        "fixture_note",
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// A bookkeeping call (`job__list` here; `memory__*`, `skill__*` and
// `agent__list_*` are the same class) takes one dim line: the name is on
// screen once, with neither a running nor a failed status attached.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn a_bookkeeping_call_takes_one_line() {
    let dir = fresh_config_dir("bookkeeping");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "job__list".to_string(),
            arguments: "{}".to_string(),
        },
        Reply::Text("listing reply b8k".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    pty.send(b"list the jobs\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "listing reply b8k",
        0,
        "the reply after the bookkeeping call",
    );
    pty.read_for(Duration::from_millis(500));

    let frame = repaint(&mut pty);
    let names = frame.matches("job__list").count();
    assert_eq!(
        names, 1,
        "a bookkeeping call should take exactly one line naming it (saw {names}):\n{frame}"
    );
    assert!(
        !frame.contains("failed"),
        "the bookkeeping line reads as a failure:\n{frame}"
    );
    assert_eq!(fake.served(), 2, "{frame}");

    quit(pty);
}

// ---------------------------------------------------------------------
// A mouse click on a folded tool row focuses and opens it. The row's
// screen position is not known from outside, so the click lands on the
// row the repaint put the header on; a second click folds it again.
// ---------------------------------------------------------------------

const ECHO_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Say something.\n\
    # @option --word! The word.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   echo \"clicked output c4v\" >&2\n\
    \x20   echo \"clicked output c4v\" >> \"$LLM_OUTPUT\"\n\
    }\n";

#[test]
#[serial]
fn a_mouse_click_on_the_folded_tool_row_opens_it() {
    let dir = fresh_config_dir("click");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_echo".to_string(),
            arguments: "{\"word\": \"hi\"}".to_string(),
        },
        Reply::Text("echo reply m9s".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_echo.sh", ECHO_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    pty.send(b"say hi\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "echo reply m9s",
        0,
        "the reply after the echo tool",
    );
    pty.read_for(Duration::from_millis(500));

    let output = squeezed("clicked output c4v");
    let folded = repaint(&mut pty);
    assert!(
        folded.contains("fixture_echo("),
        "no folded header names the echo tool:\n{folded}"
    );
    assert!(
        !folded.contains(&output),
        "the tool row is already open before any click:\n{folded}"
    );
    let raw = String::from_utf8_lossy(pty.raw()).into_owned();
    assert!(
        raw.contains("\x1b[?1000h") || raw.contains("\x1b[?1002h"),
        "the UI never asked the terminal for mouse reports"
    );
    assert!(
        raw.contains("\x1b[?1006h"),
        "the UI never asked the terminal for SGR-encoded mouse reports"
    );

    let rows = repaint_rows(&mut pty);
    let header_row = rows
        .iter()
        .find(|(_, text)| text.contains("fixture_echo("))
        .map(|(row, _)| *row)
        .unwrap_or_else(|| panic!("no screen row carries the tool header: {rows:?}"));

    // A left click (press and release) on column 4 of the header row.
    // An escape sequence counts as a hotkey only once the keyboard has
    // been idle; the repaint before the click was a keystroke.
    let click = format!("\x1b[<0;4;{header_row}M\x1b[<0;4;{header_row}m");
    pty.read_for(HOTKEY_GAP);
    pty.send(click.as_bytes()).unwrap();
    pty.read_for(Duration::from_millis(400));
    let opened = repaint(&mut pty);
    assert!(
        opened.contains(&output),
        "a left click on the tool header's screen row {header_row} did not open the folded \
         tool row; rows were {rows:?}; frame:\n{opened}"
    );
    assert!(
        opened.contains("fixture_echo("),
        "the opened row lost its header:\n{opened}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(click.as_bytes()).unwrap();
    pty.read_for(Duration::from_millis(400));
    let refolded = repaint(&mut pty);
    assert!(
        !refolded.contains(&output),
        "a second click on row {header_row} did not fold the row again:\n{refolded}"
    );

    pty.send(ESC).unwrap();
    pty.read_for(Duration::from_millis(300));
    quit(pty);
}

// ---------------------------------------------------------------------
// A narrower terminal re-wraps the cells in place: every marker is on
// screen exactly as often as before (user cell plus dry-run echo), never
// a duplicated row, and growing back restores the original layout.
// The terminal is tall enough that the whole transcript stays in view
// at every width (the dry-run echo repeats the prompt plus a preamble).
// ---------------------------------------------------------------------

#[test]
#[serial]
fn a_resize_rewraps_without_duplicating_rows() {
    let (_guard, mut pty) = boot("rewrap", &[]);
    pty.resize(70, 100).unwrap();
    pty.read_for(Duration::from_millis(500));

    let filler = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor";
    for n in 1..=2 {
        let before = pty.raw().len();
        let marker = format!("rewrapmark{n:02}");
        pty.send(format!("{marker} {filler} {filler}\r").as_bytes())
            .unwrap();
        wait_for_squeezed_after(&mut pty, &marker, before, "a long prompt");
        pty.read_for(Duration::from_millis(400));
    }
    pty.read_for(Duration::from_millis(500));

    let wide = repaint(&mut pty);
    let counts: Vec<usize> = (1..=2)
        .map(|n| wide.matches(&format!("rewrapmark{n:02}")).count())
        .collect();
    assert!(
        counts.iter().all(|&c| c == 2),
        "each exchange should show its marker twice (user cell and echo) at 100 columns, saw {counts:?}:\n{wide}"
    );

    for (rows, cols) in [(70u16, 50u16), (70, 100), (70, 64), (70, 100)] {
        pty.resize(rows, cols).unwrap();
        pty.read_for(Duration::from_millis(700));
        let frame = repaint(&mut pty);
        for n in 1..=2 {
            let marker = format!("rewrapmark{n:02}");
            let seen = frame.matches(&marker).count();
            assert_eq!(
                seen, 2,
                "{marker} shows {seen} times after a resize to {rows}x{cols} (expected 2):\n{frame}"
            );
        }
        assert!(
            !frame.contains(CHIP),
            "a resize detached the view from the bottom:\n{frame}"
        );
    }

    pty.read_for(HOTKEY_GAP);
    pty.send(CTRL_UP).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(ESC).unwrap();
    pty.read_for(Duration::from_millis(300));
    quit(pty);
}

// =====================================================================
// Collapsed rows and the focus cursor: what a finished tool looks like
// folded, how it opens, and what wins when a draft is in the composer.
// =====================================================================

/// The squeezed text strictly between the first `start` and the first
/// `end` after it.
fn between<'a>(frame: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = frame.find(start)? + start.len();
    let to = frame[from..].find(end)? + from;
    Some(&frame[from..to])
}

/// The one squeezed screen row carrying `name(`: the tool's folded
/// header.
fn folded_header_row<'a>(rows: &'a [(u16, String)], name: &str) -> &'a str {
    let needle = format!("{name}(");
    let mut headers = rows.iter().filter(|(_, text)| text.contains(&needle));
    let (_, header) = headers
        .next()
        .unwrap_or_else(|| panic!("no screen row carries the {name} header: {rows:?}"));
    assert!(
        headers.next().is_none(),
        "the {name} header is on more than one row: {rows:?}"
    );
    header
}

/// The `args)<elapsed>` part of a folded header, read from its right
/// end: the row has to end in `status`, so the text in between can be
/// checked without a search for the status word landing inside a path.
fn header_before_status<'a>(header: &'a str, name: &str, status: &str) -> &'a str {
    let body = header
        .strip_suffix(status)
        .unwrap_or_else(|| panic!("the {name} header does not end in {status:?}: {header:?}"));
    let needle = format!("{name}(");
    let from = body
        .find(&needle)
        .unwrap_or_else(|| panic!("the {name} header lost its name: {header:?}"))
        + needle.len();
    &body[from..]
}

/// Checks a folded tool header squeezed to `name(args)<elapsed><status>`:
/// the piece after the closing paren must be a duration (`0.4s`, `12s`,
/// `350ms`) and nothing else, which is also what keeps an error message
/// out of the header.
fn assert_header_is_name_args_elapsed(header: &str, name: &str) {
    let args_end = header
        .rfind(')')
        .unwrap_or_else(|| panic!("no closing paren in the {name} header {header:?}"));
    let elapsed = &header[args_end + 1..];
    let digits = elapsed.trim_end_matches("ms").trim_end_matches('s');
    assert!(
        !digits.is_empty()
            && digits.chars().all(|c| c.is_ascii_digit() || c == '.')
            && (elapsed.ends_with('s')),
        "the {name} header should read name(args) elapsed status, got {header:?} \
         (after the args: {elapsed:?})"
    );
}

/// Same shape as `SLEEP_TOOL`, but it writes an error and exits non-zero
/// once it has marked its start.
const FAIL_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Fail on purpose.\n\
    # @option --marker! Path prefix for the start file.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   local marker\n\
    \x20   marker=\"$(jq -r '.marker' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   : > \"${marker}-started\"\n\
    \x20   echo \"boom-error-q8x\" >&2\n\
    \x20   exit 1\n\
    }\n";

// ---------------------------------------------------------------------
// A finished tool row is one folded line, `name(args)  elapsed  ok`, with
// the tool's output out of sight. Ctrl+Up focuses it, Enter opens it
// (output visible), Enter folds it again, Esc drops the focus so a bare
// Enter opens nothing, and with a draft typed Enter submits the draft
// even while a row is focused.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn a_finished_tool_row_is_one_folded_line_that_opens_on_enter_when_focused() {
    let dir = fresh_config_dir("tool-focus");
    let _guard = TempDirGuard(dir.clone());
    let marker = dir.join("tool");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_sleep".to_string(),
            arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
        },
        Reply::Text("fixture done tail-m4e".to_string()),
        Reply::Text("after focus reply-n2p".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    pty.send(b"call the sleep tool\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-started"),
        "the tool's start marker",
    );
    fs::write(dir.join("tool-release"), b"").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "fixture done tail-m4e",
        0,
        "the reply after the tool",
    );
    pty.read_for(Duration::from_millis(500));

    let rows = repaint_rows(&mut pty);
    let header = folded_header_row(&rows, "fixture_sleep");
    assert_header_is_name_args_elapsed(
        header_before_status(header, "fixture_sleep", "ok"),
        "fixture_sleep",
    );
    let folded: String = rows.iter().map(|(_, text)| text.as_str()).collect();
    let output = squeezed("fixture slept p3t");
    assert!(
        !folded.contains(&output),
        "the folded tool row still shows the tool's output:\n{folded}"
    );

    // Focus the row and open it.
    pty.read_for(HOTKEY_GAP);
    pty.send(CTRL_UP).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    let opened = repaint(&mut pty);
    assert!(
        opened.contains(&output),
        "Enter on the focused tool row did not open it (no tool output on screen):\n{opened}"
    );
    assert!(
        opened.contains("fixture_sleep("),
        "the opened row lost its header:\n{opened}"
    );

    // Enter again folds it.
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    let refolded = repaint(&mut pty);
    assert!(
        !refolded.contains(&output),
        "a second Enter did not fold the opened row:\n{refolded}"
    );

    // Esc drops the focus: a bare Enter now opens nothing.
    pty.send(ESC).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    let unfocused = repaint(&mut pty);
    assert!(
        !unfocused.contains(&output),
        "Enter after Esc still toggled the row, so Esc did not clear the focus:\n{unfocused}"
    );
    assert!(
        !unfocused.contains("fakellm:noreplyscripted"),
        "a bare Enter on the empty composer submitted a turn:\n{unfocused}"
    );

    // A focused row plus a draft: Enter submits the draft.
    pty.send(CTRL_UP).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    pty.send(b"draft wins k7q\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "after focus reply-n2p",
        before,
        "the reply to the draft sent over a focused row",
    );
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains(">draftwinsk7q"),
        "the draft did not become a user cell:\n{frame}"
    );
    assert!(
        !frame.contains(&output),
        "Enter with a draft toggled the focused row instead of only submitting:\n{frame}"
    );
    assert_eq!(fake.served(), 3, "{frame}");

    quit(pty);
}

// ---------------------------------------------------------------------
// A tool that exits non-zero folds to `name(args)  elapsed  failed: <first
// line of the error>`; what the tool wrote to stderr is there once the
// row is opened.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn a_failed_tool_row_says_failed_and_keeps_the_error_for_the_opened_row() {
    let dir = fresh_config_dir("erroring");
    let _guard = TempDirGuard(dir.clone());
    let marker = dir.join("tool");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_fail".to_string(),
            arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
        },
        Reply::Text("after failure reply-f3w".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_fail.sh", FAIL_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    pty.send(b"call the failing tool\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "after failure reply-f3w",
        0,
        "the reply after the failed tool",
    );
    pty.read_for(Duration::from_millis(500));

    let rows = repaint_rows(&mut pty);
    let header = folded_header_row(&rows, "fixture_fail");
    let status = squeezed("failed: Tool call 'fixture_fail' exited with code 1");
    assert_header_is_name_args_elapsed(
        header_before_status(header, "fixture_fail", &status),
        "fixture_fail",
    );
    assert!(
        !header.contains("boom"),
        "the folded header carries the tool's stderr: {header:?}"
    );
    let folded: String = rows.iter().map(|(_, text)| text.as_str()).collect();
    let errors_folded = folded.matches("boom-error-q8x").count();

    pty.read_for(HOTKEY_GAP);
    pty.send(CTRL_UP).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    let opened = repaint(&mut pty);
    assert!(
        opened.matches("boom-error-q8x").count() > errors_folded,
        "the opened failed row does not show the tool's error (folded: {errors_folded}):\n{opened}"
    );

    pty.send(ESC).unwrap();
    pty.read_for(Duration::from_millis(300));
    quit(pty);
}

// ---------------------------------------------------------------------
// With the view scrolled up during a turn, the cells that land (the
// tool's end, the reply) do not drag the view down: the follow chip
// counts them as new, and End brings them into view and clears the chip.
// ---------------------------------------------------------------------

/// The count a follow chip `N new below  End = follow` carries at the
/// right end of a squeezed screen row, if the row ends in one.
fn follow_chip_count(row: &str) -> Option<usize> {
    let before = row.strip_suffix("End=follow")?.strip_suffix("newbelow")?;
    before
        .rsplit(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

#[test]
#[serial]
fn cells_landing_while_scrolled_up_count_on_the_follow_chip_until_end() {
    let dir = fresh_config_dir("scrolled-landing");
    let _guard = TempDirGuard(dir.clone());
    let marker = dir.join("tool");
    let mut replies: Vec<Reply> = (1..=5)
        .map(|n| Reply::Text(format!("fillerreply{n:02}")))
        .collect();
    replies.push(Reply::ToolCall {
        name: "fixture_sleep".to_string(),
        arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
    });
    replies.push(Reply::Text("late reply tail-v6c".to_string()));
    let fake = FakeLlm::start(replies);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    pty.resize(14, 100).unwrap();
    pty.read_for(Duration::from_millis(500));
    for n in 1..=5 {
        let before = pty.raw().len();
        pty.send(format!("fillerline{n:02}\r").as_bytes()).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            &format!("fillerreply{n:02}"),
            before,
            "a filler reply",
        );
        pty.read_for(Duration::from_millis(300));
    }

    pty.send(b"call the sleep tool\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-started"),
        "the tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    pty.send(PAGE_UP).unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "End = follow",
        before,
        "the follow chip after scrolling up mid-turn",
    );

    fs::write(dir.join("tool-release"), b"").unwrap();
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let late = squeezed("late reply tail-v6c");
    let scrolled = loop {
        pty.read_for(Duration::from_millis(300));
        let rows = repaint_rows(&mut pty);
        let frame: String = rows.iter().map(|(_, text)| text.as_str()).collect();
        if let Some(count) = rows.iter().find_map(|(_, text)| follow_chip_count(text)) {
            assert!(count > 0, "the follow chip counts nothing new:\n{frame}");
            break frame;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the follow chip never counted the cells that landed while scrolled up:\n{frame}"
        );
    };
    assert!(
        !scrolled.contains(&late),
        "the reply pulled the scrolled view down to the bottom:\n{scrolled}"
    );

    pty.send(END).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains(&late),
        "End did not bring the late reply into view:\n{frame}"
    );
    assert!(
        !frame.contains("End=follow"),
        "End did not clear the follow chip:\n{frame}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// todo__* calls take no transcript row of their own; what they set shows
// up in the Todo panel (F2).
// ---------------------------------------------------------------------

#[test]
#[serial]
fn todo_calls_take_no_transcript_row_and_fill_the_todo_panel() {
    let dir = fresh_config_dir("todo-hidden");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "todo__init".to_string(),
            arguments: "{\"goal\": \"walk the dog w7k\"}".to_string(),
        },
        Reply::ToolCall {
            name: "todo__add".to_string(),
            arguments: "{\"task\": \"feed the cat z3r\"}".to_string(),
        },
        Reply::Text("todo noted r4d".to_string()),
    ]);
    write_fake_llm_config_with(&dir, &fake, "auto_continue: true\nmax_auto_continues: 0\n");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    pty.send(b"plan the chores\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "todo noted r4d",
        0,
        "the reply after the todo calls",
    );
    pty.read_for(Duration::from_millis(800));

    let frame = repaint(&mut pty);
    assert!(
        !frame.contains("todo__init") && !frame.contains("todo__add"),
        "a todo call took a transcript row:\n{frame}"
    );
    assert!(
        !frame.contains("feedthecatz3r"),
        "the todo item is on screen before the panel is opened:\n{frame}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(F2).unwrap();
    pty.read_for(Duration::from_millis(500));
    let panel = repaint(&mut pty);
    assert!(
        panel.contains("Todo"),
        "F2 did not open the Todo panel:\n{panel}"
    );
    assert!(
        panel.contains("feedthecatz3r"),
        "the Todo panel does not list the item the todo__add call set:\n{panel}"
    );

    pty.send(F2).unwrap();
    pty.read_for(Duration::from_millis(300));
    quit(pty);
}

// ---------------------------------------------------------------------
// With the reasoning delta held open on the wire, the "Thinking for Ns"
// status row is on screen well before the hold limit, and it is gone
// once the answer lands.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn the_thinking_status_row_is_live_during_a_held_reasoning_delta() {
    let dir_for_release = fresh_config_dir("thinking-release");
    let _release_guard = TempDirGuard(dir_for_release.clone());
    let release = dir_for_release.join("release");
    let fake = FakeLlm::start(vec![Reply::Streamed(vec![
        Chunk::Reasoning("pondering deeply q9z".to_string()),
        Chunk::WaitFor(release.clone()),
        Chunk::Text("final answer k2m".to_string()),
    ])]);
    let (_guard, mut pty) = boot_with_fake("thinking", &fake);

    let started = std::time::Instant::now();
    pty.send(b"think hard\r").unwrap();
    let seen = wait_for_squeezed_after(&mut pty, "Thinking for", 0, "the live thinking row");
    assert!(
        started.elapsed() < HOLD_LIMIT,
        "the Thinking row appeared only after the fake gave up holding the stream:\n{seen}"
    );

    fs::write(&release, b"").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "final answer k2m",
        0,
        "the answer after reasoning",
    );
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains(&squeezed("Thinking for")),
        "the Thinking row is still up after the answer landed:\n{frame}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// F2 shows the Todo panel and hides it again.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn f2_shows_and_hides_the_todo_panel() {
    let (_guard, mut pty) = boot("f2", &[]);

    let frame = repaint(&mut pty);
    assert!(
        !frame.contains("Todo"),
        "the Todo panel is open before F2 was pressed:\n{frame}"
    );

    pty.send(F2).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains("Todo"),
        "F2 did not open the Todo panel:\n{frame}"
    );

    pty.send(F2).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains("Todo"),
        "a second F2 did not close the Todo panel:\n{frame}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// Once the transcript overflows a short terminal, PgUp detaches the view
// from the bottom and the chip offers "End = follow"; End clears it. A
// submitted line follows the bottom again on its own, so the reply it
// gets is never out of sight.
// ---------------------------------------------------------------------

/// Fills a 14-row terminal past one screen with dry-run exchanges, so
/// there is something to scroll, and checks the chip is still down.
fn overflow_a_short_terminal(pty: &mut Pty) {
    pty.resize(14, 100).unwrap();
    pty.read_for(Duration::from_millis(500));
    for n in 1..=5 {
        let before = pty.raw().len();
        let marker = format!("fillerline{n:02}");
        pty.send(format!("{marker}\r").as_bytes()).unwrap();
        wait_for_squeezed_after(pty, &marker, before, "a filler prompt");
        pty.read_for(Duration::from_millis(300));
    }
    let frame = repaint(pty);
    assert!(
        !frame.contains(CHIP),
        "the follow chip is up while the view follows the bottom:\n{frame}"
    );
}

#[test]
#[serial]
fn follow_chip_appears_on_scroll_up_and_clears_on_end() {
    let (_guard, mut pty) = boot("follow-chip", &[]);
    overflow_a_short_terminal(&mut pty);

    let before = pty.raw().len();
    pty.send(PAGE_UP).unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "End = follow",
        before,
        "the follow chip after scrolling up",
    );

    pty.send(END).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains("End=follow"),
        "End did not clear the follow chip:\n{frame}"
    );
    assert!(
        frame.contains("fillerline05"),
        "End did not bring the newest output into view:\n{frame}"
    );

    pty.send(PAGE_UP).unwrap();
    pty.read_for(Duration::from_millis(500));
    let before = pty.raw().len();
    pty.send(b"latemarker\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "latemarker",
        before,
        "a prompt sent while scrolled up",
    );
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains("End=follow"),
        "the view did not follow the line just sent:\n{frame}"
    );
    assert!(
        frame.contains("latemarker"),
        "the line sent while scrolled up is out of sight:\n{frame}"
    );

    quit(pty);
}

// =====================================================================
// Edge cases: tool-cell pairing, invalid input, tiny terminals, wide
// graphemes, undo to an empty transcript.
// =====================================================================

/// A tool that marks its start at `<marker>-started`, then waits (at most
/// eight seconds) for `<marker>-release` to appear before finishing. Its
/// result goes to `LLM_OUTPUT` (a file the runner reads back for the
/// model); the progress line on stderr is what the transcript can show.
const SLEEP_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Wait until released.\n\
    # @option --marker! Path prefix for the start and release files.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   local marker\n\
    \x20   marker=\"$(jq -r '.marker' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   : > \"${marker}-started\"\n\
    \x20   for _ in $(seq 1 160); do\n\
    \x20       [ -e \"${marker}-release\" ] && break\n\
    \x20       sleep 0.05\n\
    \x20   done\n\
    \x20   echo \"fixture slept p3t\" >&2\n\
    \x20   echo \"fixture slept p3t\" >> \"$LLM_OUTPUT\"\n\
    }\n";

/// Drops one bash tool into the config's `functions/tools` dir; the binary
/// builds its shim on startup.
fn write_tool(dir: &Path, file: &str, body: &str) {
    let tools = dir.join("functions").join("tools");
    fs::create_dir_all(&tools).unwrap();
    fs::write(tools.join(file), body).unwrap();
}

/// Pumps the pty until `path` exists, so the child's output never backs up
/// while the test waits on a side effect.
fn wait_for_file(pty: &mut Pty, path: &Path, what: &str) {
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    while !path.exists() {
        let text = pty.read_for(Duration::from_millis(100));
        assert!(
            std::time::Instant::now() < deadline,
            "{what} ({}) never appeared:\n{text}",
            path.display()
        );
    }
}

// ---------------------------------------------------------------------
// A tool call is one cell: its start and end events land in the same
// row. Observable from the outside: while the tool runs its cell names
// it; once it has finished and the model replied, the same name is on
// screen exactly once, not in a second cell.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn a_tool_call_is_one_cell_that_finishes_in_place() {
    let dir = fresh_config_dir("tool-cell");
    let _guard = TempDirGuard(dir.clone());
    let marker = dir.join("tool");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_sleep".to_string(),
            arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
        },
        Reply::Text("fixture done tail-r7q".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    pty.send(b"call the sleep tool\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-started"),
        "the tool's start marker",
    );
    pty.read_for(Duration::from_millis(500));
    let running = repaint(&mut pty);
    assert!(
        running.contains("fixture_sleep"),
        "no cell names the running tool:\n{running}"
    );

    fs::write(dir.join("tool-release"), b"").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "fixture done tail-r7q",
        0,
        "the reply after the tool",
    );
    pty.read_for(Duration::from_millis(500));
    let finished = repaint(&mut pty);
    let names = finished.matches("fixture_sleep").count();
    assert_eq!(
        names, 1,
        "the finished tool should be one cell naming it once (saw {names}); \
         while running the frame was:\n{running}\nafter:\n{finished}"
    );
    assert!(
        finished.contains(&squeezed("fixture done tail-r7q")),
        "the model's reply after the tool is missing:\n{finished}"
    );
    assert_eq!(fake.served(), 2, "{finished}");

    quit(pty);
}

// ---------------------------------------------------------------------
// Invalid input: `.undo` on an empty transcript (nothing recorded) must
// not take the UI down; the process is still there, still takes keys,
// and still quits with the tty restored.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn undo_with_nothing_to_undo_leaves_the_ui_alive() {
    let (_guard, mut pty) = boot("undo-empty", &[]);

    pty.send(b".undo\r").unwrap();
    pty.read_for(Duration::from_secs(1));
    pty.send(b".undo\r").unwrap();
    pty.read_for(Duration::from_secs(1));

    let before = pty.raw().len();
    pty.send(b"still-alive-marker\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "still-alive-marker",
        before,
        "a prompt after a refused .undo",
    );
    pty.read_for(Duration::from_millis(500));

    quit(pty);
}

// ---------------------------------------------------------------------
// Every widget clamps to the buffer. With the welcome block up and the
// Todo panel open, the terminal is shrunk to a handful of cells and then
// to a single cell; nothing may panic, a key typed under every size
// still reaches the composer, and once the terminal grows back both
// widgets are drawn again and the draft typed along the way is
// submitted whole.
//
// After a shrink the composer's own viewport (ratatui-textarea) may stay
// scrolled right, so the draft can look blank until it is edited or
// sent; the draft itself is intact, which is what the submitted user
// cell asserts.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn tiny_terminal_sizes_clamp_the_welcome_block_and_todo_panel() {
    let (_guard, mut pty) = boot("tiny", &[]);

    pty.send(F2).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains("Todo"),
        "F2 did not open the Todo panel:\n{frame}"
    );

    // One distinct digit per size, the first a control resize that is
    // not tiny at all.
    for (digit, rows, cols) in [
        (b"1", 20u16, 70u16),
        (b"2", 6, 20),
        (b"3", 3, 8),
        (b"4", 2, 4),
        (b"5", 1, 1),
        (b"6", 5, 40),
    ] {
        pty.resize(rows, cols).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(digit).unwrap();
        pty.read_for(Duration::from_millis(400));
        let since = strip_ansi(&String::from_utf8_lossy(pty.raw()));
        assert!(
            !since.contains("panicked") && !since.contains("exit="),
            "the UI died at {rows}x{cols}:\n{since}"
        );
    }

    pty.resize(24, 80).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains(&squeezed("Tip: Press Ctrl-C")),
        "the welcome block did not come back after growing the terminal:\n{frame}"
    );
    assert!(
        frame.contains("Todo"),
        "the Todo panel did not come back after growing the terminal:\n{frame}"
    );

    let before = pty.raw().len();
    pty.send(b"\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        ">123456",
        before,
        "the draft typed across the resizes, as the user cell",
    );
    pty.read_for(Duration::from_millis(500));

    quit(pty);
}

// ---------------------------------------------------------------------
// A prompt carrying a VS16 sequence and a wide emoji reaches the user
// cell and the dry-run echo with both graphemes intact.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn emoji_and_vs16_prompts_echo_whole() {
    let (_guard, mut pty) = boot("emoji", &[]);

    let before = pty.raw().len();
    pty.send("\u{2714}\u{FE0F} check \u{1F44D} emoji-marker-v5\r".as_bytes())
        .unwrap();
    wait_for_squeezed_after(&mut pty, "emoji-marker-v5", before, "the emoji prompt");
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains('\u{2714}') && frame.contains('\u{1F44D}'),
        "the emoji graphemes did not survive into the transcript:\n{frame}"
    );
    assert!(
        frame.matches("emoji-marker-v5").count() >= 2,
        "the user cell and the echo should both carry the prompt:\n{frame}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// "Undo removes cells back to the last user cell", at the boundary where
// the previous user cell is a command (`.session scratch`) rather than a
// prompt: the only exchange goes, the command cell stays, and the undone
// prompt is handed back to the composer. The welcome block does not
// return: a submitted line (the command) is still in the transcript.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn undoing_the_only_exchange_stops_at_the_command_cell_before_it() {
    let fake = FakeLlm::start(vec![Reply::Text("reply-solo-zz".to_string())]);
    let (_guard, mut pty) = boot_with_fake("undo-to-empty", &fake);

    pty.send(b".session scratch\r").unwrap();
    pty.read_for(Duration::from_secs(1));

    let before = pty.raw().len();
    pty.send(b"solomarker one\r").unwrap();
    wait_for_squeezed_after(&mut pty, "reply-solo-zz", before, "the only reply");
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains(&squeezed("Tip: Press Ctrl-C")),
        "the welcome block is up with a turn on screen:\n{frame}"
    );

    pty.send(b".undo\r").unwrap();
    pty.read_for(Duration::from_secs(1));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains("reply-solo-zz") && !frame.contains(">solomarkerone"),
        ".undo left the only exchange on screen:\n{frame}"
    );
    assert!(
        frame.contains(">.sessionscratch"),
        ".undo removed the command cell before the exchange:\n{frame}"
    );
    assert!(
        !frame.contains(&squeezed("Tip: Press Ctrl-C")),
        "the welcome block came back over the command cell:\n{frame}"
    );
    assert!(
        frame.contains("solomarkerone"),
        ".undo did not hand the undone prompt back to the composer:\n{frame}"
    );

    pty.send(CTRL_C).unwrap();
    pty.read_for(Duration::from_millis(300));
    quit(pty);
}

// =====================================================================
// Key gating and hidden spans: Shift+G at the bottom, a think span that
// closes late, Esc over a focused row mid-turn, the view keys under a
// modal, and a folded header at forty columns.
// =====================================================================

// ---------------------------------------------------------------------
// Shift+G jumps to the bottom only while the view is scrolled AND
// the composer is empty. With the view at the bottom the letter is
// plain typing, before any scroll and again right after a jump.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn shift_g_types_the_letter_while_the_view_follows_the_bottom() {
    let (_guard, mut pty) = boot("shift-g-unscrolled", &[]);
    overflow_a_short_terminal(&mut pty);

    // Unscrolled, empty composer: G is a letter.
    pty.send(b"Gx7").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains("Gx7"),
        "Shift+G on an unscrolled empty composer was swallowed instead of typed:\n{frame}"
    );
    assert!(
        !frame.contains(CHIP),
        "typing G raised the follow chip:\n{frame}"
    );
    pty.send(CTRL_C).unwrap();
    pty.read_for(Duration::from_millis(300));

    // Scrolled, empty composer: G jumps back and types nothing.
    let before = pty.raw().len();
    pty.send(PAGE_UP).unwrap();
    wait_for_squeezed_after(&mut pty, "End = follow", before, "the chip after PgUp");
    pty.send(b"G").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains(CHIP),
        "Shift+G while scrolled on an empty composer did not follow the bottom:\n{frame}"
    );
    assert!(
        !frame.contains(">G"),
        "Shift+G both jumped and typed its letter:\n{frame}"
    );

    // Right after the jump the view follows again: G is a letter once more.
    pty.send(b"Gx7").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains("Gx7"),
        "Shift+G right after a jump was swallowed instead of typed:\n{frame}"
    );

    pty.send(CTRL_C).unwrap();
    pty.read_for(Duration::from_millis(300));
    quit(pty);
}

// ---------------------------------------------------------------------
// The streaming cell hides a `<think>` span whose closing tag has
// not arrived yet. With the stream held between the reasoning and the
// answer, the open span is off screen while held, and the late
// `</think>` leaves neither tag nor reasoning behind.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn an_open_think_span_stays_hidden_until_its_late_close_arrives() {
    let dir_for_release = fresh_config_dir("late-think-release");
    let _release_guard = TempDirGuard(dir_for_release.clone());
    let release = dir_for_release.join("release");
    let fake = FakeLlm::start(vec![Reply::Streamed(vec![
        Chunk::Reasoning("weighing the options v4w".to_string()),
        Chunk::WaitFor(release.clone()),
        Chunk::Text("the answer n7p".to_string()),
    ])]);
    let (_guard, mut pty) = boot_with_fake("late-think", &fake);

    let started = std::time::Instant::now();
    pty.send(b"think then hold\r").unwrap();
    wait_for_squeezed_after(&mut pty, "Thinking for", 0, "the live thinking row");
    assert!(
        started.elapsed() < HOLD_LIMIT,
        "the Thinking row appeared only after the fake gave up holding the stream"
    );
    pty.read_for(Duration::from_millis(500));
    let held = repaint(&mut pty);
    assert!(
        !held.contains("<think>") && !held.contains("</think>"),
        "a think tag is on screen while the span is still open:\n{held}"
    );
    assert!(
        !held.contains(&squeezed("weighing the options")),
        "the reasoning of an open think span is on screen while held:\n{held}"
    );

    fs::write(&release, b"").unwrap();
    let after = wait_for_squeezed_after(&mut pty, "the answer n7p", 0, "the answer after the hold");
    assert!(
        !after.contains("<think>") && !after.contains("</think>"),
        "a think tag was painted when the late close arrived:\n{after}"
    );
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains(&squeezed("the answer n7p")),
        "the answer is not on screen:\n{frame}"
    );
    assert!(
        !frame.contains("<think>") && !frame.contains("</think>"),
        "a think tag is on screen after the late close:\n{frame}"
    );
    assert!(
        !frame.contains(&squeezed("weighing the options")),
        "the reasoning is on screen after the late close:\n{frame}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// The other late close: a reply that is reasoning and nothing else
// gets its `</think>` only at `[DONE]`. Nothing of the span survives on
// screen and the UI takes the next turn.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn a_reasoning_only_reply_leaves_no_think_span_behind() {
    let fake = FakeLlm::start(vec![
        Reply::Streamed(vec![Chunk::Reasoning(
            "only reasoning here z3k".to_string(),
        )]),
        Reply::Text("second reply p4m".to_string()),
    ]);
    let (_guard, mut pty) = boot_with_fake("think-only", &fake);

    pty.send(b"reason only\r").unwrap();
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    loop {
        pty.read_for(Duration::from_millis(300));
        if fake.served() >= 1 {
            let frame = repaint(&mut pty);
            if !frame.contains(&squeezed("Thinking for"))
                && !frame.contains(&squeezed("Esc to interrupt"))
            {
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the reasoning-only turn never finished"
        );
    }
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains("<think>") && !frame.contains("</think>"),
        "a think tag is on screen after a reasoning-only reply:\n{frame}"
    );
    assert!(
        !frame.contains(&squeezed("only reasoning here")),
        "the reasoning is on screen after a reasoning-only reply:\n{frame}"
    );

    let before = pty.raw().len();
    pty.send(b"and now answer\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "second reply p4m",
        before,
        "the next turn's reply",
    );
    assert_eq!(fake.served(), 2);

    quit(pty);
}

// ---------------------------------------------------------------------
// While a turn runs, Esc interrupts it even with a transcript row
// focused: the interrupted marker lands, the stream stops painting, and
// the focus walk is intact afterwards (Ctrl+Up, Enter opens the row).
// Whether the focus itself survives that Esc is not pinned here: the
// contract says the interrupt comes first, not what becomes of the focus.
// A recording session is opened first so the interrupted marker, not an
// "Aborted." error, closes the turn (the lifecycle suite's precedent).
// ---------------------------------------------------------------------

#[test]
#[serial]
fn esc_with_a_focused_row_interrupts_the_running_turn_first() {
    let dir = fresh_config_dir("esc-focus-running");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("release");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_echo".to_string(),
            arguments: "{\"word\": \"hi\"}".to_string(),
        },
        Reply::Text("echo reply m9s".to_string()),
        Reply::Streamed(vec![
            Chunk::Text("held head h5t".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("held tail never shown".to_string()),
        ]),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_echo.sh", ECHO_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b".session scratch\r").unwrap();
    pty.read_for(Duration::from_secs(1));

    pty.send(b"say hi\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "echo reply m9s",
        0,
        "the reply after the echo tool",
    );
    pty.read_for(Duration::from_millis(500));
    let output = squeezed("clicked output c4v");

    let before = pty.raw().len();
    pty.send(b"stream and hold\r").unwrap();
    wait_for_squeezed_after(&mut pty, "held head h5t", before, "the first delta");
    pty.wait_for("Esc to interrupt", PTY_DEADLINE)
        .expect("the status row never offered Esc to interrupt");

    // Focus the tool row mid-turn, then Esc.
    pty.read_for(HOTKEY_GAP);
    pty.send(CTRL_UP).unwrap();
    pty.read_for(HOTKEY_GAP);
    let pressed_at = pty.raw().len();
    pty.send(ESC).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
        if since.to_lowercase().contains("interrupted") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Esc with a row focused did not interrupt the running turn:\n{since}"
        );
    }
    fs::write(&release, b"").unwrap();
    pty.read_for(Duration::from_secs(1));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains(&squeezed("Esc to interrupt")),
        "the turn is still running after Esc:\n{frame}"
    );
    assert!(
        !frame.contains(&squeezed("held tail never shown")),
        "the stream kept painting after the interrupt:\n{frame}"
    );
    assert!(
        !frame.contains(&output),
        "the tool row opened on its own around the interrupt:\n{frame}"
    );

    // The focus walk is intact once the turn is over.
    pty.read_for(HOTKEY_GAP);
    pty.send(CTRL_UP).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    let opened = repaint(&mut pty);
    assert!(
        opened.contains(&output),
        "Ctrl+Up then Enter after the interrupt did not open the tool row:\n{opened}"
    );
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(ESC).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    let unfocused = repaint(&mut pty);
    assert!(
        !unfocused.contains(&output),
        "Esc on the idle UI did not clear the focus:\n{unfocused}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// Keys that only move the view belong to the transcript, not the modal:
// with a tool's Confirm question up, F2 toggles the Todo panel, PgUp
// raises the follow chip and End clears it, and the question is still
// there to answer afterwards.
// ---------------------------------------------------------------------

const CONFIRM_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Ask the user before continuing and record the answer.\n\
    # @option --out! Where to write the answer.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    # shellcheck disable=SC1090\n\
    source \"$LLM_PROMPT_UTILS_FILE\"\n\
    \n\
    main() {\n\
    \x20   local out v\n\
    \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   v=\"$(confirm \"Really do it? q9d-q\")\"\n\
    \x20   printf '%s' \"${v:-cancelled}\" > \"$out\"\n\
    \x20   echo \"fixture confirm: ${v:-cancelled} q9d-r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

#[test]
#[serial]
fn view_keys_work_under_a_tools_confirm_modal() {
    let dir = fresh_config_dir("confirm-view-keys");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("confirm-answer");
    let mut replies: Vec<Reply> = (1..=5)
        .map(|n| Reply::Text(format!("fillerreply{n:02}")))
        .collect();
    replies.push(Reply::ToolCall {
        name: "fixture_confirm_rec".to_string(),
        arguments: format!("{{\"out\": \"{}\"}}", out.display()),
    });
    replies.push(Reply::Text("fixture done z2v".to_string()));
    let fake = FakeLlm::start(replies);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_confirm_rec.sh", CONFIRM_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    pty.resize(14, 100).unwrap();
    pty.read_for(Duration::from_millis(500));
    for n in 1..=5 {
        let before = pty.raw().len();
        pty.send(format!("fillerline{n:02}\r").as_bytes()).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            &format!("fillerreply{n:02}"),
            before,
            "a filler reply",
        );
        pty.read_for(Duration::from_millis(300));
    }

    pty.send(b"call the confirm tool\r").unwrap();
    pty.wait_for("q9d-q", PTY_DEADLINE)
        .expect("the tool's confirm question never reached the screen");
    pty.read_for(HOTKEY_GAP);

    pty.send(F2).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains("Todo"),
        "F2 under the Confirm modal did not open the Todo panel:\n{frame}"
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(F2).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains("Todo"),
        "a second F2 under the Confirm modal did not close the Todo panel:\n{frame}"
    );

    pty.read_for(HOTKEY_GAP);
    let before = pty.raw().len();
    pty.send(PAGE_UP).unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "End = follow",
        before,
        "the follow chip after PgUp under the Confirm modal",
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(END).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains(CHIP),
        "End under the Confirm modal did not clear the follow chip:\n{frame}"
    );
    assert!(
        frame.contains("q9d-q"),
        "the Confirm question is gone after the view keys:\n{frame}"
    );
    assert!(
        !out.exists(),
        "a view key answered the Confirm question: {:?}",
        fs::read_to_string(&out)
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    wait_for_squeezed_after(&mut pty, "fixture done z2v", 0, "the reply after the tool");
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), "1");

    quit(pty);
}

// ---------------------------------------------------------------------
// The same view keys under the Input modal an agent variable raises;
// Esc then cancels the question and the UI is still alive.
// ---------------------------------------------------------------------

fn write_agent_needing_a_variable(dir: &Path, name: &str) {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        format!(
            "name: {name}\n\
             description: an agent with a variable\n\
             version: 0.1.0\n\
             instructions: |\n\
             \x20 You are an assistant for {{{{username}}}}. Reply briefly.\n\
             variables:\n\
             \x20 - name: username\n\
             \x20   description: Your user name\n"
        ),
    )
    .unwrap();
}

#[test]
#[serial]
fn view_keys_work_under_the_input_modal() {
    let dir = fresh_config_dir("input-view-keys");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    write_agent_needing_a_variable(&dir, "vars-agent-g4r");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    overflow_a_short_terminal(&mut pty);

    pty.send(b".agent vars-agent-g4r\r").unwrap();
    wait_for_squeezed_after(&mut pty, "Enter = answer", 0, "the Input modal");
    pty.read_for(HOTKEY_GAP);

    pty.send(F2).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        frame.contains("Todo"),
        "F2 under the Input modal did not open the Todo panel:\n{frame}"
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(F2).unwrap();
    pty.read_for(Duration::from_millis(500));

    pty.read_for(HOTKEY_GAP);
    let before = pty.raw().len();
    pty.send(PAGE_UP).unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "End = follow",
        before,
        "the follow chip after PgUp under the Input modal",
    );
    pty.read_for(HOTKEY_GAP);
    pty.send(END).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains(CHIP),
        "End under the Input modal did not clear the follow chip:\n{frame}"
    );
    assert!(
        frame.contains(&squeezed("Enter = answer")),
        "the Input question is gone after the view keys:\n{frame}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(ESC).unwrap();
    pty.read_for(Duration::from_secs(1));
    let frame = repaint(&mut pty);
    assert!(
        !frame.contains(&squeezed("Enter = answer")),
        "Esc did not close the Input modal:\n{frame}"
    );

    quit(pty);
}

// ---------------------------------------------------------------------
// A folded tool header at forty columns is still one row that ends in
// its status: the state token is what the narrow width keeps, the
// arguments are what it cuts.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn a_folded_tool_header_keeps_its_status_at_forty_columns() {
    let dir = fresh_config_dir("forty-cols");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_echo".to_string(),
            arguments: "{\"word\": \"a-rather-long-argument-value-q2w\"}".to_string(),
        },
        Reply::Text("echo reply m9s".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_echo.sh", ECHO_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    pty.send(b"say it\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "echo reply m9s",
        0,
        "the reply after the echo tool",
    );
    pty.read_for(Duration::from_millis(500));

    pty.resize(24, 40).unwrap();
    pty.read_for(Duration::from_millis(800));
    let rows = repaint_rows(&mut pty);
    let headers: Vec<&(u16, String)> = rows
        .iter()
        .filter(|(_, text)| text.contains("fixture_echo"))
        .collect();
    assert_eq!(
        headers.len(),
        1,
        "the tool name should be on exactly one row at 40 columns: {rows:?}"
    );
    let (row, header) = headers[0];
    assert!(
        header.ends_with("ok"),
        "the folded header on row {row} lost its status token at 40 columns: {header:?}; rows {rows:?}"
    );
    let args_and_elapsed = between(header, "fixture_echo(", "ok")
        .unwrap_or_else(|| panic!("no `fixture_echo(...) ... ok` header on row {row}: {header:?}"));
    assert_header_is_name_args_elapsed(args_and_elapsed, "fixture_echo");
    let frame: String = rows.iter().map(|(_, t)| t.as_str()).collect();
    assert!(
        !frame.contains(&squeezed("clicked output c4v")),
        "the row opened on its own at 40 columns:\n{frame}"
    );

    quit(pty);
}
