use super::REPL_COMMANDS;
use super::placeholder_edit::{SharedLineSnapshot, recorded_placeholders};

use crate::config::{CommandPrefix, RequestContext};
use crate::utils::NO_COLOR;

use nu_ansi_term::{Color, Style};
use parking_lot::RwLock;
use reedline::{Highlighter, StyledText};
use std::ops::Range;
use std::sync::Arc;

const DEFAULT_COLOR: Color = Color::Default;
const MATCH_COLOR: Color = Color::Green;
const PLACEHOLDER_COLOR: Color = Color::Green;

pub struct ReplHighlighter {
    ctx: Arc<RwLock<RequestContext>>,
    snapshot: SharedLineSnapshot,
}

impl ReplHighlighter {
    pub fn new(ctx: Arc<RwLock<RequestContext>>, snapshot: SharedLineSnapshot) -> Self {
        Self { ctx, snapshot }
    }
}

impl Highlighter for ReplHighlighter {
    fn highlight(&self, line: &str, cursor: usize) -> StyledText {
        let (prefix, pasted, highlight) = {
            let ctx = self.ctx.read();
            (
                ctx.command_prefix(),
                ctx.pasted_images.len(),
                ctx.app.config.highlight,
            )
        };
        {
            let mut snapshot = self.snapshot.lock();
            snapshot.line.clear();
            snapshot.line.push_str(line);
            snapshot.cursor = cursor;
            snapshot.pasted = pasted;
        }
        highlight_line(line, prefix, pasted, !*NO_COLOR, highlight)
    }
}

/// Placeholders are coloured only when both `NO_COLOR` and the `highlight`
/// setting allow it, since the prompt that shows them in green follows
/// `highlight` alone.
fn highlight_line(
    line: &str,
    prefix: CommandPrefix,
    pasted: usize,
    color: bool,
    highlight: bool,
) -> StyledText {
    if !color {
        let mut styled_text = StyledText::new();
        styled_text.push((Style::default(), line.to_string()));
        return styled_text;
    }
    let placeholders: Vec<Range<usize>> = if highlight {
        recorded_placeholders(line, pasted).collect()
    } else {
        Vec::new()
    };
    styled_line(line, command_match(line, prefix), &placeholders)
}

/// Splits `line` at the command match and the placeholders so each part gets
/// one style; a placeholder's colour wins where the two overlap.
fn styled_line(
    line: &str,
    command: Option<Range<usize>>,
    placeholders: &[Range<usize>],
) -> StyledText {
    let style_at = |pos: usize| {
        if placeholders.iter().any(|span| span.contains(&pos)) {
            Style::new().fg(PLACEHOLDER_COLOR)
        } else if command.as_ref().is_some_and(|span| span.contains(&pos)) {
            Style::new().fg(MATCH_COLOR)
        } else {
            Style::new().fg(DEFAULT_COLOR)
        }
    };
    let mut bounds: Vec<usize> = placeholders
        .iter()
        .chain(command.as_ref())
        .flat_map(|span| [span.start, span.end])
        .chain([0, line.len()])
        .collect();
    bounds.sort_unstable();
    bounds.dedup();

    let mut styled_text = StyledText::new();
    if line.is_empty() {
        styled_text.push((Style::new().fg(DEFAULT_COLOR), String::new()));
    }
    for window in bounds.windows(2) {
        let (start, end) = (window[0], window[1]);
        let style = style_at(start);
        match styled_text.buffer.last_mut() {
            Some((last, text)) if *last == style => text.push_str(&line[start..end]),
            _ => styled_text.push((style, line[start..end].to_string())),
        }
    }
    styled_text
}

/// The span of the longest built-in command name in `line` that starts with
/// an accepted sigil at the start of the line or right after whitespace.
fn command_match(line: &str, prefix: CommandPrefix) -> Option<Range<usize>> {
    line.match_indices(|c: char| prefix.accepts(c))
        .filter(|&(i, _)| {
            line[..i]
                .chars()
                .next_back()
                .is_none_or(char::is_whitespace)
        })
        .flat_map(|(i, sigil)| {
            let name_start = i + sigil.len();
            REPL_COMMANDS
                .iter()
                .filter(move |cmd| line[name_start..].starts_with(&cmd.name[1..]))
                .map(move |cmd| i..name_start + cmd.name.len() - 1)
        })
        .fold(None, |acc: Option<Range<usize>>, item| match acc {
            Some(acc) if acc.len() >= item.len() => Some(acc),
            _ => Some(item),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppState, WorkingMode};

    const LINE: &str = "see [Image #1] and [Image #7]";

    fn spans(styled: &StyledText) -> Vec<(Style, &str)> {
        styled
            .buffer
            .iter()
            .map(|(style, text)| (*style, text.as_str()))
            .collect()
    }

    fn plain() -> Style {
        Style::new().fg(DEFAULT_COLOR)
    }

    fn green() -> Style {
        Style::new().fg(Color::Green)
    }

    #[test]
    fn only_recorded_placeholders_are_green() {
        let styled = highlight_line(LINE, CommandPrefix::Dot, 1, true, true);

        assert_eq!(
            spans(&styled),
            vec![
                (plain(), "see "),
                (green(), "[Image #1]"),
                (plain(), " and [Image #7]"),
            ]
        );
    }

    #[test]
    fn placeholders_stay_plain_without_colour_or_highlight() {
        assert_eq!(
            spans(&highlight_line(LINE, CommandPrefix::Dot, 1, false, true)),
            vec![(Style::default(), LINE)]
        );
        assert_eq!(
            spans(&highlight_line(LINE, CommandPrefix::Dot, 1, true, false)),
            vec![(plain(), LINE)]
        );
    }

    #[test]
    fn command_colouring_is_unchanged() {
        assert_eq!(
            spans(&highlight_line(
                ".info tools x",
                CommandPrefix::Dot,
                0,
                true,
                true
            )),
            vec![(green(), ".info tools"), (plain(), " x")]
        );
        assert_eq!(
            spans(&highlight_line(
                ".info tools x",
                CommandPrefix::Dot,
                0,
                true,
                false
            )),
            vec![(green(), ".info tools"), (plain(), " x")]
        );
        assert_eq!(
            spans(&highlight_line("hello", CommandPrefix::Dot, 0, true, true)),
            vec![(plain(), "hello")]
        );
        assert_eq!(
            spans(&highlight_line("", CommandPrefix::Dot, 0, true, true)),
            vec![(plain(), "")]
        );
    }

    #[test]
    fn command_and_placeholder_colouring_compose() {
        let line = ".file [Image #1] x";

        let styled = highlight_line(line, CommandPrefix::Dot, 1, true, true);

        assert_eq!(styled.raw_string(), line);
        assert_eq!(
            spans(&styled),
            vec![
                (green(), ".file"),
                (plain(), " "),
                (green(), "[Image #1]"),
                (plain(), " x")
            ]
        );
    }

    #[test]
    fn highlighting_records_the_line_cursor_and_pasted_count() {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Repl);
        ctx.pasted_images = vec!["/a.png".into(), "/b.png".into()];
        let snapshot = SharedLineSnapshot::default();
        let highlighter = ReplHighlighter::new(Arc::new(RwLock::new(ctx)), snapshot.clone());

        let styled = highlighter.highlight(LINE, 14);

        assert_eq!(styled.raw_string(), LINE);
        let snapshot = snapshot.lock();
        assert_eq!(
            (snapshot.line.as_str(), snapshot.cursor, snapshot.pasted),
            (LINE, 14, 2)
        );
    }

    fn matched(line: &str, prefix: CommandPrefix) -> Option<&str> {
        command_match(line, prefix).map(|range| &line[range])
    }

    #[test]
    fn matches_only_sigils_the_prefix_accepts() {
        assert_eq!(matched("/info", CommandPrefix::Slash), Some("/info"));
        assert_eq!(matched("/info", CommandPrefix::Dot), None);
        assert_eq!(matched(".info", CommandPrefix::Slash), None);
        assert_eq!(matched("/info", CommandPrefix::Both), Some("/info"));
        assert_eq!(matched(".info", CommandPrefix::Both), Some(".info"));
    }

    #[test]
    fn prefers_the_longest_command_name() {
        assert_eq!(
            matched("/exit rag", CommandPrefix::Slash),
            Some("/exit rag")
        );
        assert_eq!(
            matched(".info tools", CommandPrefix::Dot),
            Some(".info tools")
        );
    }

    #[test]
    fn requires_the_command_to_start_at_a_token_boundary() {
        assert_eq!(matched("see /tmp/exit", CommandPrefix::Slash), None);
        assert_eq!(matched("/exit", CommandPrefix::Slash), Some("/exit"));
        assert_eq!(
            matched("  /exit rag", CommandPrefix::Slash),
            Some("/exit rag")
        );
        assert_eq!(
            command_match("x/exit /exit", CommandPrefix::Slash),
            Some(7..12)
        );
    }
}
