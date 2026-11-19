//! Real-process checks of the bottom pane: the composer field's surface
//! and command colours as the terminal sees them, and the three prompt
//! modals a tool can ask for through the prompt channel (select, checkbox,
//! password). Colours and backgrounds are read from the raw escape stream,
//! so the file runs the binary on a pseudo-terminal and is unix-only. The
//! cases run one at a time: each spawn forks a session leader and takes a
//! controlling tty.
#![cfg(unix)]

mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use common::fake_llm::{Chunk, FakeLlm, Reply};
use common::pty::Pty;
use common::tui_harness::*;
use serial_test::serial;

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

/// The palette lists what the context allows from the first key: with
/// function calling on, the `.tool` verbs are in the band before any
/// command has run, under either prefix. The full completer parity is
/// pinned by the unit tests of the shared table.
#[test]
#[serial]
fn the_cold_start_palette_lists_the_tool_verbs_under_either_prefix() {
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
        // The band pads the name to the description column, so the rows
        // are matched with whitespace squeezed out on both sides.
        let [enable, disable] = rows.map(|row| row.split_whitespace().collect::<String>());
        squeezed_positions(&frame, [enable.as_str(), disable.as_str()]);

        // Ctrl+C closes the band first; the second one clears the draft.
        pty.send(b"\x03").unwrap();
        pty.read_for(HOTKEY_GAP);
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

/// A tool's password request names the tool and masks what is typed:
/// the screen shows a run of mask characters and never the text, which
/// still reaches the tool whole on Enter.
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
        field.contains("fixture_password_rec asks:"),
        "the password modal did not name the tool:\n{field}"
    );
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

/// A streamed reply held open at `release` with no tool call in it, so
/// the driver has no round at which to hand a queued line to the model.
fn held_stream(release: &Path) -> Reply {
    Reply::Streamed(vec![
        Chunk::Text("first half q9d-m".to_string()),
        Chunk::WaitFor(release.to_path_buf()),
        Chunk::Text(" tail q9d-t".to_string()),
    ])
}

/// Starts a turn on the held stream and queues `line` behind it; the
/// preview shows the line held and the model has had its one request.
fn queue_behind_held_stream(pty: &mut Pty, fake: &FakeLlm, line: &[u8]) {
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b"stream and hold\r").unwrap();
    wait_for_squeezed(pty, "first half q9d-m", "the first delta");
    pty.send(line).unwrap();
    pty.wait_for("pending: 1", PTY_DEADLINE)
        .expect("the preview never showed the queued line");
    assert_eq!(fake.served(), 1, "the queued line went out mid-stream");
}

/// Where each marker first appears in `frame` with whitespace squeezed
/// out, so the order of transcript cells can be compared no matter how
/// the rows wrapped.
fn squeezed_positions<const N: usize>(frame: &str, markers: [&str; N]) -> [usize; N] {
    let squeezed: String = frame.chars().filter(|c| !c.is_whitespace()).collect();
    markers.map(|marker| {
        squeezed
            .find(marker)
            .unwrap_or_else(|| panic!("`{marker}` is not on the screen:\n{frame}"))
    })
}

/// A prompt queued while the model streams, with no tool round for the
/// driver to hand it over at, becomes the next turn's prompt once the
/// reply ends: the second request goes out only after the first reply,
/// the line sits as a user cell between the two replies, and the
/// preview is gone.
#[test]
#[serial]
fn a_queued_prompt_the_model_never_got_becomes_the_next_prompt() {
    let dir = fresh_config_dir("queued-next-prompt");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("release");
    let fake = FakeLlm::start(vec![
        held_stream(&release),
        Reply::Text("second reply q9d-s".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);
    queue_behind_held_stream(&mut pty, &fake, b"follow-up q9d-n\r");

    fs::write(&release, "").unwrap();
    wait_for_squeezed(
        &mut pty,
        "second reply q9d-s",
        "the reply to the queued prompt",
    );
    assert_eq!(fake.served(), 2);
    pty.read_for(Duration::from_millis(500));
    let repainted = repaint(&mut pty, 40, 101);
    let frame = since(&pty, repainted);
    let [tail, header, second] =
        squeezed_positions(&frame, ["tailq9d-t", ">follow-upq9d-n", "secondreplyq9d-s"]);
    assert!(
        tail < header && header < second,
        "the queued line is not a prompt between the two replies:\n{frame}"
    );
    assert!(
        !frame.contains("pending:"),
        "the preview outlived the queued line:\n{frame}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}

/// A command queued while the model streams never goes to the model: it
/// runs as a turn of its own once the reply ends, its header and output
/// after the reply, with no second request.
#[test]
#[serial]
fn a_queued_command_runs_as_its_own_turn_after_the_reply() {
    let dir = fresh_config_dir("queued-command");
    let _guard = TempDirGuard(dir.clone());
    let release = dir.join("release");
    let fake = FakeLlm::start(vec![held_stream(&release)]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);
    queue_behind_held_stream(&mut pty, &fake, b".info\r");

    let released_at = pty.raw().len();
    fs::write(&release, "").unwrap();
    pty.wait_for_after("log_path", released_at, PTY_DEADLINE)
        .expect("the queued `.info` never ran");
    pty.read_for(Duration::from_millis(500));
    // Tall enough for the `.info` table to leave the reply above it on
    // screen.
    let repainted = repaint(&mut pty, 80, 101);
    let frame = since(&pty, repainted);
    let [tail, header, output] = squeezed_positions(&frame, ["tailq9d-t", ">.info", "log_path"]);
    assert!(
        tail < header && header < output,
        "the queued command did not run as its own turn after the reply:\n{frame}"
    );
    assert_eq!(fake.served(), 1, "a command reached the model");
    assert!(
        !frame.contains("pending:"),
        "the preview outlived the queued command:\n{frame}"
    );

    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}
