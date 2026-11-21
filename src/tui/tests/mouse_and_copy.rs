use super::*;

fn alt_char(c: char) -> KeyEvent {
    alt(KeyCode::Char(c))
}

fn finished_tool(harness: &mut Harness, call_id: &str, name: &str, output: &str) {
    narrate(harness, call_id, &format!("Call {name} {{}}"));
    start_tool(harness, call_id, name, None);
    tool_output(harness, call_id, output);
    finish_tool(harness, call_id, true, None);
}

#[test]
fn alt_m_flips_the_mouse_mode_the_footer_shows() {
    let mut harness = Harness::standalone(80, 24);
    let frame = harness.draw();
    assert!(frame.contains("jobs 0 . mouse scroll"), "{frame}");

    assert_eq!(
        harness.app.handle_key(alt_char('m')),
        Some(Command::SetMouseCapture(false))
    );
    let frame = harness.draw();
    assert!(frame.contains("jobs 0 . mouse select"), "{frame}");

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
    assert!(frame.contains("jobs 0 . mouse scroll"), "{frame}");
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
        Some(Command::Copy("red line\nsecond\n".to_string()))
    );
    let frame = harness.draw();
    assert!(frame.contains("Copied 2 lines from fs_read"), "{frame}");
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
        Some(Command::Copy("second\n".to_string())),
        "the closed newer row is passed over"
    );
    let frame = harness.draw();
    assert!(frame.contains("Copied 1 line from fs_ls"), "{frame}");
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
        Some(Command::Copy(
            "job__start:\nstarted\njob__check:\nstill running\n".to_string()
        ))
    );
    let frame = harness.draw();
    assert!(
        frame.contains("Copied 4 lines from job execute_command"),
        "{frame}"
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
