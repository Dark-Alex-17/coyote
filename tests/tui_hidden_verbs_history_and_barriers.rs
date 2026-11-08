//! Black-box pins for the bottom pane's hidden verbs, history walk and
//! rewind barriers: the verbs hidden until their prompts are routed stay
//! out of the palette after the state warms up (a command ran, an agent
//! was switched) under both prefixes while typing one in full still runs
//! it; the history walk (Up/Down, Ctrl+R) never reopens the palette over
//! the recalled line; `.macro enable` is not a rewind stop and a picker
//! with nothing above the macro cannot crash; a backtick document
//! command's grandchild dies on Esc while its shell still leads the
//! group; Ctrl+D during a turn drops the queued lines. The built binary
//! on a pseudo-terminal, a scripted fake model where a turn has to be
//! held open. Unix-only and serial like its sibling pty suites.
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
/// The verbs hidden from the palette until their prompt sites are
/// routed, each paired with a typed head that narrows the palette to it
/// (and to nothing else spelling it). The heads are hand-picked, so the
/// verbs are checked against the palette's own list before they are used.
const HIDDEN_HEADS: [(&str, &str); 5] = [
    ("ra", "rag"),
    ("va", "vault"),
    ("ins", "install"),
    ("unin", "uninstall"),
    ("upd", "update"),
];

/// The verbs `HIDDEN_UNTIL_ROUTED` names, read from the palette's source
/// the way the palette suite does: the binary has no library target to
/// import the constant from.
fn hidden_until_routed() -> Vec<String> {
    let source =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui/bottom_pane.rs"))
            .unwrap();
    let start = source
        .find("const HIDDEN_UNTIL_ROUTED")
        .expect("the palette names its hidden verbs");
    let list = &source[start..];
    let list = &list[list.find('=').unwrap() + 1..];
    let list = &list[list.find("&[").unwrap() + 2..];
    let list = &list[..list.find(']').unwrap()];
    list.split(',')
        .map(|entry| entry.trim().trim_matches('"').to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Fails when `HIDDEN_HEADS` and the palette's list name different verbs.
fn assert_hidden_heads_match_the_palette() {
    let mut listed = hidden_until_routed();
    listed.sort();
    let mut heads: Vec<String> = HIDDEN_HEADS
        .iter()
        .map(|(_, verb)| format!(".{verb}"))
        .collect();
    heads.sort();
    assert_eq!(
        heads, listed,
        "HIDDEN_HEADS drifted from the palette's HIDDEN_UNTIL_ROUTED"
    );
}

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
        "coyote-hidden-walk-{scenario}-{}-{unique}-{sequence}",
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

/// An agent with nothing to ask for, so `.agent <name>` switches at once.
fn write_plain_agent(dir: &Path, name: &str) {
    let agent_dir = dir.join("agents").join(name);
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(
        agent_dir.join("config.yaml"),
        format!(
            "name: {name}\n\
             description: a plain agent\n\
             version: 0.1.0\n\
             instructions: |\n\
             \x20 You are the plain agent. Reply briefly.\n"
        ),
    )
    .unwrap();
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

/// Alternates the width (141/140) so every call is a real resize, and
/// returns the frame.
fn next_frame(pty: &mut Pty, toggle: &mut bool) -> String {
    *toggle = !*toggle;
    repainted_frame(pty, 60, if *toggle { 141 } else { 140 })
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

/// The placeholder is on screen: the composer holds no draft.
fn shows_placeholder(flat: &str) -> bool {
    flat.contains("Typeapromptora.command")
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

/// Sends one key and lets the parser see it alone (Esc must not be
/// glued to the next byte).
fn key(pty: &mut Pty, bytes: &[u8]) {
    pty.send(bytes).unwrap();
    pty.read_for(Duration::from_millis(250));
}

/// Types `<sigil><head>` into an idle composer, repaints, then closes the
/// band and clears the draft. Returns every printable byte written from
/// the typing on.
fn palette_frame_for(pty: &mut Pty, sigil: char, head: &str, toggle: &mut bool) -> String {
    let typed_at = pty.raw().len();
    pty.send(format!("{sigil}{head}").as_bytes()).unwrap();
    pty.read_for(Duration::from_millis(400));
    next_frame(pty, toggle);
    let frame = since(pty, typed_at);
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(200));
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(300));
    frame
}

/// Every hidden verb that shows up as `<sigil><verb>` anywhere in the
/// frame painted for its head (the composer holds only the head, so any
/// full spelling is a palette row), plus the positive control: `ses`
/// must offer `<sigil>session` or the band was not read at all.
fn hidden_verbs_shown(pty: &mut Pty, sigil: char, toggle: &mut bool, state: &str) -> Vec<String> {
    let control = palette_frame_for(pty, sigil, "ses", toggle);
    assert!(
        control.contains(&format!("{sigil}session")),
        "{state}: under `{sigil}` typing `{sigil}ses` offers no `{sigil}session` row; the band \
         was not read:\n{control}"
    );
    let mut shown = Vec::new();
    for (head, verb) in HIDDEN_HEADS {
        let frame = palette_frame_for(pty, sigil, head, toggle);
        let spelled = format!("{sigil}{verb}");
        if frame.contains(&spelled) {
            shown.push(format!(
                "{state}: `{sigil}{head}` offers {spelled}:\n{frame}"
            ));
        }
    }
    shown
}

// ---------------------------------------------------------------------
// Hidden verbs stay hidden once the palette state is rebuilt.
// ---------------------------------------------------------------------

/// Under `sigil` the five hidden verbs are absent from the palette on a
/// cold start, after a command ran, after a dry-run exchange, and after
/// an `.agent` switch rebuilt the palette's state; the `session` control
/// row is offered in every state.
fn assert_hidden_verbs_stay_hidden(prefix_yaml: &str, sigil: char, scenario: &str) {
    assert_hidden_heads_match_the_palette();
    let dir = fresh_config_dir(scenario);
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, prefix_yaml);
    write_plain_agent(&dir, "plain-agent");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(500));
    let mut toggle = false;
    let mut shown = Vec::new();

    shown.extend(hidden_verbs_shown(&mut pty, sigil, &mut toggle, "cold"));

    let info_at = pty.raw().len();
    pty.send(format!("{sigil}info tools\r").as_bytes()).unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "Tools enabled for the next request",
        info_at,
        "`info tools` running cold",
    );
    pty.read_for(Duration::from_millis(500));
    shown.extend(hidden_verbs_shown(
        &mut pty,
        sigil,
        &mut toggle,
        "after a command ran",
    ));

    let turn_at = pty.raw().len();
    pty.send(b"warm-up-q8w\r").unwrap();
    wait_for_squeezed_after(&mut pty, "> warm-up-q8w", turn_at, "the dry-run exchange");
    pty.read_for(Duration::from_millis(2000));
    shown.extend(hidden_verbs_shown(
        &mut pty,
        sigil,
        &mut toggle,
        "after an exchange",
    ));

    let agent_at = pty.raw().len();
    pty.send(format!("{sigil}agent plain-agent\r").as_bytes())
        .unwrap();
    wait_for_squeezed_after(&mut pty, "plain-agent", agent_at, "the agent switch");
    pty.read_for(Duration::from_millis(1500));
    shown.extend(hidden_verbs_shown(
        &mut pty,
        sigil,
        &mut toggle,
        "after an agent switch",
    ));

    pty.read_for(HOTKEY_GAP);
    quit_clearing_any_draft(&mut pty);
    assert!(
        shown.is_empty(),
        "under `{sigil}` the palette offers hidden verbs in {} state(s):\n\n{}",
        shown.len(),
        shown.join("\n\n")
    );
}

#[test]
#[serial]
fn hidden_verbs_stay_out_of_the_palette_cold_and_warm_under_dot() {
    assert_hidden_verbs_stay_hidden("", '.', "hidden-warm-dot");
}

#[test]
#[serial]
fn hidden_verbs_stay_out_of_the_palette_cold_and_warm_under_slash() {
    assert_hidden_verbs_stay_hidden("command_prefix: slash\n", '/', "hidden-warm-slash");
}

/// A hidden verb typed in full still runs: `.rag`, `.vault` and
/// `.uninstall` under the TUI end in the TUI's refusal (or the command's
/// own usage), never a hang or a crash, and the composer takes keys
/// afterwards. The palette is simply not offering them; the command
/// table still has them. (`.install`/`.update` reach the network.)
#[test]
#[serial]
fn a_hidden_verb_typed_in_full_still_runs_and_the_ui_survives() {
    let dir = fresh_config_dir("hidden-typed");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(500));

    for (verb, outcome) in [
        ("rag", "Interactive prompts are not available"),
        ("vault", "Usage: .vault"),
        ("uninstall", "Usage: .uninstall"),
    ] {
        let at = pty.raw().len();
        pty.send(format!(".{verb}\r").as_bytes()).unwrap();
        let started = Instant::now();
        // The refusal is a command cell; wait for the command header to
        // be committed, then for the composer to take keys again.
        pty.read_for(Duration::from_millis(1500));
        assert_composer_takes_keys(&mut pty, at, &format!("`.{verb}` typed in full"));
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "`.{verb}` held the turn for {:?}",
            started.elapsed()
        );
        // The command cell's header and its refusal or usage: the typed
        // line alone would also be echoed by the composer.
        let after = squeezed(&since(&pty, at));
        assert!(
            after.contains(&format!(">.{verb}")) && after.contains(&squeezed(outcome)),
            "`.{verb}` did not run to `{outcome}` in the transcript:\n{}",
            since(&pty, at)
        );
    }
    quit_and_check(&mut pty);
}

// ---------------------------------------------------------------------
// The history walk never reopens the palette.
// ---------------------------------------------------------------------

/// After `.info tools` and `.info` ran, Up on the empty composer recalls
/// `.info` with NO palette band over it (no `Show system info` row), the
/// next Up walks on to `.info tools` instead of moving a band highlight,
/// Down comes back to `.info`, and Enter runs it. Ctrl+R to `.info` is a
/// recall without a band too. The palette returns with the next real
/// edit.
#[test]
#[serial]
fn walking_the_history_with_up_down_and_ctrl_r_never_reopens_the_palette() {
    let dir = fresh_config_dir("history-walk");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(500));

    let at = pty.raw().len();
    pty.send(b".info tools\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "Tools enabled for the next request",
        at,
        "`.info tools`",
    );
    pty.read_for(Duration::from_millis(800));
    let at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", at, PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(1500));

    let mut toggle = false;
    let mut misses: Vec<String> = Vec::new();

    // Up: `.info` recalled, no band.
    key(&mut pty, b"\x1b[A");
    let frame = next_frame(&mut pty, &mut toggle);
    let row = composer_row(&frame, "dryrun:dry-model");
    if row != ".info" {
        misses.push(format!(
            "Up did not recall `.info` (composer: `{row}`):\n{frame}"
        ));
    }
    if frame.contains("Show system info") {
        misses.push(format!(
            "Up reopened the palette over the recalled `.info`:\n{frame}"
        ));
    }

    // Up again: the walk continues to `.info tools`.
    key(&mut pty, b"\x1b[A");
    let frame = next_frame(&mut pty, &mut toggle);
    let row = composer_row(&frame, "dryrun:dry-model");
    if row != ".info tools" {
        misses.push(format!(
            "a second Up did not walk on to `.info tools` (composer: `{row}`):\n{frame}"
        ));
    }
    if frame.contains("Show system info") {
        misses.push(format!("the second Up shows a palette band:\n{frame}"));
    }

    // Down: back to `.info`.
    key(&mut pty, b"\x1b[B");
    let frame = next_frame(&mut pty, &mut toggle);
    let row = composer_row(&frame, "dryrun:dry-model");
    if row != ".info" {
        misses.push(format!(
            "Down did not come back to `.info` (composer: `{row}`):\n{frame}"
        ));
    }

    // Enter runs the recalled command (a palette would swallow it).
    let run_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    if pty
        .wait_for_after("log_path", run_at, PTY_DEADLINE)
        .is_err()
    {
        misses.push(format!(
            "Enter on the recalled `.info` did not run it:\n{}",
            since(&pty, run_at)
        ));
    }
    pty.read_for(Duration::from_millis(1500));

    // Ctrl+R, `info`, Enter: recalled without a band.
    pty.send(b"\x12").unwrap();
    pty.read_for(Duration::from_millis(600));
    pty.send(b"info").unwrap();
    pty.read_for(Duration::from_millis(600));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = next_frame(&mut pty, &mut toggle);
    let row = composer_row(&frame, "dryrun:dry-model");
    if !row.starts_with(".info") {
        misses.push(format!(
            "Ctrl+R `info` Enter did not recall an `.info` line (composer: `{row}`):\n{frame}"
        ));
    }
    if frame.contains("Show system info") {
        misses.push(format!("the Ctrl+R recall reopened the palette:\n{frame}"));
    }
    // Enter on the recalled line must run it, as it does after Up.
    let run_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    if pty
        .wait_for_after("log_path", run_at, Duration::from_secs(6))
        .is_err()
    {
        let frame = next_frame(&mut pty, &mut toggle);
        misses.push(format!(
            "Enter on the Ctrl+R-recalled `.info` did not run it (composer now: `{}`):\n{}",
            composer_row(&frame, "dryrun:dry-model"),
            since(&pty, run_at)
        ));
    }
    pty.read_for(Duration::from_millis(800));

    // A real edit brings the palette back: any draft cleared, `.in`
    // opens.
    clear_any_draft(&mut pty);
    pty.send(b".in").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = next_frame(&mut pty, &mut toggle);
    if !frame.contains("Show system info") {
        misses.push(format!(
            "after the history walk a fresh `.in` no longer opens the palette:\n{frame}"
        ));
    }
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} history-walk expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// `.macro enable` is not a rewind stop; nothing above the macro is safe.
// ---------------------------------------------------------------------

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

/// `before`, `.macro enable two-step`, `after`: the toggle only enables
/// one macro and runs no exchange, so the picker offers BOTH prompts
/// without the macro-stop notice, and choosing `before` pops both
/// exchanges and prefills `before`.
#[test]
#[serial]
fn macro_enable_is_not_a_rewind_stop_and_two_rows_pop_two_exchanges() {
    let dir = fresh_config_dir("rewind-macro-enable");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    write_two_prompt_macro(&dir, "two-step");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    let at = pty.raw().len();
    pty.send(b"before-q8w\r").unwrap();
    wait_for_squeezed_after(&mut pty, "> before-q8w", at, "the first exchange");
    pty.read_for(Duration::from_millis(2500));
    let at = pty.raw().len();
    pty.send(b".macro enable two-step\r").unwrap();
    pty.read_for(Duration::from_millis(2000));
    let toggled = since(&pty, at);
    assert!(
        !squeezed(&toggled).contains("macro-prompt-one"),
        "`.macro enable two-step` ran the macro instead of enabling it:\n{toggled}"
    );
    let at = pty.raw().len();
    pty.send(b"after-q8w\r").unwrap();
    wait_for_squeezed_after(&mut pty, "> after-q8w", at, "the second exchange");
    pty.read_for(Duration::from_millis(2500));

    let picker_at = open_rewind_picker(&mut pty);
    let picker = repainted_frame(&mut pty, 60, 101);
    let band_flat = squeezed(&picker_band(&picker));
    let mut misses: Vec<String> = Vec::new();
    if !band_flat.contains("after-q8w") || !band_flat.contains("before-q8w") {
        misses.push(format!(
            "the picker does not offer both prompts across `.macro enable`:\n{picker}"
        ));
    }
    if squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
        misses.push(format!(
            "`.macro enable` was taken for a macro turn (the stop notice is shown):\n{}",
            since(&pty, picker_at)
        ));
    }
    // Down to `before-q8w`, Enter: both exchanges go, `before-q8w` is the
    // draft.
    pty.send(b"\x1b[B").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let frame = repainted_frame(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    if flat.contains("Rewindto") {
        misses.push(format!("the picker is still up after Enter:\n{frame}"));
    }
    if flat.contains(">after-q8w") || flat.contains(">before-q8w") {
        misses.push(format!(
            "choosing the older row did not pop both exchanges:\n{frame}"
        ));
    }
    let row = composer_row(&frame, "dryrun:dry-model");
    if row != "before-q8w" {
        misses.push(format!(
            "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
        ));
    }
    clear_any_draft(&mut pty);
    let alive_at = pty.raw().len();
    pty.send(b"still-alive-q8w\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "> still-alive-q8w",
        alive_at,
        "the exchange after",
    );
    pty.read_for(Duration::from_millis(800));
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} rewind expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

/// The newest turn IS the macro: Esc Esc has nothing to offer. Whatever
/// the pane shows (the notice, an empty picker, nothing), Enter and Esc
/// cannot crash it, no macro-internal prompt is offered or prefilled, the
/// macro's exchanges stay, and the composer takes keys afterwards.
#[test]
#[serial]
fn esc_esc_with_nothing_above_the_macro_turn_does_not_crash_or_pop() {
    let dir = fresh_config_dir("rewind-only-macro");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    write_two_prompt_macro(&dir, "two-step");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    let at = pty.raw().len();
    pty.send(b".two-step\r").unwrap();
    wait_for_squeezed_after(&mut pty, "macro-prompt-two", at, "the macro's second step");
    pty.read_for(Duration::from_millis(2500));

    let picker_at = pty.raw().len();
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(150));
    pty.send(b"\x1b").unwrap();
    pty.read_for(HOTKEY_GAP);
    let opened = since(&pty, picker_at);
    let frame = repainted_frame(&mut pty, 60, 101);
    let band_flat = squeezed(&picker_band(&frame));
    assert!(
        !band_flat.contains("macro-prompt-one") && !band_flat.contains("macro-prompt-two"),
        "the picker offers a prompt the macro ran:\n{frame}"
    );
    assert!(
        !band_flat.contains("two-step"),
        "the macro turn is a rewind row:\n{frame}"
    );
    // Enter, then Esc: neither may crash or pop the macro's exchanges.
    let keys_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let after_enter = since(&pty, keys_at);
    assert!(
        !after_enter.contains("tty-probe-done"),
        "Enter on the picker over a macro-only transcript ended the process (exit {}); \
         Esc Esc painted:\n{opened}\n--- after Enter ---\n{after_enter}",
        exit_code(&after_enter)
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(800));
    let after_esc = since(&pty, keys_at);
    assert!(
        !after_esc.contains("tty-probe-done"),
        "Esc after Enter on the picker over a macro-only transcript ended the process (exit {}); \
         Esc Esc painted:\n{opened}\n--- after Enter, Esc ---\n{after_esc}",
        exit_code(&after_esc)
    );
    let frame = repainted_frame(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    assert!(
        flat.contains("macro-prompt-two"),
        "Enter on an empty picker popped the macro's exchanges (Esc Esc painted:\n{opened}\n):\n{frame}"
    );
    let row = composer_row(&frame, "dryrun:dry-model");
    assert!(
        !row.contains("macro-prompt"),
        "a macro-internal prompt was prefilled (composer: `{row}`):\n{frame}"
    );
    clear_any_draft(&mut pty);
    let settled_at = pty.raw().len();
    assert_composer_takes_keys(&mut pty, settled_at, "Esc Esc over a macro-only transcript");
    quit_clearing_any_draft(&mut pty);
}

// ---------------------------------------------------------------------
// A backtick document command's grandchild dies on Esc while its shell
// still leads the group.
// ---------------------------------------------------------------------

/// The pid a script wrote to `path`. The file is created empty before
/// the pid lands in it, so an empty or partial read is retried until the
/// deadline.
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

/// Waits up to `limit` for `pid` to be gone; true when it is.
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

/// The sleepers a test forked, killed by pid when it ends however it
/// ends, so a failed assertion leaves none behind.
struct Sleepers(Vec<i32>);

impl Drop for Sleepers {
    fn drop(&mut self) {
        for &pid in &self.0 {
            // SAFETY: kill takes a pid and a signal number and touches no memory.
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

fn write_script(dir: &Path, name: &str, body: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join(name);
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script.to_str().unwrap().to_string()
}

/// `` .file `sh <script>` `` where the script forks a sleeper and waits
/// on it (a grandchild of the document command's shell), then one where
/// it backgrounds the sleeper and exits, leaving it holding the capture
/// pipe. Each script records the sleeper's pid, so the checks and the
/// cleanup touch that process and no other. Esc ends each turn within
/// ten seconds and the composer takes keys. Both sleepers die with the
/// shell's group within two seconds of Esc: the exited shell is kept a
/// zombie until then, so its pid still names the group it led.
#[test]
#[serial]
fn a_backtick_document_commands_grandchild_dies_on_esc_even_after_its_shell_exited() {
    let dir = fresh_config_dir("backtick-grandchild");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let waits_pid = dir.join("waits.pid");
    let orphans_pid = dir.join("orphans.pid");
    let waits = write_script(
        &dir,
        "gc-waits.sh",
        &format!(
            "#!/bin/sh\nsleep 6174 & echo $! > '{}'\nwait\necho waited\n",
            waits_pid.display()
        ),
    );
    let orphans = write_script(
        &dir,
        "gc-orphans.sh",
        &format!(
            "#!/bin/sh\nsleep 6175 & echo $! > '{}'\necho backgrounded\n",
            orphans_pid.display()
        ),
    );
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    let mut misses: Vec<String> = Vec::new();
    let mut sleepers = Sleepers(Vec::new());
    for (script, pid_file, what) in [
        (waits, &waits_pid, "a waited-on grandchild"),
        (orphans, &orphans_pid, "a backgrounded, orphaned grandchild"),
    ] {
        let turn_at = pty.raw().len();
        pty.send(format!(".file `sh {script}` -- summarize\r").as_bytes())
            .unwrap();
        // The sleeper must be up before Esc, or the test proves nothing.
        let pid = wait_for_pid_in(&mut pty, pid_file, &format!("{what}: the sleeper's pid"));
        sleepers.0.push(pid);
        assert!(
            process_alive(pid),
            "{what}: the sleeper (pid {pid}) is gone before Esc:\n{}",
            since(&pty, turn_at)
        );
        pty.read_for(Duration::from_millis(800));
        pty.send(b"\x1b").unwrap();
        let esc_at = Instant::now();
        if !wait_for_process_gone(pid, Duration::from_secs(2)) {
            misses.push(format!(
                "{what}: the sleeper (pid {pid}) outlived Esc by two seconds:\n{}",
                since(&pty, turn_at)
            ));
        }
        assert_composer_takes_keys(&mut pty, turn_at, &format!("Esc on `.file` with {what}"));
        if esc_at.elapsed() > Duration::from_secs(10) {
            misses.push(format!(
                "{what}: Esc took {:?} to end the backtick turn",
                esc_at.elapsed()
            ));
        }
    }
    quit_and_check(&mut pty);
    assert!(
        misses.is_empty(),
        "{} grandchild expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// Ctrl+D during a turn drops the queued lines.
// ---------------------------------------------------------------------

/// A line queued while a tool sleeps, then Ctrl+D: the session ends
/// cleanly (exit 0, terminal restored) within the deadline, and the
/// queued line never opens a request of its own on the way out.
#[test]
#[serial]
fn ctrl_d_during_a_tool_round_quits_without_sending_the_queued_line() {
    let dir = fresh_config_dir("ctrl-d-queued");
    let _guard = TempDirGuard(dir.clone());
    let tool_a = dir.join("tool-a");
    let fake = FakeLlm::start(vec![
        sleep_call(&tool_a),
        Reply::Text("never-sent-q8w".to_string()),
        Reply::Text("never-sent-either-q8w".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    pty.send(b"first-turn-q8w\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-a-started"),
        "the sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    pty.send(b"ghost-d-q8w\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = repainted_frame(&mut pty, 40, 101);
    assert!(
        squeezed(&frame).contains("pending:1"),
        "the line queued while the tool slept did not show `pending: 1`:\n{frame}"
    );

    let quit_at = Instant::now();
    pty.send(b"\x04").unwrap();
    let transcript = pty
        .wait_for("tty-probe-done", PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("Ctrl+D during the tool round did not end the session: {e}"));
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert!(
        quit_at.elapsed() < Duration::from_secs(15),
        "Ctrl+D took {:?} to end the session",
        quit_at.elapsed()
    );
    // Time for a wrongly flushed queued line to open a request.
    std::thread::sleep(Duration::from_millis(1500));
    let bodies = fake.bodies();
    assert!(
        bodies.iter().all(|b| !b.contains("ghost-d-q8w")),
        "the line queued before Ctrl+D reached the model:\n{}",
        bodies.join("\n---\n")
    );
    assert_eq!(
        fake.served(),
        1,
        "Ctrl+D let the queued line (or the tool's result) open another request:\n{}",
        bodies.join("\n---\n")
    );
    assert!(
        !transcript.contains("never-sent"),
        "a reply scripted behind the quit streamed:\n{transcript}"
    );
}
