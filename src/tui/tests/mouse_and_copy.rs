use super::*;
use crate::tui::run_loop::{CopyOutcome, copy_notice, copy_to_clipboard_with};

fn alt_char(c: char) -> KeyEvent {
    alt(KeyCode::Char(c))
}

fn finished_tool(harness: &mut Harness, call_id: &str, name: &str, output: &str) {
    narrate(harness, call_id, &format!("Call {name} {{}}"));
    start_tool(harness, call_id, name, None);
    tool_output(harness, call_id, output);
    finish_tool(harness, call_id, true, None);
}

fn copy(text: &str, from: &str) -> Option<Command> {
    Some(Command::Copy {
        text: text.to_string(),
        from: from.to_string(),
        dropped: 0,
    })
}

#[test]
fn alt_m_flips_the_mouse_mode_the_footer_shows() {
    let mut harness = Harness::standalone(80, 24);
    let frame = harness.draw();
    assert!(frame.contains("mouse scroll\""), "{frame}");

    assert_eq!(
        harness.app.handle_key(alt_char('m')),
        Some(Command::SetMouseCapture(false))
    );
    let frame = harness.draw();
    assert!(frame.contains("mouse select\""), "{frame}");

    harness.app.handle_key(ctrl(KeyCode::Char('t')));
    assert_eq!(
        harness.app.handle_key(alt_char('m')),
        Some(Command::SetMouseCapture(true)),
        "the pager does not take Alt+M"
    );
    harness
        .app
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let frame = harness.draw();
    assert!(frame.contains("mouse scroll\""), "{frame}");
}

/// At 40 columns the left half gives way: the model's start and the
/// whole mouse mode both stay.
#[test]
fn a_narrow_footer_keeps_the_mouse_mode_whole() {
    let mut harness = Harness::standalone(40, 20);
    harness.app.handle_key(alt_char('m'));
    let frame = harness.draw();
    let footer = frame.lines().rfind(|line| !line.is_empty()).unwrap();
    assert!(footer.starts_with("\"openai:test-model"), "{frame}");
    assert!(footer.ends_with(" mouse select\""), "{frame}");
}

#[test]
fn a_failed_mouse_switch_puts_the_footer_back_and_says_so() {
    let mut harness = Harness::standalone(80, 24);
    assert_eq!(
        harness.app.handle_key(alt_char('m')),
        Some(Command::SetMouseCapture(false))
    );
    harness
        .app
        .mouse_capture_failed(true, &std::io::Error::other("tty gone"));
    let frame = harness.draw();
    assert!(frame.contains("mouse scroll\""), "{frame}");
    assert!(
        frame.contains("Could not switch the mouse mode: tty gone"),
        "{frame}"
    );
    assert_eq!(
        harness.app.handle_key(alt_char('m')),
        Some(Command::SetMouseCapture(false)),
        "the next Alt+M starts from the mode the terminal is in"
    );
}

#[test]
fn alt_c_copies_the_focused_tool_rows_whole_output_without_escapes() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("look", false);
    finished_tool(
        &mut harness,
        "call-1",
        "fs_read",
        "\x1b[31mred\x1b[0m line\nsecond\n",
    );
    finished_tool(&mut harness, "call-2", "fs_ls", "other\n");
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.handle_key(ctrl(KeyCode::Up));
    harness.app.handle_key(ctrl(KeyCode::Up));

    assert_eq!(
        harness.app.handle_key(alt_char('c')),
        copy("red line\nsecond\n", "fs_read")
    );
    let frame = harness.draw();
    assert!(
        !frame.contains("Copied"),
        "the loop reports the copy once it is done: {frame}"
    );
}

/// Alt+C under a password question still copies the focused row, and
/// nothing typed into the field comes along.
#[test]
fn alt_c_under_a_password_modal_copies_the_tool_row_not_the_field() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("look", false);
    finished_tool(&mut harness, "call-1", "fs_read", "row output\n");
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.handle_key(ctrl(KeyCode::Up));
    let req = PromptRequest::new(PromptAction::Password, "Token:", Origin::detached());
    let asker = ask_on_a_thread(&mut harness, req);
    harness.type_text("hunter2");

    let copied = harness.app.handle_key(alt_char('c'));
    assert_eq!(copied, copy("row output\n", "fs_read"));
    let Some(Command::Copy { text, .. }) = copied else {
        unreachable!()
    };
    assert!(!text.contains("hunter2"));

    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Text("hunter2".to_string())
    );
}

#[test]
fn alt_c_with_no_focus_copies_the_newest_open_tool_row() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("look", false);
    finished_tool(&mut harness, "call-1", "fs_read", "first\n");
    finished_tool(&mut harness, "call-2", "fs_ls", "second\n");
    finished_tool(&mut harness, "call-3", "fs_cat", "third\n");
    harness.app.end_turn(false, &TurnOutcome::default());
    for _ in 0..2 {
        harness.app.handle_key(ctrl(KeyCode::Up));
    }
    harness.press(KeyCode::Enter);
    harness.press(KeyCode::Esc);
    assert!(!harness.app.transcript.focused());

    assert_eq!(
        harness.app.handle_key(alt_char('c')),
        copy("second\n", "fs_ls"),
        "the closed newer row is passed over"
    );
}

#[test]
fn alt_c_on_a_job_row_joins_its_calls_under_their_names() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("build it", false);
    narrate(
        &mut harness,
        "call-1",
        "Call job__start {\"tool\":\"execute_command\"}",
    );
    start_tool(&mut harness, "call-1", "job__start", None);
    harness.app.handle_msg(UiMsg::JobStarted {
        job_id: "job_1a2b3c4d".to_string(),
        tool: "execute_command".to_string(),
    });
    tool_output(&mut harness, "call-1", "started");
    finish_tool(&mut harness, "call-1", true, None);
    narrate(
        &mut harness,
        "call-2",
        "Call job__check {\"id\":\"job_1a2b3c4d\"}",
    );
    start_tool(
        &mut harness,
        "call-2",
        "job__check",
        Some(serde_json::json!({"id": "job_1a2b3c4d"})),
    );
    tool_output(&mut harness, "call-2", "\x1b[1mstill\x1b[0m running\n");
    finish_tool(&mut harness, "call-2", true, None);
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.handle_key(ctrl(KeyCode::Down));

    assert_eq!(
        harness.app.handle_key(alt_char('c')),
        copy(
            "job__start:\nstarted\njob__check:\nstill running\n",
            "job execute_command"
        )
    );
}

#[test]
fn alt_c_with_nothing_to_copy_says_so() {
    let mut harness = Harness::standalone(80, 24);
    assert_eq!(harness.app.handle_key(alt_char('c')), None);
    let frame = harness.draw();
    assert!(
        frame.contains("No tool call to copy: focus one with Ctrl+Up or a click"),
        "{frame}"
    );

    harness.app.begin_turn("look", false);
    finished_tool(&mut harness, "call-1", "fs_read", "");
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.handle_key(ctrl(KeyCode::Up));
    assert_eq!(harness.app.handle_key(alt_char('c')), None);
    let frame = harness.draw();
    assert!(frame.contains("Nothing to copy from fs_read"), "{frame}");
}

#[test]
fn the_copy_notice_says_where_the_text_went_and_what_was_left_out() {
    let notice = |outcome, lines, dropped| copy_notice(&outcome, lines, "fs_read", dropped);

    assert_eq!(
        notice(CopyOutcome::Native, 1, 0),
        (
            NoticeKind::Checkpoint,
            "Copied 1 line from fs_read".to_string()
        )
    );
    assert_eq!(
        notice(CopyOutcome::Native, 3, 512),
        (
            NoticeKind::Checkpoint,
            "Copied 3 lines from fs_read; the oldest 512 bytes were already dropped".to_string()
        )
    );
    assert_eq!(
        notice(CopyOutcome::Osc52 { kept_lines: None }, 2, 0),
        (
            NoticeKind::Checkpoint,
            "Sent 2 lines from fs_read to the terminal clipboard".to_string()
        )
    );
    assert_eq!(
        notice(
            CopyOutcome::Osc52 {
                kept_lines: Some(40)
            },
            900,
            0
        ),
        (
            NoticeKind::Checkpoint,
            "Sent the last 40 lines from fs_read to the terminal clipboard; only those fit"
                .to_string()
        )
    );
    assert_eq!(
        notice(CopyOutcome::Failed("broken pipe".to_string()), 2, 512),
        (
            NoticeKind::ToolWarning,
            "Copy failed: broken pipe".to_string()
        )
    );
}

#[test]
fn a_native_copy_writes_nothing_to_the_terminal() {
    let mut written = Vec::new();
    let mut handed = None;
    let outcome = copy_to_clipboard_with(&mut written, "two\nlines\n", |text| {
        handed = Some(text.to_string());
        true
    });
    assert_eq!(outcome, CopyOutcome::Native);
    assert_eq!(handed.as_deref(), Some("two\nlines\n"));
    assert!(written.is_empty());
}

#[test]
fn without_a_system_clipboard_the_terminal_gets_an_osc52_request() {
    let mut written = Vec::new();
    let outcome = copy_to_clipboard_with(&mut written, "hi\n", |_| false);
    assert_eq!(outcome, CopyOutcome::Osc52 { kept_lines: None });
    assert_eq!(written, b"\x1b]52;c;aGkK\x07");
}

#[test]
fn an_osc52_copy_too_long_for_one_request_keeps_the_newest_whole_lines() {
    let line = format!("{}\n", "q".repeat(63));
    let text = line.repeat(4000);
    let mut written = Vec::new();
    let outcome = copy_to_clipboard_with(&mut written, &text, |_| false);
    let CopyOutcome::Osc52 {
        kept_lines: Some(kept),
    } = outcome
    else {
        panic!("the cut was not reported: {outcome:?}");
    };
    assert!(kept < 4000);
    let kept_text = line.repeat(kept);
    assert_eq!(
        written,
        crate::utils::osc52_sequence(&kept_text).into_bytes()
    );
}

#[test]
fn a_failed_terminal_write_is_a_failed_copy() {
    struct Broken;
    impl std::io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("the terminal is gone"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let outcome = copy_to_clipboard_with(&mut Broken, "hi", |_| false);
    assert_eq!(
        outcome,
        CopyOutcome::Failed("the terminal is gone".to_string())
    );
}
