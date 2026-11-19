//! The transcript: its cells in order, where the view sits in them and
//! which row holds the focus. Following the bottom is the rest state; a
//! scroll away from it holds the view still while new cells arrive
//! below, and a chip offers the way back (and counts them) until the
//! view follows again.

use super::cells::Cell;
use super::render::Renderer;
use crate::config::CommandPrefix;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

const REVERSED: Style = Style::new().add_modifier(Modifier::REVERSED);
/// Lines one wheel notch moves the view.
pub(super) const WHEEL_LINES: i32 = 1;

/// The screen rows one cell took in the last frame, counted from the
/// view's top row.
struct RowSpan {
    cell: usize,
    first_row: u16,
    rows: u16,
}

/// The line at the top of the last frame, by the cell it belongs to and
/// the row inside that cell. The blank line between two cells is held by
/// the cell below it with `gap` set, so the cell above growing does not
/// carry it down.
#[derive(Clone, Copy)]
struct Anchor {
    cell: usize,
    row: usize,
    gap: bool,
}

pub(super) struct Transcript {
    cells: Vec<Cell>,
    /// What the greeting names; it is drawn whenever the transcript has
    /// no turn in it, on boot and again after an undo empties it.
    model: String,
    prefix: CommandPrefix,
    /// Lines between the bottom of the view and the bottom of the
    /// transcript; zero while the view follows new output.
    offset_from_bottom: usize,
    /// The offset the last frame was drawn at; what a scroll since has
    /// moved it by is applied to `anchor`.
    last_offset: usize,
    /// The line the last frame started on. A scrolled view is put back on
    /// it, so a cell above the view or on it growing or shrinking leaves
    /// the lines on screen where they were.
    anchor: Option<Anchor>,
    /// The line count the last frame laid out. A scrolled view with no
    /// anchor to go back to takes what the count grew by as output
    /// arrived below it.
    last_total: usize,
    /// Cells appended while the view was scrolled away from the bottom.
    unseen: usize,
    /// The row Ctrl+Up/Down or a click put the cursor on; only a cell
    /// that opens and closes can hold it.
    focus: Option<usize>,
    /// The Assistant cell the reply streams into. A cell pushed
    /// mid-stream (a background job's output, a notice) lands after it
    /// and the next delta still finds it.
    stream: Option<usize>,
    /// Set when the focus moved; the next frame scrolls so the focused
    /// row's header is in view.
    reveal_focus: bool,
    last_layout: Vec<RowSpan>,
    last_area: Rect,
    /// Goes up at every change to the cells, so a layout of all of them
    /// (the pager's) is looked at again only then.
    generation: u64,
    /// The generation each cell last changed at, by index. No two changes
    /// share one, so a cell laid out at some revision is known to read
    /// the same while it keeps it, wherever it has moved to.
    revisions: Vec<u64>,
    /// The bytes every call's whole output in the cells keeps.
    full_output_bytes: usize,
}

impl Transcript {
    pub(super) fn new(model: String, prefix: CommandPrefix) -> Self {
        Self {
            cells: vec![Cell::welcome(model.clone(), prefix)],
            model,
            prefix,
            offset_from_bottom: 0,
            last_offset: 0,
            anchor: None,
            last_total: 0,
            unseen: 0,
            focus: None,
            stream: None,
            reveal_focus: false,
            last_layout: Vec::new(),
            last_area: Rect::ZERO,
            generation: 1,
            revisions: vec![1],
            full_output_bytes: 0,
        }
    }

    pub(super) fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    fn next_revision(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Cell `index` to change, counted as changed.
    fn touch(&mut self, index: usize) -> Option<&mut Cell> {
        if index >= self.cells.len() {
            return None;
        }
        self.revisions[index] = self.next_revision();
        let cell = &mut self.cells[index];
        cell.forget_wrapped_lines();
        Some(cell)
    }

    /// The cells for laying out, with the revision of each: what they
    /// show is left as it is and only the line caches inside them fill,
    /// so nothing counts as changed.
    pub(super) fn cells_for_layout(&mut self) -> (&mut [Cell], &[u64]) {
        (&mut self.cells, &self.revisions)
    }

    pub(super) fn len(&self) -> usize {
        self.cells.len()
    }

    pub(super) fn cell_mut(&mut self, index: usize) -> Option<&mut Cell> {
        self.touch(index)
    }

    pub(super) fn last_mut(&mut self) -> Option<&mut Cell> {
        self.touch(self.cells.len().checked_sub(1)?)
    }

    /// Counts `delta` more bytes kept by the whole output of a call in
    /// some cell, then gives up the oldest calls' whole output until the
    /// total fits `budget`. Only a cell that gave something up changes.
    pub(super) fn account_full_output(&mut self, delta: isize, budget: usize) {
        self.full_output_bytes = self.full_output_bytes.saturating_add_signed(delta);
        if self.full_output_bytes <= budget {
            return;
        }
        for index in 0..self.cells.len() {
            let excess = self.full_output_bytes.saturating_sub(budget);
            if excess == 0 {
                break;
            }
            if self.cells[index].full_output_bytes() == 0 {
                continue;
            }
            let freed = self
                .touch(index)
                .map_or(0, |cell| cell.evict_full_outputs(excess));
            self.full_output_bytes = self.full_output_bytes.saturating_sub(freed);
        }
    }

    /// Drops every cell's cached lines and counts each cell as changed,
    /// so the transcript and the pager both style the text anew; for when
    /// the renderer itself changed.
    pub(super) fn forget_layout(&mut self) {
        for index in 0..self.cells.len() {
            if let Some(cell) = self.touch(index) {
                cell.forget_layout();
            }
        }
    }

    /// Appends a cell. The greeting gives way to the first real entry,
    /// whatever brings it: a submitted line or a notice.
    pub(super) fn push(&mut self, cell: Cell) {
        self.clear_welcome();
        if self.offset_from_bottom > 0 {
            self.unseen += 1;
        }
        self.full_output_bytes += cell.full_output_bytes();
        let revision = self.next_revision();
        self.revisions.push(revision);
        self.cells.push(cell);
    }

    /// Appends the cell a reply streams into and remembers where it is.
    pub(super) fn open_stream(&mut self, cell: Cell) {
        self.push(cell);
        self.stream = Some(self.cells.len() - 1);
    }

    pub(super) fn stream_cell_mut(&mut self) -> Option<&mut Cell> {
        let stream = self.stream?;
        self.touch(stream)
    }

    /// Forgets the streaming cell and hands it over to be finalised.
    pub(super) fn close_stream(&mut self) -> Option<&mut Cell> {
        let stream = self.stream.take()?;
        self.touch(stream)
    }

    /// Puts `cell` where another was; the row keeps its place and its
    /// focus. Whole output the caller moves from the old cell into the
    /// new one afterwards is counted again with `account_full_output`.
    pub(super) fn replace(&mut self, index: usize, cell: Cell) -> Cell {
        self.full_output_bytes += cell.full_output_bytes();
        self.revisions[index] = self.next_revision();
        let old = std::mem::replace(&mut self.cells[index], cell);
        self.full_output_bytes = self
            .full_output_bytes
            .saturating_sub(old.full_output_bytes());
        old
    }

    pub(super) fn remove(&mut self, index: usize) -> Cell {
        let unseen_from = self.cells.len().saturating_sub(self.unseen);
        self.next_revision();
        self.revisions.remove(index);
        let removed = self.cells.remove(index);
        self.full_output_bytes = self
            .full_output_bytes
            .saturating_sub(removed.full_output_bytes());
        if index >= unseen_from {
            self.unseen -= 1;
        }
        let shift = |held: Option<usize>| match held {
            Some(held) if held == index => None,
            Some(held) if held > index => Some(held - 1),
            other => other,
        };
        self.focus = shift(self.focus);
        self.stream = shift(self.stream);
        self.anchor = match self.anchor {
            Some(anchor) if anchor.cell == index && index < self.cells.len() => Some(Anchor {
                cell: index,
                row: 0,
                ..anchor
            }),
            Some(anchor) if anchor.cell == index => index.checked_sub(1).map(|cell| Anchor {
                cell,
                row: usize::MAX,
                gap: false,
            }),
            Some(anchor) if anchor.cell > index => Some(Anchor {
                cell: anchor.cell - 1,
                ..anchor
            }),
            other => other,
        };
        removed
    }

    pub(super) fn truncate(&mut self, len: usize) {
        self.unseen = self
            .unseen
            .saturating_sub(self.cells.len().saturating_sub(len));
        let cut: usize = self
            .cells
            .iter()
            .skip(len)
            .map(Cell::full_output_bytes)
            .sum();
        self.full_output_bytes = self.full_output_bytes.saturating_sub(cut);
        self.next_revision();
        self.cells.truncate(len);
        self.revisions.truncate(len);
        if self.focus.is_some_and(|focus| focus >= len) {
            self.focus = None;
        }
        if self.stream.is_some_and(|stream| stream >= len) {
            self.stream = None;
        }
        self.anchor = match self.anchor {
            Some(anchor) if anchor.gap && anchor.cell == len => {
                len.checked_sub(1).map(|cell| Anchor {
                    cell,
                    row: usize::MAX,
                    gap: false,
                })
            }
            Some(anchor) if anchor.cell >= len => None,
            other => other,
        };
        if self.cells.is_empty() {
            self.cells
                .push(Cell::welcome(self.model.clone(), self.prefix));
            let revision = self.next_revision();
            self.revisions.push(revision);
        }
    }

    /// Drops the greeting. Called ahead of reading `len` as a cell index
    /// so the index survives the next push.
    pub(super) fn clear_welcome(&mut self) {
        if matches!(self.cells.first(), Some(Cell::Welcome { .. })) {
            self.remove(0);
        }
    }

    fn welcome_only(&self) -> bool {
        matches!(self.cells.as_slice(), [Cell::Welcome { .. }])
    }

    pub(super) fn scrolled(&self) -> bool {
        self.offset_from_bottom > 0
    }

    pub(super) fn focused(&self) -> bool {
        self.focus.is_some()
    }

    /// Moves the view `delta` lines: down towards the bottom when
    /// positive, up when negative. Reaching the bottom resumes following.
    pub(super) fn scroll_lines(&mut self, delta: i32) {
        let offset = i64::try_from(self.offset_from_bottom).unwrap_or(i64::MAX) - i64::from(delta);
        self.offset_from_bottom = usize::try_from(offset).unwrap_or(0);
        if self.offset_from_bottom == 0 {
            self.unseen = 0;
        }
    }

    pub(super) fn scroll_page(&mut self, direction: i32) {
        let page = i32::from(self.last_area.height.max(1));
        self.scroll_lines(direction.saturating_mul(page));
    }

    pub(super) fn scroll_to_bottom(&mut self) {
        self.offset_from_bottom = 0;
        self.unseen = 0;
    }

    fn collapsible_indices(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        self.cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.collapsible())
            .map(|(index, _)| index)
    }

    /// Focus the previous row that opens and closes; from no focus, the
    /// last one.
    pub(super) fn focus_prev(&mut self) {
        let target = match self.focus {
            None => self.collapsible_indices().next_back(),
            Some(focus) => self
                .collapsible_indices()
                .rev()
                .find(|index| *index < focus)
                .or(Some(focus)),
        };
        self.set_focus(target);
    }

    /// Focus the next row that opens and closes; from no focus, the
    /// first one.
    pub(super) fn focus_next(&mut self) {
        let target = match self.focus {
            None => self.collapsible_indices().next(),
            Some(focus) => self
                .collapsible_indices()
                .find(|index| *index > focus)
                .or(Some(focus)),
        };
        self.set_focus(target);
    }

    fn set_focus(&mut self, target: Option<usize>) {
        if target.is_some() {
            self.focus = target;
            self.reveal_focus = true;
        }
    }

    pub(super) fn toggle_focused(&mut self) {
        if let Some(focus) = self.focus
            && let Some(cell) = self.touch(focus)
        {
            cell.toggle();
        }
    }

    pub(super) fn clear_focus(&mut self) {
        self.focus = None;
    }

    /// A left click on a row that opens and closes focuses it and flips
    /// it; a click anywhere else does nothing.
    pub(super) fn click(&mut self, column: u16, row: u16) {
        if !self.last_area.contains(Position::new(column, row)) {
            return;
        }
        let row = row - self.last_area.y;
        let Some(index) = self
            .last_layout
            .iter()
            .find(|span| span.first_row <= row && row < span.first_row + span.rows)
            .map(|span| span.cell)
        else {
            return;
        };
        if self.cells.get(index).is_some_and(Cell::collapsible)
            && let Some(cell) = self.touch(index)
        {
            cell.toggle();
            self.focus = Some(index);
        }
    }

    /// Lays the cells out for `area` and hands back the rows to draw.
    /// The scroll offset is clamped to what the transcript can show, and
    /// the rows each cell received are kept so a click can find its cell.
    pub(super) fn view(&mut self, area: Rect, renderer: &mut Renderer) -> TranscriptView {
        // An empty area lays nothing out, so the frame after it is
        // compared with the last one that did and keeps its anchor.
        if area.is_empty() {
            self.last_layout.clear();
            return TranscriptView {
                lines: Vec::new(),
                centre: false,
                chip: None,
            };
        }
        let previous = std::mem::replace(&mut self.last_area, area);
        let resized = area.width != previous.width;
        let centre = self.welcome_only();
        // A blank line separates each cell from the one before; `headers`
        // holds the line each cell starts on.
        let mut cell_lines = Vec::with_capacity(self.cells.len());
        let mut headers: Vec<usize> = Vec::with_capacity(self.cells.len());
        let mut total = 0;
        for cell in &mut self.cells {
            if !headers.is_empty() {
                total += 1;
            }
            headers.push(total);
            let lines = cell.cached_lines(area.width, renderer);
            total += lines.len();
            cell_lines.push(lines);
        }

        let height = usize::from(area.height);
        // A new width re-wraps every cell, so a row inside one no longer
        // names the same text; the offset is kept as it was instead.
        if self.offset_from_bottom > 0 && !resized {
            match self.anchor.and_then(|anchor| anchor.line(&headers, total)) {
                Some(top) => {
                    self.offset_from_bottom = anchored_offset(
                        self.last_offset,
                        self.offset_from_bottom,
                        top + usize::from(previous.height),
                        total,
                    );
                }
                None => self.offset_from_bottom += total.saturating_sub(self.last_total),
            }
        }
        self.last_total = total;
        self.offset_from_bottom = self.offset_from_bottom.min(total.saturating_sub(height));
        if std::mem::take(&mut self.reveal_focus)
            && let Some(header) = self.focus.map(|focus| headers[focus])
        {
            self.offset_from_bottom = revealed(self.offset_from_bottom, header, total, height);
        }
        if self.offset_from_bottom == 0 {
            self.unseen = 0;
        }
        let end = total - self.offset_from_bottom;
        let start = end.saturating_sub(height);
        self.last_offset = self.offset_from_bottom;
        self.anchor = headers
            .partition_point(|header| *header <= start)
            .checked_sub(1)
            .map(|cell| match headers.get(cell + 1) {
                Some(next) if start + 1 == *next => Anchor {
                    cell: cell + 1,
                    row: 0,
                    gap: true,
                },
                _ => Anchor {
                    cell,
                    row: start - headers[cell],
                    gap: false,
                },
            });

        let mut lines = Vec::with_capacity(end - start);
        self.last_layout.clear();
        let first = self.anchor.map_or(0, |anchor| anchor.cell);
        for (index, cell_rows) in cell_lines.iter().enumerate().skip(first) {
            let header = headers[index];
            if header > end {
                break;
            }
            if index > 0 && header > start {
                lines.push(Line::default());
            }
            let from = start.saturating_sub(header);
            let to = cell_rows.len().min(end - header);
            if from >= to {
                continue;
            }
            self.last_layout.push(RowSpan {
                cell: index,
                first_row: u16::try_from(header + from - start).unwrap_or(u16::MAX),
                rows: u16::try_from(to - from).unwrap_or(u16::MAX),
            });
            lines.extend(cell_rows[from..to].iter().enumerate().map(|(row, line)| {
                if from + row == 0 && self.focus == Some(index) {
                    line.clone().patch_style(REVERSED)
                } else {
                    line.clone()
                }
            }));
        }

        let chip = (self.offset_from_bottom > 0).then(|| {
            if self.unseen > 0 {
                format!("{} new below  End = follow", self.unseen)
            } else {
                "End = follow".to_string()
            }
        });
        TranscriptView {
            lines,
            centre,
            chip,
        }
    }
}

/// The offset that moves the view the least it has to for line `header`
/// to be on it.
fn revealed(offset: usize, header: usize, total: usize, height: usize) -> usize {
    let end = total - offset;
    let start = end.saturating_sub(height);
    if header < start {
        total - (header + height).min(total)
    } else if header >= end {
        total - (header + 1)
    } else {
        offset
    }
}

/// The offset that ends the view at line `end`, where the last frame
/// drawn at `last_offset` ended, moved by the scroll that has taken the
/// offset to `offset` since. Ending where it ended keeps the bottom row
/// still when the view changes height, as it stays following. It is at
/// least 1: cells below the view shrinking can bring the bottom up past
/// that line, and the view stays scrolled until the user returns.
fn anchored_offset(last_offset: usize, offset: usize, end: usize, total: usize) -> usize {
    let wide = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    let end = wide(end) + wide(last_offset) - wide(offset);
    usize::try_from(wide(total) - end).unwrap_or(0).max(1)
}

impl Anchor {
    /// Where the anchored row is now, given each cell's first line; a
    /// cell that shrank past the row gives the line after its last.
    fn line(self, headers: &[usize], total: usize) -> Option<usize> {
        let header = *headers.get(self.cell)?;
        if self.gap {
            return Some(header.saturating_sub(1));
        }
        let end = headers.get(self.cell + 1).map_or(total, |next| next - 1);
        Some(header + self.row.min(end - header))
    }
}

/// The rows of the transcript the view shows, with the follow chip while
/// the view is scrolled away from the bottom. The greeting alone sits in
/// the middle of the area rather than at its bottom.
pub(super) struct TranscriptView {
    lines: Vec<Line<'static>>,
    centre: bool,
    chip: Option<String>,
}

impl Widget for TranscriptView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let mut content = area;
        if self.centre {
            let spare = usize::from(area.height).saturating_sub(self.lines.len());
            let top = u16::try_from(spare / 2).unwrap_or(0);
            content.y += top;
            content.height -= top;
        }
        let skip = self.lines.len().saturating_sub(usize::from(content.height));
        let visible: Vec<Line<'static>> = self.lines.into_iter().skip(skip).collect();
        Paragraph::new(visible).render(content, buf);
        if let Some(chip) = self.chip {
            let last_row = Rect {
                y: area.bottom() - 1,
                height: 1,
                ..area
            };
            Paragraph::new(Line::from(Span::styled(chip, REVERSED)))
                .right_aligned()
                .render(last_row, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, CommandPrefix};
    use crate::tui::cells::{ToolStatus, append_output};
    use std::time::{Duration, Instant};

    fn transcript() -> (Transcript, Renderer) {
        let config = AppConfig {
            highlight: false,
            ..Default::default()
        };
        let renderer = Renderer::new(config.render_options().unwrap()).unwrap();
        let mut transcript = Transcript::new("m".to_string(), CommandPrefix::Dot);
        transcript.clear_welcome();
        (transcript, renderer)
    }

    fn tool(call_id: &str) -> Cell {
        Cell::tool(
            call_id.to_string(),
            format!("Call fs_read {call_id}"),
            Instant::now(),
        )
    }

    fn area(height: u16) -> Rect {
        Rect::new(0, 0, 40, height)
    }

    /// Appends `lines` lines of output to the tool row at `index`.
    fn grow_tool(transcript: &mut Transcript, index: usize, call_id: &str, lines: usize) {
        let (target, _) = transcript
            .cell_mut(index)
            .unwrap()
            .output_for(call_id)
            .unwrap();
        let text: String = (0..lines).map(|n| format!("{call_id} out {n}\n")).collect();
        append_output(target, text.as_bytes());
    }

    fn finish_tool(transcript: &mut Transcript, index: usize) {
        if let Some(Cell::Tool { status, .. }) = transcript.cell_mut(index) {
            *status = ToolStatus::Ok {
                elapsed: Duration::from_secs(1),
            };
        }
    }

    fn rows(view: &TranscriptView) -> Vec<String> {
        view.lines.iter().map(ToString::to_string).collect()
    }

    /// A tool row whose whole output holds `text`, counted as pushed.
    fn tool_with_output(transcript: &mut Transcript, call_id: &str, text: &str) {
        transcript.push(tool(call_id));
        let index = transcript.len() - 1;
        let (_, full) = transcript
            .cell_mut(index)
            .unwrap()
            .output_for(call_id)
            .unwrap();
        let grew = full.push(text);
        transcript.account_full_output(grew, usize::MAX);
    }

    /// Output under the budget changes no cell; over it, the oldest calls
    /// give up their whole output and only their cells count as changed.
    #[test]
    fn the_budget_gives_up_the_oldest_output_and_changes_nothing_else() {
        let (mut transcript, mut renderer) = transcript();
        for n in 1..=3 {
            tool_with_output(
                &mut transcript,
                &format!("c{n}"),
                &format!("call {n} out\n"),
            );
        }
        let each = "call 1 out\n".len();
        let generation = transcript.generation();
        transcript.account_full_output(0, 3 * each);
        assert_eq!(
            transcript.generation(),
            generation,
            "a budget pass that gave nothing up counted as a change"
        );
        let revisions = transcript.revisions.clone();
        transcript.account_full_output(0, 2 * each - 1);
        assert_eq!(transcript.full_output_bytes, each);
        let kept: Vec<usize> = transcript
            .cells()
            .iter()
            .map(Cell::full_output_bytes)
            .collect();
        assert_eq!(kept, [0, 0, each]);
        assert_ne!(transcript.revisions[0], revisions[0]);
        assert_ne!(transcript.revisions[1], revisions[1]);
        assert_eq!(transcript.revisions[2], revisions[2]);
        let note = transcript.cells_for_layout().0[0].full_lines(80, &mut renderer);
        assert!(
            note.iter()
                .any(|line| line.to_string() == format!("[... {each} earlier bytes not kept]")),
            "{note:?}"
        );
        transcript.remove(2);
        assert_eq!(transcript.full_output_bytes, 0);
    }

    /// The running count of whole-output bytes stays the sum over the
    /// cells through a replace, a remove and a truncate.
    #[test]
    fn the_full_output_count_matches_the_cells_after_replace_remove_and_truncate() {
        fn recounted(transcript: &Transcript) -> usize {
            transcript.cells().iter().map(Cell::full_output_bytes).sum()
        }
        let (mut transcript, _) = transcript();
        tool_with_output(&mut transcript, "c1", "one\n");
        tool_with_output(&mut transcript, "c2", "second call\n");
        tool_with_output(&mut transcript, "c3", "the third call's output\n");
        tool_with_output(&mut transcript, "c4", "four 4\n");
        assert_eq!(transcript.full_output_bytes, 47);
        assert_eq!(transcript.full_output_bytes, recounted(&transcript));

        let mut swapped = tool("c9");
        let (_, full) = swapped.output_for("c9").unwrap();
        full.push("a much longer replacement output\n");
        transcript.replace(1, swapped);
        assert_eq!(transcript.full_output_bytes, 68);
        assert_eq!(transcript.full_output_bytes, recounted(&transcript));

        transcript.remove(0);
        assert_eq!(transcript.full_output_bytes, 64);
        assert_eq!(transcript.full_output_bytes, recounted(&transcript));

        transcript.truncate(1);
        assert_eq!(transcript.full_output_bytes, 33);
        assert_eq!(transcript.full_output_bytes, recounted(&transcript));
    }

    /// Ctrl+Up from nothing takes the last row that opens; Ctrl+Down the
    /// first; a bookkeeping call and a user line are skipped over.
    #[test]
    fn focus_walks_the_rows_that_open_and_skips_the_rest() {
        let (mut transcript, _) = transcript();
        transcript.push(Cell::user("hi"));
        transcript.push(tool("c1"));
        transcript.push(Cell::tool(
            "c2".to_string(),
            "Call memory__read {}".to_string(),
            Instant::now(),
        ));
        transcript.push(tool("c3"));
        transcript.focus_prev();
        assert_eq!(transcript.focus, Some(3));
        transcript.focus_prev();
        assert_eq!(transcript.focus, Some(1));
        transcript.focus_prev();
        assert_eq!(transcript.focus, Some(1), "focus ran off the first row");
        transcript.clear_focus();
        transcript.focus_next();
        assert_eq!(transcript.focus, Some(1));
        transcript.focus_next();
        assert_eq!(transcript.focus, Some(3));
        transcript.toggle_focused();
        assert!(matches!(
            transcript.cells()[3],
            Cell::Tool { expanded: true, .. }
        ));
        transcript.truncate(2);
        assert_eq!(transcript.focus, None);
    }

    /// Removing a cell ahead of the focused one keeps the focus on the
    /// same row; removing the focused one clears it.
    #[test]
    fn removing_cells_keeps_the_focus_on_its_row() {
        let (mut transcript, _) = transcript();
        transcript.push(tool("c1"));
        transcript.push(tool("c2"));
        transcript.focus_prev();
        assert_eq!(transcript.focus, Some(1));
        transcript.remove(0);
        assert_eq!(transcript.focus, Some(0));
        transcript.remove(0);
        assert_eq!(transcript.focus, None);
    }

    /// Removing a cell that arrived below a scrolled view takes it out
    /// of the chip's count; removing one above the view leaves it.
    #[test]
    fn removing_an_unseen_cell_drops_it_from_the_count() {
        let (mut transcript, mut renderer) = transcript();
        for i in 0..10 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        transcript.view(area(5), &mut renderer);
        transcript.scroll_lines(-100);
        transcript.push(Cell::user("late 1"));
        transcript.push(Cell::user("late 2"));
        assert_eq!(transcript.unseen, 2);
        transcript.remove(0);
        assert_eq!(transcript.unseen, 2);
        transcript.remove(9);
        assert_eq!(transcript.unseen, 1);
        transcript.remove(9);
        assert_eq!(transcript.unseen, 0);
    }

    /// Truncating below a scrolled view takes the cut cells out of the
    /// chip's count and no more than that.
    #[test]
    fn truncating_unseen_cells_drops_them_from_the_count() {
        let (mut transcript, mut renderer) = transcript();
        for i in 0..10 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        transcript.view(area(5), &mut renderer);
        transcript.scroll_lines(-100);
        transcript.push(Cell::user("late 1"));
        transcript.push(Cell::user("late 2"));
        transcript.push(Cell::user("late 3"));
        assert_eq!(transcript.unseen, 3);
        transcript.truncate(12);
        assert_eq!(transcript.unseen, 2);
        transcript.truncate(5);
        assert_eq!(transcript.unseen, 0);
    }

    /// The offset is clamped to the lines above the view, a push while
    /// scrolled counts as unseen and grows the offset by what it added,
    /// and reaching the bottom forgets the count.
    #[test]
    fn scrolling_clamps_and_counts_what_arrives_below() {
        let (mut transcript, mut renderer) = transcript();
        for i in 0..10 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        // 10 cells and 9 blank separators: 19 lines, 5 on screen.
        transcript.view(area(5), &mut renderer);
        transcript.scroll_lines(-100);
        transcript.view(area(5), &mut renderer);
        assert_eq!(transcript.offset_from_bottom, 14);
        transcript.push(Cell::user("late"));
        assert_eq!(transcript.unseen, 1);
        let view = transcript.view(area(5), &mut renderer);
        assert_eq!(view.chip.as_deref(), Some("1 new below  End = follow"));
        assert_eq!(transcript.offset_from_bottom, 16, "the view slid");
        transcript.scroll_page(1);
        let view = transcript.view(area(5), &mut renderer);
        assert_eq!(transcript.offset_from_bottom, 11);
        assert_eq!(transcript.unseen, 1, "a partial scroll forgot the count");
        assert_eq!(view.chip.as_deref(), Some("1 new below  End = follow"));
        transcript.scroll_to_bottom();
        let view = transcript.view(area(5), &mut renderer);
        assert_eq!(transcript.offset_from_bottom, 0);
        assert_eq!(view.chip, None);
        assert_eq!(transcript.unseen, 0);
    }

    /// Cells landing below a scrolled view leave the lines on screen
    /// where they were; only the chip's count tells they arrived.
    #[test]
    fn a_scrolled_view_keeps_its_lines_while_cells_arrive_below() {
        let (mut transcript, mut renderer) = transcript();
        for i in 0..10 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        transcript.view(area(5), &mut renderer);
        transcript.scroll_lines(-6);
        let before = transcript.view(area(5), &mut renderer);
        for i in 0..3 {
            transcript.push(Cell::user(&format!("late {i}")));
        }
        let after = transcript.view(area(5), &mut renderer);
        assert_eq!(
            after.lines[0].to_string(),
            before.lines[0].to_string(),
            "the first visible line moved"
        );
        assert_eq!(after.chip.as_deref(), Some("3 new below  End = follow"));
        transcript.scroll_to_bottom();
        let view = transcript.view(area(5), &mut renderer);
        assert_eq!(view.chip, None);
        assert_eq!(view.lines.last().unwrap().to_string(), "> late 2");
    }

    /// A narrower view re-wraps every cell onto more lines; that is not
    /// output arriving below and must not push the offset up. Cells
    /// pushed after the resize still do.
    #[test]
    fn a_resize_does_not_inflate_a_scrolled_offset() {
        let (mut transcript, mut renderer) = transcript();
        for i in 0..10 {
            transcript.push(Cell::user(&format!("line {i} {}", "x".repeat(30))));
        }
        transcript.view(area(5), &mut renderer);
        transcript.scroll_lines(-6);
        transcript.view(area(5), &mut renderer);
        assert_eq!(transcript.offset_from_bottom, 6);
        let narrow = Rect::new(0, 0, 20, 5);
        transcript.view(narrow, &mut renderer);
        assert_eq!(
            transcript.offset_from_bottom, 6,
            "the re-wrap counted as new output"
        );
        transcript.push(Cell::user("late"));
        transcript.view(narrow, &mut renderer);
        assert_eq!(
            transcript.offset_from_bottom, 8,
            "the cell and its separator"
        );
    }

    /// Tool rows above the view at 0, on it at 11 and below it at 15,
    /// with user lines between; all three are running.
    fn tools_around_users() -> (Transcript, Renderer) {
        let (mut transcript, renderer) = transcript();
        transcript.push(tool("a"));
        grow_tool(&mut transcript, 0, "a", 3);
        for i in 1..=10 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        transcript.push(tool("b"));
        grow_tool(&mut transcript, 11, "b", 3);
        for i in 12..=14 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        transcript.push(tool("c"));
        (transcript, renderer)
    }

    /// While scrolled, a row on the view closing as its call ends, a row
    /// above it growing and one below it growing all leave the lines
    /// above the change where they were; a cell pushed below is counted.
    #[test]
    fn a_scrolled_view_holds_its_lines_while_other_cells_change_size() {
        let (mut transcript, mut renderer) = tools_around_users();
        transcript.view(area(8), &mut renderer);
        transcript.scroll_lines(-10);
        let before = rows(&transcript.view(area(8), &mut renderer));
        assert_eq!(before[0], "> line 9", "{before:?}");
        assert!(before[4].starts_with("fs_read("), "{before:?}");

        finish_tool(&mut transcript, 11);
        let closed = rows(&transcript.view(area(8), &mut renderer));
        assert_eq!(closed[..4], before[..4], "the view moved as a row closed");
        assert!(closed[4].contains("ok"), "{closed:?}");
        assert_eq!(closed[6], "> line 12", "{closed:?}");

        grow_tool(&mut transcript, 0, "a", 5);
        let view = rows(&transcript.view(area(8), &mut renderer));
        assert_eq!(view, closed, "the view moved as a row above it grew");

        grow_tool(&mut transcript, 15, "c", 5);
        transcript.push(Cell::user("late"));
        let view = transcript.view(area(8), &mut renderer);
        assert_eq!(rows(&view), closed, "the view moved as cells below grew");
        assert_eq!(view.chip.as_deref(), Some("1 new below  End = follow"));

        transcript.scroll_lines(1);
        let view = rows(&transcript.view(area(8), &mut renderer));
        assert_eq!(
            view[..7],
            closed[1..],
            "a line down moved by other than a line"
        );
    }

    /// A tool row's wrapped lines are kept between frames, so each way
    /// it changes has to show on the next one: output arriving, the call
    /// ending, the row opening and the view changing width.
    #[test]
    fn a_tool_row_redraws_after_each_change() {
        let (mut transcript, mut renderer) = transcript();
        transcript.push(tool("a"));
        let has = |view: &TranscriptView, text: &str| rows(view).iter().any(|row| row == text);
        let view = transcript.view(area(20), &mut renderer);
        assert!(!has(&view, "a out 1"), "{:?}", rows(&view));

        grow_tool(&mut transcript, 0, "a", 2);
        let view = transcript.view(area(20), &mut renderer);
        assert!(has(&view, "a out 1"), "output: {:?}", rows(&view));

        finish_tool(&mut transcript, 0);
        let view = transcript.view(area(20), &mut renderer);
        assert!(!has(&view, "a out 1"), "status: {:?}", rows(&view));
        assert!(rows(&view).iter().any(|row| row.ends_with("ok")));

        transcript.focus_prev();
        transcript.toggle_focused();
        let view = transcript.view(area(20), &mut renderer);
        assert!(has(&view, "a out 1"), "opened: {:?}", rows(&view));

        let wide = transcript.view(area(20), &mut renderer).lines.len();
        let narrow = transcript
            .view(Rect::new(0, 0, 4, 20), &mut renderer)
            .lines
            .len();
        assert!(narrow > wide, "width: {wide} then {narrow}");
    }

    /// A row the view starts inside closing past that row leaves the view
    /// starting on the line after it, not on text further up or down.
    #[test]
    fn a_row_closing_under_the_top_of_a_scrolled_view_keeps_what_follows_it() {
        let (mut transcript, mut renderer) = tools_around_users();
        transcript.view(area(8), &mut renderer);
        transcript.scroll_lines(-3);
        let before = rows(&transcript.view(area(8), &mut renderer));
        assert_eq!(before[0], "b out 1", "{before:?}");
        finish_tool(&mut transcript, 11);
        let after = rows(&transcript.view(area(8), &mut renderer));
        assert_eq!(after[..2], ["", "> line 12"], "{after:?}");
    }

    /// Scrolled away with nothing new below, the chip still offers the
    /// way back; it is the only sign the view is not following.
    #[test]
    fn a_scrolled_view_with_nothing_new_still_offers_the_way_back() {
        let (mut transcript, mut renderer) = transcript();
        for i in 0..10 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        transcript.view(area(5), &mut renderer);
        transcript.scroll_lines(-3);
        let view = transcript.view(area(5), &mut renderer);
        assert_eq!(view.chip.as_deref(), Some("End = follow"));
        transcript.scroll_lines(3);
        let view = transcript.view(area(5), &mut renderer);
        assert_eq!(view.chip, None);
    }

    /// Truncating everything away brings the greeting back; the next
    /// turn clears it as on boot.
    #[test]
    fn truncating_to_nothing_reseeds_the_greeting() {
        let (mut transcript, _) = transcript();
        transcript.push(Cell::user("hi"));
        transcript.push(Cell::assistant("hello".to_string(), true));
        transcript.truncate(0);
        assert!(matches!(transcript.cells(), [Cell::Welcome { .. }]));
        assert!(transcript.welcome_only());
        transcript.clear_welcome();
        assert!(transcript.cells().is_empty());
        transcript.push(Cell::user("again"));
        transcript.truncate(1);
        assert_eq!(
            transcript.len(),
            1,
            "a truncation that leaves cells adds nothing"
        );
    }

    /// A zero-sized area (the todo panel has the whole width) lays
    /// nothing out and leaves no rows for a click to land on.
    #[test]
    fn an_empty_area_lays_nothing_out() {
        let (mut transcript, mut renderer) = transcript();
        transcript.push(Cell::user("hi"));
        transcript.push(tool("c1"));
        transcript.view(area(10), &mut renderer);
        assert_eq!(transcript.last_layout.len(), 2);
        let view = transcript.view(Rect::ZERO, &mut renderer);
        assert!(view.lines.is_empty());
        assert_eq!(view.chip, None);
        assert!(transcript.last_layout.is_empty());
        transcript.click(0, 0);
        assert_eq!(transcript.focus, None);
    }

    /// Ten user lines, scrolled 6 lines up a 5-line view, which then
    /// starts on `> line 4`.
    fn scrolled_users() -> (Transcript, Renderer, Vec<String>) {
        let (mut transcript, mut renderer) = transcript();
        for i in 0..10 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        transcript.view(area(5), &mut renderer);
        transcript.scroll_lines(-6);
        let before = rows(&transcript.view(area(5), &mut renderer));
        assert_eq!(before[0], "> line 4", "{before:?}");
        (transcript, renderer, before)
    }

    /// Removing the cell the view starts on puts the view on the cell
    /// after it, and output arriving below then leaves it there.
    #[test]
    fn removing_the_top_cell_of_a_scrolled_view_holds_it_on_the_next() {
        let (mut transcript, mut renderer, before) = scrolled_users();
        transcript.remove(4);
        for i in 0..3 {
            transcript.push(Cell::user(&format!("late {i}")));
        }
        let view = transcript.view(area(5), &mut renderer);
        assert_eq!(rows(&view)[0], before[2], "{:?}", rows(&view));
        assert_eq!(view.chip.as_deref(), Some("3 new below  End = follow"));
    }

    /// Five user lines, a running tool row and ten more user lines, in a
    /// 5-line view scrolled to start on the blank line after the tool row.
    fn scrolled_to_the_gap_after_a_tool() -> (Transcript, Renderer, Vec<String>) {
        let (mut transcript, mut renderer) = transcript();
        for i in 0..5 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        transcript.push(tool("a"));
        grow_tool(&mut transcript, 5, "a", 2);
        for i in 6..16 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        let all = rows(&transcript.view(area(100), &mut renderer));
        let top = all.iter().position(|row| row == "> line 6").unwrap() - 1;
        let offset = i32::try_from(all.len() - top - 5).unwrap();
        transcript.view(area(5), &mut renderer);
        transcript.scroll_lines(-offset);
        let before = rows(&transcript.view(area(5), &mut renderer));
        assert_eq!(before[..2], ["", "> line 6"], "{before:?}");
        (transcript, renderer, before)
    }

    /// A view starting on the blank line after a running tool row stays
    /// on it as the row grows, rather than starting on the new output.
    #[test]
    fn a_view_starting_between_cells_holds_as_the_cell_above_grows() {
        let (mut transcript, mut renderer, before) = scrolled_to_the_gap_after_a_tool();
        let offset = transcript.offset_from_bottom;
        grow_tool(&mut transcript, 5, "a", 3);
        let after = rows(&transcript.view(area(5), &mut renderer));
        assert_eq!(after, before);
        assert_eq!(transcript.offset_from_bottom, offset);
    }

    /// Removing the cell after the blank line the view starts on leaves
    /// the view on that blank line, now before the cell that followed.
    #[test]
    fn removing_the_cell_after_a_view_starting_between_cells_keeps_the_gap() {
        let (mut transcript, mut renderer, _) = scrolled_to_the_gap_after_a_tool();
        transcript.remove(6);
        transcript.push(Cell::user("late"));
        let view = transcript.view(area(5), &mut renderer);
        assert_eq!(rows(&view)[..2], ["", "> line 7"], "{:?}", rows(&view));
        assert_eq!(view.chip.as_deref(), Some("1 new below  End = follow"));
    }

    /// Output arriving while the view had no room (the todo panel took
    /// it) leaves the lines it showed where they were once it is back.
    #[test]
    fn output_arriving_while_the_view_is_hidden_does_not_move_it() {
        let (mut transcript, mut renderer, before) = scrolled_users();
        transcript.view(Rect::ZERO, &mut renderer);
        for i in 0..3 {
            transcript.push(Cell::user(&format!("late {i}")));
        }
        let view = transcript.view(area(5), &mut renderer);
        assert_eq!(rows(&view), before);
        assert_eq!(view.chip.as_deref(), Some("3 new below  End = follow"));
    }

    /// With no anchor to go back to, as after a rewind cut the cell the
    /// view started on, what the line count grew by still counts as
    /// output below.
    #[test]
    fn a_scrolled_view_without_an_anchor_still_makes_room_for_output_below() {
        let (mut transcript, mut renderer, before) = scrolled_users();
        transcript.anchor = None;
        for i in 0..3 {
            transcript.push(Cell::user(&format!("late {i}")));
        }
        let view = transcript.view(area(5), &mut renderer);
        assert_eq!(rows(&view), before);
    }

    /// A running row below the view closing brings the bottom up past
    /// the view's last line; the view stays scrolled, with its count,
    /// rather than going back to following.
    #[test]
    fn a_row_closing_below_a_scrolled_view_keeps_it_scrolled() {
        let (mut transcript, mut renderer) = transcript();
        for i in 0..10 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        transcript.push(tool("b"));
        grow_tool(&mut transcript, 10, "b", 5);
        transcript.view(area(5), &mut renderer);
        transcript.scroll_lines(-2);
        transcript.view(area(5), &mut renderer);
        transcript.push(Cell::user("late"));
        transcript.view(area(5), &mut renderer);
        finish_tool(&mut transcript, 10);
        let view = transcript.view(area(5), &mut renderer);
        assert!(transcript.scrolled());
        assert_eq!(view.chip.as_deref(), Some("1 new below  End = follow"));
        transcript.push(Cell::user("later"));
        let view = transcript.view(area(5), &mut renderer);
        assert!(transcript.scrolled(), "output below scrolled the view");
        assert_eq!(view.chip.as_deref(), Some("2 new below  End = follow"));
    }

    /// A transcript shorter than the view has nowhere to scroll to, so
    /// a scroll up leaves it following.
    #[test]
    fn a_transcript_shorter_than_the_view_stays_following() {
        let (mut transcript, mut renderer) = transcript();
        transcript.push(Cell::user("hi"));
        transcript.view(area(5), &mut renderer);
        transcript.scroll_lines(-1);
        let view = transcript.view(area(5), &mut renderer);
        assert!(!transcript.scrolled());
        assert_eq!(view.chip, None);
    }

    /// Focusing a row that is scrolled out of view brings its header
    /// onto the screen, by the least movement that does.
    #[test]
    fn focus_reveals_its_row() {
        let (mut transcript, mut renderer) = transcript();
        transcript.push(tool("c1"));
        for i in 0..10 {
            transcript.push(Cell::user(&format!("line {i}")));
        }
        transcript.push(tool("c2"));
        transcript.view(area(4), &mut renderer);
        transcript.focus_prev();
        transcript.view(area(4), &mut renderer);
        assert_eq!(
            transcript.offset_from_bottom, 0,
            "the last row was in view already"
        );
        transcript.focus_prev();
        transcript.view(area(4), &mut renderer);
        // 12 cells, 11 separators and a second line on each running tool
        // row: 25 lines; the first row's header is line 0 and the view
        // must start there.
        assert_eq!(transcript.offset_from_bottom, 21);
        assert_eq!(transcript.last_layout[0].cell, 0);
        assert_eq!(transcript.last_layout[0].first_row, 0);
        transcript.focus_next();
        transcript.view(area(4), &mut renderer);
        // The last row's header is line 23; the view stops once that is
        // its bottom line, leaving the row's own second line below.
        assert_eq!(transcript.offset_from_bottom, 1);
    }

    /// A click lands on the cell that took that screen row in the last
    /// frame; a row that does not open ignores it.
    #[test]
    fn a_click_maps_through_the_last_layout() {
        let (mut transcript, mut renderer) = transcript();
        transcript.push(Cell::user("hi"));
        transcript.push(tool("c1"));
        transcript.view(Rect::new(0, 2, 40, 10), &mut renderer);
        // Three lines drawn from the top of the area: the user line on
        // its first row, a blank, then the tool row.
        transcript.click(5, 2);
        assert_eq!(transcript.focus, None);
        let generation = transcript.generation();
        transcript.click(5, 2);
        assert_eq!(
            transcript.generation(),
            generation,
            "a click that changed nothing counted as a change"
        );
        transcript.click(5, 2 + 2);
        assert_eq!(transcript.focus, Some(1));
        assert_ne!(transcript.generation(), generation);
        assert!(matches!(
            transcript.cells()[1],
            Cell::Tool { expanded: true, .. }
        ));
        transcript.click(5, 2 + 2);
        assert!(matches!(
            transcript.cells()[1],
            Cell::Tool {
                expanded: false,
                ..
            }
        ));
        transcript.click(5, 20);
        assert_eq!(
            transcript.focus,
            Some(1),
            "a click outside the area did something"
        );
    }
}
