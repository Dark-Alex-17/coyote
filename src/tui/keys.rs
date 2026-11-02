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
    /// Whether a second has passed since the last composer keystroke: the
    /// condition for a bare hotkey to count as an answer, and for Enter to
    /// answer a tool's question (a user's own question takes it at once).
    pub(crate) idle_ok: bool,
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
    Nothing,
}

pub(crate) fn map_key(key: KeyEvent, ctx: &KeyContext) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match ctx.modal {
        Some(ModalKind::Confirm { default, tool }) => match key.code {
            KeyCode::Enter if tool && !ctx.idle_ok => Action::Nothing,
            KeyCode::Enter => Action::Answer(PromptAnswer::Confirmed(default)),
            KeyCode::Esc => Action::Answer(PromptAnswer::Cancelled),
            KeyCode::Char('c') if ctrl => Action::Answer(PromptAnswer::Cancelled),
            KeyCode::Char('y') if ctx.idle_ok && !ctrl => {
                Action::Answer(PromptAnswer::Confirmed(true))
            }
            KeyCode::Char('n') if ctx.idle_ok && !ctrl => {
                Action::Answer(PromptAnswer::Confirmed(false))
            }
            KeyCode::Char('a') if ctx.idle_ok && !ctrl && tool => Action::AnswerYesForSession,
            _ => Action::Nothing,
        },
        Some(ModalKind::Input) => match key.code {
            KeyCode::Enter if key.modifiers.is_empty() => Action::SubmitModalInput,
            KeyCode::Esc => Action::Answer(PromptAnswer::Cancelled),
            KeyCode::Char('c') if ctrl => Action::Answer(PromptAnswer::Cancelled),
            _ => Action::ModalInput(key),
        },
        None => match key.code {
            KeyCode::Char('c') if ctrl => {
                if ctx.turn_running {
                    Action::Interrupt
                } else if ctx.composer_empty {
                    Action::Quit
                } else {
                    Action::ClearComposer
                }
            }
            KeyCode::Char('d') if ctrl => Action::Quit,
            KeyCode::Esc if ctx.turn_running => Action::Interrupt,
            KeyCode::Esc => Action::Nothing,
            KeyCode::Char('z') if ctrl => Action::Suspend,
            // The editor would inherit fds 1 and 2 on the turn's capture pipe.
            KeyCode::Char('o') if ctrl && ctx.turn_running => Action::Nothing,
            KeyCode::Char('o') if ctrl => Action::OpenEditor,
            KeyCode::Char('l') if ctrl => Action::ScheduleFrame,
            KeyCode::Char('j') if ctrl => Action::InsertNewline,
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                Action::InsertNewline
            }
            KeyCode::Enter if ctx.turn_running => Action::Nothing,
            KeyCode::Enter => Action::Submit,
            _ => Action::ComposerInput(key),
        },
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
            (ctrl('d'), running, Action::Quit),
            (plain(KeyCode::Esc), IDLE, Action::Nothing),
            (plain(KeyCode::Esc), running, Action::Interrupt),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
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
            (
                plain(KeyCode::Enter),
                no_tool_busy_typing,
                Action::Answer(PromptAnswer::Confirmed(true)),
            ),
            (
                plain(KeyCode::Esc),
                busy_typing,
                Action::Answer(PromptAnswer::Cancelled),
            ),
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
