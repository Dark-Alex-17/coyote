use super::*;
use crate::tui::cells::RewindBarrier;
use crate::tui::rewind::{REWIND_ESC_GAP, rewind_barrier, rewind_exchanges};

#[test]
fn a_successful_undo_takes_the_last_exchange_out_of_the_transcript() {
    let mut harness = Harness::standalone(80, 24);
    exchange(&mut harness);
    exchange(&mut harness);
    assert_eq!(harness.app.transcript.len(), 6);
    harness.app.handle_key(ctrl(KeyCode::Up));
    harness.app.begin_turn(".undo", true);
    harness.app.end_turn(true, &TurnOutcome::default());
    let cells = harness.app.transcript.cells();
    assert_eq!(
        cells.len(),
        3,
        "the second exchange and the command cell are gone"
    );
    assert!(matches!(&cells[0], Cell::User { text, .. } if text == "first question"));
    assert!(matches!(&cells[2], Cell::Assistant { .. }));
    assert!(!harness.app.transcript.focused());
    let frame = harness.draw();
    assert!(!frame.contains(".undo"), "{frame}");
}

#[test]
fn a_failed_undo_keeps_the_transcript_and_its_command_cell() {
    let mut harness = Harness::standalone(80, 24);
    exchange(&mut harness);
    harness.app.begin_turn(".undo", true);
    harness.app.end_turn(
        true,
        &TurnOutcome {
            error: Some("nothing to undo".to_string()),
            ..TurnOutcome::default()
        },
    );
    let cells = harness.app.transcript.cells();
    assert_eq!(cells.len(), 5);
    assert!(matches!(&cells[3], Cell::Command { name, .. } if name == ".undo"));
    assert!(matches!(&cells[4], Cell::Error { .. }));
}

#[test]
fn an_undo_with_no_user_line_before_it_keeps_its_command_cell() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn(".info", true);
    harness.app.end_turn(true, &TurnOutcome::default());
    harness.app.begin_turn(".undo", true);
    harness.app.end_turn(true, &TurnOutcome::default());
    let cells = harness.app.transcript.cells();
    assert_eq!(cells.len(), 2);
    assert!(matches!(&cells[1], Cell::Command { name, .. } if name == ".undo"));
}

#[test]
fn an_undo_after_a_file_command_stops_at_the_command_cell_that_opened_the_exchange() {
    let mut harness = Harness::standalone(80, 24);
    exchange(&mut harness);
    harness.app.begin_turn(".file notes.md -- summarize", true);
    harness
        .app
        .handle_msg(UiMsg::AssistantText("a summary".to_string()));
    harness.app.end_turn(true, &TurnOutcome::default());
    assert_eq!(harness.app.transcript.len(), 5);
    harness.app.begin_turn(".undo", true);
    harness.app.end_turn(true, &TurnOutcome::default());
    let cells = harness.app.transcript.cells();
    assert_eq!(
        cells.len(),
        3,
        "the file exchange and the command cell are gone"
    );
    assert!(matches!(&cells[0], Cell::User { text, .. } if text == "first question"));
    assert!(matches!(&cells[2], Cell::Assistant { .. }));
}

/// Any answered command outside the modifying pair opened an
/// exchange, whichever REPL arm handled it.
#[test]
fn an_undo_after_an_answered_starter_or_prompt_command_stops_at_that_command_cell() {
    for command in [".starter 1", ".prompt x y"] {
        let mut harness = Harness::standalone(80, 24);
        exchange(&mut harness);
        harness.app.begin_turn(command, true);
        harness
            .app
            .handle_msg(UiMsg::AssistantText("a reply".to_string()));
        harness.app.end_turn(true, &TurnOutcome::default());
        assert_eq!(harness.app.transcript.len(), 5);
        harness.app.begin_turn(".undo", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 3, "{command}: the command's exchange is gone");
        assert!(matches!(&cells[0], Cell::User { text, .. } if text == "first question"));
        assert!(matches!(&cells[2], Cell::Assistant { .. }));
    }
}

#[test]
fn an_undo_looks_past_a_command_that_opened_no_exchange() {
    let mut harness = Harness::standalone(80, 24);
    exchange(&mut harness);
    harness.app.begin_turn(".info", true);
    harness.app.end_turn(true, &TurnOutcome::default());
    harness.app.begin_turn(".undo", true);
    harness.app.end_turn(true, &TurnOutcome::default());
    assert!(harness.draw().contains("Tip: Press"));
}

/// A `.file` whose request reached the model and failed before any
/// reply (the connection dropped, Esc before the first delta) left
/// the session holding an exchange with an interrupted reply, so the
/// `.undo` after it takes that exchange and not the one before.
#[test]
fn an_undo_after_a_file_command_whose_request_got_no_reply_stops_at_that_command_cell() {
    for began in [UiMsg::ThinkingStarted, UiMsg::StreamDone] {
        let mut harness = Harness::standalone(80, 24);
        exchange(&mut harness);
        harness.app.begin_turn(".file notes.md -- summarize", true);
        harness.app.handle_msg(began);
        harness.app.end_turn(
            true,
            &TurnOutcome {
                error: Some("connection reset".to_string()),
                ..TurnOutcome::default()
            },
        );
        assert_eq!(harness.app.transcript.len(), 5);
        assert!(matches!(
            &harness.app.transcript.cells()[3],
            Cell::Command { opened: true, .. }
        ));
        harness.app.begin_turn(".undo", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 3, "the file command and its error are gone");
        assert!(matches!(&cells[0], Cell::User { text, .. } if text == "first question"));
        assert!(matches!(&cells[2], Cell::Assistant { .. }));
    }
}

/// A `.file` that bailed on usage opened nothing in the session, so
/// the `.undo` after it takes back the exchange before it.
#[test]
fn an_undo_looks_past_a_file_command_that_bailed_before_a_reply() {
    let mut harness = Harness::standalone(80, 24);
    exchange(&mut harness);
    harness.app.begin_turn(".file", true);
    harness.app.end_turn(
        true,
        &TurnOutcome {
            error: Some("usage: .file <path>".to_string()),
            ..TurnOutcome::default()
        },
    );
    assert!(matches!(
        harness.app.transcript.cells().last(),
        Some(Cell::Error { .. })
    ));
    harness.app.begin_turn(".undo", true);
    harness.app.end_turn(true, &TurnOutcome::default());
    assert!(harness.draw().contains("Tip: Press"));
}

/// A prompt whose turn failed before any request (no API key)
/// recorded nothing in the session, so the `.undo` after it takes
/// the exchange before it, and the transcript drops the failed line
/// along with that exchange.
#[test]
fn an_undo_after_a_prompt_that_never_reached_the_model_takes_the_exchange_before_it() {
    let mut harness = Harness::standalone(80, 24);
    exchange(&mut harness);
    harness.app.begin_turn("second", false);
    harness.app.end_turn(
        false,
        &TurnOutcome {
            error: Some("no api key".to_string()),
            ..TurnOutcome::default()
        },
    );
    let cells = harness.app.transcript.cells();
    assert_eq!(cells.len(), 5);
    assert!(matches!(&cells[0], Cell::User { opened: true, .. }));
    assert!(matches!(&cells[3], Cell::User { opened: false, .. }));
    assert!(matches!(&cells[4], Cell::Error { .. }));
    harness.app.begin_turn(".undo", true);
    harness.app.end_turn(true, &TurnOutcome::default());
    let frame = harness.draw();
    assert!(matches!(
        harness.app.transcript.cells(),
        [Cell::Welcome { .. }]
    ));
    assert!(frame.contains("Tip: Press"), "{frame}");
    assert!(!frame.contains("first question"), "{frame}");
}

/// `.regenerate` replaces the last reply inside the exchange it
/// follows, so the `.undo` after it takes that whole exchange, user
/// line included, as the session does.
#[test]
fn an_undo_after_a_regenerate_takes_the_original_exchange_too() {
    let mut harness = Harness::standalone(80, 24);
    exchange(&mut harness);
    harness.app.begin_turn(".regenerate", true);
    harness
        .app
        .handle_msg(UiMsg::AssistantText("second answer".to_string()));
    harness.app.end_turn(true, &TurnOutcome::default());
    assert_eq!(harness.app.transcript.len(), 5);
    harness.app.begin_turn(".undo", true);
    harness.app.end_turn(true, &TurnOutcome::default());
    let frame = harness.draw();
    assert!(frame.contains("Tip: Press"), "{frame}");
    assert!(!frame.contains("first question"), "{frame}");
}

/// Undoing the only exchange empties the transcript; the greeting
/// comes back and the next turn clears it as on boot.
#[test]
fn undoing_the_only_exchange_brings_the_greeting_back() {
    let mut harness = Harness::standalone(80, 24);
    exchange(&mut harness);
    assert!(!harness.draw().contains("Tip: Press"));
    harness.app.begin_turn(".undo", true);
    harness.app.end_turn(true, &TurnOutcome::default());
    let frame = harness.draw();
    assert!(frame.contains("Tip: Press"), "{frame}");
    assert!(!frame.contains(".undo"), "{frame}");
    harness.app.begin_turn("second question", false);
    let frame = harness.draw();
    assert!(!frame.contains("Tip: Press"), "{frame}");
    assert!(matches!(
        harness.app.transcript.cells(),
        [Cell::User { text, .. }] if text == "second question"
    ));
}

#[test]
fn rewind_exchanges_pops_the_newest_exchanges_and_counts_how_far_it_got() {
    let mut ctx = chat_ctx();
    let mut session = Session::default();
    for prompt in ["first", "second", "third"] {
        let input = Input::from_str(&ctx, prompt, None).unwrap();
        session.add_message(&input, "reply").unwrap();
    }
    ctx.session = Some(session);
    let (popped, stopped) = rewind_exchanges(&mut ctx, 2);
    assert_eq!(popped, 2);
    assert!(stopped.is_none());
    let session = ctx.session.as_ref().unwrap();
    let prompts: Vec<&str> = session
        .messages()
        .iter()
        .filter(|m| m.role.is_user())
        .filter_map(|m| m.content.as_text())
        .collect();
    assert_eq!(prompts, ["first"]);
    assert_eq!(ctx.pending_prefill.as_deref(), Some("second"));

    let (popped, stopped) = rewind_exchanges(&mut ctx, 3);
    assert_eq!(popped, 1);
    let err = stopped.expect("the second pop finds nothing");
    assert!(err.to_string().contains("Nothing to undo"), "{err}");
    assert!(!ctx.session.as_ref().unwrap().has_user_messages());
    assert_eq!(ctx.pending_prefill.as_deref(), Some("first"));

    ctx.session = None;
    let (popped, stopped) = rewind_exchanges(&mut ctx, 1);
    assert_eq!(popped, 0);
    let err = stopped.expect("no session to pop from");
    assert!(err.to_string().contains("No session"), "{err}");
}

fn rewind(harness: &Harness) -> Option<&RewindState> {
    match &harness.app.view {
        View::Rewind(rewind) => Some(rewind),
        _ => None,
    }
}

fn three_exchanges(harness: &mut Harness) {
    for question in ["first question", "second\nwith detail", "third question"] {
        harness.app.begin_turn(question, false);
        harness
            .app
            .handle_msg(UiMsg::AssistantText("an answer".to_string()));
        harness.app.end_turn(false, &TurnOutcome::default());
    }
}

#[test]
fn esc_esc_on_an_empty_idle_composer_opens_the_rewind_picker_newest_first() {
    let mut harness = Harness::standalone(80, 24);
    three_exchanges(&mut harness);
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert!(rewind(&harness).is_none(), "one Esc is not enough");
    assert_eq!(harness.press(KeyCode::Esc), None);
    let picker = rewind(&harness).expect("the second Esc opens it");
    let titles: Vec<(usize, &str)> = picker
        .rows
        .iter()
        .map(|row| (row.opener, row.title.as_str()))
        .collect();
    assert_eq!(
        titles,
        [
            (4, "third question"),
            (2, "second\nwith detail"),
            (0, "first question"),
        ]
    );
    assert_eq!(picker.highlight, 0);
    let frame = harness.draw();
    assert_welcome_snapshot("rewind_picker_80x24", &frame);
    assert!(frame.contains("third question"), "{frame}");
    assert!(frame.contains("+1 line"), "{frame}");
    harness.press(KeyCode::Down);
    let Some(Command::Rewind(target)) = harness.press(KeyCode::Enter) else {
        panic!("the second row rewinds two exchanges");
    };
    let rows: Vec<(usize, &str)> = target
        .rows
        .iter()
        .map(|row| (row.opener, row.title.as_str()))
        .collect();
    assert_eq!(rows, [(4, "third question"), (2, "second\nwith detail")]);
    assert!(rewind(&harness).is_none());
}

#[test]
fn esc_esc_does_not_open_the_picker_when_anything_outranks_it() {
    let mut harness = Harness::standalone(80, 24);
    harness.press(KeyCode::Esc);
    harness.press(KeyCode::Esc);
    assert!(rewind(&harness).is_none(), "nothing to rewind to");

    three_exchanges(&mut harness);
    harness.type_text("typed");
    harness.press(KeyCode::Esc);
    harness.press(KeyCode::Esc);
    assert!(rewind(&harness).is_none(), "a draft");
    harness.app.set_composer_text("");

    harness.press(KeyCode::Esc);
    harness
        .clock
        .advance(REWIND_ESC_GAP + Duration::from_millis(1));
    harness.press(KeyCode::Esc);
    assert!(rewind(&harness).is_none(), "too far apart");
    harness
        .clock
        .advance(REWIND_ESC_GAP + Duration::from_millis(1));

    harness.app.begin_turn("running", false);
    assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
    assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
    assert!(rewind(&harness).is_none(), "a turn");
    harness.app.end_turn(false, &TurnOutcome::default());

    harness.type_text(".in");
    assert!(palette(&harness).is_some());
    harness.press(KeyCode::Esc);
    harness.app.set_composer_text("");
    harness.press(KeyCode::Esc);
    assert!(
        rewind(&harness).is_none(),
        "closing the palette is not an idle Esc"
    );

    let asker = ask_on_a_thread(&mut harness, confirm_request(None));
    harness.press(KeyCode::Esc);
    assert!(matches!(asker.join().unwrap(), Ok(PromptAnswer::Cancelled)));
    harness.press(KeyCode::Esc);
    assert!(
        rewind(&harness).is_none(),
        "cancelling a modal is not an idle Esc"
    );

    harness.press(KeyCode::Esc);
    assert!(rewind(&harness).is_some(), "two idle Escs in a row open it");
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert!(rewind(&harness).is_none(), "Esc closes it");
    assert_eq!(harness.app.composer_text(), "");
}

#[test]
fn under_vi_the_picker_opens_from_normal_mode() {
    let mut harness = vi_harness();
    three_exchanges(&mut harness);
    harness.press(KeyCode::Esc);
    assert_eq!(harness.app.vi_mode, Some(ViMode::Normal));
    harness.press(KeyCode::Esc);
    assert!(rewind(&harness).is_none(), "the first Esc only left insert");
    harness.press(KeyCode::Esc);
    assert!(rewind(&harness).is_some());
    harness.press(KeyCode::Enter);
    assert!(rewind(&harness).is_none());
}

/// Opens the picker on an idle composer and takes the row `down`
/// steps below the newest.
fn choose_rewind(harness: &mut Harness, down: usize) -> RewindTarget {
    harness.press(KeyCode::Esc);
    harness.press(KeyCode::Esc);
    for _ in 0..down {
        harness.press(KeyCode::Down);
    }
    match harness.press(KeyCode::Enter) {
        Some(Command::Rewind(target)) => target,
        other => panic!("the row did not rewind: {other:?}"),
    }
}

#[test]
fn a_rewind_command_pops_the_session_and_truncates_the_transcript() {
    let mut ctx = chat_ctx();
    let mut session = Session::default();
    for prompt in ["first question", "second\nwith detail", "third question"] {
        let input = Input::from_str(&ctx, prompt, None).unwrap();
        session.add_message(&input, "an answer").unwrap();
    }
    ctx.session = Some(session);
    let mut harness = Harness::for_ctx(&ctx);
    three_exchanges(&mut harness);
    assert_eq!(harness.app.transcript.len(), 6);
    let target = choose_rewind(&mut harness, 1);
    apply_rewind(&mut harness.app, &mut ctx, target);
    assert_eq!(harness.app.transcript.len(), 2);
    assert_eq!(harness.app.composer_text(), "second\nwith detail");
    assert_eq!(ctx.pending_prefill, None);
    let session = ctx.session.as_ref().unwrap();
    assert_eq!(
        session
            .messages()
            .iter()
            .filter(|m| m.role.is_user())
            .count(),
        1
    );
    assert!(!harness.draw().contains("Rewind stopped"));
}

/// A dry run keeps no session, so the prefill cannot come from a pop:
/// the chosen row's own text is what the composer gets, and the
/// transcript goes back to it all the same.
#[test]
fn a_rewind_without_a_session_still_truncates_and_prefills_the_chosen_prompt() {
    let mut ctx = chat_ctx();
    assert!(ctx.session.is_none());
    let mut harness = Harness::for_ctx(&ctx);
    three_exchanges(&mut harness);
    let target = choose_rewind(&mut harness, 1);
    apply_rewind(&mut harness.app, &mut ctx, target);
    assert_eq!(harness.app.transcript.len(), 2);
    assert_eq!(harness.app.composer_text(), "second\nwith detail");
    assert!(!harness.draw().contains("Rewind stopped"));
}

/// A session shorter than the transcript is popped as far as it goes
/// and the transcript cut back only that far, with a notice.
#[test]
fn a_rewind_the_session_cannot_finish_stops_where_the_session_did() {
    let mut ctx = chat_ctx();
    let mut session = Session::default();
    let input = Input::from_str(&ctx, "third question", None).unwrap();
    session.add_message(&input, "an answer").unwrap();
    ctx.session = Some(session);
    let mut harness = Harness::for_ctx(&ctx);
    three_exchanges(&mut harness);
    let target = choose_rewind(&mut harness, 1);
    apply_rewind(&mut harness.app, &mut ctx, target);
    assert_eq!(
        harness.app.transcript.len(),
        5,
        "two exchanges stay and the notice follows them"
    );
    assert_eq!(harness.app.composer_text(), "third question");
    assert!(!ctx.session.as_ref().unwrap().has_user_messages());
    let frame = harness.draw();
    assert!(frame.contains("Rewind stopped: Nothing to undo"), "{frame}");
}

/// A macro's one transcript row stands for every exchange it ran, so
/// the picker offers only the rows above it: popping one exchange per
/// offered row leaves the session holding exactly what the transcript
/// still shows.
#[test]
fn a_rewind_above_a_macro_row_pops_one_exchange_per_offered_row() {
    let mut ctx = chat_ctx();
    let mut session = Session::default();
    for prompt in [
        "first question",
        "macro step one",
        "macro step two",
        "third question",
    ] {
        let input = Input::from_str(&ctx, prompt, None).unwrap();
        session.add_message(&input, "an answer").unwrap();
    }
    ctx.session = Some(session);
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.macros = vec![("deploy".to_string(), "ship it".to_string())];
    for (line, command) in [
        ("first question", false),
        (".deploy prod", true),
        ("third question", false),
    ] {
        harness.app.begin_turn(line, command);
        harness
            .app
            .handle_msg(UiMsg::AssistantText("an answer".to_string()));
        harness.app.end_turn(command, &TurnOutcome::default());
    }
    assert_eq!(harness.app.transcript.len(), 6);
    harness.press(KeyCode::Esc);
    harness.press(KeyCode::Esc);
    let picker = rewind(&harness).expect("the newest row is offered");
    assert_eq!(
        picker.rows.len(),
        1,
        "nothing at or below the macro is offered"
    );
    let frame = harness.draw();
    assert!(
        frame.contains("Rewind stops at the last macro turn"),
        "{frame}"
    );
    let target = match harness.press(KeyCode::Enter) {
        Some(Command::Rewind(target)) => target,
        other => panic!("the row did not rewind: {other:?}"),
    };
    apply_rewind(&mut harness.app, &mut ctx, target);
    let cells = harness.app.transcript.cells();
    assert!(matches!(&cells[2], Cell::Command { name, .. } if name == ".deploy prod"));
    assert!(
        !cells
            .iter()
            .any(|cell| matches!(cell, Cell::User { text, .. } if text == "third question"))
    );
    assert_eq!(harness.app.composer_text(), "third question");
    let session = ctx.session.as_ref().unwrap();
    let prompts: Vec<&str> = session
        .messages()
        .iter()
        .filter(|m| m.role.is_user())
        .filter_map(|m| m.content.as_text())
        .collect();
    assert_eq!(
        prompts,
        ["first question", "macro step one", "macro step two"],
        "the macro's exchanges stay in the session as its row stays in the transcript"
    );
    assert!(!harness.draw().contains("Rewind stopped"));
}

#[test]
fn a_rewind_barrier_is_read_from_the_command_word_and_the_enabled_macros() {
    let deploy = [("deploy".to_string(), "ship it".to_string())];
    let dot = crate::config::CommandPrefix::Dot;
    let barrier =
        |line: &str, macros: &[(String, String)]| rewind_barrier(line, dot, macros, false);
    assert_eq!(barrier(".deploy prod", &deploy), Some(RewindBarrier::Macro));
    assert_eq!(barrier(".deploy prod", &[]), None);
    assert_eq!(
        barrier(".macro deploy", &deploy),
        Some(RewindBarrier::Macro)
    );
    // `.macro <unknown>` that never asked the model created the
    // macro; one that did ran a macro the list had not got.
    assert_eq!(barrier(".macro deploy", &[]), None);
    assert_eq!(
        rewind_barrier(".macro deploy", dot, &[], true),
        Some(RewindBarrier::Macro)
    );
    assert_eq!(barrier(".macro enable deploy", &deploy), None);
    assert_eq!(barrier(".macro disable deploy", &deploy), None);
    assert_eq!(barrier(".macro", &deploy), None);
    assert_eq!(barrier(".session other", &[]), Some(RewindBarrier::Session));
    assert_eq!(barrier(".agent coder", &[]), Some(RewindBarrier::Session));
    assert_eq!(barrier(".agent", &[]), None);
    assert_eq!(barrier(".exit session", &[]), Some(RewindBarrier::Session));
    assert_eq!(barrier(".empty session", &[]), Some(RewindBarrier::Session));
    assert_eq!(barrier(".edit session", &[]), Some(RewindBarrier::Session));
    assert_eq!(barrier(".edit role", &[]), None);
    assert_eq!(barrier(".exit role", &[]), None);
    assert_eq!(barrier(".info", &[]), None);
    assert_eq!(barrier("plain words", &deploy), None);
    assert_eq!(
        rewind_barrier(
            "/session other",
            crate::config::CommandPrefix::Slash,
            &[],
            false
        ),
        Some(RewindBarrier::Session)
    );
}

/// The barrier is committed with the cell: disabling the macro
/// afterwards does not let the picker count past the turn it ran,
/// and a session swap, which opened no exchange, stops it the same
/// way with its own notice.
#[test]
fn the_picker_stops_at_a_barrier_committed_before_the_macros_changed() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.macros = vec![("deploy".to_string(), "ship it".to_string())];
    for (line, command) in [
        ("first question", false),
        (".deploy prod", true),
        ("third question", false),
    ] {
        harness.app.begin_turn(line, command);
        harness
            .app
            .handle_msg(UiMsg::AssistantText("an answer".to_string()));
        harness.app.end_turn(command, &TurnOutcome::default());
    }
    harness.app.macros.clear();
    harness.press(KeyCode::Esc);
    harness.press(KeyCode::Esc);
    assert_eq!(rewind(&harness).map(|picker| picker.rows.len()), Some(1));
    let frame = harness.draw();
    assert!(
        frame.contains("Rewind stops at the last macro turn"),
        "{frame}"
    );
    harness.press(KeyCode::Esc);

    harness.app.begin_turn(".session other", true);
    harness.app.end_turn(true, &TurnOutcome::default());
    harness.app.begin_turn("fourth question", false);
    harness
        .app
        .handle_msg(UiMsg::AssistantText("an answer".to_string()));
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.press(KeyCode::Esc);
    harness.press(KeyCode::Esc);
    let picker = rewind(&harness).expect("the row above the swap is offered");
    assert_eq!(
        picker
            .rows
            .iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        ["fourth question"]
    );
    assert!(matches!(
        harness.app.transcript.cells().last(),
        Some(Cell::Notice { text, .. }) if text == "Rewind stops at the last session command"
    ));
}

/// A session command that failed left the context as it was, so its
/// cell carries no barrier and the picker counts past it.
#[test]
fn a_failed_session_command_is_not_a_barrier() {
    let mut harness = Harness::standalone(80, 24);
    exchange(&mut harness);
    harness.app.begin_turn(".session nosuch", true);
    harness.app.end_turn(
        true,
        &TurnOutcome {
            error: Some("session not found".to_string()),
            ..TurnOutcome::default()
        },
    );
    assert!(matches!(
        &harness.app.transcript.cells()[3],
        Cell::Command {
            name,
            rewind_barrier: None,
            ..
        } if name == ".session nosuch"
    ));
    harness.press(KeyCode::Esc);
    harness.press(KeyCode::Esc);
    assert_eq!(rewind(&harness).map(|picker| picker.rows.len()), Some(1));
}

/// A macro that failed at a later step already recorded the exchanges
/// before it, so its cell is a barrier like a macro that finished.
#[test]
fn a_macro_that_asked_the_model_is_a_barrier_even_when_it_failed_later() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.macros = vec![("deploy".to_string(), "ship it".to_string())];
    exchange(&mut harness);
    harness.app.begin_turn(".deploy prod", true);
    harness
        .app
        .handle_msg(UiMsg::AssistantText("step one done".to_string()));
    harness.app.end_turn(
        true,
        &TurnOutcome {
            error: Some("step two failed".to_string()),
            ..TurnOutcome::default()
        },
    );
    assert!(matches!(
        &harness.app.transcript.cells()[3],
        Cell::Command {
            name,
            rewind_barrier: Some(RewindBarrier::Macro),
            ..
        } if name == ".deploy prod"
    ));
    harness.press(KeyCode::Esc);
    harness.press(KeyCode::Esc);
    assert!(rewind(&harness).is_none());
    assert!(matches!(
        harness.app.transcript.cells().last(),
        Some(Cell::Notice { text, .. }) if text == "Rewind stops at the last macro turn"
    ));
}
