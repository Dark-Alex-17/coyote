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

fn selected(text: &str) -> Option<Command> {
    copy(text, "the selection")
}

fn mouse_at(
    harness: &mut Harness,
    kind: MouseEventKind,
    (column, row): (u16, u16),
) -> Option<Command> {
    harness.app.handle_input(TermInput::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }))
}

fn left_click(harness: &mut Harness, at: (u16, u16)) -> Option<Command> {
    mouse_at(harness, MouseEventKind::Down(MouseButton::Left), at);
    mouse_at(harness, MouseEventKind::Up(MouseButton::Left), at)
}

fn left_drag(harness: &mut Harness, from: (u16, u16), to: (u16, u16)) -> Option<Command> {
    mouse_at(harness, MouseEventKind::Down(MouseButton::Left), from);
    mouse_at(harness, MouseEventKind::Drag(MouseButton::Left), to);
    mouse_at(harness, MouseEventKind::Up(MouseButton::Left), to)
}

/// The first screen row of the last frame that holds `needle`.
fn row_with(harness: &Harness, needle: &str) -> u16 {
    let buffer = harness.terminal.backend().buffer();
    (0..buffer.area.height)
        .find(|&y| {
            let text: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            text.contains(needle)
        })
        .unwrap_or_else(|| panic!("no row holds {needle:?}:\n{buffer:?}"))
}

/// A finished tool row opened from the keyboard, its focus cleared.
fn opened_tool(harness: &mut Harness, output: &str) {
    harness.app.begin_turn("look", false);
    finished_tool(harness, "call-1", "fs_read", output);
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.handle_key(ctrl(KeyCode::Up));
    harness.press(KeyCode::Enter);
    harness.draw();
}

const PARAGRAPH: &str = "The quick brown fox jumps over the lazy dog and keeps running \
    across the field until the light goes down behind the hills.";

fn replied(harness: &mut Harness, reply: &str) {
    harness.app.begin_turn("tell me", false);
    harness.app.handle_msg(UiMsg::TextDelta(reply.to_string()));
    harness.app.handle_msg(UiMsg::StreamDone);
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.draw();
}

/// A drag over an open tool row's output copies the text under it with
/// no indent, and the release leaves the composer alone.
#[test]
fn a_drag_over_tool_output_copies_it_without_the_indent() {
    let mut harness = Harness::standalone(80, 24);
    opened_tool(&mut harness, "alpha one\nbeta two\n");
    let alpha = row_with(&harness, "alpha one");
    let beta = row_with(&harness, "beta two");
    assert_eq!(
        left_drag(&mut harness, (0, alpha), (5, beta)),
        selected("alpha one\nbeta")
    );
    assert_eq!(harness.app.composer_text(), "");
}

/// The copy goes out as an OSC 52 request when there is no system
/// clipboard, the text whole.
#[test]
fn a_dragged_selection_reaches_the_terminal_as_osc52() {
    let mut harness = Harness::standalone(80, 24);
    opened_tool(&mut harness, "alpha one\nbeta two\n");
    let alpha = row_with(&harness, "alpha one");
    let beta = row_with(&harness, "beta two");
    let Some(Command::Copy { text, .. }) = left_drag(&mut harness, (0, alpha), (5, beta)) else {
        panic!("the drag copied nothing");
    };
    let mut written = Vec::new();
    let outcome = copy_to_clipboard_with(&mut written, &text, |_| false);
    assert_eq!(outcome, CopyOutcome::Osc52 { kept_lines: None });
    assert_eq!(written, b"\x1b]52;c;YWxwaGEgb25lCmJldGE=\x07");
}

/// A double click copies the word under it; a triple click the whole
/// wrapped line, its rows joined back with the spaces the wrap took.
#[test]
fn a_double_click_copies_a_word_and_a_triple_click_the_wrapped_line() {
    let mut harness = Harness::standalone(60, 24);
    replied(&mut harness, PARAGRAPH);
    let first = row_with(&harness, "The quick brown");
    let second = row_with(&harness, "the hills.");
    assert!(second > first, "the paragraph did not wrap");
    let brown = (10, first);
    assert_eq!(left_click(&mut harness, brown), None);
    assert_eq!(left_click(&mut harness, brown), selected("brown"));

    harness.clock.advance(Duration::from_secs(1));
    let at = (2, second);
    assert_eq!(left_click(&mut harness, at), None);
    left_click(&mut harness, at);
    assert_eq!(left_click(&mut harness, at), selected(PARAGRAPH));
}

/// In the transcript view a triple click on a wrapped line's last row
/// copies the whole line too, not just the row.
#[test]
fn a_triple_click_in_the_transcript_view_copies_the_wrapped_line() {
    let mut harness = Harness::standalone(60, 24);
    replied(&mut harness, PARAGRAPH);
    harness.app.handle_key(ctrl(KeyCode::Char('t')));
    harness.draw();
    let first = row_with(&harness, "The quick brown");
    let last = row_with(&harness, "the hills.");
    assert!(last > first, "the paragraph did not wrap");
    let at = (2, last);
    assert_eq!(left_click(&mut harness, at), None);
    left_click(&mut harness, at);
    assert_eq!(left_click(&mut harness, at), selected(PARAGRAPH));
}

/// Presses further apart than the double-click window are two clicks.
#[test]
fn presses_out_of_the_window_copy_nothing() {
    let mut harness = Harness::standalone(60, 24);
    replied(&mut harness, PARAGRAPH);
    let brown = (10, row_with(&harness, "The quick brown"));
    left_click(&mut harness, brown);
    harness.clock.advance(Duration::from_millis(401));
    assert_eq!(left_click(&mut harness, brown), None);
}

/// A drag held on the transcript's top row scrolls it a line per drag
/// event, and the copy reaches the lines that came into view.
#[test]
fn a_drag_at_the_top_edge_scrolls_and_extends_the_selection() {
    let mut harness = Harness::standalone(80, 24);
    counted_turn(&mut harness, 60);
    harness.app.handle_key(ctrl(KeyCode::Up));
    harness.press(KeyCode::Enter);
    harness.draw();
    harness.app.transcript.scroll_to_bottom();
    harness.draw();
    let last = row_with(&harness, "count 60");
    let top_text: String = {
        let buffer = harness.terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect()
    };
    let top: usize = top_text.trim()["count ".len()..].parse().unwrap();
    mouse_at(
        &mut harness,
        MouseEventKind::Down(MouseButton::Left),
        (79, last),
    );
    for _ in 0..3 {
        mouse_at(
            &mut harness,
            MouseEventKind::Drag(MouseButton::Left),
            (2, 0),
        );
        harness.draw();
    }
    let Some(Command::Copy { text, .. }) =
        mouse_at(&mut harness, MouseEventKind::Up(MouseButton::Left), (2, 0))
    else {
        panic!("the drag copied nothing");
    };
    let first = format!("count {:02}", top - 3);
    assert!(
        text.starts_with(&first),
        "{text:?} does not start at {first}"
    );
    assert!(text.ends_with("count 60"), "{text:?}");
}

/// A press and release on one cell of a tool row's header folds it, as
/// before, and copies nothing.
#[test]
fn a_click_on_a_tool_row_still_folds_it() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("look", false);
    finished_tool(&mut harness, "call-1", "fs_read", "alpha one\n");
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.draw();
    let header = row_with(&harness, "\u{25B8} fs_read(");
    assert_eq!(left_click(&mut harness, (4, header)), None);
    assert!(harness.draw().contains("alpha one"));
    harness.clock.advance(Duration::from_secs(1));
    let header = row_with(&harness, "\u{25BE} fs_read(");
    assert_eq!(left_click(&mut harness, (4, header)), None);
    assert!(!harness.draw().contains("alpha one"));
}

/// A release on a tool row's header folds nothing when the press was
/// made off the transcript.
#[test]
fn a_release_without_a_press_on_the_row_folds_nothing() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("look", false);
    finished_tool(&mut harness, "call-1", "fs_read", "alpha one\n");
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.draw();
    let header = row_with(&harness, "\u{25B8} fs_read(");
    assert_eq!(left_drag(&mut harness, (2, 22), (4, header)), None);
    assert!(!harness.draw().contains("alpha one"));
}

/// With history on hand and the composer focused, the wheel scrolls the
/// transcript and never walks the history.
#[test]
fn the_wheel_scrolls_the_transcript_and_never_recalls_history() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.history.push("an older line");
    counted_turn(&mut harness, 60);
    harness.app.handle_key(ctrl(KeyCode::Up));
    harness.press(KeyCode::Enter);
    harness.press(KeyCode::Esc);
    harness.draw();
    for kind in [MouseEventKind::ScrollUp, MouseEventKind::ScrollUp] {
        assert_eq!(mouse_at(&mut harness, kind, (2, 22)), None);
    }
    harness.draw();
    assert!(harness.app.transcript.scrolled());
    assert_eq!(harness.app.composer_text(), "");
    mouse_at(&mut harness, MouseEventKind::ScrollDown, (2, 22));
    mouse_at(&mut harness, MouseEventKind::ScrollDown, (2, 22));
    harness.draw();
    assert!(!harness.app.transcript.scrolled());
    assert_eq!(harness.app.composer_text(), "");
}

/// A drag over a thread view copies its text the same way; its rows
/// carry no indent.
#[test]
fn a_drag_over_the_thread_view_copies_its_text() {
    let mut harness = Harness::standalone(80, 24);
    opened_tool(&mut harness, "alpha one\nbeta two\n");
    harness.app.handle_key(ctrl(KeyCode::Char('t')));
    harness.draw();
    let alpha = row_with(&harness, "alpha one");
    let beta = row_with(&harness, "beta two");
    assert_eq!(
        left_drag(&mut harness, (0, alpha), (3, beta)),
        selected("alpha one\nbeta")
    );
}

/// The selected cells take the selection background; Esc clears the
/// selection and does nothing else, and the next key works as usual.
#[test]
fn a_selection_is_drawn_on_its_background_until_a_key_clears_it() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.set_no_color(false);
    opened_tool(&mut harness, "alpha one\nbeta two\n");
    let alpha = row_with(&harness, "alpha one");
    let beta = row_with(&harness, "beta two");
    left_drag(&mut harness, (2, alpha), (5, beta));
    let frame = harness.draw();
    insta::assert_snapshot!("selection_80x24", frame);
    let selection = harness.app.theme.selection.bg;
    assert!(selection.is_some());
    let buffer = harness.terminal.backend().buffer();
    for x in 2..80 {
        assert_eq!(buffer[(x, alpha)].bg, selection.unwrap(), "column {x}");
    }
    for x in 0..6 {
        assert_eq!(buffer[(x, beta)].bg, selection.unwrap(), "column {x}");
    }
    assert_ne!(buffer[(6, beta)].bg, selection.unwrap());
    assert_ne!(buffer[(0, alpha)].bg, selection.unwrap());

    assert_eq!(harness.press(KeyCode::Esc), None);
    harness.draw();
    let buffer = harness.terminal.backend().buffer();
    assert_ne!(buffer[(3, alpha)].bg, selection.unwrap());
    assert!(
        !harness.app.transcript.focused(),
        "Esc cleared the selection but left the focus"
    );
}

/// An Esc pressed over a selection during a turn clears it and still
/// interrupts the turn: one press, not two.
#[test]
fn esc_during_a_turn_clears_the_selection_and_interrupts() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("tell me", false);
    harness
        .app
        .handle_msg(UiMsg::TextDelta("alpha one\n\nbeta two".to_string()));
    harness.draw();
    let alpha = row_with(&harness, "alpha one");
    assert_eq!(
        left_drag(&mut harness, (0, alpha), (4, alpha)),
        selected("alpha")
    );
    assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
    assert_eq!(harness.app.selection.current(), None);
}

/// With nothing but a selection for it to clear, an Esc clears it and
/// does nothing else.
#[test]
fn esc_with_only_a_selection_just_clears_it() {
    let mut harness = Harness::standalone(80, 24);
    replied(&mut harness, "alpha one");
    let alpha = row_with(&harness, "alpha one");
    left_drag(&mut harness, (0, alpha), (4, alpha));
    assert!(harness.app.selection.current().is_some());
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(harness.app.selection.current(), None);
    assert_eq!(
        harness.app.last_esc, None,
        "the Esc counted towards a rewind"
    );
}

/// A tool output row that nearly fills the width, followed by a short
/// line, is two lines of output: the copy keeps the newline.
#[test]
fn copying_tool_output_keeps_a_real_newline_after_a_nearly_full_row() {
    let mut harness = Harness::standalone(40, 24);
    let full = format!("{} {}", "x".repeat(20), "y".repeat(14));
    opened_tool(&mut harness, &format!("{full}\ntail\n"));
    let first = row_with(&harness, &full);
    let tail = row_with(&harness, "tail");
    assert_eq!(tail, first + 1);
    assert_eq!(
        left_drag(&mut harness, (0, first), (39, tail)),
        selected(&format!("{full}\ntail"))
    );
}

/// A reply line wrapped over several rows copies back as the one line
/// it was, with the spaces the wrap took out and no newline.
#[test]
fn copying_soft_wrapped_prose_rejoins_it_without_a_newline() {
    let mut harness = Harness::standalone(40, 24);
    replied(&mut harness, PARAGRAPH);
    let first = row_with(&harness, "The quick brown");
    let last = row_with(&harness, "hills.");
    assert!(last > first + 1, "the paragraph did not wrap");
    assert_eq!(
        left_drag(&mut harness, (0, first), (39, last)),
        selected(PARAGRAPH)
    );
}

/// A wrapped list item copies back as one line: the hanging indent its
/// continuation rows repeat is left out.
#[test]
fn copying_a_wrapped_list_item_leaves_out_its_hanging_indent() {
    let mut harness = Harness::standalone(40, 24);
    replied(&mut harness, &format!("- {PARAGRAPH}"));
    let first = row_with(&harness, "The quick brown");
    let last = row_with(&harness, "hills.");
    assert!(last > first + 1, "the item did not wrap");
    assert_eq!(
        left_drag(&mut harness, (0, first), (39, last)),
        selected(&format!("\u{2022} {PARAGRAPH}"))
    );
}

/// A word wider than the frame is split mid-word, and its copy joins
/// the pieces with no space.
#[test]
fn copying_a_word_split_by_the_wrap_rejoins_it_without_a_space() {
    let mut harness = Harness::standalone(40, 24);
    let word = "z".repeat(60);
    replied(&mut harness, &word);
    let first = row_with(&harness, "zzzz");
    let last = first + 1;
    let buffer = harness.terminal.backend().buffer();
    assert!(
        (0..buffer.area.width).any(|x| buffer[(x, last)].symbol() == "z"),
        "the word did not wrap:\n{buffer:?}"
    );
    assert_eq!(
        left_drag(&mut harness, (0, first), (39, last)),
        selected(&word)
    );
}

/// A wrapped quote keeps its bar on the first row only once copied.
#[test]
fn copying_a_wrapped_quote_leaves_out_the_repeated_bar() {
    let mut harness = Harness::standalone(40, 24);
    replied(&mut harness, &format!("> {PARAGRAPH}"));
    let first = row_with(&harness, "The quick brown");
    let last = row_with(&harness, "hills.");
    assert!(last > first + 1, "the quote did not wrap");
    assert_eq!(
        left_drag(&mut harness, (0, first), (39, last)),
        selected(&format!("\u{2502} {PARAGRAPH}"))
    );
}

/// Under `NO_COLOR` the selection is reversed instead.
#[test]
fn under_no_color_a_selection_is_reversed() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.set_no_color(true);
    opened_tool(&mut harness, "alpha one\nbeta two\n");
    let alpha = row_with(&harness, "alpha one");
    let beta = row_with(&harness, "beta two");
    left_drag(&mut harness, (2, alpha), (5, beta));
    harness.draw();
    let buffer = harness.terminal.backend().buffer();
    assert!(buffer[(3, alpha)].modifier.contains(Modifier::REVERSED));
    assert_eq!(buffer[(3, alpha)].bg, Color::Reset);
    assert!(!buffer[(3, beta + 1)].modifier.contains(Modifier::REVERSED));
}

#[test]
fn the_footer_names_no_mouse_mode_and_alt_m_does_nothing() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.handle_key(ctrl(KeyCode::Char('t')));
    assert_eq!(harness.app.handle_key(alt_char('m')), None);
    harness
        .app
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let frame = harness.draw();
    assert!(!frame.contains("mouse"), "{frame}");
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
