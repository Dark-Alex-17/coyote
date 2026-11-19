//! Real-process checks of the readline chords and a question's text field,
//! sent as the terminal encodes them: Ctrl+W, Ctrl+Y, Alt+B, Ctrl+K, Alt+F,
//! Ctrl+_, Alt+D and Ctrl+U edit the composer; in a question's text field
//! `?` is typed rather than opening the key help, F1-F12 type nothing, and
//! Ctrl+X shows the todo panel without eating the field's text or closing
//! the question. Each case runs the binary on a pseudo-terminal, so the
//! file is unix-only, and the cases run one at a time.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::pty::Pty;
use common::tui_harness::*;
use serial_test::serial;

/// A row only the key help shows.
const HELP_MARK: &str = "Suspend to the shell";

const CTRL_E: &[u8] = b"\x05";
const CTRL_K: &[u8] = b"\x0b";
const CTRL_U: &[u8] = b"\x15";
const CTRL_W: &[u8] = b"\x17";
const CTRL_Y: &[u8] = b"\x19";
/// What a terminal sends for Ctrl+_ and, in most, for Ctrl+/ as well.
const CTRL_UNDERSCORE: &[u8] = b"\x1f";
const ALT_B: &[u8] = b"\x1bb";
const ALT_D: &[u8] = b"\x1bd";
const ALT_F: &[u8] = b"\x1bf";
const HOME: &[u8] = b"\x1b[H";

/// xterm's encodings of F1-F12.
const FUNCTION_KEYS: [&[u8]; 12] = [
    b"\x1bOP",
    b"\x1bOQ",
    b"\x1bOR",
    b"\x1bOS",
    b"\x1b[15~",
    b"\x1b[17~",
    b"\x1b[18~",
    b"\x1b[19~",
    b"\x1b[20~",
    b"\x1b[21~",
    b"\x1b[23~",
    b"\x1b[24~",
];

/// Ctrl+C clears a non-empty composer and quits from an empty one, so the
/// Ctrl+D after it may find the UI already gone.
fn leave(mut pty: Pty) {
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(300));
    let _ = pty.send(b"\x04");
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert!(!transcript.contains("panicked"), "{transcript}");
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert_tty_restored(&transcript);
}

fn shows(frame: &str, needle: &str) -> bool {
    squeezed(frame).contains(&squeezed(needle))
}

/// Sends `keys` and reads the composer row off the next full repaint.
fn step(pty: &mut Pty, keys: &[u8]) -> String {
    pty.send(keys).unwrap();
    pty.read_for(Duration::from_millis(300));
    composer_row(&flipped_frame(pty))
}

#[test]
#[serial]
fn readline_chords_edit_the_composer() {
    let dir = fresh_config_dir("readline-chords");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"alpha beta gamma").unwrap();
    repainted_frame_showing(&mut pty, "alpha beta gamma", "the composer took no text");

    assert_eq!(
        step(&mut pty, CTRL_W),
        "alpha beta",
        "Ctrl+W did not cut the last word"
    );
    assert_eq!(
        step(&mut pty, CTRL_Y),
        "alpha beta gamma",
        "Ctrl+Y did not paste the cut word"
    );
    pty.send(ALT_B).unwrap();
    pty.read_for(Duration::from_millis(150));
    pty.send(ALT_B).unwrap();
    pty.read_for(Duration::from_millis(150));
    assert_eq!(
        step(&mut pty, CTRL_K),
        "alpha",
        "Alt+B twice then Ctrl+K did not cut `beta gamma`"
    );
    assert_eq!(
        step(&mut pty, CTRL_Y),
        "alpha beta gamma",
        "Ctrl+Y did not paste the killed text"
    );
    pty.send(HOME).unwrap();
    pty.read_for(Duration::from_millis(150));
    pty.send(ALT_F).unwrap();
    pty.read_for(Duration::from_millis(150));
    // The wiki promises "move a word", not emacs end-of-word: accept the
    // cursor landing at either edge of the gap after `alpha`.
    let moved = step(&mut pty, b"Z");
    assert!(
        moved == "alphaZ beta gamma" || moved == "alpha Zbeta gamma",
        "Alt+F did not move one word forward: {moved:?}"
    );
    assert_eq!(
        step(&mut pty, CTRL_UNDERSCORE),
        "alpha beta gamma",
        "Ctrl+_ did not undo the typed Z"
    );
    pty.send(HOME).unwrap();
    pty.read_for(Duration::from_millis(150));
    assert_eq!(
        step(&mut pty, ALT_D),
        "beta gamma",
        "Alt+D did not cut the word after the cursor"
    );
    pty.send(CTRL_E).unwrap();
    pty.read_for(Duration::from_millis(150));
    pty.send(ALT_B).unwrap();
    pty.read_for(Duration::from_millis(150));
    assert_eq!(
        step(&mut pty, CTRL_U),
        "gamma",
        "Ctrl+U did not cut back to the start of the line"
    );
    leave(pty);
}

#[test]
#[serial]
fn in_a_text_field_question_mark_types_ctrl_x_shows_the_todo_and_function_keys_do_nothing() {
    let dir = fresh_config_dir("text-field-keys");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.send(b".temp-role you are the temp role g4r\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let before = pty.raw().len();
    pty.send(b".save role\r").unwrap();
    pty.wait_for_after("Role name:", before, PTY_DEADLINE)
        .expect("`.save role` did not ask `Role name:`");
    pty.read_for(HOTKEY_GAP);

    pty.send(b"?").unwrap();
    for key in FUNCTION_KEYS {
        pty.send(key).unwrap();
        pty.read_for(Duration::from_millis(100));
    }
    pty.send(b"one two").unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = flipped_frame(&mut pty);
    assert!(
        !shows(&frame, HELP_MARK),
        "`?` in the text field opened the key help:\n{frame}"
    );
    assert!(
        shows(&frame, "?one two"),
        "`?` was not typed into the field, or an F-key left debris:\n{frame}"
    );

    pty.send(CTRL_X).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = flipped_frame(&mut pty);
    assert!(
        frame.contains("Todo"),
        "Ctrl+X in the text field did not show the todo panel:\n{frame}"
    );
    assert!(
        shows(&frame, "?one two") && shows(&frame, "Role name:"),
        "Ctrl+X ate the field text or closed the question:\n{frame}"
    );
    pty.send(CTRL_X).unwrap();
    pty.read_for(Duration::from_millis(300));
    esc_cancels_the_modal(&mut pty, ".save role");
    leave(pty);
}
