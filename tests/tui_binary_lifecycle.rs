//! Real-process checks of the full-screen UI's terminal lifecycle: however
//! the binary leaves (Ctrl+D, `.exit`, SIGTERM, SIGINT, SIGHUP, a panic
//! after the alternate screen is up) the tty it ran on must come back in
//! its cooked, echoing state. Also here: the refusals to start without a
//! tty, the enter and leave sequences, Ctrl+Z and `fg`, the editor's
//! shell-out, the save modal on the way out, the colour environment, a
//! resize, and a cold start with no cache dir. Each case runs the binary
//! on a pseudo-terminal under a bash wrapper that reports the exit code
//! and `stty -a` afterwards, so the file is unix-only. Turns driven by a
//! scripted model, paced streams, the interrupt keys and the confirm
//! modal live in `tui_binary.rs`; the command menus' modals, the plain
//! REPL's type-ahead and the runs with no human to ask live in
//! `tui_binary_menus.rs`.
//! The cases run one at a time: each spawn forks a session leader and takes
//! a controlling tty, and a fork landing between another test's `openpty`
//! and its `exec` has failed the spawn with a garbled errno.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

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
    // is seen, before a loaded host can let the wait run out. The wait is
    // crossterm's own fixed timeout inside the process, so no terminal on
    // this side can hold it open for longer.
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
    // The screen is entered only once the probe has answered or timed
    // out, so a hang-up that landed inside it leaves no alternate screen
    // and no footer behind.
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the alternate screen was entered, so the hang-up missed the probe:\n{transcript}"
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
        "'{{coyote}}'{{args}} <{{stdin}} >'{}' & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {{report}}",
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
/// transcript ahead of the tty report: the prompt line and the reply's
/// last line each once, in that order. The dry-run reply echoes the
/// whole request, so the lines between the prompt line and that last
/// line are not checked; no line before or after them looks like a log
/// line, a warning or an error (about the missing directories, say).
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
    // and ends on the rendered user message.
    let prompt = "> **zebra** and `quartz` marker";
    let reply = "zebra and quartz marker";
    for line in [prompt, reply] {
        assert_eq!(
            printed.iter().filter(|printed| **printed == line).count(),
            1,
            "{line:?} is not printed exactly once: {printed:#?}"
        );
    }
    let prompt_at = printed.iter().position(|line| *line == prompt).unwrap();
    let reply_at = printed.iter().position(|line| *line == reply).unwrap();
    assert!(prompt_at < reply_at, "{printed:#?}");
    let leaked: Vec<&&str> = printed[..=prompt_at]
        .iter()
        .chain(&printed[reply_at..])
        .filter(|line| {
            let line = line.trim_start();
            [
                "WARN", "ERROR", "INFO", "DEBUG", "Failed", "warning:", "error:", "Error:",
                "Warning:",
            ]
            .iter()
            .any(|prefix| line.starts_with(prefix))
                || ["panicked", "No such file", "cannot", "could not"]
                    .iter()
                    .any(|word| line.contains(word))
                || [" WARN ", " ERROR ", " INFO ", " DEBUG "]
                    .iter()
                    .any(|level| line.contains(level))
        })
        .collect();
    assert!(
        leaked.is_empty(),
        "lines printed around the echoed request look like log output: {leaked:#?}"
    );
}
