//! The key table: one pure function from a key press and the UI's state to
//! the action the app takes. Nothing here touches the terminal, so every
//! binding is pinned by a plain table test.

use crate::prompt::PromptAnswer;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The prompt modal currently replacing the composer, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalKind {
    /// A yes/no question; `default` is what Enter answers and `tool` says
    /// whether "yes for this session" is on offer.
    Confirm {
        default: bool,
        tool: bool,
    },
    Input,
    /// One row per option plus, unless the request is closed, an "Other
    /// (custom)" row; choosing that row opens a text field in the list's
    /// place.
    Select {
        custom_field_open: bool,
    },
    Checkbox,
    Password,
}

/// What is open over the composer, if anything; the list keys and Esc
/// go to it before anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PopupKind {
    /// Typing goes on into the composer under it.
    Palette,
    /// Typing goes into the band's filter line.
    HistorySearch,
    Rewind,
    /// The running tasks and the questions waiting on the user; letters
    /// act on the highlighted row and never reach the composer.
    Roster,
}

/// What the full-screen pager is taking keys for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PagerMode {
    Browse,
    /// Typing goes into the find field.
    Find,
    /// The thread picker is open over the text.
    Picker,
}

/// The vi layer's mode; the composer under emacs bindings has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViMode {
    Insert,
    Normal,
}

/// A normal-mode cursor motion, named after the key that asks for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViMotion {
    Back,
    Down,
    Up,
    Forward,
    WordForward,
    WordBack,
    Head,
    End,
}

/// What the key table needs to know about the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyContext {
    pub(crate) turn_running: bool,
    pub(crate) composer_empty: bool,
    /// Whether the composer holds one line (an empty one counts): only
    /// then do the plain arrows walk the history instead of the draft.
    pub(crate) single_line: bool,
    pub(crate) modal: Option<ModalKind>,
    /// Whether a second has passed since the last key pressed (accepted
    /// or ignored, outside a modal or under one) or since the previous
    /// answer: the condition for a bare hotkey, or Enter on a list or
    /// confirm, to count as an answer, so a modal that appears
    /// mid-sentence cannot be answered by the keystroke already in
    /// flight. Keys that only move a list's cursor or type into a field
    /// are not gated; the letter and digit shortcuts of a list are, like
    /// a confirm's hotkeys.
    pub(crate) idle_ok: bool,
    /// Whether the front modal has been up for a second, counted from
    /// its appearance, the previous answer or its custom field opening
    /// and not from the last key: the condition for Enter to send a text
    /// field. Typing the answer is expected under a field, so it must
    /// not postpone sending it the way it postpones a hotkey.
    pub(crate) field_settled: bool,
    /// Whether the whole question is on screen. A question taller than
    /// its pane is clipped with a marker, and nothing clipped may be
    /// approved: only Esc and Ctrl+C answer until the terminal is tall
    /// enough to show all of it.
    pub(crate) question_fully_shown: bool,
    /// Whether the transcript is scrolled away from its bottom; only
    /// then do End, and Shift+G on an empty composer, leave the
    /// composer to jump the view back.
    pub(crate) scrolled: bool,
    /// Whether a transcript row holds the focus; an Enter on an empty
    /// composer goes to it, and so does Esc while no turn is running.
    pub(crate) focused: bool,
    pub(crate) popup: Option<PopupKind>,
    pub(crate) vi: Option<ViMode>,
    /// The pager covers the whole screen and takes every key but the
    /// terminal's own.
    pub(crate) pager: Option<PagerMode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Send the composer text as the next line.
    Submit,
    /// Hold the composer text for the running turn: a prompt reaches the
    /// model at its next tool round, a command waits for the turn to end.
    QueueInput,
    /// Take the newest held line back into the composer.
    EditQueued,
    InsertNewline,
    ClearComposer,
    /// Soft-interrupt the running turn.
    Interrupt,
    /// An Esc with nothing to cancel on an empty composer; two of them
    /// close together open the rewind picker.
    IdleEsc,
    Quit,
    Suspend,
    OpenEditor,
    /// Repaint from scratch.
    ScheduleFrame,
    Answer(PromptAnswer),
    /// Answer yes and remember it for the tool's later identical prompts.
    AnswerYesForSession,
    /// Send the modal's text field as the answer.
    SubmitModalInput,
    /// Send the list modal's current row (select) or ticked rows (checkbox).
    SubmitModalChoice,
    /// Move the list modal's cursor by this many rows, down when positive.
    ModalMove(i32),
    /// Tick or untick the checkbox row under the cursor.
    ModalToggle,
    /// Choose the select row at this index outright.
    ModalJump(usize),
    ModalInput(KeyEvent),
    ComposerInput(KeyEvent),
    /// Show the next older history entry in the composer.
    HistoryPrev,
    /// Show the next newer history entry, or the draft past the newest.
    HistoryNext,
    OpenHistorySearch,
    /// Move the popup's highlight by this many rows, down when positive.
    PaletteMove(i32),
    /// Take the popup's highlighted row into the composer; with no row
    /// highlighted the Enter goes on to submit.
    PaletteAccept,
    ClosePopup,
    /// Type into the popup's filter line.
    PopupChar(char),
    PopupBackspace,
    ViNormal,
    /// Enter insert mode, after the motion if one is given.
    ViInsert {
        append: Option<ViMotion>,
    },
    ViMotion(ViMotion),
    /// Delete the character under the cursor.
    ViDeleteChar,
    /// The `d` of `dd`: the second press deletes the cursor's line.
    ViDeleteLine,
    ToggleTodo,
    ToggleRoster,
    /// Cancel the task on the roster's highlighted row.
    RosterCancel,
    /// Show the highlighted roster row's transcript in the pager.
    RosterOpen,
    OpenPager,
    ClosePager,
    /// Move the pager's text by this many lines, down when positive.
    PagerScroll(i32),
    /// Move the pager's text by this many pages, down when positive.
    PagerPage(i32),
    PagerTop,
    PagerBottom,
    PagerFindOpen,
    PagerFindChar(char),
    PagerFindBackspace,
    /// Jump to the next match of the find text, or the previous one.
    PagerFindNext {
        forward: bool,
    },
    /// Search for the find field's text and close the field, keeping the
    /// text for `n` and `N`.
    PagerFindSubmit,
    PagerFindCancel,
    OpenThreadPicker,
    /// Move the thread picker's highlight by this many rows.
    ThreadPickerMove(i32),
    ThreadPickerChoose,
    CloseThreadPicker,
    /// Move the transcript view by this many lines, down when positive.
    ScrollLines(i32),
    /// Move the transcript view by this many pages, down when positive.
    ScrollPage(i32),
    ScrollToBottom,
    FocusPrev,
    FocusNext,
    /// Open or close the focused transcript row.
    ToggleFocused,
    ClearFocus,
    Nothing,
}

pub(crate) fn map_key(key: KeyEvent, ctx: &KeyContext) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // A chord's letter is matched case-folded once here: with the kitty
    // protocol Ctrl+Shift+C arrives as an uppercase `C`.
    let code = lowercase(key.code);
    // Job control and repaint are the terminal's, not the modal's: they
    // work the same whatever is on screen. So do the keys that only move
    // the view: a question waiting below does not stop the user reading
    // what came before it. Keys that act on a row stay with the modal.
    match code {
        KeyCode::Char('z') if ctrl => return Action::Suspend,
        KeyCode::Char('l') if ctrl => return Action::ScheduleFrame,
        _ => {}
    }
    if let Some(mode) = ctx.pager {
        return pager_key(key, code, mode);
    }
    match code {
        KeyCode::F(2) => return Action::ToggleTodo,
        KeyCode::PageUp => return Action::ScrollPage(-1),
        KeyCode::PageDown => return Action::ScrollPage(1),
        KeyCode::Up if shift => return Action::ScrollLines(-1),
        KeyCode::Down if shift => return Action::ScrollLines(1),
        KeyCode::End if ctx.scrolled => return Action::ScrollToBottom,
        _ => {}
    }
    match ctx.modal {
        Some(ModalKind::Confirm { default, tool }) => {
            let answerable = ctx.idle_ok && ctx.question_fully_shown;
            match code {
                KeyCode::Esc => Action::Answer(PromptAnswer::Cancelled),
                KeyCode::Char('c') if ctrl => Action::Answer(PromptAnswer::Cancelled),
                _ if !answerable => Action::Nothing,
                KeyCode::Enter => Action::Answer(PromptAnswer::Confirmed(default)),
                KeyCode::Char('y') if !ctrl => Action::Answer(PromptAnswer::Confirmed(true)),
                KeyCode::Char('n') if !ctrl => Action::Answer(PromptAnswer::Confirmed(false)),
                KeyCode::Char('a') if !ctrl && tool => Action::AnswerYesForSession,
                _ => Action::Nothing,
            }
        }
        Some(
            ModalKind::Input
            | ModalKind::Password
            | ModalKind::Select {
                custom_field_open: true,
            },
        ) => match code {
            KeyCode::Esc => Action::Answer(PromptAnswer::Cancelled),
            KeyCode::Char('c') if ctrl => Action::Answer(PromptAnswer::Cancelled),
            // The field is one line, so the composer's newline chords have
            // nothing to insert: Enter submits however it is held, once
            // the modal has settled; a gated Enter is swallowed rather
            // than typed.
            KeyCode::Enter if !(ctx.field_settled && ctx.question_fully_shown) => Action::Nothing,
            KeyCode::Enter => Action::SubmitModalInput,
            KeyCode::Char('j') if ctrl => Action::Nothing,
            _ => Action::ModalInput(key),
        },
        Some(
            ModalKind::Select {
                custom_field_open: false,
            }
            | ModalKind::Checkbox,
        ) => {
            let answerable = ctx.idle_ok && ctx.question_fully_shown;
            let checkbox = ctx.modal == Some(ModalKind::Checkbox);
            match code {
                KeyCode::Esc => Action::Answer(PromptAnswer::Cancelled),
                KeyCode::Char('c') if ctrl => Action::Answer(PromptAnswer::Cancelled),
                KeyCode::Up => Action::ModalMove(-1),
                KeyCode::Down => Action::ModalMove(1),
                _ if !answerable => Action::Nothing,
                KeyCode::Char(' ') if checkbox => Action::ModalToggle,
                KeyCode::Enter => Action::SubmitModalChoice,
                KeyCode::Char('k') if !ctrl => Action::ModalMove(-1),
                KeyCode::Char('j') if !ctrl => Action::ModalMove(1),
                KeyCode::Char(digit @ '1'..='9') if !ctrl && !checkbox => {
                    Action::ModalJump(usize::from(digit as u8 - b'1'))
                }
                _ => Action::Nothing,
            }
        }
        None => match code {
            KeyCode::F(3) => Action::ToggleRoster,
            KeyCode::Char('t') if ctrl => Action::OpenPager,
            // A band is the innermost thing open, so the quit chords
            // close it first, as Esc does.
            KeyCode::Char('c' | 'd') if ctrl && ctx.popup.is_some() => Action::ClosePopup,
            KeyCode::Char('c') if ctrl => {
                if ctx.turn_running {
                    Action::Interrupt
                } else if ctx.composer_empty {
                    Action::Quit
                } else {
                    Action::ClearComposer
                }
            }
            // Readline's split: EOF only on an empty line; with a draft the
            // chord reaches the textarea, which deletes forward.
            KeyCode::Char('d') if ctrl && ctx.composer_empty => Action::Quit,
            KeyCode::Up if ctrl => Action::FocusPrev,
            KeyCode::Down if ctrl => Action::FocusNext,
            KeyCode::Up if alt => Action::EditQueued,
            KeyCode::Esc if ctx.popup.is_some() => Action::ClosePopup,
            KeyCode::Up if ctx.popup.is_some() => Action::PaletteMove(-1),
            KeyCode::Down if ctx.popup.is_some() => Action::PaletteMove(1),
            KeyCode::Tab if ctx.popup.is_some() => Action::PaletteMove(1),
            KeyCode::BackTab if ctx.popup.is_some() => Action::PaletteMove(-1),
            // The bands that are not the palette have no draft under
            // them to take a newline: Enter accepts however it is held.
            KeyCode::Enter
                if matches!(
                    ctx.popup,
                    Some(PopupKind::HistorySearch | PopupKind::Rewind | PopupKind::Roster)
                ) =>
            {
                Action::PaletteAccept
            }
            KeyCode::Char(c) if !ctrl && ctx.popup == Some(PopupKind::Roster) => match c {
                'j' => Action::PaletteMove(1),
                'k' => Action::PaletteMove(-1),
                'x' => Action::RosterCancel,
                'o' => Action::RosterOpen,
                _ => Action::Nothing,
            },
            // The draft is out of sight under the roster: no key edits it.
            _ if ctx.popup == Some(PopupKind::Roster) => Action::Nothing,
            KeyCode::Char(_) if !ctrl && ctx.popup == Some(PopupKind::HistorySearch) => {
                match key.code {
                    KeyCode::Char(typed) => Action::PopupChar(typed),
                    _ => Action::Nothing,
                }
            }
            KeyCode::Backspace if ctx.popup == Some(PopupKind::HistorySearch) => {
                Action::PopupBackspace
            }
            // A draft keeps the letter: `G` mid-sentence is typing, and a
            // band that takes letters above gets it as a filter character.
            KeyCode::Char('g') if shift && !ctrl && ctx.scrolled && ctx.composer_empty => {
                Action::ScrollToBottom
            }
            // Interrupting the turn outranks a focused row; the focus
            // goes with it.
            KeyCode::Esc if ctx.turn_running => Action::Interrupt,
            KeyCode::Esc if ctx.focused => Action::ClearFocus,
            KeyCode::Esc if ctx.vi == Some(ViMode::Insert) => Action::ViNormal,
            KeyCode::Esc if ctx.composer_empty => Action::IdleEsc,
            KeyCode::Esc => Action::Nothing,
            // The editor would inherit fds 1 and 2 on the turn's capture pipe.
            KeyCode::Char('o') if ctrl && ctx.turn_running => Action::Nothing,
            KeyCode::Char('o') if ctrl => Action::OpenEditor,
            KeyCode::Char('r') if ctrl => Action::OpenHistorySearch,
            KeyCode::Char('j') if ctrl => Action::InsertNewline,
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                Action::InsertNewline
            }
            KeyCode::Enter if ctx.popup.is_some() => Action::PaletteAccept,
            KeyCode::Enter if ctx.focused && ctx.composer_empty => Action::ToggleFocused,
            KeyCode::Enter if ctx.turn_running => Action::QueueInput,
            KeyCode::Enter => Action::Submit,
            KeyCode::Up if ctx.single_line && key.modifiers.is_empty() => Action::HistoryPrev,
            KeyCode::Down if ctx.single_line && key.modifiers.is_empty() => Action::HistoryNext,
            KeyCode::Char(_) if !ctrl && ctx.vi == Some(ViMode::Normal) => vi_normal(key.code),
            _ => Action::ComposerInput(key),
        },
    }
}

/// Normal mode's letters, matched with their case: `a` and `A` differ.
/// A letter with no binding is swallowed, not typed.
fn vi_normal(code: KeyCode) -> Action {
    match code {
        KeyCode::Char('h') => Action::ViMotion(ViMotion::Back),
        KeyCode::Char('j') => Action::ViMotion(ViMotion::Down),
        KeyCode::Char('k') => Action::ViMotion(ViMotion::Up),
        KeyCode::Char('l') => Action::ViMotion(ViMotion::Forward),
        KeyCode::Char('w') => Action::ViMotion(ViMotion::WordForward),
        KeyCode::Char('b') => Action::ViMotion(ViMotion::WordBack),
        KeyCode::Char('0') => Action::ViMotion(ViMotion::Head),
        KeyCode::Char('$') => Action::ViMotion(ViMotion::End),
        KeyCode::Char('x') => Action::ViDeleteChar,
        KeyCode::Char('d') => Action::ViDeleteLine,
        KeyCode::Char('i') => Action::ViInsert { append: None },
        KeyCode::Char('a') => Action::ViInsert {
            append: Some(ViMotion::Forward),
        },
        KeyCode::Char('A') => Action::ViInsert {
            append: Some(ViMotion::End),
        },
        _ => Action::Nothing,
    }
}

/// The pager's keys: less's motions over the text, a find field and
/// the thread picker. `code` is the case-folded key; `G` and `N` are
/// told from `g` and `n` by the key as typed or by Shift.
fn pager_key(key: KeyEvent, code: KeyCode, mode: PagerMode) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let upper = key.modifiers.contains(KeyModifiers::SHIFT)
        || matches!(key.code, KeyCode::Char(c) if c.is_ascii_uppercase());
    match mode {
        PagerMode::Find => match code {
            KeyCode::Esc => Action::PagerFindCancel,
            KeyCode::Char('c') if ctrl => Action::PagerFindCancel,
            KeyCode::Enter => Action::PagerFindSubmit,
            KeyCode::Backspace => Action::PagerFindBackspace,
            KeyCode::Char(_) if !ctrl => match key.code {
                KeyCode::Char(typed) => Action::PagerFindChar(typed),
                _ => Action::Nothing,
            },
            _ => Action::Nothing,
        },
        PagerMode::Picker => match code {
            KeyCode::Esc | KeyCode::Tab => Action::CloseThreadPicker,
            KeyCode::Char('c' | 'd') if ctrl => Action::CloseThreadPicker,
            KeyCode::Char('q') if !ctrl => Action::CloseThreadPicker,
            KeyCode::Up => Action::ThreadPickerMove(-1),
            KeyCode::Down => Action::ThreadPickerMove(1),
            KeyCode::Char('k') if !ctrl => Action::ThreadPickerMove(-1),
            KeyCode::Char('j') if !ctrl => Action::ThreadPickerMove(1),
            KeyCode::Enter => Action::ThreadPickerChoose,
            _ => Action::Nothing,
        },
        PagerMode::Browse => match code {
            KeyCode::Esc => Action::ClosePager,
            KeyCode::Char('c' | 'd' | 't') if ctrl => Action::ClosePager,
            _ if ctrl => Action::Nothing,
            KeyCode::Char('q') => Action::ClosePager,
            KeyCode::Down | KeyCode::Char('j') => Action::PagerScroll(1),
            KeyCode::Up | KeyCode::Char('k') => Action::PagerScroll(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => Action::PagerPage(1),
            KeyCode::PageUp => Action::PagerPage(-1),
            KeyCode::Home => Action::PagerTop,
            KeyCode::End => Action::PagerBottom,
            KeyCode::Char('g') if upper => Action::PagerBottom,
            KeyCode::Char('g') => Action::PagerTop,
            KeyCode::Char('/') => Action::PagerFindOpen,
            KeyCode::Char('n') => Action::PagerFindNext { forward: !upper },
            KeyCode::Tab => Action::OpenThreadPicker,
            _ => Action::Nothing,
        },
    }
}

/// A hotkey answers the same with Shift or caps lock held.
fn lowercase(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn ctrl(c: char) -> KeyEvent {
        press(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn plain(code: KeyCode) -> KeyEvent {
        press(code, KeyModifiers::NONE)
    }

    const IDLE: KeyContext = KeyContext {
        turn_running: false,
        composer_empty: true,
        single_line: true,
        modal: None,
        idle_ok: true,
        field_settled: true,
        question_fully_shown: true,
        scrolled: false,
        focused: false,
        popup: None,
        vi: None,
        pager: None,
    };

    #[test]
    fn interrupt_and_quit_follow_the_turn_and_composer_state() {
        let typing = KeyContext {
            composer_empty: false,
            ..IDLE
        };
        let running = KeyContext {
            turn_running: true,
            ..IDLE
        };
        let running_vi_insert = KeyContext {
            vi: Some(ViMode::Insert),
            ..running
        };
        let running_with = |popup: PopupKind| KeyContext {
            popup: Some(popup),
            ..running
        };
        let under = |modal: ModalKind| KeyContext {
            modal: Some(modal),
            ..running
        };
        let cancelled = Action::Answer(PromptAnswer::Cancelled);
        let cases = [
            (ctrl('c'), IDLE, Action::Quit),
            (ctrl('c'), typing, Action::ClearComposer),
            (ctrl('c'), running, Action::Interrupt),
            (ctrl('d'), IDLE, Action::Quit),
            (ctrl('d'), typing, Action::ComposerInput(ctrl('d'))),
            (ctrl('d'), running, Action::Quit),
            (plain(KeyCode::Esc), IDLE, Action::IdleEsc),
            (plain(KeyCode::Esc), typing, Action::Nothing),
            (plain(KeyCode::Esc), running, Action::Interrupt),
            (plain(KeyCode::Esc), running_vi_insert, Action::Interrupt),
            (
                plain(KeyCode::Esc),
                running_with(PopupKind::Palette),
                Action::ClosePopup,
            ),
            (
                plain(KeyCode::Esc),
                running_with(PopupKind::Rewind),
                Action::ClosePopup,
            ),
            (
                plain(KeyCode::Esc),
                running_with(PopupKind::HistorySearch),
                Action::ClosePopup,
            ),
            (
                plain(KeyCode::Esc),
                under(ModalKind::Confirm {
                    default: true,
                    tool: true,
                }),
                cancelled.clone(),
            ),
            (
                plain(KeyCode::Esc),
                under(ModalKind::Input),
                cancelled.clone(),
            ),
            (
                plain(KeyCode::Esc),
                under(ModalKind::Select {
                    custom_field_open: false,
                }),
                cancelled.clone(),
            ),
            (
                plain(KeyCode::Esc),
                under(ModalKind::Select {
                    custom_field_open: true,
                }),
                cancelled.clone(),
            ),
            (
                plain(KeyCode::Esc),
                under(ModalKind::Checkbox),
                cancelled.clone(),
            ),
            (plain(KeyCode::Esc), under(ModalKind::Password), cancelled),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// Under the kitty protocol a chord held with Shift arrives with an
    /// uppercase letter; the chord still matches, in every mode.
    #[test]
    fn chords_match_with_shift_held() {
        let shifted = |c: char| {
            press(
                KeyCode::Char(c),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )
        };
        let running = KeyContext {
            turn_running: true,
            ..IDLE
        };
        assert_eq!(map_key(shifted('C'), &running), Action::Interrupt);
        assert_eq!(map_key(shifted('D'), &IDLE), Action::Quit);
        assert_eq!(map_key(shifted('L'), &IDLE), Action::ScheduleFrame);
        assert_eq!(map_key(shifted('Z'), &IDLE), Action::Suspend);
        assert_eq!(map_key(shifted('J'), &IDLE), Action::InsertNewline);
        assert_eq!(map_key(shifted('O'), &IDLE), Action::OpenEditor);
    }

    #[test]
    fn composer_keys() {
        let running = KeyContext {
            turn_running: true,
            ..IDLE
        };
        let cases = [
            (plain(KeyCode::Enter), IDLE, Action::Submit),
            (plain(KeyCode::Enter), running, Action::QueueInput),
            (
                press(KeyCode::Enter, KeyModifiers::SHIFT),
                IDLE,
                Action::InsertNewline,
            ),
            (
                press(KeyCode::Enter, KeyModifiers::ALT),
                IDLE,
                Action::InsertNewline,
            ),
            (ctrl('j'), IDLE, Action::InsertNewline),
            (ctrl('z'), IDLE, Action::Suspend),
            (ctrl('o'), IDLE, Action::OpenEditor),
            (ctrl('o'), running, Action::Nothing),
            (ctrl('l'), IDLE, Action::ScheduleFrame),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
        let typed = plain(KeyCode::Char('x'));
        assert_eq!(map_key(typed, &IDLE), Action::ComposerInput(typed));
        assert_eq!(map_key(typed, &running), Action::ComposerInput(typed));
    }

    /// Enter during a turn holds the line; a modal or a popup up at the
    /// time keeps Enter for itself. Alt+Up takes a held line back whether
    /// or not a turn runs.
    #[test]
    fn queued_input_keys() {
        let running = KeyContext {
            turn_running: true,
            composer_empty: false,
            ..IDLE
        };
        let alt_up = press(KeyCode::Up, KeyModifiers::ALT);
        let cases = [
            (plain(KeyCode::Enter), running, Action::QueueInput),
            (alt_up, IDLE, Action::EditQueued),
            (alt_up, running, Action::EditQueued),
            (
                plain(KeyCode::Enter),
                KeyContext {
                    popup: Some(PopupKind::Palette),
                    ..running
                },
                Action::PaletteAccept,
            ),
            (
                plain(KeyCode::Enter),
                KeyContext {
                    modal: Some(ModalKind::Confirm {
                        default: false,
                        tool: false,
                    }),
                    ..running
                },
                Action::Answer(PromptAnswer::Confirmed(false)),
            ),
            (
                plain(KeyCode::Enter),
                KeyContext {
                    modal: Some(ModalKind::Input),
                    ..running
                },
                Action::SubmitModalInput,
            ),
            (plain(KeyCode::Up), running, Action::HistoryPrev),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// With the palette open the list keys and Esc go to it, whatever
    /// the turn or the focus is doing, and the quit chords close it
    /// before they can quit or interrupt; typing still reaches the
    /// composer and the view keys still move the transcript.
    #[test]
    fn popup_keys() {
        let popup = KeyContext {
            popup: Some(PopupKind::Palette),
            composer_empty: false,
            ..IDLE
        };
        let running = KeyContext {
            turn_running: true,
            ..popup
        };
        let focused = KeyContext {
            focused: true,
            ..popup
        };
        let cases = [
            (plain(KeyCode::Esc), popup, Action::ClosePopup),
            (plain(KeyCode::Esc), running, Action::ClosePopup),
            (plain(KeyCode::Esc), focused, Action::ClosePopup),
            (ctrl('c'), popup, Action::ClosePopup),
            (ctrl('c'), running, Action::ClosePopup),
            (ctrl('d'), popup, Action::ClosePopup),
            (plain(KeyCode::Up), popup, Action::PaletteMove(-1)),
            (plain(KeyCode::Down), popup, Action::PaletteMove(1)),
            (plain(KeyCode::Tab), popup, Action::PaletteMove(1)),
            (plain(KeyCode::BackTab), popup, Action::PaletteMove(-1)),
            (plain(KeyCode::Enter), popup, Action::PaletteAccept),
            (plain(KeyCode::Enter), running, Action::PaletteAccept),
            (
                press(KeyCode::Enter, KeyModifiers::SHIFT),
                popup,
                Action::InsertNewline,
            ),
            (
                press(KeyCode::Up, KeyModifiers::SHIFT),
                popup,
                Action::ScrollLines(-1),
            ),
            (
                press(KeyCode::Up, KeyModifiers::CONTROL),
                popup,
                Action::FocusPrev,
            ),
            (
                plain(KeyCode::Tab),
                IDLE,
                Action::ComposerInput(plain(KeyCode::Tab)),
            ),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
        let typed = plain(KeyCode::Char('x'));
        assert_eq!(map_key(typed, &popup), Action::ComposerInput(typed));
    }

    /// The plain arrows walk the history only on a one-line composer;
    /// a multi-line draft keeps them for its own cursor, and a modifier
    /// (Alt+Up edits the queue) keeps them too.
    #[test]
    fn single_line_arrows_walk_the_history() {
        let multi_line = KeyContext {
            single_line: false,
            composer_empty: false,
            ..IDLE
        };
        let running = KeyContext {
            turn_running: true,
            ..IDLE
        };
        let typing = KeyContext {
            composer_empty: false,
            ..IDLE
        };
        let cases = [
            (plain(KeyCode::Up), IDLE, Action::HistoryPrev),
            (plain(KeyCode::Down), IDLE, Action::HistoryNext),
            (plain(KeyCode::Up), typing, Action::HistoryPrev),
            (plain(KeyCode::Up), running, Action::HistoryPrev),
            (
                plain(KeyCode::Up),
                multi_line,
                Action::ComposerInput(plain(KeyCode::Up)),
            ),
            (
                plain(KeyCode::Down),
                multi_line,
                Action::ComposerInput(plain(KeyCode::Down)),
            ),
            (
                press(KeyCode::Up, KeyModifiers::ALT),
                IDLE,
                Action::EditQueued,
            ),
            (ctrl('r'), IDLE, Action::OpenHistorySearch),
            (ctrl('r'), running, Action::OpenHistorySearch),
            (ctrl('r'), multi_line, Action::OpenHistorySearch),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// With the search band open the letters and Backspace edit its
    /// filter and Enter takes the highlighted entry however it is held;
    /// the rewind picker types into the composer instead.
    #[test]
    fn history_search_and_rewind_popup_keys() {
        let search = KeyContext {
            popup: Some(PopupKind::HistorySearch),
            ..IDLE
        };
        let scrolled_search = KeyContext {
            scrolled: true,
            ..search
        };
        let rewind = KeyContext {
            popup: Some(PopupKind::Rewind),
            ..IDLE
        };
        let shifted_g = press(KeyCode::Char('G'), KeyModifiers::SHIFT);
        let cases = [
            (plain(KeyCode::Char('g')), search, Action::PopupChar('g')),
            (shifted_g, search, Action::PopupChar('G')),
            (shifted_g, scrolled_search, Action::PopupChar('G')),
            (plain(KeyCode::Char(' ')), search, Action::PopupChar(' ')),
            (plain(KeyCode::Backspace), search, Action::PopupBackspace),
            (plain(KeyCode::Enter), search, Action::PaletteAccept),
            (
                press(KeyCode::Enter, KeyModifiers::SHIFT),
                search,
                Action::PaletteAccept,
            ),
            (plain(KeyCode::Up), search, Action::PaletteMove(-1)),
            (plain(KeyCode::Down), search, Action::PaletteMove(1)),
            (plain(KeyCode::Esc), search, Action::ClosePopup),
            (ctrl('r'), search, Action::OpenHistorySearch),
            (ctrl('c'), search, Action::ClosePopup),
            (ctrl('d'), search, Action::ClosePopup),
            (plain(KeyCode::Enter), rewind, Action::PaletteAccept),
            (plain(KeyCode::Up), rewind, Action::PaletteMove(-1)),
            (plain(KeyCode::Down), rewind, Action::PaletteMove(1)),
            (plain(KeyCode::Esc), rewind, Action::ClosePopup),
            (ctrl('c'), rewind, Action::ClosePopup),
            (
                plain(KeyCode::Char('g')),
                rewind,
                Action::ComposerInput(plain(KeyCode::Char('g'))),
            ),
            (
                plain(KeyCode::Backspace),
                rewind,
                Action::ComposerInput(plain(KeyCode::Backspace)),
            ),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// Insert mode is the emacs table but for Esc, which enters normal
    /// mode when nothing outranks it: a popup, a focused row and a
    /// running turn each still take the Esc first.
    #[test]
    fn vi_insert_keys() {
        let insert = KeyContext {
            vi: Some(ViMode::Insert),
            composer_empty: false,
            ..IDLE
        };
        let empty = KeyContext {
            composer_empty: true,
            ..insert
        };
        let running = KeyContext {
            turn_running: true,
            ..insert
        };
        let focused = KeyContext {
            focused: true,
            ..insert
        };
        let popup = KeyContext {
            popup: Some(PopupKind::Palette),
            ..insert
        };
        let typed = plain(KeyCode::Char('x'));
        let cases = [
            (plain(KeyCode::Esc), insert, Action::ViNormal),
            (plain(KeyCode::Esc), empty, Action::ViNormal),
            (plain(KeyCode::Esc), running, Action::Interrupt),
            (plain(KeyCode::Esc), focused, Action::ClearFocus),
            (plain(KeyCode::Esc), popup, Action::ClosePopup),
            (typed, insert, Action::ComposerInput(typed)),
            (
                plain(KeyCode::Char('j')),
                insert,
                Action::ComposerInput(plain(KeyCode::Char('j'))),
            ),
            (plain(KeyCode::Enter), insert, Action::Submit),
            (plain(KeyCode::Up), insert, Action::HistoryPrev),
            (ctrl('c'), insert, Action::ClearComposer),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// Normal mode's letters move, delete and re-enter insert; the rest
    /// are swallowed. Enter submits as in insert, Esc keeps the usual
    /// meanings, and the chords stay the chords.
    #[test]
    fn vi_normal_keys() {
        let normal = KeyContext {
            vi: Some(ViMode::Normal),
            composer_empty: false,
            ..IDLE
        };
        let empty = KeyContext {
            composer_empty: true,
            ..normal
        };
        let running = KeyContext {
            turn_running: true,
            ..normal
        };
        let letter = |c: char| plain(KeyCode::Char(c));
        let cases = [
            (letter('h'), normal, Action::ViMotion(ViMotion::Back)),
            (letter('j'), normal, Action::ViMotion(ViMotion::Down)),
            (letter('k'), normal, Action::ViMotion(ViMotion::Up)),
            (letter('l'), normal, Action::ViMotion(ViMotion::Forward)),
            (letter('w'), normal, Action::ViMotion(ViMotion::WordForward)),
            (letter('b'), normal, Action::ViMotion(ViMotion::WordBack)),
            (letter('0'), normal, Action::ViMotion(ViMotion::Head)),
            (letter('$'), normal, Action::ViMotion(ViMotion::End)),
            (letter('x'), normal, Action::ViDeleteChar),
            (letter('d'), normal, Action::ViDeleteLine),
            (letter('i'), normal, Action::ViInsert { append: None }),
            (
                letter('a'),
                normal,
                Action::ViInsert {
                    append: Some(ViMotion::Forward),
                },
            ),
            (
                press(KeyCode::Char('A'), KeyModifiers::SHIFT),
                normal,
                Action::ViInsert {
                    append: Some(ViMotion::End),
                },
            ),
            (letter('q'), normal, Action::Nothing),
            (letter('H'), normal, Action::Nothing),
            (plain(KeyCode::Enter), normal, Action::Submit),
            (plain(KeyCode::Enter), running, Action::QueueInput),
            (plain(KeyCode::Esc), normal, Action::Nothing),
            (plain(KeyCode::Esc), empty, Action::IdleEsc),
            (plain(KeyCode::Esc), running, Action::Interrupt),
            (ctrl('c'), normal, Action::ClearComposer),
            (ctrl('j'), normal, Action::InsertNewline),
            (ctrl('r'), normal, Action::OpenHistorySearch),
            (plain(KeyCode::Up), normal, Action::HistoryPrev),
            (
                plain(KeyCode::Backspace),
                normal,
                Action::ComposerInput(plain(KeyCode::Backspace)),
            ),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// The transcript's keys: F2, the page and Shift+arrow scrolls and
    /// the Ctrl+arrow focus walk work whatever the turn is doing; End
    /// goes to the view only while it is scrolled, Shift+G only while
    /// it is scrolled and the composer is empty; Enter goes to a
    /// focused row while the composer is empty, Esc while no turn runs.
    /// The plain arrows of a multi-line draft and Tab stay the
    /// composer's.
    #[test]
    fn transcript_keys() {
        let running = KeyContext {
            turn_running: true,
            ..IDLE
        };
        let scrolled = KeyContext {
            scrolled: true,
            ..IDLE
        };
        let scrolled_typing = KeyContext {
            composer_empty: false,
            ..scrolled
        };
        let focused = KeyContext {
            focused: true,
            ..IDLE
        };
        let focused_running = KeyContext {
            turn_running: true,
            ..focused
        };
        let focused_typing = KeyContext {
            composer_empty: false,
            ..focused
        };
        let focused_multi_line = KeyContext {
            single_line: false,
            ..focused_typing
        };
        let scrolled_multi_line = KeyContext {
            single_line: false,
            composer_empty: false,
            ..scrolled
        };
        let shift = |code: KeyCode| press(code, KeyModifiers::SHIFT);
        let control = |code: KeyCode| press(code, KeyModifiers::CONTROL);
        let cases = [
            (plain(KeyCode::F(2)), IDLE, Action::ToggleTodo),
            (plain(KeyCode::F(2)), running, Action::ToggleTodo),
            (plain(KeyCode::PageUp), IDLE, Action::ScrollPage(-1)),
            (plain(KeyCode::PageDown), running, Action::ScrollPage(1)),
            (shift(KeyCode::Up), IDLE, Action::ScrollLines(-1)),
            (shift(KeyCode::Down), running, Action::ScrollLines(1)),
            (control(KeyCode::Up), IDLE, Action::FocusPrev),
            (control(KeyCode::Down), running, Action::FocusNext),
            (plain(KeyCode::End), scrolled, Action::ScrollToBottom),
            (
                plain(KeyCode::End),
                IDLE,
                Action::ComposerInput(plain(KeyCode::End)),
            ),
            (shift(KeyCode::Char('G')), scrolled, Action::ScrollToBottom),
            (
                shift(KeyCode::Char('G')),
                scrolled_typing,
                Action::ComposerInput(shift(KeyCode::Char('G'))),
            ),
            (
                shift(KeyCode::Char('G')),
                IDLE,
                Action::ComposerInput(shift(KeyCode::Char('G'))),
            ),
            (plain(KeyCode::Esc), focused, Action::ClearFocus),
            (plain(KeyCode::Esc), focused_running, Action::Interrupt),
            (plain(KeyCode::Enter), focused, Action::ToggleFocused),
            (
                plain(KeyCode::Enter),
                focused_running,
                Action::ToggleFocused,
            ),
            (plain(KeyCode::Enter), focused_typing, Action::Submit),
            (shift(KeyCode::Enter), focused, Action::InsertNewline),
            (
                plain(KeyCode::Up),
                focused_multi_line,
                Action::ComposerInput(plain(KeyCode::Up)),
            ),
            (
                plain(KeyCode::Down),
                scrolled_multi_line,
                Action::ComposerInput(plain(KeyCode::Down)),
            ),
            (
                plain(KeyCode::Tab),
                focused,
                Action::ComposerInput(plain(KeyCode::Tab)),
            ),
            (
                press(KeyCode::Up, KeyModifiers::ALT),
                focused,
                Action::EditQueued,
            ),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// Suspend and repaint belong to the terminal, and the keys that only
    /// move the view belong to the transcript: a modal, answerable or
    /// not, does not take them.
    #[test]
    fn terminal_and_view_keys_work_under_every_modal() {
        let confirm = KeyContext {
            modal: Some(ModalKind::Confirm {
                default: false,
                tool: true,
            }),
            idle_ok: false,
            question_fully_shown: false,
            ..IDLE
        };
        let input = KeyContext {
            modal: Some(ModalKind::Input),
            ..IDLE
        };
        let shift = |code: KeyCode| press(code, KeyModifiers::SHIFT);
        for ctx in [confirm, input] {
            assert_eq!(map_key(ctrl('z'), &ctx), Action::Suspend, "{ctx:?}");
            assert_eq!(map_key(ctrl('l'), &ctx), Action::ScheduleFrame, "{ctx:?}");
            let scrolled = KeyContext {
                scrolled: true,
                ..ctx
            };
            let cases = [
                (plain(KeyCode::F(2)), ctx, Action::ToggleTodo),
                (plain(KeyCode::PageUp), ctx, Action::ScrollPage(-1)),
                (plain(KeyCode::PageDown), ctx, Action::ScrollPage(1)),
                (shift(KeyCode::Up), ctx, Action::ScrollLines(-1)),
                (shift(KeyCode::Down), ctx, Action::ScrollLines(1)),
                (plain(KeyCode::End), scrolled, Action::ScrollToBottom),
            ];
            for (key, ctx, expected) in cases {
                assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
            }
        }
    }

    /// The keys that act on a row are the modal's while one is up: a
    /// confirm swallows them (and Enter still answers it with a row
    /// focused), an input field types them.
    #[test]
    fn focus_keys_stay_with_the_modal() {
        let confirm = KeyContext {
            modal: Some(ModalKind::Confirm {
                default: false,
                tool: true,
            }),
            focused: true,
            scrolled: true,
            ..IDLE
        };
        let input = KeyContext {
            modal: Some(ModalKind::Input),
            ..confirm
        };
        let control = |code: KeyCode| press(code, KeyModifiers::CONTROL);
        let shift_g = press(KeyCode::Char('G'), KeyModifiers::SHIFT);
        let cases = [
            (control(KeyCode::Up), confirm, Action::Nothing),
            (control(KeyCode::Down), confirm, Action::Nothing),
            (shift_g, confirm, Action::Nothing),
            (
                plain(KeyCode::Enter),
                confirm,
                Action::Answer(PromptAnswer::Confirmed(false)),
            ),
            (
                control(KeyCode::Up),
                input,
                Action::ModalInput(control(KeyCode::Up)),
            ),
            (
                control(KeyCode::Down),
                input,
                Action::ModalInput(control(KeyCode::Down)),
            ),
            (shift_g, input, Action::ModalInput(shift_g)),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    #[test]
    fn confirm_modal_hotkeys_need_the_typing_idle_gap() {
        let modal = KeyContext {
            modal: Some(ModalKind::Confirm {
                default: false,
                tool: true,
            }),
            ..IDLE
        };
        let busy_typing = KeyContext {
            idle_ok: false,
            ..modal
        };
        let no_tool = KeyContext {
            modal: Some(ModalKind::Confirm {
                default: true,
                tool: false,
            }),
            ..IDLE
        };
        let no_tool_busy_typing = KeyContext {
            idle_ok: false,
            ..no_tool
        };
        let clipped = KeyContext {
            question_fully_shown: false,
            ..modal
        };
        let cases = [
            (
                plain(KeyCode::Char('y')),
                modal,
                Action::Answer(PromptAnswer::Confirmed(true)),
            ),
            (
                plain(KeyCode::Char('n')),
                modal,
                Action::Answer(PromptAnswer::Confirmed(false)),
            ),
            (
                press(KeyCode::Char('Y'), KeyModifiers::SHIFT),
                modal,
                Action::Answer(PromptAnswer::Confirmed(true)),
            ),
            (
                press(KeyCode::Char('N'), KeyModifiers::SHIFT),
                modal,
                Action::Answer(PromptAnswer::Confirmed(false)),
            ),
            (
                press(KeyCode::Char('A'), KeyModifiers::SHIFT),
                modal,
                Action::AnswerYesForSession,
            ),
            (
                plain(KeyCode::Char('a')),
                modal,
                Action::AnswerYesForSession,
            ),
            (plain(KeyCode::Char('a')), no_tool, Action::Nothing),
            (plain(KeyCode::Char('y')), busy_typing, Action::Nothing),
            (plain(KeyCode::Enter), busy_typing, Action::Nothing),
            (
                plain(KeyCode::Enter),
                modal,
                Action::Answer(PromptAnswer::Confirmed(false)),
            ),
            (
                plain(KeyCode::Enter),
                no_tool,
                Action::Answer(PromptAnswer::Confirmed(true)),
            ),
            (plain(KeyCode::Enter), no_tool_busy_typing, Action::Nothing),
            (
                plain(KeyCode::Esc),
                busy_typing,
                Action::Answer(PromptAnswer::Cancelled),
            ),
            (plain(KeyCode::Char('y')), clipped, Action::Nothing),
            (plain(KeyCode::Char('n')), clipped, Action::Nothing),
            (plain(KeyCode::Char('a')), clipped, Action::Nothing),
            (plain(KeyCode::Enter), clipped, Action::Nothing),
            (
                plain(KeyCode::Esc),
                clipped,
                Action::Answer(PromptAnswer::Cancelled),
            ),
            (ctrl('c'), clipped, Action::Answer(PromptAnswer::Cancelled)),
            (ctrl('c'), modal, Action::Answer(PromptAnswer::Cancelled)),
            (plain(KeyCode::Char('x')), modal, Action::Nothing),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    #[test]
    fn input_modal_types_into_its_field() {
        let modal = KeyContext {
            modal: Some(ModalKind::Input),
            ..IDLE
        };
        let typed = plain(KeyCode::Char('s'));
        assert_eq!(map_key(typed, &modal), Action::ModalInput(typed));
        assert_eq!(
            map_key(plain(KeyCode::Enter), &modal),
            Action::SubmitModalInput
        );
        for modifiers in [
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
        ] {
            assert_eq!(
                map_key(press(KeyCode::Enter, modifiers), &modal),
                Action::SubmitModalInput,
                "{modifiers:?}"
            );
        }
        assert_eq!(
            map_key(press(KeyCode::Char('j'), KeyModifiers::CONTROL), &modal),
            Action::Nothing
        );
        assert_eq!(
            map_key(plain(KeyCode::Esc), &modal),
            Action::Answer(PromptAnswer::Cancelled)
        );
    }

    /// Enter on a field modal is an answer, so it waits for the modal to
    /// settle; unlike a confirm's hotkeys it does not wait out the typing
    /// gap, since typing is how the field is answered. A gated Enter is
    /// swallowed, not typed into the field. Letters are typed whatever
    /// the gates say, and Esc cancels through them.
    #[test]
    fn field_modal_enter_needs_the_modal_settled_not_the_typing_gap() {
        let typed = plain(KeyCode::Char('s'));
        for kind in [
            ModalKind::Input,
            ModalKind::Password,
            ModalKind::Select {
                custom_field_open: true,
            },
        ] {
            let modal = KeyContext {
                modal: Some(kind),
                ..IDLE
            };
            let unsettled = KeyContext {
                field_settled: false,
                ..modal
            };
            let busy_typing = KeyContext {
                idle_ok: false,
                ..modal
            };
            let clipped = KeyContext {
                question_fully_shown: false,
                ..modal
            };
            let cases = [
                (plain(KeyCode::Enter), modal, Action::SubmitModalInput),
                (plain(KeyCode::Enter), busy_typing, Action::SubmitModalInput),
                (plain(KeyCode::Enter), unsettled, Action::Nothing),
                (plain(KeyCode::Enter), clipped, Action::Nothing),
                (typed, unsettled, Action::ModalInput(typed)),
                (
                    plain(KeyCode::Esc),
                    unsettled,
                    Action::Answer(PromptAnswer::Cancelled),
                ),
                (
                    ctrl('c'),
                    unsettled,
                    Action::Answer(PromptAnswer::Cancelled),
                ),
            ];
            for (key, ctx, expected) in cases {
                assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
            }
        }
    }

    /// The list modals: the arrows move without the gate; Enter, Space on
    /// a checkbox, the `j`/`k` letters and a select's digits wait for it,
    /// so a typed-ahead space cannot tick a row the user never saw. A
    /// checkbox has no digit shortcuts (a digit would have to both move
    /// and answer), and a select has no Space.
    #[test]
    fn list_modal_keys() {
        let select = KeyContext {
            modal: Some(ModalKind::Select {
                custom_field_open: false,
            }),
            ..IDLE
        };
        let select_busy = KeyContext {
            idle_ok: false,
            ..select
        };
        let select_clipped = KeyContext {
            question_fully_shown: false,
            ..select
        };
        let checkbox = KeyContext {
            modal: Some(ModalKind::Checkbox),
            ..IDLE
        };
        let checkbox_busy = KeyContext {
            idle_ok: false,
            ..checkbox
        };
        let cases = [
            (plain(KeyCode::Up), select, Action::ModalMove(-1)),
            (plain(KeyCode::Down), select, Action::ModalMove(1)),
            (plain(KeyCode::Up), select_busy, Action::ModalMove(-1)),
            (plain(KeyCode::Down), checkbox_busy, Action::ModalMove(1)),
            (plain(KeyCode::Char('k')), select, Action::ModalMove(-1)),
            (plain(KeyCode::Char('j')), select, Action::ModalMove(1)),
            (plain(KeyCode::Char('k')), select_busy, Action::Nothing),
            (plain(KeyCode::Char('j')), checkbox_busy, Action::Nothing),
            (plain(KeyCode::Char('1')), select, Action::ModalJump(0)),
            (plain(KeyCode::Char('9')), select, Action::ModalJump(8)),
            (plain(KeyCode::Char('0')), select, Action::Nothing),
            (plain(KeyCode::Char('2')), select_busy, Action::Nothing),
            (plain(KeyCode::Char('2')), select_clipped, Action::Nothing),
            (plain(KeyCode::Char('2')), checkbox, Action::Nothing),
            (plain(KeyCode::Char(' ')), checkbox, Action::ModalToggle),
            (plain(KeyCode::Char(' ')), checkbox_busy, Action::Nothing),
            (plain(KeyCode::Char(' ')), select, Action::Nothing),
            (plain(KeyCode::Enter), select, Action::SubmitModalChoice),
            (plain(KeyCode::Enter), checkbox, Action::SubmitModalChoice),
            (plain(KeyCode::Enter), select_busy, Action::Nothing),
            (plain(KeyCode::Enter), select_clipped, Action::Nothing),
            (plain(KeyCode::Enter), checkbox_busy, Action::Nothing),
            (plain(KeyCode::Char('x')), select, Action::Nothing),
            (
                plain(KeyCode::Esc),
                select_busy,
                Action::Answer(PromptAnswer::Cancelled),
            ),
            (
                ctrl('c'),
                checkbox_busy,
                Action::Answer(PromptAnswer::Cancelled),
            ),
            (ctrl('z'), select, Action::Suspend),
            (plain(KeyCode::PageUp), checkbox, Action::ScrollPage(-1)),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// The table stays pure: no terminal writes and no std I/O in this file.
    #[test]
    fn the_key_table_does_no_io() {
        let source = include_str!("keys.rs");
        let production = &source[..source.find("\n#[cfg(test)]\nmod tests {").unwrap()];
        for needle in [["io", "::"].concat(), ["execute", "!"].concat()] {
            assert!(!production.contains(&needle), "{needle} in keys.rs");
        }
    }

    /// Ctrl+T opens the pager over an idle or busy composer, never under a
    /// question; F3 toggles the roster the same way.
    #[test]
    fn ctrl_t_and_f3_open_the_pager_and_the_roster() {
        let running = KeyContext {
            turn_running: true,
            ..IDLE
        };
        let typing = KeyContext {
            composer_empty: false,
            ..IDLE
        };
        let roster = KeyContext {
            popup: Some(PopupKind::Roster),
            ..IDLE
        };
        let confirm = KeyContext {
            modal: Some(ModalKind::Confirm {
                default: true,
                tool: false,
            }),
            ..IDLE
        };
        let cases = [
            (ctrl('t'), IDLE, Action::OpenPager),
            (ctrl('t'), running, Action::OpenPager),
            (ctrl('t'), typing, Action::OpenPager),
            (ctrl('t'), roster, Action::OpenPager),
            (ctrl('t'), confirm, Action::Nothing),
            (plain(KeyCode::F(3)), IDLE, Action::ToggleRoster),
            (plain(KeyCode::F(3)), running, Action::ToggleRoster),
            (plain(KeyCode::F(3)), roster, Action::ToggleRoster),
            (plain(KeyCode::F(3)), confirm, Action::Nothing),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// With the roster up its letters act on the highlighted row and no
    /// other letter reaches the composer; Esc and the quit chords close
    /// it, Enter answers the row even mid-turn.
    #[test]
    fn roster_keys() {
        let roster = KeyContext {
            popup: Some(PopupKind::Roster),
            ..IDLE
        };
        let running = KeyContext {
            turn_running: true,
            ..roster
        };
        let cases = [
            (plain(KeyCode::Char('j')), roster, Action::PaletteMove(1)),
            (plain(KeyCode::Char('k')), roster, Action::PaletteMove(-1)),
            (plain(KeyCode::Down), roster, Action::PaletteMove(1)),
            (plain(KeyCode::Up), roster, Action::PaletteMove(-1)),
            (plain(KeyCode::Char('x')), roster, Action::RosterCancel),
            (plain(KeyCode::Char('x')), running, Action::RosterCancel),
            (plain(KeyCode::Char('o')), roster, Action::RosterOpen),
            (plain(KeyCode::Char('a')), roster, Action::Nothing),
            (plain(KeyCode::Backspace), roster, Action::Nothing),
            (ctrl('u'), roster, Action::Nothing),
            (ctrl('w'), roster, Action::Nothing),
            (plain(KeyCode::Left), roster, Action::Nothing),
            (plain(KeyCode::Home), roster, Action::Nothing),
            (plain(KeyCode::Enter), roster, Action::PaletteAccept),
            (plain(KeyCode::Enter), running, Action::PaletteAccept),
            (plain(KeyCode::Esc), roster, Action::ClosePopup),
            (plain(KeyCode::Esc), running, Action::ClosePopup),
            (ctrl('c'), running, Action::ClosePopup),
            (
                plain(KeyCode::Char('x')),
                IDLE,
                Action::ComposerInput(plain(KeyCode::Char('x'))),
            ),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// The pager takes every key but suspend and repaint, in each of its
    /// three modes.
    #[test]
    fn pager_keys() {
        let browse = KeyContext {
            pager: Some(PagerMode::Browse),
            turn_running: true,
            ..IDLE
        };
        let find = KeyContext {
            pager: Some(PagerMode::Find),
            ..IDLE
        };
        let picker = KeyContext {
            pager: Some(PagerMode::Picker),
            ..IDLE
        };
        let shift = |c: char| press(KeyCode::Char(c), KeyModifiers::SHIFT);
        let cases = [
            (plain(KeyCode::Char('j')), browse, Action::PagerScroll(1)),
            (plain(KeyCode::Down), browse, Action::PagerScroll(1)),
            (plain(KeyCode::Char('k')), browse, Action::PagerScroll(-1)),
            (plain(KeyCode::Up), browse, Action::PagerScroll(-1)),
            (plain(KeyCode::PageDown), browse, Action::PagerPage(1)),
            (plain(KeyCode::PageUp), browse, Action::PagerPage(-1)),
            (plain(KeyCode::Char('g')), browse, Action::PagerTop),
            (plain(KeyCode::Char('G')), browse, Action::PagerBottom),
            (shift('G'), browse, Action::PagerBottom),
            (plain(KeyCode::Char('q')), browse, Action::ClosePager),
            (plain(KeyCode::Esc), browse, Action::ClosePager),
            (ctrl('t'), browse, Action::ClosePager),
            (ctrl('c'), browse, Action::ClosePager),
            (ctrl('z'), browse, Action::Suspend),
            (ctrl('l'), browse, Action::ScheduleFrame),
            (plain(KeyCode::F(2)), browse, Action::Nothing),
            (plain(KeyCode::Char('/')), browse, Action::PagerFindOpen),
            (
                plain(KeyCode::Char('n')),
                browse,
                Action::PagerFindNext { forward: true },
            ),
            (
                plain(KeyCode::Char('N')),
                browse,
                Action::PagerFindNext { forward: false },
            ),
            (plain(KeyCode::Tab), browse, Action::OpenThreadPicker),
            (plain(KeyCode::Char('x')), browse, Action::Nothing),
            (plain(KeyCode::Char('q')), find, Action::PagerFindChar('q')),
            (plain(KeyCode::Char('G')), find, Action::PagerFindChar('G')),
            (plain(KeyCode::Backspace), find, Action::PagerFindBackspace),
            (plain(KeyCode::Enter), find, Action::PagerFindSubmit),
            (plain(KeyCode::Esc), find, Action::PagerFindCancel),
            (
                plain(KeyCode::Char('j')),
                picker,
                Action::ThreadPickerMove(1),
            ),
            (plain(KeyCode::Up), picker, Action::ThreadPickerMove(-1)),
            (plain(KeyCode::Enter), picker, Action::ThreadPickerChoose),
            (plain(KeyCode::Esc), picker, Action::CloseThreadPicker),
            (plain(KeyCode::Tab), picker, Action::CloseThreadPicker),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }
}
