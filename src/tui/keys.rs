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

/// What is drawn over the transcript and takes the keys, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayKind {
    /// Covers the whole screen.
    Pager(PagerMode),
    /// Covers the transcript; the bottom pane stays in sight under it.
    Help,
}

/// The layer that takes the keys: whichever of these is open first, in
/// this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layer {
    Overlay(OverlayKind),
    Modal(ModalKind),
    Band(PopupKind),
    Composer,
}

pub(crate) fn layer(
    overlay: Option<OverlayKind>,
    modal: Option<ModalKind>,
    popup: Option<PopupKind>,
) -> Layer {
    match (overlay, modal, popup) {
        (Some(overlay), _, _) => Layer::Overlay(overlay),
        (None, Some(modal), _) => Layer::Modal(modal),
        (None, None, Some(popup)) => Layer::Band(popup),
        (None, None, None) => Layer::Composer,
    }
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
    /// The pager or the key help; either takes every key but the
    /// terminal's own.
    pub(crate) overlay: Option<OverlayKind>,
}

impl KeyContext {
    pub(crate) fn layer(&self) -> Layer {
        layer(self.overlay, self.modal, self.popup)
    }

    /// Whether a plain `?` on the composer opens the help: only an empty
    /// composer with nothing open over it (no band, question, pager or
    /// help) gives it up. Anywhere else it is typing, vi normal mode's
    /// swallowing included. The status strip advertises `?` by the same
    /// test.
    pub(crate) fn question_mark_opens_help(&self) -> bool {
        self.composer_empty && self.layer() == Layer::Composer
    }
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
    /// Cut from the cursor back to the start of its line, in the composer
    /// or a question's text field; at the start, join the line above.
    DeleteToLineHead,
    /// Undo the last edit of the composer or a question's text field.
    Undo,
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
    /// Open the key help, or close it when it is open.
    ToggleHelp,
    /// Move the key help by this many lines, down when positive.
    HelpScroll(i32),
    /// Move the key help by this many pages, down when positive.
    HelpPage(i32),
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

/// Where a row of the key help applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HelpContext {
    Anywhere,
    /// The view keys, which a question waiting below does not take.
    Transcript,
    Composer,
    /// The text area's own editing keys, which the key map hands on.
    ComposerEditing,
    TurnRunning,
    Question,
    Bands,
    Roster,
    ViNormal,
    Pager,
    PagerFind,
    ThreadPicker,
    KeyHelp,
}

impl HelpContext {
    pub(crate) fn title(self) -> &'static str {
        match self {
            HelpContext::Anywhere => "Anywhere",
            HelpContext::Transcript => "Transcript, also under a question",
            HelpContext::Composer => "Composer",
            HelpContext::ComposerEditing => "Composer and text field editing",
            HelpContext::TurnRunning => "While a turn runs",
            HelpContext::Question => "Question",
            HelpContext::Bands => "Palette, history search and rewind",
            HelpContext::Roster => "Tasks (the roster)",
            HelpContext::ViNormal => "Vim mode",
            HelpContext::Pager => "Transcript view",
            HelpContext::PagerFind => "Transcript view find",
            HelpContext::ThreadPicker => "Thread picker",
            HelpContext::KeyHelp => "This help",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HelpRow {
    pub(crate) context: HelpContext,
    pub(crate) keys: &'static str,
    pub(crate) does: &'static str,
}

const fn row(context: HelpContext, keys: &'static str, does: &'static str) -> HelpRow {
    HelpRow {
        context,
        keys,
        does,
    }
}

/// What the key help lists, grouped by context in the order shown. The
/// unit tests replay every row through `map_key` and sweep the key map
/// for bindings no row names, so the two cannot drift apart. The mouse
/// wheel rows are not keys; the app's tests pin them.
///
/// Some text area chords are left out on purpose: Ctrl+A and Ctrl+B are
/// terminal multiplexer prefixes (screen, tmux), and terminals send Ctrl+H
/// and Ctrl+M for Backspace and Enter, so naming them would promise keys
/// many users never get.
pub(crate) const KEY_HELP: &[HelpRow] = {
    use HelpContext::*;
    &[
        row(Anywhere, "Ctrl+Z", "Suspend to the shell"),
        row(Anywhere, "Ctrl+L", "Redraw the screen"),
        row(Transcript, "Ctrl+X", "Show or hide the todo panel"),
        row(Transcript, "PgUp / PgDn", "Scroll the transcript a page"),
        row(
            Transcript,
            "Shift+Up / Shift+Down",
            "Scroll the transcript a line",
        ),
        row(
            Transcript,
            "End",
            "Jump back to the newest output when scrolled",
        ),
        row(Transcript, "Mouse wheel", "Scroll the transcript"),
        row(
            Composer,
            "Enter",
            "Send the line; on an empty composer, open or close the focused row",
        ),
        row(
            Composer,
            "Shift+Enter / Alt+Enter / Ctrl+J",
            "Insert a newline",
        ),
        row(
            Composer,
            "Up / Down",
            "Walk the history on a one-line composer",
        ),
        row(Composer, "Ctrl+R", "Search the history"),
        row(
            Composer,
            "Alt+Up",
            "Take the newest queued line back to edit",
        ),
        row(
            Composer,
            "Ctrl+O",
            "Edit the line in $VISUAL or $EDITOR, between turns",
        ),
        row(Composer, "Ctrl+T", "Open the transcript view"),
        row(Composer, "Alt+T", "Show or hide tasks (the roster)"),
        row(Composer, "?", "Show this help, on an empty composer"),
        row(
            Composer,
            "Ctrl+Up / Ctrl+Down",
            "Move the focus between transcript rows",
        ),
        row(
            Composer,
            "Shift+G",
            "Jump back to the newest output, when scrolled with an empty composer",
        ),
        row(
            Composer,
            "Esc",
            "Leave the focused row, or Vim insert mode; twice on an empty composer, rewind",
        ),
        row(
            Composer,
            "Ctrl+C",
            "Clear the line, or quit when it is empty",
        ),
        row(
            Composer,
            "Ctrl+D",
            "Quit on an empty composer; otherwise delete forward",
        ),
        row(
            ComposerEditing,
            "Left / Right / Ctrl+F",
            "Move the cursor a character",
        ),
        row(
            ComposerEditing,
            "Up / Down / Ctrl+P / Ctrl+N",
            "Move the cursor a line in a multi-line draft",
        ),
        row(
            ComposerEditing,
            "Ctrl+Left / Ctrl+Right / Alt+B / Alt+F",
            "Move the cursor a word",
        ),
        row(
            ComposerEditing,
            "Alt+[ / Alt+] / Alt+P / Alt+N",
            "Move the cursor a paragraph",
        ),
        row(
            ComposerEditing,
            "Home / End / Ctrl+E",
            "Start or end of the line",
        ),
        row(
            ComposerEditing,
            "Alt+< / Alt+>",
            "Start or end of the draft",
        ),
        row(ComposerEditing, "Alt+V / Ctrl+V", "Move the cursor a page"),
        row(
            ComposerEditing,
            "Tab",
            "Indent with spaces, except on a command line",
        ),
        row(
            ComposerEditing,
            "Backspace / Delete",
            "Delete before or under the cursor",
        ),
        row(
            ComposerEditing,
            "Ctrl+W / Alt+Backspace",
            "Cut the word before the cursor",
        ),
        row(
            ComposerEditing,
            "Alt+D / Alt+Delete",
            "Cut the word after the cursor",
        ),
        row(ComposerEditing, "Ctrl+K", "Cut to the end of the line"),
        row(
            ComposerEditing,
            "Ctrl+U",
            "Cut to the start of the line; at its start, join the line above",
        ),
        row(ComposerEditing, "Ctrl+Y", "Paste what was last cut"),
        row(ComposerEditing, "Ctrl+_ / Ctrl+/ / Ctrl+7", "Undo"),
        row(
            ComposerEditing,
            "Shift+Left / Shift+Right",
            "Select text to type over or delete; there is no cut, Ctrl+X opens the todo panel",
        ),
        row(TurnRunning, "Esc / Ctrl+C", "Interrupt the turn"),
        row(TurnRunning, "Enter", "Hold the line for the running turn"),
        row(Question, "y / n", "Answer yes or no"),
        row(
            Question,
            "a",
            "Answer yes for the rest of the session, for a tool",
        ),
        row(
            Question,
            "Enter",
            "Send the default, the typed answer or the chosen options",
        ),
        row(Question, "Up / Down / j / k", "Move between the options"),
        row(Question, "Space", "Tick or untick an option"),
        row(
            Question,
            "1-9",
            "Choose that option, in a single-choice list",
        ),
        row(
            Question,
            "Esc",
            "Cancel the question; the command that asked it may carry on",
        ),
        row(
            Question,
            "Ctrl+C",
            "Cancel the question and stop the command that asked it",
        ),
        row(Bands, "Up / Down / Tab / Shift+Tab", "Move the highlight"),
        row(Bands, "Enter", "Take the highlighted row"),
        row(Bands, "Backspace", "Delete a history search character"),
        row(Bands, "Esc / Ctrl+C / Ctrl+D", "Close the band"),
        row(
            Roster,
            "j / k / Up / Down / Tab / Shift+Tab",
            "Move between the rows",
        ),
        row(Roster, "x", "Cancel the highlighted task"),
        row(
            Roster,
            "o",
            "Show the highlighted task in the transcript view",
        ),
        row(Roster, "Enter", "Answer the highlighted question"),
        row(Roster, "Ctrl+T", "Open the transcript view"),
        row(
            Roster,
            "Esc / Ctrl+C / Ctrl+D / Alt+T",
            "Close tasks (the roster)",
        ),
        row(ViNormal, "h / j / k / l", "Move the cursor"),
        row(ViNormal, "w / b", "Next or previous word"),
        row(ViNormal, "0 / $", "Start or end of the line"),
        row(ViNormal, "x", "Delete the character under the cursor"),
        row(ViNormal, "dd", "Delete the line"),
        row(
            ViNormal,
            "i / a / A",
            "Insert before or after the cursor, or at the line end",
        ),
        row(Pager, "j / k / Up / Down", "Scroll a line"),
        row(Pager, "Space / PgDn / PgUp", "Scroll a page"),
        row(Pager, "g / Home / G / End", "Top or bottom"),
        row(Pager, "/", "Find"),
        row(Pager, "n / N", "Next or previous match"),
        row(Pager, "Tab", "Pick a thread"),
        row(Pager, "Mouse wheel", "Scroll"),
        row(
            Pager,
            "q / Esc / Ctrl+C / Ctrl+D / Ctrl+T",
            "Close the transcript view",
        ),
        row(PagerFind, "Enter", "Search and keep the text for n and N"),
        row(PagerFind, "Backspace", "Delete a character"),
        row(PagerFind, "Esc / Ctrl+C", "Cancel the find"),
        row(ThreadPicker, "j / k / Up / Down", "Move the highlight"),
        row(ThreadPicker, "Enter", "Open the thread"),
        row(
            ThreadPicker,
            "q / Esc / Tab / Ctrl+C / Ctrl+D",
            "Back to the transcript view",
        ),
        row(KeyHelp, "j / k / Up / Down", "Scroll a line"),
        row(KeyHelp, "PgUp / PgDn", "Scroll a page"),
        row(KeyHelp, "Mouse wheel", "Scroll"),
        row(KeyHelp, "? / q / Esc / Ctrl+C / Ctrl+D", "Close the help"),
    ]
};

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
    let modal = match ctx.layer() {
        Layer::Overlay(OverlayKind::Pager(mode)) => return pager_key(key, code, mode),
        Layer::Overlay(OverlayKind::Help) => return help_key(key, code),
        Layer::Modal(modal) => Some(modal),
        Layer::Band(_) | Layer::Composer => None,
    };
    match code {
        KeyCode::Char('x') if ctrl => return Action::ToggleTodo,
        KeyCode::PageUp => return Action::ScrollPage(-1),
        KeyCode::PageDown => return Action::ScrollPage(1),
        KeyCode::Up if shift => return Action::ScrollLines(-1),
        KeyCode::Down if shift => return Action::ScrollLines(1),
        KeyCode::End if ctx.scrolled => return Action::ScrollToBottom,
        _ => {}
    }
    match modal {
        Some(ModalKind::Confirm { default, tool }) => {
            let answerable = ctx.idle_ok && ctx.question_fully_shown;
            match code {
                KeyCode::Esc => Action::Answer(PromptAnswer::Cancelled),
                KeyCode::Char('c') if ctrl => Action::Answer(PromptAnswer::Interrupted),
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
            KeyCode::Char('c') if ctrl => Action::Answer(PromptAnswer::Interrupted),
            // The field is one line, so the composer's newline chords have
            // nothing to insert: Enter submits however it is held, once
            // the modal has settled; a gated Enter is swallowed rather
            // than typed.
            KeyCode::Enter if !(ctx.field_settled && ctx.question_fully_shown) => Action::Nothing,
            KeyCode::Enter => Action::SubmitModalInput,
            KeyCode::Char('j') if ctrl => Action::Nothing,
            KeyCode::Char('u') if ctrl => Action::DeleteToLineHead,
            KeyCode::Char('_' | '/' | '7') if ctrl => Action::Undo,
            KeyCode::Char('-') if ctrl && shift => Action::Undo,
            // The text area cuts a word on Alt+H as well as Ctrl+W, but
            // Zellij takes Alt+H even when locked, so it is swallowed
            // here and in the composer rather than offered.
            KeyCode::Char('h') if alt && !ctrl => Action::Nothing,
            _ => Action::ModalInput(key),
        },
        Some(
            ModalKind::Select {
                custom_field_open: false,
            }
            | ModalKind::Checkbox,
        ) => {
            let answerable = ctx.idle_ok && ctx.question_fully_shown;
            let checkbox = modal == Some(ModalKind::Checkbox);
            match code {
                KeyCode::Esc => Action::Answer(PromptAnswer::Cancelled),
                KeyCode::Char('c') if ctrl => Action::Answer(PromptAnswer::Interrupted),
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
            KeyCode::Char('t') if alt && !ctrl => Action::ToggleRoster,
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
            // The draft, the focus and the queue are out of sight under the
            // roster: only the band's own keys do anything.
            _ if ctx.popup == Some(PopupKind::Roster) => match code {
                KeyCode::Esc => Action::ClosePopup,
                KeyCode::Up | KeyCode::BackTab if !ctrl && !alt => Action::PaletteMove(-1),
                KeyCode::Down | KeyCode::Tab if !ctrl && !alt => Action::PaletteMove(1),
                KeyCode::Enter => Action::PaletteAccept,
                KeyCode::Char('j') if !ctrl => Action::PaletteMove(1),
                KeyCode::Char('k') if !ctrl => Action::PaletteMove(-1),
                KeyCode::Char('x') if !ctrl => Action::RosterCancel,
                KeyCode::Char('o') if !ctrl => Action::RosterOpen,
                _ => Action::Nothing,
            },
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
                    Some(PopupKind::HistorySearch | PopupKind::Rewind)
                ) =>
            {
                Action::PaletteAccept
            }
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
            KeyCode::Char('u') if ctrl => Action::DeleteToLineHead,
            // A legacy terminal sends Ctrl+_, Ctrl+/ and Ctrl+7 as one
            // byte, which crossterm reads as Ctrl+7. Under the kitty
            // protocol Ctrl+_ arrives as its unshifted key, Ctrl+Shift+-;
            // Ctrl+- without Shift is not bound.
            KeyCode::Char('_' | '/' | '7') if ctrl => Action::Undo,
            KeyCode::Char('-') if ctrl && shift => Action::Undo,
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
            KeyCode::Char('?') if !ctrl && !alt && ctx.question_mark_opens_help() => {
                Action::ToggleHelp
            }
            KeyCode::Char(_) if !ctrl && ctx.vi == Some(ViMode::Normal) => vi_normal(key.code),
            KeyCode::Char('h') if alt && !ctrl => Action::Nothing,
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

/// The key help's keys: it scrolls and closes, nothing under it is
/// reached.
fn help_key(key: KeyEvent, code: KeyCode) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match code {
        KeyCode::Esc => Action::ToggleHelp,
        KeyCode::Char('c' | 'd') if ctrl => Action::ToggleHelp,
        _ if ctrl => Action::Nothing,
        KeyCode::Char('?' | 'q') => Action::ToggleHelp,
        KeyCode::Down | KeyCode::Char('j') => Action::HelpScroll(1),
        KeyCode::Up | KeyCode::Char('k') => Action::HelpScroll(-1),
        KeyCode::PageDown => Action::HelpPage(1),
        KeyCode::PageUp => Action::HelpPage(-1),
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
    use crate::config::CommandPrefix;
    use crate::tui::composer::Composer;
    use crate::tui::theme::Theme;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Color;
    use ratatui::widgets::Widget;

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
        overlay: None,
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

    /// Esc closes the question; Ctrl-C closes it as an interrupt, in every
    /// modal kind, so a caller that carries on after a cancel can still
    /// stop on Ctrl-C.
    #[test]
    fn esc_cancels_and_ctrl_c_interrupts_every_modal() {
        for modal in [
            ModalKind::Confirm {
                default: false,
                tool: true,
            },
            ModalKind::Confirm {
                default: true,
                tool: false,
            },
            ModalKind::Input,
            ModalKind::Password,
            ModalKind::Select {
                custom_field_open: false,
            },
            ModalKind::Select {
                custom_field_open: true,
            },
            ModalKind::Checkbox,
        ] {
            let ctx = KeyContext {
                modal: Some(modal),
                ..IDLE
            };
            assert_eq!(
                map_key(plain(KeyCode::Esc), &ctx),
                Action::Answer(PromptAnswer::Cancelled),
                "{modal:?}"
            );
            assert_eq!(
                map_key(ctrl('c'), &ctx),
                Action::Answer(PromptAnswer::Interrupted),
                "{modal:?}"
            );
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

    /// Ctrl+_ as a kitty-protocol terminal reports it: the unshifted key
    /// with Shift held.
    fn kitty_ctrl_underscore() -> KeyEvent {
        press(
            KeyCode::Char('-'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )
    }

    #[test]
    fn kitty_ctrl_underscore_undoes_and_ctrl_minus_does_not() {
        for ctx in [IDLE, INPUT] {
            assert_eq!(
                map_key(kitty_ctrl_underscore(), &ctx),
                Action::Undo,
                "{ctx:?}"
            );
            assert_ne!(map_key(ctrl('-'), &ctx), Action::Undo, "{ctx:?}");
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

    /// The transcript's keys: Ctrl+X, the page and Shift+arrow scrolls and
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
            (ctrl('x'), IDLE, Action::ToggleTodo),
            (ctrl('x'), running, Action::ToggleTodo),
            (ctrl('x'), focused_typing, Action::ToggleTodo),
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
                (ctrl('x'), ctx, Action::ToggleTodo),
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
            (
                ctrl('c'),
                clipped,
                Action::Answer(PromptAnswer::Interrupted),
            ),
            (ctrl('c'), modal, Action::Answer(PromptAnswer::Interrupted)),
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
                    Action::Answer(PromptAnswer::Interrupted),
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
                Action::Answer(PromptAnswer::Interrupted),
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
        let source = include_str!("keys.rs").replace("\r\n", "\n");
        let production = &source[..source.find("\n#[cfg(test)]\nmod tests {").unwrap()];
        for needle in [["io", "::"].concat(), ["execute", "!"].concat()] {
            assert!(!production.contains(&needle), "{needle} in keys.rs");
        }
    }

    /// Overlay wins over modal and band, modal over band, band over composer.
    #[test]
    fn layer_takes_the_topmost_open_layer() {
        let overlay = Some(OverlayKind::Help);
        let modal = Some(ModalKind::Input);
        let popup = Some(PopupKind::Palette);
        let help = Layer::Overlay(OverlayKind::Help);
        let input = Layer::Modal(ModalKind::Input);
        let palette = Layer::Band(PopupKind::Palette);
        let cases = [
            (None, None, None, Layer::Composer),
            (None, None, popup, palette),
            (None, modal, None, input),
            (None, modal, popup, input),
            (overlay, None, None, help),
            (overlay, None, popup, help),
            (overlay, modal, None, help),
            (overlay, modal, popup, help),
        ];
        for (overlay, modal, popup, expected) in cases {
            assert_eq!(
                layer(overlay, modal, popup),
                expected,
                "{overlay:?} {modal:?} {popup:?}"
            );
        }
    }

    /// Ctrl+T opens the pager over an idle or busy composer, never under a
    /// question; Alt+T toggles the roster the same way, and a text field
    /// takes it as typing. Ctrl+G is bound to nothing: the text area it
    /// falls through to ignores it.
    #[test]
    fn ctrl_t_and_alt_t_open_the_pager_and_the_roster() {
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
        let input = KeyContext {
            modal: Some(ModalKind::Input),
            ..IDLE
        };
        let select = KeyContext {
            modal: Some(ModalKind::Select {
                custom_field_open: false,
            }),
            ..IDLE
        };
        let vi_normal = KeyContext {
            vi: Some(ViMode::Normal),
            ..typing
        };
        let alt_t = press(KeyCode::Char('t'), KeyModifiers::ALT);
        let cases = [
            (ctrl('t'), IDLE, Action::OpenPager),
            (ctrl('t'), running, Action::OpenPager),
            (ctrl('t'), typing, Action::OpenPager),
            (ctrl('t'), roster, Action::OpenPager),
            (ctrl('t'), confirm, Action::Nothing),
            (alt_t, IDLE, Action::ToggleRoster),
            (alt_t, running, Action::ToggleRoster),
            (alt_t, typing, Action::ToggleRoster),
            (alt_t, vi_normal, Action::ToggleRoster),
            (alt_t, roster, Action::ToggleRoster),
            (alt_t, confirm, Action::Nothing),
            (alt_t, select, Action::Nothing),
            (alt_t, input, Action::ModalInput(alt_t)),
            (ctrl('g'), IDLE, Action::ComposerInput(ctrl('g'))),
            (ctrl('g'), running, Action::ComposerInput(ctrl('g'))),
            (ctrl('g'), typing, Action::ComposerInput(ctrl('g'))),
            (ctrl('g'), vi_normal, Action::ComposerInput(ctrl('g'))),
            (ctrl('g'), roster, Action::Nothing),
            (ctrl('g'), confirm, Action::Nothing),
            (ctrl('g'), select, Action::Nothing),
            (ctrl('g'), input, Action::ModalInput(ctrl('g'))),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// With the roster up its letters act on the highlighted row and no
    /// other letter reaches the composer; the focus and queue chords do
    /// nothing either. Esc and the quit chords close it, Enter answers
    /// the row even mid-turn.
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
            (
                press(KeyCode::Up, KeyModifiers::ALT),
                roster,
                Action::Nothing,
            ),
            (
                press(KeyCode::Up, KeyModifiers::CONTROL),
                roster,
                Action::Nothing,
            ),
            (
                press(KeyCode::Down, KeyModifiers::CONTROL),
                roster,
                Action::Nothing,
            ),
            (plain(KeyCode::Enter), roster, Action::PaletteAccept),
            (plain(KeyCode::Enter), running, Action::PaletteAccept),
            (plain(KeyCode::Esc), roster, Action::ClosePopup),
            (plain(KeyCode::Esc), running, Action::ClosePopup),
            (ctrl('c'), running, Action::ClosePopup),
            (ctrl('x'), roster, Action::ToggleTodo),
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
            overlay: Some(OverlayKind::Pager(PagerMode::Browse)),
            turn_running: true,
            ..IDLE
        };
        let find = KeyContext {
            overlay: Some(OverlayKind::Pager(PagerMode::Find)),
            ..IDLE
        };
        let picker = KeyContext {
            overlay: Some(OverlayKind::Pager(PagerMode::Picker)),
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
            (ctrl('x'), browse, Action::Nothing),
            (ctrl('g'), browse, Action::Nothing),
            (
                press(KeyCode::Char('t'), KeyModifiers::ALT),
                browse,
                Action::Nothing,
            ),
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

    const TYPING: KeyContext = KeyContext {
        composer_empty: false,
        ..IDLE
    };
    const MULTI_LINE: KeyContext = KeyContext {
        single_line: false,
        ..TYPING
    };
    const SCROLLED: KeyContext = KeyContext {
        scrolled: true,
        ..IDLE
    };
    const FOCUSED: KeyContext = KeyContext {
        focused: true,
        ..IDLE
    };
    const VI_INSERT: KeyContext = KeyContext {
        vi: Some(ViMode::Insert),
        ..IDLE
    };
    const VI_INSERT_TYPING: KeyContext = KeyContext {
        vi: Some(ViMode::Insert),
        ..TYPING
    };
    const RUNNING: KeyContext = KeyContext {
        turn_running: true,
        ..IDLE
    };
    const RUNNING_TYPING: KeyContext = KeyContext {
        turn_running: true,
        ..TYPING
    };
    const RUNNING_FOCUSED: KeyContext = KeyContext {
        turn_running: true,
        ..FOCUSED
    };
    const CONFIRM: KeyContext = KeyContext {
        modal: Some(ModalKind::Confirm {
            default: false,
            tool: true,
        }),
        ..IDLE
    };
    const CONFIRM_NO_TOOL: KeyContext = KeyContext {
        modal: Some(ModalKind::Confirm {
            default: true,
            tool: false,
        }),
        ..IDLE
    };
    const SCROLLED_CONFIRM: KeyContext = KeyContext {
        scrolled: true,
        ..CONFIRM
    };
    const INPUT: KeyContext = KeyContext {
        modal: Some(ModalKind::Input),
        ..IDLE
    };
    const PASSWORD: KeyContext = KeyContext {
        modal: Some(ModalKind::Password),
        ..IDLE
    };
    const SELECT: KeyContext = KeyContext {
        modal: Some(ModalKind::Select {
            custom_field_open: false,
        }),
        ..IDLE
    };
    const CUSTOM_FIELD: KeyContext = KeyContext {
        modal: Some(ModalKind::Select {
            custom_field_open: true,
        }),
        ..IDLE
    };
    const CHECKBOX: KeyContext = KeyContext {
        modal: Some(ModalKind::Checkbox),
        ..IDLE
    };
    const PALETTE: KeyContext = KeyContext {
        popup: Some(PopupKind::Palette),
        ..TYPING
    };
    const RUNNING_PALETTE: KeyContext = KeyContext {
        turn_running: true,
        ..PALETTE
    };
    const SEARCH: KeyContext = KeyContext {
        popup: Some(PopupKind::HistorySearch),
        ..IDLE
    };
    const REWIND: KeyContext = KeyContext {
        popup: Some(PopupKind::Rewind),
        ..IDLE
    };
    const ROSTER: KeyContext = KeyContext {
        popup: Some(PopupKind::Roster),
        ..IDLE
    };
    const RUNNING_ROSTER: KeyContext = KeyContext {
        turn_running: true,
        ..ROSTER
    };
    const VI_NORMAL: KeyContext = KeyContext {
        vi: Some(ViMode::Normal),
        ..TYPING
    };
    const VI_NORMAL_EMPTY: KeyContext = KeyContext {
        vi: Some(ViMode::Normal),
        ..IDLE
    };
    const VI_NORMAL_SCROLLED: KeyContext = KeyContext {
        scrolled: true,
        ..VI_NORMAL_EMPTY
    };
    const BROWSE: KeyContext = KeyContext {
        overlay: Some(OverlayKind::Pager(PagerMode::Browse)),
        ..IDLE
    };
    const FIND: KeyContext = KeyContext {
        overlay: Some(OverlayKind::Pager(PagerMode::Find)),
        ..IDLE
    };
    const PICKER: KeyContext = KeyContext {
        overlay: Some(OverlayKind::Pager(PagerMode::Picker)),
        ..IDLE
    };
    const HELP: KeyContext = KeyContext {
        overlay: Some(OverlayKind::Help),
        ..IDLE
    };
    const RUNNING_HELP: KeyContext = KeyContext {
        overlay: Some(OverlayKind::Help),
        ..RUNNING_TYPING
    };

    /// The contexts the key help is checked in, each with the help
    /// contexts whose rows apply there.
    fn swept_contexts() -> Vec<(KeyContext, Vec<HelpContext>)> {
        use HelpContext as H;
        let composer = [H::Anywhere, H::Transcript, H::Composer, H::ComposerEditing];
        let groups: [(&[KeyContext], Vec<HelpContext>); 10] = [
            (
                &[
                    IDLE,
                    TYPING,
                    MULTI_LINE,
                    SCROLLED,
                    FOCUSED,
                    VI_INSERT,
                    VI_INSERT_TYPING,
                ],
                composer.to_vec(),
            ),
            (
                &[RUNNING, RUNNING_TYPING, RUNNING_FOCUSED],
                [&composer[..], &[H::TurnRunning]].concat(),
            ),
            (
                &[CONFIRM, CONFIRM_NO_TOOL, SCROLLED_CONFIRM, SELECT, CHECKBOX],
                vec![H::Anywhere, H::Transcript, H::Question],
            ),
            (
                &[INPUT, PASSWORD, CUSTOM_FIELD],
                vec![H::Anywhere, H::Transcript, H::Question, H::ComposerEditing],
            ),
            (
                &[PALETTE, RUNNING_PALETTE, SEARCH, REWIND],
                [&composer[..], &[H::Bands]].concat(),
            ),
            (
                &[ROSTER, RUNNING_ROSTER],
                vec![H::Anywhere, H::Transcript, H::Roster],
            ),
            (
                &[VI_NORMAL, VI_NORMAL_EMPTY, VI_NORMAL_SCROLLED],
                [&composer[..], &[H::ViNormal]].concat(),
            ),
            (&[BROWSE], vec![H::Anywhere, H::Pager]),
            (&[FIND], vec![H::Anywhere, H::PagerFind]),
            (&[PICKER], vec![H::Anywhere, H::ThreadPicker]),
        ];
        let mut swept: Vec<(KeyContext, Vec<HelpContext>)> = groups
            .into_iter()
            .flat_map(|(contexts, applies)| contexts.iter().map(move |ctx| (*ctx, applies.clone())))
            .collect();
        for ctx in [HELP, RUNNING_HELP] {
            swept.push((ctx, vec![H::Anywhere, H::KeyHelp]));
        }
        swept
    }

    type Example = (KeyEvent, KeyContext, Action);

    /// Every row of `KEY_HELP` in order, replayed: each key it names in a
    /// swept context, with the action that key must map to there. A
    /// composer editing key must reach the text area untouched; a mouse
    /// wheel row has no key to replay.
    fn help_examples() -> Vec<(HelpContext, &'static str, Vec<Example>)> {
        use HelpContext as H;
        let letter = |c: char| plain(KeyCode::Char(c));
        let shift = |code: KeyCode| press(code, KeyModifiers::SHIFT);
        let alt = |code: KeyCode| press(code, KeyModifiers::ALT);
        let control = |code: KeyCode| press(code, KeyModifiers::CONTROL);
        let typed = |key: KeyEvent, ctx: KeyContext| (key, ctx, Action::ComposerInput(key));
        let cancelled = || Action::Answer(PromptAnswer::Cancelled);
        vec![
            (
                H::Anywhere,
                "Ctrl+Z",
                vec![
                    (ctrl('z'), IDLE, Action::Suspend),
                    (ctrl('z'), BROWSE, Action::Suspend),
                    (ctrl('z'), HELP, Action::Suspend),
                ],
            ),
            (
                H::Anywhere,
                "Ctrl+L",
                vec![
                    (ctrl('l'), CONFIRM, Action::ScheduleFrame),
                    (ctrl('l'), HELP, Action::ScheduleFrame),
                ],
            ),
            (
                H::Transcript,
                "Ctrl+X",
                vec![
                    (ctrl('x'), TYPING, Action::ToggleTodo),
                    (ctrl('x'), CONFIRM, Action::ToggleTodo),
                ],
            ),
            (
                H::Transcript,
                "PgUp / PgDn",
                vec![
                    (plain(KeyCode::PageUp), IDLE, Action::ScrollPage(-1)),
                    (plain(KeyCode::PageDown), INPUT, Action::ScrollPage(1)),
                ],
            ),
            (
                H::Transcript,
                "Shift+Up / Shift+Down",
                vec![
                    (shift(KeyCode::Up), IDLE, Action::ScrollLines(-1)),
                    (shift(KeyCode::Down), CONFIRM, Action::ScrollLines(1)),
                ],
            ),
            (
                H::Transcript,
                "End",
                vec![
                    (plain(KeyCode::End), SCROLLED, Action::ScrollToBottom),
                    (
                        plain(KeyCode::End),
                        SCROLLED_CONFIRM,
                        Action::ScrollToBottom,
                    ),
                ],
            ),
            (H::Transcript, "Mouse wheel", vec![]),
            (
                H::Composer,
                "Enter",
                vec![
                    (plain(KeyCode::Enter), TYPING, Action::Submit),
                    (plain(KeyCode::Enter), FOCUSED, Action::ToggleFocused),
                ],
            ),
            (
                H::Composer,
                "Shift+Enter / Alt+Enter / Ctrl+J",
                vec![
                    (shift(KeyCode::Enter), TYPING, Action::InsertNewline),
                    (alt(KeyCode::Enter), TYPING, Action::InsertNewline),
                    (ctrl('j'), TYPING, Action::InsertNewline),
                ],
            ),
            (
                H::Composer,
                "Up / Down",
                vec![
                    (plain(KeyCode::Up), IDLE, Action::HistoryPrev),
                    (plain(KeyCode::Down), IDLE, Action::HistoryNext),
                ],
            ),
            (
                H::Composer,
                "Ctrl+R",
                vec![(ctrl('r'), IDLE, Action::OpenHistorySearch)],
            ),
            (
                H::Composer,
                "Alt+Up",
                vec![(alt(KeyCode::Up), IDLE, Action::EditQueued)],
            ),
            (
                H::Composer,
                "Ctrl+O",
                vec![(ctrl('o'), IDLE, Action::OpenEditor)],
            ),
            (
                H::Composer,
                "Ctrl+T",
                vec![(ctrl('t'), IDLE, Action::OpenPager)],
            ),
            (
                H::Composer,
                "Alt+T",
                vec![(
                    press(KeyCode::Char('t'), KeyModifiers::ALT),
                    IDLE,
                    Action::ToggleRoster,
                )],
            ),
            (
                H::Composer,
                "?",
                vec![
                    (letter('?'), IDLE, Action::ToggleHelp),
                    (
                        shift(KeyCode::Char('?')),
                        VI_NORMAL_EMPTY,
                        Action::ToggleHelp,
                    ),
                ],
            ),
            (
                H::Composer,
                "Ctrl+Up / Ctrl+Down",
                vec![
                    (control(KeyCode::Up), IDLE, Action::FocusPrev),
                    (control(KeyCode::Down), IDLE, Action::FocusNext),
                ],
            ),
            (
                H::Composer,
                "Shift+G",
                vec![(shift(KeyCode::Char('G')), SCROLLED, Action::ScrollToBottom)],
            ),
            (
                H::Composer,
                "Esc",
                vec![
                    (plain(KeyCode::Esc), FOCUSED, Action::ClearFocus),
                    (plain(KeyCode::Esc), VI_INSERT, Action::ViNormal),
                    (plain(KeyCode::Esc), IDLE, Action::IdleEsc),
                ],
            ),
            (
                H::Composer,
                "Ctrl+C",
                vec![
                    (ctrl('c'), TYPING, Action::ClearComposer),
                    (ctrl('c'), IDLE, Action::Quit),
                ],
            ),
            (
                H::Composer,
                "Ctrl+D",
                vec![(ctrl('d'), IDLE, Action::Quit), typed(ctrl('d'), TYPING)],
            ),
            (
                H::ComposerEditing,
                "Left / Right / Ctrl+F",
                vec![
                    typed(plain(KeyCode::Left), TYPING),
                    typed(plain(KeyCode::Right), MULTI_LINE),
                    typed(ctrl('f'), TYPING),
                ],
            ),
            (
                H::ComposerEditing,
                "Up / Down / Ctrl+P / Ctrl+N",
                vec![
                    typed(plain(KeyCode::Up), MULTI_LINE),
                    typed(plain(KeyCode::Down), MULTI_LINE),
                    typed(ctrl('p'), MULTI_LINE),
                    typed(ctrl('n'), MULTI_LINE),
                ],
            ),
            (
                H::ComposerEditing,
                "Ctrl+Left / Ctrl+Right / Alt+B / Alt+F",
                vec![
                    typed(control(KeyCode::Left), TYPING),
                    typed(control(KeyCode::Right), TYPING),
                    typed(alt(KeyCode::Char('b')), TYPING),
                    typed(alt(KeyCode::Char('f')), MULTI_LINE),
                ],
            ),
            (
                H::ComposerEditing,
                "Alt+[ / Alt+] / Alt+P / Alt+N",
                vec![
                    typed(alt(KeyCode::Char('[')), MULTI_LINE),
                    typed(alt(KeyCode::Char(']')), MULTI_LINE),
                    typed(alt(KeyCode::Char('p')), MULTI_LINE),
                    typed(alt(KeyCode::Char('n')), MULTI_LINE),
                ],
            ),
            (
                H::ComposerEditing,
                "Home / End / Ctrl+E",
                vec![
                    typed(plain(KeyCode::Home), TYPING),
                    typed(plain(KeyCode::End), TYPING),
                    typed(ctrl('e'), MULTI_LINE),
                ],
            ),
            (
                H::ComposerEditing,
                "Alt+< / Alt+>",
                vec![
                    typed(alt(KeyCode::Char('<')), MULTI_LINE),
                    typed(alt(KeyCode::Char('>')), MULTI_LINE),
                ],
            ),
            (
                H::ComposerEditing,
                "Alt+V / Ctrl+V",
                vec![
                    typed(alt(KeyCode::Char('v')), MULTI_LINE),
                    typed(ctrl('v'), MULTI_LINE),
                ],
            ),
            (
                H::ComposerEditing,
                "Tab",
                vec![typed(plain(KeyCode::Tab), TYPING)],
            ),
            (
                H::ComposerEditing,
                "Backspace / Delete",
                vec![
                    typed(plain(KeyCode::Backspace), TYPING),
                    typed(plain(KeyCode::Delete), TYPING),
                ],
            ),
            (
                H::ComposerEditing,
                "Ctrl+W / Alt+Backspace",
                vec![
                    typed(ctrl('w'), TYPING),
                    typed(alt(KeyCode::Backspace), TYPING),
                ],
            ),
            (
                H::ComposerEditing,
                "Alt+D / Alt+Delete",
                vec![
                    typed(alt(KeyCode::Char('d')), TYPING),
                    typed(alt(KeyCode::Delete), TYPING),
                ],
            ),
            (H::ComposerEditing, "Ctrl+K", vec![typed(ctrl('k'), TYPING)]),
            (
                H::ComposerEditing,
                "Ctrl+U",
                vec![
                    (ctrl('u'), MULTI_LINE, Action::DeleteToLineHead),
                    (ctrl('u'), INPUT, Action::DeleteToLineHead),
                ],
            ),
            (H::ComposerEditing, "Ctrl+Y", vec![typed(ctrl('y'), IDLE)]),
            (
                H::ComposerEditing,
                "Ctrl+_ / Ctrl+/ / Ctrl+7",
                vec![
                    (control(KeyCode::Char('_')), TYPING, Action::Undo),
                    (control(KeyCode::Char('/')), VI_INSERT_TYPING, Action::Undo),
                    (control(KeyCode::Char('7')), CUSTOM_FIELD, Action::Undo),
                    (kitty_ctrl_underscore(), TYPING, Action::Undo),
                    (kitty_ctrl_underscore(), INPUT, Action::Undo),
                ],
            ),
            (
                H::ComposerEditing,
                "Shift+Left / Shift+Right",
                vec![
                    typed(shift(KeyCode::Left), TYPING),
                    typed(shift(KeyCode::Right), MULTI_LINE),
                ],
            ),
            (
                H::TurnRunning,
                "Esc / Ctrl+C",
                vec![
                    (plain(KeyCode::Esc), RUNNING, Action::Interrupt),
                    (ctrl('c'), RUNNING_TYPING, Action::Interrupt),
                ],
            ),
            (
                H::TurnRunning,
                "Enter",
                vec![(plain(KeyCode::Enter), RUNNING_TYPING, Action::QueueInput)],
            ),
            (
                H::Question,
                "y / n",
                vec![
                    (
                        letter('y'),
                        CONFIRM,
                        Action::Answer(PromptAnswer::Confirmed(true)),
                    ),
                    (
                        letter('n'),
                        CONFIRM,
                        Action::Answer(PromptAnswer::Confirmed(false)),
                    ),
                ],
            ),
            (
                H::Question,
                "a",
                vec![(letter('a'), CONFIRM, Action::AnswerYesForSession)],
            ),
            (
                H::Question,
                "Enter",
                vec![
                    (
                        plain(KeyCode::Enter),
                        CONFIRM,
                        Action::Answer(PromptAnswer::Confirmed(false)),
                    ),
                    (
                        plain(KeyCode::Enter),
                        CONFIRM_NO_TOOL,
                        Action::Answer(PromptAnswer::Confirmed(true)),
                    ),
                    (plain(KeyCode::Enter), INPUT, Action::SubmitModalInput),
                    (plain(KeyCode::Enter), SELECT, Action::SubmitModalChoice),
                ],
            ),
            (
                H::Question,
                "Up / Down / j / k",
                vec![
                    (plain(KeyCode::Up), SELECT, Action::ModalMove(-1)),
                    (plain(KeyCode::Down), CHECKBOX, Action::ModalMove(1)),
                    (letter('j'), SELECT, Action::ModalMove(1)),
                    (letter('k'), CHECKBOX, Action::ModalMove(-1)),
                ],
            ),
            (
                H::Question,
                "Space",
                vec![(letter(' '), CHECKBOX, Action::ModalToggle)],
            ),
            (
                H::Question,
                "1-9",
                (1..=9u8)
                    .map(|n| {
                        (
                            letter(char::from(b'0' + n)),
                            SELECT,
                            Action::ModalJump(usize::from(n - 1)),
                        )
                    })
                    .collect(),
            ),
            (
                H::Question,
                "Esc",
                vec![
                    (plain(KeyCode::Esc), PASSWORD, cancelled()),
                    (plain(KeyCode::Esc), CONFIRM, cancelled()),
                    (plain(KeyCode::Esc), CHECKBOX, cancelled()),
                ],
            ),
            (
                H::Question,
                "Ctrl+C",
                vec![
                    (
                        ctrl('c'),
                        CUSTOM_FIELD,
                        Action::Answer(PromptAnswer::Interrupted),
                    ),
                    (
                        ctrl('c'),
                        CONFIRM,
                        Action::Answer(PromptAnswer::Interrupted),
                    ),
                    (ctrl('c'), SELECT, Action::Answer(PromptAnswer::Interrupted)),
                ],
            ),
            (
                H::Bands,
                "Up / Down / Tab / Shift+Tab",
                vec![
                    (plain(KeyCode::Up), PALETTE, Action::PaletteMove(-1)),
                    (plain(KeyCode::Down), SEARCH, Action::PaletteMove(1)),
                    (plain(KeyCode::Tab), PALETTE, Action::PaletteMove(1)),
                    (plain(KeyCode::BackTab), REWIND, Action::PaletteMove(-1)),
                ],
            ),
            (
                H::Bands,
                "Enter",
                vec![
                    (plain(KeyCode::Enter), PALETTE, Action::PaletteAccept),
                    (plain(KeyCode::Enter), SEARCH, Action::PaletteAccept),
                ],
            ),
            (
                H::Bands,
                "Backspace",
                vec![(plain(KeyCode::Backspace), SEARCH, Action::PopupBackspace)],
            ),
            (
                H::Bands,
                "Esc / Ctrl+C / Ctrl+D",
                vec![
                    (plain(KeyCode::Esc), PALETTE, Action::ClosePopup),
                    (ctrl('c'), RUNNING_PALETTE, Action::ClosePopup),
                    (ctrl('d'), REWIND, Action::ClosePopup),
                ],
            ),
            (
                H::Roster,
                "j / k / Up / Down / Tab / Shift+Tab",
                vec![
                    (letter('j'), ROSTER, Action::PaletteMove(1)),
                    (letter('k'), ROSTER, Action::PaletteMove(-1)),
                    (plain(KeyCode::Up), ROSTER, Action::PaletteMove(-1)),
                    (plain(KeyCode::Down), ROSTER, Action::PaletteMove(1)),
                    (plain(KeyCode::Tab), ROSTER, Action::PaletteMove(1)),
                    (plain(KeyCode::BackTab), ROSTER, Action::PaletteMove(-1)),
                ],
            ),
            (
                H::Roster,
                "x",
                vec![(letter('x'), RUNNING_ROSTER, Action::RosterCancel)],
            ),
            (
                H::Roster,
                "o",
                vec![(letter('o'), ROSTER, Action::RosterOpen)],
            ),
            (
                H::Roster,
                "Enter",
                vec![(plain(KeyCode::Enter), RUNNING_ROSTER, Action::PaletteAccept)],
            ),
            (
                H::Roster,
                "Ctrl+T",
                vec![(ctrl('t'), ROSTER, Action::OpenPager)],
            ),
            (
                H::Roster,
                "Esc / Ctrl+C / Ctrl+D / Alt+T",
                vec![
                    (plain(KeyCode::Esc), ROSTER, Action::ClosePopup),
                    (ctrl('c'), RUNNING_ROSTER, Action::ClosePopup),
                    (ctrl('d'), ROSTER, Action::ClosePopup),
                    (
                        press(KeyCode::Char('t'), KeyModifiers::ALT),
                        ROSTER,
                        Action::ToggleRoster,
                    ),
                ],
            ),
            (
                H::ViNormal,
                "h / j / k / l",
                vec![
                    (letter('h'), VI_NORMAL, Action::ViMotion(ViMotion::Back)),
                    (letter('j'), VI_NORMAL, Action::ViMotion(ViMotion::Down)),
                    (letter('k'), VI_NORMAL, Action::ViMotion(ViMotion::Up)),
                    (letter('l'), VI_NORMAL, Action::ViMotion(ViMotion::Forward)),
                ],
            ),
            (
                H::ViNormal,
                "w / b",
                vec![
                    (
                        letter('w'),
                        VI_NORMAL,
                        Action::ViMotion(ViMotion::WordForward),
                    ),
                    (letter('b'), VI_NORMAL, Action::ViMotion(ViMotion::WordBack)),
                ],
            ),
            (
                H::ViNormal,
                "0 / $",
                vec![
                    (letter('0'), VI_NORMAL, Action::ViMotion(ViMotion::Head)),
                    (letter('$'), VI_NORMAL, Action::ViMotion(ViMotion::End)),
                ],
            ),
            (
                H::ViNormal,
                "x",
                vec![(letter('x'), VI_NORMAL, Action::ViDeleteChar)],
            ),
            (
                H::ViNormal,
                "dd",
                vec![(letter('d'), VI_NORMAL, Action::ViDeleteLine)],
            ),
            (
                H::ViNormal,
                "i / a / A",
                vec![
                    (letter('i'), VI_NORMAL, Action::ViInsert { append: None }),
                    (
                        letter('a'),
                        VI_NORMAL,
                        Action::ViInsert {
                            append: Some(ViMotion::Forward),
                        },
                    ),
                    (
                        shift(KeyCode::Char('A')),
                        VI_NORMAL,
                        Action::ViInsert {
                            append: Some(ViMotion::End),
                        },
                    ),
                ],
            ),
            (
                H::Pager,
                "j / k / Up / Down",
                vec![
                    (letter('j'), BROWSE, Action::PagerScroll(1)),
                    (letter('k'), BROWSE, Action::PagerScroll(-1)),
                    (plain(KeyCode::Up), BROWSE, Action::PagerScroll(-1)),
                    (plain(KeyCode::Down), BROWSE, Action::PagerScroll(1)),
                ],
            ),
            (
                H::Pager,
                "Space / PgDn / PgUp",
                vec![
                    (letter(' '), BROWSE, Action::PagerPage(1)),
                    (plain(KeyCode::PageDown), BROWSE, Action::PagerPage(1)),
                    (plain(KeyCode::PageUp), BROWSE, Action::PagerPage(-1)),
                ],
            ),
            (
                H::Pager,
                "g / Home / G / End",
                vec![
                    (letter('g'), BROWSE, Action::PagerTop),
                    (plain(KeyCode::Home), BROWSE, Action::PagerTop),
                    (letter('G'), BROWSE, Action::PagerBottom),
                    (plain(KeyCode::End), BROWSE, Action::PagerBottom),
                ],
            ),
            (
                H::Pager,
                "/",
                vec![(letter('/'), BROWSE, Action::PagerFindOpen)],
            ),
            (
                H::Pager,
                "n / N",
                vec![
                    (letter('n'), BROWSE, Action::PagerFindNext { forward: true }),
                    (
                        letter('N'),
                        BROWSE,
                        Action::PagerFindNext { forward: false },
                    ),
                ],
            ),
            (
                H::Pager,
                "Tab",
                vec![(plain(KeyCode::Tab), BROWSE, Action::OpenThreadPicker)],
            ),
            (H::Pager, "Mouse wheel", vec![]),
            (
                H::Pager,
                "q / Esc / Ctrl+C / Ctrl+D / Ctrl+T",
                vec![
                    (letter('q'), BROWSE, Action::ClosePager),
                    (plain(KeyCode::Esc), BROWSE, Action::ClosePager),
                    (ctrl('c'), BROWSE, Action::ClosePager),
                    (ctrl('d'), BROWSE, Action::ClosePager),
                    (ctrl('t'), BROWSE, Action::ClosePager),
                ],
            ),
            (
                H::PagerFind,
                "Enter",
                vec![(plain(KeyCode::Enter), FIND, Action::PagerFindSubmit)],
            ),
            (
                H::PagerFind,
                "Backspace",
                vec![(plain(KeyCode::Backspace), FIND, Action::PagerFindBackspace)],
            ),
            (
                H::PagerFind,
                "Esc / Ctrl+C",
                vec![
                    (plain(KeyCode::Esc), FIND, Action::PagerFindCancel),
                    (ctrl('c'), FIND, Action::PagerFindCancel),
                ],
            ),
            (
                H::ThreadPicker,
                "j / k / Up / Down",
                vec![
                    (letter('j'), PICKER, Action::ThreadPickerMove(1)),
                    (letter('k'), PICKER, Action::ThreadPickerMove(-1)),
                    (plain(KeyCode::Up), PICKER, Action::ThreadPickerMove(-1)),
                    (plain(KeyCode::Down), PICKER, Action::ThreadPickerMove(1)),
                ],
            ),
            (
                H::ThreadPicker,
                "Enter",
                vec![(plain(KeyCode::Enter), PICKER, Action::ThreadPickerChoose)],
            ),
            (
                H::ThreadPicker,
                "q / Esc / Tab / Ctrl+C / Ctrl+D",
                vec![
                    (letter('q'), PICKER, Action::CloseThreadPicker),
                    (plain(KeyCode::Esc), PICKER, Action::CloseThreadPicker),
                    (plain(KeyCode::Tab), PICKER, Action::CloseThreadPicker),
                    (ctrl('c'), PICKER, Action::CloseThreadPicker),
                    (ctrl('d'), PICKER, Action::CloseThreadPicker),
                ],
            ),
            (
                H::KeyHelp,
                "j / k / Up / Down",
                vec![
                    (letter('j'), HELP, Action::HelpScroll(1)),
                    (letter('k'), HELP, Action::HelpScroll(-1)),
                    (plain(KeyCode::Up), RUNNING_HELP, Action::HelpScroll(-1)),
                    (plain(KeyCode::Down), HELP, Action::HelpScroll(1)),
                ],
            ),
            (
                H::KeyHelp,
                "PgUp / PgDn",
                vec![
                    (plain(KeyCode::PageUp), HELP, Action::HelpPage(-1)),
                    (plain(KeyCode::PageDown), HELP, Action::HelpPage(1)),
                ],
            ),
            (H::KeyHelp, "Mouse wheel", vec![]),
            (
                H::KeyHelp,
                "? / q / Esc / Ctrl+C / Ctrl+D",
                vec![
                    (letter('?'), RUNNING_HELP, Action::ToggleHelp),
                    (letter('q'), HELP, Action::ToggleHelp),
                    (plain(KeyCode::Esc), RUNNING_HELP, Action::ToggleHelp),
                    (ctrl('c'), HELP, Action::ToggleHelp),
                    (ctrl('d'), RUNNING_HELP, Action::ToggleHelp),
                ],
            ),
        ]
    }

    /// Typing handed on to a field, a filter or the composer, or a key
    /// swallowed: what an unbound key produces.
    fn passes_through(action: &Action) -> bool {
        matches!(
            action,
            Action::ComposerInput(_)
                | Action::ModalInput(_)
                | Action::PopupChar(_)
                | Action::PagerFindChar(_)
                | Action::Nothing
        )
    }

    /// Every printable ASCII key and every named key, function keys
    /// included, plain and under each modifier.
    fn key_universe() -> Vec<KeyEvent> {
        let modifiers = [
            KeyModifiers::NONE,
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ];
        let mut codes: Vec<KeyCode> = (b' '..=b'~')
            .map(|byte| KeyCode::Char(char::from(byte)))
            .collect();
        codes.extend([
            KeyCode::Esc,
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::BackTab,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ]);
        codes.extend((1..=12).map(KeyCode::F));
        codes
            .into_iter()
            .flat_map(|code| modifiers.map(|modifiers| press(code, modifiers)))
            .collect()
    }

    /// The help's own name for a key, as a label token spells it: the
    /// letter case-folded, Ctrl and Alt kept, and Shift kept only on a
    /// named key, since on a character it is the case or the symbol.
    /// The kitty protocol's Ctrl+Shift+- is the Ctrl+_ the help names.
    fn normalise(key: KeyEvent) -> (KeyCode, KeyModifiers) {
        if key == kitty_ctrl_underscore() {
            return (KeyCode::Char('_'), KeyModifiers::CONTROL);
        }
        let code = lowercase(key.code);
        let mut modifiers =
            key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        if matches!(code, KeyCode::Char(_) | KeyCode::BackTab) {
            modifiers.remove(KeyModifiers::SHIFT);
        }
        (code, modifiers)
    }

    /// The keys a help row's label names, one token per ` / `-separated
    /// name; panics on a name it cannot read.
    fn label_keys(label: &str) -> Vec<(KeyCode, KeyModifiers)> {
        let mut keys = Vec::new();
        for token in label.split(" / ") {
            match token {
                "1-9" => keys.extend(('1'..='9').map(|c| (KeyCode::Char(c), KeyModifiers::NONE))),
                "dd" => keys.push((KeyCode::Char('d'), KeyModifiers::NONE)),
                "Shift+Tab" => keys.push((KeyCode::BackTab, KeyModifiers::NONE)),
                _ => {
                    let mut name = token;
                    let mut modifiers = KeyModifiers::NONE;
                    loop {
                        if let Some(rest) = name.strip_prefix("Ctrl+") {
                            modifiers |= KeyModifiers::CONTROL;
                            name = rest;
                        } else if let Some(rest) = name.strip_prefix("Alt+") {
                            modifiers |= KeyModifiers::ALT;
                            name = rest;
                        } else if let Some(rest) = name.strip_prefix("Shift+") {
                            modifiers |= KeyModifiers::SHIFT;
                            name = rest;
                        } else {
                            break;
                        }
                    }
                    let code = match name {
                        "Up" => KeyCode::Up,
                        "Down" => KeyCode::Down,
                        "Left" => KeyCode::Left,
                        "Right" => KeyCode::Right,
                        "Home" => KeyCode::Home,
                        "End" => KeyCode::End,
                        "PgUp" => KeyCode::PageUp,
                        "PgDn" => KeyCode::PageDown,
                        "Esc" => KeyCode::Esc,
                        "Enter" => KeyCode::Enter,
                        "Tab" => KeyCode::Tab,
                        "Space" => KeyCode::Char(' '),
                        "Backspace" => KeyCode::Backspace,
                        "Delete" => KeyCode::Delete,
                        _ => {
                            let mut chars = name.chars();
                            match (chars.next(), chars.next()) {
                                (Some(c), None) => KeyCode::Char(c),
                                _ => panic!("{label:?}: no key is called {token:?}"),
                            }
                        }
                    };
                    keys.push(normalise(press(code, modifiers)));
                }
            }
        }
        keys
    }

    /// `key` with each modifier the key map ignores for it in `ctx`
    /// taken off: Alt+Esc does what Esc does, so the help need not name
    /// it.
    fn without_ignored_modifiers(key: KeyEvent, ctx: &KeyContext) -> KeyEvent {
        let action = map_key(key, ctx);
        let mut key = key;
        for modifier in [
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
        ] {
            if key.modifiers.contains(modifier) {
                let bare = press(key.code, key.modifiers - modifier);
                if map_key(bare, ctx) == action {
                    key = bare;
                }
            }
        }
        key
    }

    /// Each help row names a key that, in a context the row applies to,
    /// does what the row says, and no row documents a key that does
    /// nothing. A composer editing key must reach the text area as
    /// typed, or be one of the edits the app makes on it itself; any
    /// other row may also hand its key to the text area, as Ctrl+D does
    /// with a draft, so long as it binds the key somewhere.
    #[test]
    fn every_help_row_replays_through_the_key_map() {
        let swept = swept_contexts();
        let examples = help_examples();
        assert_eq!(examples.len(), KEY_HELP.len(), "one replay per help row");
        for (row, (context, keys, replays)) in KEY_HELP.iter().zip(&examples) {
            assert_eq!((row.context, row.keys), (*context, *keys), "{row:?}");
            assert_eq!(
                replays.is_empty(),
                row.keys == "Mouse wheel",
                "{row:?}: only a mouse wheel row replays no key"
            );
            if row.context != HelpContext::ComposerEditing && !replays.is_empty() {
                assert!(
                    replays.iter().any(|(_, _, action)| !passes_through(action)),
                    "{row:?} binds no key"
                );
            }
            for (key, ctx, expected) in replays {
                assert!(
                    swept
                        .iter()
                        .any(|(swept, applies)| swept == ctx && applies.contains(&row.context)),
                    "{row:?}: {ctx:?} is not a swept context the row applies to"
                );
                if passes_through(expected) {
                    assert_eq!(*expected, Action::ComposerInput(*key), "{row:?}");
                } else if row.context == HelpContext::ComposerEditing {
                    assert!(
                        matches!(expected, Action::DeleteToLineHead | Action::Undo),
                        "{row:?}: {expected:?} is not a text edit"
                    );
                }
                assert_eq!(map_key(*key, ctx), *expected, "{row:?}: {key:?} in {ctx:?}");
            }
        }
    }

    /// Each row's label names exactly the keys its replays press: no
    /// label token goes unreplayed and no replayed key goes unnamed.
    #[test]
    fn every_help_label_names_the_keys_its_row_replays() {
        for (row, (_, _, replays)) in KEY_HELP.iter().zip(help_examples()) {
            if row.keys == "Mouse wheel" {
                continue;
            }
            let named = label_keys(row.keys);
            let replayed: Vec<(KeyCode, KeyModifiers)> =
                replays.iter().map(|(key, _, _)| normalise(*key)).collect();
            for key in &named {
                assert!(
                    replayed.contains(key),
                    "{row:?} names {key:?}, never replayed"
                );
            }
            for key in &replayed {
                assert!(named.contains(key), "{row:?} replays {key:?}, not named");
            }
        }
    }

    /// A draft primed with something to undo and to paste, its cursor
    /// inside a word mid-way down, laid out two rows high so a page
    /// step has somewhere to go.
    fn primed_composer() -> Composer {
        let mut composer = Composer::new(Color::Reset, CommandPrefix::Dot);
        composer.set_text("one two three\n\nfour five six\nseven eight nine\n\nten eleven");
        composer.move_cursor(ViMotion::Up);
        composer.move_cursor(ViMotion::Up);
        composer.move_cursor(ViMotion::Head);
        composer.move_cursor(ViMotion::WordForward);
        composer.input(ctrl('k'));
        composer.move_cursor(ViMotion::Back);
        let area = Rect::new(0, 0, 20, 3);
        composer
            .view(CommandPrefix::Dot, &[], &Theme::fallback(true), false)
            .render(area, &mut Buffer::empty(area));
        composer
    }

    /// Every key a help row hands to the text area edits the draft or
    /// moves its cursor there, so no row names a key the text area
    /// ignores; the edits the app makes itself are applied the same
    /// way. A marker typed after the key shows where the cursor went.
    #[test]
    fn every_key_the_help_hands_to_the_text_area_changes_the_draft() {
        let marker = plain(KeyCode::Char('|'));
        let mut unmoved = primed_composer();
        unmoved.input(marker);
        for (context, keys, replays) in help_examples() {
            for (key, _, action) in replays {
                let mut composer = primed_composer();
                match action {
                    Action::ComposerInput(typed) if typed == key => composer.input(key),
                    Action::DeleteToLineHead => composer.delete_line_by_head(),
                    Action::Undo => composer.undo(),
                    _ => continue,
                }
                composer.input(marker);
                assert_ne!(
                    composer.text(),
                    unmoved.text(),
                    "{context:?} {keys:?}: {key:?} left the draft and cursor alone"
                );
            }
        }
    }

    /// Alt+H reaches neither the composer's nor a question field's text
    /// area, which would cut the word before the cursor.
    #[test]
    fn alt_h_cuts_nothing() {
        let key = press(KeyCode::Char('h'), KeyModifiers::ALT);
        assert_eq!(map_key(key, &TYPING), Action::Nothing);
        for modal in [ModalKind::Input, ModalKind::Password] {
            let ctx = KeyContext {
                modal: Some(modal),
                ..IDLE
            };
            assert_eq!(map_key(key, &ctx), Action::Nothing, "{modal:?}");
        }
    }

    /// Every binding the key map reaches, in every swept context, is
    /// that key with that action among the replays of a help row
    /// applying there; modifiers the key map ignores for the key do not
    /// count as a different key.
    #[test]
    fn every_binding_the_key_map_reaches_has_a_help_row() {
        let examples = help_examples();
        let mut missing = Vec::new();
        for (ctx, applies) in swept_contexts() {
            let documented: Vec<((KeyCode, KeyModifiers), &Action)> = examples
                .iter()
                .filter(|(context, _, _)| applies.contains(context))
                .flat_map(|(_, _, replays)| {
                    replays
                        .iter()
                        .map(|(key, _, action)| (normalise(*key), action))
                })
                .collect();
            for key in key_universe() {
                let action = map_key(key, &ctx);
                if passes_through(&action) {
                    continue;
                }
                let named = normalise(without_ignored_modifiers(key, &ctx));
                if !documented.contains(&(named, &action)) {
                    missing.push(format!("{key:?} in {ctx:?} -> {action:?}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "bindings no help row names:\n{}",
            missing.join("\n")
        );
    }

    /// The help shows one heading per context, so each context's rows
    /// sit together.
    #[test]
    fn help_rows_are_grouped_by_context() {
        let mut seen: Vec<HelpContext> = Vec::new();
        for row in KEY_HELP {
            if seen.last() != Some(&row.context) {
                assert!(!seen.contains(&row.context), "{row:?} is out of its group");
                seen.push(row.context);
            }
        }
    }

    /// Terminal multiplexers own the function keys, so none is bound:
    /// each produces what any unbound key does.
    #[test]
    fn function_keys_are_unbound_everywhere() {
        for (ctx, _) in swept_contexts() {
            for n in 1..=12 {
                for modifiers in [
                    KeyModifiers::NONE,
                    KeyModifiers::SHIFT,
                    KeyModifiers::CONTROL,
                    KeyModifiers::ALT,
                ] {
                    let key = press(KeyCode::F(n), modifiers);
                    let action = map_key(key, &ctx);
                    assert!(passes_through(&action), "{key:?} in {ctx:?} -> {action:?}");
                }
            }
        }
    }

    /// `?` opens the help only on an empty composer with nothing over
    /// it; elsewhere it stays typing, or whatever the context makes of
    /// an unbound letter.
    #[test]
    fn question_mark_opens_the_help_only_on_a_bare_empty_composer() {
        let mark = plain(KeyCode::Char('?'));
        let shifted = press(KeyCode::Char('?'), KeyModifiers::SHIFT);
        let alt_mark = press(KeyCode::Char('?'), KeyModifiers::ALT);
        let cases = [
            (mark, IDLE, Action::ToggleHelp),
            (shifted, IDLE, Action::ToggleHelp),
            (mark, RUNNING, Action::ToggleHelp),
            (mark, FOCUSED, Action::ToggleHelp),
            (mark, VI_INSERT, Action::ToggleHelp),
            (mark, VI_NORMAL_EMPTY, Action::ToggleHelp),
            (mark, TYPING, Action::ComposerInput(mark)),
            (mark, RUNNING_TYPING, Action::ComposerInput(mark)),
            (alt_mark, IDLE, Action::ComposerInput(alt_mark)),
            (mark, VI_NORMAL, Action::Nothing),
            (mark, PALETTE, Action::ComposerInput(mark)),
            (mark, REWIND, Action::ComposerInput(mark)),
            (mark, SEARCH, Action::PopupChar('?')),
            (mark, ROSTER, Action::Nothing),
            (mark, CONFIRM, Action::Nothing),
            (mark, INPUT, Action::ModalInput(mark)),
            (mark, BROWSE, Action::Nothing),
            (mark, FIND, Action::PagerFindChar('?')),
            (mark, HELP, Action::ToggleHelp),
        ];
        for (key, ctx, expected) in cases {
            assert_eq!(map_key(key, &ctx), expected, "{key:?} in {ctx:?}");
        }
    }

    /// The status strip shows `? = keys` by `question_mark_opens_help`,
    /// so on the composer that test and the key map must agree.
    #[test]
    fn the_question_mark_guard_matches_the_key_map() {
        let mark = plain(KeyCode::Char('?'));
        let cases = [
            (IDLE, true),
            (TYPING, false),
            (SEARCH, false),
            (PALETTE, false),
            (CONFIRM, false),
            (INPUT, false),
            (BROWSE, false),
        ];
        for (ctx, opens) in cases {
            assert_eq!(ctx.question_mark_opens_help(), opens, "{ctx:?}");
            let toggles = map_key(mark, &ctx) == Action::ToggleHelp;
            assert_eq!(toggles, opens, "{ctx:?}");
        }
        assert!(!HELP.question_mark_opens_help());
    }

    /// Every chord ratatui-textarea 0.9.4 binds in `TextArea::input`, by
    /// name: typed characters, the mouse and the clipboard keys aside.
    /// Shift extends a selection with the named keys that move.
    fn text_area_chords() -> Vec<(String, KeyEvent)> {
        let alt = |code: KeyCode| press(code, KeyModifiers::ALT);
        let control = |code: KeyCode| press(code, KeyModifiers::CONTROL);
        let ctrl_alt = |code: KeyCode| press(code, KeyModifiers::CONTROL | KeyModifiers::ALT);
        let mut chords: Vec<KeyEvent> = "mhdkjwnpfbaeuryxcv".chars().map(ctrl).collect();
        chords.extend("hdnpfb<>][v".chars().map(|c| alt(KeyCode::Char(c))));
        chords.extend([
            plain(KeyCode::Enter),
            plain(KeyCode::Tab),
            alt(KeyCode::Backspace),
            alt(KeyCode::Delete),
            plain(KeyCode::Backspace),
            plain(KeyCode::Delete),
        ]);
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
        ] {
            chords.push(plain(code));
            chords.push(press(code, KeyModifiers::SHIFT));
        }
        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Left, KeyCode::Right] {
            chords.push(control(code));
            chords.push(ctrl_alt(code));
        }
        chords.extend("bfpn".chars().map(|c| ctrl_alt(KeyCode::Char(c))));
        chords
            .into_iter()
            .map(|key| (chord_name(key), key))
            .collect()
    }

    fn chord_name(key: KeyEvent) -> String {
        let mut name = String::new();
        for (modifier, prefix) in [
            (KeyModifiers::CONTROL, "Ctrl+"),
            (KeyModifiers::ALT, "Alt+"),
            (KeyModifiers::SHIFT, "Shift+"),
        ] {
            if key.modifiers.contains(modifier) {
                name.push_str(prefix);
            }
        }
        match key.code {
            KeyCode::Char(c) if c.is_ascii_alphabetic() => name.push(c.to_ascii_uppercase()),
            KeyCode::Char(c) => name.push(c),
            KeyCode::PageUp => name.push_str("PgUp"),
            KeyCode::PageDown => name.push_str("PgDn"),
            code => name.push_str(&format!("{code:?}")),
        }
        name
    }

    /// The text area chords the key map keeps for itself in `ctx`, by name.
    fn shadowed(ctx: &KeyContext) -> Vec<String> {
        let mut names: Vec<String> = text_area_chords()
            .into_iter()
            .filter(|(_, key)| {
                !matches!(
                    map_key(*key, ctx),
                    Action::ComposerInput(_) | Action::ModalInput(_)
                )
            })
            .map(|(name, _)| name)
            .collect();
        names.sort();
        names
    }

    fn sorted(names: &[&str]) -> Vec<String> {
        let mut names: Vec<String> = names.iter().map(|name| name.to_string()).collect();
        names.sort();
        names
    }

    /// Each text area chord the key map takes before the text area sees
    /// it is listed here, so every shadowing is a choice: everything else
    /// the text area binds reaches it, in the composer and in a question's
    /// text field.
    #[test]
    fn the_key_map_shadows_only_the_listed_text_area_chords() {
        let always = [
            "Enter",
            "Ctrl+J",
            "Ctrl+C",
            "Ctrl+U",
            "Ctrl+X",
            "Alt+H",
            "Shift+Up",
            "Shift+Down",
            "PgUp",
            "PgDn",
            "Shift+PgUp",
            "Shift+PgDn",
        ];
        let composer = [
            &always[..],
            &[
                "Ctrl+R",
                "Ctrl+Up",
                "Ctrl+Down",
                "Ctrl+Alt+Up",
                "Ctrl+Alt+Down",
            ],
        ]
        .concat();
        let empty_composer = [&composer[..], &["Ctrl+D", "Up", "Down"]].concat();
        let one_line_draft = [&composer[..], &["Up", "Down"]].concat();
        let cases = [
            (IDLE, sorted(&empty_composer)),
            (TYPING, sorted(&one_line_draft)),
            (MULTI_LINE, sorted(&composer)),
            (INPUT, sorted(&always)),
            (CUSTOM_FIELD, sorted(&always)),
        ];
        for (ctx, expected) in cases {
            assert_eq!(shadowed(&ctx), expected, "{ctx:?}");
        }
    }

    /// The readline chords users reach for first are handed on whole, so
    /// they edit the composer and a question's text field; Ctrl+U is cut
    /// to the line start, which the text area alone would take as undo.
    #[test]
    fn readline_chords_reach_the_composer_and_the_text_field() {
        let alt = |c: char| press(KeyCode::Char(c), KeyModifiers::ALT);
        let chords = [
            alt('f'),
            alt('b'),
            alt('d'),
            ctrl('w'),
            ctrl('k'),
            ctrl('y'),
            ctrl('e'),
        ];
        let composers = [IDLE, TYPING, MULTI_LINE, VI_INSERT_TYPING, RUNNING_TYPING];
        let fields = [INPUT, PASSWORD, CUSTOM_FIELD];
        for key in chords {
            for ctx in composers {
                assert_eq!(map_key(key, &ctx), Action::ComposerInput(key), "{ctx:?}");
            }
            for ctx in fields {
                assert_eq!(map_key(key, &ctx), Action::ModalInput(key), "{ctx:?}");
            }
        }
        for ctx in composers.iter().chain(&fields) {
            assert_eq!(map_key(ctrl('u'), ctx), Action::DeleteToLineHead, "{ctx:?}");
        }
    }

    /// The help covers everything: past suspend and repaint, no key
    /// reaches the composer, the transcript or a panel under it.
    #[test]
    fn the_help_takes_every_key_but_the_terminals() {
        let cases = [
            (plain(KeyCode::Char('x')), HELP),
            (plain(KeyCode::Enter), RUNNING_HELP),
            (plain(KeyCode::Home), HELP),
            (ctrl('x'), HELP),
            (ctrl('g'), HELP),
            (press(KeyCode::Char('t'), KeyModifiers::ALT), HELP),
            (ctrl('t'), HELP),
            (ctrl('j'), HELP),
        ];
        for (key, ctx) in cases {
            assert_eq!(map_key(key, &ctx), Action::Nothing, "{key:?} in {ctx:?}");
        }
    }
}
