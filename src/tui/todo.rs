//! The todo panel: the turn's goal and checklist, shown beside the
//! transcript on Ctrl+X. The list is read from the context between turns and
//! from `TodoUpdated` events while one runs. A list longer than the panel
//! scrolls, by Alt+PgUp / Alt+PgDn and by the wheel over it, and its
//! borders count the lines out of view above and below.
//! Alt+X, or a click on the panel's title, opens the whole list in a
//! pop-up laid out like the key help, every item wrapped rather than cut.

use super::help::{HINT, MARGIN};
use super::render;
use super::theme::{DIM, Theme};
use crate::config::todo::TodoItem;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};

/// Narrowest frame that keeps a transcript beside the panel; under it
/// the panel takes the whole transcript area.
const SPLIT_MIN_WIDTH: u16 = 60;
const PANEL_MIN_WIDTH: u16 = 20;
const PANEL_MAX_WIDTH: u16 = 40;
/// Columns of an item's `[ ] ` marker, which its wrapped lines are
/// indented by.
const MARKER_WIDTH: u16 = 4;

#[derive(Default)]
pub(super) struct TodoPanel {
    visible: bool,
    goal: String,
    items: Vec<TodoItem>,
    /// The first line shown, counted in the wrapped list.
    top: usize,
    /// Lines the last frame had room for, what a page moves by.
    page: usize,
    /// Wrapped lines of the last frame, which the scroll is clamped to.
    last_total: usize,
    /// Where the last frame drew the panel, for the wheel to find it.
    last_area: Rect,
}

impl TodoPanel {
    pub(super) fn toggle(&mut self) {
        self.visible = !self.visible;
        if !self.visible {
            self.last_area = Rect::ZERO;
        }
    }

    pub(super) fn visible(&self) -> bool {
        self.visible
    }

    pub(super) fn set(&mut self, goal: String, items: Vec<TodoItem>) {
        // A new goal is a new list, read from its top.
        if goal != self.goal {
            self.top = 0;
        }
        self.goal = goal;
        self.items = items;
    }

    /// The items alone: the event that carries them says nothing about
    /// the goal, which stays as the context last gave it.
    pub(super) fn set_items(&mut self, items: Vec<TodoItem>) {
        self.items = items;
    }

    /// Moves the list by `delta` lines, down when positive, within what
    /// the last frame showed.
    pub(super) fn scroll(&mut self, delta: i32) {
        let top = self
            .top
            .saturating_add_signed(isize::try_from(delta).unwrap_or(0));
        self.top = top.min(self.bottom());
    }

    /// Moves the list by a panel's height, down when `direction` is
    /// positive.
    pub(super) fn page(&mut self, direction: i32) {
        let page = i32::try_from(self.page.max(1)).unwrap_or(i32::MAX);
        self.scroll(direction.saturating_mul(page));
    }

    /// Whether the last frame drew the panel over this cell.
    pub(super) fn contains(&self, column: u16, row: u16) -> bool {
        self.visible && self.last_area.contains(Position::new(column, row))
    }

    /// Whether the last frame drew the panel's top border, its title,
    /// over this cell.
    pub(super) fn title_contains(&self, column: u16, row: u16) -> bool {
        let title = Rect {
            height: self.last_area.height.min(1),
            ..self.last_area
        };
        self.visible && title.contains(Position::new(column, row))
    }

    /// The furthest the top can go: the last page starts there.
    fn bottom(&self) -> usize {
        self.last_total.saturating_sub(self.page)
    }

    /// Columns the panel takes beside a transcript in a frame `width`
    /// wide, or None when the frame is too narrow to share and the panel
    /// takes the whole area.
    pub(super) fn width_beside(width: u16) -> Option<u16> {
        (width >= SPLIT_MIN_WIDTH).then(|| (width / 3).clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH))
    }

    pub(super) fn view<'a>(&'a mut self, theme: &'a Theme) -> TodoView<'a> {
        TodoView { panel: self, theme }
    }
}

/// The goal and every item laid out at `width`, nothing cut: an item's
/// text wraps under itself, clear of its marker.
fn popup_lines(goal: &str, items: &[TodoItem], width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !goal.is_empty() {
        lines.extend(render::plain_lines(goal, width, theme.heading));
    }
    if items.is_empty() {
        lines.extend(render::plain_lines("(no todos)", width, DIM));
    }
    let indent = " ".repeat(usize::from(MARKER_WIDTH));
    for item in items {
        let (mark, style) = if item.done {
            ("[x] ", theme.todo_done)
        } else {
            ("[ ] ", theme.todo_open)
        };
        let text = render::plain_lines(&item.desc, width.saturating_sub(MARKER_WIDTH), style);
        for (i, line) in text.into_iter().enumerate() {
            let lead = if i == 0 {
                Span::styled(mark, style)
            } else {
                Span::raw(indent.clone())
            };
            let mut spans = vec![lead];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// Where the todo pop-up is scrolled to. The list itself is read from the
/// panel at each frame, so an update shows while the pop-up is open.
#[derive(Debug, Default)]
pub(super) struct TodoPopup {
    top: usize,
    /// Text rows of the last frame drawn, what a page step moves by.
    page: usize,
    /// The last frame's text length, which the scroll is clamped to.
    last_total: usize,
}

impl TodoPopup {
    pub(super) fn scroll(&mut self, delta: i32) {
        let top = self
            .top
            .saturating_add_signed(isize::try_from(delta).unwrap_or(0));
        self.top = top.min(self.bottom());
    }

    pub(super) fn page(&mut self, direction: i32) {
        let rows = i32::try_from(self.page.saturating_sub(1).max(1)).unwrap_or(i32::MAX);
        self.scroll(direction.saturating_mul(rows));
    }

    pub(super) fn jump_to_top(&mut self) {
        self.top = 0;
    }

    pub(super) fn jump_to_bottom(&mut self) {
        self.top = self.bottom();
    }

    /// The highest `top` that still fills the last frame; before any
    /// frame there is no bound yet.
    fn bottom(&self) -> usize {
        if self.page == 0 {
            return usize::MAX;
        }
        self.last_total.saturating_sub(self.page)
    }

    pub(super) fn view<'a>(
        &'a mut self,
        panel: &'a TodoPanel,
        theme: &'a Theme,
    ) -> TodoPopupView<'a> {
        TodoPopupView {
            popup: self,
            panel,
            theme,
        }
    }
}

pub(super) struct TodoPopupView<'a> {
    popup: &'a mut TodoPopup,
    panel: &'a TodoPanel,
    theme: &'a Theme,
}

impl Widget for TodoPopupView<'_> {
    /// `area` is the screen above the status line, as for the key help.
    /// With no row left for the list only the frame is drawn, so no
    /// position names lines that cannot be seen.
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area()).inner(MARGIN);
        if area.is_empty() {
            return;
        }
        let block = Block::bordered()
            .title(Span::styled("Todo", self.theme.heading))
            .title_bottom(Line::styled(HINT, DIM));
        let inner = block.inner(area);
        let text = popup_lines(&self.panel.goal, &self.panel.items, inner.width, self.theme);
        let total = text.len();
        let rows = usize::from(inner.height);
        let popup = self.popup;
        popup.page = rows.max(1);
        popup.last_total = total;
        popup.top = popup.top.min(popup.bottom());
        Clear.render(area, buf);
        buf.set_style(area, self.theme.surface_raised);
        if inner.is_empty() {
            block.render(area, buf);
            return;
        }
        let start = popup.top;
        let end = (start + rows).min(total);
        let position = format!("{}-{end}/{total}", (start + 1).min(end));
        block
            .title(Line::styled(position, DIM).right_aligned())
            .render(area, buf);
        let visible: Vec<Line<'static>> = text.into_iter().skip(start).take(rows).collect();
        Paragraph::new(visible).render(inner, buf);
    }
}

pub(super) struct TodoView<'a> {
    panel: &'a mut TodoPanel,
    theme: &'a Theme,
}

impl Widget for TodoView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        let panel = self.panel;
        panel.last_area = area;
        if area.is_empty() {
            return;
        }
        let mut block = Block::bordered().title("Todo");
        let inner = block.inner(area);
        if inner.is_empty() {
            block.render(area, buf);
            return;
        }
        let mut lines = Vec::new();
        if !panel.goal.is_empty() {
            lines.extend(render::plain_lines(
                &panel.goal,
                inner.width,
                self.theme.heading,
            ));
        }
        if panel.items.is_empty() {
            lines.extend(render::plain_lines("(no todos)", inner.width, DIM));
        }
        for item in &panel.items {
            let (mark, style) = if item.done {
                ("x", self.theme.todo_done)
            } else {
                (" ", self.theme.todo_open)
            };
            lines.extend(render::plain_lines(
                &format!("[{mark}] {}", item.desc),
                inner.width,
                style,
            ));
        }
        let rows = usize::from(inner.height);
        panel.page = rows;
        panel.last_total = lines.len();
        panel.top = panel.top.min(panel.bottom());
        let above = panel.top;
        let below = lines.len().saturating_sub(panel.top + rows);
        if above > 0 {
            block = block.title(Line::styled(format!("(+{above} above)"), DIM).right_aligned());
        }
        if below > 0 {
            block =
                block.title_bottom(Line::styled(format!("(+{below} below)"), DIM).right_aligned());
        }
        block.render(area, buf);
        let visible: Vec<Line<'static>> = lines.into_iter().skip(above).take(rows).collect();
        Paragraph::new(visible).render(inner, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn the_panel_shares_wide_frames_and_takes_narrow_ones() {
        assert_eq!(TodoPanel::width_beside(59), None);
        assert_eq!(TodoPanel::width_beside(60), Some(20));
        assert_eq!(TodoPanel::width_beside(90), Some(30));
        assert_eq!(TodoPanel::width_beside(200), Some(40));
    }

    #[test]
    fn items_replace_the_list_but_keep_the_goal() {
        let mut panel = TodoPanel::default();
        panel.set(
            "ship it".to_string(),
            vec![TodoItem {
                id: 1,
                desc: "write".to_string(),
                done: false,
            }],
        );
        panel.set_items(vec![TodoItem {
            id: 1,
            desc: "write".to_string(),
            done: true,
        }]);
        assert_eq!(panel.goal, "ship it");
        assert!(panel.items[0].done);
        assert!(!panel.visible());
        panel.toggle();
        assert!(panel.visible());
    }

    fn items(count: usize) -> Vec<TodoItem> {
        (1..=count)
            .map(|id| TodoItem {
                id,
                desc: format!("step {id}"),
                done: false,
            })
            .collect()
    }

    /// The panel drawn into a 24x7 frame: five rows inside the border.
    fn draw(panel: &mut TodoPanel) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(24, 7)).unwrap();
        let theme = Theme::fallback(false);
        terminal
            .draw(|frame| frame.render_widget(panel.view(&theme), frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// A list longer than the panel shows its first page with the lines
    /// below counted on the bottom border; scrolling and paging move it,
    /// count the lines above on the top border, and stop at either end.
    #[test]
    fn a_long_list_scrolls_and_counts_what_is_out_of_view() {
        let mut panel = TodoPanel::default();
        panel.toggle();
        panel.set(String::new(), items(12));
        let rows = draw(&mut panel);
        assert!(rows[1].contains("[ ] step 1"), "{rows:#?}");
        assert!(rows[5].contains("[ ] step 5"), "{rows:#?}");
        assert!(rows[6].contains("(+7 below)"), "{rows:#?}");
        assert!(!rows[0].contains("above"), "{rows:#?}");

        panel.scroll(1);
        let rows = draw(&mut panel);
        assert!(rows[1].contains("step 2"), "{rows:#?}");
        assert!(rows[0].contains("(+1 above)"), "{rows:#?}");
        assert!(rows[6].contains("(+6 below)"), "{rows:#?}");

        panel.page(1);
        panel.page(1);
        let rows = draw(&mut panel);
        assert!(rows[5].contains("step 12"), "{rows:#?}");
        assert!(rows[0].contains("(+7 above)"), "{rows:#?}");
        assert!(!rows[6].contains("below"), "{rows:#?}");

        panel.page(-1);
        let rows = draw(&mut panel);
        assert!(rows[1].contains("step 3"), "{rows:#?}");
        panel.scroll(-100);
        let rows = draw(&mut panel);
        assert!(rows[1].contains("step 1"), "{rows:#?}");
        assert!(!rows[0].contains("above"), "{rows:#?}");

        panel.page(1);
        panel.set_items(items(12));
        assert!(
            draw(&mut panel)[0].contains("above"),
            "an update moved a scrolled list"
        );
        panel.set("a new goal".to_string(), items(12));
        let rows = draw(&mut panel);
        assert!(
            rows[1].contains("a new goal"),
            "a new list did not open at its top: {rows:#?}"
        );
    }

    /// A list that fits shows whole with no counts and does not move.
    #[test]
    fn a_list_that_fits_does_not_scroll() {
        let mut panel = TodoPanel::default();
        panel.toggle();
        panel.set("goal".to_string(), items(3));
        draw(&mut panel);
        panel.page(1);
        let rows = draw(&mut panel);
        assert!(rows[1].contains("goal"), "{rows:#?}");
        assert!(rows[4].contains("step 3"), "{rows:#?}");
        assert!(
            !rows
                .iter()
                .any(|row| row.contains("above") || row.contains("below"))
        );
        assert!(panel.contains(3, 3));
        assert!(panel.title_contains(3, 0));
        assert!(!panel.title_contains(3, 1), "a row under the title");
        panel.toggle();
        assert!(!panel.contains(3, 3), "a hidden panel takes no wheel");
        assert!(!panel.title_contains(3, 0), "a hidden panel has no title");
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(Line::to_string).collect()
    }

    /// The pop-up's list keeps every word of an item, wrapped under its
    /// text and clear of the marker, with the goal first.
    #[test]
    fn popup_lines_wrap_items_under_their_text() {
        let theme = Theme::fallback(false);
        let items = vec![
            TodoItem {
                id: 1,
                desc: "write the parser".to_string(),
                done: true,
            },
            TodoItem {
                id: 2,
                desc: "wire it into the loop and test it".to_string(),
                done: false,
            },
        ];
        let lines = popup_lines("ship it", &items, 16, &theme);
        assert_eq!(
            texts(&lines),
            [
                "ship it",
                "[x] write the",
                "    parser",
                "[ ] wire it into",
                "    the loop and",
                "    test it",
            ]
        );
        assert_eq!(lines[0].spans[0].style, theme.heading);
        assert_eq!(lines[1].spans[0].style, theme.todo_done);
        assert_eq!(lines[3].spans[0].style, theme.todo_open);
        assert_eq!(texts(&popup_lines("", &[], 16, &theme)), ["(no todos)"]);
    }

    /// The pop-up drawn into a 30x8 frame: a margin and a border leave
    /// four rows for the list.
    fn draw_popup(popup: &mut TodoPopup, panel: &TodoPanel) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(30, 8)).unwrap();
        let theme = Theme::fallback(false);
        terminal
            .draw(|frame| frame.render_widget(popup.view(panel, &theme), frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// The pop-up scrolls within its list and says where it is; the top
    /// and bottom jumps land on either end, and it reads a hidden panel.
    #[test]
    fn the_popup_scrolls_within_the_list() {
        let mut panel = TodoPanel::default();
        panel.set("goal".to_string(), items(9));
        let mut popup = TodoPopup::default();
        let rows = draw_popup(&mut popup, &panel);
        assert!(
            rows[1].contains("Todo") && rows[1].contains("1-4/10"),
            "{rows:#?}"
        );
        assert!(rows[2].contains("goal"), "{rows:#?}");
        popup.scroll(1);
        let rows = draw_popup(&mut popup, &panel);
        assert!(rows[2].contains("[ ] step 1"), "{rows:#?}");
        popup.page(1);
        assert!(draw_popup(&mut popup, &panel)[1].contains("5-8/10"));
        popup.jump_to_bottom();
        let rows = draw_popup(&mut popup, &panel);
        assert!(rows[1].contains("7-10/10"), "{rows:#?}");
        assert!(rows[5].contains("step 9"), "{rows:#?}");
        popup.scroll(5);
        assert_eq!(popup.top, 6);
        popup.jump_to_top();
        assert!(draw_popup(&mut popup, &panel)[2].contains("goal"));
        assert!(!panel.visible());
    }
}
