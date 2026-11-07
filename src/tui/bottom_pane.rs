//! What the bottom pane shows besides the composer. The composer is always
//! there; a view is a band of rows that sits on top of it: the command
//! palette, the history search, the rewind picker. The prompt modal is
//! not a view: a question outranks all of them and closes whatever is
//! open.

use super::DIM;
use super::cells::Cell;
use super::history::ComposerHistory;
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

/// Rows a band takes from the transcript at most, its header included.
pub(super) const BAND_MAX_ROWS: u16 = 8;
const NO_MATCHES: &str = "(no matches)";
const SEARCH_LABEL: &str = "history: ";
const REWIND_HEADER: &str = "Rewind to (Enter = choose  Esc = close)";

pub(super) enum View {
    Composer,
    Palette(PaletteState),
    HistorySearch(SearchState),
    Rewind(RewindState),
}

impl View {
    /// The rows the view asks the transcript for; the frame may give
    /// fewer.
    pub(super) fn band_rows(&self) -> u16 {
        match self {
            View::Composer => 0,
            View::Palette(palette) => palette.band_rows(),
            View::HistorySearch(search) => search.band_rows(),
            View::Rewind(rewind) => rewind.band_rows(),
        }
    }
}

/// Clamps a list's highlight moved by `delta` rows to its rows.
fn step_highlight(highlight: usize, delta: i32, len: usize) -> usize {
    let last = len.saturating_sub(1);
    let moved = highlight as i64 + i64::from(delta);
    usize::try_from(moved.max(0)).unwrap_or(last).min(last)
}

/// A multi-line entry shows its first line, scrubbed of escape
/// sequences, and how much follows.
fn first_line(text: &str) -> (String, String) {
    let mut lines = text.lines();
    let first = super::render::plain_text(lines.next().unwrap_or_default());
    let rest = lines.count();
    let tail = match rest {
        0 => String::new(),
        1 => "+1 line".to_string(),
        n => format!("+{n} lines"),
    };
    (first, tail)
}

fn rows_within_cap(len: usize) -> u16 {
    u16::try_from(len)
        .unwrap_or(BAND_MAX_ROWS)
        .min(BAND_MAX_ROWS)
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
    /// blocks on an MCP round trip. Once the first token is a command on
    /// its own and a space follows it, the palette is at the argument
    /// position: the command rows are not offered again, and the group's
    /// sub-verbs (`.info tools` after `.info `) come as argument rows.
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
        let candidates =
            |filter: &str| command_candidates(snapshot.state, snapshot.model_has_reasoning, filter);
        let at_arguments =
            parts.len() > 1 && candidates(cmd).iter().any(|command| command.name() == cmd);

        let mut rows = Vec::new();
        let mut token_range = if at_arguments {
            parts[parts.len() - 1].1..line.len()
        } else {
            cmd_start..line.len()
        };
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

        let command_filter = std::iter::once(cmd)
            .chain(parts.iter().skip(1).take(1).map(|(v, _)| *v))
            .collect::<Vec<&str>>()
            .join(" ");
        if rows.is_empty() && at_arguments && parts.len() == 2 {
            let group = format!("{cmd} ");
            rows.extend(
                candidates(&command_filter)
                    .into_iter()
                    .filter_map(|command| {
                        let verb = command.name().strip_prefix(group.as_str())?;
                        Some(PaletteRow {
                            value: format!("{verb} "),
                            description: command.description().to_string(),
                        })
                    }),
            );
        } else if rows.is_empty() && !at_arguments {
            rows.extend(
                candidates(&command_filter)
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

    /// `line` with the highlighted row in the typed token's place; None
    /// without a highlight, or when `line` is no longer the one the
    /// range was measured on.
    pub(super) fn accept(&self, line: &str) -> Option<String> {
        let row = self.highlighted()?;
        let Range { start, end } = self.token_range;
        if start > end {
            return None;
        }
        let mut text = String::with_capacity(line.len() + row.value.len());
        text.push_str(line.get(..start)?);
        text.push_str(&row.value);
        text.push_str(line.get(end..)?);
        Some(text)
    }

    /// Rows the band asks for: one per row up to the cap, the last of
    /// them given over to the count of what is hidden.
    pub(super) fn band_rows(&self) -> u16 {
        rows_within_cap(self.rows.len())
    }
}

/// The Ctrl+R band: a filter line over the history entries containing
/// it, newest first. The highlight starts on the newest match, so Enter
/// straight after Ctrl+R recalls the last line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SearchState {
    pub(super) filter: String,
    pub(super) matches: Vec<String>,
    pub(super) highlight: usize,
}

impl SearchState {
    pub(super) fn new(history: &ComposerHistory) -> Self {
        let mut search = Self {
            filter: String::new(),
            matches: Vec::new(),
            highlight: 0,
        };
        search.refilter(history);
        search
    }

    pub(super) fn push_char(&mut self, c: char, history: &ComposerHistory) {
        self.filter.push(c);
        self.refilter(history);
    }

    pub(super) fn pop_char(&mut self, history: &ComposerHistory) {
        self.filter.pop();
        self.refilter(history);
    }

    fn refilter(&mut self, history: &ComposerHistory) {
        self.matches = history
            .search(&self.filter)
            .into_iter()
            .map(str::to_string)
            .collect();
        self.highlight = 0;
    }

    pub(super) fn move_highlight(&mut self, delta: i32) {
        self.highlight = step_highlight(self.highlight, delta, self.matches.len());
    }

    pub(super) fn highlighted(&self) -> Option<&str> {
        self.matches.get(self.highlight).map(String::as_str)
    }

    /// The filter line plus the matches, or the one row saying there
    /// are none.
    pub(super) fn band_rows(&self) -> u16 {
        1 + rows_within_cap(self.matches.len().max(1)).min(BAND_MAX_ROWS - 1)
    }
}

/// One exchange the rewind picker can go back to: the cell that opened
/// it and the first line of the prompt that did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RewindRow {
    pub(super) opener: usize,
    pub(super) title: String,
}

/// What the picker chose: its rows down to the highlighted one, newest
/// first, one per exchange to take back. The last row's opener is where
/// the transcript goes back to and its title what the composer gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RewindTarget {
    pub(super) rows: Vec<RewindRow>,
}

/// The Esc-Esc band: this run's exchanges, newest first; choosing the
/// k-th row takes the k newest back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RewindState {
    pub(super) rows: Vec<RewindRow>,
    pub(super) highlight: usize,
}

impl RewindState {
    /// None when no cell opened an exchange, so there is nothing to go
    /// back to. A macro's row is skipped, since its one cell stands for
    /// as many exchanges as the macro ran; the flag says whether any was.
    pub(super) fn from_cells(
        cells: &[Cell],
        prefix: CommandPrefix,
        macros: &[(String, String)],
    ) -> (Option<Self>, bool) {
        let is_macro = |cell: &Cell| {
            matches!(cell, Cell::Command { name, .. }
                if super::command_word(name, prefix)
                    .and_then(|word| word.strip_prefix('.').map(str::to_string))
                    .is_some_and(|word| macros.iter().any(|(name, _)| *name == word)))
        };
        let mut skipped_macros = false;
        let rows: Vec<RewindRow> = cells
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, cell)| super::opens_exchange(cell, prefix))
            .filter(|(_, cell)| {
                let skip = is_macro(cell);
                skipped_macros |= skip;
                !skip
            })
            .map(|(opener, cell)| {
                let text = match cell {
                    Cell::User { text, .. } => text.as_str(),
                    Cell::Command { name, .. } => name.as_str(),
                    _ => "",
                };
                RewindRow {
                    opener,
                    title: text.to_string(),
                }
            })
            .collect();
        (
            (!rows.is_empty()).then_some(Self { rows, highlight: 0 }),
            skipped_macros,
        )
    }

    pub(super) fn move_highlight(&mut self, delta: i32) {
        self.highlight = step_highlight(self.highlight, delta, self.rows.len());
    }

    /// The highlighted row and every newer one; None with no rows.
    pub(super) fn chosen(&self) -> Option<RewindTarget> {
        self.rows.get(..=self.highlight).map(|rows| RewindTarget {
            rows: rows.to_vec(),
        })
    }

    pub(super) fn band_rows(&self) -> u16 {
        1 + rows_within_cap(self.rows.len()).min(BAND_MAX_ROWS - 1)
    }
}

/// Draws a list's rows: the first column padded to its widest entry,
/// the second dim after it, the highlighted row reversed. Given fewer
/// rows than the list has, the band keeps the highlight in view and
/// ends with how many rows are not shown.
fn render_rows(rows: &[(&str, &str)], highlight: Option<usize>, area: Rect, buf: &mut Buffer) {
    let area = area.intersection(*buf.area());
    if area.is_empty() {
        return;
    }
    let height = usize::from(area.height);
    let clipped = rows.len() > height;
    let shown = if clipped { height - 1 } else { rows.len() };
    let start = highlight.map_or(0, |index| index.saturating_sub(shown.saturating_sub(1)));
    let visible = &rows[start..(start + shown).min(rows.len())];
    let name_width = visible
        .iter()
        .map(|(name, _)| name.width())
        .max()
        .unwrap_or(0);
    for (offset, (name, description)) in visible.iter().enumerate() {
        let y = area.y + u16::try_from(offset).unwrap_or(u16::MAX);
        let line_area = Rect::new(area.x, y, area.width, 1);
        let line = Line::from(vec![
            Span::raw(format!("{name:<name_width$}")),
            Span::raw("  "),
            Span::styled(*description, DIM),
        ]);
        Paragraph::new(line).render(line_area, buf);
        if highlight == Some(start + offset) {
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

/// A one-row header over the rows under it; the rest of the area is
/// what is left for them.
fn render_header(line: Line<'_>, area: Rect, buf: &mut Buffer) -> Rect {
    let area = area.intersection(*buf.area());
    if area.is_empty() {
        return area;
    }
    Paragraph::new(line).render(Rect::new(area.x, area.y, area.width, 1), buf);
    Rect::new(area.x, area.y + 1, area.width, area.height - 1)
}

/// The palette's band: `name` padded to a column, the description dim
/// after it.
pub(super) struct PaletteView<'a>(pub(super) &'a PaletteState);

impl Widget for PaletteView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let rows: Vec<(&str, &str)> = self
            .0
            .rows
            .iter()
            .map(|row| (row.value.trim_end(), row.description.as_str()))
            .collect();
        render_rows(&rows, self.0.highlight, area, buf);
    }
}

/// The search band: the filter line, then the matches' first lines.
pub(super) struct SearchView<'a>(pub(super) &'a SearchState);

impl Widget for SearchView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let header = Line::from(vec![
            Span::styled(SEARCH_LABEL, DIM),
            Span::raw(self.0.filter.as_str()),
        ]);
        let rows_area = render_header(header, area, buf);
        if self.0.matches.is_empty() {
            render_rows(&[(NO_MATCHES, "")], None, rows_area, buf);
            buf.set_style(rows_area.intersection(*buf.area()), DIM);
            return;
        }
        let titles: Vec<(String, String)> = self.0.matches.iter().map(|m| first_line(m)).collect();
        let rows: Vec<(&str, &str)> = titles
            .iter()
            .map(|(first, tail)| (first.as_str(), tail.as_str()))
            .collect();
        render_rows(&rows, Some(self.0.highlight), rows_area, buf);
    }
}

/// The rewind band: a header saying what the rows are, then the
/// exchanges' first lines, newest first.
pub(super) struct RewindView<'a>(pub(super) &'a RewindState);

impl Widget for RewindView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let rows_area = render_header(Line::from(Span::styled(REWIND_HEADER, DIM)), area, buf);
        let titles: Vec<(String, String)> = self
            .0
            .rows
            .iter()
            .map(|row| first_line(&row.title))
            .collect();
        let rows: Vec<(&str, &str)> = titles
            .iter()
            .map(|(first, tail)| (first.as_str(), tail.as_str()))
            .collect();
        render_rows(&rows, Some(self.0.highlight), rows_area, buf);
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

    /// `.info` is a command and the head of a group: with a space after
    /// it the group's verbs are argument rows replacing the second token,
    /// not command rows replacing the line. A third token is past them.
    #[test]
    fn a_complete_command_offers_its_group_verbs_as_arguments() {
        let ctx = ctx_with(CommandPrefix::Dot, false);
        let snapshot = PaletteSnapshot::from_ctx(&ctx);
        let info = PaletteState::build(".info ", CommandPrefix::Dot, snapshot, &[], Some(&ctx))
            .expect("the group's verbs are rows");
        assert!(
            info.rows.iter().any(|row| row.value == "mcp-server "),
            "{info:?}"
        );
        assert!(info.rows.iter().all(|row| !row.value.starts_with('.')));
        assert_eq!(info.token_range, 6..6);
        assert_eq!(
            info.rows
                .iter()
                .map(|row| &row.value)
                .collect::<HashSet<_>>(),
            PaletteState::build(".info ", CommandPrefix::Dot, snapshot, &[], None)
                .expect("the verbs need no context")
                .rows
                .iter()
                .map(|row| &row.value)
                .collect::<HashSet<_>>()
        );
        let typed = PaletteState::build(".info mc", CommandPrefix::Dot, snapshot, &[], None)
            .expect("a typed verb narrows them");
        assert_eq!(typed.token_range, 6..8);
        assert_eq!(typed.rows[0].value, "mcp-server ");
        assert!(
            PaletteState::build(
                ".info mcp-server srv ",
                CommandPrefix::Dot,
                snapshot,
                &[],
                Some(&ctx)
            )
            .is_none()
        );
        assert!(PaletteState::build(".help ", CommandPrefix::Dot, snapshot, &[], None).is_none());
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
        palette.move_highlight(1);
        assert_eq!(palette.accept(".a").as_deref(), Some(".agent "));
        palette.move_highlight(1);
        assert_eq!(palette.accept(".a").as_deref(), Some(".alias "));
        palette.token_range = 6..7;
        palette.rows[1] = PaletteRow {
            value: "coder".to_string(),
            description: String::new(),
        };
        assert_eq!(palette.accept(".role c").as_deref(), Some(".role coder"));
    }

    #[test]
    fn accept_refuses_a_range_the_line_no_longer_has() {
        let mut palette = rows(&[".info"]);
        palette.move_highlight(1);
        palette.token_range = 0..3;
        assert_eq!(palette.accept(".i"), None);
        palette.token_range = 2..3;
        assert_eq!(palette.accept(".\u{e9}"), None, "inside a multi-byte char");
        palette.token_range = 2..1;
        assert_eq!(palette.accept(".info"), None);
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
        palette.move_highlight(1);
        palette.move_highlight(1);
        assert_eq!(palette.highlight, Some(1));
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
        assert_eq!(palette.band_rows(), BAND_MAX_ROWS);
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

    fn history(entries: &[&str]) -> ComposerHistory {
        let mut history = ComposerHistory::default();
        for entry in entries {
            history.push(entry);
        }
        history
    }

    fn buffer_lines(buf: &Buffer, width: u16, height: u16) -> Vec<String> {
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn the_search_opens_on_every_entry_newest_first_and_narrows_as_typed() {
        let history = history(&["git status", "cargo test", "git log"]);
        let mut search = SearchState::new(&history);
        assert_eq!(search.matches, ["git log", "cargo test", "git status"]);
        assert_eq!(search.highlight, 0);
        assert_eq!(search.highlighted(), Some("git log"));
        search.move_highlight(1);
        search.push_char('g', &history);
        search.push_char('i', &history);
        assert_eq!(search.filter, "gi");
        assert_eq!(search.matches, ["git log", "git status"]);
        assert_eq!(
            search.highlight, 0,
            "a new filter starts at the newest match"
        );
        search.move_highlight(5);
        assert_eq!(search.highlighted(), Some("git status"));
        search.move_highlight(-5);
        assert_eq!(search.highlighted(), Some("git log"));
        search.push_char('z', &history);
        assert!(search.matches.is_empty());
        assert_eq!(search.highlighted(), None);
        assert_eq!(search.band_rows(), 2);
        search.pop_char(&history);
        assert_eq!(search.matches.len(), 2);
        assert_eq!(search.band_rows(), 3);
    }

    #[test]
    fn the_search_band_shows_the_filter_over_the_first_lines() {
        let history = history(&["one", "two\nmore\nlines"]);
        let mut search = SearchState::new(&history);
        search.push_char('o', &history);
        let area = Rect::new(0, 0, 30, 3);
        let mut buf = Buffer::empty(area);
        SearchView(&search).render(area, &mut buf);
        assert_eq!(
            buffer_lines(&buf, 30, 3),
            ["history: o", "two  +2 lines", "one"]
        );
        assert!(buf[(0, 0)].modifier.contains(Modifier::DIM));
        assert!(buf[(0, 1)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(0, 2)].modifier.contains(Modifier::REVERSED));

        search.push_char('q', &history);
        let mut buf = Buffer::empty(area);
        SearchView(&search).render(area, &mut buf);
        assert_eq!(buffer_lines(&buf, 30, 3), ["history: oq", NO_MATCHES, ""]);
        assert!(buf[(0, 1)].modifier.contains(Modifier::DIM));
    }

    fn opened(mut cell: Cell) -> Cell {
        match &mut cell {
            Cell::User { opened, .. } | Cell::Command { opened, .. } => *opened = true,
            _ => {}
        }
        cell
    }

    #[test]
    fn the_rewind_rows_are_the_openers_newest_first() {
        let cells = [
            opened(Cell::user("first question\nwith detail")),
            Cell::assistant("answer".to_string(), true),
            Cell::user("failed before the request"),
            opened(Cell::command(".file notes.md")),
            Cell::command(".info"),
            opened(Cell::command(".regenerate")),
            opened(Cell::user("third")),
        ];
        assert_eq!(
            RewindState::from_cells(&cells[1..3], CommandPrefix::Dot, &[]),
            (None, false)
        );
        let (rewind, skipped) = RewindState::from_cells(&cells, CommandPrefix::Dot, &[]);
        assert!(!skipped);
        let mut rewind = rewind.unwrap();
        let titles: Vec<(usize, &str)> = rewind
            .rows
            .iter()
            .map(|row| (row.opener, row.title.as_str()))
            .collect();
        assert_eq!(
            titles,
            [
                (6, "third"),
                (3, ".file notes.md"),
                (0, "first question\nwith detail"),
            ]
        );
        let chosen = |rewind: &RewindState| rewind.chosen().unwrap().rows.len();
        assert_eq!(chosen(&rewind), 1);
        rewind.move_highlight(1);
        assert_eq!(chosen(&rewind), 2);
        assert_eq!(
            rewind.chosen().unwrap().rows.last().map(|row| row.opener),
            Some(3)
        );
        rewind.move_highlight(9);
        assert_eq!(chosen(&rewind), 3);
        rewind.move_highlight(-9);
        assert_eq!(chosen(&rewind), 1);
        assert_eq!(rewind.band_rows(), 4);

        let area = Rect::new(0, 0, 50, 4);
        let mut buf = Buffer::empty(area);
        RewindView(&rewind).render(area, &mut buf);
        assert_eq!(
            buffer_lines(&buf, 50, 4),
            [
                REWIND_HEADER,
                "third",
                ".file notes.md",
                "first question  +1 line",
            ]
        );
        assert!(buf[(0, 1)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn a_macro_row_is_skipped_and_reported() {
        let macros = [("deploy".to_string(), "ship it".to_string())];
        let cells = [
            opened(Cell::user("first")),
            Cell::assistant("answer".to_string(), true),
            opened(Cell::command(".deploy prod")),
            Cell::assistant("done".to_string(), true),
            opened(Cell::user("third")),
        ];
        let (rewind, skipped) = RewindState::from_cells(&cells, CommandPrefix::Dot, &macros);
        assert!(skipped);
        let openers: Vec<usize> = rewind.unwrap().rows.iter().map(|row| row.opener).collect();
        assert_eq!(openers, [4, 0]);
        let (rewind, skipped) = RewindState::from_cells(&cells[2..4], CommandPrefix::Dot, &macros);
        assert!(rewind.is_none());
        assert!(skipped);
    }

    #[test]
    fn band_rows_stay_within_the_cap_with_their_header() {
        let names: Vec<String> = (0..20).map(|i| format!("entry {i}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let search = SearchState::new(&history(&names));
        assert_eq!(search.band_rows(), BAND_MAX_ROWS);
        let cells: Vec<Cell> = names.iter().map(|name| opened(Cell::user(name))).collect();
        let (rewind, _) = RewindState::from_cells(&cells, CommandPrefix::Dot, &[]);
        let rewind = rewind.unwrap();
        assert_eq!(rewind.band_rows(), BAND_MAX_ROWS);
        assert_eq!(View::Composer.band_rows(), 0);
        assert_eq!(View::Rewind(rewind).band_rows(), BAND_MAX_ROWS);
    }
}
