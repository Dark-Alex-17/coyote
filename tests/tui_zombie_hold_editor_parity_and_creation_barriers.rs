//! Black-box pins for the bottom pane's process and creation edges: a
//! backtick `.file` command's no-`wait` orphan dies on Esc AND on Ctrl+C
//! while its exited shell is still unreaped (the zombie hold), and a
//! sleeper left behind by a shell that finished on its own is observed;
//! Ctrl+O and `.edit` report an editor that wrote the file and then
//! exited 3 with one identical line; a `.macro <new>` created in the
//! editor (no request made) is not a rewind barrier yet its file exists;
//! `.role <new>` writes the role file with the editor's content and `n`
//! creates nothing. The built binary on a pseudo-terminal with the
//! dry-run model. Unix-only and serial like its sibling pty suites.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process};

use common::pty::{Pty, strip_ansi};
use serial_test::serial;

const PTY_DEADLINE: Duration = Duration::from_secs(20);
/// Past both idle gates (1 s from the last keystroke, 1 s from a modal's
/// appearance).
const HOTKEY_GAP: Duration = Duration::from_millis(1200);
const REPAINT_SETTLE: Duration = Duration::from_millis(500);
const STOP_NOTICE: &str = "Rewind stops at the last";
const DOWN: &[u8] = b"\x1b[B";
const CTRL_C: &[u8] = b"\x03";
const CTRL_D: &[u8] = b"\x04";
const CTRL_O: &[u8] = b"\x0f";
const ESC: &[u8] = b"\x1b";
const ENTER_ALT: &[u8] = b"\x1b[?1049h";
const LEAVE_ALT: &[u8] = b"\x1b[?1049l";
const PASTE_ON: &[u8] = b"\x1b[?2004h";

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
        "coyote-tui-hold-{scenario}-{}-{unique}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

/// Dry-run config; `extra` is appended as top-level YAML.
fn write_dry_run_config(dir: &Path, extra: &str) {
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
             {extra}\
             workspace_instructions: false\n\
             memory: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

/// Writes an executable shell script at `dir/<name>` and returns its path.
fn write_script(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join(name);
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script.to_str().unwrap().to_string()
}

const TTY_REPORT: &str = "echo; echo \"exit=$rc\"; \
     echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done";
const WAIT_WRAPPER: &str =
    "'{coyote}'{args} <{stdin} & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {report}";

fn built_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_coyote"))
}

/// The environment every spawn shares: config, cache and HOME under
/// `dir`, the fixture bin first on PATH, no inherited COYOTE_* or colour
/// variables.
fn base_env(cmd: &mut Command, dir: &Path) {
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
    let path = env::join_paths(
        iter::once(fixture_bin).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            cmd.env_remove(&key);
        }
    }
    cmd.current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("HOME", dir.join("home"))
        .env("TERM", "xterm-256color")
        .env("PATH", path)
        .env_remove("IS_SANDBOX")
        .env_remove("COLORTERM")
        .env_remove("NO_COLOR");
}

fn spawn_tui(dir: &Path, extra_env: &[(&str, &str)]) -> Pty {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(
        WAIT_WRAPPER
            .replace("{coyote}", built_binary().to_str().unwrap())
            .replace("{args}", "")
            .replace("{stdin}", "/dev/tty")
            .replace("{report}", TTY_REPORT),
    );
    base_env(&mut cmd, dir);
    cmd.env("COYOTE_UI", "tui").envs(extra_env.iter().copied());
    Pty::spawn(cmd).unwrap()
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

/// The tty is cooked again once the UI is gone.
fn assert_tty_restored(transcript: &str) {
    let flags = transcript
        .lines()
        .find(|line| line.starts_with("tty-flags:"))
        .unwrap_or_else(|| panic!("no tty-flags line:\n{transcript}"));
    assert!(
        flags.contains(" icanon") && flags.contains(" echo") && !flags.contains("-icanon"),
        "the tty was left raw: {flags}\n{transcript}"
    );
}

fn since(pty: &Pty, from: usize) -> String {
    strip_ansi(&String::from_utf8_lossy(
        &pty.raw()[from.min(pty.raw().len())..],
    ))
}

fn squeezed(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Forces a full repaint by resizing and returns the raw offset it starts
/// after.
fn repaint(pty: &mut Pty, rows: u16, cols: u16) -> usize {
    let at = pty.raw().len();
    pty.resize(rows, cols).unwrap();
    pty.read_for(REPAINT_SETTLE);
    at
}

fn repainted_frame(pty: &mut Pty, rows: u16, cols: u16) -> String {
    let at = repaint(pty, rows, cols);
    since(pty, at)
}

/// A fresh full frame on every call: the width alternates so the resize
/// is never coalesced away.
fn next_frame(pty: &mut Pty, toggle: &mut bool) -> String {
    *toggle = !*toggle;
    repainted_frame(pty, 40, if *toggle { 101 } else { 100 })
}

fn wait_for_squeezed_after(pty: &mut Pty, marker: &str, from: usize, what: &str) -> String {
    let deadline = Instant::now() + PTY_DEADLINE;
    let wanted = squeezed(marker);
    loop {
        pty.read_for(Duration::from_millis(200));
        let text = since(pty, from);
        if squeezed(&text).contains(&wanted) {
            return text;
        }
        assert!(Instant::now() < deadline, "{what} never appeared:\n{text}");
    }
}

/// Quit from a composer that may hold a draft: Ctrl+C clears a draft (or
/// quits an idle empty composer); Ctrl+D follows if still up.
fn quit_clearing_any_draft(pty: &mut Pty) -> String {
    pty.send(CTRL_C).unwrap();
    let after = pty.read_for(HOTKEY_GAP);
    if !after.contains("tty-probe-done") {
        pty.send(CTRL_D).unwrap();
    }
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert!(
        !transcript.contains("panicked"),
        "the UI panicked somewhere:\n{transcript}"
    );
    transcript
}

/// The composer's text in a repainted frame: what sits between the
/// field's rule and the footer, runs of blanks folded to one space.
fn composer_row(frame: &str, footer_marker: &str) -> String {
    let rule = frame
        .rfind('\u{2500}')
        .map_or(0, |at| at + '\u{2500}'.len_utf8());
    let after = &frame[rule..];
    let footer = after.find(footer_marker).unwrap_or(after.len());
    after[..footer]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// What sits above the composer's rule in a repainted frame.
fn above_rule(frame: &str) -> String {
    let rule = frame.rfind('\u{2500}').unwrap_or(frame.len());
    frame[..rule].to_string()
}

fn shows_placeholder(flat: &str) -> bool {
    flat.contains("Typeapromptora.command") || flat.contains("Typeapromptora/command")
}

/// Clears a prefilled draft without risking the idle-empty Ctrl+C quit.
fn clear_any_draft(pty: &mut Pty) {
    static TOGGLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let odd = TOGGLE.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
    let frame = repainted_frame(pty, 60, if odd { 98 } else { 99 });
    if !shows_placeholder(&squeezed(&frame)) {
        pty.send(CTRL_C).unwrap();
        pty.read_for(Duration::from_millis(500));
    }
}

/// Esc Esc on the idle empty composer; returns the offset the picker was
/// asked at, with the band on screen.
fn open_rewind_picker(pty: &mut Pty) -> usize {
    let picker_at = pty.raw().len();
    pty.send(ESC).unwrap();
    pty.read_for(Duration::from_millis(150));
    pty.send(ESC).unwrap();
    pty.wait_for_after("Rewind to", picker_at, PTY_DEADLINE)
        .expect("Esc Esc did not open the rewind picker");
    pty.read_for(HOTKEY_GAP);
    picker_at
}

/// The picker's band below its header in a repainted frame.
fn picker_band(frame: &str) -> String {
    frame.split("Rewind to").nth(1).unwrap_or("").to_string()
}

/// Runs a dry-run exchange and waits for its user cell.
fn dry_run_exchange(pty: &mut Pty, prompt: &str) {
    let at = pty.raw().len();
    pty.send(format!("{prompt}\r").as_bytes()).unwrap();
    wait_for_squeezed_after(pty, &format!("> {prompt}"), at, "the exchange");
    pty.read_for(Duration::from_millis(2500));
}

/// Waits for the terminal to be handed away and taken back: alt-screen
/// leave, re-enter and bracketed paste back on. Returns the re-entry
/// offset.
fn wait_for_lend_and_return(pty: &mut Pty, from: usize, what: &str) -> usize {
    let left = pty
        .wait_for_raw(LEAVE_ALT, from, PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("{what} did not release the terminal: {e}"));
    let re_entered = pty
        .wait_for_raw(ENTER_ALT, left, PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("after {what} the alternate screen was not re-entered: {e}"));
    pty.wait_for_raw(PASTE_ON, re_entered, PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("after {what} bracketed paste was not re-enabled: {e}"));
    pty.read_for(Duration::from_millis(300));
    re_entered
}

/// The composer takes a command again: `.info` renders its table.
fn assert_composer_takes_keys(pty: &mut Pty, after: usize, what: &str) {
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", after, PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("keys were not handled after {what}: {e}"));
    pty.read_for(Duration::from_millis(300));
}

/// Every report above the composer rule that names the editor, cut by
/// its shape: from the word `editor` (any case) through the first run of
/// digits that FOLLOWS it, whitespace folded. A repainted frame has no
/// row breaks, so the word before `editor` may be glued to the previous
/// cell's text and is left out of the cut.
fn editor_report_lines(frame: &str) -> Vec<String> {
    let flat = above_rule(frame)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let lower = flat.to_lowercase();
    let mut reports = Vec::new();
    let mut start = 0;
    while let Some(rel) = lower[start..].find("editor") {
        let at = start + rel;
        let tail = &flat[at..];
        let end = tail
            .find(|c: char| c.is_ascii_digit())
            .map(|digits| {
                digits
                    + tail[digits..]
                        .find(|c: char| !c.is_ascii_digit())
                        .unwrap_or(tail.len() - digits)
            })
            .unwrap_or(tail.len());
        let report = tail[..end].to_string();
        start = (at + end).max(at + "editor".len());
        if report.to_lowercase().contains("exit") || report.to_lowercase().contains("status") {
            reports.push(report);
        }
        if start >= flat.len() {
            break;
        }
    }
    reports
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

/// Files under `dir` (outside the cache) carrying `name` in their path.
fn files_named(dir: &Path, name: &str) -> Vec<PathBuf> {
    walk(dir)
        .into_iter()
        .filter(|path| !path.to_string_lossy().contains("/cache/"))
        .filter(|path| path.to_string_lossy().contains(name))
        .collect()
}

// ---------------------------------------------------------------------
// Process helpers for the zombie-hold pins.
// ---------------------------------------------------------------------

/// The pid a script wrote to `path`; an empty or partial read is retried.
fn wait_for_pid_in(pty: &mut Pty, path: &Path, what: &str) -> i32 {
    let deadline = Instant::now() + PTY_DEADLINE;
    loop {
        if let Some(pid) = fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            return pid;
        }
        let text = pty.read_for(Duration::from_millis(50));
        assert!(
            Instant::now() < deadline,
            "{what}: no pid landed in {}:\n{text}",
            path.display()
        );
    }
}

/// A reaped pid answers ESRCH to a null signal; a zombie or a running
/// process answers 0.
fn process_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 touches no memory; it only checks the pid.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_for_process_gone(pid: i32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while process_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    true
}

/// The sleepers a test forked and has not yet seen die. A pid proven
/// gone is taken out at once, since a freed pid may be someone else's
/// by now; the rest are killed only when the test panics, so a failed
/// assertion leaves none behind.
struct Sleepers(Vec<i32>);

impl Sleepers {
    fn forget(&mut self, pid: i32) {
        self.0.retain(|&held| held != pid);
    }
}

impl Drop for Sleepers {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        for &pid in &self.0 {
            // SAFETY: kill takes a pid and a signal number and touches no memory.
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

// ---------------------------------------------------------------------
// The no-`wait` orphan dies on Esc and on Ctrl+C under the zombie hold.
// ---------------------------------------------------------------------

/// Two `.file` turns whose backtick shell backgrounds a sleeper WITHOUT
/// `wait` and exits (the sleeper keeps the shell's stdout, so the shell
/// is exited-but-unreaped while it lives). The first turn is ended by
/// Esc, the second by Ctrl+C. Each sleeper is gone within three seconds
/// of the key, the composer takes keys again, and the session quits
/// cleanly. A third turn pairs a sleeper whose shell released the pipe
/// at once (that command finished on its own, so nothing is held for
/// it) with a live blocker; the blocker dies, the other sleeper's fate
/// is reported, not asserted.
#[test]
#[serial]
fn a_no_wait_orphan_dies_on_esc_and_on_ctrl_c_while_its_shell_is_unreaped() {
    let dir = fresh_config_dir("zombie-hold");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let esc_pid = dir.join("esc.pid");
    let ctrlc_pid = dir.join("ctrlc.pid");
    let finished_pid = dir.join("finished.pid");
    let blocker_pid = dir.join("blocker.pid");
    let orphan_esc = write_script(
        &dir,
        "orphan-esc.sh",
        &format!(
            "#!/bin/sh\nsleep 6181 & echo $! > '{}'\necho backgrounded\n",
            esc_pid.display()
        ),
    );
    let orphan_ctrlc = write_script(
        &dir,
        "orphan-ctrlc.sh",
        &format!(
            "#!/bin/sh\nsleep 6182 & echo $! > '{}'\necho backgrounded\n",
            ctrlc_pid.display()
        ),
    );
    let finished = write_script(
        &dir,
        "finished.sh",
        &format!(
            "#!/bin/sh\nsleep 6183 </dev/null >/dev/null 2>&1 & echo $! > '{}'\necho released\n",
            finished_pid.display()
        ),
    );
    let blocker = write_script(
        &dir,
        "blocker.sh",
        &format!(
            "#!/bin/sh\nsleep 6184 & echo $! > '{}'\nwait\n",
            blocker_pid.display()
        ),
    );
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    let mut misses: Vec<String> = Vec::new();
    let mut sleepers = Sleepers(Vec::new());
    for (script, pid_file, interrupt, what) in [
        (&orphan_esc, &esc_pid, ESC, "Esc on a no-wait orphan"),
        (
            &orphan_ctrlc,
            &ctrlc_pid,
            CTRL_C,
            "Ctrl+C on a no-wait orphan",
        ),
    ] {
        let turn_at = pty.raw().len();
        pty.send(format!(".file `sh {script}` -- summarize\r").as_bytes())
            .unwrap();
        let pid = wait_for_pid_in(&mut pty, pid_file, &format!("{what}: the sleeper's pid"));
        sleepers.0.push(pid);
        assert!(
            process_alive(pid),
            "{what}: the sleeper (pid {pid}) is gone before the key:\n{}",
            since(&pty, turn_at)
        );
        // Let the shell exit so the key lands on an exited-but-unreaped
        // leader, which is the case under test. The turn cannot have
        // ended on its own: the orphan still holds the shell's stdout, so
        // the capture never reaches EOF while it lives.
        pty.read_for(Duration::from_millis(1200));
        pty.send(interrupt).unwrap();
        let key_at = Instant::now();
        if wait_for_process_gone(pid, Duration::from_secs(3)) {
            sleepers.forget(pid);
        } else {
            misses.push(format!(
                "{what}: the sleeper (pid {pid}) outlived the key by three seconds:\n{}",
                since(&pty, turn_at)
            ));
        }
        assert_composer_takes_keys(&mut pty, turn_at, what);
        if key_at.elapsed() > Duration::from_secs(10) {
            misses.push(format!(
                "{what}: took {:?} to end the backtick turn",
                key_at.elapsed()
            ));
        }
        let after = since(&pty, turn_at);
        if after.contains("panicked") {
            misses.push(format!("{what}: the UI panicked:\n{after}"));
        }
    }

    let turn_at = pty.raw().len();
    pty.send(format!(".file `sh {finished}` `sh {blocker}` -- summarize\r").as_bytes())
        .unwrap();
    let finished_sleeper =
        wait_for_pid_in(&mut pty, &finished_pid, "the finished shell's sleeper pid");
    sleepers.0.push(finished_sleeper);
    let blocker_sleeper = wait_for_pid_in(&mut pty, &blocker_pid, "the blocker sleeper's pid");
    sleepers.0.push(blocker_sleeper);
    pty.read_for(Duration::from_millis(1200));
    pty.send(ESC).unwrap();
    let blocker_gone = wait_for_process_gone(blocker_sleeper, Duration::from_secs(3));
    let finished_gone = wait_for_process_gone(finished_sleeper, Duration::from_secs(1));
    if blocker_gone {
        sleepers.forget(blocker_sleeper);
    } else {
        misses.push(format!(
            "the live blocker's grandchild (pid {blocker_sleeper}) outlived Esc:\n{}",
            since(&pty, turn_at)
        ));
    }
    if finished_gone {
        sleepers.forget(finished_sleeper);
    } else {
        // Alive a moment ago and this test's own sleeper, which no held
        // group will ever reach: end it here rather than leave it behind.
        // SAFETY: kill takes a pid and a signal number and touches no memory.
        let _ = unsafe { libc::kill(finished_sleeper, libc::SIGKILL) };
        sleepers.forget(finished_sleeper);
    }
    eprintln!(
        "observation: the sleeper left by the shell that finished on its own (pid {finished_sleeper}) {} Esc",
        if finished_gone {
            "DIED on"
        } else {
            "SURVIVED (nothing is held for a finished command)"
        }
    );
    assert_composer_takes_keys(&mut pty, turn_at, "Esc on the finished + blocker pair");

    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} zombie-hold expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// Ctrl+O and `.edit config` report an editor that wrote the file and
// then exited 3 with one identical line.
// ---------------------------------------------------------------------

/// Opens an `$EDITOR` script via `open`; returns the new editor report
/// lines above the composer and the frame they were read from.
fn editor_report_after(
    open: &[u8],
    what: &str,
    editor_body: &str,
    scenario: &str,
) -> (Vec<String>, String) {
    let dir = fresh_config_dir(scenario);
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let editor = write_script(&dir, "editor.sh", editor_body);
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 100).unwrap();
    pty.read_for(Duration::from_millis(400));
    dry_run_exchange(&mut pty, &format!("warm-up-{scenario}"));
    let mut toggle = false;
    let before = next_frame(&mut pty, &mut toggle);
    let reports_before = editor_report_lines(&before);

    let edit_at = pty.raw().len();
    if open == CTRL_O {
        pty.send(b"draft-text").unwrap();
        pty.read_for(Duration::from_millis(300));
    }
    pty.send(open).unwrap();
    wait_for_lend_and_return(&mut pty, edit_at, what);
    pty.read_for(Duration::from_millis(1200));
    let frame = next_frame(&mut pty, &mut toggle);
    let reports: Vec<String> = editor_report_lines(&frame)
        .into_iter()
        .filter(|line| !reports_before.contains(line))
        .collect();
    let settled_at = pty.raw().len();
    clear_any_draft(&mut pty);
    assert_composer_takes_keys(&mut pty, settled_at, what);
    quit_clearing_any_draft(&mut pty);
    (reports, frame)
}

/// An editor that appends ` edited-by-script` to the file it was handed
/// and exits 3: Ctrl+O (with a draft) and `.edit config` each leave
/// exactly one report naming the exit status 3, and the two reports are
/// the same text.
#[test]
#[serial]
fn ctrl_o_and_dot_edit_report_a_modifying_editor_that_exits_3_identically() {
    const EDITOR: &str = "#!/bin/sh\nprintf ' edited-by-script\\n' >> \"$1\"\nexit 3\n";
    let (from_ctrl_o, ctrl_o_frame) = editor_report_after(CTRL_O, "Ctrl+O", EDITOR, "ctrl-o-mod-3");
    let (from_dot_edit, dot_edit_frame) = editor_report_after(
        b".edit config\r",
        "`.edit config`",
        EDITOR,
        "dot-edit-mod-3",
    );
    let mut misses: Vec<String> = Vec::new();
    if from_ctrl_o.len() != 1 {
        misses.push(format!(
            "Ctrl+O into an editor exiting 3 left {} report(s) {from_ctrl_o:?}:\n{ctrl_o_frame}",
            from_ctrl_o.len()
        ));
    }
    if from_dot_edit.len() != 1 {
        misses.push(format!(
            "`.edit config` into an editor exiting 3 left {} report(s) {from_dot_edit:?}:\n{dot_edit_frame}",
            from_dot_edit.len()
        ));
    }
    if !from_ctrl_o.iter().all(|r| r.contains('3'))
        || !from_dot_edit.iter().all(|r| r.contains('3'))
    {
        misses.push(format!(
            "a report does not carry the editor's status 3: Ctrl+O {from_ctrl_o:?}, `.edit` {from_dot_edit:?}"
        ));
    }
    if from_ctrl_o != from_dot_edit {
        misses.push(format!(
            "Ctrl+O and `.edit config` report the editor differently:\n  Ctrl+O: {from_ctrl_o:?}\n  .edit:  {from_dot_edit:?}\n{ctrl_o_frame}\n----\n{dot_edit_frame}"
        ));
    }
    assert!(
        misses.is_empty(),
        "{} editor-parity expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// `.macro <new>` created in the editor is not a rewind barrier.
// ---------------------------------------------------------------------

/// Answers a `Create ...` confirm with `key` after the idle gap and, on
/// `y`, waits for the editor's lend to return. Returns the offset the
/// command was sent at.
fn create_via_modal(pty: &mut Pty, command: &str, key: &[u8], what: &str) -> usize {
    let at = pty.raw().len();
    pty.send(format!("{command}\r").as_bytes()).unwrap();
    pty.wait_for_after("Create", at, PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("`{command}` did not ask to create in a modal: {e}"));
    pty.read_for(HOTKEY_GAP);
    let answered = pty.raw().len();
    pty.send(key).unwrap();
    if key == b"y" {
        wait_for_lend_and_return(pty, answered, what);
        pty.read_for(Duration::from_millis(1200));
    } else {
        pty.read_for(Duration::from_millis(1200));
    }
    at
}

/// `before-create`, `.macro fresh-macro` (confirm `y`, editor writes a
/// one-step body, exit 0), `after-create`. The macro file exists. Esc
/// Esc: the picker offers BOTH prompts with no "Rewind stops at the last
/// macro turn" notice; choosing `before-create` pops both and prefills
/// it.
#[test]
#[serial]
fn creating_a_macro_in_the_editor_is_not_a_rewind_barrier_and_writes_its_file() {
    let dir = fresh_config_dir("macro-create-barrier");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let editor = write_script(
        &dir,
        "macro-editor.sh",
        "#!/bin/sh\nprintf 'description: a macro made in the editor\\nisolated: false\\nsteps:\\n  - fresh-macro-step\\n' > \"$1\"\nexit 0\n",
    );
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    dry_run_exchange(&mut pty, "before-create");
    let create_at = create_via_modal(&mut pty, ".macro fresh-macro", b"y", "the macro editor");
    let mut misses: Vec<String> = Vec::new();
    let created = files_named(&dir, "fresh-macro");
    if !created.iter().any(|p| p.is_file()) {
        misses.push(format!(
            "`.macro fresh-macro` + `y` + editor wrote no macro file (found {created:?}):\n{}",
            since(&pty, create_at)
        ));
    } else {
        let body = fs::read_to_string(&created[0]).unwrap_or_default();
        if !body.contains("fresh-macro-step") {
            misses.push(format!(
                "the macro file does not hold the editor's body: {body:?}"
            ));
        }
    }
    let after_create = since(&pty, create_at);
    if squeezed(&after_create).contains("fresh-macro-step") {
        misses.push(format!(
            "creating the macro ran its step (the creation requested the model):\n{after_create}"
        ));
    }
    clear_any_draft(&mut pty);
    dry_run_exchange(&mut pty, "after-create");

    let picker_at = open_rewind_picker(&mut pty);
    let picker = repainted_frame(&mut pty, 60, 101);
    let band_flat = squeezed(&picker_band(&picker));
    if !band_flat.contains("after-create") || !band_flat.contains("before-create") {
        misses.push(format!(
            "the picker does not offer both prompts across `.macro <new>` creation:\n{picker}"
        ));
    }
    if squeezed(&since(&pty, picker_at)).contains(&squeezed(STOP_NOTICE)) {
        misses.push(format!(
            "`.macro <new>` creation was taken for a barrier (a stop notice is shown):\n{}",
            since(&pty, picker_at)
        ));
    }
    pty.send(DOWN).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let frame = repainted_frame(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    if flat.contains("Rewindto") {
        misses.push(format!("the picker is still up after Enter:\n{frame}"));
    }
    if flat.contains(">after-create") || flat.contains(">before-create") {
        misses.push(format!(
            "choosing the older row did not pop both exchanges:\n{frame}"
        ));
    }
    let row = composer_row(&frame, "dryrun:dry-model");
    if row != "before-create" {
        misses.push(format!(
            "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
        ));
    }
    clear_any_draft(&mut pty);
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} macro-creation expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// `.role <new>` writes the role file with the editor's content; `n`
// creates nothing.
// ---------------------------------------------------------------------

/// `.role declined-role` + `n`: no file. `.role fresh-role` + `y`: the
/// editor (appends a marker line, exits 0) is lent the terminal and
/// afterwards a file named for the role exists under the config dir and
/// carries the marker; no editor report is shown; the UI takes keys and
/// quits cleanly.
#[test]
#[serial]
fn dot_role_new_writes_the_role_file_with_the_editors_content_and_n_creates_nothing() {
    let dir = fresh_config_dir("role-create");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let editor = write_script(
        &dir,
        "role-editor.sh",
        "#!/bin/sh\nprintf 'role-marker-line\\n' >> \"$1\"\nexit 0\n",
    );
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 100).unwrap();
    pty.read_for(Duration::from_millis(400));
    let mut misses: Vec<String> = Vec::new();

    let declined_at = create_via_modal(&mut pty, ".role declined-role", b"n", "the declined role");
    let declined = files_named(&dir, "declined-role");
    if !declined.is_empty() {
        misses.push(format!(
            "`n` at the create-role confirm still created {declined:?}:\n{}",
            since(&pty, declined_at)
        ));
    }
    if since(&pty, declined_at).contains("\u{1b}[?1049l") {
        misses.push("`n` lent the terminal to an editor".to_string());
    }
    clear_any_draft(&mut pty);

    let create_at = create_via_modal(&mut pty, ".role fresh-role", b"y", "the role editor");
    let created = files_named(&dir, "fresh-role");
    match created.iter().find(|p| p.is_file()) {
        None => misses.push(format!(
            "`.role fresh-role` + `y` + editor wrote no role file (found {created:?}):\n{}",
            since(&pty, create_at)
        )),
        Some(file) => {
            let body = fs::read_to_string(file).unwrap_or_default();
            if !body.contains("role-marker-line") {
                misses.push(format!(
                    "the role file {} does not hold the editor's line: {body:?}",
                    file.display()
                ));
            }
        }
    }
    let mut toggle = false;
    let frame = next_frame(&mut pty, &mut toggle);
    let reports = editor_report_lines(&frame);
    if !reports.is_empty() {
        misses.push(format!(
            "a clean (exit 0) role editor was reported: {reports:?}\n{frame}"
        ));
    }
    let settled_at = pty.raw().len();
    clear_any_draft(&mut pty);
    assert_composer_takes_keys(&mut pty, settled_at, "the role editor");
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} role-creation expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}
