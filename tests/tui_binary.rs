//! Real-process checks of the full-screen UI's terminal lifecycle: however
//! the binary leaves (Ctrl+D, SIGTERM, a panic after the alternate screen is
//! up) the tty it ran on must come back in its cooked, echoing state. Each
//! case runs the binary on a pseudo-terminal under a bash wrapper that
//! reports the exit code and `stty -a` afterwards, so the file is unix-only.
//! The command menus' modals, the plain REPL's type-ahead and the runs
//! with no human to ask live in `tui_binary_menus.rs`.
//! The cases run one at a time: each spawn forks a session leader and takes
//! a controlling tty, and a fork landing between another test's `openpty`
//! and its `exec` has failed the spawn with a garbled errno.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::fake_llm::{Chunk, FakeLlm, HOLD_LIMIT, Reply};
use common::pty::{Pty, strip_ansi};
use common::tui_harness::*;
use serial_test::serial;

/// Also the one place a REPL command's `print!` output is seen through the
/// fd capture: in-process tests never see it, because libtest diverts
/// `print!` before it reaches fd 1. The screen is made tall enough for the
/// whole table so its rows can be placed: the first one is painted
/// directly under `> .info`, which is where the Command cell puts it; a
/// capture that gave up before the drain thread ran would instead leave
/// the cell empty and show the table a blank row lower, as a notice. The
/// table is a `Command` cell inside the alternate screen, so once the
/// screen is left nothing of it may leak to the terminal the shell gets
/// back. The transcript printed on exit would carry the cell on purpose,
/// so it is switched off here: what is left after the leave is the
/// wrapper's own report.
#[test]
#[serial]
fn a_command_prints_into_the_transcript_and_ctrl_d_leaves_the_tty_cooked() {
    let dir = fresh_config_dir("ctrl-d");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "save_session: false\ntui_dump_on_exit: false\n");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(80, 100).unwrap();
    pty.read_for(Duration::from_millis(300));
    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .unwrap();
    // Let the capture settle so a late forward would have happened.
    pty.read_for(Duration::from_millis(500));
    let header_rows = painted_at(pty.raw(), b"> .info", info_at);
    let &(header_row, 1) = header_rows.first().unwrap_or_else(|| {
        panic!(
            "`> .info` was never painted at a column-one cursor position:\n{}",
            strip_ansi(&String::from_utf8_lossy(&pty.raw()[info_at..]))
        )
    }) else {
        panic!("`> .info` was painted mid-row: {header_rows:?}");
    };
    let first_rows = painted_at(pty.raw(), b"model", info_at);
    assert!(
        first_rows.contains(&(header_row + 1, 1)),
        "the table's first row was not painted directly under `> .info` (row \
         {header_row}); `model` was painted at {first_rows:?}:\n{}",
        strip_ansi(&String::from_utf8_lossy(&pty.raw()[info_at..]))
    );
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);

    let raw = pty.raw();
    let left = rfind(raw, LEAVE_ALT).expect("the alternate screen was never left");
    let after = strip_ansi(&String::from_utf8_lossy(&raw[left..]));
    let report_at = after.find("exit=").expect("no exit= line after the leave");
    let between = after[..report_at].trim();
    for marker in ["log_path", "model", "dryrun:dry-model", "config_file"] {
        assert!(
            !between.contains(marker),
            "`.info` output ({marker:?}) reached the real terminal after the \
             alternate screen was left: {between:?}"
        );
    }
}

/// The TUI treats SIGTERM as an orderly quit and exits 0 (the REPL's
/// default disposition would be 143).
#[test]
#[serial]
fn sigterm_leaves_the_tty_cooked() {
    signal_leaves_the_tty_cooked("sigterm", libc::SIGTERM);
}

/// Raw mode turns Ctrl+C into a key, so a SIGINT that does arrive is a
/// `kill -INT` from outside and gets the same orderly quit as SIGTERM.
#[test]
#[serial]
fn sigint_leaves_the_tty_cooked() {
    signal_leaves_the_tty_cooked("sigint", libc::SIGINT);
}

fn signal_leaves_the_tty_cooked(scenario: &str, signal: libc::c_int) {
    let dir = fresh_config_dir(scenario);
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let pid = child_pid(&transcript);
    // SAFETY: kill takes a pid and a signal number and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        find(pty.raw(), LEAVE_ALT, 0).is_some(),
        "signal {signal} never left the alternate screen:\n{transcript}"
    );
}

/// The panic hook restores the terminal before delegating to the original
/// hook: the alternate screen must be left BEFORE the panic message is
/// printed, or the message lands on a screen that is about to vanish.
#[test]
#[serial]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the panic hook is compiled out of release builds"
)]
fn a_panic_after_entering_the_alternate_screen_still_restores_the_tty() {
    let dir = fresh_config_dir("panic");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[("COYOTE_TUI_PANIC_AT_START", "1")]);

    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_eq!(exit_code(&transcript), 101, "{transcript}");
    assert_tty_restored(&transcript);

    let raw = pty.raw();
    let entered = find(raw, ENTER_ALT, 0).expect("the alternate screen was never entered");
    let left = find(raw, LEAVE_ALT, entered).expect("the alternate screen was never left");
    let message = find(raw, b"COYOTE_TUI_PANIC_AT_START", entered)
        .expect("the panic message never reached the terminal");
    assert!(
        left < message,
        "the panic message was printed while still on the alternate screen \
         (leave at {left}, message at {message}):\n{transcript}"
    );
    assert!(
        find(raw, PASTE_OFF, entered).is_some_and(|off| off < message),
        "bracketed paste was still on when the panic message was printed"
    );
}

/// A panic inside the exit work (after Ctrl+D) is not lost with the
/// screen: its message, captured as the exit's late output, is printed
/// on the restored terminal after the alternate screen is left, and the
/// process exits non-zero with the terminal restored.
#[test]
#[serial]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the exit-panic hook is compiled out of release builds"
)]
fn a_panic_inside_the_exit_work_prints_its_message_on_the_restored_terminal() {
    let dir = fresh_config_dir("panic-at-exit");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[("COYOTE_TUI_PANIC_AT_EXIT", "1")]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_ne!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    let raw = pty.raw();
    let entered = find(raw, ENTER_ALT, 0).expect("the alternate screen was never entered");
    let left = find(raw, LEAVE_ALT, entered).expect("the alternate screen was never left");
    let message = find(raw, b"COYOTE_TUI_PANIC_AT_EXIT requested a panic", entered)
        .expect("the exit panic's message never reached the terminal");
    assert!(
        left < message,
        "the panic message was printed while still on the alternate screen \
         (leave at {left}, message at {message}):\n{transcript}"
    );
    // The captured panic line is printed once; the task error the exit
    // returns names the payload again on its own line, as any error would.
    let panic_line = b"panicked at src/tui/mod.rs";
    assert_eq!(
        count(&raw[left..], panic_line),
        1,
        "the captured panic line was not printed exactly once:\n{transcript}"
    );
}

/// Without a tty on stdin the TUI refuses to start, says why on the
/// terminal it does have, and exits 1.
#[test]
#[serial]
fn without_a_tty_on_stdin_the_tui_refuses_to_start() {
    let dir = fresh_config_dir("no-tty");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui_under(&dir, &[], WAIT_WRAPPER, "/dev/null");

    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert!(transcript.contains("No TTY for TUI"), "{transcript}");
    assert_eq!(exit_code(&transcript), 1, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the alternate screen was entered without a tty:\n{transcript}"
    );
}

const PASTE_ON: &[u8] = b"\x1b[?2004h";
const PASTE_OFF: &[u8] = b"\x1b[?2004l";
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";

/// The UI hides the cursor on every frame and never positions one, so
/// the only `show` is the leave sequence's: it must come after the last
/// `hide` and before `before`, the offset at which the shell or the
/// wrapper's report took over the terminal.
fn assert_cursor_shown_before(raw: &[u8], before: usize, what: &str) {
    let window = &raw[..before];
    let shown = rfind(window, CURSOR_SHOW)
        .unwrap_or_else(|| panic!("the cursor was never shown before {what}"));
    let hidden = rfind(window, CURSOR_HIDE);
    assert!(
        hidden.is_none_or(|hidden| hidden < shown),
        "the cursor was hidden again after it was shown, before {what} \
         (hide={hidden:?}, show={shown})"
    );
}

/// Kitty keyboard push (`ESC [ > <flags> u`) or pop (`ESC [ < <count> u`),
/// by its introducer byte.
fn find_keyboard_stack_op(haystack: &[u8], introducer: u8, from: usize) -> Option<usize> {
    let mut at = from;
    while let Some(start) = find(haystack, &[0x1b, b'[', introducer], at) {
        let rest = &haystack[start + 3..];
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 && rest.get(digits) == Some(&b'u') {
            return Some(start);
        }
        at = start + 1;
    }
    None
}

fn find_keyboard_push(haystack: &[u8], from: usize) -> Option<usize> {
    find_keyboard_stack_op(haystack, b'>', from)
}

fn rfind_keyboard_pop(haystack: &[u8]) -> Option<usize> {
    let mut last = None;
    let mut from = 0;
    while let Some(at) = find_keyboard_stack_op(haystack, b'<', from) {
        last = Some(at);
        from = at + 1;
    }
    last
}

fn is_rgb_sgr(params: &[u16]) -> bool {
    params
        .windows(2)
        .any(|w| (w[0] == 38 || w[0] == 48 || w[0] == 58) && w[1] == 2)
}

/// The alternate screen is entered once and left exactly once on Ctrl+D;
/// bracketed paste comes on after it and goes off before it is left;
/// keyboard-enhancement flags, when the terminal takes them, are pushed
/// after alt-screen entry and popped before leaving it, and the support
/// probe is sent before the reader starts.
#[test]
#[serial]
fn enter_and_leave_sequences_mirror_around_the_alternate_screen() {
    let dir = fresh_config_dir("mirror");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");

    let raw = pty.raw();
    let enter_alt = find(raw, ENTER_ALT, 0).expect("the alternate screen was never entered");
    let leave_alt = rfind(raw, LEAVE_ALT).expect("the alternate screen was never left");
    assert!(enter_alt < leave_alt);
    assert!(
        find(raw, ENTER_ALT, enter_alt + 1).is_none(),
        "the alternate screen was entered more than once"
    );
    assert_eq!(
        count(raw, LEAVE_ALT),
        1,
        "the alternate screen must be left exactly once:\n{transcript}"
    );

    let paste_on = find(raw, PASTE_ON, 0).expect("bracketed paste was never enabled");
    let paste_off = rfind(raw, PASTE_OFF).expect("bracketed paste was never disabled");
    assert!(
        enter_alt < paste_on && paste_on < paste_off && paste_off < leave_alt,
        "bracketed paste must come on inside the alternate screen and go off \
         before it is left (enter={enter_alt}, on={paste_on}, off={paste_off}, \
         leave={leave_alt})"
    );

    // The keyboard-enhancement probe is answered by nothing on a pty, so
    // the flags may legitimately never be pushed; if they are, the push
    // follows alt-screen entry and the pop precedes leaving it.
    assert!(
        find(raw, b"\x1b[?u", 0).is_some(),
        "the keyboard-enhancement support probe (CSI ? u) was never sent"
    );
    if let Some(push) = find_keyboard_push(raw, 0) {
        let pop = rfind_keyboard_pop(raw).expect("flags pushed but never popped");
        assert!(enter_alt < push, "flags pushed before alt-screen entry");
        assert!(push < pop && pop < leave_alt, "flags popped after leaving");
    }
}

/// SIGHUP runs the same leave path as SIGTERM.
#[test]
#[serial]
fn sighup_leaves_the_tty_cooked() {
    signal_leaves_the_tty_cooked("sighup", libc::SIGHUP);
}

/// A hang-up while the keyboard-enhancement probe is still waiting on
/// the terminal (the first two seconds, before the footer) must kill the
/// process the ordinary way. The signal streams are armed only after the
/// screen is entered, so nothing latches the signal while the probe
/// blocks; otherwise the process would spin on the dead tty forever.
/// The tty flags are not checked here: crossterm's probe switches raw
/// mode on for its own duration, and a process that dies inside it
/// leaves the line discipline as the probe had it.
#[test]
#[serial]
fn sighup_during_the_startup_probe_kills_the_process() {
    let dir = fresh_config_dir("sighup-probe");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    let transcript = pty.wait_for("cpid=", PTY_DEADLINE).unwrap();
    let pid = child_pid(&transcript);
    // The probe opens with its query and then waits two seconds for a
    // reply nobody here sends; the hang-up goes out as soon as the query
    // is seen, before a loaded host can let the wait run out.
    pty.wait_for_raw(b"\x1b[?u\x1b[c", 0, PTY_DEADLINE).unwrap();
    // SAFETY: kill takes a pid and a signal number and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGHUP) }, 0);
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    // The signal streams are armed only after the probe, so the kernel's
    // default disposition ends the process: 128 + SIGHUP, never an error
    // return from `run`.
    let code = exit_code(&transcript);
    assert_eq!(
        code, 129,
        "expected the hang-up to end the process, got exit={code}:\n{transcript}"
    );
    assert!(
        !transcript.contains("no session"),
        "the footer was already up, so the hang-up missed the probe:\n{transcript}"
    );
}

/// Drives Ctrl-Z under the job-control wrapper up to the point where the
/// shell has the terminal back, and checks what the shell sees: the
/// alternate screen and bracketed paste gone before the shell spoke, and
/// a cooked tty. Returns the raw offset of the shell's `job-stopped` line.
fn suspend_and_check_handback(pty: &mut Pty, before_suspend: usize) -> usize {
    pty.send(b"\x1a").unwrap();
    let stopped = pty.wait_for("stopped-probe-done", PTY_DEADLINE).unwrap();
    let left_at = find(pty.raw(), LEAVE_ALT, before_suspend)
        .unwrap_or_else(|| panic!("Ctrl-Z stopped the job without leaving the alternate screen"));
    let stopped_marker = find(pty.raw(), b"job-stopped", before_suspend).unwrap();
    assert!(
        left_at < stopped_marker,
        "the alternate screen was left only after the shell regained the terminal"
    );
    assert!(
        find(pty.raw(), PASTE_OFF, before_suspend).is_some_and(|off| off < stopped_marker),
        "bracketed paste was still enabled when the shell got the terminal back"
    );
    assert_cursor_shown_before(pty.raw(), stopped_marker, "the shell regained the terminal");
    let flags = stopped
        .lines()
        .find_map(|line| line.strip_prefix("stopped-tty-flags: "))
        .unwrap();
    let tokens: Vec<&str> = flags
        .split(|c: char| c.is_whitespace() || c == ';')
        .collect();
    for flag in ["echo", "icanon"] {
        assert!(
            tokens.contains(&flag),
            "the suspended UI left the shell with {flag} off:\n{stopped}"
        );
    }
    stopped_marker
}

/// Waits for the alternate screen to be re-entered after `from` and gives
/// raw mode the moment it needs to be re-applied before any key is sent.
/// Returns the re-entry offset.
fn wait_for_re_entry(pty: &mut Pty, from: usize) -> usize {
    let re_entered = pty
        .wait_for_raw(ENTER_ALT, from, PTY_DEADLINE)
        .expect("the alternate screen was not re-entered");
    pty.wait_for_raw(PASTE_ON, re_entered, PTY_DEADLINE)
        .expect("bracketed paste was not re-enabled on re-entry");
    // Raw mode is re-applied a few syscalls after the mode sequences are
    // written; keys sent inside that window are still cooked (echoed,
    // CR->NL). A person cannot type that fast, so settle first.
    pty.read_for(Duration::from_millis(300));
    re_entered
}

/// The screen the UI comes back to must show its chrome again without
/// waiting for something else to change: the hint, the model and the
/// session tokens.
fn assert_repainted_after(pty: &mut Pty, re_entered: usize, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let repainted = strip_ansi(&String::from_utf8_lossy(&pty.raw()[re_entered..]));
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
             Printable bytes since re-entry: {repainted:?}"
        );
        pty.read_for(Duration::from_millis(200));
    }
}

/// Under a job-control shell, Ctrl-Z hands the terminal back cooked and
/// out of the alternate screen; `fg` brings the alternate screen back with
/// raw mode and key handling restored, and the usual clean exit still
/// works afterwards.
#[test]
#[serial]
fn ctrl_z_hands_the_terminal_to_the_shell_and_fg_takes_it_back() {
    let dir = fresh_config_dir("ctrl-z");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui_under(&dir, &[], JOB_CONTROL_WRAPPER, "/dev/tty");

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before_suspend = pty.raw().len();
    let stopped_marker = suspend_and_check_handback(&mut pty, before_suspend);

    // The wrapper resumes the job with `fg` after a second.
    wait_for_re_entry(&mut pty, stopped_marker);
    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE)
        .expect("keys were not handled after resume");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(
        pty.raw()[before_suspend..]
            .windows(ENTER_ALT.len())
            .filter(|w| *w == ENTER_ALT)
            .count(),
        1,
        "exactly one alt-screen re-entry is expected after the suspend"
    );
}

/// Keys typed while the job is stopped belong to the shell's turn at the
/// terminal, not to the composer: whatever is still queued when `fg`
/// brings the UI back is dropped, the same as typeahead after an editor.
#[test]
#[serial]
fn keys_typed_while_stopped_do_not_reach_the_composer_after_fg() {
    let dir = fresh_config_dir("ctrl-z-typeahead");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui_under(&dir, &[], JOB_CONTROL_WRAPPER, "/dev/tty");

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before_suspend = pty.raw().len();
    let stopped_marker = suspend_and_check_handback(&mut pty, before_suspend);
    // The wrapper sleeps a second before `fg`; these bytes sit in the tty
    // queue until the UI is back in raw mode.
    pty.send(b"stopped-typeahead-w2q").unwrap();

    let re_entered = wait_for_re_entry(&mut pty, stopped_marker);
    assert_repainted_after(&mut pty, re_entered, "fg");
    pty.read_for(Duration::from_millis(500));
    let since_re_entry = strip_ansi(&String::from_utf8_lossy(&pty.raw()[re_entered..]));
    assert!(
        !since_re_entry.contains("stopped-typeahead-w2q"),
        "keys typed while the job was stopped landed in the composer after fg:\n{since_re_entry}"
    );

    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", re_entered, PTY_DEADLINE)
        .expect("keys were not handled after resume");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// After `fg` the UI must repaint its screen on its own, not come back
/// blank until some later change touches enough cells.
#[test]
#[serial]
fn resuming_after_ctrl_z_repaints_the_screen() {
    let dir = fresh_config_dir("ctrl-z-repaint");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui_under(&dir, &[], JOB_CONTROL_WRAPPER, "/dev/tty");

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before_suspend = pty.raw().len();
    let stopped_marker = suspend_and_check_handback(&mut pty, before_suspend);
    let re_entered = wait_for_re_entry(&mut pty, stopped_marker);
    assert_repainted_after(&mut pty, re_entered, "fg");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// With an editor that returns at once, Ctrl-O must release the terminal
/// (alternate screen left) and come back to a repainted screen that still
/// takes keys.
#[test]
#[serial]
fn an_editor_shell_out_comes_back_to_a_repainted_screen() {
    let dir = fresh_config_dir("editor");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[("EDITOR", "true"), ("VISUAL", "true")]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before = pty.raw().len();
    pty.send(b"\x0f").unwrap();
    let left = pty
        .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
        .expect("Ctrl-O did not release the terminal for the editor");
    let re_entered = wait_for_re_entry(&mut pty, left);
    assert_repainted_after(&mut pty, re_entered, "the editor shell-out");
    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE)
        .expect("keys were not handled after the editor returned");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// An editor that leaves the tty with echo and canonical mode off must
/// not have the last word: the UI puts the line discipline back to its
/// own starting point before taking the terminal again, so what Ctrl+D
/// restores at the end is the user's shell state, not the editor's.
#[test]
#[serial]
fn an_editor_that_changes_the_tty_modes_does_not_decide_what_exit_restores() {
    use std::os::unix::fs::PermissionsExt;

    let dir = fresh_config_dir("editor-stty");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let editor = dir.join("stty-editor.sh");
    fs::write(&editor, "#!/bin/sh\nstty -echo -icanon </dev/tty\nexit 0\n").unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    let editor = editor.to_str().unwrap();
    let mut pty = spawn_tui(&dir, &[("EDITOR", editor), ("VISUAL", editor)]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before = pty.raw().len();
    pty.send(b"\x0f").unwrap();
    let left = pty
        .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
        .expect("Ctrl-O did not release the terminal for the editor");
    let re_entered = wait_for_re_entry(&mut pty, left);
    assert_repainted_after(&mut pty, re_entered, "the editor shell-out");
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", re_entered, PTY_DEADLINE)
        .expect("keys were not handled after the editor returned");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// While the editor is up the keyboard is the editor's: the event stream
/// is stopped before the terminal is released and only restarted after
/// the editor returns. A line typed into an editor that reads the tty
/// comes back through the file into the composer, once; none of it is
/// replayed into the composer as typeahead or submitted as a prompt.
#[test]
#[serial]
fn keys_typed_while_the_editor_runs_reach_the_editor_not_the_composer() {
    use std::os::unix::fs::PermissionsExt;

    let dir = fresh_config_dir("editor-typeahead");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let editor = dir.join("line-editor.sh");
    fs::write(
        &editor,
        "#!/bin/sh\nIFS= read -r line </dev/tty\nprintf '%s' \"$line\" > \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    let editor = editor.to_str().unwrap();
    let mut pty = spawn_tui(&dir, &[("EDITOR", editor), ("VISUAL", editor)]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before = pty.raw().len();
    pty.send(b"\x0f").unwrap();
    let left = pty
        .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
        .expect("Ctrl-O did not release the terminal for the editor");
    // Raw mode is switched off a few syscalls after the leave sequence is
    // written; a line sent before that misses the line discipline and the
    // editor's read never ends.
    pty.read_for(Duration::from_millis(300));
    pty.send(b"typed-x7x\r").unwrap();
    let re_entered = wait_for_re_entry(&mut pty, left);
    pty.wait_for_after("typed-x7x", re_entered, PTY_DEADLINE)
        .expect("the edited line did not come back into the composer");
    pty.read_for(Duration::from_millis(500));
    let since_re_entry = strip_ansi(&String::from_utf8_lossy(&pty.raw()[re_entered..]));
    assert_eq!(
        since_re_entry.matches("typed-x7x").count(),
        1,
        "the typed line was painted more than once after the editor returned \
         (replayed as typeahead or submitted):\n{since_re_entry}"
    );
    assert!(!since_re_entry.contains("Working"), "{since_re_entry}");

    // The edited draft must be cleared before Ctrl+D is the quit chord.
    pty.send(b"\x03").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// Ctrl+C while the editor has the terminal is the editor's to handle,
/// but the tty delivers SIGINT to the whole foreground group, coyote
/// included. The latched signal is forgotten when the screen comes back:
/// the UI is still up afterwards and leaves only on Ctrl+D. Run under the
/// job-control wrapper, which keeps the shell out of that group the way a
/// user's shell is.
#[test]
#[serial]
fn a_ctrl_c_meant_for_the_editor_does_not_end_the_session() {
    use std::os::unix::fs::PermissionsExt;

    let dir = fresh_config_dir("editor-ctrl-c");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let editor = dir.join("int-editor.sh");
    // The read survives the interrupt so the editor ends on the line that
    // follows, not on the signal.
    fs::write(
        &editor,
        "#!/bin/sh\ntrap '' INT\nIFS= read -r x </dev/tty\n: > \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    let editor = editor.to_str().unwrap();
    let mut pty = spawn_tui_under(
        &dir,
        &[("EDITOR", editor), ("VISUAL", editor)],
        JOB_CONTROL_WRAPPER,
        "/dev/tty",
    );

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before = pty.raw().len();
    pty.send(b"\x0f").unwrap();
    let left = pty
        .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
        .expect("Ctrl-O did not release the terminal for the editor");
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    let re_entered = wait_for_re_entry(&mut pty, left);
    assert_repainted_after(&mut pty, re_entered, "the interrupted editor");
    pty.read_for(Duration::from_millis(1500));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[re_entered..]));
    assert!(
        !since.contains("exit="),
        "the editor's Ctrl+C ended the session:\n{since}"
    );
    assert!(since.contains("Type a prompt"), "{since}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A `kill -TERM` delivered while the editor has the terminal is for
/// coyote, not the editor, and is not forgotten with the editor's Ctrl+C:
/// once the editor returns the UI quits on it, orderly, with the tty
/// cooked.
#[test]
#[serial]
fn a_sigterm_during_the_editor_quits_once_the_editor_returns() {
    use std::os::unix::fs::PermissionsExt;

    let dir = fresh_config_dir("editor-sigterm");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let editor = dir.join("line-editor.sh");
    fs::write(&editor, "#!/bin/sh\nIFS= read -r x </dev/tty\n: > \"$1\"\n").unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    let editor = editor.to_str().unwrap();
    let mut pty = spawn_tui(&dir, &[("EDITOR", editor), ("VISUAL", editor)]);

    let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let pid = child_pid(&transcript);
    let before = pty.raw().len();
    pty.send(b"\x0f").unwrap();
    pty.wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
        .expect("Ctrl-O did not release the terminal for the editor");
    pty.read_for(Duration::from_millis(300));
    // SAFETY: kill takes a pid and a signal number and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    pty.read_for(Duration::from_millis(300));
    let released_at = pty.raw().len();
    pty.send(b"\r").unwrap();

    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        !strip_ansi(&String::from_utf8_lossy(&pty.raw()[..released_at])).contains("exit="),
        "the UI quit while the editor still had the terminal:\n{transcript}"
    );
}

/// The dry-run model echoes the role prompt plus the user's text, so a
/// markdown prompt comes back rendered: `**zebra**` loses its asterisks and
/// the inline code its backticks. Ratatui's diffing paints words with
/// cursor moves between them, which `strip_ansi` drops, so the rendered
/// reply reads "zebraandquartzmarker" once spaces are ignored.
const MARKDOWN_PROMPT: &[u8] = b"**zebra** and `quartz` marker\r";

/// Waits for the rendered reply of [`MARKDOWN_PROMPT`] and returns the
/// transcript; the typed prompt itself keeps its asterisks, so the
/// asterisk-free form can only come from the rendered assistant cell.
fn wait_for_rendered_reply(pty: &mut Pty) -> String {
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    loop {
        let text = pty.read_for(Duration::from_millis(200));
        let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        if squeezed.contains("zebraandquartzmarker") {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the rendered dry-run reply never appeared:\n{text}"
        );
    }
}

/// A streamed dry-run reply: the turn shows its working status row, then
/// the rendered reply lands in the transcript while the UI stays live
/// enough to take Ctrl+D afterwards.
#[test]
#[serial]
fn a_typed_prompt_gets_a_dry_run_reply_in_the_transcript() {
    let dir = fresh_config_dir("reply");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(MARKDOWN_PROMPT).unwrap();
    let transcript = wait_for_rendered_reply(&mut pty);
    let working = transcript
        .find("Working")
        .unwrap_or_else(|| panic!("no working status row was shown:\n{transcript}"));
    let squeezed_after: String = transcript[working..]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        squeezed_after.contains("zebraandquartzmarker"),
        "the reply was painted before the working row:\n{transcript}"
    );
    assert!(
        transcript.contains("**zebra**"),
        "the user's own prompt never reached the screen:\n{transcript}"
    );

    pty.send(b"\x04").unwrap();
    let transcript_end = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript_end), 0, "{transcript_end}");
    assert_tty_restored(&transcript_end);
}

/// A `.exit session` command turn renders its "Save session?" modal and
/// completes on `y`. A dirty temp session with `save_session` unset is
/// what triggers the question; `y` then asks for a name and writes it.
/// (The REPL behaves the same way on this path: dry-run turns are never
/// recorded, so the session is dirtied through a setting instead.)
#[test]
#[serial]
fn exit_session_shows_the_save_modal_and_saves_on_y() {
    let dir = fresh_config_dir("exit-session");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".session\r").unwrap();
    pty.wait_for("temp", PTY_DEADLINE).unwrap();
    // A dry-run turn records nothing, so a session setting dirties it.
    pty.send(b".set skills_enabled false\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    pty.send(b".exit session\r").unwrap();
    pty.wait_for("Save session?", PTY_DEADLINE).unwrap();
    // A bare hotkey only counts once typing has been idle for a second
    // (pinned by the ManualClock unit test), so wait the gap out before
    // answering. Ratatui repaints only changed cells, so the modal
    // title's unchanged `S` is absent from the stream.
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    pty.wait_for("ession name", PTY_DEADLINE).unwrap();
    let named_at = pty.raw().len();
    answer_field_modal(&mut pty, "probe-saved");
    // The footer said "no session" at startup too; only a fresh one means
    // the session was written and closed.
    pty.wait_for_after("no session", named_at, PTY_DEADLINE)
        .unwrap();
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);

    let saved = walk(&dir).into_iter().find(|p| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("probe-saved"))
    });
    assert!(
        saved.is_some(),
        "no probe-saved session file was written under {dir:?}:\n{transcript}"
    );
}

/// `NO_COLOR` is honoured without stripping bold or underline. The
/// rendered markdown reply is the colourful part of a dry run.
#[test]
#[serial]
fn no_color_paints_no_colour_but_keeps_bold_or_underline() {
    let dir = fresh_config_dir("no-color");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[("NO_COLOR", "1")]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(MARKDOWN_PROMPT).unwrap();
    wait_for_rendered_reply(&mut pty);
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);

    let sgrs = sgr_params(pty.raw());
    let coloured: Vec<_> = sgrs.iter().filter(|p| is_colour_sgr(p)).collect();
    assert!(
        coloured.is_empty(),
        "NO_COLOR=1 but colour SGRs were emitted: {coloured:?}"
    );
    let styled = sgrs.iter().any(|p| p.contains(&1) || p.contains(&4));
    assert!(styled, "NO_COLOR stripped bold/underline too: {sgrs:?}");
}

/// Truecolor degrades to 256 or 16 colours when `COLORTERM` is not
/// truecolor. `spawn_tui` removes `COLORTERM`, so no RGB SGR may reach the
/// terminal.
#[test]
#[serial]
fn without_colorterm_no_rgb_colour_reaches_the_terminal() {
    let dir = fresh_config_dir("no-truecolor");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(MARKDOWN_PROMPT).unwrap();
    wait_for_rendered_reply(&mut pty);
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);

    let sgrs = sgr_params(pty.raw());
    let rgb: Vec<_> = sgrs.iter().filter(|p| is_rgb_sgr(p)).collect();
    assert!(
        rgb.is_empty(),
        "COLORTERM unset but RGB SGRs were emitted: {rgb:?}"
    );
    assert!(
        sgrs.iter().any(|p| is_colour_sgr(p)),
        "no colour at all was painted on a 256-colour terminal: {sgrs:?}"
    );
}

/// The stdout half of the tty check: stdin is the tty and stdout a file,
/// the shape of `coyote > log`. The refusal goes to the terminal (stderr),
/// nothing of the alternate screen reaches the file, and the exit code
/// is 1.
#[test]
#[serial]
fn without_a_tty_on_stdout_the_tui_refuses_to_start() {
    let dir = fresh_config_dir("no-tty-stdout");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let captured = dir.join("stdout.log");
    let wrapper = format!(
        "'{{coyote}}' <{{stdin}} >'{}' & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {{report}}",
        captured.display()
    );
    let mut pty = spawn_tui_under(&dir, &[], &wrapper, "/dev/tty");

    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert!(
        transcript.contains("No TTY for TUI"),
        "no clear refusal reached the terminal:\n{transcript}"
    );
    assert_eq!(exit_code(&transcript), 1, "{transcript}");
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the alternate screen was entered on the terminal while stdout was a file:\n{transcript}"
    );
    let file = fs::read(&captured).unwrap_or_default();
    assert!(
        find(&file, ENTER_ALT, 0).is_none(),
        "alt-screen escapes were written into the stdout file: {:?}",
        String::from_utf8_lossy(&file)
    );
    assert_tty_restored(&transcript);
}

/// Opens the `.exit session` "Save session?" modal on a dirty session and
/// then sends `signal` to the binary. Nobody is left to answer, so the
/// signal arm must leave the screen after the modal was shown and exit
/// without waiting on the question.
fn signal_during_the_save_modal(scenario: &str, signal: libc::c_int) {
    let dir = fresh_config_dir(scenario);
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);

    let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let pid = child_pid(&transcript);
    pty.send(b".session\r").unwrap();
    pty.wait_for("temp", PTY_DEADLINE).unwrap();
    pty.send(b".set skills_enabled false\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    pty.send(b".exit session\r").unwrap();
    pty.wait_for("Save session?", PTY_DEADLINE).unwrap();
    let modal_at = pty.raw().len();

    // SAFETY: kill takes a pid and a signal number and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        find(pty.raw(), LEAVE_ALT, modal_at.saturating_sub(16)).is_some(),
        "signal {signal} during the save modal never left the alternate screen:\n{transcript}"
    );
}

/// The terminal went away with the "Save session?" modal up.
#[test]
#[serial]
fn sighup_while_the_save_modal_is_open_still_exits_and_leaves_the_screen() {
    signal_during_the_save_modal("sighup-modal", libc::SIGHUP);
}

/// A SIGTERM with the modal up: the same arm as SIGHUP, with the terminal
/// still there to see the result. The dirty session is dropped unsaved,
/// as the REPL's default disposition would drop it, and that is still an
/// orderly exit.
#[test]
#[serial]
fn sigterm_while_the_save_modal_is_open_still_exits_and_leaves_the_screen() {
    signal_during_the_save_modal("sigterm-modal", libc::SIGTERM);
}

/// A window resize on the real tty (SIGWINCH) must bring a fresh frame with
/// the chrome repainted at the new size, and the UI keeps taking keys
/// afterwards.
#[test]
#[serial]
fn a_window_resize_repaints_the_screen_and_keys_still_work() {
    let dir = fresh_config_dir("resize");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    pty.resize(20, 60).unwrap();
    assert_repainted_after(&mut pty, before, "a resize to 60x20");

    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE)
        .expect("keys were not handled after the resize");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// Teardown mirrors `Repl::run`: `top_level_agent_finished` fires the
/// agent's `agent.completed` hook and `tui::run` awaits
/// `hooks::drain_pending` before returning, so a Ctrl+D under `--agent`
/// leaves the hook's marker behind. The hook is an agent-scoped global
/// hook admitted through `global_hooks: ["*"]`. The drain acknowledges
/// the hook's spawn, not its exit (hook children deliberately outlive
/// coyote), so the marker is awaited with a bounded deadline the way
/// `macro_bracketing` awaits its markers.
#[test]
#[serial]
fn ctrl_d_under_an_agent_fires_the_completed_hook_before_exit() {
    let dir = fresh_config_dir("hook-drain");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let marker = dir.join("agent-completed.log");
    let mut config = fs::read_to_string(dir.join("config.yaml")).unwrap();
    config.push_str(&format!(
        "hooks:\n\
         \x20 agent.completed:\n\
         \x20   - name: mark\n\
         \x20     command: 'echo COMPLETED >> \"{}\"'\n",
        marker.display()
    ));
    fs::write(dir.join("config.yaml"), config).unwrap();
    let agent_dir = dir.join("agents/probe-agent");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        "name: probe-agent\n\
         description: a test agent\n\
         version: 0.1.0\n\
         instructions: |\n\
         \x20 You are a probe agent. Reply briefly.\n\
         global_hooks:\n\
         \x20 - \"*\"\n",
    )
    .unwrap();
    let wrapper = WAIT_WRAPPER.replace("'{coyote}'", "'{coyote}' --agent probe-agent");
    let mut pty = spawn_tui_under(&dir, &[], &wrapper, "/dev/tty");

    pty.wait_for("Type a prompt", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);

    let completed = |marker: &Path| {
        fs::read_to_string(marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "COMPLETED")
            .count()
    };
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let fired = loop {
        let fired = completed(&marker);
        if fired > 0 || std::time::Instant::now() >= deadline {
            break fired;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        fired, 1,
        "agent.completed fired {fired} times within {PTY_DEADLINE:?} of Ctrl+D (expected once):\n{transcript}"
    );
}

/// An unknown dot-command is an ordinary command turn that fails: the
/// REPL's hint lands in the transcript as the error, the UI stays up and
/// takes the next command, and Ctrl+D still leaves the tty cooked.
#[test]
#[serial]
fn an_unknown_command_shows_the_hint_and_the_ui_keeps_taking_keys() {
    let dir = fresh_config_dir("unknown-command");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".nosuchcommand-x7x\r").unwrap();
    pty.wait_for("Unknown command", PTY_DEADLINE)
        .expect("no hint was shown for an unknown command");
    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE)
        .expect("keys were not handled after the unknown command");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// An empty Enter starts no turn (no working row, no dry-run echo) and
/// `.exit` leaves exactly as Ctrl+D does: alternate screen left once,
/// exit 0, tty cooked.
#[test]
#[serial]
fn an_empty_enter_starts_no_turn_and_dot_exit_leaves_like_ctrl_d() {
    let dir = fresh_config_dir("empty-enter-dot-exit");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    assert!(
        !since.contains("Working"),
        "an empty Enter started a turn:\n{since}"
    );

    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        1,
        "`.exit` must leave the alternate screen exactly once:\n{transcript}"
    );
}

/// Teardown mirrors `Repl::run`: Ctrl+D runs `exit_session()`, which asks
/// "Save session?" for a dirty session with `save_session` unset. With the
/// terminal still there the question is the modal, `y` asks for a name,
/// and the named session is on disk when the process has left.
#[test]
#[serial]
fn ctrl_d_on_a_dirty_session_asks_to_save_and_y_writes_it_before_leaving() {
    let dir = fresh_config_dir("ctrl-d-dirty");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    open_and_dirty_a_session(&mut pty);
    pty.send(b"\x04").unwrap();
    pty.wait_for("Save session?", PTY_DEADLINE)
        .expect("Ctrl+D on a dirty session did not ask to save it");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    pty.wait_for("ession name", PTY_DEADLINE).unwrap();
    answer_field_modal(&mut pty, "probe-ctrl-d");
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    let saved = walk(&dir).into_iter().find(|p| {
        p.file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("probe-ctrl-d"))
    });
    assert!(
        saved.is_some(),
        "no probe-ctrl-d session file was written under {dir:?}:\n{transcript}"
    );
}

/// `n` on the "Save session?" modal closes the session without asking for
/// a name or writing a file; the UI is still live afterwards.
#[test]
#[serial]
fn declining_the_save_modal_closes_the_session_without_a_file() {
    let dir = fresh_config_dir("decline-save");
    let _guard = TempDirGuard(dir.clone());
    write_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    open_and_dirty_a_session(&mut pty);
    let sessions = dir.join("sessions");
    let files_before = walk(&sessions).len();
    pty.send(b".exit session\r").unwrap();
    pty.wait_for("Save session?", PTY_DEADLINE).unwrap();
    pty.read_for(HOTKEY_GAP);
    let answered_at = pty.raw().len();
    pty.send(b"n").unwrap();
    pty.wait_for_after("no session", answered_at, PTY_DEADLINE)
        .expect("declining the save did not close the session");
    pty.read_for(Duration::from_millis(500));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[answered_at..]));
    assert!(
        !since.contains("ession name"),
        "`n` still asked for a session name:\n{since}"
    );
    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE)
        .expect("keys were not handled after the modal closed");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    let files_after = walk(&sessions);
    assert_eq!(
        files_after.len(),
        files_before,
        "declining the save still wrote something: {files_after:?}"
    );
}

/// Bracketed paste is on while the screen is up, so a paste holding a
/// newline reaches the composer as text and is not submitted as a prompt.
#[test]
#[serial]
fn a_bracketed_paste_with_a_newline_lands_in_the_composer_unsubmitted() {
    let dir = fresh_config_dir("paste");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    pty.send(b"\x1b[200~pasted-a1x\rpasted-b2x\x1b[201~")
        .unwrap();
    pty.wait_for_after("pasted-b2x", before, PTY_DEADLINE)
        .expect("the pasted text never reached the composer");
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    assert!(since.contains("pasted-a1x"), "{since}");
    assert!(
        !since.contains("Working"),
        "a paste containing a newline was submitted as a prompt:\n{since}"
    );
    // The CR split the paste: the second piece sits on the row right
    // under the first, at the same column, not on the first row after an
    // embedded control byte.
    let first = painted_at(pty.raw(), b"pasted-a1x", before);
    let second = painted_at(pty.raw(), b"pasted-b2x", before);
    let (Some(&(row_a, col_a)), Some(&(row_b, col_b))) = (first.last(), second.last()) else {
        panic!("the pasted pieces were not positioned on screen:\n{since}");
    };
    assert_eq!(
        (row_b, col_b),
        (row_a + 1, col_a),
        "the pasted lines are not stacked ({first:?} / {second:?}):\n{since}"
    );

    // The pasted draft must be cleared before Ctrl+D is the quit chord.
    pty.send(b"\x03").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `exit()` is idempotent and the composer is inert once teardown runs: a
/// second Ctrl+D on its heels changes nothing. One leave, exit 0.
#[test]
#[serial]
fn a_second_ctrl_d_during_teardown_changes_nothing() {
    let dir = fresh_config_dir("double-ctrl-d");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"\x04\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        1,
        "two Ctrl+Ds left the alternate screen more than once:\n{transcript}"
    );
}

/// The first run ever: no cache dir, no sessions, no config beyond the
/// dry-run file. The UI comes up with its chrome, and leaving prints the
/// transcript and nothing else (no log line, no warning about the missing
/// directories) ahead of the tty report.
#[test]
#[serial]
fn a_cold_start_with_no_cache_dir_comes_up_and_leaves_clean() {
    let dir = fresh_config_dir("cold-start");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    // `fresh_config_dir` pre-creates `sessions`; a first run has neither it
    // nor the cache dir.
    fs::remove_dir_all(dir.join("sessions")).unwrap();
    assert!(!dir.join("cache").exists());
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("Type a prompt", PTY_DEADLINE).unwrap();
    pty.wait_for("dryrun:dry-model", PTY_DEADLINE).unwrap();
    pty.send(MARKDOWN_PROMPT).unwrap();
    wait_for_rendered_reply(&mut pty);

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    let after = strip_ansi(&String::from_utf8_lossy(
        &pty.raw()[rfind(pty.raw(), LEAVE_ALT).unwrap()..],
    ));
    let report_at = after.find("exit=").unwrap();
    let printed: Vec<&str> = after[..report_at]
        .trim()
        .lines()
        .map(str::trim_end)
        .collect();
    // The dry-run reply echoes the whole request, system prompt included,
    // so only its last line is pinned: the rendered user message.
    let prompt = "> **zebra** and `quartz` marker";
    let reply = "zebra and quartz marker";
    assert_eq!(printed.first(), Some(&prompt), "{printed:#?}");
    assert_eq!(printed.last(), Some(&reply), "{printed:#?}");
    for line in [prompt, reply] {
        assert_eq!(
            printed.iter().filter(|printed| **printed == line).count(),
            1,
            "{line:?} is not printed exactly once: {printed:#?}"
        );
    }
    let leaked: Vec<&&str> = printed
        .iter()
        .filter(|line| {
            let line = line.trim_start();
            [
                "WARN", "ERROR", "INFO", "DEBUG", "Failed", "warning:", "error:", "Error:",
                "Warning:",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
                || line.contains("panicked")
                || [" WARN ", " ERROR ", " INFO ", " DEBUG "]
                    .iter()
                    .any(|level| line.contains(level))
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "the bytes between the leave and the report are not just the transcript: {leaked:#?}"
    );
}

// ---------------------------------------------------------------------
// Scenarios driven by a scripted model: a real tool call in the
// full-screen UI, so the prompt modal, the busy UI and the signal arm are
// seen on a pty rather than through a TestBackend.
// ---------------------------------------------------------------------

/// A tool that marks its start at `<marker>-started`, then waits (at most
/// eight seconds) for `<marker>-release` to appear before finishing.
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
    \x20   echo \"fixture slept p3t\" >> \"$LLM_OUTPUT\"\n\
    }\n";

/// A turn that does not stop when told to: the UI waits out
/// `EXIT_CANCEL_WAIT` and then ends the process without it, still with
/// the tty restored, the alternate screen left and a notice saying what
/// was left behind. The exit is bounded: the wrapper's report lands
/// within ten seconds of the kill.
///
/// Nothing a user can configure makes such a turn any more: an aborted
/// loader is killed and an aborted tool is SIGKILLed, so the turn is the
/// binary's `COYOTE_TUI_STUBBORN_TURN` hook, which keeps the work task
/// on its thread past the deadline after the reply has streamed.
/// `sigterm_during_a_running_tool_still_leaves_the_tty_cooked` is the
/// cooperating twin.
#[test]
#[serial]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the stubborn-turn hook is compiled out of release builds"
)]
fn sigterm_during_a_turn_that_ignores_the_abort_leaves_it_behind_in_bounded_time() {
    let dir = fresh_config_dir("sigterm-stubborn-turn");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text("reply before the stall v7q".to_string())]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[("COYOTE_TUI_STUBBORN_TURN", "1")]);

    let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let pid = child_pid(&transcript);
    pty.send(b"say something\r").unwrap();
    wait_for_squeezed(
        &mut pty,
        "reply before the stall v7q",
        "the reply of the turn that then stalls",
    );
    let signalled_at = pty.raw().len();
    let started = std::time::Instant::now();
    // SAFETY: kill takes a pid and a signal number and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    let took = started.elapsed();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        find(pty.raw(), LEAVE_ALT, signalled_at.saturating_sub(16)).is_some(),
        "SIGTERM during the stubborn turn never left the alternate screen:\n{transcript}"
    );
    assert!(
        transcript.contains("did not stop within"),
        "no notice said the turn was left behind:\n{transcript}"
    );
    let exit_line = find(pty.raw(), b"exit=", signalled_at).expect("the wrapper reported the exit");
    assert_cursor_shown_before(pty.raw(), exit_line, "the exit report");
    assert!(
        took < Duration::from_secs(10),
        "the exit was not bounded by the deadline: {took:?}"
    );
}

/// `TuiPrompter` renders a tool's `Confirm` request as the modal and
/// answers it: a tool asking through the prompt channel on a real tty
/// shows its question in the UI, `y` reaches the tool as a `1` and lets
/// it finish, the model's follow-up reply lands in the transcript, and no
/// raw protocol line reaches the screen.
#[test]
#[serial]
fn a_tools_confirm_request_is_the_modal_and_y_lets_the_tool_finish() {
    let dir = fresh_config_dir("tool-confirm");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("confirm-answer");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture done z2v".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_confirm_rec.sh", RECORDING_CONFIRM_TOOL);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"call the confirm tool\r").unwrap();
    pty.wait_for("q9d-q", PTY_DEADLINE)
        .expect("the tool's confirm question never reached the screen");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(&mut pty, "fixture done z2v", "the reply after the tool");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "`y` on the confirm modal did not reach the tool as a yes:\n{transcript}"
    );
    assert!(
        !transcript.contains("coyote_prompt"),
        "a raw prompt-protocol line reached the screen:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// Keys are handled and frames drawn while a work task sleeps: with a tool
/// blocked mid-turn, typed text shows up in the composer; once the tool is
/// released the turn finishes, and Enter then submits the typed-ahead text
/// as the next turn (it was neither lost nor swallowed).
#[test]
#[serial]
fn typing_while_a_tool_sleeps_reaches_the_composer_and_submits_afterwards() {
    let dir = fresh_config_dir("tool-sleep-keys");
    let _guard = TempDirGuard(dir.clone());
    let marker = dir.join("tool");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_sleep".to_string(),
            arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
        },
        Reply::Text("fixture done tail-k4q".to_string()),
        Reply::Text("third reply m8n".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"call the sleep tool\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-started"),
        "the sleep tool's start marker",
    );
    let typed_at = pty.raw().len();
    let typed = std::time::Instant::now();
    pty.send(b"typeahead-q9z").unwrap();
    // Keystrokes that straddle a frame boundary are painted as two cell
    // runs with a cursor move between them, so the stripped transcript need
    // not hold the marker contiguously. Ctrl+L repaints the whole screen in
    // one frame, which also proves a key is handled while the tool sleeps.
    pty.send(b"\x0c").unwrap();
    pty.wait_for_after("typeahead-q9z", typed_at, PTY_DEADLINE)
        .expect("text typed while the tool slept was not drawn in the composer");
    let drawn_after = typed.elapsed();
    // The tool only ends when released (or after eight seconds on its
    // own); the second model call is the sign the turn has moved on.
    assert!(
        !dir.join("tool-release").exists() && fake.served() == 1,
        "text typed while the tool ran was drawn only {drawn_after:?} later, after the \
         turn had finished (model calls served: {}); frames must be drawn and keys \
         handled while a work task sleeps",
        fake.served()
    );

    fs::write(dir.join("tool-release"), b"").unwrap();
    wait_for_squeezed(
        &mut pty,
        "fixture done tail-k4q",
        "the reply after the tool",
    );
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    let transcript = wait_for_squeezed(
        &mut pty,
        "third reply m8n",
        "the reply to the typed-ahead prompt",
    );
    assert_eq!(fake.served(), 3, "{transcript}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// SIGTERM while a work task is still running (a tool blocked mid-turn)
/// still runs the leave path: the alternate screen is left, the tty is
/// cooked and the exit is orderly. The abort kills the tool, so the exit
/// comes well inside `EXIT_CANCEL_WAIT` (5 s) rather than waiting out the
/// tool's own eight-second give-up; a turn that did miss that deadline
/// would be left behind and the process would still exit, which
/// `sigterm_during_a_turn_that_ignores_the_abort_leaves_it_behind_in_bounded_time`
/// pins.
#[test]
#[serial]
fn sigterm_during_a_running_tool_still_leaves_the_tty_cooked() {
    let dir = fresh_config_dir("sigterm-tool");
    let _guard = TempDirGuard(dir.clone());
    let marker = dir.join("tool");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_sleep".to_string(),
            arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
        },
        Reply::Text("fixture done after sigterm".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);

    let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let pid = child_pid(&transcript);
    pty.send(b"call the sleep tool\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-started"),
        "the sleep tool's start marker",
    );
    let signalled_at = pty.raw().len();
    let started = std::time::Instant::now();
    // SAFETY: kill takes a pid and a signal number and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    let took = started.elapsed();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        find(pty.raw(), LEAVE_ALT, signalled_at.saturating_sub(16)).is_some(),
        "SIGTERM during the tool never left the alternate screen:\n{transcript}"
    );
    let exit_line = find(pty.raw(), b"exit=", signalled_at).expect("the wrapper reported the exit");
    assert_cursor_shown_before(pty.raw(), exit_line, "the exit report");
    assert!(
        took < Duration::from_secs(5),
        "the exit waited the tool out instead of aborting it: {took:?}"
    );
}

/// Two turns in a row: the second prompt gets its own rendered reply and
/// the first turn's cells are still there, so the transcript grows rather
/// than being replaced.
#[test]
#[serial]
fn a_second_turn_renders_after_the_first_without_losing_it() {
    let dir = fresh_config_dir("two-turns");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"**alpha** one-1q\r").unwrap();
    wait_for_squeezed(&mut pty, "alpha one-1q", "the first rendered reply");
    pty.read_for(Duration::from_millis(300));
    let second_at = pty.raw().len();
    pty.send(b"**beta** two-2w\r").unwrap();
    let transcript = wait_for_squeezed(&mut pty, "beta two-2w", "the second rendered reply");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[second_at..]));
    assert!(
        since.contains("**beta**"),
        "the second prompt never reached the screen as a user cell:\n{since}"
    );
    assert!(
        transcript.contains("**alpha**"),
        "the first turn vanished from the transcript:\n{transcript}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// The tty check names stdin and stdout only: `coyote 2> log` is a normal
/// run. The UI comes up, Ctrl+D leaves cleanly, and nothing of the
/// alternate screen lands in the stderr file.
#[test]
#[serial]
fn stderr_redirected_to_a_file_is_not_a_tty_refusal() {
    let dir = fresh_config_dir("stderr-file");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let captured = dir.join("stderr.log");
    let wrapper = format!(
        "'{{coyote}}' <{{stdin}} 2>'{}' & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {{report}}",
        captured.display()
    );
    let mut pty = spawn_tui_under(&dir, &[], &wrapper, "/dev/tty");

    pty.wait_for("no session", PTY_DEADLINE)
        .expect("the UI did not come up with stderr redirected to a file");
    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE).unwrap();
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        !transcript.contains("No TTY for TUI"),
        "a stderr redirect was treated as a missing tty:\n{transcript}"
    );
    let file = fs::read(&captured).unwrap_or_default();
    assert!(
        find(&file, ENTER_ALT, 0).is_none() && find(&file, LEAVE_ALT, 0).is_none(),
        "alt-screen escapes were written into the stderr file: {:?}",
        String::from_utf8_lossy(&file)
    );
}

/// A window dragged down to almost nothing and back: no panic, the chrome
/// returns at full size and keys still work.
#[test]
#[serial]
fn a_tiny_window_does_not_crash_the_ui_and_growing_back_repaints() {
    let dir = fresh_config_dir("tiny-resize");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    for (rows, cols) in [(3u16, 12u16), (1, 1), (2, 40)] {
        pty.resize(rows, cols).unwrap();
        pty.read_for(Duration::from_millis(400));
    }
    let before = pty.raw().len();
    pty.resize(40, 100).unwrap();
    assert_repainted_after(&mut pty, before, "growing back from a 1x1 window");

    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE)
        .expect("keys were not handled after the tiny-window round trip");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        1,
        "the alternate screen was left more than once (a panic restore?):\n{transcript}"
    );
}

// ---------------------------------------------------------------------
// Paced streams: the fake holds the SSE response open between two deltas
// so the screen can be read while the model is still "talking".
// ---------------------------------------------------------------------

/// A streamed reply renders progressively: the first delta is on screen
/// while the stream is still open (the fake has not sent the second delta
/// yet), and the second delta joins it once released. The whole reply
/// must not wait for the stream to end before it is painted.
#[test]
#[serial]
fn a_streamed_reply_is_painted_before_the_stream_ends() {
    let dir = fresh_config_dir("paced-stream");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("stream-release");
    let fake = FakeLlm::start(vec![Reply::Streamed(vec![
        Chunk::Text("first half h4x".to_string()),
        Chunk::WaitFor(release.clone()),
        Chunk::Text(" second half j9k".to_string()),
    ])]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"stream something\r").unwrap();
    let started = std::time::Instant::now();
    let transcript = wait_for_squeezed(&mut pty, "first half h4x", "the first delta");
    let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        started.elapsed() < HOLD_LIMIT,
        "the first delta appeared only after the fake gave up holding the stream"
    );
    assert!(
        !squeezed.contains("secondhalfj9k"),
        "the second delta was on screen before the fake sent it:\n{transcript}"
    );
    // Give a late paint a chance before deciding; the second delta has
    // not been sent, so it must still be absent.
    let settled = pty.read_for(Duration::from_millis(600));
    assert!(
        !settled
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .contains("secondhalfj9k"),
        "the second delta reached the screen before the fake sent it:\n{settled}"
    );

    fs::write(&release, b"").unwrap();
    let transcript = wait_for_squeezed(&mut pty, "second half j9k", "the second delta");
    assert_eq!(fake.served(), 1, "{transcript}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `exit()` is idempotent on the panic path too: the hook restores the
/// terminal and nothing afterwards (a guard drop, the normal leave path)
/// leaves the alternate screen or disables bracketed paste a second time.
#[test]
#[serial]
#[cfg_attr(
    not(debug_assertions),
    ignore = "the panic hook is compiled out of release builds"
)]
fn a_panic_at_start_leaves_the_alternate_screen_exactly_once() {
    let dir = fresh_config_dir("panic-once");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[("COYOTE_TUI_PANIC_AT_START", "1")]);

    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 101, "{transcript}");
    assert_tty_restored(&transcript);
    let raw = pty.raw();
    assert_eq!(
        count(raw, ENTER_ALT),
        1,
        "the alternate screen must be entered exactly once:\n{transcript}"
    );
    assert_eq!(
        count(raw, LEAVE_ALT),
        1,
        "the alternate screen must be left exactly once on a panic:\n{transcript}"
    );
    assert_eq!(
        count(raw, PASTE_OFF),
        1,
        "bracketed paste must be disabled exactly once on a panic:\n{transcript}"
    );
    if let Some(push) = find_keyboard_push(raw, 0) {
        let pop = rfind_keyboard_pop(raw).expect("flags pushed but never popped on panic");
        assert!(push < pop, "{transcript}");
    }
}

const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

/// A reasoning model's stream renders progressively too: with the
/// reasoning delta held open on the wire the turn is visibly busy (the
/// live `Thinking for Ns` status row offers the interrupt), the reasoning
/// text itself stays hidden like the REPL hides a think span, and the
/// answer is not on screen; once released the answer lands without the
/// reasoning text and the status row stops ticking (it repaints once a
/// second while it lives).
#[test]
#[serial]
fn a_reasoning_stream_keeps_the_turn_busy_until_the_answer_is_sent() {
    let dir = fresh_config_dir("reasoning-stream");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("thinking-release");
    let fake = FakeLlm::start(vec![Reply::Streamed(vec![
        Chunk::Reasoning("pondering the question v5w".to_string()),
        Chunk::WaitFor(release.clone()),
        Chunk::Text("thought-out reply c3d".to_string()),
    ])]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"think about it\r").unwrap();
    let started = std::time::Instant::now();
    let transcript = wait_for_squeezed(&mut pty, "Thinking for", "the live thinking row");
    assert!(
        started.elapsed() < HOLD_LIMIT,
        "the Thinking row appeared only after the fake gave up holding the stream"
    );
    assert!(
        transcript.contains("Esc to interrupt"),
        "no busy status row was shown while the model was reasoning:\n{transcript}"
    );
    let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        !squeezed.contains("ponderingthequestionv5w"),
        "the reasoning text was painted into the transcript:\n{transcript}"
    );
    assert!(
        !squeezed.contains("thought-outreplyc3d"),
        "the answer was on screen before the fake sent it:\n{transcript}"
    );
    let settled = pty.read_for(Duration::from_millis(600));
    assert!(
        !settled
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .contains("thought-outreplyc3d"),
        "the answer reached the screen before the fake sent it:\n{settled}"
    );

    fs::write(&release, b"").unwrap();
    wait_for_squeezed(
        &mut pty,
        "thought-out reply c3d",
        "the answer after reasoning",
    );
    pty.read_for(Duration::from_millis(400));
    let answered_at = pty.raw().len();
    pty.read_for(Duration::from_millis(1600));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[answered_at..]));
    assert!(
        !since.contains("Esc to interrupt") && !since.contains("Working"),
        "the busy status row kept repainting after the answer arrived:\n{since}"
    );
    let whole: String = strip_ansi(&String::from_utf8_lossy(pty.raw()))
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        !whole.contains("ponderingthequestionv5w"),
        "the reasoning text reached the screen at some point during the turn"
    );
    assert_eq!(fake.served(), 1);

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// Frames are drawn inside synchronized updates on the real tty: every
/// begin has its end, the pairs never nest or interleave, the first begin
/// comes after the alternate screen is entered and the last end before it
/// is left.
#[test]
#[serial]
fn every_frame_on_the_tty_is_wrapped_in_a_synchronized_update() {
    let dir = fresh_config_dir("sync-updates");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);

    let raw = pty.raw();
    let enter_alt = find(raw, ENTER_ALT, 0).expect("the alternate screen was never entered");
    let leave_alt = rfind(raw, LEAVE_ALT).expect("the alternate screen was never left");
    let begins = count(raw, SYNC_BEGIN);
    let ends = count(raw, SYNC_END);
    assert!(
        begins >= 2,
        "fewer than two frames were drawn inside synchronized updates ({begins})"
    );
    assert_eq!(
        begins, ends,
        "synchronized updates were not balanced (begin={begins}, end={ends})"
    );

    // Walk the stream: begins and ends must strictly alternate.
    let mut at = 0usize;
    let mut open = false;
    let mut first_begin = None;
    let mut last_end = None;
    while let Some(next) = [find(raw, SYNC_BEGIN, at), find(raw, SYNC_END, at)]
        .into_iter()
        .flatten()
        .min()
    {
        if raw[next..].starts_with(SYNC_BEGIN) {
            assert!(
                !open,
                "a synchronized update began at {next} while one was open"
            );
            first_begin.get_or_insert(next);
            open = true;
        } else {
            assert!(open, "a synchronized update ended at {next} with none open");
            open = false;
            last_end = Some(next);
        }
        at = next + 1;
    }
    assert!(!open, "the last synchronized update was never ended");
    let first_begin = first_begin.unwrap();
    let last_end = last_end.unwrap();
    assert!(
        enter_alt < first_begin,
        "a frame was drawn before the alternate screen was entered \
         (begin={first_begin}, enter={enter_alt})"
    );
    assert!(
        last_end < leave_alt,
        "a frame ended after the alternate screen was left (end={last_end}, leave={leave_alt})"
    );
}

/// The status row offers "Esc to interrupt" while a turn runs. Pressing
/// Esc on a stream the model is still holding ends the turn at once: the
/// next command is handled well before the model would have let go, the
/// rest of the reply never lands, a fresh turn gets its own request, and
/// the UI leaves cleanly.
#[test]
#[serial]
fn esc_interrupts_a_held_stream_and_the_ui_keeps_taking_keys() {
    let dir = fresh_config_dir("esc-interrupt");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("never-released");
    let fake = FakeLlm::start(vec![
        Reply::Streamed(vec![
            Chunk::Text("first half int-a1".to_string()),
            Chunk::WaitFor(release),
            Chunk::Text(" tail int-b2".to_string()),
        ]),
        Reply::Text("fresh reply after the interrupt s2".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"stream and hold\r").unwrap();
    wait_for_squeezed(&mut pty, "first half int-a1", "the first delta");
    pty.wait_for("Esc to interrupt", PTY_DEADLINE)
        .expect("the status row never offered Esc to interrupt");
    let pressed = std::time::Instant::now();
    pty.send(b"\x1b").unwrap();
    // A command typed right after the interrupt runs as its own turn; had
    // the stream still been running it would queue behind the fake's
    // eight-second hold.
    pty.read_for(Duration::from_millis(500));
    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    let transcript = pty
        .wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("keys were not handled after the interrupt");
    assert!(
        pressed.elapsed() < Duration::from_secs(6),
        "the command ran only after the fake gave up holding the stream, not on Esc"
    );
    let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        !squeezed.contains("tailint-b2"),
        "the interrupted stream's tail still reached the screen:\n{transcript}"
    );

    pty.read_for(Duration::from_millis(300));
    pty.send(b"next prompt\r").unwrap();
    let transcript = wait_for_squeezed(
        &mut pty,
        "fresh reply after the interrupt s2",
        "the reply to the turn after the interrupt",
    );
    assert_eq!(fake.served(), 2, "{transcript}");
    let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        !squeezed.contains("tailint-b2"),
        "the interrupted stream's tail landed later:\n{transcript}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A tool that records the `COYOTE_UI` it was given, the value a tool
/// branches on to learn which surface answers its prompts.
const UI_RECORD_TOOL: &str = "#!/usr/bin/env bash\n\
    \n\
    # @describe Record the UI the tool runs under.\n\
    # @option --out! Path to write the UI name to.\n\
    \n\
    # @env LLM_OUTPUT=/dev/stdout The output path\n\
    \n\
    main() {\n\
    \x20   local out\n\
    \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
    \x20   printf '%s' \"${COYOTE_UI:-unset}\" > \"$out\"\n\
    \x20   echo \"fixture recorded ui w7e\" >> \"$LLM_OUTPUT\"\n\
    }\n";

/// Tools run from the full-screen UI are told `COYOTE_UI=tui`.
#[test]
#[serial]
fn a_tool_run_from_the_tui_sees_coyote_ui_tui() {
    let dir = fresh_config_dir("tool-ui-env");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("ui-seen");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_ui".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture done r4t".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_ui.sh", UI_RECORD_TOOL);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"call the ui tool\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's COYOTE_UI record");
    wait_for_squeezed(&mut pty, "fixture done r4t", "the reply after the tool");
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), "tui");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `TuiPrompter` answers a Confirm modal both ways: `n` is a real "no"
/// (the tool reads `0`, not a cancellation), the turn still finishes with
/// the model's follow-up, and the UI keeps taking keys afterwards.
#[test]
#[serial]
fn declining_a_tools_confirm_with_n_gives_the_tool_a_no_and_the_turn_still_finishes() {
    let dir = fresh_config_dir("tool-confirm-no");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("confirm-answer");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture declined t6k".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_confirm_rec.sh", RECORDING_CONFIRM_TOOL);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"call the confirm tool\r").unwrap();
    pty.wait_for("q9d-q", PTY_DEADLINE)
        .expect("the tool's confirm question never reached the screen");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"n").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript =
        wait_for_squeezed(&mut pty, "fixture declined t6k", "the reply after the tool");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "0",
        "`n` on the confirm modal did not reach the tool as a no:\n{transcript}"
    );
    assert!(
        !transcript.contains("coyote_prompt"),
        "a raw prompt-protocol line reached the screen:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE)
        .expect("keys were not handled after the declined confirm");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// On a terminal that answers the keyboard-enhancement probe (kitty,
/// ghostty, wezterm, foot) the flags are pushed after alt-screen entry
/// and popped before it is left, and with them on, a key *release* the
/// terminal reports in the kitty encoding must not type into the
/// composer (`KeyEventKind::Press` only) while a kitty-encoded press
/// still does.
#[test]
#[serial]
fn a_terminal_answering_the_keyboard_probe_gets_flags_pushed_and_releases_are_ignored() {
    let dir = fresh_config_dir("kitty-flags");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    let probe = pty
        .wait_for_raw(b"\x1b[?u", 0, PTY_DEADLINE)
        .expect("the keyboard-enhancement support probe (CSI ? u) was never sent");
    // A kitty-protocol terminal answers "flags currently 0" and then the
    // primary device attributes the probe pairs the query with (CSI c).
    pty.wait_for_raw(b"\x1b[c", probe, PTY_DEADLINE)
        .expect("the probe never asked for primary device attributes");
    pty.send(b"\x1b[?0u\x1b[?62;22c").unwrap();
    if let Err(err) = pty.wait_for("no session", PTY_DEADLINE) {
        panic!(
            "the UI never came up after the terminal answered the keyboard probe: {err}\n\
             raw bytes since the probe: {:?}",
            String::from_utf8_lossy(&pty.raw()[probe..])
        );
    }

    let raw = pty.raw();
    let enter_alt = find(raw, ENTER_ALT, 0).expect("the alternate screen was never entered");
    let push = find_keyboard_push(raw, 0)
        .expect("the terminal answered the probe but the flags were never pushed");
    assert!(
        enter_alt < push,
        "flags pushed before alt-screen entry (enter={enter_alt}, push={push})"
    );
    assert!(probe < push, "flags pushed before the probe was sent");

    pty.send(b"qz7").unwrap();
    pty.read_for(Duration::from_millis(300));
    // kitty encoding: CSI <codepoint> ; <mods>:<event-type> u; type 3 is release.
    pty.send(b"\x1b[107;1:3u").unwrap();
    pty.read_for(Duration::from_millis(300));
    // ...and type 1 is press.
    pty.send(b"\x1b[109;1:1u").unwrap();
    pty.read_for(Duration::from_millis(300));
    // Submitting shows the composer's exact contents twice: the prompt echo
    // and the dry-run reply, both painted whole rather than cell by cell.
    // Counted from the submit on, so the composer's own paints of the
    // typed text before Enter do not stand in for them.
    let submitted_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let text = loop {
        pty.read_for(Duration::from_millis(200));
        let text = strip_ansi(&String::from_utf8_lossy(&pty.raw()[submitted_at..]));
        let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            !squeezed.contains("qz7k"),
            "a kitty key *release* typed into the composer:\n{text}"
        );
        if squeezed.matches("qz7m").count() >= 2 {
            break text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "a kitty-encoded key press did not reach the composer (or Enter did not submit it):\n{text}"
        );
    };
    assert!(
        !text.contains("qz7mk") && !text.contains("qz7km"),
        "a kitty key *release* typed into the composer:\n{text}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    let raw = pty.raw();
    let leave_alt = rfind(raw, LEAVE_ALT).expect("the alternate screen was never left");
    let pop = rfind_keyboard_pop(raw).expect("flags pushed but never popped");
    assert!(
        push < pop && pop < leave_alt,
        "flags must be popped before leaving the alternate screen (push={push}, pop={pop}, leave={leave_alt})"
    );
}

/// A user who turns streaming off in their config still gets the reply:
/// a non-streamed assistant message (one JSON body, no SSE) lands in the
/// transcript as a rendered cell, the busy status row clears, and the UI
/// keeps taking keys afterwards.
#[test]
#[serial]
fn a_non_streamed_reply_lands_in_the_transcript_and_the_ui_stays_live() {
    let dir = fresh_config_dir("no-stream");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::Text("whole reply at once w4n".to_string()),
        Reply::Text("second whole reply w4o".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    // Same config with streaming turned off at the top level.
    let config = dir.join("config.yaml");
    let text = fs::read_to_string(&config).unwrap();
    assert!(
        text.contains("stream: true\n"),
        "fixture config lost its stream key"
    );
    fs::write(&config, text.replace("stream: true\n", "stream: false\n")).unwrap();
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"first\r").unwrap();
    wait_for_squeezed(
        &mut pty,
        "whole reply at once w4n",
        "the non-streamed reply",
    );
    assert_eq!(fake.served(), 1);

    // The turn is over: the busy row must stop repainting.
    pty.read_for(Duration::from_millis(400));
    let settled_at = pty.raw().len();
    pty.read_for(Duration::from_millis(1600));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[settled_at..]));
    assert!(
        !since.contains("Esc to interrupt") && !since.contains("Working"),
        "the busy status row kept repainting after a non-streamed reply:\n{since}"
    );

    // And a second turn still works, with the first still on screen.
    pty.send(b"second\r").unwrap();
    let text = wait_for_squeezed(&mut pty, "second whole reply w4o", "the second reply");
    let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        squeezed.contains("wholereplyatoncew4n"),
        "the first reply vanished from the transcript once the second arrived:\n{text}"
    );
    assert_eq!(fake.served(), 2);

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// OSC 8 hyperlinks and other raw escapes are stripped before the rendered
/// markdown reaches `ansi-to-tui`: a reply carrying a link shows the link's
/// text in the transcript, no OSC 8 introducer reaches the tty and no
/// fragment of one (`]8;;`) is painted as visible text.
#[test]
#[serial]
fn a_reply_with_a_link_shows_its_text_and_leaks_no_osc_8() {
    let dir = fresh_config_dir("osc8-link");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text(
        "see [the fixture page r2x](https://example.invalid/r2x-page) for more".to_string(),
    )]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let submitted_at = pty.raw().len();
    pty.send(b"link please\r").unwrap();
    let transcript = wait_for_squeezed(&mut pty, "the fixture page r2x", "the link's text");
    assert_eq!(fake.served(), 1, "{transcript}");
    // Let the reply settle fully before inspecting the bytes.
    pty.read_for(Duration::from_millis(600));

    let raw = pty.raw();
    let settled_at = raw.len();
    assert!(
        find(raw, b"\x1b]8;", 0).is_none(),
        "an OSC 8 sequence reached the tty through the frame:\n{}",
        strip_ansi(&String::from_utf8_lossy(raw))
    );
    let visible = strip_ansi(&String::from_utf8_lossy(raw));
    assert!(
        !visible.contains("]8;;"),
        "an OSC 8 fragment was painted as text:\n{visible}"
    );
    // Past the startup colour queries the UI writes no operating-system
    // commands of its own, so any `ESC ]` after the submit came through
    // from the reply.
    if let Some(at) = find(raw, b"\x1b]", submitted_at) {
        let context = &raw[at.saturating_sub(40)..(at + 80).min(raw.len())];
        panic!(
            "an OSC introducer reached the tty at {at}: {:?}",
            String::from_utf8_lossy(context)
        );
    }

    // The UI is still live afterwards.
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", settled_at, PTY_DEADLINE)
        .expect("`.info` after the linked reply never rendered");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

// ---------------------------------------------------------------------
// The interrupt keys at rest and mid-turn, and what the normal screen
// shows once the UI has left.
// ---------------------------------------------------------------------

/// Ctrl+C on an idle, empty composer quits exactly as Ctrl+D does: the
/// alternate screen is left once, the process exits 0, the tty is cooked.
#[test]
#[serial]
fn ctrl_c_on_an_idle_empty_composer_quits_like_ctrl_d() {
    let dir = fresh_config_dir("ctrl-c-idle-empty");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x03").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        1,
        "Ctrl+C on an idle empty composer must leave the alternate screen exactly once:\n{transcript}"
    );
}

/// Ctrl+C while the composer holds a draft clears the draft and nothing
/// else: the UI stays up, the draft is never submitted (Enter afterwards
/// starts no turn and the model sees no request), and the composer still
/// takes a command.
#[test]
#[serial]
fn ctrl_c_with_a_draft_clears_the_composer_and_does_not_quit() {
    let dir = fresh_config_dir("ctrl-c-draft");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text("must never be requested k4m".to_string())]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"draft to discard q9x").unwrap();
    pty.wait_for("draft to discard q9x", PTY_DEADLINE)
        .expect("the typed draft never reached the composer");
    pty.read_for(HOTKEY_GAP);
    let cleared_at = pty.raw().len();
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[cleared_at..]));
    assert!(
        !since.contains("exit="),
        "Ctrl+C with a draft in the composer quit the UI:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "Ctrl+C with a draft in the composer left the alternate screen:\n{since}"
    );

    // Enter now must find an empty composer: no turn, no request.
    pty.send(b"\r").unwrap();
    pty.read_for(HOTKEY_GAP);
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[cleared_at..]));
    assert!(
        !since.contains("Working"),
        "Enter after Ctrl+C started a turn, so the draft was not cleared:\n{since}"
    );
    assert_eq!(
        fake.served(),
        0,
        "the discarded draft reached the model:\n{since}"
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("keys were not handled after Ctrl+C cleared the draft");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(fake.served(), 0, "{transcript}");
}

/// Ctrl+D with a draft in the composer is not the quit chord: the UI
/// stays up and the draft stays put; once Ctrl+C has cleared it, Ctrl+D
/// leaves as usual.
#[test]
#[serial]
fn ctrl_d_with_a_draft_does_not_quit() {
    let dir = fresh_config_dir("ctrl-d-draft");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"draft kept on ctrl-d w2v").unwrap();
    pty.wait_for("draft kept on ctrl-d w2v", PTY_DEADLINE)
        .expect("the typed draft never reached the composer");
    pty.read_for(HOTKEY_GAP);
    let pressed_at = pty.raw().len();
    pty.send(b"\x04").unwrap();
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
    assert!(
        !since.contains("exit="),
        "Ctrl+D with a draft in the composer quit the UI:\n{since}"
    );
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "Ctrl+D with a draft in the composer left the alternate screen:\n{since}"
    );

    pty.send(b"\x03").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// Ctrl+C while a turn runs is the same soft interrupt as Esc: the held
/// stream ends at once, the process does NOT exit, the rest of the reply
/// never lands, and the next command is handled well before the model
/// would have let go.
#[test]
#[serial]
fn ctrl_c_during_a_held_stream_interrupts_the_turn_and_the_ui_stays_up() {
    let dir = fresh_config_dir("ctrl-c-interrupt");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("never-released");
    let fake = FakeLlm::start(vec![
        Reply::Streamed(vec![
            Chunk::Text("first half cc-a1".to_string()),
            Chunk::WaitFor(release),
            Chunk::Text(" tail cc-b2".to_string()),
        ]),
        Reply::Text("fresh reply after ctrl-c s3".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"stream and hold\r").unwrap();
    wait_for_squeezed(&mut pty, "first half cc-a1", "the first delta");
    pty.wait_for("Esc to interrupt", PTY_DEADLINE)
        .expect("the status row never offered Esc to interrupt");
    let pressed = std::time::Instant::now();
    let pressed_at = pty.raw().len();
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(500));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
    assert!(
        !since.contains("exit="),
        "Ctrl+C during a running turn quit the UI instead of interrupting the turn:\n{since}"
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    let transcript = pty
        .wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("keys were not handled after the Ctrl+C interrupt");
    assert!(
        pressed.elapsed() < Duration::from_secs(6),
        "the command ran only after the fake gave up holding the stream, not on Ctrl+C"
    );
    let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        !squeezed.contains("tailcc-b2"),
        "the interrupted stream's tail still reached the screen:\n{transcript}"
    );

    pty.read_for(Duration::from_millis(300));
    pty.send(b"next prompt\r").unwrap();
    let transcript = wait_for_squeezed(
        &mut pty,
        "fresh reply after ctrl-c s3",
        "the reply to the turn after the interrupt",
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// With a session open, Esc on a held stream behaves as the REPL's Ctrl+C
/// does: a "Response interrupted" notice lands in the transcript, the
/// partial text already streamed is kept, both survive a full repaint
/// once the UI is idle again, and the rest of the reply never arrives.
#[test]
#[serial]
fn esc_mid_stream_leaves_an_interrupted_cell_in_the_transcript() {
    let dir = fresh_config_dir("esc-interrupted-cell");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("never-released");
    let fake = FakeLlm::start(vec![Reply::Streamed(vec![
        Chunk::Text("partial text before esc e5p".to_string()),
        Chunk::WaitFor(release),
        Chunk::Text(" tail never shown e5q".to_string()),
    ])]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    // The partial-text path needs a session; without one the streaming
    // call fails the turn with "Aborted." (the REPL does the same).
    pty.send(b".session\r").unwrap();
    pty.wait_for("temp", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(b"stream and hold\r").unwrap();
    wait_for_squeezed(&mut pty, "partial text before esc e5p", "the first delta");
    pty.wait_for("Esc to interrupt", PTY_DEADLINE)
        .expect("the status row never offered Esc to interrupt");
    let pressed_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(6);
    let since = loop {
        pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
        if since.to_lowercase().contains("interrupted") {
            break since;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no cell said the turn was interrupted after Esc:\n{since}"
        );
    };
    assert!(
        !since.contains("exit="),
        "Esc during a running turn quit the UI:\n{since}"
    );
    // A resize forces a full repaint: the partial text and the interrupted
    // cell must both still be on the idle screen.
    pty.read_for(Duration::from_millis(800));
    let repaint_at = pty.raw().len();
    pty.resize(38, 100).unwrap();
    pty.wait_for_after("temp", repaint_at, PTY_DEADLINE)
        .expect("the screen was not repainted after the resize");
    pty.read_for(Duration::from_millis(500));
    let repainted = strip_ansi(&String::from_utf8_lossy(&pty.raw()[repaint_at..]));
    let squeezed: String = repainted.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        squeezed.contains("partialtextbeforeesce5p"),
        "the partial text streamed before Esc vanished from the transcript:\n{repainted}"
    );
    assert!(
        repainted.to_lowercase().contains("interrupted"),
        "the interrupted cell did not survive to the idle screen:\n{repainted}"
    );
    assert!(
        !squeezed.contains("tailnevershowne5q"),
        "the interrupted stream's tail reached the screen:\n{repainted}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// Without a session, Esc on a held stream fails the turn the way the
/// REPL's Ctrl+C does there: an "Aborted." error cell, no partial text
/// kept, and the UI stays up and keeps taking keys.
#[test]
#[serial]
fn esc_mid_stream_without_a_session_shows_the_aborted_error_and_stays_up() {
    let dir = fresh_config_dir("esc-aborted-no-session");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("never-released");
    let fake = FakeLlm::start(vec![Reply::Streamed(vec![
        Chunk::Text("partial before esc n8a".to_string()),
        Chunk::WaitFor(release),
        Chunk::Text(" tail never shown n8b".to_string()),
    ])]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"stream and hold\r").unwrap();
    wait_for_squeezed(&mut pty, "partial before esc n8a", "the first delta");
    pty.wait_for("Esc to interrupt", PTY_DEADLINE)
        .expect("the status row never offered Esc to interrupt");
    let pressed_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    let since = pty
        .wait_for_after("Aborted.", pressed_at, Duration::from_secs(6))
        .expect("no Aborted. error followed Esc on a session-less stream");
    assert!(
        !since.contains("exit="),
        "Esc during a running turn quit the UI:\n{since}"
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    let transcript = pty
        .wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("keys were not handled after the interrupt");
    let squeezed: String = transcript.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        !squeezed.contains("tailnevershownn8b"),
        "the interrupted stream's tail reached the screen:\n{transcript}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A lone Esc on an idle, empty composer is not a quit: the UI stays on
/// the alternate screen and keeps taking keys.
#[test]
#[serial]
fn esc_on_an_idle_empty_composer_does_not_quit() {
    let dir = fresh_config_dir("esc-idle");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(HOTKEY_GAP);
    let pressed_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(1000));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
    assert!(!since.contains("exit="), "a lone Esc quit the UI:\n{since}");
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        0,
        "a lone Esc left the alternate screen:\n{since}"
    );

    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .expect("keys were not handled after a lone Esc");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// On exit the committed transcript is written to the normal screen: after
/// a turn, Ctrl+D leaves the alternate screen and the user's prompt and the
/// reply are printed where the shell prompt will follow.
#[test]
#[serial]
fn ctrl_d_prints_the_committed_transcript_on_the_normal_screen() {
    let dir = fresh_config_dir("ctrl-d-dump");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text("the reply to keep v7k".to_string())]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"prompt to keep u6j\r").unwrap();
    wait_for_squeezed(&mut pty, "the reply to keep v7k", "the reply");
    pty.read_for(Duration::from_millis(800));
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);

    let raw = pty.raw();
    let left = rfind(raw, LEAVE_ALT).expect("the alternate screen was never left");
    let after = strip_ansi(&String::from_utf8_lossy(&raw[left..]));
    let report_at = after.find("exit=").expect("no exit= line after the leave");
    let between: String = after[..report_at]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    for marker in ["prompttokeepu6j", "thereplytokeepv7k"] {
        assert!(
            between.contains(marker),
            "the committed transcript ({marker:?}) was not printed on the normal screen \
             after the alternate screen was left: {:?}",
            after[..report_at].trim()
        );
    }
}

// ---------------------------------------------------------------------
// Shifted answers on the confirm modal, a narrow screen, fenced commands
// and the editor-backed commands the UI refuses.
// ---------------------------------------------------------------------

/// A confirm tool whose question is long enough to need several rows on
/// a narrow screen.
const LONG_QUESTION: &str =
    "Really continue with this long question that needs several rows at forty columns? q9d-wide";

/// The recording confirm tool asking [`LONG_QUESTION`] instead.
fn long_question_confirm_tool() -> String {
    RECORDING_CONFIRM_TOOL.replace("Really do it? q9d-q", LONG_QUESTION)
}

/// The yes/no modal takes a shifted answer too: `Y` is a yes (the tool
/// reads `1`) and `N` is a no (`0`), exactly like their lowercase twins.
/// Two turns in one session, so the second modal is also proof the UI
/// re-arms after the first.
#[test]
#[serial]
fn uppercase_y_and_n_answer_a_tools_confirm_like_their_lowercase_twins() {
    let dir = fresh_config_dir("tool-confirm-upper");
    let _guard = TempDirGuard(dir.clone());
    let out_yes = dir.join("confirm-answer-yes");
    let out_no = dir.join("confirm-answer-no");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out_yes.display()),
        },
        Reply::Text("fixture upper yes u7y".to_string()),
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out_no.display()),
        },
        Reply::Text("fixture upper no u7n".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_confirm_rec.sh", RECORDING_CONFIRM_TOOL);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"call the confirm tool\r").unwrap();
    pty.wait_for("q9d-q", PTY_DEADLINE)
        .expect("the tool's confirm question never reached the screen");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"Y").unwrap();
    wait_for_file(&mut pty, &out_yes, "the tool's recorded answer to Y");
    let transcript = wait_for_squeezed(&mut pty, "fixture upper yes u7y", "the reply after Y");
    assert_eq!(
        fs::read_to_string(&out_yes).unwrap().trim(),
        "1",
        "`Y` on the confirm modal did not reach the tool as a yes:\n{transcript}"
    );

    let second = pty.raw().len();
    pty.send(b"call the confirm tool again\r").unwrap();
    pty.wait_for_after("q9d-q", second, PTY_DEADLINE)
        .expect("the second confirm question never reached the screen");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"N").unwrap();
    wait_for_file(&mut pty, &out_no, "the tool's recorded answer to N");
    let transcript = wait_for_squeezed(&mut pty, "fixture upper no u7n", "the reply after N");
    assert_eq!(
        fs::read_to_string(&out_no).unwrap().trim(),
        "0",
        "`N` on the confirm modal did not reach the tool as a no:\n{transcript}"
    );
    assert!(
        !transcript.contains("coyote_prompt"),
        "a raw prompt-protocol line reached the screen:\n{transcript}"
    );
    assert_eq!(fake.served(), 4, "{transcript}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// On a 40-column screen a confirm question longer than the width is
/// wrapped, not clipped: every character of it is on screen when the
/// modal is up, and `y` still answers it.
#[test]
#[serial]
fn a_long_confirm_question_is_fully_visible_on_a_forty_column_screen() {
    let dir = fresh_config_dir("tool-confirm-narrow");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("confirm-answer");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture narrow done n4w".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_confirm_rec.sh",
        &long_question_confirm_tool(),
    );
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(24, 40).unwrap();
    pty.read_for(Duration::from_millis(500));
    let before = pty.raw().len();
    pty.send(b"call the confirm tool\r").unwrap();
    let modal = pty
        .wait_for_after("q9d-wide", before, PTY_DEADLINE)
        .expect("the tool's confirm question never reached the 40-column screen");
    // Let the modal finish painting before reading it back.
    pty.read_for(Duration::from_millis(500));
    let modal = format!(
        "{modal}{}",
        strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]))
    );
    let shown = squeezed(&modal);
    assert!(
        shown.contains(&squeezed(LONG_QUESTION)),
        "the confirm question was clipped on a 40-column screen; printable bytes since the \
         prompt was sent:\n{modal}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    let transcript = wait_for_squeezed(
        &mut pty,
        "fixture narrow done n4w",
        "the reply after the tool",
    );
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "{transcript}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A `.command` wrapped in the REPL's `:::` multi-line fence is still a
/// command: pasted as three lines and submitted, `.info` yields its table
/// in the transcript instead of going to the model as a prompt (a dry-run
/// reply would only echo the fence back). The table is taller than the
/// screen, so the rows checked are its last ones.
#[test]
#[serial]
fn a_fenced_dot_command_runs_as_a_command_and_not_as_a_prompt() {
    let dir = fresh_config_dir("fenced-command");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    pty.send(b"\x1b[200~:::\r.info\r:::\x1b[201~").unwrap();
    pty.wait_for_after(".info", before, PTY_DEADLINE)
        .expect("the pasted fence never reached the composer");
    pty.read_for(Duration::from_millis(300));
    // Three pasted lines, three stacked composer rows.
    let fences = painted_at(pty.raw(), b":::", before);
    let command = painted_at(pty.raw(), b".info", before);
    let (Some(&(open, col)), Some(&(close, _)), Some(&(middle, _))) =
        (fences.first(), fences.last(), command.last())
    else {
        panic!("the fenced paste was not positioned on screen");
    };
    assert_eq!(
        middle,
        open + 1,
        "fences at {fences:?}, command at {command:?}"
    );
    assert_eq!(close, open + 2, "fences at {fences:?}");
    assert_eq!(command.last().map(|p| p.1), Some(col), "{command:?}");
    let submitted = pty.raw().len();
    pty.send(b"\r").unwrap();
    let since = pty
        .wait_for_after("log_path", submitted, PTY_DEADLINE)
        .expect("the fenced `.info` did not run as a command");
    assert!(
        since.contains("config_dir") && since.contains("dry_run"),
        "the `.info` table's tail is incomplete:\n{since}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `.skill <name>` scaffolds the skill file and opens it in `$EDITOR` on
/// the lent terminal: the alternate screen is left for the editor, the
/// editor is handed the skill file's path, the file exists afterwards,
/// and the UI comes back repainted and taking keys.
#[test]
#[serial]
fn dot_skill_scaffolds_the_file_and_opens_it_on_the_lent_terminal() {
    let dir = fresh_config_dir("skill-editor");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let launched = dir.join("editor-launched");
    let editor = recording_editor(&dir, &launched);
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".skill probe-skill-zq7\r").unwrap();
    let left = pty
        .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
        .expect("`.skill` did not release the terminal for the editor");
    let re_entered = wait_for_re_entry(&mut pty, left);
    assert_repainted_after(&mut pty, re_entered, "the `.skill` editor");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));

    let edited = fs::read_to_string(&launched)
        .unwrap_or_else(|e| panic!("`.skill` never launched the editor ({e}):\n{since}"));
    let opened = edited.lines().next().unwrap_or_default();
    assert!(
        opened.contains("probe-skill-zq7") && opened.contains("SKILL.md"),
        "the editor was not handed the skill file: {edited:?}\n{since}"
    );
    assert!(
        edited.lines().nth(1) == Some("existed"),
        "the skill file {opened:?} was not scaffolded before the editor opened: {edited:?}\n{since}"
    );
    let body = fs::read_to_string(opened).unwrap();
    assert!(
        body.starts_with("---\n") && body.contains("description:"),
        "the skill file {opened:?} holds {body:?}, not the scaffold:\n{since}"
    );
    let skill_files = new_files_named(&dir, &files_before, "probe-skill-zq7");
    assert!(
        !skill_files.is_empty(),
        "no probe-skill-zq7 file appeared under {dir:?}:\n{since}"
    );

    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", re_entered, PTY_DEADLINE)
        .expect("keys were not handled after the `.skill` editor returned");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

// ---------------------------------------------------------------------
// The pty harness itself: the escape stripper every assertion above
// reads through, and the descriptor hygiene of the spawn.
// ---------------------------------------------------------------------

#[test]
fn csi_with_intermediate_bytes_is_dropped_whole() {
    // Kitty keyboard pop (`ESC [ < 1 u`, also without its count) and a
    // DEC private mode reset (`ESC [ ? 2004 l`), plus a space
    // intermediate variant.
    assert_eq!(strip_ansi("a\x1b[<1ub"), "ab");
    assert_eq!(strip_ansi("a\x1b[<ub"), "ab");
    assert_eq!(strip_ansi("a\x1b[?2004lb"), "ab");
    assert_eq!(strip_ansi("a\x1b[2 qb"), "ab");
    assert_eq!(strip_ansi("a\x1b[1;31mb\x1b[0m"), "ab");
    // A CSI cut short by the next escape does not swallow that escape.
    assert_eq!(strip_ansi("a\x1b[31\x1b[0mb"), "ab");
}

#[test]
fn non_csi_escapes_with_an_intermediate_are_dropped_whole() {
    assert_eq!(strip_ansi("x\x1b(By"), "xy");
    assert_eq!(strip_ansi("x\x1b=y\x1b>z"), "xyz");
    assert_eq!(strip_ansi("x\x1b]0;title\x07y\x1b]8;;\x1b\\z"), "xyz");
}

#[test]
fn a_truncated_escape_at_the_end_does_not_panic() {
    assert_eq!(strip_ansi("tail\x1b"), "tail");
    assert_eq!(strip_ansi("tail\x1b["), "tail");
    assert_eq!(strip_ansi("tail\x1b]"), "tail");
    assert_eq!(strip_ansi("tail\x1b("), "tail");
}

/// Control strings other than OSC are consumed whole as well: a DCS, APC,
/// PM or SOS payload is not screen text.
#[test]
fn control_string_payloads_are_not_screen_text() {
    assert_eq!(strip_ansi("a\x1bPq#0;2;0;0;0\x1b\\b"), "ab");
    assert_eq!(strip_ansi("a\x1b_Gf=100\x1b\\b"), "ab");
    assert_eq!(strip_ansi("a\x1b^privacy\x07b"), "ab");
    assert_eq!(strip_ansi("a\x1bXstart\x1b\\b"), "ab");
    assert_eq!(strip_ansi("tail\x1bP"), "tail");
    // An unterminated string ends at the next bare ESC, which then starts
    // its own sequence: the text after it is kept.
    assert_eq!(strip_ansi("a\x1b]8;;broken\x1b[31mb\x1b[0m"), "ab");
}

/// The conceal oracle reads `8` only as its own parameter: the palette
/// index or channel value 8 inside an extended colour is colour.
#[test]
fn the_conceal_oracle_steps_over_extended_colour_arguments() {
    assert!(is_conceal_sgr(&[8]));
    assert!(is_conceal_sgr(&[1, 8]));
    assert!(is_conceal_sgr(&[38, 5, 8, 8]));
    assert!(is_conceal_sgr(&[48, 2, 8, 8, 8, 8]));
    assert!(!is_conceal_sgr(&[38, 5, 8]));
    assert!(!is_conceal_sgr(&[48, 5, 8, 1]));
    assert!(!is_conceal_sgr(&[58, 2, 8, 8, 8]));
    assert!(!is_conceal_sgr(&[0]));
    assert!(!is_conceal_sgr(&[28]));
}

// ---------------------------------------------------------------------
// The clipped confirm modal, the hint on a narrow screen, the
// editor-backed commands and the `!shell` arm under the UI, and the
// `.info` capture under CPU load.
// ---------------------------------------------------------------------

/// A `Remove '<path>'?` question whose path alone spans 13 rows at 40
/// columns (520 chars, broken at the width), so a 20-row screen cannot
/// show it whole.
fn clipped_question() -> String {
    let path: String = (0..13)
        .map(|i| format!("/component-{i:02}-{}", "x".repeat(24)))
        .collect();
    format!("q9d-clip Remove '{path}'? q9d-clip-end")
}

/// The recording confirm tool asking [`clipped_question`] instead.
fn clipped_question_confirm_tool() -> String {
    RECORDING_CONFIRM_TOOL.replace("Really do it? q9d-q", &clipped_question())
}

/// Every token of the modal's hint once the question fits, whitespace
/// squeezed out.
const HINT_TOKENS: [&str; 5] = [
    "y=yes",
    "n=no",
    "a=yesforthissession",
    "Enter=yes",
    "Esc=cancel",
];

/// Starts the UI at 40x20 with the clipped-question confirm tool, submits
/// the prompt and returns once the modal's clip marker is on screen,
/// handing back the printable bytes since the prompt was sent.
fn open_clipped_modal(pty: &mut Pty) -> String {
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(20, 40).unwrap();
    pty.read_for(Duration::from_millis(500));
    let before = pty.raw().len();
    pty.send(b"call the confirm tool\r").unwrap();
    pty.wait_for_after("q9d-clip", before, PTY_DEADLINE)
        .expect("the tool's confirm question never reached the 40x20 screen");
    pty.wait_for_after("more lines", before, PTY_DEADLINE)
        .expect("the clipped question showed no `(+N more lines` marker");
    pty.read_for(Duration::from_millis(500));
    strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]))
}

/// A question taller than the screen is marked `(+N more lines; enlarge
/// the terminal)` and `y` does nothing while it is clipped: the tool gets
/// no answer, and the legend offers only Esc. Once the window is enlarged
/// the whole question is visible, the full hint is back and `y` answers
/// it.
#[test]
#[serial]
fn a_clipped_confirm_question_refuses_y_until_the_terminal_is_enlarged() {
    let dir = fresh_config_dir("confirm-clipped-y");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("confirm-answer");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture clipped done c7y".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_confirm_rec.sh",
        &clipped_question_confirm_tool(),
    );
    let mut pty = spawn_tui(&dir, &[]);

    let modal = open_clipped_modal(&mut pty);
    let shown = squeezed(&modal);
    assert!(
        shown.contains("(+") && shown.contains("morelines;enlargetheterminal)"),
        "no `(+N more lines; enlarge the terminal)` marker on the clipped modal:\n{modal}"
    );
    assert!(
        !shown.contains("q9d-clip-end"),
        "the question's tail was painted although the marker says it is clipped:\n{modal}"
    );
    assert!(
        shown.contains("Esc=cancelenlargetheterminaltoanswer"),
        "the clipped legend is missing from the 40-column modal:\n{modal}"
    );
    assert!(
        !shown.contains("y=yes"),
        "the clipped legend still offers an approving key:\n{modal}"
    );

    pty.read_for(HOTKEY_GAP);
    pty.send(b"y").unwrap();
    let after_y = pty.read_for(Duration::from_secs(2));
    assert!(
        !out.exists(),
        "`y` answered the confirm while its question was clipped (the tool recorded {:?}):\n{after_y}",
        fs::read_to_string(&out).unwrap_or_default()
    );
    assert_eq!(
        fake.served(),
        1,
        "the turn moved on while the modal was clipped:\n{after_y}"
    );

    let grown_at = pty.raw().len();
    pty.resize(50, 100).unwrap();
    let grown = pty
        .wait_for_after("q9d-clip-end", grown_at, PTY_DEADLINE)
        .expect("enlarging the terminal did not reveal the rest of the question");
    assert!(
        !squeezed(&grown).contains("morelines;enlarge"),
        "the clip marker is still shown after the whole question fits:\n{grown}"
    );
    // `read_for` hands back the whole screen so far; the approving keys
    // were never offered before the question fit.
    let grown = squeezed(&pty.read_for(HOTKEY_GAP));
    for token in HINT_TOKENS {
        assert!(
            grown.contains(token),
            "the hint token {token:?} did not come back once the question fit:\n{grown}"
        );
    }
    pty.send(b"y").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer after enlarging");
    let transcript = wait_for_squeezed(
        &mut pty,
        "fixture clipped done c7y",
        "the reply after the tool",
    );
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "`y` on the fully visible modal did not reach the tool as a yes:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// Esc still cancels a clipped confirm: the tool reads no yes, the turn
/// finishes with the model's follow-up and the UI keeps taking keys.
#[test]
#[serial]
fn esc_still_cancels_a_clipped_confirm_question() {
    let dir = fresh_config_dir("confirm-clipped-esc");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("confirm-answer");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_confirm_rec".to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture clipped esc c7e".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_confirm_rec.sh",
        &clipped_question_confirm_tool(),
    );
    let mut pty = spawn_tui(&dir, &[]);

    let _modal = open_clipped_modal(&mut pty);
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer to Esc");
    let transcript = wait_for_squeezed(&mut pty, "fixture clipped esc c7e", "the reply after Esc");
    let answer = fs::read_to_string(&out).unwrap();
    assert_ne!(
        answer.trim(),
        "1",
        "Esc on the clipped modal reached the tool as a yes:\n{transcript}"
    );
    assert_eq!(fake.served(), 2, "{transcript}");

    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE)
        .expect("keys were not handled after Esc on the clipped modal");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `.role <new-name>` asks `Create a new role?` in a modal; `y` opens the
/// new role's file in `$EDITOR` on the lent terminal, and the UI comes
/// back taking keys.
#[test]
#[serial]
fn dot_role_for_a_new_role_asks_in_the_modal_and_opens_the_editor_on_y() {
    let dir = fresh_config_dir("role-modal");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let launched = dir.join("editor-launched");
    let editor = recording_editor(&dir, &launched);
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before = pty.raw().len();
    pty.send(b".role new-role-xk3\r").unwrap();
    pty.wait_for_after("Create a new role?", before, PTY_DEADLINE)
        .expect("`.role <new>` did not ask `Create a new role?` in a modal");
    assert!(
        !launched.exists(),
        "the editor opened before the question was answered"
    );
    pty.read_for(HOTKEY_GAP);
    let answered = pty.raw().len();
    pty.send(b"y").unwrap();
    let left = pty
        .wait_for_raw(LEAVE_ALT, answered, PTY_DEADLINE)
        .expect("`y` did not release the terminal for the role editor");
    let re_entered = wait_for_re_entry(&mut pty, left);
    assert_repainted_after(&mut pty, re_entered, "the `.role` editor");
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    let edited = fs::read_to_string(&launched)
        .unwrap_or_else(|e| panic!("`y` never launched the editor ({e}):\n{since}"));
    assert!(
        edited
            .lines()
            .next()
            .is_some_and(|path| path.contains("new-role-xk3")),
        "the editor was not handed the new role's file: {edited:?}\n{since}"
    );
    assert!(
        edited.lines().nth(1) == Some("missing"),
        "the new role's file existed before its editor opened: {edited:?}\n{since}"
    );
    let role_files = new_files_named(&dir, &[], "new-role-xk3");
    assert!(
        role_files.iter().any(|path| path.is_file()),
        "`y` opened the editor but wrote no role file (new files: {role_files:?}):\n{since}"
    );

    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", re_entered, PTY_DEADLINE)
        .expect("keys were not handled after the role editor returned");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// `.edit session` on a live named session writes it to disk and opens
/// its yaml in `$EDITOR` on the lent terminal; the UI comes back taking
/// keys.
#[test]
#[serial]
fn dot_edit_session_opens_the_editor_on_the_lent_terminal() {
    let dir = fresh_config_dir("edit-session-editor");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let launched = dir.join("editor-launched");
    let editor = recording_editor(&dir, &launched);
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".session named-sess-xk3\r").unwrap();
    pty.wait_for("named-sess-xk3", PTY_DEADLINE)
        .expect("the named session never showed in the footer");
    pty.read_for(Duration::from_millis(500));
    let files_before = walk(&dir);
    let before = pty.raw().len();
    pty.send(b".edit session\r").unwrap();
    let left = pty
        .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
        .expect("`.edit session` did not release the terminal for the editor");
    let re_entered = wait_for_re_entry(&mut pty, left);
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));

    let edited = fs::read_to_string(&launched)
        .unwrap_or_else(|e| panic!("`.edit session` never launched the editor ({e}):\n{since}"));
    let opened = edited.lines().next().unwrap_or_default();
    assert!(
        opened.contains("named-sess-xk3"),
        "the editor was not handed the session file: {edited:?}\n{since}"
    );
    assert!(
        edited.lines().nth(1) == Some("existed"),
        "the session file {opened:?} was not written before the editor opened: {edited:?}\n{since}"
    );
    let session_files = new_files_named(&dir, &files_before, "named-sess-xk3");
    assert!(
        !session_files.is_empty(),
        "no named-sess-xk3 session file appeared under {dir:?}:\n{since}"
    );

    pty.wait_for_after("Type a prompt", re_entered, PTY_DEADLINE)
        .expect("the composer was not repainted after the session editor returned");
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", re_entered, PTY_DEADLINE)
        .expect("keys were not handled after the session editor returned");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// A `!shell` line is a command, not a message for the model: it runs on
/// the lent terminal (alternate screen left, the child's output on the
/// normal screen, alternate screen re-entered) and the fake model is
/// never asked.
#[test]
#[serial]
fn a_bang_shell_line_runs_on_the_lent_terminal_and_never_reaches_the_model() {
    let dir = fresh_config_dir("bang-shell");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text(
        "fixture must not be served b4s".to_string(),
    )]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    pty.send(b"!echo bang-probe-q9d\r").unwrap();
    let left = pty
        .wait_for_raw(LEAVE_ALT, before, PTY_DEADLINE)
        .expect("`!echo` did not release the terminal for the shell");
    let re_entered = wait_for_re_entry(&mut pty, left);
    // The typed line echoes in the composer before the send, so only the
    // window between the leave and the re-entry can hold the child's own
    // output.
    let on_the_normal_screen = strip_ansi(&String::from_utf8_lossy(&pty.raw()[left..re_entered]));
    assert!(
        on_the_normal_screen.contains("bang-probe-q9d"),
        "the shell's output did not land on the normal screen:\n{on_the_normal_screen}"
    );
    pty.read_for(Duration::from_millis(800));
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    assert_eq!(
        fake.served(),
        0,
        "`!echo` was sent to the model as a prompt:\n{since}"
    );
    assert!(
        !since.contains("b4s"),
        "the model's reply landed for a `!shell` line:\n{since}"
    );

    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", re_entered, PTY_DEADLINE)
        .expect("keys were not handled after the shell returned");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(fake.served(), 0, "{transcript}");
}

/// Busy loops on every core for the life of the guard.
struct CpuLoad(Vec<std::process::Child>);

impl CpuLoad {
    fn start() -> CpuLoad {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let children = (0..cores * 2)
            .filter_map(|_| {
                Command::new("sh")
                    .arg("-c")
                    // Counted, so a loop that outlives its test (a killed
                    // harness) still burns out on its own within a minute.
                    .arg("i=0; while [ $i -lt 40000000 ]; do i=$((i+1)); done")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .ok()
            })
            .collect();
        CpuLoad(children)
    }
}

impl Drop for CpuLoad {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Under CPU load the `.info` table still lands in the Command cell
/// directly under `> .info`: the capture waits for its drain thread
/// rather than calling the pipe quiet early and routing the table into a
/// later notice a row lower. Three cold starts, each on a fresh screen so
/// the row placement is unambiguous.
#[test]
#[serial]
fn dot_info_lands_in_its_command_cell_under_cpu_load() {
    let load = CpuLoad::start();
    for round in 0..3 {
        let dir = fresh_config_dir(&format!("info-under-load-{round}"));
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(80, 100).unwrap();
        pty.read_for(Duration::from_millis(300));
        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .unwrap_or_else(|e| panic!("round {round}: the `.info` table never appeared: {e:?}"));
        pty.read_for(Duration::from_millis(800));
        let printable = strip_ansi(&String::from_utf8_lossy(&pty.raw()[info_at..]));
        let header_rows = painted_at(pty.raw(), b"> .info", info_at);
        let &(header_row, 1) = header_rows.first().unwrap_or_else(|| {
            panic!("round {round}: `> .info` was never painted at column one:\n{printable}")
        }) else {
            panic!("round {round}: `> .info` was painted mid-row: {header_rows:?}");
        };
        let first_rows = painted_at(pty.raw(), b"model", info_at);
        assert!(
            first_rows.contains(&(header_row + 1, 1)),
            "round {round}: under load the table's first row was not painted directly under \
             `> .info` (row {header_row}); `model` was painted at {first_rows:?}:\n{printable}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "round {round}: {transcript}");
        assert_tty_restored(&transcript);
    }
    drop(load);
}
