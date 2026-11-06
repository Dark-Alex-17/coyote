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
//! - the "Thinking for Ns" status row is live while a reasoning delta is
//!   held;
//! - `F2` shows and hides the Todo panel;
//! - scrolling up raises the follow chip, `End` and a submitted line
//!   clear it.
//! - a tool call is one cell that finishes in place (its start and end
//!   events pair up), never a running cell plus a finished one;
//! - `.undo` with nothing to undo, a terminal shrunk to a few cells, and
//!   an emoji/VS16 prompt leave the UI alive and quitting cleanly;
//! - undoing the only exchange stops at the command cell before it and
//!   leaves that cell in place.
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
const END: &[u8] = b"\x1b[F";
const CTRL_L: &[u8] = b"\x0c";
const CTRL_D: &[u8] = b"\x04";

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
            "{what}: {marker:?} never appeared on screen:\n{text}"
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

    // The composer holds the undone prompt; Ctrl+D would not quit over it.
    drop(pty);
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

#[test]
#[serial]
fn follow_chip_appears_on_scroll_up_and_clears_on_end() {
    let (_guard, mut pty) = boot("follow-chip", &[]);

    pty.resize(14, 100).unwrap();
    pty.read_for(Duration::from_millis(500));

    for n in 1..=5 {
        let before = pty.raw().len();
        let marker = format!("fillerline{n:02}");
        pty.send(format!("{marker}\r").as_bytes()).unwrap();
        wait_for_squeezed_after(&mut pty, &marker, before, "a filler prompt");
        pty.read_for(Duration::from_millis(300));
    }

    let frame = repaint(&mut pty);
    assert!(
        !frame.contains("End=follow"),
        "the follow chip is up while the view follows the bottom:\n{frame}"
    );

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
// Criterion: "tool/job/agent cells pair with their terminal event by
// COYOTE_TOOL_CALL_ID". Observable from the outside: while the tool runs
// its cell names it; once it has finished and the model replied, the
// same name is on screen exactly once (the end event landed in the cell
// that the start event opened, not in a second one).
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
// Checklist item 6: "every custom widget clamps to the buffer". With the
// welcome block up and the Todo panel open, the terminal is shrunk to a
// handful of cells and then to a single cell; nothing may panic, a key
// typed under every size still reaches the composer, and once the
// terminal grows back both widgets are drawn again and the draft typed
// along the way is submitted whole.
//
// Known and out of this task's scope: after a shrink the composer's own
// viewport (ratatui-textarea) may stay scrolled right, so the draft can
// look blank until it is edited or sent; the draft itself is intact,
// which is what the submitted user cell asserts.
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
// Checklist item 6: "rows with VS16/emoji graphemes repaint whole". A
// prompt carrying a VS16 sequence and a wide emoji reaches the user cell
// and the dry-run echo intact.
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
        frame.contains("solomarkerone"),
        ".undo did not hand the undone prompt back to the composer:\n{frame}"
    );

    drop(pty);
}
