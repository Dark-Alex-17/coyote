//! The todo panel: the turn's goal and checklist, shown beside the
//! transcript on Ctrl+X. The list is read from the context between turns and
//! from `TodoUpdated` events while one runs. A list longer than the panel
//! scrolls, by Alt+PgUp / Alt+PgDn and by the wheel over it, and its
//! borders count the lines out of view above and below.

use super::render;
use super::theme::{DIM, Theme};
use crate::config::todo::TodoItem;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Widget};

/// Narrowest frame that keeps a transcript beside the panel; under it
/// the panel takes the whole transcript area.
const SPLIT_MIN_WIDTH: u16 = 60;
const PANEL_MIN_WIDTH: u16 = 20;
const PANEL_MAX_WIDTH: u16 = 40;

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
        panel.toggle();
        assert!(!panel.contains(3, 3), "a hidden panel takes no wheel");
    }
}
