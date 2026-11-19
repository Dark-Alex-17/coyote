//! Real-process checks written from the TUI's user-facing contract, not
//! its code, for scrolling cases the files next door leave open: a burst
//! of wheel notches that lands in one read scrolls exactly as many rows as
//! there were notches, Shift+Down scrolls back one row and End jumps back
//! to the newest output; and scrolling a notch at a time through a
//! transcript of several turns never jumps more than one row, even where
//! one reply ends and the next prompt begins. Each case runs the binary on
//! a pseudo-terminal, so the file is unix-only, and the cases run one at a
//! time.
#![cfg(unix)]

mod common;

use std::time::Duration;

use common::fake_llm::{FakeLlm, Reply};
use common::pty::{Pty, strip_ansi};
use common::tui_harness::*;
use serial_test::serial;

const CTRL_L: &[u8] = b"\x0c";
const WHEEL_UP: &[u8] = b"\x1b[<64;20;5M";
const SHIFT_DOWN: &[u8] = b"\x1b[1;2B";
const END: &[u8] = b"\x1b[F";

fn leave(mut pty: Pty) {
    pty.read_for(Duration::from_millis(300));
    let _ = pty.send(b"\x03");
    pty.read_for(Duration::from_millis(300));
    let _ = pty.send(b"\x03");
    pty.read_for(Duration::from_millis(300));
    let _ = pty.send(b"\x04");
    let transcript = pty.wait_for("tty-probe-done", PTY_DEADLINE).unwrap();
    assert!(!transcript.contains("panicked"), "{transcript}");
    assert_tty_restored(&transcript);
}

fn visible_rows(frame: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut rest = frame;
    while let Some(at) = rest.find("row-") {
        let digits: String = rest[at + 4..].chars().take(3).collect();
        if let Ok(n) = digits.parse() {
            out.push(n);
        }
        rest = &rest[at + 4..];
    }
    out
}

/// The topmost and bottommost transcript rows of a full repaint that
/// leaves the scroll position alone.
fn span(pty: &mut Pty) -> (u32, u32) {
    pty.read_for(Duration::from_millis(300));
    let from = pty.raw().len();
    pty.send(CTRL_L).unwrap();
    pty.read_for(Duration::from_millis(500));
    let frame = strip_ansi(&String::from_utf8_lossy(&pty.raw()[from..]));
    let rows = visible_rows(&frame);
    assert!(
        !rows.is_empty(),
        "Ctrl+L repainted no transcript rows:\n{frame}"
    );
    (*rows.iter().min().unwrap(), *rows.iter().max().unwrap())
}

fn rows(range: std::ops::RangeInclusive<u32>) -> String {
    range.map(|n| format!("row-{n:03}\n")).collect()
}

#[test]
#[serial]
fn a_burst_of_wheel_notches_scrolls_one_row_each_and_end_returns() {
    let dir = fresh_config_dir("wheel-burst");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![Reply::Text(format!("```\n{}```\n", rows(1..=150)))]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(30, 100).unwrap();
    pty.send(b"print the rows\r").unwrap();
    pty.wait_for("row-150", PTY_DEADLINE).unwrap();
    pty.read_for(Duration::from_millis(1500));
    let (bottom, _) = span(&mut pty);

    pty.send(&WHEEL_UP.repeat(12)).unwrap();
    let (burst, _) = span(&mut pty);
    assert_eq!(
        burst,
        bottom - 12,
        "twelve notches in one write did not scroll twelve rows: row-{bottom:03} -> row-{burst:03}"
    );

    pty.send(SHIFT_DOWN).unwrap();
    let (down, _) = span(&mut pty);
    assert_eq!(down, burst + 1, "Shift+Down did not scroll back one row");

    pty.send(END).unwrap();
    let (end, _) = span(&mut pty);
    assert_eq!(end, bottom, "End did not jump back to the newest output");
    leave(pty);
}

#[test]
#[serial]
fn notch_by_notch_scrolling_across_turns_never_jumps() {
    let dir = fresh_config_dir("wheel-cells");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::Text(format!("```\n{}```\n", rows(1..=12))),
        Reply::Text(format!("```\n{}```\n", rows(13..=24))),
        Reply::Text(format!("```\n{}```\n", rows(25..=36))),
        Reply::Text(format!("```\n{}```\n", rows(37..=48))),
    ]);
    write_fake_llm_config(&dir, &fake);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(30, 100).unwrap();
    for (n, last) in [
        (1, "row-012"),
        (2, "row-024"),
        (3, "row-036"),
        (4, "row-048"),
    ] {
        pty.send(format!("turn {n}\r").as_bytes()).unwrap();
        pty.wait_for(last, PTY_DEADLINE).unwrap();
        pty.read_for(Duration::from_millis(800));
    }
    let mut at = span(&mut pty);
    let start = at;
    let mut moved_steps = 0;
    for step in 0..30 {
        pty.send(WHEEL_UP).unwrap();
        let next = span(&mut pty);
        let (top_d, bot_d) = (at.0 - next.0, at.1 - next.1);
        assert!(
            next.0 <= at.0 && next.1 <= at.1 && top_d <= 1 && bot_d <= 1,
            "notch {step} jumped: rows {at:?} -> {next:?} (from {start:?})"
        );
        if next != at {
            moved_steps += 1;
        }
        at = next;
        if at.0 == 1 {
            break;
        }
    }
    assert!(
        moved_steps >= 15,
        "thirty notches moved the labelled rows only {moved_steps} times: {start:?} -> {at:?}"
    );
    leave(pty);
}
