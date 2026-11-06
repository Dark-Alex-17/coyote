//! The todo panel: the turn's goal and checklist, shown beside the
//! transcript on F2. The list is read from the context between turns and
//! from `TodoUpdated` events while one runs.

use super::{DIM, render};
use crate::config::todo::TodoItem;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
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
}

impl TodoPanel {
    pub(super) fn toggle(&mut self) {
        self.visible = !self.visible;
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

    /// Columns the panel takes beside a transcript in a frame `width`
    /// wide, or None when the frame is too narrow to share and the panel
    /// takes the whole area.
    pub(super) fn width_beside(width: u16) -> Option<u16> {
        (width >= SPLIT_MIN_WIDTH).then(|| (width / 3).clamp(PANEL_MIN_WIDTH, PANEL_MAX_WIDTH))
    }
}

impl Widget for &TodoPanel {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let block = Block::bordered().title("Todo");
        let inner = block.inner(area);
        block.render(area, buf);
        if inner.is_empty() {
            return;
        }
        let mut lines = Vec::new();
        if !self.goal.is_empty() {
            lines.extend(render::plain_lines(
                &self.goal,
                inner.width,
                Style::new().add_modifier(Modifier::BOLD),
            ));
        }
        if self.items.is_empty() {
            lines.extend(render::plain_lines("(no todos)", inner.width, DIM));
        }
        for item in &self.items {
            let (mark, style) = if item.done {
                ("x", DIM)
            } else {
                (" ", Style::new())
            };
            lines.extend(render::plain_lines(
                &format!("[{mark}] {}", item.desc),
                inner.width,
                style,
            ));
        }
        Paragraph::new(lines).render(inner, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
