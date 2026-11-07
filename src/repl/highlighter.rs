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
const UNKNOWN_COLOR: Color = Color::Red;

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
        let mut span = classify_command(line, prefix, &[]);
        if matches!(span, Some(CommandSpan { known: false, .. })) {
            // Macro names come from a directory scan, so they are only
            // consulted for a word no built-in claims.
            let macros = self.ctx.read().visible_macro_completions();
            let names: Vec<&str> = macros.iter().map(|(name, _)| name.as_str()).collect();
            span = classify_command(line, prefix, &names);
        }
        match span {
            Some(CommandSpan { range, known }) => {
                let colour = if known { MATCH_COLOR } else { UNKNOWN_COLOR };
                styled_text.push((
                    Style::new().fg(DEFAULT_COLOR),
                    line[..range.start].to_string(),
                ));
                styled_text.push((Style::new().fg(colour), line[range.clone()].to_string()));
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

/// The byte range of the command word in a typed line and whether it
/// names something that would run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandSpan {
    pub(crate) range: Range<usize>,
    pub(crate) known: bool,
}

/// Classifies the command in `line`, if any. Known is the longest built-in
/// name, or a name from `extra_known`, that starts with an accepted sigil
/// at the start of the line or right after whitespace. Unknown is a sigil
/// opening the line followed by a word that matches nothing: that is the
/// only place the line is dispatched as a command, so a path or a dotted
/// word later in a sentence is left alone.
pub(crate) fn classify_command(
    line: &str,
    prefix: CommandPrefix,
    extra_known: &[&str],
) -> Option<CommandSpan> {
    if let Some(range) = known_match(line, prefix, extra_known) {
        return Some(CommandSpan { range, known: true });
    }
    let start = line.len() - line.trim_start().len();
    let sigil = line[start..]
        .chars()
        .next()
        .filter(|&c| prefix.accepts(c))?;
    let word_start = start + sigil.len_utf8();
    let word_len = line[word_start..]
        .find(char::is_whitespace)
        .unwrap_or(line.len() - word_start);
    (word_len > 0).then(|| CommandSpan {
        range: start..word_start + word_len,
        known: false,
    })
}

fn known_match(line: &str, prefix: CommandPrefix, extra_known: &[&str]) -> Option<Range<usize>> {
    line.match_indices(|c: char| prefix.accepts(c))
        .filter(|&(i, _)| {
            line[..i]
                .chars()
                .next_back()
                .is_none_or(char::is_whitespace)
        })
        .flat_map(|(i, sigil)| {
            let name_start = i + sigil.len();
            let rest = &line[name_start..];
            let builtin = REPL_COMMANDS
                .iter()
                .filter(move |cmd| rest.starts_with(&cmd.name[1..]))
                .map(move |cmd| i..name_start + cmd.name.len() - 1);
            let extra = extra_known
                .iter()
                .filter(move |name| {
                    rest.strip_prefix(*name)
                        .is_some_and(|after| after.chars().next().is_none_or(char::is_whitespace))
                })
                .map(move |name| i..name_start + name.len());
            builtin.chain(extra)
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
        classify_command(line, prefix, &[])
            .filter(|span| span.known)
            .map(|span| &line[span.range])
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
            classify_command("x/exit /exit", CommandPrefix::Slash, &[]),
            Some(CommandSpan {
                range: 7..12,
                known: true
            })
        );
    }

    #[test]
    fn a_known_command_keeps_its_arguments_out_of_the_span() {
        assert_eq!(
            classify_command(".info x", CommandPrefix::Dot, &[]),
            Some(CommandSpan {
                range: 0..5,
                known: true
            })
        );
        assert_eq!(
            classify_command("/info x", CommandPrefix::Slash, &[]),
            Some(CommandSpan {
                range: 0..5,
                known: true
            })
        );
    }

    #[test]
    fn a_word_no_command_claims_is_unknown_only_at_the_line_start() {
        assert_eq!(
            classify_command(".nosuch", CommandPrefix::Dot, &[]),
            Some(CommandSpan {
                range: 0..7,
                known: false
            })
        );
        assert_eq!(
            classify_command("  .nosuch args", CommandPrefix::Dot, &[]),
            Some(CommandSpan {
                range: 2..9,
                known: false
            })
        );
        assert_eq!(classify_command(".", CommandPrefix::Dot, &[]), None);
        assert_eq!(classify_command(". x", CommandPrefix::Dot, &[]), None);
        assert_eq!(
            classify_command("see /tmp/exit", CommandPrefix::Slash, &[]),
            None
        );
        assert_eq!(classify_command(".nosuch", CommandPrefix::Slash, &[]), None);
    }

    #[test]
    fn an_extra_known_name_is_known_as_a_whole_word() {
        assert_eq!(
            classify_command(".mymacro arg", CommandPrefix::Dot, &["mymacro"]),
            Some(CommandSpan {
                range: 0..8,
                known: true
            })
        );
        assert_eq!(
            classify_command(".mymacros", CommandPrefix::Dot, &["mymacro"]),
            Some(CommandSpan {
                range: 0..9,
                known: false
            })
        );
    }
}
