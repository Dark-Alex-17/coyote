//! Real-process checks of the UI's lifecycle and signal handling: cold
//! start, exit and terminal restore, suspend and resume, signals,
//! interrupts and bracketed paste. Every module drives the binary
//! through the shared pty harness in `common`, so the file is
//! unix-only.
#![cfg(unix)]

mod common;

mod binary_lifecycle {
    //! Real-process checks of the full-screen UI's terminal lifecycle: however
    //! the binary leaves (Ctrl+D, `.exit`, SIGTERM, SIGINT, SIGHUP, a panic
    //! after the alternate screen is up) the tty it ran on must come back in
    //! its cooked, echoing state. Also here: the refusals to start without a
    //! tty, the enter and leave sequences, Ctrl+Z and `fg`, the editor's
    //! shell-out, the save modal on the way out, the colour environment, a
    //! resize, and a cold start with no cache dir. Each case runs the binary
    //! on a pseudo-terminal under a bash wrapper that reports the exit code
    //! and `stty -a` afterwards, so the module is unix-only. Turns driven by a
    //! scripted model, paced streams, the interrupt keys and the confirm
    //! modal live in `binary` in `tui_tools.rs`; the command menus' modals, the plain
    //! REPL's type-ahead and the runs with no human to ask live in
    //! `binary_menus` in `tui_modals.rs`.
    //! The cases run one at a time: each spawn forks a session leader and takes
    //! a controlling tty, and a fork landing between another test's `openpty`
    //! and its `exec` has failed the spawn with a garbled errno.
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
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
        let panic_line = b"panicked at src/tui/control.rs";
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

        // The harness answers the keyboard-enhancement probe as a terminal
        // without the protocol, so no flags may be pushed.
        assert!(
            find(raw, b"\x1b[?u", 0).is_some(),
            "the keyboard-enhancement support probe (CSI ? u) was never sent"
        );
        assert!(
            find_keyboard_push(raw, 0).is_none(),
            "flags were pushed although the probe was answered without the protocol"
        );
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
        pty.leave_keyboard_probe_unanswered();

        let transcript = pty.wait_for("cpid=", PTY_DEADLINE).unwrap();
        let pid = child_pid(&transcript);
        // The probe opens with its query and then waits two seconds for a
        // reply this test never sends; the hang-up goes out as soon as the
        // query is seen, before a loaded host can let the wait run out. The
        // wait is crossterm's own fixed timeout inside the process, so no
        // terminal on this side can hold it open for longer.
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
    fn suspend_and_check_handback_with_cursor(pty: &mut Pty, before_suspend: usize) -> usize {
        pty.send(b"\x1a").unwrap();
        let stopped = pty.wait_for("stopped-probe-done", PTY_DEADLINE).unwrap();
        let left_at = find(pty.raw(), LEAVE_ALT, before_suspend).unwrap_or_else(|| {
            panic!("Ctrl-Z stopped the job without leaving the alternate screen")
        });
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
        let stopped_marker = suspend_and_check_handback_with_cursor(&mut pty, before_suspend);

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
        let stopped_marker = suspend_and_check_handback_with_cursor(&mut pty, before_suspend);
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
        let stopped_marker = suspend_and_check_handback_with_cursor(&mut pty, before_suspend);
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
}

mod binary_exit {
    //! Real-process checks of the transcript the full-screen UI prints on the
    //! normal screen when it leaves: on Ctrl+C, `.exit` and SIGTERM, nothing
    //! before any turn or under `tui_dump_on_exit: false`, the printed reply
    //! without colour under `NO_COLOR`, and row for row what the plain REPL
    //! prints for the same reply, and a bounded, clean exit while a roster
    //! cancel still waits out a job's kill grace (the Ctrl+D case lives in
    //! `binary` in `tui_tools.rs`, the cold start in `binary_lifecycle`). Each
    //! case runs the binary on a pseudo-terminal under a bash wrapper that
    //! reports the exit code and `stty -a` afterwards, so the module is
    //! unix-only. The cases run one at a time: each spawn forks a session
    //! leader and takes a controlling tty, and a fork landing between another
    //! test's `openpty` and its `exec` has failed the spawn with a garbled
    //! errno.
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use super::common::fake_llm::{FakeLlm, Reply};
    use super::common::pty::strip_ansi;
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// Ctrl+C on an idle empty composer leaves the UI like Ctrl+D does, and
    /// the transcript is printed on the normal screen on that path too.
    #[test]
    #[serial]
    fn ctrl_c_on_an_idle_composer_also_prints_the_transcript() {
        leaving_prints_the_transcript(
            "ctrl-c-dump",
            |pty, _| pty.send(b"\x03").unwrap(),
            "prompt to keep z1c",
            "the reply to keep h3w",
        );
    }

    /// `.exit` leaves the UI too, and prints the transcript the same way.
    #[test]
    #[serial]
    fn dot_exit_also_prints_the_transcript() {
        leaving_prints_the_transcript(
            "dot-exit-dump",
            |pty, _| pty.send(b".exit\r").unwrap(),
            "prompt to keep e8y",
            "the reply to keep m4s",
        );
    }

    /// SIGTERM after a finished turn leaves the UI and prints the transcript
    /// like a key does.
    #[test]
    #[serial]
    fn sigterm_after_a_turn_also_prints_the_transcript() {
        leaving_prints_the_transcript(
            "sigterm-dump",
            |_, pid| {
                // SAFETY: kill takes a pid and a signal number and touches no memory.
                assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
            },
            "prompt to keep s7g",
            "the reply to keep t4m",
        );
    }

    /// With `tui_dump_on_exit: false` the transcript is not printed when the
    /// UI leaves: the wrapper's report follows the leave directly.
    #[test]
    #[serial]
    fn tui_dump_on_exit_false_prints_nothing_between_the_leave_and_the_report() {
        let dir = fresh_config_dir("dump-off");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text("the reply to drop k4w".to_string())]);
        write_fake_llm_config_with(&dir, &fake, "tui_dump_on_exit: false\n");
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"prompt to drop j3q\r").unwrap();
        wait_for_squeezed(&mut pty, "the reply to drop k4w", "the reply");
        pty.read_for(Duration::from_millis(800));
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);

        let between = strip_ansi(&String::from_utf8_lossy(&raw_between_leave_and_report(
            &pty,
        )));
        assert!(
            between.trim().is_empty(),
            "the transcript was printed with tui_dump_on_exit: false: {:?}",
            between.trim()
        );
    }

    /// A first start where nothing is said: Ctrl+D prints no transcript, no
    /// banner and no log line before the report.
    #[test]
    #[serial]
    fn ctrl_d_before_any_turn_prints_nothing_on_the_normal_screen() {
        let dir = fresh_config_dir("dump-empty");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(500));
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        let between = strip_ansi(&String::from_utf8_lossy(&raw_between_leave_and_report(
            &pty,
        )));
        assert!(
            between.trim().is_empty(),
            "bytes were printed after the leave with no turn: {:?}",
            between.trim()
        );
    }

    /// A markdown reply with a heading, bold, inline code, a list and a
    /// fenced block.
    const MARKDOWN_REPLY: &str = "# Heading r6t\n\nSome **bold b4k** and `code c2m` here.\n\n- item one i1q\n- item two i2q\n\n```rust\nfn fenced_f7p() {}\n```\n";

    fn is_color_sgr(params: &[u16]) -> bool {
        params
            .iter()
            .any(|p| matches!(p, 30..=38 | 40..=48 | 90..=97 | 100..=107))
    }

    /// With `NO_COLOR` set the transcript printed on exit carries no colour
    /// escape, while the reply's words are all there.
    #[test]
    #[serial]
    fn the_transcript_printed_on_exit_honours_no_color() {
        let dir = fresh_config_dir("dump-no-color");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text(MARKDOWN_REPLY.to_string())]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[("NO_COLOR", "1")]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"markdown please m9d\r").unwrap();
        wait_for_squeezed(&mut pty, "fenced_f7p", "the reply");
        pty.read_for(Duration::from_millis(800));
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);

        let between = raw_between_leave_and_report(&pty);
        let text = squeezed(&strip_ansi(&String::from_utf8_lossy(&between)));
        for word in [
            "Headingr6t",
            "boldb4k",
            "codec2m",
            "itemtwoi2q",
            "fenced_f7p",
        ] {
            assert!(
                text.contains(word),
                "{word:?} missing from the dump: {text:?}"
            );
        }
        let colored: Vec<Vec<u16>> = sgr_params(&between)
            .into_iter()
            .filter(|params| is_color_sgr(params))
            .collect();
        assert!(
            colored.is_empty(),
            "the dump carries colour under NO_COLOR: {colored:?}\n{:?}",
            String::from_utf8_lossy(&between)
        );
    }

    /// The non-blank text rows of a stretch of output, trimmed.
    fn text_rows(raw: &[u8]) -> Vec<String> {
        strip_ansi(&String::from_utf8_lossy(raw))
            .lines()
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect()
    }

    /// The reply rows the transcript printed on exit holds are the rows the
    /// plain REPL prints for the same reply, in the same order.
    #[test]
    #[serial]
    fn the_transcript_printed_on_exit_renders_the_reply_like_the_repl() {
        let repl_dir = fresh_config_dir("dump-vs-repl-repl");
        let _repl_guard = TempDirGuard(repl_dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text(MARKDOWN_REPLY.to_string())]);
        write_fake_llm_config(&repl_dir, &fake);
        let mut repl = spawn_repl_under(&repl_dir, &[], WAIT_WRAPPER, "/dev/tty");
        let from = wait_for_repl_prompt(&mut repl, "fake-model)>", 0);
        repl.send(b"markdown please m9d\r").unwrap();
        repl.wait_for_after("fenced_f7p", from, PTY_DEADLINE)
            .unwrap();
        let replied = find(repl.raw(), b"fenced_f7p", from)
            .expect("the reply was seen in the stripped transcript but not in the raw bytes");
        wait_for_repl_prompt(&mut repl, "fake-model)>", replied);
        let repl_rows = text_rows(&repl.raw()[from..]);
        repl.send(b"\x04").unwrap();
        let transcript = repl.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        let start = repl_rows
            .iter()
            .position(|row| row.contains("Heading r6t"))
            .unwrap_or_else(|| panic!("the REPL printed no heading: {repl_rows:#?}"));
        let end = repl_rows
            .iter()
            .position(|row| row.contains("fenced_f7p"))
            .unwrap();
        let repl_reply = &repl_rows[start..=end];

        let dir = fresh_config_dir("dump-vs-repl-tui");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text(MARKDOWN_REPLY.to_string())]);
        write_fake_llm_config(&dir, &fake);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"markdown please m9d\r").unwrap();
        wait_for_squeezed(&mut pty, "fenced_f7p", "the reply");
        pty.read_for(Duration::from_millis(800));
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        let dumped = text_rows(&raw_between_leave_and_report(&pty));
        let at = dumped
            .iter()
            .position(|row| row.contains("Heading r6t"))
            .unwrap_or_else(|| panic!("the dump holds no heading: {dumped:#?}"));
        let dump_reply: Vec<String> = dumped[at..]
            .iter()
            .take(repl_reply.len())
            .cloned()
            .collect();
        assert_eq!(dump_reply, repl_reply, "dump rows differ from the REPL's");
    }

    /// A job that shrugs off SIGTERM: it records its pid and loops forever.
    fn term_ignoring_tool(pid_file: &Path) -> String {
        format!(
            "#!/usr/bin/env bash\n\
         \n\
         # @describe Ignore SIGTERM and loop.\n\
         \n\
         # @env LLM_OUTPUT=/dev/stdout The output path\n\
         \n\
         main() {{\n\
         \x20   trap '' TERM\n\
         \x20   echo $$ > \"{}\"\n\
         \x20   for _ in $(seq 1 600); do sleep 0.5; done\n\
         }}\n",
            pid_file.display()
        )
    }

    /// `x` on a job that ignores SIGTERM starts a cancel that has to sit
    /// through the kill grace; the user leaves at once. The exit is bounded,
    /// exits 0, restores the terminal and prints the transcript.
    #[test]
    #[serial]
    fn leaving_during_a_slow_roster_cancel_is_bounded_and_clean() {
        let dir = fresh_config_dir("roster-slow-cancel-exit");
        let _guard = TempDirGuard(dir.clone());
        let job_pid = dir.join("job.pid");
        let blocker_pid = dir.join("blocker.pid");
        let _sleepers = Sleepers(vec![blocker_pid.clone()]);
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "job__start".to_string(),
                arguments: r#"{"tool":"fixture_stubborn","arguments":{}}"#.to_string(),
            },
            Reply::ToolCall {
                name: "fixture_block".to_string(),
                arguments: "{}".to_string(),
            },
        ]);
        write_fake_llm_config_with(
            &dir,
            &fake,
            "enabled_tools:\n  - fixture_stubborn\n  - fixture_block\n",
        );
        write_tool(&dir, "fixture_stubborn.sh", &term_ignoring_tool(&job_pid));
        write_tool(&dir, "fixture_block.sh", &sleeping_tool(&blocker_pid));
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"start the stubborn one s5w\r").unwrap();
        wait_for_file(&mut pty, &job_pid, "the job's pid file");
        wait_for_file(&mut pty, &blocker_pid, "the blocking tool's pid file");
        pty.read_for(Duration::from_millis(1200));
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let pid: i32 = fs::read_to_string(&job_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        struct KillOnDrop(i32);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                // SAFETY: kill takes a pid and a signal number and touches no memory.
                let _ = unsafe { libc::kill(self.0, libc::SIGKILL) };
            }
        }
        let _kill = KillOnDrop(pid);
        assert!(alive(pid), "the job died with the interrupted turn");

        pty.send(ALT_T).unwrap();
        let roster =
            repainted_frame_showing(&mut pty, "fixture_stubborn", "Alt+T did not list the job");
        assert!(
            roster_band(&roster).contains("running"),
            "the job is not running on the roster:\n{roster}"
        );
        pty.send(b"x").unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(300));
        let from = pty.raw().len();
        let started = std::time::Instant::now();
        pty.send(b"\x04").unwrap();
        let transcript = pty
            .wait_for("tty-probe-done", Duration::from_secs(40))
            .unwrap_or_else(|err| panic!("the UI did not leave within 40s: {err:?}"));
        let took = started.elapsed();
        let tail = String::from_utf8_lossy(&pty.raw()[from..]).to_string();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert!(!transcript.contains("panicked"), "{transcript}");
        let printed = strip_ansi(&tail);
        let left = printed.rfind("exit=").unwrap_or(printed.len());
        assert!(
            printed[..left].contains("s5w"),
            "the transcript was not printed on exit:\n{printed}"
        );
        assert!(
            took < Duration::from_secs(30),
            "leaving during a slow cancel took {took:?}"
        );
    }
}

mod suspend_and_signals {
    //! Suspend, signal, resize and keyboard-protocol cases for the full-screen
    //! UI. Each case runs the binary cold on a pseudo-terminal under the same
    //! bash wrappers `tests/common/tui_harness.rs` holds, so the exit code and
    //! the tty flags are reported after every exit path. Unix-only for the
    //! same reasons.
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{Chunk, FakeLlm, HOLD_LIMIT, Reply};
    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
    use serial_test::serial;

    fn write_fake_llm_config_saving(dir: &Path, fake: &FakeLlm, extra: &str, save_session: bool) {
        let vault_pass = dir.join("vault-pass");
        fs::write(&vault_pass, "test-password\n").unwrap();
        fs::write(
            dir.join("config.yaml"),
            format!(
                "model: fake:fake-model\n\
             function_calling_support: true\n\
             stream: true\n\
             save_session: {save_session}\n\
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
             memory: false\n\
             {extra}",
                vault_pass = vault_pass.display(),
                api_base = fake.api_base(),
            ),
        )
        .unwrap();
    }

    /// Marks `<marker>-started`, then waits (at most eight seconds) for
    /// `<marker>-release` before finishing.
    const SIGNALS_SLEEP_TOOL: &str = "#!/usr/bin/env bash\n\
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
    \x20   echo \"fixture slept r12s\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// The largest 1-based column any cursor-position sequence
    /// (`ESC [ row ; col H`) in `raw[from..to]` addressed: the width the UI
    /// believed the terminal had while painting.
    fn max_cup_col(raw: &[u8], from: usize, to: usize) -> u16 {
        let window = &raw[from.min(raw.len())..to.min(raw.len())];
        let mut widest = 0;
        let mut at = 0;
        while let Some(start) = find(window, b"\x1b[", at) {
            let params: Vec<u8> = window[start + 2..]
                .iter()
                .copied()
                .take_while(|b| b.is_ascii_digit() || *b == b';')
                .collect();
            if window.get(start + 2 + params.len()) == Some(&b'H')
                && let Some((_, col)) = std::str::from_utf8(&params)
                    .ok()
                    .and_then(|p| p.split_once(';'))
                && let Ok(col) = col.parse::<u16>()
            {
                widest = widest.max(col);
            }
            at = start + 1;
        }
        widest
    }

    /// The screen the UI comes back to shows its chrome again: the model and
    /// the session tokens plus whatever the composer is expected to show
    /// (`composer`: the placeholder hint when it is empty, the draft otherwise).
    fn assert_chrome_repainted_after(
        pty: &mut Pty,
        re_entered: usize,
        model: &str,
        composer: &str,
        what: &str,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let repainted = since(pty, re_entered);
            let missing: Vec<&str> = [composer, model, "no session"]
                .into_iter()
                .filter(|token| !repainted.contains(token))
                .collect();
            if missing.is_empty() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "after {what} the screen was not repainted; still missing {missing:?}. \
             Printable bytes since re-entry: {repainted:?}"
            );
            pty.read_for(Duration::from_millis(200));
        }
    }

    // ---------------------------------------------------------------------
    // Enter on a draft while the context is away (a tool is running). The
    // draft is queued and rides into the next tool round.
    // ---------------------------------------------------------------------

    /// Keys are handled while a work task sleeps; Enter while the context is
    /// away queues the line (the strip above the field shows `pending: 1`
    /// and the text) and never ends the turn or the process. Once the tool
    /// is released the driver hands the queued line to the model at the
    /// tool-round boundary, so the fake serves exactly two requests, the
    /// second carrying the draft, the strip is gone and the composer is
    /// empty.
    #[test]
    #[serial]
    fn enter_on_a_draft_while_a_tool_sleeps_queues_it_and_the_model_gets_it_next_round() {
        let dir = fresh_config_dir("enter-mid-tool");
        let _guard = TempDirGuard(dir.clone());
        let marker = dir.join("tool");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_sleep".to_string(),
                arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
            },
            Reply::Text("fixture done r12-a".to_string()),
        ]);
        write_fake_llm_config_with(&dir, &fake, "");
        write_tool(&dir, "fixture_sleep.sh", SIGNALS_SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the sleep tool\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        let typed_at = pty.raw().len();
        pty.send(b"draft-r12-d\r").unwrap();
        let after_enter = pty.read_for(Duration::from_millis(1000));
        let mid = since(&pty, typed_at);
        assert!(
            !mid.contains("exit=") && find(pty.raw(), LEAVE_ALT, typed_at).is_none(),
            "Enter on a draft while a tool ran ended the UI (a panic or a quit):\n{after_enter}"
        );
        let queued = squeezed(&mid);
        assert!(
            queued.contains("pending:1") && queued.contains("draft-r12-d"),
            "Enter on a draft while a tool ran did not queue it:\n{mid}"
        );
        assert_eq!(
            fake.served(),
            1,
            "Enter on a draft while a tool ran moved the turn on (the tool was not waited for):\n{mid}"
        );
        assert!(
            !dir.join("tool-release").exists(),
            "the fixture was released by something other than the test"
        );

        fs::write(dir.join("tool-release"), b"").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "fixture done r12-a",
            typed_at,
            "the reply after the tool",
        );
        pty.read_for(Duration::from_millis(800));
        assert_eq!(
            fake.served(),
            2,
            "the queued line did not ride into the tool round as a notification"
        );
        let bodies = fake.bodies();
        assert!(
            bodies[1].contains("draft-r12-d"),
            "the queued line is missing from the model's second request:\n{}",
            bodies[1]
        );

        // Ctrl+L repaints the whole screen, so what follows is the current
        // frame alone: no strip, the mid-turn cell in the transcript, and no
        // draft left in the field.
        let repaint_at = pty.raw().len();
        pty.send(b"\x0c").unwrap();
        pty.read_for(Duration::from_millis(600));
        let repainted = since(&pty, repaint_at);
        assert!(
            !repainted.contains("pending:"),
            "the queue strip outlived the turn:\n{repainted}"
        );
        let frame = squeezed(&repainted);
        assert!(
            frame.contains(">draft-r12-d(sentmid-turn)"),
            "the transcript has no mid-turn cell for the line the model read:\n{repainted}"
        );
        assert_eq!(
            frame.matches("draft-r12-d").count(),
            1,
            "the queued line is still in the composer after it went to the model:\n{repainted}"
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
    }

    // ---------------------------------------------------------------------
    // Esc on the "Save session?" modal raised by `.exit session`: a
    // cancelled confirm must leave the UI in a known state within bounded
    // time - either live (keys still handled) or exited with the tty cooked.
    // It must never hang with the modal gone and the composer dead, and it
    // must never panic.
    // ---------------------------------------------------------------------

    /// `.session`, a dirtying `.set`, `.exit session`, then Esc on the modal.
    /// Within the deadline the UI either takes `.info` again (cancelled exit)
    /// or has left the alternate screen with exit 0 and a cooked tty.
    #[test]
    #[serial]
    fn esc_on_the_save_session_modal_neither_hangs_nor_panics() {
        let dir = fresh_config_dir("esc-save-modal");
        let _guard = TempDirGuard(dir.clone());
        // No `save_session` line: the exit path asks about a dirty session.
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
        pty.read_for(HOTKEY_GAP);
        let esc_at = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(1500));

        let after_esc = since(&pty, esc_at);
        assert!(
            !after_esc.contains("panicked"),
            "Esc on the save modal panicked:\n{after_esc}"
        );
        if after_esc.contains("exit=") {
            // The cancelled confirm ended the session: that exit must be clean.
            let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
            assert_eq!(exit_code(&transcript), 0, "{transcript}");
            assert_tty_restored(&transcript);
            assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
            return;
        }
        // The UI stayed up: the modal is gone and keys are handled again.
        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("after Esc on the save modal the UI stayed up but stopped handling keys");
        pty.read_for(Duration::from_millis(500));
        // Quit. If the dirty session is still seated the quit path asks
        // again and `n` declines; if the cancelled exit already closed it,
        // Ctrl+D leaves at once. Both must end clean.
        let quit_at = pty.raw().len();
        pty.send(b"\x04").unwrap();
        pty.read_for(Duration::from_millis(1500));
        if since(&pty, quit_at).contains("Save session?") {
            pty.read_for(HOTKEY_GAP);
            pty.send(b"n").unwrap();
        }
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
    }

    // ---------------------------------------------------------------------
    // Ctrl-Z suspend and resume, here in the middle of a turn.
    // ---------------------------------------------------------------------

    /// Ctrl-Z while a reply is still streaming hands the terminal back cooked
    /// and out of the alternate screen; `fg` re-enters and repaints with the
    /// partial reply still there; the stream then finishes into the same
    /// transcript and the usual clean exit follows.
    #[test]
    #[serial]
    fn ctrl_z_mid_stream_hands_the_terminal_back_and_the_reply_finishes_after_fg() {
        let dir = fresh_config_dir("ctrl-z-mid-stream");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("head before suspend r12-h".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text(" tail after resume r12-t".to_string()),
        ])]);
        write_fake_llm_config_with(&dir, &fake, "");
        let mut pty = spawn_tui_under_wrapper(&dir, &[], JOB_CONTROL_WRAPPER);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"stream and hold\r").unwrap();
        let started = Instant::now();
        wait_for_squeezed_after(&mut pty, "head before suspend r12-h", 0, "the first delta");
        assert!(
            started.elapsed() < HOLD_LIMIT,
            "the first delta appeared only after the fake gave up holding the stream"
        );
        pty.read_for(Duration::from_millis(300));
        let before_suspend = pty.raw().len();
        let stopped_marker = suspend_and_check_handback(&mut pty, before_suspend);

        let re_entered = wait_for_re_entry(&mut pty, stopped_marker);
        // The partial reply belongs to the transcript the UI comes back with.
        wait_for_squeezed_after(
            &mut pty,
            "head before suspend r12-h",
            re_entered,
            "the partial reply after fg",
        );
        fs::write(&release, b"").unwrap();
        let tail = wait_for_squeezed_after(
            &mut pty,
            "tail after resume r12-t",
            re_entered,
            "the rest of the stream after fg",
        );
        assert!(
            !tail.contains("exit="),
            "the UI left before the stream finished:\n{tail}"
        );
        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("keys were not handled after the mid-stream resume");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(
            count(&pty.raw()[before_suspend..], ENTER_ALT),
            1,
            "exactly one alt-screen re-entry is expected after the suspend:\n{transcript}"
        );
        assert_eq!(fake.served(), 1, "{transcript}");
    }

    // ---------------------------------------------------------------------
    // The window changes size while the UI does not
    // own the terminal (editor shell-out, stopped job). When it comes back
    // it paints at the size the terminal has now, not the size it left at.
    // ---------------------------------------------------------------------

    /// Resized from 100 to 60 columns while `$EDITOR` holds the terminal; on
    /// return the repaint addresses no column past 60 and the edited draft
    /// is in the composer.
    #[test]
    #[serial]
    fn a_resize_during_the_editor_shell_out_repaints_at_the_new_width_on_return() {
        use std::os::unix::fs::PermissionsExt;

        let dir = fresh_config_dir("editor-resize");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let release = dir.join("editor-release");
        let editor = dir.join("waiting-editor.sh");
        fs::write(
        &editor,
        format!(
            "#!/bin/sh\nwhile [ ! -e '{}' ]; do sleep 0.05; done\nprintf 'edited-r12-e' > \"$1\"\n",
            release.display()
        ),
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
        pty.read_for(Duration::from_millis(300));
        pty.resize(40, 60).unwrap();
        pty.read_for(Duration::from_millis(300));
        fs::write(&release, b"").unwrap();

        let re_entered = wait_for_re_entry(&mut pty, left);
        assert_chrome_repainted_after(
            &mut pty,
            re_entered,
            "dryrun:dry-model",
            "edited-r12-e",
            "the editor shell-out",
        );
        pty.wait_for_after("edited-r12-e", re_entered, PTY_DEADLINE)
            .expect("the edited draft did not come back into the composer");
        pty.read_for(Duration::from_millis(500));
        let quit_at = pty.raw().len();
        let widest = max_cup_col(pty.raw(), re_entered, quit_at);
        assert!(
            widest <= 60,
            "after the editor returned on a 60-column terminal the UI still painted at column \
         {widest}: the resize during the shell-out was not picked up:\n{}",
            since(&pty, re_entered)
        );

        // The edited draft must be cleared before Ctrl+D is the quit chord.
        pty.send(b"\x03").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// Resized from 100 to 60 columns while the job is stopped under Ctrl-Z;
    /// after `fg` the repaint addresses no column past 60 and keys work.
    #[test]
    #[serial]
    fn a_resize_while_stopped_under_ctrl_z_is_painted_at_the_new_width_after_fg() {
        let dir = fresh_config_dir("ctrl-z-resize");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui_under_wrapper(&dir, &[], JOB_CONTROL_WRAPPER);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let before_suspend = pty.raw().len();
        let stopped_marker = suspend_and_check_handback(&mut pty, before_suspend);
        pty.resize(40, 60).unwrap();

        let re_entered = wait_for_re_entry(&mut pty, stopped_marker);
        assert_chrome_repainted_after(
            &mut pty,
            re_entered,
            "dryrun:dry-model",
            "Type a prompt",
            "fg",
        );
        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("keys were not handled after resume");
        pty.read_for(Duration::from_millis(500));
        let quit_at = pty.raw().len();
        let widest = max_cup_col(pty.raw(), re_entered, quit_at);
        assert!(
            widest <= 60,
            "after fg on a 60-column terminal the UI still painted at column {widest}: the resize \
         while stopped was not picked up:\n{}",
            since(&pty, re_entered)
        );

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------
    // SIGTERM during a command turn whose output is captured (the real fds
    // redirected): the leave path still reaches the terminal, the loader
    // under it is killed, and nothing of the command reaches the model.
    // ---------------------------------------------------------------------

    /// `.file <doc>` through a loader that sleeps, then SIGTERM: the process
    /// leaves the alternate screen exactly once, the tty comes back cooked,
    /// the exit is orderly and well inside the cancel wait, the loader is
    /// gone, and the model was never called.
    #[test]
    #[serial]
    fn sigterm_during_a_slow_file_loader_kills_it_and_leaves_the_tty_cooked() {
        let dir = fresh_config_dir("sigterm-loader");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![Reply::Text("never reached r12-n".to_string())]);
        let script = dir.join("slow-file.sh");
        fs::write(
            &script,
            "#!/usr/bin/env bash\necho $$ > \"$1.pid\"\nsleep 30\ncp \"$1\" \"$2\"\n",
        )
        .unwrap();
        write_fake_llm_config_with(
            &dir,
            &fake,
            &format!(
                "document_loaders:\n\x20 slow: 'bash {} $1 $2'\n",
                script.display()
            ),
        );
        let document = dir.join("doc.slow");
        fs::write(&document, "slow document body\n").unwrap();
        let pid_file = dir.join("doc.slow.pid");
        let mut pty = spawn_tui(&dir, &[]);

        let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let pid = child_pid(&transcript);
        pty.send(format!(".file {} -- summarize\r", document.display()).as_bytes())
            .unwrap();
        wait_for_file(&mut pty, &pid_file, "the slow loader's pid file");
        let loader = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert!(alive(loader), "the loader exited before the signal");
        pty.read_for(Duration::from_millis(500));
        let signalled_at = pty.raw().len();
        let started = Instant::now();
        // SAFETY: kill takes a pid and a signal number and touches no memory.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let took = started.elapsed();

        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            1,
            "SIGTERM during the loader left the alternate screen {} times:\n{transcript}",
            count(pty.raw(), LEAVE_ALT)
        );
        assert!(
            find(pty.raw(), LEAVE_ALT, signalled_at.saturating_sub(16)).is_some(),
            "the alternate screen was left before the signal, not by it:\n{transcript}"
        );
        assert!(
            took < Duration::from_secs(8),
            "the exit waited the loader out instead of aborting it: {took:?}"
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive(loader) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !alive(loader),
            "the loader (pid {loader}) outlived the UI's exit:\n{transcript}"
        );
        assert_eq!(
            fake.served(),
            0,
            "the interrupted document reached the model:\n{transcript}"
        );
        assert!(
            !squeezed(&transcript).contains("neverreachedr12-n"),
            "the model's reply landed after the signal:\n{transcript}"
        );
    }

    /// A line queued while a tool sleeps, then SIGTERM: the exit takes the
    /// line off the turn's queue before the turn is told to stop, so the
    /// tool result the interrupt closes cannot hand it to the model and the
    /// session the exit autosaves (one is open, under `save_session: true`)
    /// does not carry it either. The process still leaves cleanly.
    #[test]
    #[serial]
    fn sigterm_with_a_queued_line_does_not_hand_it_to_the_model() {
        let dir = fresh_config_dir("sigterm-queued");
        let _guard = TempDirGuard(dir.clone());
        let marker = dir.join("tool");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_sleep".to_string(),
                arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
            },
            Reply::Text("never reached r12-q".to_string()),
        ]);
        write_fake_llm_config_saving(&dir, &fake, "", true);
        write_tool(&dir, "fixture_sleep.sh", SIGNALS_SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let pid = child_pid(&transcript);
        let session_at = pty.raw().len();
        pty.send(b".session sigq-r12\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "sigq-r12 . agents",
            session_at,
            "the footer's session token",
        );
        pty.read_for(Duration::from_millis(400));
        pty.send(b"call the sleep tool\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        let typed_at = pty.raw().len();
        pty.send(b"queued-r12-q\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "pending: 1",
            typed_at,
            "the queue strip after Enter on a draft",
        );
        // A full repaint: the incremental frame can carry a torn cell diff
        // of the strip's text, so the queued line is read from a fresh one.
        let repaint_at = pty.raw().len();
        pty.resize(24, 81).unwrap();
        pty.read_for(Duration::from_millis(600));
        let mid = squeezed(&since(&pty, repaint_at));
        assert!(
            mid.contains("queued-r12-q"),
            "Enter on a draft while a tool ran did not queue it:\n{mid}"
        );
        let started = Instant::now();
        // SAFETY: kill takes a pid and a signal number and touches no memory.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        let took = started.elapsed();

        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
        assert!(
            took < Duration::from_secs(8),
            "the exit waited the tool out instead of aborting it: {took:?}"
        );
        assert!(
            fake.bodies()
                .iter()
                .all(|body| !body.contains("queued-r12-q")),
            "the queued line reached the model through the signal exit:\n{:?}",
            fake.bodies()
        );
        let saved: Vec<(PathBuf, String)> = fs::read_dir(dir.join("sessions"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .map(|path| {
                let text = fs::read_to_string(&path).unwrap_or_default();
                (path, text)
            })
            .collect();
        assert!(
            !saved.is_empty(),
            "the signal exit saved no session under {}:\n{transcript}",
            dir.join("sessions").display()
        );
        for (path, saved) in saved {
            assert!(
                !saved.contains("queued-r12-q"),
                "the queued line was saved with the session {}:\n{saved}",
                path.display()
            );
        }
    }

    // ---------------------------------------------------------------------
    // With the keyboard-enhancement flags pushed, a
    // terminal encodes control chords as CSI u. They must drive the composer
    // exactly like their legacy bytes do.
    // ---------------------------------------------------------------------

    /// Under a terminal that answered the probe (flags pushed), Ctrl+L,
    /// Ctrl+C, Esc and Ctrl+D arrive as `CSI <codepoint>;5u` / `CSI 27u`.
    /// Ctrl+C clears the draft without quitting, Esc on an empty composer
    /// does not quit, Ctrl+D on the empty composer quits with the flags
    /// popped before the alternate screen is left.
    #[test]
    #[serial]
    fn kitty_encoded_control_chords_drive_the_composer_like_their_legacy_bytes() {
        let dir = fresh_config_dir("kitty-chords");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);
        pty.leave_keyboard_probe_unanswered();

        let probe = pty
            .wait_for_raw(b"\x1b[?u", 0, PTY_DEADLINE)
            .expect("the keyboard-enhancement support probe (CSI ? u) was never sent");
        pty.wait_for_raw(b"\x1b[c", probe, PTY_DEADLINE)
            .expect("the probe never asked for primary device attributes");
        pty.send(b"\x1b[?0u\x1b[?62;22c").unwrap();
        pty.wait_for("no session", PTY_DEADLINE)
            .expect("the UI never came up after the terminal answered the keyboard probe");
        let push = find_keyboard_push(pty.raw(), 0)
            .expect("the terminal answered the probe but the flags were never pushed");

        // A draft, then Ctrl+C as CSI 99;5u: the draft goes, the UI stays.
        pty.send(b"draft-k12").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "draft-k12",
            push,
            "plain text under the kitty flags",
        );
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x1b[99;5u").unwrap();
        pty.read_for(Duration::from_millis(500));
        // Ctrl+L as CSI 108;5u repaints the whole screen: what is painted from
        // here on is the composer's real contents.
        let repaint_at = pty.raw().len();
        pty.send(b"\x1b[108;5u").unwrap();
        pty.wait_for_after("no session", repaint_at, PTY_DEADLINE)
            .expect("Ctrl+L encoded as CSI 108;5u did not repaint the screen");
        pty.read_for(Duration::from_millis(400));
        let repainted = since(&pty, repaint_at);
        assert!(
            !repainted.contains("exit="),
            "Ctrl+C encoded as CSI 99;5u quit the UI instead of clearing the draft:\n{repainted}"
        );
        assert!(
            !squeezed(&repainted).contains("draft-k12"),
            "Ctrl+C encoded as CSI 99;5u did not clear the draft:\n{repainted}"
        );

        // Esc as CSI 27u on the empty composer: no quit.
        let esc_at = pty.raw().len();
        pty.send(b"\x1b[27u").unwrap();
        pty.read_for(Duration::from_millis(1000));
        let after_esc = since(&pty, esc_at);
        assert!(
            !after_esc.contains("exit=") && find(pty.raw(), LEAVE_ALT, esc_at).is_none(),
            "Esc encoded as CSI 27u on an idle empty composer quit the UI:\n{after_esc}"
        );

        // A prompt still goes through with the chords in between.
        pty.send(b"**hello** under kitty r12-k\r").unwrap();
        pty.wait_for_after("Working", esc_at, PTY_DEADLINE)
            .expect("Enter after the kitty chords started no turn");
        // Wait the turn out: Enter while the context is away keeps the draft
        // in the composer instead of submitting it, so the command below is
        // only a command once the reply has landed. The typed prompt keeps its
        // asterisks; the asterisk-free form is the rendered assistant cell.
        wait_for_squeezed_after(
            &mut pty,
            "hello under kitty r12-k",
            esc_at,
            "the rendered dry-run reply to the prompt typed after the kitty chords",
        );
        pty.read_for(Duration::from_millis(800));
        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("the turn after the kitty chords never ended, or keys stopped being handled");
        let before_quit = since(&pty, esc_at);
        assert!(
            squeezed(&before_quit).contains("hellounderkittyr12-k"),
            "the prompt typed after the kitty chords never reached the transcript:\n{before_quit}"
        );
        pty.read_for(Duration::from_millis(500));

        // Ctrl+D as CSI 100;5u on the empty composer quits.
        let quit_at = pty.raw().len();
        pty.send(b"\x1b[100;5u").unwrap();
        let transcript = match pty.wait_for("tty-probe-done", Duration::from_secs(8)) {
            Ok(transcript) => transcript,
            Err(err) => {
                pty.send(b"\x04").unwrap();
                let after = pty
                    .wait_for("tty-probe-done", PTY_DEADLINE)
                    .unwrap_or_else(|_| strip_ansi(&String::from_utf8_lossy(pty.raw())));
                panic!(
                    "Ctrl+D encoded as CSI 100;5u did not quit ({err}); the legacy byte then did:\n{after}"
                );
            }
        };
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        let enter_alt =
            find(pty.raw(), ENTER_ALT, 0).expect("the alternate screen was never entered");
        assert!(enter_alt < push, "flags pushed before alt-screen entry");
        let leave_alt = rfind(pty.raw(), LEAVE_ALT).expect("the alternate screen was never left");
        let pop = rfind_keyboard_pop(pty.raw()).expect("flags pushed but never popped");
        // `quit_at` is the stream length when the chord was sent; the pop may
        // be the very next byte written.
        assert!(
            push < pop && pop < leave_alt && quit_at <= pop,
            "flags must be popped on the way out, before leaving the alternate screen \
         (push={push}, pop={pop}, leave={leave_alt}, quit sent at {quit_at})"
        );
    }

    // ---------------------------------------------------------------------
    // Bracketed paste: a paste larger than one tty read (a pasted log)
    // lands whole and unsubmitted; Enter then sends it as one prompt.
    // ---------------------------------------------------------------------

    /// Fifty lines of about seventy columns in one bracketed paste: no turn
    /// starts on the paste, the last pasted line is visible in the composer,
    /// Enter sends it all as one prompt and the dry-run echo of the last line
    /// comes back in the transcript.
    #[test]
    #[serial]
    fn a_large_multi_line_paste_lands_whole_and_enter_sends_it_as_one_prompt() {
        let dir = fresh_config_dir("large-paste");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.wait_for_raw(PASTE_ON, 0, PTY_DEADLINE)
            .expect("bracketed paste was never enabled");
        let mut paste = Vec::new();
        paste.extend_from_slice(b"\x1b[200~");
        for n in 1..=49 {
            paste.extend_from_slice(
                format!("line {n:02} of the pasted log r12p {}\n", "x".repeat(40)).as_bytes(),
            );
        }
        paste.extend_from_slice(b"last pasted line r12-last");
        paste.extend_from_slice(b"\x1b[201~");
        assert!(paste.len() > 3000, "the paste should exceed one tty read");
        let pasted_at = pty.raw().len();
        pty.send(&paste).unwrap();
        pty.wait_for_after("r12-last", pasted_at, PTY_DEADLINE)
            .expect("the last pasted line never reached the composer");
        let settled = pty.read_for(Duration::from_millis(1200));
        let after_paste = since(&pty, pasted_at);
        assert!(
            !after_paste.contains("Working"),
            "the paste started a turn by itself:\n{settled}"
        );
        assert!(
            !after_paste.contains("exit=") && find(pty.raw(), LEAVE_ALT, pasted_at).is_none(),
            "the large paste ended the UI:\n{after_paste}"
        );

        let submitted_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        pty.wait_for_after("Working", submitted_at, PTY_DEADLINE)
            .expect("Enter after the paste started no turn");
        wait_for_squeezed_after(
            &mut pty,
            "last pasted line r12-last",
            submitted_at,
            "the pasted prompt in the transcript",
        );
        // The turn ends and the UI still takes keys: a command after it
        // renders its cell.
        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        let after = pty
            .wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("the turn on the pasted prompt never ended, or keys stopped being handled");
        assert!(
            !after.contains("exit="),
            "the UI left while echoing the pasted prompt:\n{after}"
        );
        pty.read_for(Duration::from_millis(500));

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
    }
}

mod cold_start {
    //! Cold-start cases for the full-screen UI. Each case boots the real
    //! binary cold (fresh config and cache dirs, no model) and exercises a
    //! usage pattern the lifecycle suite does not pin: malformed input bytes,
    //! wide graphemes, over-width drafts, repeated commands, resize bursts and
    //! a cold start on a tiny terminal. Unix-only for the same reasons as the
    //! lifecycle suite; cases run one at a time because each takes a
    //! controlling tty.
    use std::path::Path;
    use std::time::Duration;

    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
    use serial_test::serial;

    fn spawn_tui_bare(dir: &Path) -> Pty {
        spawn_tui(dir, &[])
    }

    /// Quits from an idle, empty composer and checks the common exit
    /// promises: exit 0, cooked tty, the alternate screen left exactly once.
    fn quit_and_check_left_once(mut pty: Pty, what: &str) -> String {
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
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui_bare(&dir);

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
        quit_and_check_left_once(pty, "after malformed input bytes");
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
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui_bare(&dir);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        let before = pty.raw().len();
        // The asterisks are stripped by rendering, so the plain form can only
        // come from the assistant cell, not from the typed draft or user cell.
        pty.send(
            "**\u{1F98A} fox** \u{65E5}\u{672C} \u{2714}\u{FE0F} h\u{E9}llo wide-marker\r"
                .as_bytes(),
        )
        .unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "\u{1F98A} fox \u{65E5}\u{672C}",
            before,
            "a wide-grapheme prompt",
        );
        let since =
            wait_for_squeezed_after(&mut pty, "wide-marker", before, "a wide-grapheme prompt");
        assert!(
            since.contains("h\u{E9}llo"),
            "the composed character did not survive the round trip:\n{since}"
        );

        pty.read_for(HOTKEY_GAP);
        quit_and_check_left_once(pty, "after a wide-grapheme prompt");
    }

    #[test]
    #[serial]
    fn a_draft_longer_than_the_row_submits_and_its_reply_renders() {
        let dir = fresh_config_dir("long-draft");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui_bare(&dir);

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
        quit_and_check_left_once(pty, "after an over-width draft");
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
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui_bare(&dir);

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
        quit_and_check_left_once(pty, "after two .info turns");
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
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui_bare(&dir);

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
        quit_and_check_left_once(pty, "after a resize burst");
    }

    #[test]
    #[serial]
    fn a_cold_start_on_a_tiny_terminal_comes_up_and_leaves_clean() {
        let dir = fresh_config_dir("tiny-start");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui_bare(&dir);
        // Shrink before the first frame: startup waits on the keyboard
        // probe, which the harness answers only once the test reads, so the
        // resize lands before anything is drawn.
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
        quit_and_check_left_once(pty, "after a tiny cold start");
    }
}

mod interrupt_and_paste {
    //! Interrupt, quit, hotkey-gate and paste cases for the full-screen UI.
    //! Each case runs the binary cold on a pseudo-terminal under the same bash
    //! wrapper `tests/common/tui_harness.rs` holds, so the exit code and the
    //! tty flags are reported after every exit path. Unix-only for the same
    //! reasons.
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::common::fake_llm::{Chunk, FakeLlm, HOLD_LIMIT, Reply};
    use super::common::pty::{Pty, strip_ansi};
    use super::common::tui_harness::*;
    use serial_test::serial;

    /// The fake-LLM config; `save_session` is written only when `prompt_free`
    /// is set, so a dirty session asks "Save session?" on the way out otherwise.
    fn write_fake_llm_config_prompt_free(dir: &Path, fake: &FakeLlm, prompt_free: bool) {
        let vault_pass = dir.join("vault-pass");
        fs::write(&vault_pass, "test-password\n").unwrap();
        let save = if prompt_free {
            "save_session: false\n"
        } else {
            ""
        };
        fs::write(
            dir.join("config.yaml"),
            format!(
                "model: fake:fake-model\n\
             function_calling_support: true\n\
             stream: true\n\
             {save}\
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
             memory: false\n",
                vault_pass = vault_pass.display(),
                api_base = fake.api_base(),
            ),
        )
        .unwrap();
    }

    /// Marks `<out>.asked` the moment it asks, records the answer at `<out>`.
    const ASKING_CONFIRM_TOOL: &str = "#!/usr/bin/env bash\n\
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
    \x20   : > \"$out.asked\"\n\
    \x20   v=\"$(confirm \"Really do it? k4q\")\"\n\
    \x20   printf '%s' \"${v:-cancelled}\" > \"$out\"\n\
    \x20   echo \"fixture confirm: ${v:-cancelled} k4r\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// Marks `<marker>-started`, then waits (at most thirty seconds; longer
    /// than its twins so the 7 s bounded-quit case cannot overlap the tool's
    /// own give-up) for `<marker>-release` before finishing.
    const LONG_SLEEP_TOOL: &str = "#!/usr/bin/env bash\n\
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
    \x20   for _ in $(seq 1 600); do\n\
    \x20       [ -e \"${marker}-release\" ] && break\n\
    \x20       sleep 0.05\n\
    \x20   done\n\
    \x20   echo \"fixture slept k4s\" >> \"$LLM_OUTPUT\"\n\
    }\n";

    /// Streams a first delta, holds the rest behind `release`, then streams a
    /// tail; `.session` is opened first so an interrupt keeps the partial
    /// text (as the REPL's Ctrl+C does). Returns the raw offset at which Esc
    /// was sent, with the interrupted cell already on screen.
    fn interrupt_a_held_stream(pty: &mut Pty, head: &str) -> usize {
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b".session\r").unwrap();
        pty.wait_for("temp", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(500));
        pty.send(b"stream and hold\r").unwrap();
        let started = Instant::now();
        wait_for_squeezed_after(pty, head, 0, "the first delta");
        assert!(
            started.elapsed() < HOLD_LIMIT,
            "the first delta appeared only after the fake gave up holding the stream"
        );
        pty.wait_for("Esc to interrupt", PTY_DEADLINE)
            .expect("the status row never offered Esc to interrupt");
        let pressed_at = pty.raw().len();
        pty.send(b"\x1b").unwrap();
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            pty.read_for(Duration::from_millis(200));
            let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
            if since.to_lowercase().contains("interrupted") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "no cell said the turn was interrupted after Esc:\n{since}"
            );
        }
        pressed_at
    }

    // ---------------------------------------------------------------------
    // An idle quit after an Esc-interrupted turn starts the exit with a
    // clean abort signal (`agent.completed` fires; the save question is
    // still asked). Both are pinned in-process; here on a real tty.
    // ---------------------------------------------------------------------

    /// Esc interrupts a held stream; the user then quits from the idle, empty
    /// composer. Teardown mirrors `Repl::run`, so the agent's `agent.completed`
    /// hook must still fire exactly once: the earlier interrupt is the turn's,
    /// not the quit's.
    #[test]
    #[serial]
    fn an_idle_quit_after_an_esc_interrupted_turn_still_fires_agent_completed() {
        let dir = fresh_config_dir("idle-quit-after-esc-hook");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("never-released");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("partial before esc k4h".to_string()),
            Chunk::WaitFor(release),
            Chunk::Text(" tail never shown k4t".to_string()),
        ])]);
        write_fake_llm_config_prompt_free(&dir, &fake, true);
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
        let wrapper = WAIT_WRAPPER.replace("{args}", "{args} --agent probe-agent");
        let mut pty = spawn_tui_under_wrapper(&dir, &[], &wrapper);

        let pressed_at = interrupt_a_held_stream(&mut pty, "partial before esc k4h");
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[pressed_at..]));
        assert!(!since.contains("exit="), "Esc quit the UI:\n{since}");
        assert!(
            !marker.exists(),
            "agent.completed fired on the interrupt, before any quit"
        );

        // Idle again: the UI takes a command, then the quit.
        let info_at = pty.raw().len();
        pty.send(b".info\r").unwrap();
        pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
            .expect("keys were not handled after the interrupt");
        pty.read_for(Duration::from_millis(500));
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");

        let completed = |marker: &Path| {
            fs::read_to_string(marker)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == "COMPLETED")
                .count()
        };
        let deadline = Instant::now() + PTY_DEADLINE;
        let fired = loop {
            let fired = completed(&marker);
            if fired > 0 || Instant::now() >= deadline {
                break fired;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(
            fired, 1,
            "agent.completed fired {fired} times after an idle Ctrl+D that followed an Esc-interrupted \
         turn (expected once: the quit must not inherit the turn's interrupt):\n{transcript}"
        );
    }

    /// The same idle quit on a dirty session with `save_session` unset: the
    /// "Save session?" question is still asked (an exit that thought itself
    /// interrupted would skip or fail it), `y` names and writes the file.
    #[test]
    #[serial]
    fn an_idle_quit_after_an_esc_interrupted_turn_still_asks_to_save_the_session() {
        let dir = fresh_config_dir("idle-quit-after-esc-save");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("never-released");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("partial before esc k4w".to_string()),
            Chunk::WaitFor(release),
            Chunk::Text(" tail never shown k4u".to_string()),
        ])]);
        write_fake_llm_config_prompt_free(&dir, &fake, false);
        let mut pty = spawn_tui(&dir, &[]);

        interrupt_a_held_stream(&mut pty, "partial before esc k4w");
        // Flip a session setting too, so the dirty bit does not hinge on
        // whether the partial text was recorded.
        pty.send(b".set skills_enabled false\r").unwrap();
        pty.read_for(Duration::from_millis(800));

        pty.send(b"\x04").unwrap();
        let asked = pty.wait_for("Save session?", PTY_DEADLINE);
        let asked = match asked {
            Ok(text) => text,
            Err(err) => {
                let transcript = pty
                    .wait_for("tty-probe-done", Duration::from_secs(5))
                    .unwrap_or_else(|_| strip_ansi(&String::from_utf8_lossy(pty.raw())));
                panic!(
                    "Ctrl+D after an Esc-interrupted turn did not ask to save the dirty session \
                 ({err}); what followed:\n{transcript}"
                );
            }
        };
        assert!(
            !asked.contains("exit="),
            "the UI exited before the save question could be answered:\n{asked}"
        );
        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        pty.wait_for("ession name", PTY_DEADLINE)
            .expect("`y` on the save modal did not ask for a session name");
        // The name field gates Enter until the modal has settled after its
        // appearance; typing itself does not postpone it.
        pty.read_for(HOTKEY_GAP);
        pty.send(b"esc-session-k4e\r").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        let saved = walk(&dir).into_iter().find(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("esc-session-k4e"))
        });
        assert!(
            saved.is_some(),
            "no esc-session-k4e session file was written under {dir:?}:\n{transcript}"
        );
    }

    // ---------------------------------------------------------------------
    // The hotkey idle gate counts from the moment a modal appears, not from
    // the user's last key. Pinned with the manual clock in-process; here on
    // a real tty with the real clock.
    // ---------------------------------------------------------------------

    /// A user idle for well over a second who taps `y` the instant the confirm
    /// modal is painted has not read it: the tap is dropped (the tool gets no
    /// answer, the turn does not move on), and the same `y` answers once the
    /// modal has been up for the idle gap.
    #[test]
    #[serial]
    fn a_hotkey_typed_as_the_confirm_modal_appears_is_dropped_until_it_has_been_up_a_second() {
        let dir = fresh_config_dir("modal-appearance-gate");
        let _guard = TempDirGuard(dir.clone());
        let out = dir.join("confirm-answer");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_confirm_rec".to_string(),
                arguments: format!("{{\"out\": \"{}\"}}", out.display()),
            },
            Reply::Text("fixture gated done k4g".to_string()),
        ]);
        write_fake_llm_config_prompt_free(&dir, &fake, true);
        write_tool(&dir, "fixture_confirm_rec.sh", ASKING_CONFIRM_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the confirm tool\r").unwrap();
        // The Enter above was the last key; by the time the model round trip,
        // the tool start and the request land, the user has been idle well
        // past the gap, which is exactly the case the gate must still cover.
        let seen = pty
            .wait_for("k4q", PTY_DEADLINE)
            .expect("the tool's confirm question never reached the screen");
        let seen_at = Instant::now();
        pty.send(b"y").unwrap();
        let send_latency = seen_at.elapsed();
        assert!(
            !seen.contains("exit="),
            "the UI left before the modal:\n{seen}"
        );

        // 600 ms is long enough for an accepted answer to reach the tool and
        // well inside the one-second gate.
        let after_first = pty.read_for(Duration::from_millis(600));
        assert!(
            !out.exists(),
            "`y` typed {send_latency:?} after the modal's first bytes arrived answered it (the tool \
         recorded {:?}); the gate must count from the modal's appearance:\n{after_first}",
            fs::read_to_string(&out).unwrap_or_default()
        );
        assert_eq!(
            fake.served(),
            1,
            "the turn moved on after the instant `y`:\n{after_first}"
        );

        pty.read_for(HOTKEY_GAP);
        pty.send(b"y").unwrap();
        wait_for_file(&mut pty, &out, "the tool's recorded answer to the second y");
        let transcript = wait_for_squeezed_after(
            &mut pty,
            "fixture gated done k4g",
            0,
            "the reply after the answered confirm",
        );
        assert_eq!(
            fs::read_to_string(&out).unwrap().trim(),
            "1",
            "`y` once the modal had been up for the gap did not reach the tool as a yes:\n{transcript}"
        );
        assert_eq!(fake.served(), 2, "{transcript}");

        // The dropped `y` went nowhere: Enter now starts no turn.
        pty.read_for(Duration::from_millis(500));
        let enter_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1000));
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[enter_at..]));
        assert!(
            !since.contains("Working"),
            "the dropped hotkey landed in the composer and Enter submitted it:\n{since}"
        );
        assert_eq!(fake.served(), 2, "{since}");

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------
    // `exit()` is idempotent: a second SIGTERM during teardown changes nothing.
    // ---------------------------------------------------------------------

    /// A second SIGTERM while the first one's teardown is running changes
    /// nothing: the alternate screen is entered once and left once, the exit
    /// is still the orderly 0 and the tty comes back cooked.
    #[test]
    #[serial]
    fn a_second_sigterm_during_teardown_changes_nothing() {
        let dir = fresh_config_dir("double-sigterm");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);

        let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let pid = child_pid(&transcript);
        pty.read_for(Duration::from_millis(300));
        // SAFETY: kill takes a pid and a signal number and touches no memory.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        std::thread::sleep(Duration::from_millis(30));
        // The process may already be gone; a second delivery that fails with
        // ESRCH is fine, one that lands must be harmless.
        // SAFETY: as above.
        let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(
            count(pty.raw(), ENTER_ALT),
            1,
            "the alternate screen was entered more than once:\n{transcript}"
        );
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            1,
            "two SIGTERMs left the alternate screen {} times:\n{transcript}",
            count(pty.raw(), LEAVE_ALT)
        );
    }

    // ---------------------------------------------------------------------
    // Ctrl+D from an empty composer while a tool is still running.
    // ---------------------------------------------------------------------

    /// Ctrl+D on an empty composer while a tool sleeps is a quit during a
    /// turn: the turn is interrupted (the tool is not waited out) and the
    /// process leaves in bounded time with the tty cooked and the alternate
    /// screen left exactly once. The REPL's teardown is what the UI mirrors,
    /// and an aborted tool is killed rather than awaited.
    #[test]
    #[serial]
    fn ctrl_d_while_a_tool_sleeps_quits_in_bounded_time_and_leaves_the_tty_cooked() {
        let dir = fresh_config_dir("ctrl-d-during-tool");
        let _guard = TempDirGuard(dir.clone());
        let marker = dir.join("tool");
        let fake = FakeLlm::start(vec![
            Reply::ToolCall {
                name: "fixture_sleep".to_string(),
                arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
            },
            Reply::Text("fixture done after ctrl-d k4d".to_string()),
        ]);
        write_fake_llm_config_prompt_free(&dir, &fake, true);
        write_tool(&dir, "fixture_sleep.sh", LONG_SLEEP_TOOL);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"call the sleep tool\r").unwrap();
        wait_for_file(
            &mut pty,
            &dir.join("tool-started"),
            "the sleep tool's start marker",
        );
        pty.read_for(Duration::from_millis(300));
        let pressed_at = pty.raw().len();
        let pressed = Instant::now();
        pty.send(b"\x04").unwrap();
        let transcript = match pty.wait_for("tty-probe-done", Duration::from_secs(7)) {
            Ok(transcript) => transcript,
            Err(err) => {
                // Not a quit: the UI must at least still be alive and leave
                // clean once the tool is released; report which it was.
                fs::write(dir.join("tool-release"), "").unwrap();
                let alive = pty.wait_for_after("k4d", pressed_at, PTY_DEADLINE).is_ok();
                pty.send(b"\x04").unwrap();
                let after = pty
                    .wait_for("tty-probe-done", PTY_DEADLINE)
                    .unwrap_or_else(|_| strip_ansi(&String::from_utf8_lossy(pty.raw())));
                panic!(
                    "Ctrl+D during a running tool neither quit within 7s nor was it refused with a \
                 live UI ({err}); after releasing the tool the reply arrived: {alive}; the exit \
                 then:\n{after}"
                );
            }
        };
        let took = pressed.elapsed();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
        assert_eq!(
            count(pty.raw(), LEAVE_ALT),
            1,
            "Ctrl+D during a tool left the alternate screen {} times:\n{transcript}",
            count(pty.raw(), LEAVE_ALT)
        );
        assert!(
            took < Duration::from_secs(7),
            "the quit waited the tool out instead of interrupting it: {took:?}"
        );
        assert!(
            !transcript.contains("did not stop within"),
            "the quit gave up on the tool instead of killing it:\n{transcript}"
        );
    }

    // ---------------------------------------------------------------------
    // Resize while a reply is still streaming.
    // ---------------------------------------------------------------------

    /// The window is resized while a stream is held mid-reply; the stream is
    /// then released. On the next full repaint every piece of the reply is
    /// drawn on exactly one row: no duplicated rows from the re-wrap, and
    /// nothing of the reply lost.
    #[test]
    #[serial]
    fn a_resize_mid_stream_leaves_no_duplicated_reply_rows() {
        let dir = fresh_config_dir("resize-mid-stream");
        let _guard = TempDirGuard(dir.clone());
        let release = dir.join("release");
        let fake = FakeLlm::start(vec![Reply::Streamed(vec![
            Chunk::Text("head head-a7q of the reply.".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text(" tail tail-b8r of the reply.".to_string()),
        ])]);
        write_fake_llm_config_prompt_free(&dir, &fake, true);
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(b"stream and hold\r").unwrap();
        let started = Instant::now();
        wait_for_squeezed_after(&mut pty, "head-a7q", 0, "the first delta");
        assert!(
            started.elapsed() < HOLD_LIMIT,
            "the first delta appeared only after the fake gave up holding the stream"
        );
        pty.wait_for("Esc to interrupt", PTY_DEADLINE)
            .expect("the status row never offered Esc to interrupt");

        // Narrower while the stream is held, then let it finish.
        pty.resize(40, 60).unwrap();
        pty.read_for(Duration::from_millis(600));
        fs::write(&release, "").unwrap();
        wait_for_squeezed_after(&mut pty, "tail-b8r", 0, "the tail after the release");
        pty.read_for(Duration::from_millis(800));
        assert_eq!(fake.served(), 1);

        // A further resize forces a full repaint of the settled transcript.
        let repaint_at = pty.raw().len();
        pty.resize(40, 70).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "tail-b8r",
            repaint_at,
            "the repaint after the second resize",
        );
        pty.read_for(Duration::from_millis(800));
        // Everything the child wrote since the resize: the full repaint plus
        // whatever incremental frames followed it.
        let repainted = strip_ansi(&String::from_utf8_lossy(&pty.raw()[repaint_at..]));
        assert!(
            squeezed(&repainted).contains("head-a7q"),
            "the head streamed before the resize is gone from the repainted transcript:\n{repainted}"
        );
        for token in [b"head-a7q".as_slice(), b"tail-b8r".as_slice()] {
            let mut rows: Vec<u16> = painted_at(pty.raw(), token, repaint_at)
                .into_iter()
                .map(|(row, _)| row)
                .collect();
            rows.sort_unstable();
            rows.dedup();
            assert!(
                rows.len() <= 1,
                "{:?} was painted on {} distinct rows after the resize ({rows:?}): duplicated rows:\n{repainted}",
                String::from_utf8_lossy(token),
                rows.len()
            );
        }
        // The child may legitimately paint several whole frames after the
        // resize (the repaint, then a status tick), so the tail is counted per
        // synchronized-update frame, never across the raw stream.
        for (n, frame) in frames_since(pty.raw(), repaint_at).iter().enumerate() {
            let flat = squeezed(&strip_ansi(&String::from_utf8_lossy(frame)));
            assert!(
                flat.matches("tail-b8r").count() <= 1,
                "frame {n} after the resize paints the tail more than once:\n{}",
                strip_ansi(&String::from_utf8_lossy(frame))
            );
        }

        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    /// Every `CSI ? 2026 h` .. `CSI ? 2026 l` span the child wrote from `from`
    /// on: one entry per synchronized-update frame.
    fn frames_since(raw: &[u8], from: usize) -> Vec<Vec<u8>> {
        const BEGIN: &[u8] = b"\x1b[?2026h";
        const END: &[u8] = b"\x1b[?2026l";
        let mut frames = Vec::new();
        let mut at = from;
        while let Some(start) = find(raw, BEGIN, at) {
            let Some(end) = find(raw, END, start + BEGIN.len()) else {
                break;
            };
            frames.push(raw[start + BEGIN.len()..end].to_vec());
            at = end + END.len();
        }
        frames
    }

    // ---------------------------------------------------------------------
    // A bracketed paste with Windows line ends (`\r\n`) is one line break per
    // pair, not a blank row; nothing is submitted, and Enter then sends both
    // lines.
    // ---------------------------------------------------------------------

    /// Pastes `first<sep>second` into the empty composer and returns the raw
    /// offset the paste started at, once `second` is on screen.
    fn paste_two_lines(pty: &mut Pty, first: &str, sep: &str, second: &str) -> usize {
        pty.read_for(Duration::from_millis(300));
        let before = pty.raw().len();
        pty.send(format!("\x1b[200~{first}{sep}{second}\x1b[201~").as_bytes())
            .unwrap();
        pty.wait_for_after(second, before, PTY_DEADLINE)
            .expect("the pasted text never reached the composer");
        pty.read_for(Duration::from_millis(800));
        before
    }

    /// The two pasted pieces sit on consecutive rows at the same column: one
    /// line break for the separator, no blank row and no stray glyph.
    fn assert_stacked_once(pty: &Pty, before: usize, first: &str, second: &str, sep: &str) {
        let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
        assert!(since.contains(first), "{since}");
        assert!(
            !since.contains("Working"),
            "a paste containing {sep:?} was submitted as a prompt:\n{since}"
        );
        let a = painted_at(pty.raw(), first.as_bytes(), before);
        let b = painted_at(pty.raw(), second.as_bytes(), before);
        let (Some(&(row_a, col_a)), Some(&(row_b, col_b))) = (a.last(), b.last()) else {
            panic!("the pasted pieces were not positioned on screen:\n{since}");
        };
        assert_eq!(
            (row_b, col_b),
            (row_a + 1, col_a),
            "{sep:?} did not produce exactly one line break ({a:?} / {b:?}):\n{since}"
        );
        // Neither a `^M` glyph nor a raw CR sits between the pieces on screen.
        let between = since
            .find(first)
            .and_then(|s| {
                since[s..]
                    .find(second)
                    .map(|e| &since[s + first.len()..s + e])
            })
            .unwrap_or("");
        assert!(
            !between.contains("^M") && !between.contains('\u{240d}'),
            "a carriage return was drawn as a glyph between the pasted lines: {between:?}\n{since}"
        );
    }

    #[test]
    #[serial]
    fn a_crlf_paste_is_one_line_break_and_enter_sends_both_lines() {
        let dir = fresh_config_dir("paste-crlf");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        // Windows line ends: one break per pair.
        let before = paste_two_lines(&mut pty, "crlf-top-k4m", "\r\n", "crlf-bottom-p7w");
        assert_stacked_once(&pty, before, "crlf-top-k4m", "crlf-bottom-p7w", "\r\n");

        // Enter submits the two-line draft; the dry-run echo carries both.
        let sent_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        wait_for_squeezed_after(&mut pty, "Working", sent_at, "the working row");
        let deadline = Instant::now() + PTY_DEADLINE;
        loop {
            pty.read_for(Duration::from_millis(200));
            let since = squeezed(&strip_ansi(&String::from_utf8_lossy(&pty.raw()[sent_at..])));
            // The user cell repeats the draft once; the reply echoes it again.
            if since.matches("crlf-bottom-p7w").count() >= 2
                && since.matches("crlf-top-k4m").count() >= 2
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the dry-run echo never carried both pasted lines:\n{}",
                strip_ansi(&String::from_utf8_lossy(&pty.raw()[sent_at..]))
            );
        }

        // Bare LF pastes behave the same way on the now-empty composer.
        pty.read_for(Duration::from_millis(500));
        let before = paste_two_lines(&mut pty, "lf-top-z2q", "\n", "lf-bottom-v9c");
        assert_stacked_once(&pty, before, "lf-top-z2q", "lf-bottom-v9c", "\n");

        // Clear the draft, then the quit chord.
        pty.send(b"\x03").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }

    // ---------------------------------------------------------------------
    // `hello` -> echo, then `.undo`. Only the command turn's liveness is
    // judged here: whatever `.undo` says, the UI keeps taking keys and quits
    // clean.
    // ---------------------------------------------------------------------

    #[test]
    #[serial]
    fn dot_undo_after_a_dry_run_turn_keeps_the_ui_live() {
        let dir = fresh_config_dir("undo");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config_with(&dir, "");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        pty.send(b"undo-probe-h3x marker").unwrap();
        pty.wait_for("undo-probe-h3x", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(300));
        let sent_at = pty.raw().len();
        pty.send(b"\r").unwrap();
        let deadline = Instant::now() + PTY_DEADLINE;
        loop {
            pty.read_for(Duration::from_millis(200));
            let since = squeezed(&strip_ansi(&String::from_utf8_lossy(&pty.raw()[sent_at..])));
            if since.matches("undo-probe-h3x").count() >= 2 {
                break;
            }
            assert!(Instant::now() < deadline, "the dry-run echo never appeared");
        }
        pty.read_for(Duration::from_millis(500));

        let undo_at = pty.raw().len();
        pty.send(b".undo\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        // Force a whole repaint so the settled transcript is observable.
        pty.resize(24, 90).unwrap();
        pty.read_for(Duration::from_millis(1000));
        let after = strip_ansi(&String::from_utf8_lossy(&pty.raw()[undo_at..]));
        assert!(
            !after.contains("panicked"),
            "the .undo command turn panicked:\n{after}"
        );

        // Still taking keys: a fresh draft is echoed into the composer. The
        // echo may land across two coalesced frames (the cell diff paints only
        // the new columns), so it is matched with whitespace squeezed out.
        let live_at = pty.raw().len();
        pty.send(b"still-live-q6d").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "still-live-q6d",
            live_at,
            "the composer stopped taking keys after .undo; the draft",
        );
        pty.send(b"\x03").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
        assert_tty_restored(&transcript);
    }
}
