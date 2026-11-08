use super::IMAGE_PLACEHOLDER_RE;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use parking_lot::Mutex;
use reedline::{
    EditCommand, EditMode, MotionTarget, PromptEditMode, PromptViMode, ReedlineEvent,
    ReedlineRawEvent,
};
use std::ops::Range;
use std::sync::Arc;

/// Reedline ends its pass over an input batch at a host command, so events
/// that arrive in the same batch after the delete are dropped. That is the
/// price of letting the host check the live buffer before erasing a whole
/// placeholder.
const DELETE_PLACEHOLDER_BACK_HOST_COMMAND: &str = "coyote-delete-placeholder-back";
const DELETE_PLACEHOLDER_FORWARD_HOST_COMMAND: &str = "coyote-delete-placeholder-forward";

/// The buffer as the highlighter last saw it. `EditMode::parse_event` gets no
/// buffer access, so this is the edit mode's only view of the line.
#[derive(Debug, Default)]
pub(super) struct LineSnapshot {
    pub(super) line: String,
    pub(super) cursor: usize,
    pub(super) pasted: usize,
}

pub(super) type SharedLineSnapshot = Arc<Mutex<LineSnapshot>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceholderDelete {
    Back,
    Forward,
}

impl PlaceholderDelete {
    pub(super) fn from_host_command(cmd: &str) -> Option<Self> {
        match cmd {
            DELETE_PLACEHOLDER_BACK_HOST_COMMAND => Some(Self::Back),
            DELETE_PLACEHOLDER_FORWARD_HOST_COMMAND => Some(Self::Forward),
            _ => None,
        }
    }

    fn host_command(self) -> &'static str {
        match self {
            Self::Back => DELETE_PLACEHOLDER_BACK_HOST_COMMAND,
            Self::Forward => DELETE_PLACEHOLDER_FORWARD_HOST_COMMAND,
        }
    }

    fn target(self, line: &str, cursor: usize, pasted: usize) -> Option<Range<usize>> {
        match self {
            Self::Back => placeholder_ending_at(line, cursor, pasted),
            Self::Forward => placeholder_starting_at(line, cursor, pasted),
        }
    }

    /// The edit to run on the live buffer. The edit mode decided from a
    /// snapshot that may be stale, so a buffer with no placeholder at the
    /// cursor gets the plain keystroke instead.
    pub(super) fn edit_command(self, line: &str, cursor: usize, pasted: usize) -> EditCommand {
        match (self, self.target(line, cursor, pasted)) {
            (Self::Back, Some(span)) => EditCommand::Erase(MotionTarget::Position(span.start)),
            (Self::Forward, Some(span)) => EditCommand::Erase(MotionTarget::Position(span.end)),
            (Self::Back, None) => EditCommand::Backspace,
            (Self::Forward, None) => EditCommand::Delete,
        }
    }
}

/// The `[Image #N]` placeholders whose number names one of the `pasted`
/// images, each with the index of its image.
pub(super) fn recorded_placeholder_indices(
    line: &str,
    pasted: usize,
) -> impl Iterator<Item = (Range<usize>, usize)> + '_ {
    IMAGE_PLACEHOLDER_RE
        .captures_iter(line)
        .filter_map(move |captures| {
            let captures = captures.ok()?;
            let number: usize = captures.get(1)?.as_str().parse().ok()?;
            if !(1..=pasted).contains(&number) {
                return None;
            }
            Some((captures.get(0)?.range(), number - 1))
        })
}

pub(super) fn recorded_placeholders(
    line: &str,
    pasted: usize,
) -> impl Iterator<Item = Range<usize>> + '_ {
    recorded_placeholder_indices(line, pasted).map(|(span, _)| span)
}

pub(super) fn placeholder_ending_at(
    line: &str,
    cursor: usize,
    pasted: usize,
) -> Option<Range<usize>> {
    recorded_placeholders(line, pasted).find(|span| span.end == cursor)
}

pub(super) fn placeholder_starting_at(
    line: &str,
    cursor: usize,
    pasted: usize,
) -> Option<Range<usize>> {
    recorded_placeholders(line, pasted).find(|span| span.start == cursor)
}

/// Turns Backspace right after a pasted placeholder, or Delete right before
/// one, into a host command that removes the whole placeholder. Every other
/// event goes to the wrapped mode unchanged.
pub(super) struct PlaceholderAwareEditMode {
    inner: Box<dyn EditMode>,
    snapshot: SharedLineSnapshot,
    /// Set by a shift-modified key, the shift-selection bindings, and
    /// cleared by any key without SHIFT. While a selection may be active,
    /// Backspace and Delete stay plain so reedline deletes the selection.
    selection_active: bool,
}

impl PlaceholderAwareEditMode {
    pub(super) fn new(inner: Box<dyn EditMode>, snapshot: SharedLineSnapshot) -> Self {
        Self {
            inner,
            snapshot,
            selection_active: false,
        }
    }

    fn placeholder_delete(&self, event: &Event) -> Option<PlaceholderDelete> {
        if self.selection_active
            || !matches!(
                self.inner.edit_mode(),
                PromptEditMode::Emacs | PromptEditMode::Vi(PromptViMode::Insert)
            )
        {
            return None;
        }
        let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event
        else {
            return None;
        };
        if *modifiers != KeyModifiers::NONE {
            return None;
        }
        let delete = match code {
            KeyCode::Backspace => PlaceholderDelete::Back,
            KeyCode::Delete => PlaceholderDelete::Forward,
            _ => return None,
        };
        let snapshot = self.snapshot.lock();
        delete
            .target(&snapshot.line, snapshot.cursor, snapshot.pasted)
            .map(|_| delete)
    }

    /// Reedline does not export the return type of
    /// `handle_mode_specific_event`, so this mode cannot override it to pass a
    /// `SwitchMode` on. Instead it applies the switches the wrapped mode emits
    /// as it parses them, the way vi applies its own mode keys; the engine then
    /// finds the mode already in place.
    fn apply_mode_switches(&mut self, event: &ReedlineEvent) {
        match event {
            ReedlineEvent::SwitchMode(_) => {
                self.inner.handle_mode_specific_event(event.clone());
            }
            ReedlineEvent::Multiple(events) => {
                for event in events {
                    self.apply_mode_switches(event);
                }
            }
            _ => {}
        }
    }
}

fn opens_history_search(event: &ReedlineEvent) -> bool {
    match event {
        ReedlineEvent::SearchHistory => true,
        ReedlineEvent::Multiple(events) | ReedlineEvent::UntilFound(events) => {
            events.iter().any(opens_history_search)
        }
        _ => false,
    }
}

impl EditMode for PlaceholderAwareEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let event = Event::from(event);
        let delete = self.placeholder_delete(&event);
        if let Event::Key(key) = &event {
            self.selection_active = key.modifiers.contains(KeyModifiers::SHIFT);
        }
        if let Some(delete) = delete {
            return ReedlineEvent::ExecuteHostCommand(delete.host_command().into());
        }
        // Only key releases are refused, and this event was accepted once.
        let Ok(event) = ReedlineRawEvent::try_from(event) else {
            return ReedlineEvent::None;
        };
        let parsed = self.inner.parse_event(event);
        self.apply_mode_switches(&parsed);
        // The highlighter is not called while the search prompt is open, so
        // the snapshot would describe the hidden draft until the next paint.
        if opens_history_search(&parsed) {
            self.snapshot.lock().line.clear();
        }
        parsed
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::{
        Emacs, Vi, default_emacs_keybindings, default_vi_insert_keybindings,
        default_vi_normal_keybindings, default_vi_visual_keybindings,
    };

    const LINE: &str = "see [Image #1] and [Image #7]";

    fn key(code: KeyCode) -> ReedlineRawEvent {
        modified_key(code, KeyModifiers::NONE)
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> ReedlineRawEvent {
        ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, modifiers))).unwrap()
    }

    fn emacs() -> Box<dyn EditMode> {
        Box::new(Emacs::new(default_emacs_keybindings()))
    }

    fn vi() -> Box<dyn EditMode> {
        Box::new(Vi::new(
            default_vi_insert_keybindings(),
            default_vi_normal_keybindings(),
            default_vi_visual_keybindings(),
        ))
    }

    fn wrapped(inner: Box<dyn EditMode>, line: &str, cursor: usize) -> PlaceholderAwareEditMode {
        let snapshot = Arc::new(Mutex::new(LineSnapshot {
            line: line.to_string(),
            cursor,
            pasted: 1,
        }));
        PlaceholderAwareEditMode::new(inner, snapshot)
    }

    fn host_command(delete: PlaceholderDelete) -> ReedlineEvent {
        ReedlineEvent::ExecuteHostCommand(delete.host_command().into())
    }

    #[test]
    fn placeholder_ending_at_hits_only_the_closing_boundary() {
        assert_eq!(placeholder_ending_at(LINE, 14, 1), Some(4..14));
        for cursor in [4, 5, 13, 15] {
            assert_eq!(placeholder_ending_at(LINE, cursor, 1), None, "{cursor}");
        }
    }

    #[test]
    fn placeholder_starting_at_hits_only_the_opening_boundary() {
        assert_eq!(placeholder_starting_at(LINE, 4, 1), Some(4..14));
        for cursor in [3, 5, 13, 14] {
            assert_eq!(placeholder_starting_at(LINE, cursor, 1), None, "{cursor}");
        }
    }

    #[test]
    fn placeholders_without_a_pasted_image_are_not_found() {
        assert_eq!(placeholder_ending_at(LINE, LINE.len(), 1), None);
        assert_eq!(placeholder_starting_at(LINE, 19, 1), None);
        assert_eq!(placeholder_ending_at(LINE, 14, 0), None);
        assert_eq!(placeholder_ending_at("[Image #0]", 10, 1), None);
        assert_eq!(
            placeholder_ending_at(LINE, LINE.len(), 7),
            Some(19..LINE.len())
        );
    }

    #[test]
    fn partial_placeholders_are_not_found() {
        for line in ["[Image #1", "Image #1]", "[Image 1]", "[Image #]"] {
            assert_eq!(placeholder_ending_at(line, line.len(), 1), None, "{line}");
            assert_eq!(placeholder_starting_at(line, 0, 1), None, "{line}");
        }
    }

    #[test]
    fn placeholder_spans_are_byte_offsets_in_multi_byte_text() {
        let line = "caf\u{e9} [Image #1] \u{1f600}";
        let start = line.find('[').unwrap();
        let end = start + "[Image #1]".len();

        assert_eq!(placeholder_ending_at(line, end, 1), Some(start..end));
        assert_eq!(placeholder_starting_at(line, start, 1), Some(start..end));
        assert_eq!(&line[start..end], "[Image #1]");
    }

    #[test]
    fn edit_command_erases_the_whole_placeholder() {
        assert_eq!(
            PlaceholderDelete::Back.edit_command(LINE, 14, 1),
            EditCommand::Erase(MotionTarget::Position(4))
        );
        assert_eq!(
            PlaceholderDelete::Forward.edit_command(LINE, 4, 1),
            EditCommand::Erase(MotionTarget::Position(14))
        );
    }

    #[test]
    fn edit_command_falls_back_to_a_plain_keystroke_on_a_stale_snapshot() {
        assert_eq!(
            PlaceholderDelete::Back.edit_command("see [Image #1", 13, 1),
            EditCommand::Backspace
        );
        assert_eq!(
            PlaceholderDelete::Forward.edit_command("see Image #1]", 4, 1),
            EditCommand::Delete
        );
    }

    #[test]
    fn host_commands_round_trip() {
        for delete in [PlaceholderDelete::Back, PlaceholderDelete::Forward] {
            assert_eq!(
                PlaceholderDelete::from_host_command(delete.host_command()),
                Some(delete)
            );
        }
        assert_eq!(
            PlaceholderDelete::from_host_command("coyote-paste-image"),
            None
        );
    }

    #[test]
    fn backspace_after_a_placeholder_runs_the_host_command() {
        for inner in [emacs(), vi()] {
            let mut mode = wrapped(inner, LINE, 14);
            assert_eq!(
                mode.parse_event(key(KeyCode::Backspace)),
                host_command(PlaceholderDelete::Back)
            );
        }
    }

    #[test]
    fn delete_before_a_placeholder_runs_the_host_command() {
        for inner in [emacs(), vi()] {
            let mut mode = wrapped(inner, LINE, 4);
            assert_eq!(
                mode.parse_event(key(KeyCode::Delete)),
                host_command(PlaceholderDelete::Forward)
            );
        }
    }

    #[test]
    fn keys_away_from_a_placeholder_reach_the_wrapped_mode() {
        for (code, cursor) in [
            (KeyCode::Backspace, 13),
            (KeyCode::Delete, 14),
            (KeyCode::Char('x'), 14),
            (KeyCode::Left, 14),
        ] {
            let mut mode = wrapped(emacs(), LINE, cursor);
            let mut plain = emacs();
            assert_eq!(
                mode.parse_event(key(code)),
                plain.parse_event(key(code)),
                "{code:?}"
            );
        }
    }

    #[test]
    fn modified_backspace_reaches_the_wrapped_mode() {
        let alt_backspace = || modified_key(KeyCode::Backspace, KeyModifiers::ALT);
        let mut mode = wrapped(emacs(), LINE, 14);

        assert_eq!(
            mode.parse_event(alt_backspace()),
            emacs().parse_event(alt_backspace())
        );
    }

    #[test]
    fn history_search_clears_the_snapshot_until_the_next_paint() {
        let ctrl_r = || modified_key(KeyCode::Char('r'), KeyModifiers::CONTROL);
        let mut mode = wrapped(emacs(), LINE, 14);

        assert_eq!(mode.parse_event(ctrl_r()), ReedlineEvent::SearchHistory);

        assert_eq!(
            mode.parse_event(key(KeyCode::Backspace)),
            emacs().parse_event(key(KeyCode::Backspace))
        );
    }

    #[test]
    fn history_search_is_found_inside_compound_events() {
        assert!(opens_history_search(&ReedlineEvent::SearchHistory));
        assert!(opens_history_search(&ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".into()),
            ReedlineEvent::Multiple(vec![ReedlineEvent::Esc, ReedlineEvent::SearchHistory]),
        ])));
        assert!(!opens_history_search(&ReedlineEvent::Multiple(vec![
            ReedlineEvent::Esc,
            ReedlineEvent::Up,
        ])));
    }

    #[test]
    fn delete_keys_stay_plain_while_a_shift_selection_is_active() {
        let shift_left = || modified_key(KeyCode::Left, KeyModifiers::SHIFT);
        for (code, cursor, delete) in [
            (KeyCode::Backspace, 14, PlaceholderDelete::Back),
            (KeyCode::Delete, 4, PlaceholderDelete::Forward),
        ] {
            let mut mode = wrapped(emacs(), LINE, cursor);
            mode.parse_event(shift_left());

            assert_eq!(
                mode.parse_event(key(code)),
                emacs().parse_event(key(code)),
                "{code:?}"
            );

            mode.parse_event(shift_left());
            mode.parse_event(key(KeyCode::Right));

            assert_eq!(
                mode.parse_event(key(code)),
                host_command(delete),
                "{code:?}"
            );
        }
    }

    #[test]
    fn vi_normal_mode_is_unchanged() {
        let mut mode = wrapped(vi(), LINE, 14);
        let mut plain = vi();
        mode.parse_event(key(KeyCode::Esc));
        plain.parse_event(key(KeyCode::Esc));
        assert_eq!(mode.edit_mode(), PromptEditMode::Vi(PromptViMode::Normal));

        assert_eq!(
            mode.parse_event(key(KeyCode::Backspace)),
            plain.parse_event(key(KeyCode::Backspace))
        );
    }

    #[test]
    fn vi_visual_delete_still_returns_to_normal_mode() {
        let mut mode = wrapped(vi(), LINE, 0);
        mode.parse_event(key(KeyCode::Esc));
        mode.parse_event(key(KeyCode::Char('v')));
        assert_eq!(mode.edit_mode(), PromptEditMode::Vi(PromptViMode::Visual));

        mode.parse_event(key(KeyCode::Delete));

        assert_eq!(mode.edit_mode(), PromptEditMode::Vi(PromptViMode::Normal));
    }
}
