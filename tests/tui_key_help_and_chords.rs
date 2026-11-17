//! Real-process checks of the TUI's key discovery and panel chords, written
//! from the keys contract rather than the dispatcher: no function key does
//! anything (F1-F12 on the empty composer open no panel, no roster and no
//! key help, and type nothing); `.keys` opens the key help, which lists the
//! Ctrl chords and no function key on any page, scrolls with PageDown, and
//! closes with `q`, `?` and Ctrl+C as well as Esc; `/keys` opens it under
//! `command_prefix: slash`, and `?` opens it from vi normal mode; Ctrl+G
//! closes the roster it opened, and `?` with the roster open does not open
//! the key help; Ctrl+X opens the todo panel without eating the composer's
//! text, and Ctrl+E is still the composer's end of line; the plain REPL
//! does not know `.keys`. Each case runs the binary on a pseudo-terminal,
//! so the file is unix-only, and the cases run one at a time.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::pty::{Pty, strip_ansi};
use common::tui_harness::*;
use serial_test::serial;

const CTRL_G: &[u8] = b"\x07";
const CTRL_X: &[u8] = b"\x18";
const CTRL_E: &[u8] = b"\x05";
const HOME: &[u8] = b"\x1b[H";
const PAGE_DOWN: &[u8] = b"\x1b[6~";
/// A row only the key help shows.
const HELP_MARK: &str = "Suspend to the shell";
/// The roster's legend.
const ROSTER_MARK: &str = "Esc = close";

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

fn boot(scenario: &str, extra: &str) -> (TempDirGuard, Pty) {
    let dir = fresh_config_dir(scenario);
    let guard = TempDirGuard(dir.clone());
    write_config(&dir, &format!("save_session: false\n{extra}"));
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(400));
    (guard, pty)
}

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

/// A function key's name as a whole word (`F1`..`F12`) anywhere in `text`.
fn names_a_function_key(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c != 'F' || (i > 0 && chars[i - 1].is_alphanumeric()) {
            continue;
        }
        let digits: String = chars[i + 1..]
            .iter()
            .take_while(|d| d.is_ascii_digit())
            .collect();
        let after = chars.get(i + 1 + digits.len());
        if !digits.is_empty() && after.is_none_or(|a| !a.is_alphanumeric()) {
            return Some(format!("F{digits}"));
        }
    }
    None
}

fn assert_help_closed(pty: &mut Pty, by: &str) {
    pty.read_for(Duration::from_millis(400));
    let frame = repainted_frame_showing(
        pty,
        "no session",
        &format!("{by} did not close the key help"),
    );
    assert!(
        !squeezed(&frame).contains(&squeezed(HELP_MARK)),
        "the key help is still up after {by}:\n{frame}"
    );
}

#[test]
#[serial]
fn function_keys_do_nothing_on_the_empty_composer() {
    let (_guard, mut pty) = boot("fkeys", "");
    for key in FUNCTION_KEYS {
        pty.send(key).unwrap();
        pty.read_for(Duration::from_millis(150));
    }
    let frame = flipped_frame(&mut pty);
    let flat = squeezed(&frame);
    assert!(
        !flat.contains(&squeezed(ROSTER_MARK)),
        "an F-key opened the roster:\n{frame}"
    );
    assert!(
        !frame.contains("Todo"),
        "an F-key opened the todo panel:\n{frame}"
    );
    assert!(
        !flat.contains(&squeezed(HELP_MARK)),
        "an F-key opened the key help:\n{frame}"
    );
    pty.send(b"q4z").unwrap();
    let typed = repainted_frame_showing(&mut pty, "q4z", "the composer stopped taking keys");
    let line = typed
        .lines()
        .find(|l| l.contains("q4z"))
        .unwrap_or_default();
    assert!(
        !line.contains('~') && !line.contains("[1") && !line.contains("OP"),
        "an F-key typed into the composer: {line:?}"
    );
    leave(pty);
}

#[test]
#[serial]
fn dot_keys_lists_ctrl_chords_and_no_function_key_scrolls_and_closes_on_every_close_key() {
    let (_guard, mut pty) = boot("dot-keys", "");

    pty.send(b".keys\r").unwrap();
    let first = repainted_frame_showing(&mut pty, HELP_MARK, ".keys did not open the key help");
    let mut pages = vec![first.clone()];
    for _ in 0..8 {
        pty.send(PAGE_DOWN).unwrap();
        pty.read_for(Duration::from_millis(200));
        pages.push(flipped_frame(&mut pty));
    }
    assert_ne!(
        squeezed(&first),
        squeezed(&pages[1]),
        "PageDown did not scroll the key help"
    );
    let all = pages.join("\n");
    for chord in ["Ctrl+G", "Ctrl+X", "Ctrl+T", "Ctrl+L"] {
        assert!(
            all.contains(chord),
            "the key help never lists {chord}:\n{all}"
        );
    }
    if let Some(named) = names_a_function_key(&all) {
        panic!("the key help names the function key {named}:\n{all}");
    }

    pty.send(b"q").unwrap();
    assert_help_closed(&mut pty, "q");

    pty.send(b"?").unwrap();
    repainted_frame_showing(&mut pty, HELP_MARK, "? did not reopen the key help");
    pty.send(b"?").unwrap();
    assert_help_closed(&mut pty, "a second ?");

    pty.send(b"?").unwrap();
    repainted_frame_showing(&mut pty, HELP_MARK, "? did not reopen the key help");
    pty.send(b"\x03").unwrap();
    assert_help_closed(&mut pty, "Ctrl+C");

    pty.send(b"still here k2w").unwrap();
    wait_for_squeezed(
        &mut pty,
        "still here k2w",
        "the composer did not take keys after the help",
    );
    leave(pty);
}

#[test]
#[serial]
fn slash_keys_opens_the_key_help_under_the_slash_prefix() {
    let (_guard, mut pty) = boot("slash-keys", "command_prefix: slash\n");
    pty.send(b"/keys\r").unwrap();
    repainted_frame_showing(&mut pty, HELP_MARK, "/keys did not open the key help");
    pty.send(b"\x1b").unwrap();
    assert_help_closed(&mut pty, "Esc");
    leave(pty);
}

#[test]
#[serial]
fn question_mark_in_vi_normal_mode_opens_the_key_help() {
    let (_guard, mut pty) = boot("vi-help", "keybindings: vi\n");
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"?").unwrap();
    repainted_frame_showing(
        &mut pty,
        HELP_MARK,
        "? in vi normal mode did not open the key help",
    );
    pty.send(b"\x1b").unwrap();
    assert_help_closed(&mut pty, "Esc");
    leave(pty);
}

#[test]
#[serial]
fn ctrl_g_toggles_the_roster_and_question_mark_over_it_opens_no_help() {
    let (_guard, mut pty) = boot("roster-toggle", "");
    pty.send(CTRL_G).unwrap();
    repainted_frame_showing(&mut pty, ROSTER_MARK, "Ctrl+G did not open the roster");

    pty.send(b"?").unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = flipped_frame(&mut pty);
    assert!(
        !squeezed(&frame).contains(&squeezed(HELP_MARK)),
        "? over the open roster opened the key help:\n{frame}"
    );

    pty.send(CTRL_G).unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = flipped_frame(&mut pty);
    assert!(
        !squeezed(&frame).contains(&squeezed(ROSTER_MARK)),
        "a second Ctrl+G did not close the roster:\n{frame}"
    );
    leave(pty);
}

#[test]
#[serial]
fn ctrl_x_keeps_the_composer_text_and_ctrl_e_is_still_end_of_line() {
    let (_guard, mut pty) = boot("ctrl-x-text", "");
    pty.send(b"bc").unwrap();
    pty.read_for(Duration::from_millis(200));
    pty.send(HOME).unwrap();
    pty.send(b"a").unwrap();
    pty.read_for(Duration::from_millis(200));
    pty.send(CTRL_E).unwrap();
    pty.send(b"d9v").unwrap();
    repainted_frame_showing(
        &mut pty,
        "abcd9v",
        "Ctrl+E did not move to the end of the line",
    );

    pty.send(CTRL_X).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = flipped_frame(&mut pty);
    assert!(
        frame.contains("Todo"),
        "Ctrl+X with text did not open the todo panel:\n{frame}"
    );
    assert!(
        squeezed(&frame).contains("abcd9v"),
        "Ctrl+X ate the composer's text:\n{frame}"
    );
    pty.send(CTRL_X).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = flipped_frame(&mut pty);
    assert!(
        !frame.contains("Todo"),
        "a second Ctrl+X did not close the todo panel:\n{frame}"
    );
    leave(pty);
}

#[test]
#[serial]
fn the_plain_repl_does_not_know_dot_keys() {
    let dir = fresh_config_dir("repl-keys");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config(&dir);
    let mut pty = spawn_tui_under(&dir, &[("COYOTE_UI", "repl")], WAIT_WRAPPER, "/dev/tty");
    pty.wait_for("dry-model)>", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let at = pty.raw().len();
    pty.send(b".keys\r").unwrap();
    pty.wait_for_after("Unknown command", at, PTY_DEADLINE)
        .expect("the REPL did not refuse .keys as an unknown command");
    let out = strip_ansi(&String::from_utf8_lossy(&pty.raw()[at..]));
    assert!(
        !out.contains(HELP_MARK),
        "the REPL printed key help:\n{out}"
    );
    assert!(
        find(pty.raw(), ENTER_ALT, 0).is_none(),
        "the REPL entered the alternate screen"
    );
    pty.send(b".exit\r").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
}
