//! Suspend, signal, resize and keyboard-protocol cases for the full-screen
//! UI. Each case runs the binary cold on a pseudo-terminal under the same
//! bash wrappers `tui_binary.rs` uses, so the exit code and the tty flags
//! are reported after every exit path. Unix-only for the same reasons.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process};

use common::fake_llm::{Chunk, FakeLlm, HOLD_LIMIT, Reply};
use common::pty::{Pty, strip_ansi};
use serial_test::serial;

const PTY_DEADLINE: Duration = Duration::from_secs(20);
/// Hotkeys on a modal count only once both the last keystroke and the
/// modal's appearance are at least HOTKEY_IDLE (1 s) old; 1200 ms settles
/// both.
const HOTKEY_GAP: Duration = Duration::from_millis(1200);
const ENTER_ALT: &[u8] = b"\x1b[?1049h";
const LEAVE_ALT: &[u8] = b"\x1b[?1049l";
const PASTE_ON: &[u8] = b"\x1b[?2004h";
const PASTE_OFF: &[u8] = b"\x1b[?2004l";

const TTY_REPORT: &str = "echo; echo \"exit=$rc\"; \
     echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done";
const WAIT_WRAPPER: &str =
    "'{coyote}' <{stdin} & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {report}";
/// The user's shell shape: the binary is the foreground job; when it
/// stops the wrapper reports what it sees and resumes it with `fg` after
/// a second.
const JOB_CONTROL_WRAPPER: &str = "set -m; '{coyote}' <{stdin}; rc=$?; \
     while [ -n \"$(jobs -s)\" ]; do echo; echo job-stopped; \
     echo \"stopped-tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo stopped-probe-done; \
     sleep 1; fg %1; rc=$?; done; {report}";

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
        "coyote-tui-signals-{scenario}-{}-{unique}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

fn write_dry_run_config(dir: &Path) {
    write_dry_run_config_with(dir, "save_session: false\n");
}

/// The dry-run config with `extra` as top-level YAML in place of the
/// default `save_session: false` line (omit it and the exit path asks
/// whether to save a dirty session).
fn write_dry_run_config_with(dir: &Path, extra: &str) {
    let vault_pass = dir.join("vault-pass");
    fs::write(&vault_pass, "test-password\n").unwrap();
    fs::write(
        dir.join("config.yaml"),
        format!(
            "model: dryrun:dry-model\n\
             dry_run: true\n\
             stream: true\n\
             {extra}\
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

/// The fake-LLM config, prompt-free on every exit path; `extra` is
/// appended as top-level YAML.
fn write_fake_llm_config(dir: &Path, fake: &FakeLlm, extra: &str) {
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
             memory: false\n\
             {extra}",
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

fn built_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_coyote"))
}

fn spawn_tui(dir: &Path, extra_env: &[(&str, &str)]) -> Pty {
    spawn_tui_under(dir, extra_env, WAIT_WRAPPER)
}

fn spawn_tui_under(dir: &Path, extra_env: &[(&str, &str)], wrapper: &str) -> Pty {
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
                .replace("{coyote}", built_binary().to_str().unwrap())
                .replace("{stdin}", "/dev/tty")
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

/// The binary with no terminal at all (stdin from /dev/null, stdout and
/// stderr piped), the way a script runs it; `COYOTE_UI` is whatever the
/// case passes.
fn run_piped_with_ui(dir: &Path, ui: &str, args: &[&str]) -> std::process::Output {
    let fixture_bin = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin");
    let path = env::join_paths(
        iter::once(fixture_bin).chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    // Spawned by its own name so the harness pin sees this spawn paired
    // with the two config env lines below.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_coyote"));
    for (key, _) in env::vars_os() {
        if key.to_string_lossy().starts_with("COYOTE_") {
            cmd.env_remove(&key);
        }
    }
    cmd.args(args)
        .current_dir(dir)
        .env("COYOTE_CONFIG_DIR", dir)
        .env("COYOTE_CACHE_DIR", dir.join("cache"))
        .env("COYOTE_UI", ui)
        .env("HOME", dir.join("home"))
        .env("TERM", "xterm-256color")
        .env("PATH", path)
        .env_remove("IS_SANDBOX")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.output().unwrap()
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

fn child_pid(transcript: &str) -> i32 {
    transcript
        .lines()
        .find_map(|line| line.strip_prefix("cpid="))
        .unwrap_or_else(|| panic!("no cpid= line in the transcript:\n{transcript}"))
        .trim()
        .parse()
        .unwrap()
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let from = from.min(haystack.len());
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|at| from + at)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

fn squeezed(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn since(pty: &Pty, from: usize) -> String {
    strip_ansi(&String::from_utf8_lossy(
        &pty.raw()[from.min(pty.raw().len())..],
    ))
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
            "{what} ({}) never appeared:\n{text}",
            path.display()
        );
    }
}

fn process_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 touches no memory; it only checks the pid.
    unsafe { libc::kill(pid, 0) == 0 }
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

/// Drives Ctrl-Z under the job-control wrapper up to the point where the
/// shell has the terminal back: alternate screen and bracketed paste gone
/// before the shell spoke, cooked tty. Returns the raw offset of the
/// shell's `job-stopped` line.
fn suspend_and_check_handback(pty: &mut Pty, before_suspend: usize) -> usize {
    pty.send(b"\x1a").unwrap();
    let stopped = pty
        .wait_for("stopped-probe-done", PTY_DEADLINE)
        .expect("Ctrl-Z did not stop the job (the shell never regained the terminal)");
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
fn wait_for_re_entry(pty: &mut Pty, from: usize) -> usize {
    let re_entered = pty
        .wait_for_raw(ENTER_ALT, from, PTY_DEADLINE)
        .expect("the alternate screen was not re-entered");
    pty.wait_for_raw(PASTE_ON, re_entered, PTY_DEADLINE)
        .expect("bracketed paste was not re-enabled on re-entry");
    pty.read_for(Duration::from_millis(300));
    re_entered
}

/// The screen the UI comes back to shows its chrome again: the model and
/// the session tokens plus whatever the composer is expected to show
/// (`composer`: the placeholder hint when it is empty, the draft otherwise).
fn assert_repainted_after(
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
// draft is not lost and the running turn is not disturbed.
// ---------------------------------------------------------------------

/// Keys are handled while a work task sleeps; a submit while the context
/// is away restores the draft rather than losing it, and never ends the
/// turn or the process. Whichever way the UI holds the draft (back in the
/// composer, or sent as the next prompt once the turn is over), the text
/// reaches the model exactly once and the UI stays live throughout.
#[test]
#[serial]
fn enter_on_a_draft_while_a_tool_sleeps_loses_nothing_and_disturbs_nothing() {
    let dir = fresh_config_dir("enter-mid-tool");
    let _guard = TempDirGuard(dir.clone());
    let marker = dir.join("tool");
    let fake = FakeLlm::start(vec![
        Reply::ToolCall {
            name: "fixture_sleep".to_string(),
            arguments: format!("{{\"marker\": \"{}\"}}", marker.display()),
        },
        Reply::Text("fixture done r12-a".to_string()),
        Reply::Text("draft reply r12-b".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake, "");
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
    pty.send(b"draft-r12-d\r").unwrap();
    let after_enter = pty.read_for(Duration::from_millis(1000));
    let mid = since(&pty, typed_at);
    assert!(
        !mid.contains("exit=") && find(pty.raw(), LEAVE_ALT, typed_at).is_none(),
        "Enter on a draft while a tool ran ended the UI (a panic or a quit):\n{after_enter}"
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

    // Either the draft is back in the composer (Ctrl+L repaints the whole
    // screen, so it is visible from here on) or it already went out as
    // the next prompt; in both cases it must not be gone.
    let repaint_at = pty.raw().len();
    pty.send(b"\x0c").unwrap();
    pty.read_for(Duration::from_millis(600));
    let repainted = since(&pty, repaint_at);
    let in_composer = squeezed(&repainted).contains("draft-r12-d");
    if in_composer {
        pty.send(b"\r").unwrap();
    } else {
        assert!(
            fake.served() >= 2 && squeezed(&since(&pty, typed_at)).contains("draft-r12-d"),
            "the draft typed while the tool ran was lost: it is neither in the composer nor \
             in the transcript (model calls served: {}):\n{repainted}",
            fake.served()
        );
    }
    let transcript = wait_for_squeezed_after(
        &mut pty,
        "draft reply r12-b",
        typed_at,
        "the reply to the draft",
    );
    assert_eq!(
        fake.served(),
        3,
        "the draft reached the model {} times instead of once:\n{transcript}",
        fake.served().saturating_sub(2)
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
}

// ---------------------------------------------------------------------
// The documented `COYOTE_UI` contract: a value that is neither `repl`
// nor `tui` is reported by name with the accepted values, and nothing
// starts (no alternate screen, no REPL banner).
// ---------------------------------------------------------------------

/// `COYOTE_UI=bogus coyote --info` reports
/// `Invalid value 'bogus', expected one of: repl, tui` and exits non-zero
/// without touching the alternate screen: the config layer parses the
/// variable on startup, so a typo never falls through to either shell.
#[test]
#[serial]
fn an_unknown_coyote_ui_value_is_reported_by_name_with_the_accepted_values() {
    let dir = fresh_config_dir("bogus-ui");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);

    let out = run_piped_with_ui(&dir, "bogus", &["--info"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let all = format!("{stdout}\n{stderr}");

    assert!(
        find(&out.stdout, ENTER_ALT, 0).is_none() && find(&out.stderr, ENTER_ALT, 0).is_none(),
        "COYOTE_UI=bogus entered the alternate screen:\n{all}"
    );
    assert!(
        !out.status.success(),
        "COYOTE_UI=bogus --info exited 0 instead of refusing the value:\n{all}"
    );
    assert!(
        all.contains("Invalid value 'bogus', expected one of: repl, tui"),
        "the refusal did not name the value and the accepted values:\n{all}"
    );
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
    write_dry_run_config_with(&dir, "");
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
    write_fake_llm_config(&dir, &fake, "");
    let mut pty = spawn_tui_under(&dir, &[], JOB_CONTROL_WRAPPER);

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
    write_dry_run_config(&dir);
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
    assert_repainted_after(
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
    write_dry_run_config(&dir);
    let mut pty = spawn_tui_under(&dir, &[], JOB_CONTROL_WRAPPER);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let before_suspend = pty.raw().len();
    let stopped_marker = suspend_and_check_handback(&mut pty, before_suspend);
    pty.resize(40, 60).unwrap();

    let re_entered = wait_for_re_entry(&mut pty, stopped_marker);
    assert_repainted_after(
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
    write_fake_llm_config(
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
    assert!(process_alive(loader), "the loader exited before the signal");
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
    while process_alive(loader) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !process_alive(loader),
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
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    let probe = pty
        .wait_for_raw(b"\x1b[?u", 0, PTY_DEADLINE)
        .expect("the keyboard-enhancement support probe (CSI ? u) was never sent");
    pty.wait_for_raw(b"\x1b[c", probe, PTY_DEADLINE)
        .expect("the probe never asked for primary device attributes");
    pty.send(b"\x1b[?0u\x1b[?62;22c").unwrap();
    pty.wait_for("no session", PTY_DEADLINE)
        .expect("the UI never came up after the terminal answered the keyboard probe");
    let push = find_keyboard_stack_op(pty.raw(), b'>', 0)
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
    let leave_alt = rfind(pty.raw(), LEAVE_ALT).expect("the alternate screen was never left");
    let mut pop = None;
    let mut from = 0;
    while let Some(at) = find_keyboard_stack_op(pty.raw(), b'<', from) {
        pop = Some(at);
        from = at + 1;
    }
    let pop = pop.expect("flags pushed but never popped");
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
    write_dry_run_config(&dir);
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
