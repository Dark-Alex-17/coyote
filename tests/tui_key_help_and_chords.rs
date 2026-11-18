//! Real-process checks of the TUI's key discovery and panel chords, written
//! from the keys contract rather than the dispatcher: no function key does
//! anything (F1-F12 on the empty composer open no panel, no roster and no
//! key help, and type nothing); `.keys` opens the key help, which lists the
//! chords and no function key on any page, scrolls with PageDown, and
//! closes with `q`, `?`, Ctrl+C and Ctrl+D as well as Esc; `/keys` opens
//! it under `command_prefix: slash`, and `?` opens it from vi normal mode;
//! Alt+T closes the roster it opened, and `?` with the roster open does
//! not open the key help; Ctrl+X opens the todo panel without eating the
//! composer's text, and Ctrl+E is still the composer's end of line; the
//! plain REPL does not know `.keys`. F1-F12 also leave the roster, the key
//! help and the pager as they are; `?` in the history search band opens no
//! help; under a Select modal Ctrl+X shows the todo panel while Alt+T,
//! Ctrl+G, `?` and the function keys do nothing; the palette does not offer
//! `.keys`. Each case runs the binary on a pseudo-terminal, so the file is
//! unix-only, and the cases run one at a time.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::pty::{Pty, strip_ansi};
use common::tui_harness::*;
use serial_test::serial;

const CTRL_E: &[u8] = b"\x05";
const HOME: &[u8] = b"\x1b[H";
const PAGE_DOWN: &[u8] = b"\x1b[6~";
/// A row only the key help shows.
const HELP_MARK: &str = "Suspend to the shell";
/// What the key help's last row does.
const LAST_HELP_ROW: &str = "Close the help";
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
    assert_eq!(
        composer_row(&typed),
        "q4z",
        "an F-key left text in the composer:\n{typed}"
    );
    leave(pty);
}

/// A full repaint of the key help at 30 rows by 100 columns. The page scan
/// compares frames, so they must share a height; the detour from 30 goes
/// through a shorter size, since a taller one would clamp the help's
/// scroll back.
fn help_page(pty: &mut Pty) -> String {
    if pty.rows().unwrap() == 30 {
        repainted_frame(pty, 28, 100);
    }
    repainted_frame(pty, 30, 100)
}

#[test]
#[serial]
fn dot_keys_lists_ctrl_chords_and_no_function_key_scrolls_and_closes_on_every_close_key() {
    let (_guard, mut pty) = boot("dot-keys", "");

    pty.send(b".keys\r").unwrap();
    repainted_frame_showing(&mut pty, HELP_MARK, ".keys did not open the key help");
    let mut pages = vec![help_page(&mut pty)];
    // Page until the help's last row is on screen, or until a PageDown
    // no longer moves the text.
    while !squeezed(pages.last().unwrap()).contains(&squeezed(LAST_HELP_ROW)) {
        assert!(pages.len() < 40, "the key help never reached its end");
        pty.send(PAGE_DOWN).unwrap();
        pty.read_for(Duration::from_millis(200));
        let page = help_page(&mut pty);
        let stuck = squeezed(&page) == squeezed(pages.last().unwrap());
        pages.push(page);
        if stuck {
            break;
        }
    }
    assert!(
        pages.len() > 1,
        "the key help fits one 30-row page, so PageDown was never tried:\n{}",
        pages[0]
    );
    assert_ne!(
        squeezed(&pages[0]),
        squeezed(&pages[1]),
        "PageDown did not scroll the key help"
    );
    let all = pages.join("\n");
    assert!(
        squeezed(&all).contains(&squeezed(LAST_HELP_ROW)),
        "paging never showed the help's last row:\n{all}"
    );
    for chord in [
        "Alt+T", "Ctrl+X", "Ctrl+T", "Ctrl+L", "Ctrl+R", "Ctrl+O", "Ctrl+Z",
    ] {
        assert!(
            all.contains(chord),
            "the key help never lists {chord}:\n{all}"
        );
    }
    for group in [
        "Composer and text field editing",
        "Pager find",
        "Thread picker",
    ] {
        assert!(
            squeezed(&all).contains(&squeezed(group)),
            "the key help never shows the {group:?} group:\n{all}"
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

    pty.send(b"?").unwrap();
    repainted_frame_showing(&mut pty, HELP_MARK, "? did not reopen the key help");
    pty.send(b"\x04").unwrap();
    assert_help_closed(&mut pty, "Ctrl+D");

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
fn alt_t_toggles_the_roster_and_question_mark_over_it_opens_no_help() {
    let (_guard, mut pty) = boot("roster-toggle", "");
    pty.send(ALT_T).unwrap();
    repainted_frame_showing(&mut pty, ROSTER_MARK, "Alt+T did not open the roster");

    pty.send(b"?").unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = flipped_frame(&mut pty);
    assert!(
        !squeezed(&frame).contains(&squeezed(HELP_MARK)),
        "? over the open roster opened the key help:\n{frame}"
    );

    pty.send(ALT_T).unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = flipped_frame(&mut pty);
    assert!(
        !squeezed(&frame).contains(&squeezed(ROSTER_MARK)),
        "a second Alt+T did not close the roster:\n{frame}"
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

// ---------------------------------------------------------------------
// Function keys over the roster, the key help and the pager; `?` in a
// band with an empty composer; Alt+T and Ctrl+X under a prompt modal;
// the palette never offering `.keys`.
// ---------------------------------------------------------------------

const CTRL_R: &[u8] = b"\x12";
const CTRL_T: &[u8] = b"\x14";
/// The pager's header for the root thread.
const PAGER_MARK: &str = "Thread: root thread";

fn send_function_keys(pty: &mut Pty) {
    for key in FUNCTION_KEYS {
        pty.send(key).unwrap();
        pty.read_for(Duration::from_millis(150));
    }
    pty.read_for(Duration::from_millis(300));
}

/// F1-F12 leave whatever is open as it is: the roster stays up and opens
/// no key help, the key help stays up, and the pager stays up (Ctrl+X in
/// the pager does nothing either). None of them is taken for an Esc.
#[test]
#[serial]
fn function_keys_leave_the_roster_the_key_help_and_the_pager_as_they_are() {
    let (_guard, mut pty) = boot("fkeys-over", "");

    pty.send(ALT_T).unwrap();
    repainted_frame_showing(&mut pty, ROSTER_MARK, "Alt+T did not open the roster");
    send_function_keys(&mut pty);
    let frame = flipped_frame(&mut pty);
    assert!(
        squeezed(&frame).contains(&squeezed(ROSTER_MARK)),
        "an F-key closed the roster:\n{frame}"
    );
    assert!(
        !squeezed(&frame).contains(&squeezed(HELP_MARK)),
        "an F-key over the roster opened the key help:\n{frame}"
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(400));

    pty.send(b".keys\r").unwrap();
    repainted_frame_showing(&mut pty, HELP_MARK, ".keys did not open the key help");
    send_function_keys(&mut pty);
    let frame = flipped_frame(&mut pty);
    assert!(
        squeezed(&frame).contains(&squeezed(HELP_MARK)),
        "an F-key closed the key help:\n{frame}"
    );
    pty.send(b"q").unwrap();
    assert_help_closed(&mut pty, "q");

    pty.send(CTRL_T).unwrap();
    repainted_frame_showing(&mut pty, PAGER_MARK, "Ctrl+T did not open the pager");
    send_function_keys(&mut pty);
    pty.send(CTRL_X).unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = flipped_frame(&mut pty);
    assert!(
        squeezed(&frame).contains(&squeezed(PAGER_MARK)),
        "an F-key or Ctrl+X closed the pager:\n{frame}"
    );
    assert!(
        !frame.contains("Todo"),
        "Ctrl+X in the pager opened the todo panel:\n{frame}"
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(400));
    repainted_frame_showing(&mut pty, "no session", "Esc did not close the pager");
    leave(pty);
}

/// `?` with the composer empty but the history search band open is a
/// search character, not the key help.
#[test]
#[serial]
fn question_mark_in_the_history_search_band_opens_no_help() {
    let (_guard, mut pty) = boot("search-help", "");
    pty.send(CTRL_R).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"?").unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = flipped_frame(&mut pty);
    assert!(
        !squeezed(&frame).contains(&squeezed(HELP_MARK)),
        "? in the history search band opened the key help:\n{frame}"
    );
    assert!(
        squeezed(&frame).contains(&squeezed("history: ?")),
        "the history search band closed or did not take ? as its query:\n{frame}"
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(400));
    leave(pty);
}

/// Under a Select modal `?` and F1-F12 leave the modal up and open no key
/// help, Alt+T opens no roster (the modal has the keyboard), the unbound
/// Ctrl+G does nothing either, and Ctrl+X still shows the todo panel
/// beside the question.
#[test]
#[serial]
fn under_a_select_modal_ctrl_x_shows_the_todo_and_alt_t_and_question_mark_do_nothing() {
    let dir = fresh_config_dir("modal-chords");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with_embeddings(&dir);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(400));
    let before = pty.raw().len();
    pty.send(b".rag new-rag-m3c\r").unwrap();
    wait_for_the_modal(&mut pty, before, ".rag new-rag-m3c", &["dry-embed-a"]);
    pty.read_for(HOTKEY_GAP);

    pty.send(b"?").unwrap();
    send_function_keys(&mut pty);
    pty.send(ALT_T).unwrap();
    pty.send(b"\x07").unwrap();
    pty.read_for(Duration::from_millis(400));
    let frame = flipped_frame(&mut pty);
    let flat = squeezed(&frame);
    assert!(
        flat.contains("Esc=cancel") && flat.contains("dry-embed-a"),
        "?, an F-key, Alt+T or Ctrl+G closed the modal:\n{frame}"
    );
    assert!(
        !flat.contains(&squeezed(HELP_MARK)),
        "? or an F-key under the modal opened the key help:\n{frame}"
    );
    assert!(
        !flat.contains(&squeezed(ROSTER_MARK)),
        "Alt+T or Ctrl+G under the modal opened the roster:\n{frame}"
    );

    pty.send(CTRL_X).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = flipped_frame(&mut pty);
    assert!(
        frame.contains("Todo"),
        "Ctrl+X under the modal did not show the todo panel:\n{frame}"
    );
    assert!(
        squeezed(&frame).contains("Esc=cancel"),
        "Ctrl+X closed the modal:\n{frame}"
    );
    pty.send(CTRL_X).unwrap();
    pty.read_for(Duration::from_millis(300));
    esc_cancels_the_modal(&mut pty, ".rag new-rag-m3c");
    leave(pty);
}

/// The palette band of a repainted frame: from the end of the status row
/// above it down to the composer's rule under it; empty with no band open.
fn palette_band(frame: &str) -> &str {
    let strip_end = "Ctrl+X = todo";
    let after = frame
        .rfind(strip_end)
        .map(|at| &frame[at + strip_end.len()..])
        .unwrap_or_else(|| panic!("the status row is not on screen:\n{frame}"));
    after.split('\u{2500}').next().unwrap_or_default()
}

/// `.keys` belongs to the TUI but the command palette does not offer it,
/// while a prefix of a REPL command does open the band on that command.
#[test]
#[serial]
fn the_palette_does_not_offer_dot_keys() {
    let (_guard, mut pty) = boot("palette-keys", "");
    pty.send(b".he").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = flipped_frame(&mut pty);
    let band = palette_band(&frame);
    assert!(
        band.contains(".help"),
        "the palette does not offer .help for .he:\n{frame}"
    );
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b"\x03").unwrap();
    pty.read_for(Duration::from_millis(300));

    pty.send(b".ke").unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = flipped_frame(&mut pty);
    let band = palette_band(&frame);
    assert!(
        !band.contains(".keys"),
        "the palette offers .keys: {band:?}\n{frame}"
    );
    leave(pty);
}
