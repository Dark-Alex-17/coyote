//! Text selected with the mouse over the transcript or a thread view: a
//! press anchors it, a drag moves its head, two or three presses on one
//! cell take a word or a line, and the release copies it. Positions name
//! a laid-out row by its cell and the row inside it, so a selection
//! holds while the view scrolls under it.

use super::cells::{BODY_INDENT, COLLAPSED_ARROW, EXPANDED_ARROW};
use super::render::{is_wrap_mark, wrap_join};
use super::rich::CopyRegion;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// How soon a press on the same cell counts towards a double or triple
/// click.
pub(super) const MULTI_CLICK: Duration = Duration::from_millis(400);

/// Where a selection was made; one in the transcript is not drawn over a
/// thread view, and the other way round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Region {
    Transcript,
    Pager,
}

/// A column of a laid-out row. `cell` and `row` name the row: the
/// transcript's cell and the row inside it, or line `row` of a thread
/// view with `cell` 0. The column counts from the region's left edge.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Pos {
    pub(super) cell: usize,
    pub(super) row: usize,
    pub(super) column: usize,
}

impl Pos {
    pub(super) fn new(cell: usize, row: usize, column: usize) -> Self {
        Self { cell, row, column }
    }

    fn key(self) -> (usize, usize) {
        (self.cell, self.row)
    }
}

/// One laid-out row as the copy reads it: its text, the columns of fold
/// arrow and indent in front of it, and what joins it to the row above.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Row {
    cell: usize,
    row: usize,
    text: String,
    gutter: usize,
    /// A newline, or for a row wrapped from the one above, nothing (a
    /// break inside a word) or the space the wrap took out.
    before: &'static str,
    /// The block the row was drawn for, by its first row, and the source
    /// a copy takes in place of the block's rows.
    block: Option<(usize, Arc<str>)>,
    /// The columns of inline formulas drawn in the row, and the source a
    /// copy takes in place of each.
    inline: Vec<(Range<usize>, Arc<str>)>,
}

impl Row {
    fn key(&self) -> (usize, usize) {
        (self.cell, self.row)
    }

    fn continues(&self) -> bool {
        self.before != "\n"
    }
}

/// The rows of `lines`, each with its position. A row led by a wrap mark
/// joins the row above the way the mark says; any other row starts a
/// line of its own.
pub(super) fn rows<'a>(lines: impl IntoIterator<Item = (Pos, &'a Line<'a>)>) -> Vec<Row> {
    lines
        .into_iter()
        .map(|(pos, line)| Row {
            cell: pos.cell,
            row: pos.row,
            text: line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect(),
            gutter: gutter_width(line),
            before: wrap_join(line).unwrap_or("\n"),
            block: None,
            inline: Vec::new(),
        })
        .collect()
}

/// Marks the rows of one cell that `regions` cover, so a copy takes each
/// region's source once rather than the rows or columns drawn for it.
pub(super) fn mark_regions(rows: &mut [Row], regions: &[CopyRegion]) {
    for region in regions {
        match region {
            CopyRegion::Block {
                rows: covered,
                source,
            } => {
                for row in rows.iter_mut().filter(|row| covered.contains(&row.row)) {
                    row.block = Some((covered.start, Arc::clone(source)));
                }
            }
            CopyRegion::Inline {
                row: at,
                cols,
                source,
            } => {
                for row in rows.iter_mut().filter(|row| row.row == *at) {
                    row.inline.push((cols.clone(), Arc::clone(source)));
                }
            }
        }
    }
}

/// The columns the fold arrow and the body indent take at the start of
/// `line`; each is a span of its own, and a wrap mark may sit among them.
/// Everything in front of a wrap mark is gutter too: a markdown row puts
/// the mark after the hanging indent or quote bar it repeats.
fn gutter_width(line: &Line) -> usize {
    let leading: usize = line
        .spans
        .iter()
        .take_while(|span| {
            is_wrap_mark(span)
                || matches!(
                    span.content.as_ref(),
                    BODY_INDENT | COLLAPSED_ARROW | EXPANDED_ARROW
                )
        })
        .map(|span| span.content.width())
        .sum();
    let before_mark = line.spans.iter().position(is_wrap_mark).map_or(0, |at| {
        line.spans[..at]
            .iter()
            .map(|span| span.content.width())
            .sum()
    });
    leading.max(before_mark)
}

/// The characters of `text` that start in columns `from..to`, with the
/// source of each of `swaps` whose columns the range touches given once
/// in place of the characters in those columns.
fn columns(text: &str, from: usize, to: usize, swaps: &[(Range<usize>, Arc<str>)]) -> String {
    let mut column = 0;
    let mut out = String::new();
    let mut swapped = None;
    for c in text.chars() {
        if column >= to {
            break;
        }
        let swap = swaps.iter().position(|(cols, _)| cols.contains(&column));
        if let Some(at) = swap {
            let (cols, source) = &swaps[at];
            if swapped != Some(at) && cols.start < to && from < cols.end {
                out.push_str(source);
                swapped = Some(at);
            }
        } else if column >= from {
            out.push(c);
        }
        column += c.width().unwrap_or(0);
    }
    out
}

/// The text from `start` to `end`, both columns included: no gutter, and
/// a wrapped row joined back onto the row it was wrapped from. A block's
/// rows give its source once, however many of them the range touches.
pub(super) fn extract(rows: &[Row], start: Pos, end: Pos) -> String {
    let mut out = String::new();
    let mut first = true;
    let mut last_block = None;
    for row in rows
        .iter()
        .filter(|row| start.key() <= row.key() && row.key() <= end.key())
    {
        let block = row
            .block
            .as_ref()
            .map(|(at, source)| ((row.cell, *at), source));
        let piece = match block {
            Some((id, _)) if last_block == Some(id) => continue,
            Some((id, source)) => {
                last_block = Some(id);
                if source.is_empty() {
                    continue;
                }
                source.to_string()
            }
            None => {
                last_block = None;
                let from = if row.key() == start.key() {
                    start.column
                } else {
                    0
                };
                let to = if row.key() == end.key() {
                    end.column.saturating_add(1)
                } else {
                    usize::MAX
                };
                columns(&row.text, from.max(row.gutter), to, &row.inline)
                    .trim_end()
                    .to_string()
            }
        };
        if !first {
            out.push_str(row.before);
        }
        first = false;
        out.push_str(&piece);
    }
    out
}

/// The word under `pos`: the run of non-blank characters it falls in,
/// or the one column when it falls on a blank.
pub(super) fn word_at(rows: &[Row], pos: Pos) -> (Pos, Pos) {
    let Some(row) = rows.iter().find(|row| row.key() == pos.key()) else {
        return (pos, pos);
    };
    let mut spans = Vec::new();
    let mut column = 0;
    for c in row.text.chars() {
        let width = c.width().unwrap_or(0);
        if width > 0 {
            spans.push((column, column + width - 1, c.is_whitespace()));
        }
        column += width;
    }
    let Some(at) = spans
        .iter()
        .position(|&(from, to, _)| from <= pos.column && pos.column <= to)
    else {
        return (pos, pos);
    };
    if spans[at].2 || spans[at].0 < row.gutter {
        return (pos, pos);
    }
    let blank = |&(from, _, blank): &(usize, usize, bool)| blank || from < row.gutter;
    let first = spans[..at].iter().rposition(blank).map_or(0, |i| i + 1);
    let last = spans[at..]
        .iter()
        .position(blank)
        .map_or(spans.len(), |i| at + i)
        - 1;
    (
        Pos::new(pos.cell, pos.row, spans[first].0),
        Pos::new(pos.cell, pos.row, spans[last].1),
    )
}

/// The whole line `pos` is on: every row wrapped from the same source
/// line.
pub(super) fn line_at(rows: &[Row], pos: Pos) -> (Pos, Pos) {
    let Some(at) = rows.iter().position(|row| row.key() == pos.key()) else {
        return (pos, pos);
    };
    let mut first = at;
    while first > 0 && rows[first].continues() {
        first -= 1;
    }
    let mut last = at;
    while rows.get(last + 1).is_some_and(Row::continues) {
        last += 1;
    }
    (
        Pos::new(rows[first].cell, rows[first].row, 0),
        Pos::new(rows[last].cell, rows[last].row, usize::MAX),
    )
}

/// A selection under way or made: where the press anchored it and where
/// the pointer has taken its head since.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Selection {
    pub(super) region: Region,
    anchor: Pos,
    head: Pos,
    dragged: bool,
    clicks: u8,
}

impl Selection {
    /// The ends in reading order.
    pub(super) fn range(&self) -> (Pos, Pos) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }
}

/// What a release ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Release {
    /// A drag, a double or a triple click: the selection to copy.
    Copy(Selection),
    /// A press with no drag, at the region and position it was made.
    Click(Region, Pos),
    /// No press was held.
    Nothing,
}

/// A press remembered so the next one can count as a double or triple
/// click.
#[derive(Clone, Copy)]
struct Press {
    at: Instant,
    column: u16,
    row: u16,
    clicks: u8,
}

#[derive(Default)]
pub(super) struct SelectionState {
    current: Option<Selection>,
    last_press: Option<Press>,
}

impl SelectionState {
    /// Starts a selection at `pos`, the screen cell `column`, `row`.
    /// Returns how many presses in a row this one makes: one, or two or
    /// three when it lands on the cell of the press before within
    /// [`MULTI_CLICK`]; a fourth starts again at one.
    pub(super) fn press(
        &mut self,
        region: Region,
        pos: Pos,
        (column, row): (u16, u16),
        now: Instant,
    ) -> u8 {
        let clicks = match self.last_press {
            Some(last)
                if (last.column, last.row) == (column, row)
                    && now.saturating_duration_since(last.at) <= MULTI_CLICK
                    && last.clicks < 3 =>
            {
                last.clicks + 1
            }
            _ => 1,
        };
        self.last_press = Some(Press {
            at: now,
            column,
            row,
            clicks,
        });
        self.current = Some(Selection {
            region,
            anchor: pos,
            head: pos,
            dragged: false,
            clicks,
        });
        clicks
    }

    /// Widens the selection a press started to `start..=end`, for a word
    /// or a line.
    pub(super) fn widen(&mut self, start: Pos, end: Pos) {
        if let Some(selection) = self.current.as_mut() {
            selection.anchor = start;
            selection.head = end;
        }
    }

    /// Moves the head to `pos`; a move off the anchor makes the press a
    /// drag rather than a click. The word or line a double or triple
    /// click took stays as it is.
    pub(super) fn drag(&mut self, pos: Pos) {
        if let Some(selection) = self.current.as_mut()
            && selection.clicks == 1
        {
            selection.dragged |= pos != selection.anchor;
            selection.head = pos;
        }
    }

    /// Ends the press. A drag, a double or a triple click hands back the
    /// selection to copy, which stays drawn; a plain click clears it and
    /// hands back where it was pressed, so the click does what a click
    /// does.
    pub(super) fn release(&mut self) -> Release {
        let Some(selection) = self.current else {
            return Release::Nothing;
        };
        if selection.dragged || selection.clicks > 1 {
            Release::Copy(selection)
        } else {
            self.current = None;
            Release::Click(selection.region, selection.anchor)
        }
    }

    /// Drops the selection; whether there was one.
    pub(super) fn clear(&mut self) -> bool {
        self.current.take().is_some()
    }

    pub(super) fn current(&self) -> Option<Selection> {
        self.current
    }

    /// The selection to draw: one a press has only anchored, with no
    /// drag yet, is still a click and is not drawn.
    pub(super) fn shown(&self) -> Option<Selection> {
        self.current
            .filter(|selection| selection.dragged || selection.clicks > 1)
    }

    /// Whether a press is held and has moved: what a drag at a region's
    /// edge scrolls for.
    pub(super) fn dragging(&self) -> bool {
        self.current.is_some_and(|selection| selection.dragged)
    }
}

/// Paints the part of the row at `y` of `area` that falls inside `range`,
/// the row being `key` (its cell and the row inside it).
pub(super) fn paint_row(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    key: (usize, usize),
    (start, end): (Pos, Pos),
    style: Style,
) {
    if key < start.key() || key > end.key() {
        return;
    }
    let width = usize::from(area.width);
    let from = if key == start.key() { start.column } else { 0 }.min(width);
    let to = if key == end.key() {
        end.column.saturating_add(1)
    } else {
        width
    }
    .min(width);
    if from >= to {
        return;
    }
    let x = area.x + u16::try_from(from).unwrap_or(u16::MAX);
    let span = u16::try_from(to - from).unwrap_or(u16::MAX);
    buf.set_style(Rect::new(x, y, span, 1).intersection(area), style);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::render::wrap_mark;
    use ratatui::text::Span;

    fn line(spans: &[&'static str]) -> Line<'static> {
        Line::from(spans.iter().map(|s| Span::raw(*s)).collect::<Vec<_>>())
    }

    /// A row a wrap continued, the mark put after `gutter`.
    fn wrapped(gutter: &[&'static str], at_space: bool, text: &'static str) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = gutter.iter().map(|s| Span::raw(*s)).collect();
        spans.push(wrap_mark(at_space));
        spans.push(Span::raw(text));
        Line::from(spans)
    }

    fn laid_out(lines: &[(usize, Line<'static>)]) -> Vec<Row> {
        rows(
            lines
                .iter()
                .enumerate()
                .map(|(i, (cell, line))| (Pos::new(*cell, i, 0), line)),
        )
    }

    /// The arrow and the indent are left out, and the rows a reply was
    /// wrapped into join back with the space the wrap took out.
    #[test]
    fn the_copy_drops_the_gutter_and_joins_wrapped_rows() {
        let rows = laid_out(&[
            (0, line(&[COLLAPSED_ARROW, "fs_read()  ok"])),
            (0, line(&[BODY_INDENT, "first output line"])),
            (1, line(&["the quick brown"])),
            (1, wrapped(&[], true, "fox jumps")),
            (1, line(&["short"])),
            (1, line(&["next"])),
        ]);
        let text = extract(&rows, Pos::new(0, 0, 0), Pos::new(1, 5, usize::MAX));
        assert_eq!(
            text,
            "fs_read()  ok\nfirst output line\nthe quick brown fox jumps\nshort\nnext"
        );
    }

    /// A row cut at the very edge, inside a word, joins back with no
    /// space; the first and last rows keep only their selected columns.
    #[test]
    fn a_hard_wrap_joins_with_nothing_and_the_ends_are_cut() {
        let rows = laid_out(&[
            (0, line(&["abcdefghij"])),
            (0, wrapped(&[], false, "klmno pq")),
        ]);
        assert_eq!(rows[1].before, "");
        assert_eq!(
            extract(&rows, Pos::new(0, 0, 2), Pos::new(0, 1, 4)),
            "cdefghijklmno"
        );
    }

    /// A full row followed by a short word with no wrap mark is two
    /// lines, however much it looks like a wrap; a marked row after the
    /// indent joins on with its gutter left out.
    #[test]
    fn a_row_without_a_wrap_marker_keeps_its_newline() {
        let rows = laid_out(&[
            (0, line(&[BODY_INDENT, "drwxr-xr-x  docs  src"])),
            (0, line(&[BODY_INDENT, "target"])),
            (0, wrapped(&[BODY_INDENT], false, "/debug")),
        ]);
        assert_eq!(rows[1].before, "\n");
        assert_eq!(
            extract(&rows, Pos::new(0, 0, 0), Pos::new(0, 2, usize::MAX)),
            "drwxr-xr-x  docs  src\ntarget/debug"
        );
    }

    /// Lines wider than the region were left for the view to clip, not
    /// wrapped, so each keeps its own line.
    #[test]
    fn clipped_lines_stay_apart() {
        let rows = laid_out(&[
            (
                0,
                line(&[BODY_INDENT, "let value = compute(first, second);"]),
            ),
            (0, line(&[BODY_INDENT, "value"])),
        ]);
        assert_eq!(
            extract(&rows, Pos::new(0, 0, 0), Pos::new(0, 1, usize::MAX)),
            "let value = compute(first, second);\nvalue"
        );
    }

    /// A double click takes the word under it and no gutter; a triple
    /// click takes every row of the wrapped line.
    #[test]
    fn a_word_and_a_line_span_what_was_clicked() {
        let rows = laid_out(&[
            (0, line(&[BODY_INDENT, "alpha beta-gamma delta"])),
            (1, line(&["the quick brown"])),
            (1, wrapped(&[], true, "fox jumps")),
        ]);
        let (start, end) = word_at(&rows, Pos::new(0, 0, 10));
        assert_eq!(extract(&rows, start, end), "beta-gamma");
        let (start, end) = word_at(&rows, Pos::new(0, 0, 0));
        assert_eq!(extract(&rows, start, end), "");
        let (start, end) = line_at(&rows, Pos::new(1, 2, 1));
        assert_eq!(extract(&rows, start, end), "the quick brown fox jumps");
    }

    /// A range touching any row of a drawn block copies the block's whole
    /// source once; the note under a block shown as source copies nothing,
    /// and the source rows above the note copy as drawn.
    #[test]
    fn a_block_copies_its_source_once_and_a_note_copies_nothing() {
        let mut rows = laid_out(&[
            (0, line(&["before"])),
            (0, line(&["┌───┐"])),
            (0, line(&["│ A │"])),
            (0, line(&["└───┘"])),
            (0, line(&["```mermaid"])),
            (0, line(&["bad"])),
            (0, line(&["```"])),
            (0, line(&["mermaid: parse error (showing source)"])),
            (0, line(&["after"])),
        ]);
        mark_regions(
            &mut rows,
            &[
                CopyRegion::Block {
                    rows: 1..4,
                    source: Arc::from("```mermaid\nA\n```"),
                },
                CopyRegion::Block {
                    rows: 7..8,
                    source: Arc::from(""),
                },
            ],
        );
        assert_eq!(
            extract(&rows, Pos::new(0, 0, 3), Pos::new(0, 8, 2)),
            "ore\n```mermaid\nA\n```\n```mermaid\nbad\n```\naft"
        );
        assert_eq!(
            extract(&rows, Pos::new(0, 2, 3), Pos::new(0, 2, 3)),
            "```mermaid\nA\n```"
        );
        assert_eq!(extract(&rows, Pos::new(0, 7, 0), Pos::new(0, 7, 9)), "");
    }

    /// A range touching any column of an inline formula copies its source
    /// once in place of the drawing; the piece a wrap moved to the next
    /// row copies nothing, and the text around is cut as usual.
    #[test]
    fn an_inline_formula_copies_its_source_once() {
        let mut rows = laid_out(&[
            (0, line(&["the value x² + y² here, and ab"])),
            (0, wrapped(&[], false, "cd done")),
        ]);
        mark_regions(
            &mut rows,
            &[
                CopyRegion::Inline {
                    row: 0,
                    cols: 10..17,
                    source: Arc::from("$x^2 + y^2$"),
                },
                CopyRegion::Inline {
                    row: 0,
                    cols: 28..30,
                    source: Arc::from("\\(abcd\\)"),
                },
                CopyRegion::Inline {
                    row: 1,
                    cols: 0..2,
                    source: Arc::from(""),
                },
            ],
        );
        let all = extract(&rows, Pos::new(0, 0, 0), Pos::new(0, 1, usize::MAX));
        assert_eq!(all, "the value $x^2 + y^2$ here, and \\(abcd\\) done");
        assert_eq!(
            extract(&rows, Pos::new(0, 0, 12), Pos::new(0, 0, 12)),
            "$x^2 + y^2$"
        );
        assert_eq!(
            extract(&rows, Pos::new(0, 0, 4), Pos::new(0, 0, 10)),
            "value $x^2 + y^2$"
        );
        assert_eq!(
            extract(&rows, Pos::new(0, 1, 0), Pos::new(0, 1, 6)),
            " done"
        );
    }

    /// Presses on one cell inside the window count up to three and then
    /// start again; a press elsewhere or too late counts one.
    #[test]
    fn presses_count_towards_double_and_triple_clicks() {
        let mut state = SelectionState::default();
        let t0 = Instant::now();
        let pos = Pos::new(0, 0, 3);
        let press = |state: &mut SelectionState, cell, at| {
            state.press(Region::Transcript, pos, cell, t0 + at)
        };
        assert_eq!(press(&mut state, (3, 1), Duration::ZERO), 1);
        assert_eq!(press(&mut state, (3, 1), Duration::from_millis(200)), 2);
        assert_eq!(press(&mut state, (3, 1), Duration::from_millis(400)), 3);
        assert_eq!(press(&mut state, (3, 1), Duration::from_millis(500)), 1);
        assert_eq!(press(&mut state, (4, 1), Duration::from_millis(600)), 1);
        assert_eq!(press(&mut state, (4, 1), Duration::from_millis(1100)), 1);
    }

    /// A press and a release on one cell is a click and leaves nothing
    /// selected; a drag hands back its range in reading order.
    #[test]
    fn a_click_selects_nothing_and_a_drag_its_range() {
        let mut state = SelectionState::default();
        let now = Instant::now();
        state.press(Region::Pager, Pos::new(0, 4, 2), (2, 5), now);
        state.drag(Pos::new(0, 4, 2));
        assert_eq!(
            state.release(),
            Release::Click(Region::Pager, Pos::new(0, 4, 2))
        );
        assert_eq!(state.current(), None);
        assert_eq!(state.release(), Release::Nothing);

        state.press(
            Region::Pager,
            Pos::new(0, 4, 2),
            (2, 5),
            now + MULTI_CLICK * 2,
        );
        state.drag(Pos::new(0, 2, 7));
        let Release::Copy(selection) = state.release() else {
            panic!("a drag hands back its selection");
        };
        assert_eq!(selection.range(), (Pos::new(0, 2, 7), Pos::new(0, 4, 2)));
        assert!(state.clear());
        assert!(!state.clear());
    }
}
