//! Cold-start cases for the full-screen UI. Each case boots the real
//! binary cold (fresh config and cache dirs, no model) and exercises a
//! usage pattern the lifecycle suite does not pin: malformed input bytes,
//! wide graphemes, over-width drafts, repeated commands, resize bursts and
//! a cold start on a tiny terminal. Unix-only for the same reasons as the
//! lifecycle suite; cases run one at a time because each takes a
//! controlling tty.
#![cfg(unix)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::pty::{Pty, strip_ansi};
use common::tui_harness::*;
use serial_test::serial;

fn spawn_tui_bare(dir: &Path) -> Pty {
    spawn_tui(dir, &[])
}

/// Waits until the whitespace-squeezed screen text contains `marker` at
/// or after raw offset `from`.
fn wait_for_squeezed_after(pty: &mut Pty, marker: &str, from: usize, what: &str) -> String {
    let wanted = squeezed(marker);
    let deadline = std::time::Instant::now() + PTY_DEADLINE;
    loop {
        let text = pty.read_for(Duration::from_millis(200));
        let since = strip_ansi(&String::from_utf8_lossy(
            &pty.raw()[from.min(pty.raw().len())..],
        ));
        if squeezed(&since).contains(&wanted) {
            return since;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{what}: {marker:?} never appeared on screen:\n{text}"
        );
    }
}

/// The hint, the model and the session tokens must all be on screen
/// again after `what`, without waiting for anything else to change.
fn assert_repainted_after(pty: &mut Pty, from: usize, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let repainted = strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]));
        let missing: Vec<&str> = ["Type a prompt", "dryrun:dry-model", "no session"]
            .into_iter()
            .filter(|token| !repainted.contains(token))
            .collect();
        if missing.is_empty() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "after {what} the screen was not repainted; still missing {missing:?}. \
             Printable bytes since: {repainted:?}"
        );
        pty.read_for(Duration::from_millis(200));
    }
}

/// Quits from an idle, empty composer and checks the common exit
/// promises: exit 0, cooked tty, the alternate screen left exactly once.
fn quit_and_check(mut pty: Pty, what: &str) -> String {
    pty.send(b"\x04").unwrap();
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert_eq!(exit_code(&transcript), 0, "{what}:\n{transcript}");
    assert_tty_restored(&transcript);
    assert_eq!(
        count(pty.raw(), LEAVE_ALT),
        1,
        "{what}: the alternate screen was left more than once (a panic restore?):\n{transcript}"
    );
    transcript
}

// ---------------------------------------------------------------------
// Robustness of the one stdin reader: bytes a terminal may legitimately
// deliver but no key produces (a bare ESC, a truncated CSI, an absurd
// parameter, invalid UTF-8, NUL, C1 controls, DEL) must neither panic the
// UI nor leave it deaf.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn malformed_input_bytes_neither_panic_the_ui_nor_leave_it_deaf() {
    let dir = fresh_config_dir("garbage");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui_bare(&dir);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));

    let bursts: [&[u8]; 7] = [
        b"\x1b",                            // bare ESC, nothing follows for a while
        b"\x1b[",                           // truncated CSI
        b"\x1b[99999999999999999m",         // parameter that overflows u16/u32
        b"\xff\xfe\xc3",                    // invalid UTF-8 and a dangling lead byte
        b"\x00\x7f",                        // NUL and DEL
        b"\x9b\x85\xc2\x9b",                // C1 CSI raw and as UTF-8
        b"\x1b[<35;1;1M\x1b[200~\x1b[201~", // SGR mouse report, empty paste
    ];
    for burst in bursts {
        pty.send(burst).unwrap();
        pty.read_for(Duration::from_millis(250));
    }

    // Still alive and still taking keys: a command runs and paints. A
    // typed letter plus Ctrl+C clears whatever the garbage left in the
    // draft (Ctrl+C on an empty composer would quit instead).
    let before = pty.raw().len();
    pty.send(b"z").unwrap();
    pty.read_for(Duration::from_millis(200));
    pty.send(b"\x03").unwrap();
    pty.read_for(HOTKEY_GAP);
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", before, PTY_DEADLINE)
        .expect("keys were not handled after the malformed bytes");
    assert!(
        find(pty.raw(), LEAVE_ALT, 0).is_none(),
        "the alternate screen was left while the UI should still be up"
    );

    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after malformed input bytes");
}

// ---------------------------------------------------------------------
// Wide graphemes and over-width drafts: the composer and the transcript
// have to carry text a terminal paints two cells wide (emoji, CJK, VS16)
// and a draft longer than the row, then render the dry-run echo of both.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn wide_graphemes_typed_into_the_composer_come_back_in_the_rendered_reply() {
    let dir = fresh_config_dir("wide");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui_bare(&dir);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    // The asterisks are stripped by rendering, so the plain form can only
    // come from the assistant cell, not from the typed draft or user cell.
    pty.send(
        "**\u{1F98A} fox** \u{65E5}\u{672C} \u{2714}\u{FE0F} h\u{E9}llo wide-marker\r".as_bytes(),
    )
    .unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "\u{1F98A} fox \u{65E5}\u{672C}",
        before,
        "a wide-grapheme prompt",
    );
    let since = wait_for_squeezed_after(&mut pty, "wide-marker", before, "a wide-grapheme prompt");
    assert!(
        since.contains("h\u{E9}llo"),
        "the composed character did not survive the round trip:\n{since}"
    );

    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after a wide-grapheme prompt");
}

#[test]
#[serial]
fn a_draft_longer_than_the_row_submits_and_its_reply_renders() {
    let dir = fresh_config_dir("long-draft");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui_bare(&dir);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    let filler: String = (0..280)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let prompt = format!("{filler} **longtail** end-marker\r");
    pty.send(prompt.as_bytes()).unwrap();
    let since = wait_for_squeezed_after(
        &mut pty,
        "longtail end-marker",
        before,
        "an over-width draft",
    );
    // The filler itself (280 columns on a 100-column screen) reached the
    // transcript in one piece, wrapped but not truncated.
    assert!(
        squeezed(&since).contains(&filler),
        "the over-width draft was truncated on its way to the transcript:\n{since}"
    );

    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after an over-width draft");
}

// ---------------------------------------------------------------------
// Repeating a command turn: each `.info` yields its own Command cell with
// its own copy of the table; the second never replaces or empties the
// first and the UI stays live between them.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn two_dot_info_turns_yield_two_tables_and_the_ui_stays_live() {
    let dir = fresh_config_dir("info-twice");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui_bare(&dir);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.send(b".info\r").unwrap();
    pty.wait_for("log_path", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(500));

    let second_at = pty.raw().len();
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", second_at, PTY_DEADLINE)
        .expect("the second .info painted no table");
    pty.read_for(Duration::from_millis(500));
    // A full repaint (resize round trip) shows the transcript as it
    // stands: the newest table is on screen with its own prompt row, and
    // the screen is not blank or stuck on the first turn.
    let repaint_at = pty.raw().len();
    pty.resize(39, 100).unwrap();
    pty.read_for(Duration::from_millis(300));
    pty.resize(40, 100).unwrap();
    // The table is taller than the 40-row screen, so only its tail and the
    // footer are visible after a full repaint; `.info`'s own row has
    // scrolled off, which is the transcript's job, not a loss.
    pty.wait_for_after("log_path", repaint_at, PTY_DEADLINE)
        .expect("the transcript did not repaint after the second .info");

    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after two .info turns");
}

// ---------------------------------------------------------------------
// Resize handling: a burst of window-size changes (a user dragging the
// edge) is coalesced into frames rather than drawn once per event, and the
// UI is live and clean afterwards. A terminal that is tiny from the very
// first frame (started inside a small split) comes up and leaves clean too.
// ---------------------------------------------------------------------

#[test]
#[serial]
fn a_burst_of_resizes_leaves_the_ui_live_and_painted_once_per_size_at_most() {
    let dir = fresh_config_dir("resize-burst");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui_bare(&dir);

    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(300));
    let before = pty.raw().len();
    // A run of sizes with no pause between them, the way SIGWINCH arrives
    // while an edge is dragged; the last one differs from the start size,
    // so a repaint is owed.
    let sizes: Vec<u16> = (50..=100)
        .step_by(5)
        .rev()
        .chain((55..=90).step_by(5))
        .collect();
    for &cols in &sizes {
        pty.resize(30, cols).unwrap();
    }
    pty.read_for(Duration::from_millis(1500));
    assert_repainted_after(&mut pty, before, "a burst of resizes ending at 90x30");
    // Frames are coalesced: each full repaint paints the footer hint once,
    // and a renderer that painted every event would paint once per size.
    // The burst lands as one repaint on an idle host; a second is allowed
    // for a frame that starts before the last size arrives under load.
    let since = strip_ansi(&String::from_utf8_lossy(&pty.raw()[before..]));
    let repaints = since.matches("Type a prompt").count();
    assert!(
        repaints <= 2,
        "{repaints} full repaints for {} resize events: frames were not coalesced",
        sizes.len()
    );

    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", before, PTY_DEADLINE)
        .expect("keys were not handled after the resize burst");
    pty.read_for(Duration::from_millis(300));

    // A second burst that ends back at the size it started from: a diff
    // renderer may owe no bytes for it, but the UI must still take keys.
    let same_size_at = pty.raw().len();
    for cols in [80u16, 60, 40, 60, 80, 90] {
        pty.resize(30, cols).unwrap();
    }
    pty.read_for(Duration::from_millis(800));
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", same_size_at, PTY_DEADLINE)
        .expect("keys were not handled after a resize burst back to the same size");
    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after a resize burst");
}

#[test]
#[serial]
fn a_cold_start_on_a_tiny_terminal_comes_up_and_leaves_clean() {
    let dir = fresh_config_dir("tiny-start");
    let _guard = TempDirGuard(dir.clone());
    write_dry_run_config_with(&dir, "");
    let mut pty = spawn_tui_bare(&dir);
    // Shrink before the first frame: startup probes the keyboard for two
    // seconds, so the resize lands well before anything is drawn.
    pty.resize(4, 20).unwrap();

    pty.wait_for_raw(ENTER_ALT, 0, PTY_DEADLINE)
        .expect("the alternate screen was never entered on a 20x4 terminal");
    pty.read_for(Duration::from_secs(3));
    assert!(
        find(pty.raw(), LEAVE_ALT, 0).is_none(),
        "the UI left the alternate screen on its own on a 20x4 terminal (a panic?)"
    );

    let before = pty.raw().len();
    pty.resize(40, 100).unwrap();
    assert_repainted_after(&mut pty, before, "growing a 20x4 cold start to 100x40");
    pty.send(b".info\r").unwrap();
    pty.wait_for_after("log_path", before, PTY_DEADLINE)
        .expect("keys were not handled after a tiny cold start");
    pty.read_for(HOTKEY_GAP);
    quit_and_check(pty, "after a tiny cold start");
}
