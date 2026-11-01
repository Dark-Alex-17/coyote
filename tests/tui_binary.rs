//! Real-process checks of the full-screen UI's terminal lifecycle: however
//! the binary leaves (Ctrl+D, SIGTERM, a panic after the alternate screen is
//! up) the tty it ran on must come back in its cooked, echoing state. Each
//! case runs the binary on a pseudo-terminal under a bash wrapper that
//! reports the exit code and `stty -a` afterwards, so the file is unix-only.
//! The cases run one at a time: each spawn forks a session leader and takes
//! a controlling tty, and a fork landing between another test's `openpty`
//! and its `exec` has failed the spawn with a garbled errno.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process};

use common::pty::{Pty, strip_ansi};
use serial_test::serial;

/// Covers the keyboard-enhancement probe, which waits two seconds for an
/// answer a pseudo-terminal never gives.
const PTY_DEADLINE: Duration = Duration::from_secs(20);

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
        "coyote-tui-binary-{scenario}-{}-{unique}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

/// A dry-run config that never asks to save a session, so every exit path
/// is prompt-free. Paths land in single-quoted YAML scalars.
fn write_dry_run_config(dir: &Path) {
    write_config(dir, Some("save_session: false\n"));
}

/// The same config with `save_session` left to the caller: `None` leaves
/// it unset, so a dirty session asks "Save session?" on `.exit session`.
fn write_config(dir: &Path, save_session: Option<&str>) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             stream: true\n\
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
             {save_session}\
             workspace_instructions: false\n\
             memory: false\n",
            vault_pass = vault_pass.display(),
            save_session = save_session.unwrap_or(""),
        ),
    )
    .unwrap();
}

/// Runs the binary in the background of a bash wrapper on a fresh pty with
/// the tty on stdin. The wrapper names the child's pid, waits for it, and
/// then reports its exit code and the tty flags so a test can check both
/// after any exit path.
fn spawn_tui(dir: &Path, extra_env: &[(&str, &str)]) -> Pty {
    spawn_tui_under(dir, extra_env, WAIT_WRAPPER, "/dev/tty")
}

/// Report line emitted by both wrappers once the binary has exited. The
/// alternate screen leaves no newline behind once escapes are stripped,
/// so the report starts a fresh line first.
const TTY_REPORT: &str = "echo; echo \"exit=$rc\"; \
     echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done";

/// The plain wrapper: one process group, `wait` for the child. Being a
/// `setsid` leader whose parent sits in another session makes that group
/// orphaned, and the kernel discards SIGTSTP for orphaned groups, so this
/// wrapper cannot observe a suspend.
const WAIT_WRAPPER: &str =
    "'{coyote}' <{stdin} & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {report}";

/// A job-control wrapper that behaves like the user's shell: the binary
/// runs as the foreground job in its own process group; when it stops the
/// wrapper regains the terminal, says so, reports the tty flags it sees,
/// and after a moment resumes the job with `fg` as a user would. (`fg`
/// must keep its own stdout: redirecting it stops bash handing the tty
/// over.)
const JOB_CONTROL_WRAPPER: &str = "set -m; '{coyote}' <{stdin}; rc=$?; \
     while [ -n \"$(jobs -s)\" ]; do echo; echo job-stopped; \
     echo \"stopped-tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo stopped-probe-done; \
     sleep 1; fg %1; rc=$?; done; {report}";

/// The one place the binary is launched: `wrapper` is one of the two bash
/// scripts above and `stdin` the path the binary's stdin is redirected from.
fn spawn_tui_under(dir: &Path, extra_env: &[(&str, &str)], wrapper: &str, stdin: &str) -> Pty {
    let home = dir.join("home");
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
    let path = env::join_paths(
        iter::once(fixture_bin).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    let mut cmd = Command::new("bash");
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            cmd.env_remove(&key);
        }
    }
    cmd.arg("-c")
        .arg(
            wrapper
                .replace("{coyote}", env!("CARGO_BIN_EXE_coyote"))
                .replace("{stdin}", stdin)
                .replace("{report}", TTY_REPORT),
        )
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("COYOTE_UI", "tui")
        .env("HOME", &home)
        .env("TERM", "xterm-256color")
        .env("PATH", path)
        .env_remove("IS_SANDBOX")
        .env_remove("COLORTERM")
        .env_remove("NO_COLOR")
        .envs(extra_env.iter().copied());
    Pty::spawn(cmd).unwrap()
}

fn tty_flag_tokens(transcript: &str) -> Vec<&str> {
    let flags = transcript
        .lines()
        .find_map(|line| line.strip_prefix("tty-flags: "))
        .unwrap_or_else(|| panic!("no tty-flags line in the transcript:\n{transcript}"));
    flags
        .split(|c: char| c.is_whitespace() || c == ';')
        .collect()
}

/// Raw mode clears both; the UI must have put both back.
fn assert_tty_restored(transcript: &str) {
    let tokens = tty_flag_tokens(transcript);
    for flag in ["echo", "icanon"] {
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

fn child_pid(transcript: &str) -> i32 {
    transcript
        .lines()
        .find_map(|line| line.strip_prefix("cpid="))
        .unwrap_or_else(|| panic!("no cpid= line in the transcript:\n{transcript}"))
        .trim()
        .parse()
        .unwrap()
}

/// Also the one place a REPL command's `print!` output is seen through the
/// fd capture: in-process tests never see it, because libtest diverts
/// `print!` before it reaches fd 1. `.info` is taller than the screen, so
/// the marker is its last row. The table is a `Command` cell inside the
/// alternate screen, so once the screen is left nothing of it may reach
/// the terminal the shell gets back: the only bytes after the leave are
/// the wrapper's own report.
#[test]
#[serial]
fn a_command_prints_into_the_transcript_and_ctrl_d_leaves_the_tty_cooked() {
    let dir = fresh_config_dir("ctrl-d");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE).unwrap();
    // Let the capture settle so a late forward would have happened.
    pty.read_for(Duration::from_millis(500));
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
    let dir = fresh_config_dir("sigterm");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let pid = child_pid(&transcript);
    // SAFETY: kill takes a pid and a signal number and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

/// The panic hook restores the terminal before delegating to the original
/// hook: the alternate screen must be left BEFORE the panic message is
/// printed, or the message lands on a screen that is about to vanish.
#[test]
#[serial]
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
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the alternate screen was entered without a tty:\n{transcript}"
    );
}

const ENTER_ALT: &[u8] = b"\x1b[?1049h";
const LEAVE_ALT: &[u8] = b"\x1b[?1049l";
const PASTE_ON: &[u8] = b"\x1b[?2004h";
const PASTE_OFF: &[u8] = b"\x1b[?2004l";
const KEYBOARD_POP: &[u8] = b"\x1b[<u";

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack[from.min(haystack.len())..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|at| from + at)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

/// Kitty keyboard push: `ESC [ > <flags> u`.
fn find_keyboard_push(haystack: &[u8], from: usize) -> Option<usize> {
    let mut at = from;
    while let Some(start) = find(haystack, b"\x1b[>", at) {
        let rest = &haystack[start + 3..];
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 && rest.get(digits) == Some(&b'u') {
            return Some(start);
        }
        at = start + 1;
    }
    None
}

/// Every SGR parameter list (`ESC [ ... m`) in the raw output, split into
/// its numeric parameters.
fn sgr_params(raw: &[u8]) -> Vec<Vec<u16>> {
    let text = String::from_utf8_lossy(raw);
    let mut out = Vec::new();
    let mut rest: &str = &text;
    while let Some(start) = rest.find("\x1b[") {
        rest = &rest[start + 2..];
        let end = rest
            .find(|c: char| !(('\x30'..='\x3f').contains(&c) || ('\x20'..='\x2f').contains(&c)));
        let Some(end) = end else { break };
        if rest.as_bytes()[end] == b'm' {
            let params = &rest[..end];
            if params.is_empty() {
                out.push(vec![0]);
            } else {
                out.push(
                    params
                        .split([';', ':'])
                        .map(|p| p.parse::<u16>().unwrap_or(u16::MAX))
                        .collect(),
                );
            }
        }
        rest = &rest[end + 1..];
    }
    out
}

fn is_colour_sgr(params: &[u16]) -> bool {
    let mut i = 0;
    while i < params.len() {
        let p = params[i];
        if (30..=37).contains(&p)
            || (40..=47).contains(&p)
            || (90..=97).contains(&p)
            || (100..=107).contains(&p)
            || p == 38
            || p == 48
            || p == 58
        {
            return true;
        }
        i += 1;
    }
    false
}

fn is_rgb_sgr(params: &[u16]) -> bool {
    params
        .windows(2)
        .any(|w| (w[0] == 38 || w[0] == 48 || w[0] == 58) && w[1] == 2)
}

/// The alternate screen is entered once and left on Ctrl+D; bracketed
/// paste comes and goes with it; keyboard-enhancement flags, when the
/// terminal takes them, are pushed after alt-screen entry and popped
/// before leaving it, and the support probe is sent before the reader
/// starts.
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

    let paste_on = find(raw, PASTE_ON, 0).expect("bracketed paste was never enabled");
    let paste_off = rfind(raw, PASTE_OFF).expect("bracketed paste was never disabled");
    assert!(
        paste_on < paste_off && paste_off < leave_alt,
        "bracketed paste must be disabled before the alternate screen is left \
         (on={paste_on}, off={paste_off}, leave={leave_alt})"
    );

    // The keyboard-enhancement probe is answered by nothing on a pty, so
    // the flags may legitimately never be pushed; if they are, the push
    // follows alt-screen entry and the pop precedes leaving it.
    assert!(
        find(raw, b"\x1b[?u", 0).is_some(),
        "the keyboard-enhancement support probe (CSI ? u) was never sent"
    );
    if let Some(push) = find_keyboard_push(raw, 0) {
        let pop = rfind(raw, KEYBOARD_POP).expect("flags pushed but never popped");
        assert!(enter_alt < push, "flags pushed before alt-screen entry");
        assert!(push < pop && pop < leave_alt, "flags popped after leaving");
    }
}

/// SIGHUP runs the same leave path as SIGTERM.
#[test]
#[serial]
fn sighup_leaves_the_tty_cooked() {
    let dir = fresh_config_dir("sighup");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let pid = child_pid(&transcript);
    // SAFETY: kill takes a pid and a signal number and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGHUP) }, 0);
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();

    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        rfind(pty.raw(), LEAVE_ALT).is_some(),
        "SIGHUP exit never left the alternate screen"
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
    write_config(&dir, None);
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
    pty.read_for(Duration::from_millis(1200));
    pty.send(b"y").unwrap();
    pty.wait_for("ession name", PTY_DEADLINE).unwrap();
    let named_at = pty.raw().len();
    pty.send(b"probe-saved\r").unwrap();
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

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
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
    write_config(&dir, None);
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

/// The TUI is a provisional `COYOTE_UI=tui` branch: any other value leaves
/// the existing REPL in charge, which never touches the alternate screen.
#[test]
#[serial]
fn any_other_coyote_ui_value_starts_the_plain_repl() {
    let dir = fresh_config_dir("repl-branch");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[("COYOTE_UI", "repl")]);

    let transcript = pty.wait_for("Welcome to", PTY_DEADLINE).unwrap();
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "COYOTE_UI=repl entered the alternate screen:\n{transcript}"
    );
    pty.read_for(Duration::from_millis(500));
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "COYOTE_UI=repl entered the alternate screen:\n{transcript}"
    );
    assert_tty_restored(&transcript);
}

/// Teardown mirrors `Repl::run`: `top_level_agent_finished` fires the
/// agent's `agent.completed` hook and `tui::run` awaits
/// `hooks::drain_pending` before returning, so a Ctrl+D under `--agent`
/// leaves the hook's marker behind. The hook is an agent-scoped global
/// hook admitted through `global_hooks: ["*"]`.
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
         description: usage probe agent\n\
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

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let fired = fs::read_to_string(&marker)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "COMPLETED")
            .count();
        if fired == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline && fired == 0,
            "agent.completed fired {fired} times after Ctrl+D (expected once):\n{transcript}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
