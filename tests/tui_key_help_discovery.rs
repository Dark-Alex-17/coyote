//! Real-process checks of how the TUI points at its keys: the idle status
//! row names `? = keys  Alt+T = tasks  Ctrl+X = todo` and drops the `?`
//! part once the composer holds text; the welcome card's key help line is
//! spelled with the active prefix and names no function key; `?` on a
//! non-empty composer types a `?`; `.keys foo` and a two-line `.keys`
//! draft are sent as typed rather than opening the help; and function
//! keys held with a modifier (Shift+F1, Ctrl+F2, Alt+F4, Ctrl+F5,
//! Shift+F12) do nothing and leave no escape debris. Each case runs the
//! binary on a pseudo-terminal, so the file is unix-only, and the cases
//! run one at a time.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::pty::Pty;
use common::tui_harness::*;
use serial_test::serial;

/// A row only the key help shows.
const HELP_MARK: &str = "Suspend to the shell";
/// The roster's legend.
const ROSTER_MARK: &str = "Esc = close";
const IDLE_HINT: &str = "? = keys  Alt+T = tasks  Ctrl+X = todo";
const TEXT_HINT: &str = "Alt+T = tasks  Ctrl+X = todo";

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

fn shows(frame: &str, needle: &str) -> bool {
    squeezed(frame).contains(&squeezed(needle))
}

#[test]
#[serial]
fn the_welcome_card_and_the_idle_hint_name_the_key_help_and_the_chords() {
    let (_guard, mut pty) = boot("discovery-hints", "");
    let frame = flipped_frame(&mut pty);
    assert!(
        shows(&frame, "Empty prompt: \"?\" or \".keys\" = keys"),
        "the welcome card does not point at the key help:\n{frame}"
    );
    assert!(
        shows(&frame, IDLE_HINT),
        "the idle status row lacks `{IDLE_HINT}`:\n{frame}"
    );
    if let Some(named) = ["F1", "F2", "F3"].iter().find(|f| {
        frame
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| w == **f)
    }) {
        panic!("the idle screen names {named}:\n{frame}");
    }

    // `?` on a composer with text types instead of opening the help, and
    // the hint drops its `?` part.
    pty.send(b"ab?").unwrap();
    let frame = repainted_frame_showing(&mut pty, "ab?", "`?` after text was not typed");
    assert!(
        !shows(&frame, HELP_MARK),
        "`?` on a non-empty composer opened the key help:\n{frame}"
    );
    assert!(
        shows(&frame, TEXT_HINT) && !shows(&frame, "? = keys"),
        "with text the hint should read `{TEXT_HINT}` alone:\n{frame}"
    );
    leave(pty);
}

#[test]
#[serial]
fn the_slash_prefix_spells_the_welcome_pointer_with_slash() {
    let (_guard, mut pty) = boot("discovery-slash", "command_prefix: slash\n");
    let frame = flipped_frame(&mut pty);
    assert!(
        shows(&frame, "Empty prompt: \"?\" or \"/keys\" = keys"),
        "the welcome card does not spell /keys under the slash prefix:\n{frame}"
    );
    leave(pty);
}

#[test]
#[serial]
fn dot_keys_with_an_argument_or_a_second_line_does_not_open_the_help() {
    let (_guard, mut pty) = boot("discovery-keys-args", "");
    pty.send(b".keys foo\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = flipped_frame(&mut pty);
    assert!(
        !shows(&frame, HELP_MARK),
        "`.keys foo` opened the key help:\n{frame}"
    );

    // Ctrl+J puts the second line under `.keys`.
    pty.send(b".keys\x0azz").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = flipped_frame(&mut pty);
    assert!(
        !shows(&frame, HELP_MARK),
        "a two-line `.keys` draft opened the key help:\n{frame}"
    );
    leave(pty);
}

#[test]
#[serial]
fn function_keys_held_with_a_modifier_do_nothing_either() {
    let (_guard, mut pty) = boot("discovery-modified-fkeys", "");
    // Shift+F1, Ctrl+F2, Alt+F4, Ctrl+F5 and Shift+F12 as xterm sends
    // them. Alt+F3 would be `\x1b[1;3R`, which is also a cursor position
    // report.
    for key in [
        &b"\x1b[1;2P"[..],
        b"\x1b[1;5Q",
        b"\x1b[1;3S",
        b"\x1b[15;5~",
        b"\x1b[24;2~",
    ] {
        pty.send(key).unwrap();
        pty.read_for(Duration::from_millis(150));
    }
    let frame = flipped_frame(&mut pty);
    for (mark, what) in [(ROSTER_MARK, "the roster"), (HELP_MARK, "the key help")] {
        assert!(
            !shows(&frame, mark),
            "a modified F-key opened {what}:\n{frame}"
        );
    }
    assert!(
        !frame.contains("Todo"),
        "a modified F-key opened the todo panel:\n{frame}"
    );
    pty.send(b"k7q").unwrap();
    let frame = repainted_frame_showing(&mut pty, "k7q", "the composer stopped taking keys");
    assert!(
        shows(&frame, TEXT_HINT),
        "the UI lost its hint row:\n{frame}"
    );
    assert_eq!(
        composer_row(&frame),
        "k7q",
        "a modified F-key left escape debris in the composer:\n{frame}"
    );
    leave(pty);
}
