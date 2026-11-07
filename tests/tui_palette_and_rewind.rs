//! Black-box pins for the bottom pane's command palette, the rewind picker,
//! the terminal lender and the prompt modals: the built binary on a
//! pseudo-terminal, a scripted fake model where a tool call is needed, and
//! the real `prompt-utils.sh` helpers as the prompt channel's client.
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
        "coyote-palette-{scenario}-{}-{unique}-{sequence}",
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

/// A tool that asks through one prompt helper and records what the helper
/// printed (or `cancelled` when it printed nothing, the fail-closed path).
fn recording_prompt_tool(call: &str) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         \n\
         # @describe Ask the user through a prompt helper and record the answer.\n\
         # @option --out! Where to write the answer.\n\
         \n\
         # @env LLM_OUTPUT=/dev/stdout The output path\n\
         \n\
         # shellcheck disable=SC1090\n\
         source \"$LLM_PROMPT_UTILS_FILE\"\n\
         \n\
         main() {{\n\
         \x20   local out v\n\
         \x20   out=\"$(jq -r '.out' <<< \"$LLM_TOOL_RAW_JSON\")\"\n\
         \x20   v=\"$({call})\"\n\
         \x20   printf '%s' \"${{v:-cancelled}}\" > \"$out\"\n\
         \x20   echo \"fixture prompt answered z2r\" >> \"$LLM_OUTPUT\"\n\
         }}\n"
    )
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
    \x20   echo \"fixture slept r12s\" >> \"$LLM_OUTPUT\"\n\
    }\n";

fn tool_call_then_done(tool: &str, out: &Path) -> FakeLlm {
    FakeLlm::start(vec![
        Reply::ToolCall {
            name: tool.to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture done z2s".to_string()),
    ])
}

const TTY_REPORT: &str = "echo; echo \"exit=$rc\"; \
     echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done";
const WAIT_WRAPPER: &str =
    "'{coyote}'{args} <{stdin} & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {report}";

fn built_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_coyote"))
}

fn spawn_tui(dir: &Path, extra_env: &[(&str, &str)]) -> Pty {
    spawn_ui(dir, "tui", extra_env)
}

fn spawn_ui(dir: &Path, ui: &str, extra_env: &[(&str, &str)]) -> Pty {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(
        WAIT_WRAPPER
            .replace("{coyote}", built_binary().to_str().unwrap())
            .replace("{args}", "")
            .replace("{stdin}", "/dev/tty")
            .replace("{report}", TTY_REPORT),
    );
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
        .env("COYOTE_UI", ui)
        .env_remove("IS_SANDBOX")
        .env_remove("COLORTERM")
        .env_remove("NO_COLOR")
        .envs(extra_env.iter().copied());
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

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
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

/// Quit from a composer that may or may not hold a draft: Ctrl+C clears
/// a draft, or on an idle empty composer ends the session outright;
/// Ctrl+D follows only when the session is still up.
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

fn finish_after_tool(pty: &mut Pty, from: usize) {
    wait_for_squeezed_after(pty, "fixture done z2s", from, "the reply after the tool");
    quit_and_check(pty);
}

// ---------------------------------------------------------------------
// The terminal lender survives a second lend in one session.
// ---------------------------------------------------------------------

/// `!cmd` shell passthrough runs through the terminal lender. Regression
/// target: the second lend deadlocked on the reader lock. Two `!true`
/// lines in one session must both come back to a composer that takes keys.
#[test]
#[serial]
fn two_bang_lines_in_one_session_both_return_to_the_composer() {
    let dir = fresh_config_dir("bang-twice");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    let first = pty.raw().len();
    pty.send(b"!true\r").unwrap();
    let back = wait_for_lend_and_return(&mut pty, first, "the first `!true`");
    assert_composer_takes_keys(&mut pty, back, "the first `!true`");

    let second = pty.raw().len();
    pty.send(b"!true\r").unwrap();
    let back = wait_for_lend_and_return(&mut pty, second, "the second `!true`");
    assert_composer_takes_keys(&mut pty, back, "the second `!true`");

    let transcript = quit_and_check(&mut pty);
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        3,
        "expected two lends and the final exit to leave the alternate screen:\n{transcript}"
    );
}

/// `.edit`-style editor shell-outs run through the lender like Ctrl+O.
/// `.edit config` with `EDITOR=true` returns to the composer, and a
/// `!true` straight after (lend after lend) returns too.
#[test]
#[serial]
fn dot_edit_then_a_bang_line_both_return_to_the_composer() {
    let dir = fresh_config_dir("edit-then-bang");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let editor = if Path::new("/usr/bin/true").exists() {
        "/usr/bin/true"
    } else {
        "/bin/true"
    };
    let mut pty = spawn_tui(&dir, &[("EDITOR", editor), ("VISUAL", editor)]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    let edit_at = pty.raw().len();
    pty.send(b".edit config\r").unwrap();
    let back = wait_for_lend_and_return(&mut pty, edit_at, "`.edit config`");
    assert_composer_takes_keys(&mut pty, back, "`.edit config`");

    let bang_at = pty.raw().len();
    pty.send(b"!true\r").unwrap();
    let back = wait_for_lend_and_return(&mut pty, bang_at, "`!true` after `.edit`");
    assert_composer_takes_keys(&mut pty, back, "`!true` after `.edit`");

    quit_and_check(&mut pty);
}

// ---------------------------------------------------------------------
// Prompt modals through the real helper; Enter settles from the modal's
// appearance, not from the last keystroke.
// ---------------------------------------------------------------------

/// Enter on an Input field settles 1 s from the modal's appearance, never
/// re-armed by typing. Typed text plus Enter inside the first second is
/// not accepted (the modal stays, the tool gets nothing); Enter once a
/// second has passed since the modal appeared is accepted even though a
/// keystroke landed 300 ms earlier.
#[test]
#[serial]
fn enter_on_an_input_modal_settles_from_appearance_not_from_typing() {
    let dir = fresh_config_dir("input-settle");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("input-answer");
    let fake = tool_call_then_done("fixture_input_rec", &out);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_input_rec.sh",
        &recording_prompt_tool("input \"Your name q9d-i\""),
    );
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let turn_at = pty.raw().len();
    pty.send(b"call the input tool\r").unwrap();
    pty.wait_for_after("q9d-i", turn_at, PTY_DEADLINE)
        .expect("the tool's input question never reached the screen");
    let appeared = Instant::now();

    // Inside the first second: text then Enter. Enter must be dropped.
    pty.send(b"abc\r").unwrap();
    pty.read_for(Duration::from_millis(400));
    assert!(
        appeared.elapsed() < Duration::from_millis(950),
        "the test itself was too slow to exercise the early Enter ({:?})",
        appeared.elapsed()
    );
    assert!(
        !out.exists(),
        "Enter inside the first second of the Input modal was accepted: {:?}",
        fs::read_to_string(&out)
    );

    // Past the appearance gate but within 300 ms of a keystroke: a
    // further character, then Enter. The appearance gate is the only one
    // on a field, so this Enter is accepted with the whole text.
    let settle = Duration::from_millis(1500).saturating_sub(appeared.elapsed());
    pty.read_for(settle);
    pty.send(b"d").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "abcd",
        "the Input modal did not hand the tool the typed text on the settled Enter"
    );
    finish_after_tool(&mut pty, turn_at);
}

/// The helper contract says a cancelled prompt returns 1 and prints
/// nothing. Esc on a tool's Input request leaves the tool with nothing
/// (the fixture records `cancelled`) and the turn still finishes.
#[test]
#[serial]
fn esc_on_a_tools_input_request_cancels_and_the_tool_fails_closed() {
    let dir = fresh_config_dir("input-esc");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("input-answer");
    let fake = tool_call_then_done("fixture_input_rec", &out);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_input_rec.sh",
        &recording_prompt_tool("input \"Your name q9d-e\""),
    );
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let turn_at = pty.raw().len();
    pty.send(b"call the input tool\r").unwrap();
    pty.wait_for_after("q9d-e", turn_at, PTY_DEADLINE)
        .expect("the tool's input question never reached the screen");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "cancelled",
        "Esc on the Input modal did not fail the helper closed"
    );
    finish_after_tool(&mut pty, turn_at);
}

/// Same contract for a Select request: Esc cancels, helper prints nothing.
#[test]
#[serial]
fn esc_on_a_tools_select_request_cancels_and_the_tool_fails_closed() {
    let dir = fresh_config_dir("select-esc");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("select-answer");
    let fake = tool_call_then_done("fixture_select_rec", &out);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_select_rec.sh",
        &recording_prompt_tool("list \"Pick one q9d-x\" alpha beta gamma"),
    );
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let turn_at = pty.raw().len();
    pty.send(b"call the select tool\r").unwrap();
    pty.wait_for_after("q9d-x", turn_at, PTY_DEADLINE)
        .expect("the tool's select question never reached the screen");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "cancelled",
        "Esc on the Select modal did not fail the helper closed"
    );
    finish_after_tool(&mut pty, turn_at);
}

/// Enter on a Select modal sends the highlighted row (row 0 by default);
/// Down then Enter sends row 1. Arrow keys plus Enter is the keyboard
/// path the digit shortcut does not exercise.
#[test]
#[serial]
fn down_then_enter_on_a_tools_select_request_sends_the_second_row() {
    let dir = fresh_config_dir("select-arrows");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("select-answer");
    let fake = tool_call_then_done("fixture_select_rec", &out);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_select_rec.sh",
        &recording_prompt_tool("list \"Pick one q9d-y\" alpha beta gamma"),
    );
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let turn_at = pty.raw().len();
    pty.send(b"call the select tool\r").unwrap();
    pty.wait_for_after("q9d-y", turn_at, PTY_DEADLINE)
        .expect("the tool's select question never reached the screen");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b[B").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "Down then Enter on the Select modal did not reach the tool as beta's index"
    );
    finish_after_tool(&mut pty, turn_at);
}

// ---------------------------------------------------------------------
// Queued input while a turn runs.
// ---------------------------------------------------------------------

/// Queued text is injected at the model's next request, else becomes the
/// next prompt; the strip shows `pending: N`. Two drafts while the tool
/// sleeps show `pending: 2`, the turn is not moved on, and once released
/// the session finishes cleanly with both lines gone from the field.
#[test]
#[serial]
fn two_drafts_while_a_tool_sleeps_show_pending_two_and_drain_after_release() {
    let dir = fresh_config_dir("pending-two");
    let _guard = TempDirGuard(dir.clone());
    let marker = dir.join("tool");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_sleep".to_string(),
            arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
        },
        Reply::Text("fixture done r12-b".to_string()),
        Reply::Text("fixture done r12-c".to_string()),
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
    pty.read_for(Duration::from_millis(300));
    let typed_at = pty.raw().len();
    pty.send(b"draft-one-r12\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(b"draft-two-r12\r").unwrap();
    pty.read_for(Duration::from_millis(1000));
    // The strip is repainted cell by cell (`pending: 1` -> `pending: 2`
    // paints one digit), so a full frame is forced before reading it.
    let frame = repainted_frame(&mut pty, 40, 101);
    let flat = squeezed(&frame);
    let mid = since(&pty, typed_at);
    assert!(
        flat.contains("pending:2"),
        "two drafts while a tool ran did not show `pending: 2`:\n{frame}"
    );
    assert!(
        squeezed(&mid).contains("draft-one-r12") && squeezed(&mid).contains("draft-two-r12"),
        "the queued previews are missing:\n{mid}"
    );
    assert_eq!(
        fake.served(),
        1,
        "queued drafts moved the turn on before the tool finished:\n{mid}"
    );

    fs::write(dir.join("tool-release"), b"").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "fixture done r12-",
        typed_at,
        "the reply after the tool",
    );
    // Let any follow-up prompt the second draft became run to its end.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        pty.read_for(Duration::from_millis(300));
        let repaint_at = pty.raw().len();
        pty.send(b"\x0c").unwrap();
        pty.read_for(Duration::from_millis(500));
        let repainted = since(&pty, repaint_at);
        if !repainted.contains("pending:") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the queue strip outlived the turn:\n{repainted}"
        );
    }
    // Both drafts sat on the queue when the tool round ended, so the
    // request after it carries both and nothing is left to run as a
    // turn of its own.
    let bodies = fake.bodies();
    assert_eq!(fake.served(), 2, "served count:\n{bodies:#?}");
    assert_eq!(
        bodies.len(),
        2,
        "the queued drafts should ride the tool-round request, not open turns:\n{bodies:#?}"
    );
    assert!(
        bodies[1].contains("draft-one-r12") && bodies[1].contains("draft-two-r12"),
        "the request after the tool round is missing a queued draft:\n{}",
        bodies[1]
    );
    quit_and_check(&mut pty);
}

// ---------------------------------------------------------------------
// The palette's set equals the REPL completer's set, per prefix, in the
// same (cold) state. The oracle is the REPL itself: its Tab menu on the
// bare sigil lists every candidate as `<sigil>name   description`.
// ---------------------------------------------------------------------

/// One command the REPL completer offers: its name without the sigil and
/// its description.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Candidate {
    name: String,
    description: String,
}

/// The candidates the REPL completer offers cold (fresh config dir, no
/// command run yet) for `sigil`: the command table plus the enabled
/// macros, which both UIs offer. The menu is one entry per row and
/// stops at the terminal's height, so the pty grows past the set first.
fn repl_completer_candidates(dir: &Path, sigil: char) -> Vec<Candidate> {
    let mut pty = spawn_ui(dir, "repl", &[]);
    // Grown before the line editor starts reading: a resize mid-read is
    // a signal the editor repaints on, and the keys behind it are lost.
    pty.resize(100, 100).unwrap();
    pty.wait_for(")> ", PTY_DEADLINE)
        .expect("the REPL prompt never appeared");
    pty.read_for(Duration::from_millis(500));
    let menu_at = pty.raw().len();
    pty.send(sigil.to_string().as_bytes()).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\t").unwrap();
    pty.read_for(Duration::from_millis(800));
    pty.send(b"\t").unwrap();
    pty.read_for(Duration::from_millis(2500));
    let menu = since(&pty, menu_at);
    let mut offered = parse_sigil_rows(&menu, sigil);
    assert!(
        offered.len() > 10,
        "the REPL completer menu was not read ({} rows):\n{menu}",
        offered.len()
    );

    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\x04").unwrap();
    let _ = pty.wait_for("tty-probe-done", PTY_DEADLINE);

    offered.sort();
    offered.dedup();
    offered
}

/// Rows of the form `<sigil>name[ sub]<2+ spaces>description`, wherever
/// they start (the first menu row shares the prompt's line). A macro
/// without a description is a padded name alone and is kept as such.
fn parse_sigil_rows(text: &str, sigil: char) -> Vec<Candidate> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut rest = line;
        while let Some(at) = rest.find(sigil) {
            let tail = &rest[at + sigil.len_utf8()..];
            let name_end = tail.find("  ").unwrap_or(tail.len());
            let name = tail[..name_end].trim_end();
            let valid = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '-' || c == ' ')
                && name.split(' ').count() <= 2
                && name.starts_with(|c: char| c.is_ascii_lowercase());
            if valid {
                let description = tail[name_end..]
                    .split("\r")
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                out.push(Candidate {
                    name: name.to_string(),
                    description,
                });
                rest = &tail[name_end..];
                continue;
            }
            rest = tail;
        }
    }
    out
}

/// Forces a repaint by nudging the width and returns the frame painted.
fn repainted_frame(pty: &mut Pty, rows: u16, cols: u16) -> String {
    let at = pty.raw().len();
    pty.resize(rows, cols).unwrap();
    pty.read_for(Duration::from_millis(500));
    since(pty, at)
}

/// Types `<sigil>name` into an idle composer, nudges the width for a
/// repaint, then closes the band and clears the draft. Returns every
/// printable byte written from the typing on: the band's rows are
/// painted whole when they first appear, while a later repaint may skip
/// cells that did not change.
fn palette_frame_for(pty: &mut Pty, sigil: char, name: &str, toggle: &mut bool) -> String {
    let typed_at = pty.raw().len();
    pty.send(format!("{sigil}{name}").as_bytes()).unwrap();
    pty.read_for(Duration::from_millis(400));
    *toggle = !*toggle;
    repainted_frame(pty, 60, if *toggle { 141 } else { 140 });
    let frame = since(pty, typed_at);
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(200));
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(300));
    frame
}

/// What a palette row for `c` reads as once whitespace is squeezed: the
/// sigil, the name, and the head of the description (the REPL menu that
/// produced `c` wraps long descriptions at its width). Once the space
/// after a command word is typed the band lists its sub-verbs by verb
/// alone (`mcp-server  Show an MCP ...`), so that form counts too.
fn row_markers(sigil: char, c: &Candidate) -> Vec<String> {
    let head: String = c.description.chars().take(20).collect();
    let mut markers = vec![squeezed(&format!("{sigil}{}{head}", c.name))];
    if let Some((_, verb)) = c.name.split_once(' ') {
        markers.push(squeezed(&format!("{verb}{head}")));
    }
    markers
}

fn offers(frame: &str, sigil: char, c: &Candidate) -> bool {
    let flat = squeezed(frame);
    row_markers(sigil, c).iter().any(|m| flat.contains(m))
}

/// Cold start under `sigil`: every candidate the REPL completer offers is
/// a palette row when its name is typed, and the bare-sigil band's total
/// (rows shown plus the `+N more` tail) equals the completer's count.
fn assert_palette_matches_completer(prefix_yaml: &str, sigil: char, scenario: &str) {
    let repl_dir = fresh_config_dir(&format!("{scenario}-repl"));
    let _repl_guard = TempDirGuard(repl_dir.clone());
    write_dry_run_config(&repl_dir, prefix_yaml);
    let completer = repl_completer_candidates(&repl_dir, sigil);

    let dir = fresh_config_dir(&format!("{scenario}-tui"));
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, prefix_yaml);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(500));

    let mut toggle = false;
    let bare = palette_frame_for(&mut pty, sigil, "", &mut toggle);
    let bare_flat = squeezed(&bare);
    let shown = completer
        .iter()
        .filter(|c| bare_flat.contains(&row_markers(sigil, c)[0]))
        .count();
    let more: usize = bare_flat
        .split("(+")
        .nth(1)
        .and_then(|s| s.split("more").next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0);

    let mut missing = Vec::new();
    for c in &completer {
        // Keys typed in one write are handled one per loop iteration and
        // the band can be painted mid-word; one retry separates a
        // torn frame from a row that is not offered.
        let offered = (0..2).any(|_| {
            let frame = palette_frame_for(&mut pty, sigil, &c.name, &mut toggle);
            offers(&frame, sigil, c)
        });
        if !offered {
            missing.push(c.name.clone());
        }
    }
    pty.read_for(HOTKEY_GAP);
    quit_and_check(&mut pty);

    assert!(
        missing.is_empty(),
        "under `{sigil}` the palette does not offer {} of the {} commands the REPL \
         completer offers cold: {missing:?}",
        missing.len(),
        completer.len()
    );
    let completer_names: Vec<&str> = completer.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        shown + more,
        completer.len(),
        "under `{sigil}` the bare palette band counts {shown} rows + {more} more, the REPL \
         completer offers {} commands: {completer_names:?}",
        completer.len()
    );
}

#[test]
#[serial]
fn palette_set_equals_the_repl_completer_set_under_slash() {
    assert_palette_matches_completer("command_prefix: slash\n", '/', "palette-slash");
}

#[test]
#[serial]
fn palette_set_equals_the_repl_completer_set_under_both() {
    assert_palette_matches_completer("command_prefix: both\n", '/', "palette-both");
}

#[test]
#[serial]
fn palette_set_equals_the_repl_completer_set_under_both_with_a_dot() {
    assert_palette_matches_completer("command_prefix: both\n", '.', "palette-both-dot");
}

#[test]
#[serial]
fn palette_set_equals_the_repl_completer_set_under_dot() {
    assert_palette_matches_completer("", '.', "palette-dot");
}

/// `.tool enable` / `.tool disable` are offered by the `.tool` filter on
/// a cold start, before any command has run, and still after one has
/// (`.info tools` itself works cold, so the commands are available).
#[test]
#[serial]
fn tool_verbs_are_offered_by_the_palette_cold_and_warm() {
    let dir = fresh_config_dir("palette-warm");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(500));

    let mut toggle = false;
    let cold = palette_frame_for(&mut pty, '.', "tool", &mut toggle);
    let cold_offers = squeezed(&cold).contains("toolenableEnableasingletool");

    let info_at = pty.raw().len();
    pty.send(b".info tools\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "Tools enabled for the next request",
        info_at,
        "`.info tools` running cold",
    );
    pty.read_for(Duration::from_millis(500));

    let warm = palette_frame_for(&mut pty, '.', "tool", &mut toggle);
    let warm_offers = squeezed(&warm).contains("toolenableEnableasingletool");
    pty.read_for(HOTKEY_GAP);
    quit_and_check(&mut pty);

    assert!(
        warm_offers,
        "`.tool` offers nothing even after a command ran:\n{warm}"
    );
    assert!(
        cold_offers,
        "`.tool enable|disable` are hidden on a cold start while `.info tools` works:\n\
         cold frame:\n{cold}"
    );
}

/// Writes an `$EDITOR` that replaces the buffer it is handed with `text`
/// and returns the editor's path.
fn rewriting_editor(dir: &Path, text: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let editor = dir.join("rewriting-editor.sh");
    fs::write(
        &editor,
        format!("#!/bin/sh\nprintf '%s\\n' '{text}' > \"$1\"\nexit 0\n"),
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();
    editor.to_str().unwrap().to_string()
}

/// The palette's token range is measured on the line the band opened
/// over. Ctrl+O hands that line to an editor that shortens it to `.i`;
/// Enter straight after, on the highlight, must not index the old range
/// into the shorter line: the UI stays up and the composer takes keys.
#[test]
#[serial]
fn the_editor_shrinking_the_line_under_an_open_palette_does_not_crash_enter() {
    let dir = fresh_config_dir("palette-editor-shrink");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let editor = rewriting_editor(&dir, ".i");
    let mut pty = spawn_tui(&dir, &[("EDITOR", &editor), ("VISUAL", &editor)]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    pty.send(b".in").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\t").unwrap();
    pty.read_for(HOTKEY_GAP);
    let edit_at = pty.raw().len();
    pty.send(b"\x0f").unwrap();
    wait_for_lend_and_return(&mut pty, edit_at, "Ctrl+O under the palette");
    pty.send(b"\r").unwrap();
    let after = pty.read_for(HOTKEY_GAP);
    assert!(
        !after.contains("exit="),
        "Enter on the palette after the editor shrank the line ended the session:\n{after}"
    );

    let cleared_at = pty.raw().len();
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(400));
    assert_composer_takes_keys(&mut pty, cleared_at, "Enter on the stale palette");
    quit_and_check(&mut pty);
}

// ---------------------------------------------------------------------
// The Esc Esc rewind picker pops N exchanges.
// ---------------------------------------------------------------------

/// Two dry-run exchanges, Esc Esc, Down (second row = two exchanges
/// back), Enter: both exchanges leave the transcript.
#[test]
#[serial]
fn esc_esc_then_the_second_row_rewinds_two_exchanges() {
    let dir = fresh_config_dir("rewind-two");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    let first_at = pty.raw().len();
    pty.send(b"hello one r3w\r").unwrap();
    wait_for_squeezed_after(&mut pty, "> hello one r3w", first_at, "the first exchange");
    pty.read_for(Duration::from_millis(2500));
    let second_at = pty.raw().len();
    pty.send(b"hello two r3w\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "> hello two r3w",
        second_at,
        "the second exchange",
    );
    pty.read_for(Duration::from_millis(2500));

    let picker_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(150));
    pty.send(b"\x1b").unwrap();
    pty.wait_for_after("Rewind to", picker_at, PTY_DEADLINE)
        .expect("Esc Esc did not open the rewind picker");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x1b[B").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));

    let repaint_at = pty.raw().len();
    pty.send(b"\x0c").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = squeezed(&since(&pty, repaint_at));
    assert!(
        !frame.contains("Rewindto"),
        "the picker is still up after Enter:\n{}",
        since(&pty, repaint_at)
    );
    assert!(
        !frame.contains(">hellotwor3w") && !frame.contains(">hellooner3w"),
        "rewinding two exchanges left one in the transcript:\n{}",
        since(&pty, repaint_at)
    );

    quit_clearing_any_draft(&mut pty);
}

/// Choosing a rewind row pops the exchanges and prefills the composer:
/// after choosing the newest row the composer holds that prompt again,
/// the way `.undo` restores the prompt in the REPL.
#[test]
#[serial]
fn choosing_a_rewind_row_prefills_the_composer_with_that_prompt() {
    let dir = fresh_config_dir("rewind-prefill");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    let first_at = pty.raw().len();
    pty.send(b"hello one p4f\r").unwrap();
    wait_for_squeezed_after(&mut pty, "> hello one p4f", first_at, "the exchange");
    pty.read_for(Duration::from_millis(2500));

    let picker_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(150));
    pty.send(b"\x1b").unwrap();
    pty.wait_for_after("Rewind to", picker_at, PTY_DEADLINE)
        .expect("Esc Esc did not open the rewind picker");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));

    let frame = repainted_frame(&mut pty, 40, 101);
    let flat = squeezed(&frame);
    assert!(
        !flat.contains(">helloonep4f"),
        "the chosen exchange was not popped:\n{frame}"
    );
    assert!(
        !flat.contains("Typeapromptora.command"),
        "after choosing a rewind row the composer shows its placeholder instead of the \
         restored prompt:\n{frame}"
    );
    assert!(
        flat.contains("helloonep4f"),
        "the chosen prompt was not prefilled into the composer:\n{frame}"
    );

    quit_clearing_any_draft(&mut pty);
}
