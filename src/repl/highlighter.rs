use super::{REPL_COMMANDS, unfence};

use crate::config::{CommandPrefix, RequestContext};
use crate::utils::NO_COLOR;

use nu_ansi_term::{Color, Style};
use parking_lot::{Mutex, RwLock};
use reedline::{Highlighter, StyledText};
use std::ops::Range;
use std::sync::Arc;

const DEFAULT_COLOR: Color = Color::Default;
const MATCH_COLOR: Color = Color::Green;
const UNKNOWN_COLOR: Color = Color::Red;

/// The macro names the highlighter paints as known. Listing them scans
/// the macro directories, so the REPL refreshes the list once per line
/// and a repaint only reads it.
#[derive(Debug, Clone, Default)]
pub struct MacroNames(Arc<Mutex<Vec<String>>>);

impl MacroNames {
    pub fn refresh(&self, ctx: &RequestContext) {
        *self.0.lock() = ctx
            .visible_macro_completions()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
    }
}

pub struct ReplHighlighter {
    ctx: Arc<RwLock<RequestContext>>,
    macro_names: MacroNames,
}

impl ReplHighlighter {
    pub fn new(ctx: Arc<RwLock<RequestContext>>, macro_names: MacroNames) -> Self {
        Self { ctx, macro_names }
    }

    /// The command word in `line`; the cached macro names are consulted
    /// only for a word no built-in claims.
    fn command_span(&self, line: &str) -> Option<CommandSpan> {
        let prefix = self.ctx.read().command_prefix();
        let span = classify_command(line, prefix, &[]);
        if !matches!(span, Some(CommandSpan { known: false, .. })) {
            return span;
        }
        let macros = self.macro_names.0.lock();
        let names: Vec<&str> = macros.iter().map(String::as_str).collect();
        classify_command(line, prefix, &names)
    }
}

impl Highlighter for ReplHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled_text = StyledText::new();

        if *NO_COLOR {
            styled_text.push((Style::default(), line.to_string()));
            return styled_text;
        }

        match self.command_span(line) {
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

/// Classifies the command in `line`, if any. The command word opens the
/// line, after any leading whitespace and a `:::` fence: that is the only
/// place the line is dispatched as a command, so a path or a dotted word
/// later in a sentence is left alone. Known is the longest built-in name, or a
/// name from `extra_known`, after the sigil that ends at the end of the
/// line or before whitespace, so `.roles` is not `.role`. Unknown is a
/// sigil followed by a word that matches nothing.
pub(crate) fn classify_command(
    line: &str,
    prefix: CommandPrefix,
    extra_known: &[&str],
) -> Option<CommandSpan> {
    let inner = unfence(line);
    let fence = inner.as_ptr() as usize - line.as_ptr() as usize;
    let start = fence + inner.len() - inner.trim_start().len();
    let sigil = line[start..]
        .chars()
        .next()
        .filter(|&c| prefix.accepts(c))?;
    if let Some(range) = known_match(line, start, sigil, extra_known) {
        return Some(CommandSpan { range, known: true });
    }
    let word_start = start + sigil.len_utf8();
    let word_len = line[word_start..]
        .find(char::is_whitespace)
        .unwrap_or(line.len() - word_start);
    (word_len > 0).then(|| CommandSpan {
        range: start..word_start + word_len,
        known: false,
    })
}

/// The longest known name following the sigil at `start`, as a range of
/// `line` that includes the sigil.
fn known_match(
    line: &str,
    start: usize,
    sigil: char,
    extra_known: &[&str],
) -> Option<Range<usize>> {
    let name_start = start + sigil.len_utf8();
    let rest = &line[name_start..];
    let whole_word = |name: &str| {
        rest.strip_prefix(name)
            .is_some_and(|after| after.chars().next().is_none_or(char::is_whitespace))
    };
    let builtin = REPL_COMMANDS
        .iter()
        .map(|cmd| &cmd.name[1..])
        .filter(|name| whole_word(name));
    let extra = extra_known.iter().copied().filter(|name| whole_word(name));
    builtin
        .chain(extra)
        .map(|name| start..name_start + name.len())
        .max_by_key(|range| range.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, AppState, WorkingMode};
    use crate::testing::TestConfigDirGuard;
    use serial_test::serial;

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
        assert_eq!(matched(".help", CommandPrefix::Dot), Some(".help"));
        assert_eq!(matched("/help", CommandPrefix::Slash), Some("/help"));
        assert_eq!(matched(".help", CommandPrefix::Both), Some(".help"));
        assert_eq!(matched("/help", CommandPrefix::Both), Some("/help"));
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
        assert_eq!(matched("see /exit", CommandPrefix::Slash), None);
        assert_eq!(matched("/exit", CommandPrefix::Slash), Some("/exit"));
        assert_eq!(
            matched("  /exit rag", CommandPrefix::Slash),
            Some("/exit rag")
        );
        assert_eq!(
            classify_command("x/exit /exit", CommandPrefix::Slash, &[]),
            None
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
    fn a_known_command_ends_at_a_word_boundary() {
        assert_eq!(
            matched(".info tools", CommandPrefix::Dot),
            Some(".info tools")
        );
        assert_eq!(matched(".info x", CommandPrefix::Dot), Some(".info"));
        assert_eq!(matched(".role", CommandPrefix::Dot), Some(".role"));
        assert_eq!(matched(".roles", CommandPrefix::Dot), None);
        assert_eq!(
            classify_command(".infox", CommandPrefix::Dot, &[]),
            Some(CommandSpan {
                range: 0..6,
                known: false
            })
        );
    }

    /// A repaint reads the names the REPL refreshed and never the macro
    /// directory: the cached name stays known after the directory is gone.
    #[test]
    #[serial]
    fn a_repaint_reads_the_cached_macro_names() {
        let _guard = TestConfigDirGuard::new("highlighter-macro-cache");
        let path = crate::config::paths::macro_file("cached-macro");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "isolated: false\nsteps:\n  - \".info\"\n").unwrap();
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            command_prefix: CommandPrefix::Dot,
            ..Default::default()
        });
        let ctx = Arc::new(RwLock::new(RequestContext::new(
            Arc::new(app),
            WorkingMode::Repl,
        )));
        let names = MacroNames::default();
        let highlighter = ReplHighlighter::new(Arc::clone(&ctx), names.clone());
        let span = |known| {
            Some(CommandSpan {
                range: 0..13,
                known,
            })
        };

        assert_eq!(highlighter.command_span(".cached-macro"), span(false));
        names.refresh(&ctx.read());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!(highlighter.command_span(".cached-macro"), span(true));
        assert_eq!(highlighter.command_span(".cached-macro x"), span(true));
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

    #[test]
    fn a_fenced_command_is_classified_inside_its_fence() {
        assert_eq!(
            classify_command(":::\n.help\n:::", CommandPrefix::Dot, &[]),
            Some(CommandSpan {
                range: 4..9,
                known: true
            })
        );
        assert_eq!(
            matched(":::\n.help\n:::", CommandPrefix::Dot),
            Some(".help")
        );
    }
}
