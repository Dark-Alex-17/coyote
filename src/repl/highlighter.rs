//! Command highlighting for the REPL line. Only the word that opens the
//! line is classified: a known command or macro paints green, an unknown
//! word after the sigil paints red, and the rest of the line keeps the
//! default colour.

use super::placeholder_edit::{SharedLineSnapshot, recorded_placeholders};
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
const PLACEHOLDER_COLOR: Color = Color::Green;

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
    snapshot: SharedLineSnapshot,
}

impl ReplHighlighter {
    pub fn new(
        ctx: Arc<RwLock<RequestContext>>,
        macro_names: MacroNames,
        snapshot: SharedLineSnapshot,
    ) -> Self {
        Self {
            ctx,
            macro_names,
            snapshot,
        }
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
    fn highlight(&self, line: &str, cursor: usize) -> StyledText {
        let (pasted, highlight) = {
            let ctx = self.ctx.read();
            (ctx.pasted_images.len(), ctx.app.config.highlight)
        };
        {
            let mut snapshot = self.snapshot.lock();
            snapshot.line.clear();
            snapshot.line.push_str(line);
            snapshot.cursor = cursor;
            snapshot.pasted = pasted;
        }
        highlight_line(line, self.command_span(line), pasted, !*NO_COLOR, highlight)
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
/// sigil followed by a word that matches nothing, or a bare sigil: the
/// dispatcher takes either as a command and refuses it by name.
pub(crate) fn classify_command(
    line: &str,
    prefix: CommandPrefix,
    extra_known: &[&str],
) -> Option<CommandSpan> {
    // `unfence` hands back a subslice of `line` (or `line` itself), so
    // the pointer offset places the inner text in `line`'s coordinates;
    // the word is searched within the inner text only, so a closing
    // fence glued to the command is not read as part of its name.
    let inner = unfence(line);
    let fence = inner.as_ptr() as usize - line.as_ptr() as usize;
    debug_assert!(
        fence + inner.len() <= line.len(),
        "unfence returned text that is not a subslice of its input"
    );
    let within = &line[..fence + inner.len()];
    let start = fence + inner.len() - inner.trim_start().len();
    let sigil = within[start..]
        .chars()
        .next()
        .filter(|&c| prefix.accepts(c))?;
    if let Some(range) = known_match(within, start, sigil, extra_known) {
        return Some(CommandSpan { range, known: true });
    }
    let word_start = start + sigil.len_utf8();
    let word_len = within[word_start..]
        .find(char::is_whitespace)
        .unwrap_or(within.len() - word_start);
    Some(CommandSpan {
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

/// Placeholders are coloured only when both `NO_COLOR` and the `highlight`
/// setting allow it, since the prompt that shows them in green follows
/// `highlight` alone.
fn highlight_line(
    line: &str,
    command: Option<CommandSpan>,
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
    styled_line(line, command, &placeholders)
}

/// Splits `line` at the command word and the placeholders so each part gets
/// one style; a placeholder's colour wins where the two overlap.
fn styled_line(
    line: &str,
    command: Option<CommandSpan>,
    placeholders: &[Range<usize>],
) -> StyledText {
    let style_at = |pos: usize| {
        if placeholders.iter().any(|span| span.contains(&pos)) {
            Style::new().fg(PLACEHOLDER_COLOR)
        } else {
            match &command {
                Some(CommandSpan { range, known }) if range.contains(&pos) => {
                    Style::new().fg(if *known { MATCH_COLOR } else { UNKNOWN_COLOR })
                }
                _ => Style::new().fg(DEFAULT_COLOR),
            }
        }
    };
    let mut bounds: Vec<usize> = placeholders
        .iter()
        .chain(command.as_ref().map(|span| &span.range))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, AppState, WorkingMode};
    use crate::testing::TestConfigDirGuard;
    use serial_test::serial;

    const LINE: &str = "see [Image #1] and [Image #7]";

    /// The line as the REPL paints it with no macros known.
    fn highlight_line(
        line: &str,
        prefix: CommandPrefix,
        pasted: usize,
        color: bool,
        highlight: bool,
    ) -> StyledText {
        super::highlight_line(
            line,
            classify_command(line, prefix, &[]),
            pasted,
            color,
            highlight,
        )
    }

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
        let highlighter = ReplHighlighter::new(
            Arc::new(RwLock::new(ctx)),
            MacroNames::default(),
            snapshot.clone(),
        );

        let styled = highlighter.highlight(LINE, 14);

        assert_eq!(styled.raw_string(), LINE);
        let snapshot = snapshot.lock();
        assert_eq!(
            (snapshot.line.as_str(), snapshot.cursor, snapshot.pasted),
            (LINE, 14, 2)
        );
    }

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
        let highlighter = ReplHighlighter::new(
            Arc::clone(&ctx),
            names.clone(),
            SharedLineSnapshot::default(),
        );
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
        assert_eq!(
            classify_command(".", CommandPrefix::Dot, &[]),
            Some(CommandSpan {
                range: 0..1,
                known: false
            })
        );
        assert_eq!(
            classify_command(". x", CommandPrefix::Dot, &[]),
            Some(CommandSpan {
                range: 0..1,
                known: false
            })
        );
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
            classify_command(":::\n.help:::", CommandPrefix::Dot, &[]),
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
