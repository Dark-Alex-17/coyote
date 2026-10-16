use super::REPL_COMMANDS;

use crate::config::{CommandPrefix, RequestContext};
use crate::utils::NO_COLOR;

use nu_ansi_term::{Color, Style};
use parking_lot::RwLock;
use reedline::{Highlighter, StyledText};
use std::ops::Range;
use std::sync::Arc;

const DEFAULT_COLOR: Color = Color::Default;
const MATCH_COLOR: Color = Color::Green;

pub struct ReplHighlighter {
    ctx: Arc<RwLock<RequestContext>>,
}

impl ReplHighlighter {
    pub fn new(ctx: Arc<RwLock<RequestContext>>) -> Self {
        Self { ctx }
    }
}

impl Highlighter for ReplHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled_text = StyledText::new();

        if *NO_COLOR {
            styled_text.push((Style::default(), line.to_string()));
            return styled_text;
        }

        let prefix = self.ctx.read().command_prefix();
        match command_match(line, prefix) {
            Some(range) => {
                styled_text.push((
                    Style::new().fg(DEFAULT_COLOR),
                    line[..range.start].to_string(),
                ));
                styled_text.push((
                    Style::new().fg(MATCH_COLOR),
                    line[range.clone()].to_string(),
                ));
                styled_text.push((
                    Style::new().fg(DEFAULT_COLOR),
                    line[range.end..].to_string(),
                ));
            }
            None => styled_text.push((Style::new().fg(DEFAULT_COLOR), line.to_string())),
        }

        styled_text
    }
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
