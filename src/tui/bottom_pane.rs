//! What the bottom pane shows besides the composer. The composer is always
//! there; a view either sits on top of it, like the command palette's band
//! of rows, or will take its place. The prompt modal is not a view: a
//! question outranks all of them and closes whatever is open.

use super::DIM;
use crate::config::{CommandPrefix, RequestContext, StateFlags};
use crate::repl::{command_candidates, split_line};
use crate::utils::fuzzy_filter;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use std::collections::HashSet;
use std::ops::Range;
use unicode_width::UnicodeWidthStr;

pub(super) const PALETTE_MAX_ROWS: u16 = 8;

pub(super) enum View {
    Composer,
    Palette(PaletteState),
}

/// What the palette needs from the context to list commands while the
/// context is away on a turn: the completer reads these live, the
/// palette reads the copy taken when the context was last in hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PaletteSnapshot {
    pub(super) state: StateFlags,
    pub(super) model_has_reasoning: bool,
}

impl PaletteSnapshot {
    pub(super) const EMPTY: Self = Self {
        state: StateFlags::empty(),
        model_has_reasoning: false,
    };

    pub(super) fn from_ctx(ctx: &RequestContext) -> Self {
        Self {
            state: ctx.state(),
            model_has_reasoning: !ctx.current_model().reasoning_levels().is_empty(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PaletteRow {
    /// The text that replaces the typed token when the row is accepted.
    pub(super) value: String,
    pub(super) description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PaletteState {
    pub(super) rows: Vec<PaletteRow>,
    pub(super) highlight: Option<usize>,
    /// The bytes of the composer's line an accepted row replaces: the
    /// whole command for a command row, the last word for an argument.
    pub(super) token_range: Range<usize>,
}

impl PaletteState {
    /// The rows for a composer line, or None when the line is not a
    /// command under `prefix` or nothing matches it. Argument rows and
    /// macros need the context, so a line typed mid-turn gets command
    /// rows only. `.prompt` arguments are never offered: listing them
    /// blocks on an MCP round trip.
    pub(super) fn build(
        line: &str,
        prefix: CommandPrefix,
        snapshot: PaletteSnapshot,
        macros: &[(String, String)],
        ctx: Option<&RequestContext>,
    ) -> Option<Self> {
        let parts = split_line(line);
        let (typed_cmd, cmd_start) = *parts.first()?;
        let cmd = prefix.normalize(typed_cmd)?;
        let cmd = cmd.as_ref();
        let sigil = typed_cmd.chars().next()?;
        if cmd == ".prompt" && parts.len() > 1 {
            return None;
        }
        let with_sigil = |name: &str| format!("{sigil}{}", &name[1..]);

        let mut rows = Vec::new();
        let mut token_range = cmd_start..line.len();
        if parts.len() > 1
            && let Some(ctx) = ctx
        {
            let args_line = &line[parts[1].1..];
            let args: Vec<&str> = parts.iter().skip(1).map(|(v, _)| *v).collect();
            rows.extend(ctx.repl_complete(cmd, &args, args_line).into_iter().map(
                |(value, description)| PaletteRow {
                    value,
                    description: description.unwrap_or_default(),
                },
            ));
            if !rows.is_empty() {
                token_range = parts[parts.len() - 1].1..line.len();
            }
        }

        if rows.is_empty() {
            let command_filter = std::iter::once(cmd)
                .chain(parts.iter().skip(1).take(1).map(|(v, _)| *v))
                .collect::<Vec<&str>>()
                .join(" ");
            rows.extend(
                command_candidates(
                    snapshot.state,
                    snapshot.model_has_reasoning,
                    &command_filter,
                )
                .into_iter()
                .map(|command| PaletteRow {
                    value: format!("{} ", with_sigil(command.name())),
                    description: command.description().to_string(),
                }),
            );
            if ctx.is_some() {
                let macros: Vec<(String, &str)> = macros
                    .iter()
                    .map(|(name, description)| (format!(".{name}"), description.as_str()))
                    .filter(|(name, _)| {
                        command_filter.len() == 1
                            || name.starts_with(command_filter.get(..2).unwrap_or(&command_filter))
                    })
                    .collect();
                let macros = fuzzy_filter(macros, |(name, _)| name.as_str(), &command_filter);
                rows.extend(macros.into_iter().map(|(name, description)| PaletteRow {
                    value: format!("{} ", with_sigil(&name)),
                    description: description.to_string(),
                }));
            }
        }

        let mut seen = HashSet::new();
        rows.retain(|row| seen.insert(row.value.clone()));
        (!rows.is_empty()).then_some(Self {
            rows,
            highlight: None,
            token_range,
        })
    }

    /// Moves the highlight by `delta` rows. From no highlight a step down
    /// lands on the first row; a step up from the first row clears it,
    /// so Enter submits again.
    pub(super) fn move_highlight(&mut self, delta: i32) {
        let last = self.rows.len().saturating_sub(1);
        self.highlight = match self.highlight {
            None if delta > 0 => Some(0),
            None => None,
            Some(index) => {
                let moved = index as i64 + i64::from(delta);
                if moved < 0 {
                    None
                } else {
                    Some(usize::try_from(moved).unwrap_or(last).min(last))
                }
            }
        };
    }

    pub(super) fn highlighted(&self) -> Option<&PaletteRow> {
        self.highlight.and_then(|index| self.rows.get(index))
    }

    /// `line` with the highlighted row in the typed token's place.
    pub(super) fn accept(&self, line: &str) -> Option<String> {
        let row = self.highlighted()?;
        let mut text = String::with_capacity(line.len() + row.value.len());
        text.push_str(&line[..self.token_range.start]);
        text.push_str(&row.value);
        text.push_str(&line[self.token_range.end..]);
        Some(text)
    }

    /// Rows the band asks for: one per row up to the cap, the last of
    /// them given over to the count of what is hidden.
    pub(super) fn band_rows(&self) -> u16 {
        u16::try_from(self.rows.len())
            .unwrap_or(PALETTE_MAX_ROWS)
            .min(PALETTE_MAX_ROWS)
    }
}

/// The palette's band: `name` padded to a column, the description dim
/// after it, the highlighted row reversed. Given fewer rows than it has,
/// the band keeps the highlight in view and ends with how many rows are
/// not shown.
pub(super) struct PaletteView<'a>(pub(super) &'a PaletteState);

impl Widget for PaletteView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let rows = &self.0.rows;
        let height = usize::from(area.height);
        let clipped = rows.len() > height;
        let shown = if clipped { height - 1 } else { rows.len() };
        let start = self
            .0
            .highlight
            .map_or(0, |index| index.saturating_sub(shown.saturating_sub(1)));
        let visible = &rows[start..(start + shown).min(rows.len())];
        let name_width = visible
            .iter()
            .map(|row| row.value.trim_end().width())
            .max()
            .unwrap_or(0);
        for (offset, row) in visible.iter().enumerate() {
            let y = area.y + u16::try_from(offset).unwrap_or(u16::MAX);
            let line_area = Rect::new(area.x, y, area.width, 1);
            let name = format!("{:<name_width$}", row.value.trim_end());
            let line = Line::from(vec![
                Span::raw(name),
                Span::raw("  "),
                Span::styled(row.description.as_str(), DIM),
            ]);
            Paragraph::new(line).render(line_area, buf);
            if self.0.highlight == Some(start + offset) {
                buf.set_style(line_area, Style::new().add_modifier(Modifier::REVERSED));
            }
        }
        if clipped {
            let hidden = rows.len() - visible.len();
            let y = area.y + u16::try_from(shown).unwrap_or(u16::MAX);
            Paragraph::new(Line::from(Span::styled(format!("(+{hidden} more)"), DIM)))
                .render(Rect::new(area.x, y, area.width, 1), buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, AppState, WorkingMode};
    use crate::repl::ReplCompleter;
    use parking_lot::RwLock;
    use std::sync::Arc;

    fn rows(values: &[&str]) -> PaletteState {
        PaletteState {
            rows: values
                .iter()
                .map(|value| PaletteRow {
                    value: format!("{value} "),
                    description: format!("about {value}"),
                })
                .collect(),
            highlight: None,
            token_range: 0..2,
        }
    }

    fn ctx_with(prefix: CommandPrefix, function_calling: bool) -> RequestContext {
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            command_prefix: prefix,
            function_calling_support: function_calling,
            ..Default::default()
        });
        RequestContext::new(Arc::new(app), WorkingMode::Repl)
    }

    fn completer_names(ctx: RequestContext, line: &str) -> HashSet<String> {
        let mut completer = ReplCompleter::new(Arc::new(RwLock::new(ctx)));
        completer
            .suggestions_for(line, line.len())
            .into_iter()
            .map(|s| s.value.trim_end().to_string())
            .collect()
    }

    fn palette_names(ctx: &RequestContext, line: &str) -> HashSet<String> {
        let macros: Vec<(String, String)> = ctx
            .visible_macro_completions()
            .into_iter()
            .map(|(name, description)| (name, description.unwrap_or_default()))
            .collect();
        PaletteState::build(
            line,
            ctx.command_prefix(),
            PaletteSnapshot::from_ctx(ctx),
            &macros,
            Some(ctx),
        )
        .map(|palette| {
            palette
                .rows
                .into_iter()
                .map(|row| row.value.trim_end().to_string())
                .collect()
        })
        .unwrap_or_default()
    }

    /// The palette and the rustyline completer offer the same command
    /// names for the bare sigil, under every prefix and with and without
    /// function calling in the state.
    #[test]
    fn palette_rows_match_the_completer_for_a_bare_sigil() {
        for prefix in [
            CommandPrefix::Dot,
            CommandPrefix::Slash,
            CommandPrefix::Both,
        ] {
            for function_calling in [false, true] {
                for sigil in [".", "/"] {
                    let ctx = ctx_with(prefix, function_calling);
                    let palette = palette_names(&ctx, sigil);
                    let completer = completer_names(ctx_with(prefix, function_calling), sigil);
                    assert_eq!(
                        palette, completer,
                        "{prefix} fc={function_calling} {sigil:?}"
                    );
                    if prefix.accepts(sigil.chars().next().unwrap()) {
                        assert!(!palette.is_empty(), "{prefix} {sigil:?}");
                    } else {
                        assert!(palette.is_empty(), "{prefix} {sigil:?}");
                    }
                }
            }
        }
    }

    /// A session or an agent cannot be seated on a bare test context
    /// without a config directory, so those states are pinned at the
    /// shared table: the palette's command rows under a snapshot of the
    /// state are the table's candidates for it.
    #[test]
    fn palette_command_rows_are_the_shared_table_for_every_state() {
        let states = [
            StateFlags::empty(),
            StateFlags::SESSION,
            StateFlags::AGENT,
            StateFlags::FUNCTION_CALLING | StateFlags::SKILLS_ENABLED,
        ];
        for state in states {
            for prefix in [
                CommandPrefix::Dot,
                CommandPrefix::Slash,
                CommandPrefix::Both,
            ] {
                let sigil = prefix.display().to_string();
                let snapshot = PaletteSnapshot {
                    state,
                    model_has_reasoning: false,
                };
                let palette: HashSet<String> =
                    PaletteState::build(&sigil, prefix, snapshot, &[], None)
                        .expect("every state has commands")
                        .rows
                        .into_iter()
                        .map(|row| row.value.trim_end()[1..].to_string())
                        .collect();
                let table: HashSet<String> = command_candidates(state, false, ".")
                    .into_iter()
                    .map(|command| command.name()[1..].to_string())
                    .collect();
                assert_eq!(palette, table, "{state:?} {prefix}");
            }
        }
    }

    #[test]
    fn mcp_sub_verbs_match_the_completer() {
        for prefix in [CommandPrefix::Dot, CommandPrefix::Both] {
            let ctx = ctx_with(prefix, false);
            let palette = palette_names(&ctx, ".mcp ");
            assert!(palette.contains(".mcp auth"), "{palette:?}");
            assert_eq!(palette, completer_names(ctx_with(prefix, false), ".mcp "));
        }
        let ctx = ctx_with(CommandPrefix::Slash, false);
        let palette = palette_names(&ctx, "/mcp ");
        assert!(palette.contains("/mcp enable"), "{palette:?}");
        assert_eq!(
            palette,
            completer_names(ctx_with(CommandPrefix::Slash, false), "/mcp ")
        );
    }

    #[test]
    fn a_full_command_followed_by_a_space_has_no_command_rows() {
        let ctx = ctx_with(CommandPrefix::Dot, false);
        assert!(palette_names(&ctx, ".help ").is_empty());
        let set = palette_names(&ctx, ".set ");
        assert!(set.contains("highlight"), "{set:?}");
    }

    #[test]
    fn prompt_arguments_are_not_offered() {
        let ctx = ctx_with(CommandPrefix::Dot, false);
        assert!(
            PaletteState::build(
                ".prompt ",
                CommandPrefix::Dot,
                PaletteSnapshot::from_ctx(&ctx),
                &[],
                Some(&ctx)
            )
            .is_none()
        );
    }

    #[test]
    fn mid_turn_rows_come_from_the_snapshot_without_macros() {
        let macros = vec![("mymacro".to_string(), "mine".to_string())];
        let snapshot = PaletteSnapshot::EMPTY;
        let away = PaletteState::build(".my", CommandPrefix::Dot, snapshot, &macros, None);
        assert!(away.is_none(), "{away:?}");
        let ctx = ctx_with(CommandPrefix::Dot, false);
        let idle = PaletteState::build(".my", CommandPrefix::Dot, snapshot, &macros, Some(&ctx))
            .expect("the macro is a row");
        assert_eq!(idle.rows[0].value, ".mymacro ");
        assert_eq!(idle.rows[0].description, "mine");
    }

    #[test]
    fn highlight_moves_between_none_and_the_rows() {
        let mut palette = rows(&[".a", ".b", ".c"]);
        palette.move_highlight(-1);
        assert_eq!(palette.highlight, None);
        palette.move_highlight(1);
        assert_eq!(palette.highlight, Some(0));
        palette.move_highlight(-1);
        assert_eq!(palette.highlight, None);
        palette.move_highlight(1);
        palette.move_highlight(1);
        palette.move_highlight(1);
        palette.move_highlight(1);
        assert_eq!(palette.highlight, Some(2));
    }

    #[test]
    fn accept_replaces_the_token_range() {
        let mut palette = rows(&[".agent", ".alias"]);
        assert_eq!(palette.accept(".a"), None);
        palette.move_highlight(2);
        assert_eq!(palette.accept(".a").as_deref(), Some(".alias "));
        palette.token_range = 6..7;
        palette.rows[1] = PaletteRow {
            value: "coder".to_string(),
            description: String::new(),
        };
        assert_eq!(palette.accept(".role c").as_deref(), Some(".role coder"));
    }

    fn rendered(palette: &PaletteState, height: u16) -> (Buffer, Vec<String>) {
        let area = Rect::new(0, 0, 30, height);
        let mut buf = Buffer::empty(area);
        PaletteView(palette).render(area, &mut buf);
        let lines = (0..height)
            .map(|y| {
                (0..30)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (buf, lines)
    }

    #[test]
    fn rows_pad_the_name_column_and_reverse_the_highlight() {
        let mut palette = rows(&[".a", ".bbb"]);
        palette.move_highlight(2);
        let (buf, lines) = rendered(&palette, 2);
        assert_eq!(lines, vec![".a    about .a", ".bbb  about .bbb"]);
        assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(0, 1)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(29, 1)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(6, 0)].modifier.contains(Modifier::DIM));
        assert!(!buf[(0, 0)].modifier.contains(Modifier::DIM));
    }

    #[test]
    fn a_clipped_band_ends_with_the_hidden_count_and_follows_the_highlight() {
        let names: Vec<String> = (0..12).map(|i| format!(".c{i:02}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut palette = rows(&names);
        assert_eq!(palette.band_rows(), PALETTE_MAX_ROWS);
        let (_, lines) = rendered(&palette, 4);
        assert_eq!(lines[0], ".c00  about .c00");
        assert_eq!(lines[2], ".c02  about .c02");
        assert_eq!(lines[3], "(+9 more)");
        for _ in 0..6 {
            palette.move_highlight(1);
        }
        let (buf, lines) = rendered(&palette, 4);
        assert_eq!(lines[2], ".c05  about .c05");
        assert!(buf[(0, 2)].modifier.contains(Modifier::REVERSED));
        assert_eq!(lines[3], "(+9 more)");
    }
}
