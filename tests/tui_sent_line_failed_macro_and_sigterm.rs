//! Black-box pins for the bottom pane's sent-line, failed-macro and
//! SIGTERM edges: a queued line the model received leaves the `pending:`
//! strip while the SAME turn still runs and Alt+Up never brings it back;
//! a non-isolated macro that recorded an exchange before a later step
//! failed is still a rewind barrier (one that failed before any exchange
//! is not); SIGTERM with queued lines drops them even when the tool is
//! released in the same instant. The built binary on a pseudo-terminal,
//! a scripted fake model where a tool round or a session is needed.
//! Unix-only and serial like its sibling pty suites.
#![cfg(unix)]

mod common;

use std::fs;
use std::time::{Duration, Instant};

use common::fake_llm::{FakeLlm, Reply};
use common::tui_harness::*;
use serial_test::serial;

// ---------------------------------------------------------------------
// A line the model received leaves the strip while the turn still runs,
// and Alt+Up never brings it back.
// ---------------------------------------------------------------------

/// Tool A sleeps: `sent-one`, `sent-two` are queued (`pending: 2`).
/// Releasing A hands both to the model at the tool-round boundary; while
/// tool B sleeps in the SAME turn the strip is gone and both lines are
/// dim `(sent mid-turn)` cells. Alt+Up on the empty composer brings
/// nothing back. A later `late-three` is queued, Alt+Up pulls THAT line
/// (not a sent one) into the composer, Enter re-queues it, and releasing
/// B sends it; three requests in all.
#[test]
#[serial]
fn a_sent_line_leaves_the_strip_mid_turn_and_alt_up_never_pops_it() {
    let dir = fresh_config_dir("sent-strip");
    let _guard = TempDirGuard(dir.clone());
    let tool_a = dir.join("tool-a");
    let tool_b = dir.join("tool-b");
    let fake = FakeLlm::start(vec![
        sleep_call(&tool_a),
        sleep_call(&tool_b),
        Reply::Text("done-w3s".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(40, 100).unwrap();
    pty.read_for(Duration::from_millis(400));
    let mut toggle = false;

    let turn_at = pty.raw().len();
    pty.send(b"call the sleep tool\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-a-started"),
        "the sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    pty.send(b"sent-one-w3\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(b"sent-two-w3\r").unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    assert!(
        squeezed(&frame).contains("pending:2"),
        "two lines queued while tool A slept did not show `pending: 2`:\n{frame}"
    );

    // Release A: the queue is drained into request 2, which asks for B.
    fs::write(dir.join("tool-a-release"), b"").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-b-started"),
        "the second sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(800));

    let mut misses: Vec<String> = Vec::new();
    // The strip is gone and cells committed while B still sleeps.
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    let marker = squeezed(MID_TURN_MARKER);
    for line in ["sent-one-w3", "sent-two-w3"] {
        if !flat.contains(&format!(">{line}{marker}")) {
            misses.push(format!(
                "`{line}` was handed to the model but has no `(sent mid-turn)` cell while the \
                 turn still runs:\n{frame}"
            ));
        }
    }
    if frame.contains("pending:") {
        misses.push(format!(
            "the queue strip outlived the lines it previewed (the model has them, tool B is \
             still running):\n{frame}"
        ));
    }
    if fake.served() != 2 {
        misses.push(format!(
            "expected two requests after tool A was released, got {}",
            fake.served()
        ));
    }

    // Alt+Up on the empty composer does not resurrect a sent line.
    pty.send(ALT_UP).unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    let row = composer_row_with_footer(&frame, "fake:fake-model");
    if row.contains("sent-one-w3") || row.contains("sent-two-w3") {
        misses.push(format!(
            "Alt+Up popped a line the model already received back into the composer \
             (`{row}`):\n{frame}"
        ));
    } else if !shows_placeholder(&flat) {
        misses.push(format!(
            "Alt+Up over an empty queue left the composer non-empty (`{row}`):\n{frame}"
        ));
    }
    if frame.contains("pending:") {
        misses.push(format!(
            "Alt+Up over an empty queue made a `pending:` strip appear:\n{frame}"
        ));
    }

    // A later line queues alone; Alt+Up pulls it, not a sent line.
    pty.send(b"late-three-w3\r").unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    if !flat.contains("pending:1") {
        misses.push(format!(
            "a line queued while tool B slept did not show `pending: 1`:\n{frame}"
        ));
    }
    let strip = squeezed(&above_rule(&frame));
    if !strip.contains("late-three-w3") {
        misses.push(format!(
            "the strip does not preview the one line it holds:\n{frame}"
        ));
    }
    pty.send(ALT_UP).unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    let row = composer_row_with_footer(&frame, "fake:fake-model");
    if row != "late-three-w3" {
        misses.push(format!(
            "Alt+Up did not bring the one queued line back for editing (composer: `{row}`):\n{frame}"
        ));
    }
    if frame.contains("pending:") {
        misses.push(format!(
            "after Alt+Up took the only queued line the strip still shows `pending:`:\n{frame}"
        ));
    }
    if fake.served() != 2 {
        misses.push(format!(
            "Alt+Up moved the turn on ({} requests served)",
            fake.served()
        ));
    }
    // Enter re-queues it.
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(600));
    let frame = next_frame(&mut pty, &mut toggle);
    if !squeezed(&frame).contains("pending:1") {
        misses.push(format!(
            "Enter on the line Alt+Up brought back did not re-queue it:\n{frame}"
        ));
    }

    // Release B; the re-queued line rides request 3.
    fs::write(dir.join("tool-b-release"), b"").unwrap();
    wait_for_squeezed_after(&mut pty, "done-w3s", turn_at, "the turn's final reply");
    wait_until_no_pending(&mut pty);
    let bodies = fake.bodies();
    if bodies.len() != 3 {
        misses.push(format!(
            "expected three requests, got {}:\n{}",
            bodies.len(),
            bodies.join("\n---\n")
        ));
    } else {
        if !bodies[1].contains("sent-one-w3") || !bodies[1].contains("sent-two-w3") {
            misses.push(format!(
                "the two lines queued during tool A did not ride request 2:\n{}",
                bodies[1]
            ));
        }
        if !bodies[2].contains("late-three-w3") {
            misses.push(format!(
                "the line re-queued during tool B did not ride request 3:\n{}",
                bodies[2]
            ));
        }
        // The sent lines stay in the history exactly once each: a
        // resurrected line would be sent again as a new user message.
        for line in ["sent-one-w3", "sent-two-w3"] {
            let times = bodies[2].matches(line).count();
            if times != 1 {
                misses.push(format!(
                    "`{line}` appears {times} times in request 3 (sent again?):\n{}",
                    bodies[2]
                ));
            }
        }
    }
    let frame = next_frame(&mut pty, &mut toggle);
    let flat = squeezed(&frame);
    if !flat.contains(&format!(">late-three-w3{marker}")) {
        misses.push(format!(
            "the re-queued line has no `(sent mid-turn)` cell after the turn:\n{frame}"
        ));
    }
    if !shows_placeholder(&flat) {
        misses.push(format!(
            "the composer is not empty after the turn (composer: `{}`):\n{frame}",
            composer_row_with_footer(&frame, "fake:fake-model")
        ));
    }
    quit_clearing_any_draft(&mut pty);
    assert!(
        misses.is_empty(),
        "{} sent-line expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// A macro that recorded an exchange is a barrier even when a later step
// failed; one that failed before any exchange is not.
// ---------------------------------------------------------------------

/// `.session s7`, `before`, macro `half` = [`inside`, `.session nosuch`]
/// (the first step records an exchange, the second fails inside a
/// session), `after`. The picker offers `after` alone with the macro
/// notice; Enter pops `after` only; `inside` and `before` stay on the
/// transcript and in the next request.
#[test]
#[serial]
fn a_macro_that_recorded_an_exchange_is_a_barrier_even_when_a_later_step_failed() {
    let dir = fresh_config_dir("failed-macro-barrier");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::Text("reply-before-w3m".to_string()),
        Reply::Text("reply-inside-w3m".to_string()),
        Reply::Text("reply-after-w3m".to_string()),
        Reply::Text("reply-tail-w3m".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_macro(&dir, "half-w3", &["inside-w3m", ".session nosuch-w3"]);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    start_session(&mut pty, "s7-w3m");
    fake_exchange(&mut pty, "before-w3m", "reply-before-w3m");
    let macro_at = pty.raw().len();
    pty.send(b".half-w3\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "reply-inside-w3m",
        macro_at,
        "the macro's first step's reply",
    );
    let failed = wait_for_squeezed_after(
        &mut pty,
        "Already in a session",
        macro_at,
        "the macro's failing second step",
    );
    assert!(
        !squeezed(&failed).contains("nosuch-w3.agents"),
        "`.session nosuch-w3` inside the macro switched sessions instead of failing:\n{failed}"
    );
    pty.read_for(Duration::from_millis(1500));
    fake_exchange(&mut pty, "after-w3m", "reply-after-w3m");
    assert_eq!(fake.served(), 3, "{}", fake.bodies().join("\n---\n"));

    let mut misses: Vec<String> = Vec::new();
    let picker_at = open_rewind_picker(&mut pty);
    let picker = picker_frame(&mut pty, picker_at, 101);
    let band_flat = squeezed(&picker_band(&picker));
    if !band_flat.contains("after-w3m") {
        misses.push(format!("the newest prompt is not a rewind row:\n{picker}"));
    }
    if band_flat.contains("before-w3m") {
        misses.push(format!(
            "a macro that recorded an exchange before failing is NOT a barrier: `before` is \
             offered:\n{picker}"
        ));
    }
    if band_flat.contains("inside-w3m") {
        misses.push(format!(
            "the failed macro's own exchange is offered as a rewind row:\n{picker}"
        ));
    }
    if !squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
        misses.push(format!(
            "the picker does not say it stops at the last macro turn:\n{}",
            since(&pty, picker_at)
        ));
    }
    // Down cannot reach past the barrier; Enter pops `after` only.
    pty.send(DOWN).unwrap();
    pty.read_for(Duration::from_millis(400));
    pty.send(b"\r").unwrap();
    pty.read_for(Duration::from_millis(1500));
    let frame = repainted_frame_after_resize(&mut pty, 60, 100);
    let flat = squeezed(&frame);
    if flat.contains("Rewindto") {
        misses.push(format!("the picker is still up after Enter:\n{frame}"));
    }
    if flat.contains(">after-w3m") {
        misses.push(format!("the chosen exchange was not popped:\n{frame}"));
    }
    if !flat.contains(">inside-w3m") || !flat.contains(">before-w3m") {
        misses.push(format!(
            "the rewind went past the failed macro's barrier:\n{frame}"
        ));
    }
    let row = composer_row_with_footer(&frame, "fake:fake-model");
    if row != "after-w3m" {
        misses.push(format!(
            "the chosen prompt was not prefilled (composer: `{row}`):\n{frame}"
        ));
    }
    clear_any_draft(&mut pty);
    fake_exchange(&mut pty, "tail-w3m", "reply-tail-w3m");
    let bodies = fake.bodies();
    if bodies.len() != 4 {
        misses.push(format!(
            "expected four requests, got {}:\n{}",
            bodies.len(),
            bodies.join("\n---\n")
        ));
    } else {
        if bodies[3].contains("after-w3m") {
            misses.push(format!(
                "the popped exchange is still in the session's next request:\n{}",
                bodies[3]
            ));
        }
        if !bodies[3].contains("inside-w3m") || !bodies[3].contains("before-w3m") {
            misses.push(format!(
                "the session lost an exchange the transcript still shows:\n{}",
                bodies[3]
            ));
        }
    }
    quit_declining_save(&mut pty);
    assert!(
        misses.is_empty(),
        "{} failed-macro barrier expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

/// Control: `.session s7`, `before`, macro `nothing` = [`.session
/// nosuch`] (fails before any exchange), `after`. The picker offers
/// `after` AND `before`: a macro that neither succeeded nor recorded an
/// exchange cuts nothing.
#[test]
#[serial]
fn a_macro_that_failed_before_any_exchange_is_not_a_barrier() {
    let dir = fresh_config_dir("empty-failed-macro");
    let _guard = TempDirGuard(dir.clone());
    let fake = FakeLlm::start(vec![
        Reply::Text("reply-before-w3n".to_string()),
        Reply::Text("reply-after-w3n".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_macro(&dir, "nothing-w3", &[".session nosuch-w3"]);
    let mut pty = spawn_tui(&dir, &[]);
    pty.wait_for("no session", PTY_DEADLINE).unwrap();
    pty.resize(60, 100).unwrap();
    pty.read_for(Duration::from_millis(400));

    start_session(&mut pty, "s7-w3n");
    fake_exchange(&mut pty, "before-w3n", "reply-before-w3n");
    let macro_at = pty.raw().len();
    pty.send(b".nothing-w3\r").unwrap();
    wait_for_squeezed_after(
        &mut pty,
        "Already in a session",
        macro_at,
        "the macro's failing step",
    );
    pty.read_for(Duration::from_millis(1500));
    fake_exchange(&mut pty, "after-w3n", "reply-after-w3n");
    assert_eq!(fake.served(), 2, "{}", fake.bodies().join("\n---\n"));

    let mut misses: Vec<String> = Vec::new();
    let picker_at = open_rewind_picker(&mut pty);
    let picker = picker_frame(&mut pty, picker_at, 101);
    let band_flat = squeezed(&picker_band(&picker));
    if !band_flat.contains("after-w3n") {
        misses.push(format!("the newest prompt is not a rewind row:\n{picker}"));
    }
    if !band_flat.contains("before-w3n") {
        misses.push(format!(
            "a macro that failed before recording anything cut the rewind rows: `before` is \
             not offered:\n{picker}"
        ));
    }
    if squeezed(&since(&pty, picker_at)).contains(&squeezed(MACRO_STOP_NOTICE)) {
        misses.push(format!(
            "the picker says it stops at a macro turn though the macro recorded nothing:\n{}",
            since(&pty, picker_at)
        ));
    }
    // Back out without popping.
    pty.send(b"\x1b").unwrap();
    pty.read_for(Duration::from_millis(600));
    quit_declining_save(&mut pty);
    assert!(
        misses.is_empty(),
        "{} control expectation(s) missed:\n\n{}",
        misses.len(),
        misses.join("\n\n")
    );
}

// ---------------------------------------------------------------------
// SIGTERM drops the queued lines even when the tool is released at once.
// ---------------------------------------------------------------------

/// Two lines queued while a tool sleeps, SIGTERM and the release file in
/// the same instant (so an unpurged queue would ride the next request):
/// the process leaves cleanly within the deadline, neither line reaches a
/// request body, and the scripted reply behind the exit never streams.
#[test]
#[serial]
fn sigterm_purges_the_turn_queue_even_when_the_tool_is_released_right_after() {
    let dir = fresh_config_dir("sigterm-release");
    let _guard = TempDirGuard(dir.clone());
    let tool_a = dir.join("tool-a");
    let fake = FakeLlm::start(vec![
        sleep_call(&tool_a),
        Reply::Text("never-sent-w3t".to_string()),
        Reply::Text("never-sent-either-w3t".to_string()),
    ]);
    write_fake_llm_config(&dir, &fake);
    write_tool(&dir, "fixture_sleep.sh", SLEEP_TOOL);
    let mut pty = spawn_tui(&dir, &[]);
    let transcript = pty.wait_for("no session", PTY_DEADLINE).unwrap();
    let pid = child_pid(&transcript);

    pty.send(b"first-turn-w3t\r").unwrap();
    wait_for_file(
        &mut pty,
        &dir.join("tool-a-started"),
        "the sleep tool's start marker",
    );
    pty.read_for(Duration::from_millis(300));
    pty.send(b"ghost-a-w3t\r").unwrap();
    pty.read_for(Duration::from_millis(500));
    pty.send(b"ghost-b-w3t\r").unwrap();
    pty.read_for(Duration::from_millis(800));
    let frame = repainted_frame_after_resize(&mut pty, 40, 101);
    assert!(
        squeezed(&frame).contains("pending:2"),
        "two lines queued while the tool slept did not show `pending: 2`:\n{frame}"
    );

    let signalled_at = Instant::now();
    // SAFETY: kill takes a pid and a signal number and touches no memory.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    fs::write(dir.join("tool-a-release"), b"").unwrap();
    let transcript = pty
        .wait_for("tty-probe-done", PTY_DEADLINE)
        .unwrap_or_else(|e| panic!("SIGTERM during the tool round did not end the session: {e}"));
    assert_eq!(exit_code(&transcript), 0, "{transcript}");
    assert!(
        signalled_at.elapsed() < Duration::from_secs(15),
        "SIGTERM took {:?} to end the session",
        signalled_at.elapsed()
    );
    assert_tty_restored(&transcript);
    assert_eq!(count(pty.raw(), LEAVE_ALT), 1, "{transcript}");
    // Time for a wrongly flushed queued line to open a request.
    std::thread::sleep(Duration::from_millis(1500));
    let bodies = fake.bodies();
    assert!(
        bodies
            .iter()
            .all(|b| !b.contains("ghost-a-w3t") && !b.contains("ghost-b-w3t")),
        "a line queued before SIGTERM reached the model once the tool finished:\n{}",
        bodies.join("\n---\n")
    );
    assert!(
        !transcript.contains("never-sent"),
        "a reply scripted behind the signal exit streamed:\n{transcript}"
    );
}
