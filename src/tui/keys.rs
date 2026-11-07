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
}

/// What the key table needs to know about the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyContext {
    pub(crate) turn_running: bool,
    pub(crate) composer_empty: bool,
    pub(crate) modal: Option<ModalKind>,
    /// Whether a second has passed since the last key pressed (accepted
    /// or ignored, outside a modal or under one) or since the previous
    /// answer: the
    /// condition for a bare hotkey or Enter to count as an answer to a
    /// confirm, so a modal that appears mid-sentence cannot be answered by
    /// the keystroke already in flight.
    pub(crate) idle_ok: bool,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Send the composer text as the next line.
    Submit,
    InsertNewline,
    ClearComposer,
    /// Soft-interrupt the running turn.
    Interrupt,
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
    ModalInput(KeyEvent),
    ComposerInput(KeyEvent),
    ToggleTodo,
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
        Some(ModalKind::Input) => match code {
            KeyCode::Esc => Action::Answer(PromptAnswer::Cancelled),
            KeyCode::Char('c') if ctrl => Action::Answer(PromptAnswer::Cancelled),
            // The field is one line, so the composer's newline chords have
            // nothing to insert: Enter submits however it is held.
            KeyCode::Enter => Action::SubmitModalInput,
            KeyCode::Char('j') if ctrl => Action::Nothing,
            _ => Action::ModalInput(key),
        },
        None => match code {
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
            // A draft keeps the letter: `G` mid-sentence is typing.
            KeyCode::Char('g') if shift && !ctrl && ctx.scrolled && ctx.composer_empty => {
                Action::ScrollToBottom
            }
            // Interrupting the turn outranks a focused row; the focus
            // goes with it.
            KeyCode::Esc if ctx.turn_running => Action::Interrupt,
            KeyCode::Esc if ctx.focused => Action::ClearFocus,
            KeyCode::Esc => Action::Nothing,
            // The editor would inherit fds 1 and 2 on the turn's capture pipe.
            KeyCode::Char('o') if ctrl && ctx.turn_running => Action::Nothing,
            KeyCode::Char('o') if ctrl => Action::OpenEditor,
            KeyCode::Char('j') if ctrl => Action::InsertNewline,
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                Action::InsertNewline
            }
            KeyCode::Enter if ctx.focused && ctx.composer_empty => Action::ToggleFocused,
            KeyCode::Enter if ctx.turn_running => Action::Nothing,
            KeyCode::Enter => Action::Submit,
            _ => Action::ComposerInput(key),
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
        modal: None,
        idle_ok: true,
        question_fully_shown: true,
        scrolled: false,
        focused: false,
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
        let cases = [
            (ctrl('c'), IDLE, Action::Quit),
            (ctrl('c'), typing, Action::ClearComposer),
            (ctrl('c'), running, Action::Interrupt),
            (ctrl('d'), IDLE, Action::Quit),
            (ctrl('d'), typing, Action::ComposerInput(ctrl('d'))),
            (ctrl('d'), running, Action::Quit),
            (plain(KeyCode::Esc), IDLE, Action::Nothing),
            (plain(KeyCode::Esc), running, Action::Interrupt),
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
            (plain(KeyCode::Enter), running, Action::Nothing),
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

    /// The transcript's keys: F2, the page and Shift+arrow scrolls and
    /// the Ctrl+arrow focus walk work whatever the turn is doing; End
    /// goes to the view only while it is scrolled, Shift+G only while
    /// it is scrolled and the composer is empty; Enter goes to a
    /// focused row while the composer is empty, Esc while no turn runs.
    /// The plain arrows and Tab stay the composer's.
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
                focused,
                Action::ComposerInput(plain(KeyCode::Up)),
            ),
            (
                plain(KeyCode::Down),
                scrolled,
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
                Action::ComposerInput(press(KeyCode::Up, KeyModifiers::ALT)),
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
    fn suspend_and_repaint_work_under_every_modal() {
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

    /// The table stays pure: no terminal writes and no std I/O in this file.
    #[test]
    fn the_key_table_does_no_io() {
        let source = include_str!("keys.rs");
        let production = &source[..source.find("\n#[cfg(test)]\nmod tests {").unwrap()];
        for needle in [["io", "::"].concat(), ["execute", "!"].concat()] {
            assert!(!production.contains(&needle), "{needle} in keys.rs");
        }
    }
}
