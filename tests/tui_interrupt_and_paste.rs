//! Interrupt, quit, hotkey-gate and paste cases for the full-screen UI.
//! Each case runs the binary cold on a pseudo-terminal under the same bash
//! wrapper
//! `tui_binary.rs` uses, so the exit code and the tty flags are reported
//! after every exit path. Unix-only for the same reasons.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
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

const TTY_REPORT: &str = "echo; echo \"exit=$rc\"; \
     echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done";
const WAIT_WRAPPER: &str =
    "'{coyote}' <{stdin} & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {report}";

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
        "coyote-tui-interrupt-{scenario}-{}-{unique}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

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

/// The fake-LLM config; `save_session` is written only when `prompt_free`
/// is set, so a dirty session asks "Save session?" on the way out otherwise.
fn write_fake_llm_config(dir: &Path, fake: &FakeLlm, prompt_free: bool) {
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

fn write_tool(dir: &Path, file: &str, body: &str) {
    let tools = dir.join("functions").join("tools");
    fs::create_dir_all(&tools).unwrap();
    fs::write(tools.join(file), body).unwrap();
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
    \x20   for _ in $(seq 1 600); do\n\
    \x20       [ -e \"${marker}-release\" ] && break\n\
    \x20       sleep 0.05\n\
    \x20   done\n\
    \x20   echo \"fixture slept k4s\" >> \"$LLM_OUTPUT\"\n\
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

/// The 1-based (row, col) of the cursor-position sequence that placed each
/// occurrence of `needle` from `from` on: the screen row it was drawn on.
fn painted_at(haystack: &[u8], needle: &[u8], from: usize) -> Vec<(u16, u16)> {
    let mut positions = Vec::new();
    let mut at = from;
    while let Some(hit) = find(haystack, needle, at) {
        let mut end = hit;
        while let Some(start) = rfind(&haystack[..end], b"\x1b[") {
            let params: Vec<u8> = haystack[start + 2..hit]
                .iter()
                .copied()
                .take_while(|b| b.is_ascii_digit() || *b == b';')
                .collect();
            if haystack.get(start + 2 + params.len()) == Some(&b'H')
                && let Some((row, col)) = std::str::from_utf8(&params)
                    .ok()
                    .and_then(|p| p.split_once(';'))
                && let (Ok(row), Ok(col)) = (row.parse(), col.parse())
            {
                positions.push((row, col));
                break;
            }
            end = start;
        }
        at = hit + 1;
    }
    positions
}

fn wait_for_squeezed_after(pty: &mut Pty, marker: &str, from: usize, what: &str) -> String {
    let deadline = Instant::now() + PTY_DEADLINE;
    let wanted = squeezed(marker);
    loop {
        pty.read_for(Duration::from_millis(200));
        let text = strip_ansi(&String::from_utf8_lossy(
            &pty.raw()[from.min(pty.raw().len())..],
        ));
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

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

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
    write_fake_llm_config(&dir, &fake, true);
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
    let mut pty = spawn_tui_under(&dir, &[], &wrapper);

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
    write_fake_llm_config(&dir, &fake, false);
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
    write_fake_llm_config(&dir, &fake, true);
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
    write_dry_run_config(&dir);
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
    write_fake_llm_config(&dir, &fake, true);
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
    write_fake_llm_config(&dir, &fake, true);
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
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    // Windows line ends: one break per pair.
    let before = paste_two_lines(&mut pty, "crlf-top-k4m", "\r\n", "crlf-bottom-p7w");
    assert_stacked_once(&pty, before, "crlf-top-k4m", "crlf-bottom-p7w", "\r\n");

    // Enter submits the two-line draft; the dry-run echo carries both.
    let sent_at = pty.raw().len();
    pty.send(b"\r").unwrap();
    let shown = wait_for_squeezed_after(&mut pty, "Working", sent_at, "the working row");
    let _ = shown;
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
    write_dry_run_config(&dir);
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
