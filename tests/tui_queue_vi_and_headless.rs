//! Black-box pins for the bottom pane's queue across an agent switch, the
//! vi layer, the command highlight, Ctrl+R, backtick document commands and
//! the headless prompter's refusal: the built binary on a pseudo-terminal
//! and a scripted fake model where a turn has to be held open. Unix-only
//! and serial like its sibling pty suites.
#![cfg(unix)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process};

use common::fake_llm::{Chunk, FakeLlm, Reply};
use common::pty::{Pty, strip_ansi};
use serial_test::serial;

const PTY_DEADLINE: Duration = Duration::from_secs(20);
/// Past both idle gates (1 s from the last keystroke, 1 s from a modal's
/// appearance).
const HOTKEY_GAP: Duration = Duration::from_millis(1200);
const REPAINT_SETTLE: Duration = Duration::from_millis(500);

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
        "coyote-queue-vi-{scenario}-{}-{unique}-{sequence}",
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
             \x20 You are the plain agent kq7. Reply briefly.\n"
        ),
    )
    .unwrap();
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

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack[from.min(haystack.len())..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|at| from + at)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

/// The last `CSI r;c H` before `at`: its offset and the position it set.
fn cursor_position_before(haystack: &[u8], at: usize) -> Option<(usize, u16, u16)> {
    let mut end = at;
    while let Some(start) = rfind(&haystack[..end], b"\x1b[") {
        let params: Vec<u8> = haystack[start + 2..at]
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
            return Some((start, row, col));
        }
        end = start;
    }
    None
}

/// Every SGR parameter list (`ESC [ ... m`) in `raw`, split into numbers.
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
        let final_byte = rest[end..].chars().next().map_or(1, char::len_utf8);
        rest = &rest[end + final_byte..];
    }
    out
}

/// The SGR lists between the cursor move that started the run of cells
/// `at` belongs to and `at` itself (the style of the text at `at` after a
/// full repaint).
fn sgr_before(raw: &[u8], at: usize) -> Vec<Vec<u16>> {
    let (start, _, _) = cursor_position_before(raw, at)
        .unwrap_or_else(|| panic!("no cursor position precedes offset {at}"));
    sgr_params(&raw[start..at])
}

/// The SGR lists between `from` and `to`.
fn sgr_between(raw: &[u8], from: usize, to: usize) -> Vec<Vec<u16>> {
    sgr_params(&raw[from..to])
}

fn has_indexed_foreground(params: &[Vec<u16>], index: u16) -> bool {
    params
        .iter()
        .any(|p| p.windows(3).any(|w| w == [38, 5, index]))
}

/// Whether any SGR list sets a foreground colour at all: an indexed or
/// truecolor `38`, or one of the basic `30-37` / `90-97` codes. `39`
/// (default) and `0` (reset) are not colours.
fn sets_a_foreground(params: &[Vec<u16>]) -> bool {
    params.iter().any(|p| {
        p.iter()
            .any(|&code| code == 38 || (30..=37).contains(&code) || (90..=97).contains(&code))
    })
}

const GREEN: u16 = 2;
const RED: u16 = 1;

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

/// Files under `root` (recursively) whose contents contain `needle`.
fn files_containing(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if fs::read(&path)
                .is_ok_and(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
            {
                hits.push(path);
            }
        }
    }
    hits
}

// ---------------------------------------------------------------------
// Queued lines survive a `.agent` switch.
// ---------------------------------------------------------------------

/// A `.agent` command and a text line queued while the model streams
/// (no tool round, so neither is handed over mid-turn): once the reply
/// ends the command runs as its own turn and the text still reaches the
/// model as the next prompt, on the switched agent's context. Regression
/// target: the carried-over lines went onto the old context's queue and
/// were silently dropped after the `.agent` turn.
#[test]
#[serial]
fn a_text_line_queued_behind_a_dot_agent_command_still_reaches_the_model() {
    let dir = fresh_config_dir("queue-across-agent");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("release");
    let fake = FakeLlm::start(vec![
        Reply::Streamed(vec![
            Chunk::Text("part one kq7 ".to_string()),
            Chunk::WaitFor(release.clone()),
            Chunk::Text("reply one kq7".to_string()),
        ]),
        Reply::Text("reply two kq7".to_string()),
        Reply::Text("reply three kq7".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_plain_agent(&dir, "plain-agent");
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let turn_at = pty.raw().len();
    pty.send(b"first prompt kq7\r").unwrap();
    wait_for_squeezed_after(&mut pty, "part one kq7", turn_at, "the held stream");
    pty.read_for(Duration::from_millis(300));

    let queued_at = pty.raw().len();
    pty.send(b".agent plain-agent\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(b"hello-after-agent-kq7\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = repainted_frame(&mut pty, 40, 101);
    assert!(
        squeezed(&frame).contains("pending:2"),
        "the command and the text queued while the model streamed did not show `pending: 2`:\n{frame}"
    );

    fs::write(&release, b"").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "reply one kq7",
        queued_at,
        "the end of the held reply",
    );
    // The queued text's own reply.
    wait_for_squeezed_after(
        &mut pty,
        "reply two kq7",
        queued_at,
        "the reply to the text queued behind `.agent`",
    );
    wait_until_no_pending(&mut pty);

    let bodies = fake.bodies();
    let carried = bodies
        .iter()
        .position(|b| b.contains("hello-after-agent-kq7"))
        .unwrap_or_else(|| {
            panic!(
                "the text queued behind `.agent` never reached the model; {} requests:\n{}",
                bodies.len(),
                bodies.join("\n---\n")
            )
        });
    assert!(
        bodies[carried].contains("plain agent kq7"),
        "the queued text was sent before the `.agent` switch (request {} carries no agent \
         instructions):\n{}",
        carried + 1,
        bodies[carried]
    );

    // The switch is in effect for what the user types next, too.
    let after_at = pty.raw().len();
    pty.send(b"after-switch-kq7\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "reply three kq7",
        after_at,
        "the reply after the switch",
    );
    let bodies = fake.bodies();
    let last = bodies.last().unwrap();
    assert!(
        last.contains("after-switch-kq7") && last.contains("plain agent kq7"),
        "the prompt typed after the switch did not go out on the agent's context:\n{last}"
    );
    pty.read_for(Duration::from_millis(500));
    quit_and_check(&mut pty);
}

// ---------------------------------------------------------------------
// A headless macro run cannot save a role by answering itself.
// ---------------------------------------------------------------------

/// `--headless --macro` with a temp role and a bare `.save role`: the
/// prompter has no human, so the name question is refused and NO role
/// file is written (before the fix the JSON refusal was taken as the
/// role's name).
#[test]
#[serial]
fn a_headless_macro_save_role_on_a_temp_role_writes_no_role_file() {
    let dir = fresh_config_dir("headless-save-role");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let macros = dir.join("macros");
    fs::create_dir_all(&macros).unwrap();
    fs::write(
        macros.join("temp-save.yaml"),
        "description: a temp role then a bare save\n\
         steps:\n\
         \x20 - .temp-role be terse kq7\n\
         \x20 - .save role\n",
    )
    .unwrap();
    let roles = dir.join("roles");
    fs::create_dir_all(&roles).unwrap();

    let mut cmd = Command::new(built_binary());
    base_env(&mut cmd, &dir);
    cmd.args(["--headless", "--macro", "temp-save"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let deadline = Instant::now() + PTY_DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("`--headless --macro temp-save` hung on the role-name question");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let output = child.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let written: Vec<PathBuf> = fs::read_dir(&roles)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert!(
        written.is_empty(),
        "a headless `.save role` on a temp role wrote {written:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    let everything = format!("{stdout}\n{stderr}");
    assert!(
        !everything.contains("Saved role") && !everything.contains("saved role"),
        "the headless run claims a saved role (exit {status}):\n{everything}"
    );
    assert!(
        everything.to_lowercase().contains("human")
            || everything.to_lowercase().contains("headless")
            || everything.to_lowercase().contains("not available")
            || everything.to_lowercase().contains("refus"),
        "the headless run neither saved nor explained the refusal (exit {status}):\n{everything}"
    );
}

// ---------------------------------------------------------------------
// The command highlight stops at a word boundary.
// ---------------------------------------------------------------------

/// Reads the style the field paints `typed` with after a repaint; closes
/// the palette band first so the field's copy is the one found. The
/// composer is painted below the transcript (whose welcome block spells
/// `.help`), so the LAST copy after the repaint is the field's.
fn command_style(pty: &mut Pty, typed: &str, cols: u16) -> (Vec<Vec<u16>>, String) {
    pty.send(typed.as_bytes()).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    let repainted = repaint(pty, 40, cols);
    let frame = since(pty, repainted);
    let at = rfind_after(pty.raw(), typed.as_bytes(), repainted)
        .unwrap_or_else(|| panic!("`{typed}` was not repainted:\n{frame}"));
    (sgr_before(pty.raw(), at), frame)
}

/// The last occurrence of `needle` at or after `from`.
fn rfind_after(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let from = from.min(haystack.len());
    rfind(&haystack[from..], needle).map(|at| from + at)
}

/// Clears the field (one Ctrl+C on a draft).
fn clear_draft(pty: &mut Pty) {
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(400));
}

/// `.helpx` is not a command even though `.help` is its prefix: red, not
/// green. `.help x` keeps `.help` green with the argument plain. Same
/// under the slash prefix.
#[test]
#[serial]
fn a_command_prefix_followed_by_more_letters_is_painted_red_not_green() {
    for (prefix_yaml, sigil) in [("", '.'), ("command_prefix: slash\n", '/')] {
        let dir = fresh_config_dir(&format!("word-boundary-{}", sigil as u8));
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir, prefix_yaml);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();

        let known = format!("{sigil}help");
        let (style, frame) = command_style(&mut pty, &known, 101);
        assert!(
            has_indexed_foreground(&style, GREEN),
            "`{known}` was not painted green: {style:?}\n{frame}"
        );
        clear_draft(&mut pty);

        let extended = format!("{sigil}helpx");
        let (style, frame) = command_style(&mut pty, &extended, 100);
        assert!(
            has_indexed_foreground(&style, RED),
            "`{extended}` (a known command plus letters) was not painted red: {style:?}\n{frame}"
        );
        assert!(
            !has_indexed_foreground(&style, GREEN),
            "`{extended}` was painted green as if `{known}` matched by prefix: {style:?}\n{frame}"
        );
        clear_draft(&mut pty);

        let with_arg = format!("{sigil}help x");
        pty.send(with_arg.as_bytes()).unwrap();
        pty.read_for(Duration::from_millis(300));
        pty.send(b"\x1b").unwrap();
        pty.read_for(Duration::from_millis(300));
        let repainted = repaint(&mut pty, 40, 101);
        let frame = since(&pty, repainted);
        let name_at = rfind_after(pty.raw(), known.as_bytes(), repainted)
            .unwrap_or_else(|| panic!("`{with_arg}` was not repainted:\n{frame}"));
        let name_style = sgr_before(pty.raw(), name_at);
        assert!(
            has_indexed_foreground(&name_style, GREEN),
            "`{known}` before an argument was not painted green: {name_style:?}\n{frame}"
        );
        // Between the command word and its argument the colour is reset:
        // the argument is plain, neither red nor green.
        let arg_at = find(pty.raw(), b" x", name_at + known.len())
            .unwrap_or_else(|| panic!("the argument of `{with_arg}` was not repainted:\n{frame}"));
        let arg_style = sgr_between(pty.raw(), name_at + known.len(), arg_at + 2);
        assert!(
            !sets_a_foreground(&arg_style),
            "the argument after `{known}` is painted, not plain: {arg_style:?}\n{frame}"
        );
        clear_draft(&mut pty);
        pty.read_for(HOTKEY_GAP);
        quit_and_check(&mut pty);
    }
}

// ---------------------------------------------------------------------
// Ctrl+R history search; the history file is private.
// ---------------------------------------------------------------------

/// After one exchange, Ctrl+R opens a history search; typing part of the
/// earlier prompt and Enter puts that prompt in the composer without
/// submitting it; Ctrl+R then Esc leaves the composer as it was. The
/// history file the pane writes is mode 0600.
#[test]
#[serial]
fn ctrl_r_recalls_an_earlier_prompt_into_the_composer_and_esc_backs_out() {
    let dir = fresh_config_dir("ctrl-r");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    let first_at = pty.raw().len();
    pty.send(b"alpha-history-kq7\r").unwrap();
    wait_for_squeezed_after(&mut pty, "> alpha-history-kq7", first_at, "the exchange");
    pty.read_for(Duration::from_millis(2500));
    let second_at = pty.raw().len();
    pty.send(b"bravo-history-kq7\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "> bravo-history-kq7",
        second_at,
        "the second exchange",
    );
    pty.read_for(Duration::from_millis(2500));

    // Ctrl+R, Esc: nothing inserted, UI live.
    let search_at = pty.raw().len();
    pty.send(b"\x12").unwrap();
    pty.read_for(Duration::from_millis(600));
    let opened = since(&pty, search_at);
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = repainted_frame(&mut pty, 40, 101);
    assert!(
        squeezed(&frame).contains("Typeapromptora.command"),
        "after Ctrl+R then Esc the composer is not back to its empty placeholder \
         (Ctrl+R painted:\n{opened}\n):\n{frame}"
    );

    // Ctrl+R, type `alpha`, Enter: the older prompt lands in the field.
    let search_at = pty.raw().len();
    pty.send(b"\x12").unwrap();
    pty.read_for(Duration::from_millis(600));
    pty.send(b"alpha").unwrap();
    pty.read_for(Duration::from_millis(600));
    let narrowed = since(&pty, search_at);
    assert!(
        squeezed(&narrowed).contains("alpha-history-kq7"),
        "the history search does not list the matching prompt:\n{narrowed}"
    );
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = repainted_frame(&mut pty, 40, 102);
    let flat = squeezed(&frame);
    assert!(
        !flat.contains("Typeapromptora.command"),
        "Enter on the history match left the composer empty:\n{frame}"
    );
    assert_eq!(
        flat.matches(">alpha-history-kq7").count(),
        1,
        "Enter on the history match submitted the prompt instead of recalling it:\n{frame}"
    );
    assert_eq!(
        composer_row(&frame),
        "alpha-history-kq7",
        "the recalled prompt is not the composer's line:\n{frame}"
    );

    quit_clearing_any_draft(&mut pty);

    let history: Vec<PathBuf> = files_containing(&dir.join("cache"), "alpha-history-kq7");
    assert!(
        !history.is_empty(),
        "no history file under the cache dir records the prompts (looked for alpha-history-kq7)"
    );
    for file in &history {
        let mode = fs::metadata(file).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "history file {} is mode {mode:o}, not 0600",
            file.display()
        );
    }
}

// ---------------------------------------------------------------------
// The vi layer's minimum set.
// ---------------------------------------------------------------------

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

/// `keybindings: vi`: Esc leaves insert mode; `0 x` deletes the first
/// character, `$ x` the last, `A` appends, `dd` clears the line, `i`
/// returns to insert. Every step is observed before the verdict so one
/// motion's miss does not hide the others.
#[test]
#[serial]
fn vi_keybindings_edit_the_composer_in_normal_mode() {
    let dir = fresh_config_dir("vi-layer");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "keybindings: vi\n");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let mut misses: Vec<String> = Vec::new();
    let mut toggle = false;

    pty.send(b"hello world").unwrap();
    pty.read_for(Duration::from_millis(400));
    key(&mut pty, b"\x1b");
    let frame = next_frame(&mut pty, &mut toggle);
    if !frame.contains("NORMAL") {
        misses.push(format!("Esc did not show the NORMAL mode title:\n{frame}"));
    }
    key(&mut pty, b"0");
    key(&mut pty, b"x");
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    if !flat.contains("elloworld") || flat.contains("helloworld") {
        misses.push(format!(
            "Esc 0 x did not delete the first character:\n{frame}"
        ));
    }

    // From here the checks read the composer row itself: squeezing the
    // whole frame glues the row to the footer that follows it, so
    // `ello worl` + `dryrun` would read as `elloworld`.
    // `$` in vi lands ON the last character, so `x` deletes it.
    key(&mut pty, b"$");
    key(&mut pty, b"x");
    let frame = next_frame(&mut pty, &mut toggle);
    if composer_row(&frame) != "ello worl" {
        misses.push(format!("$ x did not delete the last character:\n{frame}"));
    }

    // `h` then `x`: one left of the cursor, deletes that character.
    key(&mut pty, b"h");
    key(&mut pty, b"x");
    let frame = next_frame(&mut pty, &mut toggle);
    if composer_row(&frame) != "ello wol" {
        misses.push(format!(
            "h x did not delete the character left of the cursor (expected `ello wol`):\n{frame}"
        ));
    }

    key(&mut pty, b"A");
    key(&mut pty, b"!");
    let frame = next_frame(&mut pty, &mut toggle);
    if composer_row(&frame) != "ello wol!" {
        misses.push(format!("A did not append at the end of the line:\n{frame}"));
    }

    key(&mut pty, b"\x1b");
    key(&mut pty, b"d");
    key(&mut pty, b"d");
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    if flat.contains("ellowor") {
        misses.push(format!("dd did not clear the line:\n{frame}"));
    }

    key(&mut pty, b"i");
    pty.send(b"insert-again-kq7").unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = next_frame(&mut pty, &mut toggle);
    if !squeezed(&frame).contains("insert-again-kq7") {
        misses.push(format!("i did not return to insert mode:\n{frame}"));
    }

    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} vi-layer step(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// Backtick document commands under the TUI.
// ---------------------------------------------------------------------

/// `` .file `cat` `` runs the command with stdin closed: `cat` exits at
/// once instead of waiting on the tty, the turn ends and the composer
/// takes keys. `` .file `sleep N` `` is abortable: Esc ends the turn and
/// no sleeper outlives the session.
#[test]
#[serial]
fn backtick_document_commands_never_read_the_tty_and_esc_aborts_them() {
    let dir = fresh_config_dir("backtick-doc");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();

    let cat_at = pty.raw().len();
    pty.send(b".file `cat` -- summarize\r").unwrap();
    let started = Instant::now();
    // Keys typed while cat waited on the tty would be eaten by cat.
    pty.read_for(Duration::from_millis(1500));
    assert_composer_takes_keys(&mut pty, cat_at, "`.file `cat``");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the backtick `cat` held the turn for {:?}",
        started.elapsed()
    );

    let sleep_at = pty.raw().len();
    pty.send(b".file `sleep 6173` -- summarize\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    pty.send(b"\x1b").unwrap();
    let esc_at = Instant::now();
    assert_composer_takes_keys(&mut pty, sleep_at, "Esc on `.file `sleep``");
    assert!(
        esc_at.elapsed() < Duration::from_secs(10),
        "Esc took {:?} to end the backtick turn",
        esc_at.elapsed()
    );

    quit_and_check(&mut pty);
    let leftover = Command::new("pgrep")
        .args(["-f", "sleep 6173"])
        .output()
        .unwrap();
    let pids = String::from_utf8_lossy(&leftover.stdout);
    if !pids.trim().is_empty() {
        let _ = Command::new("pkill").args(["-f", "sleep 6173"]).status();
    }
    assert!(
        pids.trim().is_empty(),
        "the aborted backtick command outlived the session (pids {})",
        pids.trim()
    );
}

// ---------------------------------------------------------------------
// The palette's `.macro` arguments list the user's macros cold.
// ---------------------------------------------------------------------

/// A macro in the config dir is offered as a `.macro` argument on a cold
/// start, before any command has run.
#[test]
#[serial]
fn the_palette_offers_a_config_dir_macro_as_a_dot_macro_argument_cold() {
    let dir = fresh_config_dir("macro-args");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir, "");
    let macros = dir.join("macros");
    fs::create_dir_all(&macros).unwrap();
    fs::write(
        macros.join("cold-macro-kq7.yaml"),
        "description: a macro of the config dir\n\
         steps:\n\
         \x20 - .info\n",
    )
    .unwrap();
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 140).unwrap();
    pty.read_for(Duration::from_millis(500));

    let typed_at = pty.raw().len();
    pty.send(b".macro ").unwrap();
    pty.read_for(Duration::from_millis(600));
    repaint(&mut pty, 60, 141);
    let frame = since(&pty, typed_at);
    assert!(
        squeezed(&frame).contains("cold-macro-kq7"),
        "`.macro ` on a cold start does not offer the config dir's macro:\n{frame}"
    );

    pty.send(b"col").unwrap();
    pty.read_for(Duration::from_millis(600));
    let narrowed_at = repaint(&mut pty, 60, 140);
    let narrowed = since(&pty, narrowed_at);
    assert!(
        squeezed(&narrowed).contains("cold-macro-kq7"),
        "`.macro col` does not narrow to the macro:\n{narrowed}"
    );

    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    quit_clearing_any_draft(&mut pty);
}

// ---------------------------------------------------------------------
// The placeholder names the configured command key.
// ---------------------------------------------------------------------

/// Under `command_prefix: slash` the empty composer's placeholder points
/// at `/` for commands, not `.`.
#[test]
#[serial]
fn the_placeholder_names_the_configured_command_prefix() {
    for (prefix_yaml, sigil) in [("command_prefix: slash\n", '/'), ("", '.')] {
        let dir = fresh_config_dir(&format!("placeholder-{}", sigil as u8));
        let _guard = TempDirGuard(dir.clone());
        write_dry_run_config(&dir, prefix_yaml);
        let mut pty = spawn_tui(&dir, &[]);
        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        let frame = repainted_frame(&mut pty, 40, 101);
        let flat = squeezed(&frame);
        let other = if sigil == '/' { '.' } else { '/' };
        assert!(
            flat.contains(&format!("Typeapromptora{sigil}command")),
            "under `{sigil}` the placeholder does not point at `{sigil}` for commands:\n{frame}"
        );
        assert!(
            !flat.contains(&format!("Typeapromptora{other}command")),
            "under `{sigil}` the placeholder still points at `{other}`:\n{frame}"
        );
        quit_and_check(&mut pty);
    }
}
