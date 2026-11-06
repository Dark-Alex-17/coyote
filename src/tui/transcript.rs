//! The transcript: its cells in order, where the view sits in them and
//! which row holds the focus. Following the bottom is the rest state; a
//! scroll away from it holds the view still while new cells arrive
//! below, and a chip counts them until the view follows again.

use super::cells::Cell;
use super::render::Renderer;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

const REVERSED: Style = Style::new().add_modifier(Modifier::REVERSED);
/// Lines one wheel notch moves the view.
pub(super) const WHEEL_LINES: i32 = 3;

/// The screen rows one cell took in the last frame, counted from the
/// view's top row.
struct RowSpan {
    cell: usize,
    first_row: u16,
    rows: u16,
}

pub(super) struct Transcript {
    cells: Vec<Cell>,
    /// Lines between the bottom of the view and the bottom of the
    /// transcript; zero while the view follows new output.
    offset_from_bottom: usize,
    /// Cells appended while the view was scrolled away from the bottom.
    unseen: usize,
    /// The row Ctrl+Up/Down or a click put the cursor on; only a cell
    /// that opens and closes can hold it.
    focus: Option<usize>,
    /// Set when the focus moved; the next frame scrolls so the focused
    /// row's header is in view.
    reveal_focus: bool,
    last_layout: Vec<RowSpan>,
    last_area: Rect,
}

impl Transcript {
    pub(super) fn new(welcome: Cell) -> Self {
        Self {
            cells: vec![welcome],
            offset_from_bottom: 0,
            unseen: 0,
            focus: None,
            reveal_focus: false,
            last_layout: Vec::new(),
            last_area: Rect::ZERO,
        }
    }

    pub(super) fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub(super) fn len(&self) -> usize {
        self.cells.len()
    }

    pub(super) fn cell_mut(&mut self, index: usize) -> Option<&mut Cell> {
        self.cells.get_mut(index)
    }

    pub(super) fn cells_mut(&mut self) -> &mut [Cell] {
        &mut self.cells
    }

    pub(super) fn last_mut(&mut self) -> Option<&mut Cell> {
        self.cells.last_mut()
    }

    /// Appends a cell. The greeting gives way to the first real entry,
    /// whatever brings it: a submitted line or a notice.
    pub(super) fn push(&mut self, cell: Cell) {
        self.clear_welcome();
        if self.offset_from_bottom > 0 {
            self.unseen += 1;
        }
        self.cells.push(cell);
    }

    /// Puts `cell` where another was; the row keeps its place and its
    /// focus.
    pub(super) fn replace(&mut self, index: usize, cell: Cell) -> Cell {
        std::mem::replace(&mut self.cells[index], cell)
    }

    pub(super) fn remove(&mut self, index: usize) -> Cell {
        let removed = self.cells.remove(index);
        self.focus = match self.focus {
            Some(focus) if focus == index => None,
            Some(focus) if focus > index => Some(focus - 1),
            other => other,
        };
        removed
    }

    pub(super) fn truncate(&mut self, len: usize) {
        self.cells.truncate(len);
        if self.focus.is_some_and(|focus| focus >= len) {
            self.focus = None;
        }
    }

    /// Drops the greeting; it never comes back. Called ahead of reading
    /// `len` as a cell index so the index survives the next push.
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
        if let Some(cell) = self.focus.and_then(|focus| self.cells.get_mut(focus)) {
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
        if let Some(cell) = self.cells.get_mut(index)
            && cell.collapsible()
        {
            self.focus = Some(index);
            cell.toggle();
        }
    }

    /// Lays the cells out for `area` and hands back the rows to draw.
    /// The scroll offset is clamped to what the transcript can show, and
    /// the rows each cell received are kept so a click can find its cell.
    pub(super) fn view(&mut self, area: Rect, renderer: &mut Renderer) -> TranscriptView {
        self.last_area = area;
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut owners: Vec<Option<usize>> = Vec::new();
        let mut headers: Vec<usize> = Vec::with_capacity(self.cells.len());
        for (index, cell) in self.cells.iter_mut().enumerate() {
            if index > 0 {
                lines.push(Line::default());
                owners.push(None);
            }
            headers.push(lines.len());
            let mut cell_lines = cell.lines(area.width, renderer);
            if self.focus == Some(index)
                && let Some(header) = cell_lines.first_mut()
            {
                *header = std::mem::take(header).patch_style(REVERSED);
            }
            owners.extend(std::iter::repeat_n(Some(index), cell_lines.len()));
            lines.extend(cell_lines);
        }

        let total = lines.len();
        let height = usize::from(area.height);
        self.offset_from_bottom = self.offset_from_bottom.min(total.saturating_sub(height));
        if std::mem::take(&mut self.reveal_focus)
            && let Some(header) = self.focus.map(|focus| headers[focus])
        {
            self.reveal(header, total, height);
        }
        if self.offset_from_bottom == 0 {
            self.unseen = 0;
        }
        let end = total - self.offset_from_bottom;
        let start = end.saturating_sub(height);

        self.last_layout.clear();
        for (row, owner) in owners[start..end].iter().enumerate() {
            let Some(cell) = *owner else {
                continue;
            };
            let row = u16::try_from(row).unwrap_or(u16::MAX);
            match self.last_layout.last_mut() {
                Some(span) if span.cell == cell => span.rows += 1,
                _ => self.last_layout.push(RowSpan {
                    cell,
                    first_row: row,
                    rows: 1,
                }),
            }
        }

        let chip = (self.offset_from_bottom > 0 && self.unseen > 0)
            .then(|| format!("{} new below  End = follow", self.unseen));
        TranscriptView {
            lines: lines.drain(start..end).collect(),
            centre: self.welcome_only(),
            chip,
        }
    }

    /// Moves the view the least it has to for line `header` to be on it.
    fn reveal(&mut self, header: usize, total: usize, height: usize) {
        let end = total - self.offset_from_bottom;
        let start = end.saturating_sub(height);
        if header < start {
            self.offset_from_bottom = total - (header + height).min(total);
        } else if header >= end {
            self.offset_from_bottom = total - (header + 1);
        }
    }
}

/// The rows of the transcript the view shows, with the follow chip when
/// cells arrived below a scrolled view. The greeting alone sits in the
/// middle of the area rather than at its bottom.
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
    use std::time::Instant;

    fn transcript() -> (Transcript, Renderer) {
        let config = AppConfig {
            highlight: false,
            ..Default::default()
        };
        let renderer = Renderer::new(config.render_options().unwrap()).unwrap();
        let mut transcript = Transcript::new(Cell::welcome("m".to_string(), CommandPrefix::Dot));
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

    /// The offset is clamped to the lines above the view, a push while
    /// scrolled counts as unseen, and reaching the bottom forgets it.
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
        transcript.scroll_page(1);
        transcript.view(area(5), &mut renderer);
        assert_eq!(transcript.offset_from_bottom, 9);
        assert_eq!(transcript.unseen, 1, "a partial scroll forgot the count");
        transcript.scroll_to_bottom();
        let view = transcript.view(area(5), &mut renderer);
        assert_eq!(transcript.offset_from_bottom, 0);
        assert_eq!(view.chip, None);
        assert_eq!(transcript.unseen, 0);
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
        // 12 cells, 11 separators: 23 lines; the first row's header is
        // line 0 and the view must start there.
        assert_eq!(transcript.offset_from_bottom, 19);
        assert_eq!(transcript.last_layout[0].cell, 0);
        assert_eq!(transcript.last_layout[0].first_row, 0);
        transcript.focus_next();
        transcript.view(area(4), &mut renderer);
        assert_eq!(transcript.offset_from_bottom, 0);
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
        transcript.click(5, 2 + 2);
        assert_eq!(transcript.focus, Some(1));
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
