use super::*;
use crate::client::Model;
use crate::repl::PreparedPaste;
use crate::tui::run_loop::read_clipboard_paste;
use crate::utils::ClipboardContent;

/// The loop's paste without its worker thread: the read and the save run
/// in line, and the result lands as the worker's message would.
fn paste_clipboard_with(
    app: &mut App,
    model: Option<&Model>,
    read: impl FnOnce() -> anyhow::Result<ClipboardContent>,
) {
    app.handle_msg(UiMsg::ClipboardPasted(read_clipboard_paste(model, read)));
}

fn paste(harness: &mut Harness, model: Option<&Model>, content: ClipboardContent) {
    paste_clipboard_with(&mut harness.app, model, || Ok(content));
}

/// Pastes the image file at `/tmp/<name>`; an absolute path keeps the test
/// clear of the working directory, which other tests move.
fn paste_image(harness: &mut Harness, name: &str) -> String {
    let path = PathBuf::from("/tmp").join(name);
    paste(
        harness,
        None,
        ClipboardContent::ImageFiles(vec![path.clone()]),
    );
    path.to_str().unwrap().to_string()
}

#[test]
fn ctrl_v_and_alt_v_ask_the_loop_to_paste() {
    let mut harness = Harness::standalone(80, 24);
    for key in [ctrl(KeyCode::Char('v')), alt(KeyCode::Char('v'))] {
        assert_eq!(
            harness.app.handle_key(key),
            Some(Command::PasteImage),
            "{key:?}"
        );
    }
}

#[test]
fn each_pasted_image_gets_the_next_placeholder_and_its_path_is_kept() {
    let mut harness = Harness::standalone(80, 24);
    let first = paste_image(&mut harness, "shot.png");
    assert_eq!(harness.app.composer_text(), "[Image #1]");
    harness.type_text(" and ");
    let second = paste_image(&mut harness, "other.png");
    assert_eq!(harness.app.composer_text(), "[Image #1] and [Image #2]");
    assert_eq!(harness.app.pasted_images, vec![first, second]);
}

#[test]
fn a_text_clipboard_is_pasted_as_text() {
    let mut harness = Harness::standalone(80, 24);
    paste(
        &mut harness,
        None,
        ClipboardContent::Text("one\r\ntwo".to_string()),
    );
    assert_eq!(harness.app.composer.lines(), ["one", "two"]);
    assert!(harness.app.pasted_images.is_empty());
}

#[test]
fn an_empty_clipboard_changes_nothing() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("draft");
    paste(&mut harness, None, ClipboardContent::Empty);
    assert_eq!(harness.app.composer_text(), "draft");
    assert!(harness.app.pasted_images.is_empty());
}

#[test]
fn what_cannot_be_pasted_is_said_in_the_transcript() {
    let mut harness = Harness::standalone(120, 24);
    paste(&mut harness, None, ClipboardContent::NonImageFiles(2));
    paste_clipboard_with(&mut harness.app, None, || {
        Err(anyhow::anyhow!("no system clipboard"))
    });
    let frame = harness.draw();
    assert!(frame.contains("Copied files are not images."), "{frame}");
    assert!(
        frame.contains("Clipboard unavailable: no system clipboard"),
        "{frame}"
    );
    assert_eq!(harness.app.composer_text(), "");
}

/// With the context in hand the model is checked at paste time, as the
/// REPL does; during a turn the image is taken and the send checks it.
#[test]
fn an_image_for_a_model_without_vision_is_refused_while_idle() {
    let ctx = chat_ctx();
    assert!(!ctx.current_model().supports_vision());
    let mut harness = Harness::new(200, 24, plain_config(), FooterInfo::default());
    paste(
        &mut harness,
        Some(ctx.current_model()),
        ClipboardContent::ImageFiles(vec![PathBuf::from("shot.png")]),
    );
    let frame = harness.draw();
    assert!(frame.contains("does not accept images"), "{frame}");
    assert_eq!(harness.app.composer_text(), "");
    assert!(harness.app.pasted_images.is_empty());

    paste_image(&mut harness, "shot.png");
    assert_eq!(harness.app.composer_text(), "[Image #1]");
}

#[test]
fn backspace_and_delete_take_a_pasted_placeholder_whole() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("see ");
    paste_image(&mut harness, "shot.png");
    harness.type_text(" now");
    for _ in 0..4 {
        harness.press(KeyCode::Left);
    }
    harness.press(KeyCode::Backspace);
    assert_eq!(harness.app.composer_text(), "see  now");

    harness.app.set_composer_text("[Image #1] now");
    harness.press(KeyCode::Home);
    harness.press(KeyCode::Delete);
    assert_eq!(harness.app.composer_text(), " now");
}

#[test]
fn a_placeholder_naming_no_pasted_image_is_edited_char_by_char() {
    let mut harness = Harness::standalone(80, 24);
    paste_image(&mut harness, "shot.png");
    harness.type_text(" [Image #9]");
    harness.press(KeyCode::Backspace);
    assert_eq!(harness.app.composer_text(), "[Image #1] [Image #9");

    harness.app.set_composer_text("[Image #9]");
    harness.press(KeyCode::Home);
    harness.press(KeyCode::Delete);
    assert_eq!(harness.app.composer_text(), "Image #9]");
}

#[test]
fn an_idle_submit_hands_the_pasted_paths_on_with_the_line() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("look at ");
    let path = paste_image(&mut harness, "shot.png");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit(
            "look at [Image #1]".to_string(),
            vec![path]
        ))
    );
    assert!(harness.app.pasted_images.is_empty());
}

#[test]
fn clearing_or_replacing_the_line_forgets_the_pasted_images() {
    let mut harness = Harness::standalone(80, 24);
    paste_image(&mut harness, "shot.png");
    harness.app.handle_key(ctrl(KeyCode::Char('c')));
    assert_eq!(harness.app.composer_text(), "");
    assert!(harness.app.pasted_images.is_empty());

    harness.type_text("earlier");
    harness.press(KeyCode::Enter);
    paste_image(&mut harness, "shot.png");
    harness.press(KeyCode::Up);
    assert_eq!(harness.app.composer_text(), "earlier");
    assert!(harness.app.pasted_images.is_empty());
}

#[test]
fn a_line_with_images_waits_for_the_turn_and_a_plain_one_is_queued() {
    let mut harness = Harness::new(120, 24, plain_config(), FooterInfo::default());
    harness.app.begin_turn("explain", false);
    harness.type_text("and this ");
    let path = paste_image(&mut harness, "shot.png");
    assert_eq!(harness.press(KeyCode::Enter), None);
    assert_eq!(harness.app.composer_text(), "and this [Image #1]");
    assert_eq!(harness.app.pasted_images, vec![path]);
    assert!(harness.app.queued.is_empty());
    let frame = harness.draw();
    assert!(
        frame.contains("A line with images waits for the turn to end"),
        "{frame}"
    );

    harness.app.set_composer_text("");
    harness.type_text("plain");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Queue("plain".to_string()))
    );
}

#[test]
fn a_line_held_for_its_images_is_sent_by_enter_once_the_turn_ends() {
    let mut harness = Harness::new(120, 24, plain_config(), FooterInfo::default());
    harness.app.begin_turn("explain", false);
    harness.type_text("and this ");
    let path = paste_image(&mut harness, "shot.png");
    assert_eq!(harness.press(KeyCode::Enter), None);

    harness.app.end_turn(false, &TurnOutcome::default());
    assert_eq!(harness.app.composer_text(), "and this [Image #1]");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit(
            "and this [Image #1]".to_string(),
            vec![path]
        ))
    );
    assert_eq!(harness.app.composer_text(), "");
    assert!(harness.app.pasted_images.is_empty());
}

#[test]
fn ctrl_v_and_alt_v_paste_during_a_turn_too() {
    let mut harness = Harness::new(120, 24, plain_config(), FooterInfo::default());
    harness.app.begin_turn("explain", false);
    for key in [ctrl(KeyCode::Char('v')), alt(KeyCode::Char('v'))] {
        assert_eq!(
            harness.app.handle_key(key),
            Some(Command::PasteImage),
            "{key:?}"
        );
    }
}

#[test]
fn ctrl_v_and_alt_v_leave_the_cursor_of_a_long_draft_where_it_was() {
    let mut harness = Harness::standalone(40, 8);
    let draft: Vec<String> = (0..30).map(|n| format!("line{n}")).collect();
    harness.app.set_composer_text(&draft.join("\n"));
    for _ in 0..30 {
        harness.press(KeyCode::Up);
    }
    harness.press(KeyCode::Home);
    harness.draw();
    harness.app.handle_key(ctrl(KeyCode::Char('v')));
    harness.app.handle_key(alt(KeyCode::Char('v')));
    harness.type_text("X");
    assert_eq!(harness.app.composer.lines()[0], "Xline0");
    assert_eq!(harness.app.composer.lines().len(), 30);
}

#[test]
fn one_clipboard_holding_two_images_gets_a_placeholder_for_each() {
    let mut harness = Harness::standalone(80, 24);
    paste(
        &mut harness,
        None,
        ClipboardContent::ImageFiles(vec![PathBuf::from("a.png"), PathBuf::from("b.png")]),
    );
    let text = harness.app.composer_text();
    assert!(text.contains("[Image #1]"), "{text}");
    assert!(text.contains("[Image #2]"), "{text}");
    assert_eq!(harness.app.pasted_images.len(), 2);
}

#[test]
fn a_placeholder_removed_whole_leaves_a_plain_line_to_send() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("hi ");
    paste_image(&mut harness, "shot.png");
    harness.press(KeyCode::Backspace);
    assert_eq!(harness.app.composer_text(), "hi ");
    assert!(matches!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit(line, _)) if line == "hi"
    ));
    assert!(harness.app.pasted_images.is_empty());
}

#[test]
fn pastes_are_numbered_in_the_order_they_land() {
    let mut harness = Harness::standalone(80, 24);
    let second = PreparedPaste::Images(vec!["/tmp/second.png".to_string()]);
    let first = PreparedPaste::Images(vec!["/tmp/first.png".to_string()]);
    harness.app.handle_msg(UiMsg::ClipboardPasted(second));
    harness.type_text(" ");
    harness.app.handle_msg(UiMsg::ClipboardPasted(first));
    assert_eq!(harness.app.composer_text(), "[Image #1] [Image #2]");
    assert_eq!(
        harness.app.pasted_images,
        ["/tmp/second.png", "/tmp/first.png"]
    );
}

#[test]
fn a_paste_landing_after_the_send_goes_into_the_next_draft() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text("sent");
    assert!(matches!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit(..))
    ));
    harness.type_text("next ");
    harness
        .app
        .handle_msg(UiMsg::ClipboardPasted(PreparedPaste::Images(vec![
            "/tmp/late.png".to_string(),
        ])));
    assert_eq!(harness.app.composer_text(), "next [Image #1]");
    assert_eq!(harness.app.pasted_images, ["/tmp/late.png"]);
}

#[test]
fn a_prefill_replacing_the_draft_forgets_the_pasted_images() {
    let mut ctx = chat_ctx();
    let mut harness = Harness::for_ctx(&ctx);
    paste_image(&mut harness, "shot.png");
    ctx.pending_prefill = Some("edited line".to_string());
    harness.app.sync_from_ctx(&mut ctx);
    assert_eq!(harness.app.composer_text(), "edited line");
    assert!(harness.app.pasted_images.is_empty());
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit("edited line".to_string(), Vec::new()))
    );
}
