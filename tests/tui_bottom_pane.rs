//! Real-process checks of the bottom pane: the composer field's surface
//! and command colours as the terminal sees them, and the three prompt
//! modals a tool can ask for through the prompt channel (select, checkbox,
//! password). Colours and backgrounds are read from the raw escape stream,
//! so the file runs the binary on a pseudo-terminal and is unix-only. The
//! cases run one at a time: each spawn forks a session leader and takes a
//! controlling tty.
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{env, fs, iter, process};

use common::fake_llm::{FakeLlm, Reply};
use common::pty::{Pty, strip_ansi};
use serial_test::serial;

/// Covers the keyboard-enhancement probe, which waits two seconds for an
/// answer a pseudo-terminal never gives.
const PTY_DEADLINE: Duration = Duration::from_secs(20);
/// Hotkeys on a modal count only once both the last keystroke and the
/// modal's appearance are at least HOTKEY_IDLE (1 s) old; 1200 ms settles
/// both.
const HOTKEY_GAP: Duration = Duration::from_millis(1200);
/// Long enough for a resize to reach the binary as SIGWINCH and for the
/// frame it schedules to be written.
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
        "coyote-tui-bottom-pane-{scenario}-{}-{unique}-{sequence}",
        process::id()
    ));
    fs::create_dir_all(dir.join("home")).unwrap();
    fs::create_dir_all(dir.join("sessions")).unwrap();
    dir
}

/// A dry-run config that never asks to save a session, so every exit path
/// is prompt-free. Paths land in single-quoted YAML scalars.
fn write_dry_run_config(dir: &Path) {
    write_config(dir, "save_session: false\n");
}

fn write_config(dir: &Path, extra: &str) {
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
             {extra}\
             workspace_instructions: false\n\
             memory: false\n",
            vault_pass = vault_pass.display(),
        ),
    )
    .unwrap();
}

/// The dry-run config with its only client pointed at `fake` instead, so a
/// turn can carry a tool call.
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

/// Drops one bash tool into the config's `functions/tools` dir; the binary
/// builds its shim on startup. No `set -e`: the fixture `tput` on PATH
/// exits 1 for everything but `cols`/`lines`, and the prompt helper's
/// colour assignments would abort the tool under it.
fn write_tool(dir: &Path, file: &str, body: &str) {
    let tools = dir.join("functions").join("tools");
    fs::create_dir_all(&tools).unwrap();
    fs::write(tools.join(file), body).unwrap();
}

/// Report line emitted by the wrapper once the binary has exited. The
/// alternate screen leaves no newline behind once escapes are stripped,
/// so the report starts a fresh line first.
const TTY_REPORT: &str = "echo; echo \"exit=$rc\"; \
     echo \"tty-flags: $(stty -a </dev/tty | tr -s '\\n\\t' '  ')\"; echo tty-probe-done";

/// One process group, `wait` for the child, then the report.
const WAIT_WRAPPER: &str =
    "'{coyote}'{args} <{stdin} & cpid=$!; echo \"cpid=$cpid\"; wait $cpid; rc=$?; {report}";

fn built_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_coyote"))
}

/// Runs the binary in the background of a bash wrapper on a fresh pty with
/// the tty on stdin.
fn spawn_tui(dir: &Path, extra_env: &[(&str, &str)]) -> Pty {
    spawn_binary_under(built_binary(), dir, extra_env, WAIT_WRAPPER, "/dev/tty", "")
}

fn spawn_binary_under(
    binary: &Path,
    dir: &Path,
    extra_env: &[(&str, &str)],
    wrapper: &str,
    stdin: &str,
    args: &str,
) -> Pty {
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(
        wrapper
            .replace("{coyote}", binary.to_str().unwrap())
            .replace("{args}", args)
            .replace("{stdin}", stdin)
            .replace("{report}", TTY_REPORT),
    );
    base_env(&mut cmd, dir);
    cmd.env("COYOTE_UI", "tui").envs(extra_env.iter().copied());
    Pty::spawn(cmd).unwrap()
}

/// The environment every spawn of the binary shares: working dir, config
/// and cache dirs and `HOME` all under `dir`, the fixture bin first on
/// `PATH`, and no inherited `COYOTE_*` or color variables. With
/// `COLORTERM` unset the field's background is an indexed colour, not RGB.
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

fn exit_code(transcript: &str) -> i32 {
    transcript
        .lines()
        .find_map(|line| line.strip_prefix("exit="))
        .unwrap_or_else(|| panic!("no exit= line in the transcript:\n{transcript}"))
        .trim()
        .parse()
        .unwrap()
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

/// The 1-based (row, col) of the cursor-position sequence (`CSI r;c H`)
/// that placed each occurrence of `needle` from `from` on: the painter
/// positions the cursor before the first changed cell of a row, so this
/// is the screen row a piece of text was drawn on.
fn painted_at(haystack: &[u8], needle: &[u8], from: usize) -> Vec<(u16, u16)> {
    let mut positions = Vec::new();
    let mut at = from;
    while let Some(hit) = find(haystack, needle, at) {
        if let Some((_, row, col)) = cursor_position_before(haystack, hit) {
            positions.push((row, col));
        }
        at = hit + 1;
    }
    positions
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
        let final_byte = rest[end..].chars().next().map_or(1, char::len_utf8);
        rest = &rest[end + final_byte..];
    }
    out
}

/// The SGR parameter lists written between the cursor positioning that
/// started the run of cells `at` belongs to and `at` itself: the style
/// the painter set up for the text at `at` after a full repaint, when
/// every row starts with its own cursor move.
fn sgr_before(raw: &[u8], at: usize) -> Vec<Vec<u16>> {
    let (start, _, _) = cursor_position_before(raw, at)
        .unwrap_or_else(|| panic!("no cursor position precedes offset {at}"));
    sgr_params(&raw[start..at])
}

/// Whether the lists carry an indexed background (`48;5;N`).
fn has_indexed_background(params: &[Vec<u16>]) -> bool {
    params.iter().any(|p| p.windows(2).any(|w| w == [48, 5]))
}

/// Whether the lists set the indexed foreground `index` (`38;5;index`);
/// the crossterm backend writes the named ANSI colours this way.
fn has_indexed_foreground(params: &[Vec<u16>], index: u16) -> bool {
    params
        .iter()
        .any(|p| p.windows(3).any(|w| w == [38, 5, index]))
}

/// The palette slots the crossterm backend writes for the field's
/// `Color::Green` and `Color::Red`.
const GREEN: u16 = 2;
const RED: u16 = 1;

/// Forces a full repaint by resizing the terminal (the painter clears its
/// back buffer on a size change) and returns the raw offset the repaint
/// starts after. Typed keys are handled one per loop iteration, so text
/// typed in one go may be painted across several frames; after the
/// repaint every row is written in one piece, with its own cursor move
/// and the styles set up from a reset state.
fn repaint(pty: &mut Pty, rows: u16, cols: u16) -> usize {
    let at = pty.raw().len();
    pty.resize(rows, cols).unwrap();
    pty.read_for(REPAINT_SETTLE);
    at
}

/// Pumps the pty until `path` exists, so the child's output never backs up
/// while the test waits on a side effect.
fn wait_for_file(pty: &mut Pty, path: &Path, what: &str) {
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    while !path.exists() {
        let text = pty.read_for(Duration::from_millis(100));
        assert!(
            std::time::Instant::now() < deadline,
            "{what} ({}) never appeared:\n{}",
            path.display(),
            text
        );
    }
}

/// Waits for `marker` with whitespace squeezed out of both sides: a
/// streamed reply can be wrapped mid-word by the transcript.
fn wait_for_squeezed(pty: &mut Pty, marker: &str, what: &str) -> String {
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    let wanted: String = marker.chars().filter(|c| !c.is_whitespace()).collect();
    loop {
        let text = pty.read_for(Duration::from_millis(200));
        let squeezed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        if squeezed.contains(&wanted) {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what} never appeared:\n{text}"
        );
    }
}

fn since(pty: &Pty, from: usize) -> String {
    strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]))
}

/// A tool that asks through one of the prompt helpers (`call` is the
/// helper invocation) and records the answer it got at `out`, so a test
/// sees the value the modal produced rather than only the model's
/// follow-up. The helper prints the same shape it does on a terminal:
/// `list` the option index, `checkbox` the checked indices, `password`
/// the text.
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

/// A turn whose first reply calls `tool` with `{"out": out}` and whose
/// second is the text `fixture done z2s`.
fn tool_call_then_done(tool: &str, out: &Path) -> FakeLlm {
    FakeLlm::start(vec![
        Reply::ToolCall {
            name: tool.to_string(),
            arguments: format!("{{\"out\": \"{}\"}}", out.display()),
        },
        Reply::Text("fixture done z2s".to_string()),
    ])
}

/// Lets the model's follow-up land, then leaves with Ctrl+D and checks the
/// exit code.
fn finish_after_tool(pty: &mut Pty) {
    wait_for_squeezed(pty, "fixture done z2s", "the reply after the tool");
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// The palette lists what the context allows from the first key, the
/// rows the REPL completer gives: with function calling on, the `.tool`
/// verbs are in the band before any command has run, under either
/// prefix.
#[test]
#[serial]
fn the_cold_start_palette_matches_the_repl_completer() {
    for (prefix, typed, rows) in [
        ("slash", "/tool", ["/tool enable", "/tool disable"]),
        ("dot", ".tool", [".tool enable", ".tool disable"]),
    ] {
        let dir = fresh_config_dir(&format!("cold-palette-{prefix}"));
        let _guard = TempDirGuard(dir.clone());
        write_config(
            &dir,
            &format!(
                "save_session: false\n\
                 function_calling_support: true\n\
                 command_prefix: {prefix}\n"
            ),
        );
        let mut pty = spawn_tui(&dir, &[]);

        pty.wait_for("no session", PTY_DEADLINE).unwrap();
        pty.send(typed.as_bytes()).unwrap();
        pty.wait_for("Enable a single tool", PTY_DEADLINE)
            .unwrap_or_else(|err| {
                panic!("the band never listed the tool commands under {prefix}: {err}")
            });
        let repainted = repaint(&mut pty, 40, 101);
        let frame = since(&pty, repainted);
        for row in rows {
            assert!(
                frame.contains(row),
                "`{row}` is not in the band under {prefix}:\n{frame}"
            );
        }

        pty.send(b"\x03").unwrap();
        pty.read_for(HOTKEY_GAP);
        pty.send(b"\x04").unwrap();
        let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
        assert_eq!(exit_code(&transcript), 0, "{transcript}");
    }
}

/// The field is its own surface: the cells of its text rows are painted
/// on a background the transcript rows above the rule are not. The screen
/// is made tall enough for the `.info` table so the `> .info` header is
/// still on screen to compare against.
#[test]
#[serial]
fn the_composer_rows_carry_a_background_the_transcript_rows_do_not() {
    let dir = fresh_config_dir("field-bg");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(80, 100).unwrap();
    pty.read_for(Duration::from_millis(300));
    let info_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", info_at, PTY_DEADLINE)
        .unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"probe-bg-q9d").unwrap();
    pty.read_for(Duration::from_millis(300));
    let repainted = repaint(&mut pty, 80, 101);

    let raw = pty.raw();
    let probe_at = find(raw, b"probe-bg-q9d", repainted).unwrap_or_else(|| {
        panic!(
            "the typed probe was not repainted:\n{}",
            since(&pty, repainted)
        )
    });
    let header_at = find(raw, b"> .info", repainted).unwrap_or_else(|| {
        panic!(
            "the `> .info` header was not repainted:\n{}",
            since(&pty, repainted)
        )
    });
    let probe_style = sgr_before(raw, probe_at);
    assert!(
        has_indexed_background(&probe_style),
        "the field's text row was painted without an indexed background: {probe_style:?}\n{}",
        since(&pty, repainted)
    );
    let header_style = sgr_before(raw, header_at);
    assert!(
        !has_indexed_background(&header_style),
        "the transcript row carries the field's background: {header_style:?}\n{}",
        since(&pty, repainted)
    );
    let probe_rows = painted_at(raw, b"probe-bg-q9d", repainted);
    let header_rows = painted_at(raw, b"> .info", repainted);
    let (Some(&(probe_row, _)), Some(&(header_row, _))) = (probe_rows.first(), header_rows.first())
    else {
        panic!("the probe or the header was not positioned: {probe_rows:?} / {header_rows:?}");
    };
    assert!(
        probe_row > header_row,
        "the field (row {probe_row}) is not below the transcript (row {header_row})"
    );

    // The draft must be cleared before Ctrl+D is the quit chord.
    pty.send(b"\x03").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// `TuiPrompter` renders a tool's select request as the modal; a digit
/// jumps to that row and, once the idle gate has passed, accepts it. The
/// helper hands the tool the option's index.
#[test]
#[serial]
fn a_tools_select_request_is_the_modal_and_a_digit_picks_the_row() {
    let dir = fresh_config_dir("tool-select");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("select-answer");
    let fake = tool_call_then_done("fixture_select_rec", &out);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_select_rec.sh",
        &recording_prompt_tool("list \"Pick one q9d-s\" alpha beta gamma"),
    );
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"call the select tool\r").unwrap();
    pty.wait_for("q9d-s", PTY_DEADLINE)
        .expect("the tool's select question never reached the screen");
    pty.read_for(HOTKEY_GAP);
    pty.send(b"2").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "1",
        "`2` on the select modal did not reach the tool as beta's index"
    );
    finish_after_tool(&mut pty);
}

/// A checkbox request: Space ticks the row under the cursor, the arrows
/// move it, Enter sends the ticked set; the helper hands the tool the
/// checked indices.
#[test]
#[serial]
fn a_tools_checkbox_request_ticks_rows_and_sends_them() {
    let dir = fresh_config_dir("tool-checkbox");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("checkbox-answer");
    let fake = tool_call_then_done("fixture_checkbox_rec", &out);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_checkbox_rec.sh",
        &recording_prompt_tool("checkbox \"Tick some q9d-c\" alpha beta gamma"),
    );
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"call the checkbox tool\r").unwrap();
    pty.wait_for("q9d-c", PTY_DEADLINE)
        .expect("the tool's checkbox question never reached the screen");
    pty.read_for(HOTKEY_GAP);
    // One key per write: an escape sequence split across reads would be
    // taken for a bare Esc and cancel the modal.
    let keys: [&[u8]; 5] = [b" ", b"\x1b[B", b"\x1b[B", b" ", b"\r"];
    for key in keys {
        pty.send(key).unwrap();
        pty.read_for(Duration::from_millis(100));
    }
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    assert_eq!(
        fs::read_to_string(&out).unwrap().trim(),
        "0 2",
        "ticking alpha and gamma did not reach the tool as their indices"
    );
    finish_after_tool(&mut pty);
}

/// A password request masks what is typed: the screen shows a run of
/// mask characters and never the text, which still reaches the tool
/// whole on Enter.
#[test]
#[serial]
fn a_tools_password_request_masks_the_field_and_sends_the_text() {
    let dir = fresh_config_dir("tool-password");
    let _guard = TempDirGuard(dir.clone());
    let out = dir.join("password-answer");
    let fake = tool_call_then_done("fixture_password_rec", &out);
    write_fake_llm_config(&dir, &fake);
    write_tool(
        &dir,
        "fixture_password_rec.sh",
        &recording_prompt_tool("password \"Token q9d-p\""),
    );
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"call the password tool\r").unwrap();
    pty.wait_for("q9d-p", PTY_DEADLINE)
        .expect("the tool's password question never reached the screen");
    pty.read_for(HOTKEY_GAP);
    let modal_at = pty.raw().len();
    pty.send(b"s3cret").unwrap();
    pty.read_for(HOTKEY_GAP);
    let repainted = repaint(&mut pty, 40, 101);
    let field = since(&pty, repainted);
    assert!(
        field.contains("******"),
        "the password field showed no mask run:\n{field}"
    );
    let shown = since(&pty, modal_at);
    assert!(
        !shown.contains("s3cret"),
        "the typed password reached the screen:\n{shown}"
    );

    pty.send(b"\r").unwrap();
    wait_for_file(&mut pty, &out, "the tool's recorded answer");
    assert_eq!(fs::read_to_string(&out).unwrap().trim(), "s3cret");
    assert!(
        !since(&pty, modal_at).contains("s3cret"),
        "the password reached the screen after it was sent:\n{}",
        since(&pty, modal_at)
    );
    finish_after_tool(&mut pty);
}

/// The field colours the command word as it is typed: green for a
/// command the REPL knows, red for one it does not. Typing a command
/// opens the palette, whose rows also spell the command, so the band is
/// closed with Esc before the frame is searched for the field's copy.
#[test]
#[serial]
fn a_typed_command_is_painted_green_and_an_unknown_one_red() {
    let dir = fresh_config_dir("command-colours");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".info").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    let repainted = repaint(&mut pty, 40, 101);
    assert!(
        !since(&pty, repainted).contains("Show system info"),
        "the palette band is still up:\n{}",
        since(&pty, repainted)
    );
    let at = find(pty.raw(), b".info", repainted)
        .unwrap_or_else(|| panic!("`.info` was not repainted:\n{}", since(&pty, repainted)));
    let style = sgr_before(pty.raw(), at);
    assert!(
        has_indexed_foreground(&style, GREEN),
        "a known command was not painted green: {style:?}\n{}",
        since(&pty, repainted)
    );

    pty.send(b"\x7f\x7f\x7f\x7f\x7f").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b".nosuch").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    let repainted = repaint(&mut pty, 40, 100);
    let at = find(pty.raw(), b".nosuch", repainted)
        .unwrap_or_else(|| panic!("`.nosuch` was not repainted:\n{}", since(&pty, repainted)));
    let style = sgr_before(pty.raw(), at);
    assert!(
        has_indexed_foreground(&style, RED),
        "an unknown command was not painted red: {style:?}\n{}",
        since(&pty, repainted)
    );

    pty.send(b"\x03").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// Esc with the palette up closes the band and nothing else: the typed
/// prefix stays in the field and no turn is interrupted.
#[test]
#[serial]
fn esc_closes_the_palette_without_interrupting_or_clearing_focus() {
    let dir = fresh_config_dir("palette-esc");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".in").unwrap();
    pty.wait_for("Show system info", PTY_DEADLINE)
        .expect("the palette band never showed the `.info` row");
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    let repainted = repaint(&mut pty, 40, 101);
    let frame = since(&pty, repainted);
    assert!(
        !frame.contains("Show system info") && !frame.contains(".info"),
        "the palette band survived Esc:\n{frame}"
    );
    assert!(
        frame.contains(".in"),
        "Esc cleared the typed prefix:\n{frame}"
    );

    pty.send(b"\x03").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}
