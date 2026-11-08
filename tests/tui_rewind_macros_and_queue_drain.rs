//! Black-box pins for the bottom pane: an interrupt takes queued lines
//! off the turn's queue, a line the model reads mid-turn gets its
//! transcript cell, the rewind picker stops at a non-isolated
//! macro's row, the palette rebuilt after an editor shortens the line
//! (Tab and Enter), and the vi layer's cursor clamp at the end of a line.
//! The built binary on a pseudo-terminal, a scripted fake model where a
//! turn has to be held open. Unix-only and serial like its sibling pty
//! suites.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process};

use common::fake_llm::{FakeLlm, Reply};
use common::pty::{Pty, strip_ansi};
use serial_test::serial;

const PTY_DEADLINE: Duration = Duration::from_secs(20);
/// Past both idle gates (1 s from the last keystroke, 1 s from a modal's
/// appearance).
const HOTKEY_GAP: Duration = Duration::from_millis(1200);
const REPAINT_SETTLE: Duration = Duration::from_millis(500);
const ENTER_ALT: &[u8] = b"\x1b[?1049h";
const LEAVE_ALT: &[u8] = b"\x1b[?1049l";
const PASTE_ON: &[u8] = b"\x1b[?2004h";
const MACRO_STOP_NOTICE: &str = "Rewind stops at the last macro turn";

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
        "coyote-drain-rewind-{scenario}-{}-{unique}-{sequence}",
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

fn write_fake_llm_config(dir: &Path, fake: &FakeLlm) {
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
             memory: false\n",
            vault_pass = vault_pass.display(),
            api_base = fake.api_base(),
        ),
    )
    .unwrap();
}

fn write_tool(dir: &Path, file: &str, body: &str) {
    let tools = dir.join("functions").join("tools");
    fs::create_dir_all(&tools).unwrap();
    fs::write(tools.join(file), body).unwrap();
}

/// A non-isolated macro of two plain prompts: two exchanges recorded on
/// the live session under one transcript row.
fn write_two_prompt_macro(dir: &Path, name: &str) {
    let macros = dir.join("macros");
    fs::create_dir_all(&macros).unwrap();
    fs::write(
        macros.join(format!("{name}.yaml")),
        "description: two prompts on the live session\n\
         isolated: false\n\
         steps:\n\
         \x20 - macro-prompt-one\n\
         \x20 - macro-prompt-two\n",
    )
    .unwrap();
}

/// Marks `<marker>-started`, then waits (at most eight seconds) for
/// `<marker>-release` before finishing.
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
    \x20   echo \"fixture slept\" >> \"$LLM_OUTPUT\"\n\
    }\n";

fn sleep_call(marker: &Path) -> Reply {
    Reply::ToolCall {
        name: "fixture_sleep".to_string(),
        arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
    }
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

/// Alternates the width (101/100) so every call is a real resize from
/// the 100-column terminal the pty starts on, and returns the frame.
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

fn wait_for_file(pty: &mut Pty, path: &Path, what: &str) {
    let deadline = Instant::now() + PTY_DEADLINE;
    while !path.exists() {
        let text = pty.read_for(Duration::from_millis(100));
        assert!(
            Instant::now() < deadline,
            "{what} ({}) never appeared:\n{}",
            path.display(),
            text
        );
    }
}

/// Lend-and-return: the alternate screen is left after `from`, then
/// re-entered with bracketed paste back on; returns the re-entry offset.
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

fn quit_and_check(pty: &mut Pty) -> String {
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    transcript
}

/// Quit from a composer that may hold a draft: Ctrl+C clears a draft (or
/// quits an idle empty composer); Ctrl+D follows if still up.
fn quit_clearing_any_draft(pty: &mut Pty) -> String {
    pty.send(b"\x03").unwrap();
    let after = pty.read_for(HOTKEY_GAP);
    if !after.contains("tty-probe-done") {
        pty.send(b"\x04").unwrap();
    }
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    transcript
}

/// Repaints with Ctrl+L until the `pending:` strip is gone (a queued line
/// still running) or the deadline passes.
fn wait_until_no_pending(pty: &mut Pty) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        pty.read_for(Duration::from_millis(300));
        let repaint_at = pty.raw().len();
        pty.send(b"\x0c").unwrap();
        pty.read_for(Duration::from_millis(500));
        let repainted = since(pty, repaint_at);
        if !repainted.contains("pending:") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the queue strip outlived the turn:\n{repainted}"
        );
    }
}

/// Sends one key and lets the parser see it alone (Esc must not be
/// glued to the next byte).
fn key(pty: &mut Pty, bytes: &[u8]) {
    pty.send(bytes).unwrap();
    pty.read_for(Duration::from_millis(250));
}

/// The composer's text in a repainted frame: what sits between the
/// field's rule and the footer, runs of blanks folded to one space.
fn composer_row(frame: &str) -> String {
    let rule = frame
        .rfind('\u{2500}')
        .map_or(0, |at| at + '\u{2500}'.len_utf8());
    let after = &frame[rule..];
    let footer = after.find("dryrun:dry-model").unwrap_or(after.len());
    after[..footer]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Writes an `$EDITOR` that replaces the buffer it is handed with `text`
/// (an empty `text` empties the buffer) and returns the editor's path.
fn rewriting_editor(dir: &Path, text: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let editor = dir.join("rewriting-editor.sh");
    let body = if text.is_empty() {
        "#!/bin/sh\n: > \"$1\"\nexit 0\n".to_string()
    } else {
        format!("#!/bin/sh\nprintf '%s\\n' '{text}' > \"$1\"\nexit 0\n")
    };
    fs::write(&editor, body).unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    editor.to_str().unwrap().to_string()
}

/// Esc Esc on the idle empty composer; returns the offset the picker was
/// asked at, with the band on screen.
fn open_rewind_picker(pty: &mut Pty) -> usize {
    let picker_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(150));
    pty.send(b"\x1b").unwrap();
    pty.wait_for_after("Rewind to", picker_at, PTY_DEADLINE)
        .expect("Esc Esc did not open the rewind picker");
    pty.read_for(HOTKEY_GAP);
    picker_at
}

// ---------------------------------------------------------------------
// An interrupt takes the queued lines off the turn's queue too.
// ---------------------------------------------------------------------

/// A line queued while a tool sleeps, then Esc: the turn is interrupted,
/// the queue strip goes, and the line is neither sent as the next prompt
/// nor left on the turn's queue. The next turn's own queued line still
/// rides its tool round. Regression target: the interrupt dropped the
/// preview but left the `UserMessage` on the turn queue, and the stale
/// entry made the reconcile walk drop the line queued in the next turn.
#[test]
#[serial]
fn esc_during_a_tool_round_drops_the_queued_line_from_the_turn_queue_too() {
    let dir = fresh_config_dir("esc-drains-queue");
    let _guard = TempDirGuard(dir.clone());
    let tool_a = dir.join("tool-a");
    let tool_b = dir.join("tool-b");
    let fake = FakeLlm::start(vec![
        sleep_call(&tool_a),
        sleep_call(&tool_b),
        Reply::Text("done-two".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    // Turn 1: the tool sleeps; one line is queued; Esc interrupts.
    pty.send(b"first-turn\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-a-started"),
        "the first sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    pty.send(b"ghost-line\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = repainted_frame(&mut pty, 40, 101);
    assert!(
        squeezed(&frame).contains("pending:1"),
        "the line queued while the tool slept did not show `pending: 1`:\n{frame}"
    );

    let esc_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pty.read_for(Duration::from_millis(200));
        let text = since(&pty, esc_at).to_lowercase();
        // A tool round ends with the tool aborted; a held stream with the
        // interrupted marker.
        if text.contains("interrupted") || text.contains("aborted") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no cell said the turn was interrupted or the tool aborted after Esc:\n{}",
            since(&pty, esc_at)
        );
    }
    // Long enough for a wrongly surviving queued line to open a turn.
    pty.read_for(Duration::from_millis(2500));
    let frame = repainted_frame(&mut pty, 40, 100);
    assert!(
        !frame.contains("pending:"),
        "the queue strip survived the interrupt:\n{frame}"
    );
    assert_eq!(
        fake.served(),
        1,
        "the interrupted turn's queued line opened a request of its own:\n{:#?}",
        fake.bodies()
    );

    // Turn 2: the tool sleeps again; the new queued line must ride the
    // tool round and the stale one must stay gone.
    let second_at = pty.raw().len();
    pty.send(b"second-turn\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-b-started"),
        "the second sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    pty.send(b"live-line\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = repainted_frame(&mut pty, 40, 101);
    assert!(
        squeezed(&frame).contains("pending:1"),
        "the line queued in the second turn did not show `pending: 1`:\n{frame}"
    );
    fs::write(dir.join("tool-b-release"), b"").unwrap();
    wait_for_squeezed_after(&mut pty, "done-two", second_at, "the second turn's reply");
    wait_until_no_pending(&mut pty);

    let bodies = fake.bodies();
    assert!(
        bodies.iter().all(|b| !b.contains("ghost-line")),
        "the line queued before the interrupt reached the model:\n{}",
        bodies.join("\n---\n")
    );
    assert_eq!(
        bodies.len(),
        3,
        "unexpected request count (the interrupted turn's line opened a turn, or the \
         second turn's line became a prompt of its own):\n{}",
        bodies.join("\n---\n")
    );
    assert!(
        bodies[2].contains("live-line"),
        "the line queued in the second turn did not ride its tool round:\n{}",
        bodies[2]
    );
    quit_and_check(&mut pty);
}

/// Esc while a tool runs, with the tool allowed to finish on its own in
/// the same instant: the interrupt still ends the turn. No second
/// request goes out, the scripted reply behind it never streams, and the
/// composer takes keys. Esc with a turn running is the interrupt; the
/// tool's own exit status must not resume the turn.
#[test]
#[serial]
fn esc_during_a_tool_round_ends_the_turn_even_when_the_tool_finishes_on_its_own() {
    let dir = fresh_config_dir("esc-tool-finishes");
    let _guard = TempDirGuard(dir.clone());
    let tool_a = dir.join("tool-a");
    let fake = FakeLlm::start(vec![
        sleep_call(&tool_a),
        Reply::Text("never-streamed".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    pty.send(b"first-turn\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-a-started"),
        "the sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    let esc_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    fs::write(dir.join("tool-a-release"), b"").unwrap();
    // Long enough for the tool to exit, the kill to land, and a wrongly
    // resumed turn to open its next request.
    pty.read_for(Duration::from_secs(6));
    let after = since(&pty, esc_at);
    assert_eq!(
        fake.served(),
        1,
        "Esc did not end the turn: the tool's own exit resumed it into a second request \
         (tool status in the transcript: {}):\n{after}",
        if after.contains("aborted") {
            "aborted"
        } else if after.contains("ok") {
            "ok"
        } else {
            "unknown"
        }
    );
    assert!(
        !squeezed(&after).contains("never-streamed"),
        "the reply scripted behind the interrupted tool round streamed:\n{after}"
    );
    let settled_at = pty.raw().len();
    assert_composer_takes_keys(&mut pty, settled_at, "Esc during the tool round");
    quit_and_check(&mut pty);
}

/// A line queued during a tool round rides the next request and, at
/// that moment, is committed to the transcript as a dim cell with its
/// marker, so what the model read is on record once the strip's preview
/// is gone. Under NO_COLOR the marker is the only distinction left.
#[test]
#[serial]
fn a_line_sent_mid_turn_is_committed_to_the_transcript_when_the_model_reads_it() {
    let dir = fresh_config_dir("mid-turn-cell");
    let _guard = TempDirGuard(dir.clone());
    let tool_a = dir.join("tool-a");
    let fake = FakeLlm::start(vec![
        sleep_call(&tool_a),
        Reply::Text("done-one".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[("NO_COLOR", "1")]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    let turn_at = pty.raw().len();
    pty.send(b"first-turn\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-a-started"),
        "the sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    pty.send(b"follow-up k4e-m\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = repainted_frame(&mut pty, 40, 101);
    assert!(
        squeezed(&frame).contains("pending:1"),
        "the line queued while the tool slept did not show `pending: 1`:\n{frame}"
    );
    assert!(
        !squeezed(&frame).contains("(sentmid-turn)"),
        "the cell was committed before the model read the line:\n{frame}"
    );

    fs::write(dir.join("tool-a-release"), b"").unwrap();
    wait_for_squeezed_after(&mut pty, "done-one", turn_at, "the turn's reply");
    wait_until_no_pending(&mut pty);

    let bodies = fake.bodies();
    assert_eq!(bodies.len(), 2, "{}", bodies.join("\n---\n"));
    assert!(
        bodies[1].contains("follow-up k4e-m"),
        "the queued line did not ride the tool round:\n{}",
        bodies[1]
    );
    let frame = repainted_frame(&mut pty, 40, 100);
    assert!(
        squeezed(&frame).contains(">follow-upk4e-m(sentmid-turn)"),
        "the transcript has no mid-turn cell for the line the model read:\n{frame}"
    );
    assert!(
        !frame.contains("pending:"),
        "the queue strip outlived the line it previewed:\n{frame}"
    );
    quit_and_check(&mut pty);
}

// ---------------------------------------------------------------------
// The rewind picker stops at a non-isolated macro's row.
// ---------------------------------------------------------------------

/// The two ways a user runs a macro from the composer.
const MACRO_INVOCATIONS: [&str; 2] = [".macro two-step", ".two-step"];

/// The placeholder is on screen: the composer holds no draft.
fn shows_placeholder(flat: &str) -> bool {
    flat.contains("Typeapromptora.command") || flat.contains("Typeapromptora/command")
}

/// Clears a prefilled draft without risking the idle-empty Ctrl+C quit.
/// The frame is read after a real resize (a width the tests never park
/// on, alternating so two calls in a row both repaint).
fn clear_any_draft(pty: &mut Pty) {
    static TOGGLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let odd = TOGGLE.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
    let frame = repainted_frame(pty, 60, if odd { 98 } else { 99 });
    if !shows_placeholder(&squeezed(&frame)) {
        pty.send(b"\x03").unwrap();
        pty.read_for(Duration::from_millis(500));
    }
}

/// The picker's band below its header in a repainted frame.
fn picker_band(frame: &str) -> String {
    frame.split("Rewind to").nth(1).unwrap_or("").to_string()
}

/// Without a session the transcript is the whole of the rewind: after a
/// two-prompt macro and a plain exchange, the picker offers the plain
/// exchange and nothing at or below the macro, says it stops at the
/// macro, and choosing the row pops only that exchange and prefills it;
/// the macro's exchanges stay and the UI is live afterwards. Both macro
/// invocations are one macro turn.
#[test]
#[serial]
fn the_rewind_picker_stops_at_a_macro_row_with_a_notice_and_pops_the_newer_exchange() {
    let mut misses: Vec<String> = Vec::new();
    for invocation in MACRO_INVOCATIONS {
        let dir = fresh_config_dir("rewind-macro-dry");
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir, "");
        write_two_prompt_macro(&dir, "two-step");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));

        let macro_at = pty.raw().len();
        pty.send(format!("{invocation}\r").as_bytes()).unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "macro-prompt-two",
            macro_at,
            "the macro's second step",
        );
        pty.read_for(Duration::from_millis(2500));
        let after_at = pty.raw().len();
        pty.send(b"after-macro\r").unwrap();
        wait_for_squeezed_after(&mut pty, "> after-macro", after_at, "the plain exchange");
        pty.read_for(Duration::from_millis(2500));

        let picker_at = open_rewind_picker(&mut pty);
        let picker = repainted_frame(&mut pty, 60, 101);
        let band_flat = squeezed(&picker_band(&picker));
        if !band_flat.contains("after-macro") {
            misses.push(format!(
                "`{invocation}`: the exchange above the macro is not offered:\n{picker}"
            ));
        }
        if band_flat.contains("macro-prompt-one") || band_flat.contains("macro-prompt-two") {
            misses.push(format!(
                "`{invocation}`: the picker offers a prompt the macro ran as a row of its \
                 own:\n{picker}"
            ));
        }
        if band_flat.contains("two-step") {
            misses.push(format!(
                "`{invocation}`: the macro turn is listed as a rewind row instead of \
                 stopping the list:\n{picker}"
            ));
        }
        if !squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
            misses.push(format!(
                "`{invocation}`: opening the picker over a macro turn gave no notice that the \
                 list stops at the macro:\n{}",
                since(&pty, picker_at)
            ));
        }

        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let frame = repainted_frame(&mut pty, 60, 100);
        let flat = squeezed(&frame);
        if flat.contains("Rewindto") {
            misses.push(format!(
                "`{invocation}`: the picker is still up after Enter:\n{frame}"
            ));
        }
        if flat.contains(">after-macro") {
            misses.push(format!(
                "`{invocation}`: the chosen exchange was not popped:\n{frame}"
            ));
        }
        if !flat.contains("after-macro") {
            misses.push(format!(
                "`{invocation}`: the chosen prompt was not prefilled into the composer:\n{frame}"
            ));
        }
        if !flat.contains("macro-prompt-two") {
            misses.push(format!(
                "`{invocation}`: popping the plain exchange took the macro's exchanges with \
                 it:\n{frame}"
            ));
        }

        // Clear whatever was prefilled and prove the UI is live.
        clear_any_draft(&mut pty);
        let alive_at = pty.raw().len();
        pty.send(b"still-alive\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "> still-alive",
            alive_at,
            "the exchange after the rewind",
        );
        pty.read_for(Duration::from_millis(1000));
        quit_clearing_any_draft(&mut pty);
    }
    assert!(
        misses.is_empty(),
        "{} rewind-picker expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

/// Exit from a composer that may hold a draft and a session that may be
/// dirty: the draft is cleared, and a save question is answered no.
fn quit_declining_save(pty: &mut Pty) -> String {
    clear_any_draft(pty);
    pty.send(b"\x04").unwrap();
    let deadline = Instant::now() + PTY_DEADLINE;
    loop {
        let text = pty.read_for(Duration::from_millis(300));
        if text.contains("tty-probe-done") {
            assert_eq!(exit_code(&text), 0, "{text}");
            return text;
        }
        if text.contains("Save session?") {
            pty.read_for(HOTKEY_GAP);
            pty.send(b"n").unwrap();
        }
        assert!(
            Instant::now() < deadline,
            "the session did not end:\n{text}"
        );
    }
}

/// With a session the transcript and the session must stay in step
/// across a rewind over a non-isolated macro's row: after `old`, the
/// two-prompt macro and `new`, the picker offers `new` alone (the macro's
/// one row stands for two exchanges, so nothing at or below it can be
/// counted), Down cannot reach past it, and Enter pops `new` only. The
/// prefill is cleared and a fresh prompt is sent; every earlier reply the
/// transcript still shows is in the request, and the popped one is not.
/// Run for both ways of invoking the macro.
#[test]
#[serial]
fn rewinding_past_a_macro_row_keeps_the_session_in_step_with_the_transcript() {
    let mut misses: Vec<String> = Vec::new();
    for invocation in MACRO_INVOCATIONS {
        let dir = fresh_config_dir("rewind-macro-session");
        let _guard = TempDirGuard(dir.clone());
        let fake = FakeLlm::start(vec![
            Reply::Text("reply-old".to_string()),
            Reply::Text("reply-mone".to_string()),
            Reply::Text("reply-mtwo".to_string()),
            Reply::Text("reply-new".to_string()),
            Reply::Text("reply-after".to_string()),
        ]);
        write_fake_llm_config(&dir, &fake);
        write_two_prompt_macro(&dir, "two-step");
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.resize(60, 100).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b".session\r").unwrap();
        pty.wait_for("temp", PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(500));

        let at = pty.raw().len();
        pty.send(b"old-prompt\r").unwrap();
        wait_for_squeezed_after(&mut pty, "reply-old", at, "the first reply");
        pty.read_for(Duration::from_millis(1500));
        let at = pty.raw().len();
        pty.send(format!("{invocation}\r").as_bytes()).unwrap();
        wait_for_squeezed_after(&mut pty, "reply-mtwo", at, "the macro's second reply");
        pty.read_for(Duration::from_millis(2500));
        let at = pty.raw().len();
        pty.send(b"new-prompt\r").unwrap();
        wait_for_squeezed_after(&mut pty, "reply-new", at, "the newest reply");
        pty.read_for(Duration::from_millis(2500));
        assert_eq!(
            fake.served(),
            4,
            "{invocation}: request count before the rewind"
        );

        let picker_at = open_rewind_picker(&mut pty);
        let picker = repainted_frame(&mut pty, 60, 101);
        let band_flat = squeezed(&picker_band(&picker));
        if !band_flat.contains("new-prompt") {
            misses.push(format!(
                "`{invocation}`: the exchange above the macro is not offered:\n{picker}"
            ));
        }
        if band_flat.contains("old-prompt") || band_flat.contains("two-step") {
            misses.push(format!(
                "`{invocation}`: the picker offers a row at or below the macro, whose one \
                 row stands for two exchanges:\n{picker}"
            ));
        }
        if !squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
            misses.push(format!(
                "`{invocation}`: opening the picker over a macro turn gave no notice that the \
                 list stops at the macro:\n{}",
                since(&pty, picker_at)
            ));
        }
        pty.send(b"\x1b[B").unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\r").unwrap();
        pty.read_for(Duration::from_millis(1500));
        let frame = repainted_frame(&mut pty, 60, 100);
        let flat = squeezed(&frame);
        if flat.contains("Rewindto") {
            misses.push(format!(
                "`{invocation}`: the picker is still up after Enter:\n{frame}"
            ));
        }
        if flat.contains("reply-new") {
            misses.push(format!(
                "`{invocation}`: the newest exchange survived the rewind:\n{frame}"
            ));
        }
        let kept = ["reply-old", "reply-mone", "reply-mtwo"];
        let shown: Vec<&str> = kept
            .into_iter()
            .filter(|marker| flat.contains(marker))
            .collect();
        if shown != kept {
            misses.push(format!(
                "`{invocation}`: Down then Enter reached past the macro row; the transcript \
                 kept only {shown:?}:\n{frame}"
            ));
        }

        clear_any_draft(&mut pty);
        let after_at = pty.raw().len();
        pty.send(b"after-rewind\r").unwrap();
        wait_for_squeezed_after(
            &mut pty,
            "reply-after",
            after_at,
            "the reply after the rewind",
        );
        pty.read_for(Duration::from_millis(800));
        let bodies = fake.bodies();
        let last = bodies.last().unwrap().clone();
        assert!(
            last.contains("after-rewind"),
            "{invocation}: the prompt after the rewind is not the last request:\n{last}"
        );
        if last.contains("reply-new") {
            misses.push(format!(
                "`{invocation}`: the popped newest exchange is still in the session:\n{last}"
            ));
        }
        for marker in kept {
            let on_screen = shown.contains(&marker);
            let in_session = last.contains(marker);
            if on_screen != in_session {
                misses.push(format!(
                    "`{invocation}`: after the rewind `{marker}` is {} the transcript but {} \
                     the session's next request\n--- picker ---\n{picker}\n\
                     --- frame after the rewind ---\n{frame}\n--- next request ---\n{last}",
                    if on_screen { "in" } else { "gone from" },
                    if in_session { "in" } else { "gone from" },
                ));
            }
        }
        quit_declining_save(&mut pty);
    }
    assert!(
        misses.is_empty(),
        "{} session/transcript expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// A palette left over from a longer line cannot panic on Tab or Enter.
// ---------------------------------------------------------------------

/// `.sess` Tab (row highlighted), Ctrl+O to an editor that shortens the
/// line to `.i`, then Tab and Enter: the UI stays up both times, twice in
/// one session, and a fresh `.` still opens the palette afterwards.
#[test]
#[serial]
fn tab_and_enter_on_a_palette_outlived_by_an_editor_shortened_line_do_not_crash_twice() {
    let dir = fresh_config_dir("stale-palette-tab");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let editor = rewriting_editor(&dir, ".i");
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    for (pass, typed) in [(1, ".sess"), (2, ".in")] {
        pty.send(typed.as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(400));
        pty.send(b"\t").unwrap();
        pty.read_for(HOTKEY_GAP);
        let edit_at = pty.raw().len();
        pty.send(b"\x0f").unwrap();
        wait_for_lend_and_return(
            &mut pty,
            edit_at,
            &format!("Ctrl+O under the palette (pass {pass})"),
        );
        pty.send(b"\t").unwrap();
        let after_tab = pty.read_for(Duration::from_millis(800));
        assert!(
            !after_tab.contains("exit="),
            "pass {pass}: Tab on the palette after the editor shrank the line ended the \
             session:\n{after_tab}"
        );
        pty.send(b"\r").unwrap();
        let after_enter = pty.read_for(HOTKEY_GAP);
        assert!(
            !after_enter.contains("exit="),
            "pass {pass}: Enter after Tab on the stale palette ended the session:\n{after_enter}"
        );
        // Whatever Tab/Enter did with `.i`, the composer must take keys.
        // A draft may be left (Tab completed to a command); clear it
        // first with End + backspaces rather than Ctrl+C, which would quit
        // an empty idle composer.
        pty.send(b"\x1b[F").unwrap();
        pty.read_for(Duration::from_millis(200));
        for _ in 0..24 {
            pty.send(b"\x7f").unwrap();
        }
        pty.read_for(Duration::from_millis(400));
        let settled_at = pty.raw().len();
        assert_composer_takes_keys(&mut pty, settled_at, &format!("pass {pass}"));
    }

    // The palette state was rebuilt, not disabled: `.` at column 0 opens it.
    pty.send(b".").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = repainted_frame(&mut pty, 40, 101);
    let flat = squeezed(&frame);
    assert!(
        flat.contains("Showsysteminfo") && flat.contains("more)"),
        "after two editor passes `.` no longer opens the palette:\n{frame}"
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    quit_clearing_any_draft(&mut pty);
}

/// The editor empties the line under an open palette: Tab, then Enter on
/// the empty composer, neither crashes; the placeholder is back.
#[test]
#[serial]
fn tab_and_enter_after_an_editor_empties_the_line_under_a_palette_do_not_crash() {
    let dir = fresh_config_dir("stale-palette-empty");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let editor = rewriting_editor(&dir, "");
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    pty.send(b".in").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\t").unwrap();
    pty.read_for(HOTKEY_GAP);
    let edit_at = pty.raw().len();
    pty.send(b"\x0f").unwrap();
    wait_for_lend_and_return(&mut pty, edit_at, "Ctrl+O under the palette");
    pty.send(b"\t").unwrap();
    let after_tab = pty.read_for(Duration::from_millis(800));
    assert!(
        !after_tab.contains("exit="),
        "Tab on the palette after the editor emptied the line ended the session:\n{after_tab}"
    );
    pty.send(b"\r").unwrap();
    let after_enter = pty.read_for(HOTKEY_GAP);
    assert!(
        !after_enter.contains("exit="),
        "Enter on the emptied composer under the stale palette ended the session:\n{after_enter}"
    );
    let frame = repainted_frame(&mut pty, 40, 101);
    let flat = squeezed(&frame);
    assert!(
        flat.contains("Typeapromptora.command") || !flat.contains(".in"),
        "the emptied line came back as a draft:\n{frame}"
    );
    let settled_at = pty.raw().len();
    assert_composer_takes_keys(&mut pty, settled_at, "the emptied palette line");
    quit_clearing_any_draft(&mut pty);
}

// ---------------------------------------------------------------------
// vi normal mode clamps the cursor to the last character.
// ---------------------------------------------------------------------

/// `$` lands ON the last character and `l` cannot walk past it, so `x`
/// deletes the last character each time, `i` inserts before it and `a`
/// after it; `A` still appends; Enter submits exactly the edited text.
/// Every step is observed before the verdict.
#[test]
#[serial]
fn vi_normal_mode_keeps_the_cursor_on_the_last_character_at_the_end_of_a_line() {
    let dir = fresh_config_dir("vi-eol-clamp");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "keybindings: vi\n");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let mut misses: Vec<String> = Vec::new();
    let mut toggle = false;
    let mut check = |pty: &mut Pty, toggle: &mut bool, want: &str, step: &str| {
        let frame = next_frame(pty, toggle);
        let got = composer_row(&frame);
        if got != want {
            misses.push(format!("{step}: expected `{want}`, got `{got}`:\n{frame}"));
        }
    };

    pty.send(b"hello world").unwrap();
    pty.read_for(Duration::from_millis(400));
    key(&mut pty, b"\x1b");
    key(&mut pty, b"$");
    key(&mut pty, b"x");
    check(&mut pty, &mut toggle, "hello worl", "$ x");
    key(&mut pty, b"x");
    check(
        &mut pty,
        &mut toggle,
        "hello wor",
        "x again at the end (clamp re-applied)",
    );
    key(&mut pty, b"$");
    key(&mut pty, b"l");
    key(&mut pty, b"l");
    key(&mut pty, b"x");
    check(
        &mut pty,
        &mut toggle,
        "hello wo",
        "$ l l x (l must not pass the last char)",
    );
    key(&mut pty, b"$");
    key(&mut pty, b"i");
    key(&mut pty, b"!");
    key(&mut pty, b"\x1b");
    check(
        &mut pty,
        &mut toggle,
        "hello w!o",
        "$ i ! (insert before the last char)",
    );
    key(&mut pty, b"$");
    key(&mut pty, b"a");
    key(&mut pty, b"Z");
    key(&mut pty, b"\x1b");
    check(
        &mut pty,
        &mut toggle,
        "hello w!oZ",
        "$ a Z (append after the last char)",
    );
    key(&mut pty, b"A");
    key(&mut pty, b"Q");
    check(&mut pty, &mut toggle, "hello w!oZQ", "A Q");

    let submit_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    let deadline = Instant::now() + PTY_DEADLINE;
    loop {
        pty.read_for(Duration::from_millis(200));
        if squeezed(&since(&pty, submit_at)).contains(">hellow!oZQ") {
            break;
        }
        if Instant::now() >= deadline {
            misses.push(format!(
                "Enter did not submit `hello w!oZQ` as the prompt:\n{}",
                since(&pty, submit_at)
            ));
            break;
        }
    }
    pty.read_for(Duration::from_millis(1000));
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} vi clamp step(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}
