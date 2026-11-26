use super::*;
use crate::config::CommandPrefix;

/// A row only the key help shows.
const HELP_MARK: &str = "Suspend to the shell";
/// What the key help's last row does.
const LAST_HELP_ROW: &str = "Close the help";

fn harness_under(prefix: CommandPrefix) -> Harness {
    let footer = FooterInfo {
        model: "openai:test-model".to_string(),
        ..FooterInfo::default()
    };
    let config = Arc::new(AppConfig {
        command_prefix: prefix,
        ..(*plain_config()).clone()
    });
    Harness::new(80, 24, config, footer)
}

fn assert_help_closed(harness: &mut Harness, by: &str) {
    assert!(
        harness.app.help_mut().is_none(),
        "{by} did not close the key help"
    );
    let frame = harness.draw();
    assert!(frame.contains("no session"), "{by}: {frame}");
    assert!(
        !frame.contains(HELP_MARK),
        "the key help is still up after {by}:\n{frame}"
    );
}

/// The frames of the open key help from its top, one PageDown apart,
/// until its last row is on screen or a PageDown no longer moves it.
fn help_pages(harness: &mut Harness) -> Vec<String> {
    let mut pages = vec![harness.draw()];
    while !squeezed(pages.last().unwrap()).contains(&squeezed(LAST_HELP_ROW)) {
        assert!(pages.len() < 40, "the key help never reached its end");
        harness.press(KeyCode::PageDown);
        let page = harness.draw();
        let stuck = page == *pages.last().unwrap();
        pages.push(page);
        if stuck {
            break;
        }
    }
    pages
}

/// A function key's name as a whole word (`F1`..`F12`) anywhere in `text`.
fn names_a_function_key(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c != 'F' || (i > 0 && chars[i - 1].is_alphanumeric()) {
            continue;
        }
        let digits: String = chars[i + 1..]
            .iter()
            .take_while(|d| d.is_ascii_digit())
            .collect();
        let after = chars.get(i + 1 + digits.len());
        if !digits.is_empty() && after.is_none_or(|a| !a.is_alphanumeric()) {
            return Some(format!("F{digits}"));
        }
    }
    None
}

/// `.helpx` is not a command even though `.help` is its prefix: red, not
/// green. `.help x` keeps `.help` green with the argument plain. Same
/// under the slash prefix.
#[test]
fn a_command_prefix_followed_by_more_letters_is_painted_red_not_green() {
    for (prefix, sigil) in [(CommandPrefix::Dot, '.'), (CommandPrefix::Slash, '/')] {
        let mut harness = harness_under(prefix);
        harness.app.set_no_color(false);
        let field = |harness: &mut Harness, typed: &str| -> (Vec<(Color, bool)>, String) {
            harness.type_text(typed);
            harness.press(KeyCode::Esc);
            assert_eq!(harness.app.composer_text(), typed);
            let frame = harness.draw();
            let cells = (0..80).map(|x| composer_cell(harness, x)).collect();
            (cells, frame)
        };

        let known = format!("{sigil}help");
        let (cells, frame) = field(&mut harness, &known);
        assert!(
            cells[..5].iter().all(|(fg, _)| *fg == Color::Green),
            "`{known}` was not painted green: {cells:?}\n{frame}"
        );
        press_ctrl(&mut harness, 'c');

        let extended = format!("{sigil}helpx");
        let (cells, frame) = field(&mut harness, &extended);
        assert!(
            cells[..6].iter().all(|(fg, _)| *fg == Color::Red),
            "`{extended}` (a known command plus letters) was not painted red: {cells:?}\n{frame}"
        );
        assert!(
            cells.iter().all(|(fg, _)| *fg != Color::Green),
            "`{extended}` was painted green as if `{known}` matched by prefix: {cells:?}\n{frame}"
        );
        press_ctrl(&mut harness, 'c');

        let with_arg = format!("{sigil}help x");
        let (cells, frame) = field(&mut harness, &with_arg);
        assert!(
            cells[..5].iter().all(|(fg, _)| *fg == Color::Green),
            "`{known}` before an argument was not painted green: {cells:?}\n{frame}"
        );
        assert!(
            cells[5..7].iter().all(|(fg, _)| *fg == Color::Reset),
            "the argument after `{known}` is painted, not plain: {cells:?}\n{frame}"
        );
    }
}

/// Under `command_prefix: slash` the empty composer's placeholder points
/// at `/` for commands, not `.`.
#[test]
fn the_placeholder_names_the_configured_command_prefix() {
    for (prefix, sigil, other) in [
        (CommandPrefix::Slash, '/', '.'),
        (CommandPrefix::Dot, '.', '/'),
    ] {
        let frame = harness_under(prefix).draw();
        assert!(
            frame.contains(&format!("Type a prompt or a {sigil}command")),
            "under `{sigil}` the placeholder does not point at `{sigil}` for commands:\n{frame}"
        );
        assert!(
            !frame.contains(&format!("Type a prompt or a {other}command")),
            "under `{sigil}` the placeholder still points at `{other}`:\n{frame}"
        );
    }
}

#[test]
fn dot_keys_lists_ctrl_chords_and_no_function_key_scrolls_and_closes_on_every_close_key() {
    let mut harness = Harness::standalone(100, 30);
    harness.type_text(".keys");
    assert_eq!(harness.press(KeyCode::Enter), None);
    let frame = harness.draw();
    assert!(
        frame.contains(HELP_MARK),
        ".keys did not open the key help:\n{frame}"
    );
    let pages = help_pages(&mut harness);
    assert!(
        pages.len() > 1,
        "the key help fits one 30-row page, so PageDown was never tried:\n{}",
        pages[0]
    );
    assert_ne!(pages[0], pages[1], "PageDown did not scroll the key help");
    let all = pages.join("\n");
    assert!(
        squeezed(&all).contains(&squeezed(LAST_HELP_ROW)),
        "paging never showed the help's last row:\n{all}"
    );
    for chord in [
        "Alt+T", "Ctrl+X", "Ctrl+T", "Ctrl+L", "Ctrl+R", "Ctrl+O", "Ctrl+Z",
    ] {
        assert!(
            all.contains(chord),
            "the key help never lists {chord}:\n{all}"
        );
    }
    for group in [
        "Composer and text field editing",
        "Transcript view find",
        "Thread picker",
    ] {
        assert!(
            squeezed(&all).contains(&squeezed(group)),
            "the key help never shows the {group:?} group:\n{all}"
        );
    }
    if let Some(named) = names_a_function_key(&all) {
        panic!("the key help names the function key {named}:\n{all}");
    }

    assert_eq!(harness.press(KeyCode::Char('q')), None);
    assert_help_closed(&mut harness, "q");

    for (close, by) in [
        (
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            "a second ?",
        ),
        (ctrl(KeyCode::Char('c')), "Ctrl+C"),
        (ctrl(KeyCode::Char('d')), "Ctrl+D"),
    ] {
        harness.press(KeyCode::Char('?'));
        let frame = harness.draw();
        assert!(
            frame.contains(HELP_MARK),
            "? did not reopen the key help:\n{frame}"
        );
        assert_eq!(harness.app.handle_key(close), None, "{by}");
        assert_help_closed(&mut harness, by);
    }

    harness.type_text("still here k2w");
    assert_eq!(
        harness.app.composer_text(),
        "still here k2w",
        "the composer did not take keys after the help"
    );
}

#[test]
fn slash_keys_opens_the_key_help_under_the_slash_prefix() {
    let mut harness = harness_under(CommandPrefix::Slash);
    harness.type_text("/keys");
    assert_eq!(harness.press(KeyCode::Enter), None);
    let frame = harness.draw();
    assert!(
        frame.contains(HELP_MARK),
        "/keys did not open the key help:\n{frame}"
    );
    harness.press(KeyCode::Esc);
    assert_help_closed(&mut harness, "Esc");
}

#[test]
fn question_mark_in_vi_normal_mode_opens_the_key_help() {
    let mut harness = vi_harness();
    assert_eq!(harness.press(KeyCode::Esc), None);
    assert_eq!(harness.app.vi_mode, Some(ViMode::Normal));
    harness.press(KeyCode::Char('?'));
    let frame = harness.draw();
    assert!(
        frame.contains(HELP_MARK),
        "? in vi normal mode did not open the key help:\n{frame}"
    );
    harness.press(KeyCode::Esc);
    assert_help_closed(&mut harness, "Esc");
}

/// `.keys` belongs to the TUI but the command palette does not offer it,
/// while a prefix of a REPL command does open the band on that command.
#[test]
fn the_palette_does_not_offer_dot_keys() {
    let offered = |harness: &Harness| -> Vec<String> {
        palette(harness)
            .map(|palette| {
                palette
                    .rows
                    .iter()
                    .map(|row| row.value.trim_end().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut harness = Harness::standalone(80, 24);
    harness.type_text(".he");
    let rows = offered(&harness);
    assert!(
        rows.iter().any(|row| row == ".help"),
        "the palette does not offer .help for .he: {rows:?}\n{}",
        harness.draw()
    );
    harness.press(KeyCode::Esc);
    press_ctrl(&mut harness, 'c');
    assert_eq!(harness.app.composer_text(), "");

    harness.type_text(".ke");
    let rows = offered(&harness);
    assert!(
        !rows.iter().any(|row| row == ".keys"),
        "the palette offers .keys: {rows:?}\n{}",
        harness.draw()
    );
}

#[test]
fn the_idle_hint_names_the_key_help_and_the_chords() {
    let mut harness = Harness::standalone(80, 24);
    let frame = harness.draw();
    assert!(
        frame.contains(IDLE_STATUS_WITH_HELP),
        "the idle status row lacks `{IDLE_STATUS_WITH_HELP}`:\n{frame}"
    );
    if let Some(named) = names_a_function_key(&frame) {
        panic!("the idle screen names {named}:\n{frame}");
    }

    // `?` on a composer with text types instead of opening the help, and
    // the hint drops its `?` part.
    harness.type_text("ab?");
    assert_eq!(
        harness.app.composer_text(),
        "ab?",
        "`?` after text was not typed"
    );
    let frame = harness.draw();
    assert!(
        !frame.contains(HELP_MARK),
        "`?` on a non-empty composer opened the key help:\n{frame}"
    );
    assert!(
        frame.contains(IDLE_STATUS) && !frame.contains("? = keys"),
        "with text the hint should read `{IDLE_STATUS}` alone:\n{frame}"
    );
}

#[test]
fn dot_keys_with_an_argument_or_a_second_line_does_not_open_the_help() {
    let mut harness = Harness::standalone(80, 24);
    harness.type_text(".keys foo");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit(".keys foo".to_string(), Vec::new()))
    );
    let frame = harness.draw();
    assert!(
        harness.app.help_mut().is_none() && !frame.contains(HELP_MARK),
        "`.keys foo` opened the key help:\n{frame}"
    );

    // Ctrl+J puts the second line under `.keys`.
    harness.type_text(".keys");
    press_ctrl(&mut harness, 'j');
    harness.type_text("zz");
    assert_eq!(harness.app.composer_text(), ".keys\nzz");
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::Submit(".keys\nzz".to_string(), Vec::new()))
    );
    let frame = harness.draw();
    assert!(
        harness.app.help_mut().is_none() && !frame.contains(HELP_MARK),
        "a two-line `.keys` draft opened the key help:\n{frame}"
    );
}

/// Paging through the key help reaches the terminal's own selection and
/// the copy and diagram chords, each with its description.
#[test]
fn the_key_help_lists_the_terminal_selection_and_the_copy_and_diagram_chords() {
    let mut harness = Harness::standalone(80, 24);
    harness.press(KeyCode::Char('?'));
    let frame = harness.draw();
    assert!(
        frame.contains(HELP_MARK),
        "`?` did not open the key help:\n{frame}"
    );
    let help = squeezed(&help_pages(&mut harness).join("\n"));
    for (chord, row) in [
        (
            "Shift+drag",
            "Shift+drag / Option+drag Select with the terminal instead",
        ),
        ("Alt+C", "Alt+C Copy the focused tool call"),
        ("Alt+M", "Alt+M Show diagrams, charts and math as"),
    ] {
        assert!(
            help.contains(&squeezed(row)),
            "the key help does not list {chord} as {row:?}:\n{help}"
        );
    }
    harness.press(KeyCode::Esc);
    let frame = harness.draw();
    assert!(
        frame.contains(IDLE_STATUS_WITH_HELP),
        "Esc did not close the key help:\n{frame}"
    );
}
