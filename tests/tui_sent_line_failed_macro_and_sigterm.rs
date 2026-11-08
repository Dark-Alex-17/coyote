//! Black-box pins for the bottom pane's sent-line, failed-macro and
//! SIGTERM edges: a queued line the model received leaves the `pending:`
//! strip while the SAME turn still runs and Alt+Up never brings it back;
//! a non-isolated macro that recorded an exchange before a later step
//! failed is still a rewind barrier (one that failed before any exchange
//! is not); SIGTERM with queued lines drops them even when the tool is
//! released in the same instant. The built binary on a pseudo-terminal,
//! a scripted fake model where a tool round or a session is needed.
//! Unix-only and serial like its sibling pty suites.
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
const MACRO_STOP_NOTICE: &str = "Rewind stops at the last macro turn";
const MID_TURN_MARKER: &str = "(sent mid-turn)";
/// xterm's legacy encoding of Alt+Up.
const ALT_UP: &[u8] = b"\x1b[1;3A";
const DOWN: &[u8] = b"\x1b[B";
const CTRL_C: &[u8] = b"\x03";
const CTRL_D: &[u8] = b"\x04";
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
        "coyote-tui-w3-{scenario}-{}-{unique}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
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

/// A non-isolated macro whose steps run on the live session.
fn write_macro(dir: &Path, name: &str, steps: &[&str]) {
    let macros = dir.join("macros");
    fs::create_dir_all(&macros).unwrap();
    let mut body = String::from("description: a live-session macro\nisolated: false\nsteps:\n");
    for step in steps {
        body.push_str(&format!("  - {step}\n"));
    }
    fs::write(macros.join(format!("{name}.yaml")), body).unwrap();
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

fn child_pid(transcript: &str) -> i32 {
    transcript
        .lines()
        .find_map(|line| line.strip_prefix("cpid="))
        .unwrap_or_else(|| panic!("no cpid= line in the transcript:\n{transcript}"))
        .trim()
        .parse()
        .unwrap()
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
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

/// Pumps until a Ctrl+L repaint shows no `pending:` strip.
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

/// What sits above the composer's rule in a repainted frame: transcript
/// rows and, while a turn runs, the status strip with the queue preview.
fn above_rule(frame: &str) -> String {
    let rule = frame.rfind('\u{2500}').unwrap_or(frame.len());
    frame[..rule].to_string()
}

/// The placeholder is on screen: the composer holds no draft.
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
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(150));
    pty.send(b"\x1b").unwrap();
    pty.wait_for_after("Rewind to", picker_at, PTY_DEADLINE)
        .expect("Esc Esc did not open the rewind picker");
    pty.read_for(HOTKEY_GAP);
    picker_at
}

/// The picker's band below its header in a repainted frame.
fn picker_band(frame: &str) -> String {
    frame.split("Rewind to").nth(1).unwrap_or("").to_string()
}

/// A frame with the picker on it: a forced repaint at `cols`, or what
/// the picker painted when it opened if the resize was coalesced away.
fn picker_frame(pty: &mut Pty, picker_at: usize, cols: u16) -> String {
    let frame = repainted_frame(pty, 60, cols);
    if frame.contains("Rewind to") {
        frame
    } else {
        since(pty, picker_at)
    }
}

/// Runs an exchange against the fake model and waits for its reply.
fn fake_exchange(pty: &mut Pty, prompt: &str, reply: &str) {
    let at = pty.raw().len();
    pty.send(format!("{prompt}\r").as_bytes()).unwrap();
    wait_for_squeezed_after(pty, reply, at, &format!("the reply to `{prompt}`"));
    pty.read_for(Duration::from_millis(1200));
}

/// `.session <name>` on the idle composer. After an exchange the REPL
/// asks whether the new session should incorporate the last question and
/// answer: that Confirm is answered `n` past the idle gate. Returns once
/// a fully repainted footer carries the name.
fn start_session(pty: &mut Pty, name: &str) {
    static TOGGLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let at = pty.raw().len();
    pty.send(format!(".session {name}\r").as_bytes()).unwrap();
    let deadline = Instant::now() + PTY_DEADLINE;
    let question = squeezed("incorporates the last question and answer?");
    let token = squeezed(&format!("{name} . agents"));
    let mut answered = false;
    loop {
        pty.read_for(Duration::from_millis(300));
        let text = squeezed(&since(pty, at));
        if !answered && text.contains(&question) {
            pty.read_for(HOTKEY_GAP);
            pty.send(b"n").unwrap();
            answered = true;
            pty.read_for(Duration::from_millis(800));
        }
        let odd = TOGGLE.fetch_xor(true, std::sync::atomic::Ordering::Relaxed);
        let frame = repainted_frame(pty, 60, if odd { 100 } else { 101 });
        let flat = squeezed(&frame);
        if flat.contains(&token) {
            assert!(
                !flat.contains(&question) && shows_placeholder(&flat),
                "the session question is still up or the composer is not idle after `.session {name}`:\n{frame}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the footer's `{name}` session token never appeared:\n{}",
            since(pty, at)
        );
    }
    pty.read_for(Duration::from_millis(400));
}

/// Exit from a composer that may hold a draft and a session that may be
/// dirty: the draft is cleared, and a save question is answered no.
fn quit_declining_save(pty: &mut Pty) -> String {
    clear_any_draft(pty);
    pty.send(CTRL_D).unwrap();
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

// ---------------------------------------------------------------------
// A line the model received leaves the strip while the turn still runs,
// and Alt+Up never brings it back.
// ---------------------------------------------------------------------

/// Tool A sleeps: `sent-one`, `sent-two` are queued (`pending: 2`).
/// Releasing A hands both to the model at the tool-round boundary; while
/// tool B sleeps in the SAME turn the strip is gone and both lines are
/// dim `(sent mid-turn)` cells. Alt+Up on the empty composer brings
/// nothing back. A later `late-three` is queued, Alt+Up pulls THAT line
/// (not a sent one) into the composer, Enter re-queues it, and releasing
/// B sends it; three requests in all.
#[test]
#[serial]
fn a_sent_line_leaves_the_strip_mid_turn_and_alt_up_never_pops_it() {
    let dir = fresh_config_dir("sent-strip");
    let _guard = TempDirGuard(dir.clone());
    let tool_a = dir.join("tool-a");
    let tool_b = dir.join("tool-b");
    let fake = FakeLlm::start(vec![
        sleep_call(&tool_a),
        sleep_call(&tool_b),
        Reply::Text("done-w3s".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 100).unwrap();
    pty.read_for(Duration::from_millis(400));
    let mut toggle = false;

    let turn_at = pty.raw().len();
    pty.send(b"call the sleep tool\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-a-started"),
        "the sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    pty.send(b"sent-one-w3\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(b"sent-two-w3\r").unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    assert!(
        squeezed(&frame).contains("pending:2"),
        "two lines queued while tool A slept did not show `pending: 2`:\n{frame}"
    );

    // Release A: the queue is drained into request 2, which asks for B.
    fs::write(dir.join("tool-a-release"), b"").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-b-started"),
        "the second sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(800));

    let mut misses: Vec<String> = Vec::new();
    // The strip is gone and cells committed while B still sleeps.
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    let marker = squeezed(MID_TURN_MARKER);
    for line in ["sent-one-w3", "sent-two-w3"] {
        if !flat.contains(&format!(">{line}{marker}")) {
            misses.push(format!(
                "`{line}` was handed to the model but has no `(sent mid-turn)` cell while the \
                 turn still runs:\n{frame}"
            ));
        }
    }
    if frame.contains("pending:") {
        misses.push(format!(
            "the queue strip outlived the lines it previewed (the model has them, tool B is \
             still running):\n{frame}"
        ));
    }
    if fake.served() != 2 {
        misses.push(format!(
            "expected two requests after tool A was released, got {}",
            fake.served()
        ));
    }

    // Alt+Up on the empty composer does not resurrect a sent line.
    pty.send(ALT_UP).unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    let row = composer_row(&frame, "fake:fake-model");
    if row.contains("sent-one-w3") || row.contains("sent-two-w3") {
        misses.push(format!(
            "Alt+Up popped a line the model already received back into the composer \
             (`{row}`):\n{frame}"
        ));
    } else if !shows_placeholder(&flat) {
        misses.push(format!(
            "Alt+Up over an empty queue left the composer non-empty (`{row}`):\n{frame}"
        ));
    }
    if frame.contains("pending:") {
        misses.push(format!(
            "Alt+Up over an empty queue made a `pending:` strip appear:\n{frame}"
        ));
    }

    // A later line queues alone; Alt+Up pulls it, not a sent line.
    pty.send(b"late-three-w3\r").unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    if !flat.contains("pending:1") {
        misses.push(format!(
            "a line queued while tool B slept did not show `pending: 1`:\n{frame}"
        ));
    }
    let strip = squeezed(&above_rule(&frame));
    if !strip.contains("late-three-w3") {
        misses.push(format!(
            "the strip does not preview the one line it holds:\n{frame}"
        ));
    }
    pty.send(ALT_UP).unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    let row = composer_row(&frame, "fake:fake-model");
    if row != "late-three-w3" {
        misses.push(format!(
            "Alt+Up did not bring the one queued line back for editing (composer: `{row}`):\n{frame}"
        ));
    }
    if frame.contains("pending:") {
        misses.push(format!(
            "after Alt+Up took the only queued line the strip still shows `pending:`:\n{frame}"
        ));
    }
    if fake.served() != 2 {
        misses.push(format!(
            "Alt+Up moved the turn on ({} requests served)",
            fake.served()
        ));
    }
    // Enter re-queues it.
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    if !squeezed(&frame).contains("pending:1") {
        misses.push(format!(
            "Enter on the line Alt+Up brought back did not re-queue it:\n{frame}"
        ));
    }

    // Release B; the re-queued line rides request 3.
    fs::write(dir.join("tool-b-release"), b"").unwrap();
    wait_for_squeezed_after(&mut pty, "done-w3s", turn_at, "the turn's final reply");
    wait_until_no_pending(&mut pty);
    let bodies = fake.bodies();
    if bodies.len() != 3 {
        misses.push(format!(
            "expected three requests, got {}:\n{}",
            bodies.len(),
            bodies.join("\n---\n")
        ));
    } else {
        if !bodies[1].contains("sent-one-w3") || !bodies[1].contains("sent-two-w3") {
            misses.push(format!(
                "the two lines queued during tool A did not ride request 2:\n{}",
                bodies[1]
            ));
        }
        if !bodies[2].contains("late-three-w3") {
            misses.push(format!(
                "the line re-queued during tool B did not ride request 3:\n{}",
                bodies[2]
            ));
        }
        // The sent lines stay in the history exactly once each: a
        // resurrected line would be sent again as a new user message.
        for line in ["sent-one-w3", "sent-two-w3"] {
            let times = bodies[2].matches(line).count();
            if times != 1 {
                misses.push(format!(
                    "`{line}` appears {times} times in request 3 (sent again?):\n{}",
                    bodies[2]
                ));
            }
        }
    }
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    if !flat.contains(&format!(">late-three-w3{marker}")) {
        misses.push(format!(
            "the re-queued line has no `(sent mid-turn)` cell after the turn:\n{frame}"
        ));
    }
    if !shows_placeholder(&flat) {
        misses.push(format!(
            "the composer is not empty after the turn (composer: `{}`):\n{frame}",
            composer_row(&frame, "fake:fake-model")
        ));
    }
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} sent-line expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// A macro that recorded an exchange is a barrier even when a later step
// failed; one that failed before any exchange is not.
// ---------------------------------------------------------------------

/// `.session s7`, `before`, macro `half` = [`inside`, `.session nosuch`]
/// (the first step records an exchange, the second fails inside a
/// session), `after`. The picker offers `after` alone with the macro
/// notice; Enter pops `after` only; `inside` and `before` stay on the
/// transcript and in the next request.
#[test]
#[serial]
fn a_macro_that_recorded_an_exchange_is_a_barrier_even_when_a_later_step_failed() {
    let dir = fresh_config_dir("failed-macro-barrier");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::Text("reply-before-w3m".to_string()),
        Reply::Text("reply-inside-w3m".to_string()),
        Reply::Text("reply-after-w3m".to_string()),
        Reply::Text("reply-tail-w3m".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_macro(&dir, "half-w3", &["inside-w3m", ".session nosuch-w3"]);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    start_session(&mut pty, "s7-w3m");
    fake_exchange(&mut pty, "before-w3m", "reply-before-w3m");
    let macro_at = pty.raw().len();
    pty.send(b".half-w3\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "reply-inside-w3m",
        macro_at,
        "the macro's first step's reply",
    );
    let failed = wait_for_squeezed_after(
        &mut pty,
        "Already in a session",
        macro_at,
        "the macro's failing second step",
    );
    assert!(
        !squeezed(&failed).contains("nosuch-w3.agents"),
        "`.session nosuch-w3` inside the macro switched sessions instead of failing:\n{failed}"
    );
    pty.read_for(Duration::from_millis(1500));
    fake_exchange(&mut pty, "after-w3m", "reply-after-w3m");
    assert_eq!(fake.served(), 3, "{}", fake.bodies().join("\n---\n"));

    let mut misses: Vec<String> = Vec::new();
    let picker_at = open_rewind_picker(&mut pty);
    let picker = picker_frame(&mut pty, picker_at, 101);
    let band_flat = squeezed(&picker_band(&picker));
    if !band_flat.contains("after-w3m") {
        misses.push(format!("the newest prompt is not a rewind row:\n{picker}"));
    }
    if band_flat.contains("before-w3m") {
        misses.push(format!(
            "a macro that recorded an exchange before failing is NOT a barrier: `before` is \
             offered:\n{picker}"
        ));
    }
    if band_flat.contains("inside-w3m") {
        misses.push(format!(
            "the failed macro's own exchange is offered as a rewind row:\n{picker}"
        ));
    }
    if !squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
        misses.push(format!(
            "the picker does not say it stops at the last macro turn:\n{}",
            since(&pty, picker_at)
        ));
    }
    // Down cannot reach past the barrier; Enter pops `after` only.
    pty.send(DOWN).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let frame = repainted_frame(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    if flat.contains("Rewindto") {
        misses.push(format!("the picker is still up after Enter:\n{frame}"));
    }
    if flat.contains(">after-w3m") {
        misses.push(format!("the chosen exchange was not popped:\n{frame}"));
    }
    if !flat.contains(">inside-w3m") || !flat.contains(">before-w3m") {
        misses.push(format!(
            "the rewind went past the failed macro's barrier:\n{frame}"
        ));
    }
    let row = composer_row(&frame, "fake:fake-model");
    if row != "after-w3m" {
        misses.push(format!(
            "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
        ));
    }
    clear_any_draft(&mut pty);
    fake_exchange(&mut pty, "tail-w3m", "reply-tail-w3m");
    let bodies = fake.bodies();
    if bodies.len() != 4 {
        misses.push(format!(
            "expected four requests, got {}:\n{}",
            bodies.len(),
            bodies.join("\n---\n")
        ));
    } else {
        if bodies[3].contains("after-w3m") {
            misses.push(format!(
                "the popped exchange is still in the session's next request:\n{}",
                bodies[3]
            ));
        }
        if !bodies[3].contains("inside-w3m") || !bodies[3].contains("before-w3m") {
            misses.push(format!(
                "the session lost an exchange the transcript still shows:\n{}",
                bodies[3]
            ));
        }
    }
    quit_declining_save(&mut pty);
    assert!(
        misses.is_empty(),
        "{} failed-macro barrier expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

/// Control: `.session s7`, `before`, macro `nothing` = [`.session
/// nosuch`] (fails before any exchange), `after`. The picker offers
/// `after` AND `before`: a macro that neither succeeded nor recorded an
/// exchange cuts nothing.
#[test]
#[serial]
fn a_macro_that_failed_before_any_exchange_is_not_a_barrier() {
    let dir = fresh_config_dir("empty-failed-macro");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::Text("reply-before-w3n".to_string()),
        Reply::Text("reply-after-w3n".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_macro(&dir, "nothing-w3", &[".session nosuch-w3"]);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    start_session(&mut pty, "s7-w3n");
    fake_exchange(&mut pty, "before-w3n", "reply-before-w3n");
    let macro_at = pty.raw().len();
    pty.send(b".nothing-w3\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "Already in a session",
        macro_at,
        "the macro's failing step",
    );
    pty.read_for(Duration::from_millis(1500));
    fake_exchange(&mut pty, "after-w3n", "reply-after-w3n");
    assert_eq!(fake.served(), 2, "{}", fake.bodies().join("\n---\n"));

    let mut misses: Vec<String> = Vec::new();
    let picker_at = open_rewind_picker(&mut pty);
    let picker = picker_frame(&mut pty, picker_at, 101);
    let band_flat = squeezed(&picker_band(&picker));
    if !band_flat.contains("after-w3n") {
        misses.push(format!("the newest prompt is not a rewind row:\n{picker}"));
    }
    if !band_flat.contains("before-w3n") {
        misses.push(format!(
            "a macro that failed before recording anything cut the rewind rows: `before` is \
             not offered:\n{picker}"
        ));
    }
    if squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
        misses.push(format!(
            "the picker says it stops at a macro turn though the macro recorded nothing:\n{}",
            since(&pty, picker_at)
        ));
    }
    // Back out without popping.
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(600));
    quit_declining_save(&mut pty);
    assert!(
        misses.is_empty(),
        "{} control expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// SIGTERM drops the queued lines even when the tool is released at once.
// ---------------------------------------------------------------------

/// Two lines queued while a tool sleeps, SIGTERM and the release file in
/// the same instant (so an unpurged queue would ride the next request):
/// the process leaves cleanly within the deadline, neither line reaches a
/// request body, and the scripted reply behind the exit never streams.
#[test]
#[serial]
fn sigterm_purges_the_turn_queue_even_when_the_tool_is_released_right_after() {
    let dir = fresh_config_dir("sigterm-release");
    let _guard = TempDirGuard(dir.clone());
    let tool_a = dir.join("tool-a");
    let fake = FakeLlm::start(vec![
        sleep_call(&tool_a),
        Reply::Text("never-sent-w3t".to_string()),
        Reply::Text("never-sent-either-w3t".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let pid = child_pid(&transcript);

    pty.send(b"first-turn-w3t\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-a-started"),
        "the sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    pty.send(b"ghost-a-w3t\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(b"ghost-b-w3t\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = repainted_frame(&mut pty, 40, 101);
    assert!(
        squeezed(&frame).contains("pending:2"),
        "two lines queued while the tool slept did not show `pending: 2`:\n{frame}"
    );

    let signalled_at = Instant::now();
    // SAFETY: kill takes a pid and a signal number and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    fs::write(dir.join("tool-a-release"), b"").unwrap();
    let transcript = pty
        .wait_for("tty-probe-done", PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("SIGTERM during the tool round did not end the session: {e}"));
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert!(
        signalled_at.elapsed() < Duration::from_secs(15),
        "SIGTERM took {:?} to end the session",
        signalled_at.elapsed()
    );
    assert_tty_restored(&transcript);
    assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
    // Time for a wrongly flushed queued line to open a request.
    std::thread::sleep(Duration::from_millis(1500));
    let bodies = fake.bodies();
    assert!(
        bodies
            .iter()
            .all(|b| !b.contains("ghost-a-w3t") && !b.contains("ghost-b-w3t")),
        "a line queued before SIGTERM reached the model once the tool finished:\n{}",
        bodies.join("\n---\n")
    );
    assert!(
        !transcript.contains("never-sent"),
        "a reply scripted behind the signal exit streamed:\n{transcript}"
    );
}
