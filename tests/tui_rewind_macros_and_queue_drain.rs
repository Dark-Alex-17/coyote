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

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use common::fake_llm::{FakeLlm, Reply};
use common::pty::Pty;
use common::tui_harness::*;
use serial_test::serial;

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
    let frame = repainted_frame_after_resize(&mut pty, 40, 101);
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
    let frame = repainted_frame_after_resize(&mut pty, 40, 100);
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
    let frame = repainted_frame_after_resize(&mut pty, 40, 101);
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
    let frame = repainted_frame_after_resize(&mut pty, 40, 101);
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
    let frame = repainted_frame_after_resize(&mut pty, 40, 100);
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
        write_dry_run_config_with(&dir, "");
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
        let picker = repainted_frame_after_resize(&mut pty, 60, 101);
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
        let frame = repainted_frame_after_resize(&mut pty, 60, 100);
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
        let picker = repainted_frame_after_resize(&mut pty, 60, 101);
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
        let frame = repainted_frame_after_resize(&mut pty, 60, 100);
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
    write_dry_run_config_with(&dir, "");
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
    let frame = repainted_frame_after_resize(&mut pty, 40, 101);
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
    write_dry_run_config_with(&dir, "");
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
    let frame = repainted_frame_after_resize(&mut pty, 40, 101);
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
    write_dry_run_config_with(&dir, "keybindings: vi\n");
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
