//! Pseudo-terminal coverage for the REPL's line editor.
//!
//! Reedline only paints a prompt when it owns a real terminal in raw mode, so these
//! tests drive the `pty-reedline-target` example (see `examples/`) through a pty
//! with `expectrl` and read back what the target itself reports it accepted.
//! Assertions are made on the plain `KEY:value` lines the target prints in cooked
//! mode, and on the byte stream reedline paints, never on timing: every wait is an
//! `expect` bounded by `EXPECT_TIMEOUT`.
//!
//! **Unix only.** `expectrl` does have a Windows backend (ConPTY), but this harness
//! drives a raw pty: it sizes the window through `ptyprocess` via
//! `get_process_mut()`, answers the cursor position handshake itself, and asserts on
//! the raw VT byte stream reedline paints. ConPTY re-renders that stream through its
//! own emulator, so those assertions would not transfer. The harness is therefore
//! unix-only: the dev-dependency is declared under
//! `[target.'cfg(unix)'.dev-dependencies]` and this whole file is compiled out on
//! Windows. The pty assertions do not cover Windows; the windows-latest CI lane still
//! compiles the example and the printer wiring under `-D warnings`.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use expectrl::session::{OsProcess, OsStream};
use expectrl::{Any, Expect, Session};

/// Upper bound on any single wait for target output. Generous because the target is
/// a debug build spawned cold; the tests finish in well under a second when healthy.
const EXPECT_TIMEOUT: Duration = Duration::from_secs(20);

const TERMINAL_COLUMNS: u16 = 120;
const TERMINAL_ROWS: u16 = 40;

/// Device Status Report request crossterm sends to learn where the cursor is.
const CURSOR_POSITION_QUERY: &str = "\x1b[6n";
/// Reply a terminal would give to the first query: row 2, column 1, right below the
/// `TARGET-READY` line. The same reply answers every later query too. Reedline compares
/// the reported row with its own prompt row and treats a report more than one row above
/// it as a cleared screen, flooding newlines to the bottom before it repaints; the
/// content assertions here hold either way, but keep that in mind before asserting on
/// exact rows or injecting many separate lines before one repaint.
const CURSOR_POSITION_REPLY: &str = "\x1b[2;1R";

type Target = Session<OsProcess, OsStream>;

// Mirrored in examples/pty-reedline-target.rs.
const INJECT_WHEN_BUFFER_ENV: &str = "PTY_TARGET_INJECT_WHEN_BUFFER";
const INJECT_LINE_ENV: &str = "PTY_TARGET_INJECT_LINE";
const CONTINUE_ON_CTRLC_ENV: &str = "PTY_TARGET_CONTINUE_ON_CTRLC";
const INJECT_ON_CTRLC_ENV: &str = "PTY_TARGET_INJECT_ON_CTRLC";
const INJECTED_LINE: &str = "[mesh] peer said hi";
const INJECTED_ON_CTRLC_LINE: &str = "[mesh] envoy finished while you were away";

/// Path of the example binary `cargo test` builds alongside the test executable.
fn target_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("test executable path");
    // target/<profile>/deps/<test>-<hash> -> target/<profile>/examples/<name>
    path.pop();
    path.pop();
    path.push("examples");
    path.push("pty-reedline-target");
    assert!(
        path.is_file(),
        "example binary missing at {}; run `cargo build --example pty-reedline-target` \
         or invoke through plain `cargo test`, which builds examples",
        path.display()
    );
    path
}

/// The target with every fixture knob cleared, so a test sees only what it sets itself
/// and never what the environment running `cargo test` happens to carry.
fn target_command() -> Command {
    let mut command = Command::new(target_binary());
    for knob in [
        INJECT_WHEN_BUFFER_ENV,
        INJECT_LINE_ENV,
        CONTINUE_ON_CTRLC_ENV,
        INJECT_ON_CTRLC_ENV,
    ] {
        command.env_remove(knob);
    }
    command
}

fn spawn_target(command: Command) -> Target {
    let mut session = Session::spawn(command).expect("spawn pty target");
    session
        .get_process_mut()
        .set_window_size(TERMINAL_COLUMNS, TERMINAL_ROWS)
        .expect("set pty window size");
    session.set_expect_timeout(Some(EXPECT_TIMEOUT));
    session
}

/// Waits for `needle`, answering every cursor position query that arrives first.
///
/// Reedline asks the terminal where the cursor is before it paints the prompt and
/// again on every repaint, and stalls for two seconds per unanswered query. A pty
/// has no terminal emulator behind it, so the test plays that part. Returns the
/// bytes that arrived before the needle, with the queries removed. `expectrl::Any`
/// is first-needle-wins rather than earliest-offset; that is safe here because
/// crossterm blocks on the position query until it is answered, so a query and later
/// output never share a read.
fn wait_for(session: &mut Target, needle: &str) -> Vec<u8> {
    let mut before = Vec::new();
    loop {
        let captures = session
            .expect(Any([needle, CURSOR_POSITION_QUERY]))
            .unwrap_or_else(|err| panic!("waiting for {needle:?}: {err}"));
        before.extend_from_slice(captures.before());
        if captures.get(0) == Some(CURSOR_POSITION_QUERY.as_bytes()) {
            session
                .send(CURSOR_POSITION_REPLY)
                .expect("reply to the cursor position query");
            continue;
        }
        return before;
    }
}

/// Drops CSI sequences (colours, cursor moves, erase) and leaves everything else,
/// including the `ESC 7` / `ESC 8` cursor save and restore pair reedline wraps
/// around the text after the cursor.
fn strip_csi(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            i += 1;
            continue;
        }
        out.push(char::from(bytes[i]));
        i += 1;
    }
    out
}

#[test]
fn a_typed_line_is_read_back_through_the_pty() {
    let mut session = spawn_target(target_command());

    wait_for(&mut session, "TARGET-READY");
    wait_for(&mut session, "pty");
    session.send("hello").expect("type the line");
    session.send("\r").expect("press enter");
    wait_for(&mut session, "READ:hello");
}

/// The prompt printer queues a multi-line notification as one payload whose rows
/// are joined with `\n`. In raw mode a bare line feed would stair-step, so this
/// checks what the terminal actually receives: every row starts at column one,
/// ends with CRLF, and the prompt below them still shows the half-typed text with
/// the cursor at its end.
#[test]
fn a_multi_row_injection_paints_each_row_at_column_one_above_the_prompt() {
    const FIRST_ROW: &str = "[mesh:message] one of two";
    const SECOND_ROW: &str = "[mesh:message] two of two";

    let mut command = target_command();
    command.env(INJECT_WHEN_BUFFER_ENV, "hel");
    command.env(INJECT_LINE_ENV, format!("{FIRST_ROW}\n{SECOND_ROW}"));
    let mut session = spawn_target(command);

    wait_for(&mut session, "TARGET-READY");
    wait_for(&mut session, "pty");
    session.send("hel").expect("type half the word");

    let before_first = strip_csi(&wait_for(&mut session, FIRST_ROW));
    assert!(
        before_first.ends_with('\r'),
        "first row did not start at column one: {before_first:?}"
    );
    let after_first = strip_csi(&wait_for(&mut session, SECOND_ROW));
    assert!(
        after_first.starts_with("\r\n"),
        "first row was not terminated with CRLF: {after_first:?}"
    );
    assert!(
        after_first.ends_with('\r'),
        "second row did not start at column one: {after_first:?}"
    );
    assert!(
        !after_first.contains("pty>"),
        "prompt was repainted between the two rows: {after_first:?}"
    );

    let redrawn = strip_csi(&wait_for(&mut session, "\x1b8"));
    assert!(
        redrawn.starts_with("\r\n"),
        "second row was not terminated with CRLF: {redrawn:?}"
    );
    assert!(
        redrawn.contains("pty> hel\x1b7"),
        "prompt was not redrawn with the partial text and the cursor at its end: {redrawn:?}"
    );

    session.send("lo").expect("type the rest of the word");
    session.send("\r").expect("press enter");
    wait_for(&mut session, "READ:hello");
}

/// Ctrl-C at an idle prompt with a line queued across the interrupt, the way an
/// idle-time notification does: the interrupt is reported, the line is painted, the
/// prompt comes back, and the next typed line is read whole. The `READ:` report is the
/// proof that neither the interrupt nor the injected line left the editor in a state
/// that eats or garbles what follows.
#[test]
fn ctrl_c_with_a_line_queued_across_the_interrupt_leaves_the_prompt_usable() {
    let mut command = target_command();
    command.env(CONTINUE_ON_CTRLC_ENV, "1");
    command.env(INJECT_ON_CTRLC_ENV, INJECTED_ON_CTRLC_LINE);
    let mut session = spawn_target(command);

    wait_for(&mut session, "TARGET-READY");
    wait_for(&mut session, "ARMED:ctrlc");
    wait_for(&mut session, "pty");

    session.send("\x03").expect("press ctrl-c");
    wait_for(&mut session, "SIGNAL:ctrl-c");
    let before = strip_csi(&wait_for(&mut session, INJECTED_ON_CTRLC_LINE));
    assert!(
        before.ends_with('\r'),
        "injected line did not start at column one: {before:?}"
    );
    wait_for(&mut session, "pty");

    session
        .send("hello\r")
        .expect("type a line and press enter");
    wait_for(&mut session, "READ:hello");
}

/// The other order: the line has already been painted over a half-typed buffer when
/// Ctrl-C arrives. The interrupt must clear that buffer and hand back a prompt that
/// reads the next line whole.
#[test]
fn ctrl_c_after_a_line_painted_over_a_half_typed_buffer_leaves_the_prompt_usable() {
    let mut command = target_command();
    command.env(INJECT_WHEN_BUFFER_ENV, "hel");
    command.env(INJECT_LINE_ENV, INJECTED_LINE);
    command.env(CONTINUE_ON_CTRLC_ENV, "1");
    let mut session = spawn_target(command);

    wait_for(&mut session, "TARGET-READY");
    wait_for(&mut session, "pty");
    session.send("hel").expect("type half the word");
    wait_for(&mut session, INJECTED_LINE);

    session.send("\x03").expect("press ctrl-c");
    wait_for(&mut session, "SIGNAL:ctrl-c");
    wait_for(&mut session, "pty");

    session
        .send("hello\r")
        .expect("type a line and press enter");
    wait_for(&mut session, "READ:hello");
}

/// Prompt integrity: a line printed from outside while the user is mid-word lands
/// above the prompt, the prompt is redrawn with the partial text intact and the
/// cursor still at its end, and the keystrokes that follow complete the word the
/// user was typing. The last assertion is the one that matters: it is the target's
/// own report of what `read_line` returned, so it proves the editor's buffer and
/// cursor were untouched rather than merely redrawn correctly.
#[test]
fn an_injected_line_leaves_the_half_typed_buffer_and_cursor_intact() {
    let mut command = target_command();
    command.env(INJECT_WHEN_BUFFER_ENV, "hel");
    command.env(INJECT_LINE_ENV, INJECTED_LINE);
    let mut session = spawn_target(command);

    wait_for(&mut session, "TARGET-READY");
    wait_for(&mut session, "pty");
    session.send("hel").expect("type half the word");

    wait_for(&mut session, INJECTED_LINE);
    // The repaint that follows the injected line ends with the cursor being
    // restored (ESC 8); everything painted up to that point is the redrawn prompt.
    let redrawn = strip_csi(&wait_for(&mut session, "\x1b8"));
    assert!(
        redrawn.starts_with("\r\n"),
        "injected line was not terminated with CRLF, so the redraw did not begin on a fresh line: {redrawn:?}"
    );
    assert!(
        redrawn.contains("pty> hel\x1b7"),
        "prompt was not redrawn with the partial text and the cursor at its end: {redrawn:?}"
    );

    session.send("lo").expect("type the rest of the word");
    session.send("\r").expect("press enter");
    wait_for(&mut session, "READ:hello");
}
