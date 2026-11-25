//! The threads pop-up: every thread of the session as a tree, over the
//! screen it was opened from (the main screen, the transcript view or the
//! tasks pop-up), on Alt+S, `.threads`, `t` in the tasks pop-up or Tab in
//! the transcript view. Enter opens the highlighted thread in the
//! transcript view.

use super::DIM;
use super::bottom_pane::step_highlight;
use super::help::MARGIN;
use super::render;
use super::theme::Theme;
use super::thread_tree::{ThreadNode, ThreadTree, popup_area, toggle_fold, window_start};
use chrono::{DateTime, FixedOffset, Utc};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
use std::collections::HashSet;
use unicode_width::UnicodeWidthStr;

const TITLE: &str = "Threads";
const HINT: &str = "j/k move  h/l fold  Enter open  Esc back";
/// The legend of a pop-up too narrow for `HINT`.
const SHORT_HINT: &str = "Enter open  Esc back";

const REVERSED: Style = Style::new().add_modifier(Modifier::REVERSED);

/// The tree and the thread highlighted in it, by fold key, so a fresh
/// listing keeps the highlight where it was. The folds are the App's,
/// passed in, so they outlast the pop-up.
pub(super) struct Threads {
    tree: ThreadTree,
    highlight: String,
    /// Said above the rows: why there are no children, or why they
    /// could not be listed.
    note: Option<String>,
    /// What the run times are counted to and the spawn times shown in.
    now: DateTime<Utc>,
    offset: FixedOffset,
    /// The area the pop-up was last centred over, which a click is
    /// placed against.
    area: Rect,
}

impl Threads {
    /// The pop-up on `tree`, its run times counted to `now` and its spawn
    /// times shown at `offset`, starting on the thread keyed `wanted` (or
    /// the folded thread hiding it) when the tree has it, and on the main
    /// thread otherwise.
    pub(super) fn new(
        tree: ThreadTree,
        note: Option<String>,
        now: DateTime<Utc>,
        offset: FixedOffset,
        wanted: Option<&str>,
        folds: &HashSet<String>,
    ) -> Self {
        let shown = wanted
            .and_then(|key| tree.shown_at(key, folds))
            .unwrap_or(0);
        let highlight = tree.key(shown).to_string();
        Self {
            tree,
            highlight,
            note,
            now,
            offset,
            area: Rect::ZERO,
        }
    }

    pub(super) fn highlight(&self) -> &str {
        &self.highlight
    }

    /// The highlighted thread, or the folded ancestor hiding it.
    fn highlighted(&self, folds: &HashSet<String>) -> usize {
        self.tree.shown_at(&self.highlight, folds).unwrap_or(0)
    }

    fn select(&mut self, at: usize) {
        self.highlight = self.tree.key(at).to_string();
    }

    /// Rows the pop-up holds: the note, then a row per thread on show.
    fn height(&self, folds: &HashSet<String>) -> usize {
        usize::from(self.note.is_some()) + self.tree.visible(folds).len()
    }

    pub(super) fn move_highlight(&mut self, delta: i32, folds: &HashSet<String>) {
        let shown = self.tree.visible(folds);
        let highlighted = self.highlighted(folds);
        let at = shown.iter().position(|&at| at == highlighted).unwrap_or(0);
        self.select(shown[step_highlight(at, delta, shown.len())]);
    }

    /// Unfolds the highlighted thread; one already open hands the
    /// highlight to its first child.
    pub(super) fn unfold(&mut self, folds: &mut HashSet<String>) {
        let at = self.highlighted(folds);
        let Some(child) = self.tree.first_child(at) else {
            return;
        };
        if !folds.remove(self.tree.key(at)) {
            self.select(child);
        }
    }

    /// Folds the highlighted thread; one folded already, or with no
    /// children, hands the highlight to its parent.
    pub(super) fn fold(&mut self, folds: &mut HashSet<String>) {
        let at = self.highlighted(folds);
        let key = self.tree.key(at).to_string();
        if self.tree.has_children(at) && !folds.contains(&key) {
            folds.insert(key);
        } else if let Some(parent) = self.tree.parent(at) {
            self.select(parent);
        }
    }

    pub(super) fn toggle(&mut self, folds: &mut HashSet<String>) {
        let at = self.highlighted(folds);
        if self.tree.has_children(at) {
            toggle_fold(folds, self.tree.key(at));
        }
    }

    pub(super) fn picked(&self, folds: &HashSet<String>) -> &ThreadNode {
        self.tree.node(self.highlighted(folds))
    }

    pub(super) fn set_area(&mut self, area: Rect) {
        self.area = area;
    }

    /// A left click at `column`, `row` on the pop-up as last drawn: on a
    /// thread's fold arrow it folds or unfolds the thread, and anywhere
    /// on its row it highlights it.
    pub(super) fn click(&mut self, column: u16, row: u16, folds: &mut HashSet<String>) {
        let inner = popup_area(self.area, self.height(folds)).inner(MARGIN);
        let rows = Rect {
            y: inner.y + u16::from(self.note.is_some()),
            height: inner.height.saturating_sub(u16::from(self.note.is_some())),
            ..inner
        };
        if !rows.contains(Position::new(column, row)) {
            return;
        }
        let shown = self.tree.visible(folds);
        let highlighted = self.highlighted(folds);
        let at = shown.iter().position(|&at| at == highlighted).unwrap_or(0);
        let index = window_start(at, usize::from(rows.height)) + usize::from(row - rows.y);
        let Some(&clicked) = shown.get(index) else {
            return;
        };
        self.select(clicked);
        let arrow = usize::from(column - rows.x).checked_sub(self.tree.arrow_column(clicked));
        if self.tree.has_children(clicked) && arrow.is_some_and(|offset| offset < 2) {
            toggle_fold(folds, self.tree.key(clicked));
        }
    }
}

/// The threads pop-up centred over the area it is drawn in, framed like
/// the tasks pop-up: the title, the note, a row per thread on show with
/// the highlighted one reversed and kept in view, and the keys on the
/// bottom border.
pub(super) struct ThreadsPopup<'a> {
    pub(super) threads: &'a Threads,
    pub(super) folds: &'a HashSet<String>,
    pub(super) theme: &'a Theme,
}

impl Widget for ThreadsPopup<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let threads = self.threads;
        let area = area.intersection(*buf.area());
        let popup = popup_area(area, threads.height(self.folds));
        if popup.height < 3 || popup.width < 3 {
            return;
        }
        let hint = if HINT.width() + 2 <= usize::from(popup.width) {
            HINT
        } else {
            SHORT_HINT
        };
        let block = Block::bordered()
            .title(Span::styled(TITLE, self.theme.heading))
            .title_bottom(Line::styled(hint, DIM));
        let mut inner = block.inner(popup);
        Clear.render(popup, buf);
        buf.set_style(popup, self.theme.surface_raised);
        block.render(popup, buf);
        if let Some(note) = &threads.note {
            let line = Rect { height: 1, ..inner };
            Paragraph::new(Span::styled(render::plain_text(note), self.theme.faded))
                .render(line, buf);
            inner.y += 1;
            inner.height = inner.height.saturating_sub(1);
        }
        let shown = threads.tree.visible(self.folds);
        let highlighted = threads.highlighted(self.folds);
        let at = shown.iter().position(|&at| at == highlighted).unwrap_or(0);
        let start = window_start(at, usize::from(inner.height));
        let width = usize::from(inner.width);
        let lines: Vec<Line<'static>> = shown
            .iter()
            .skip(start)
            .take(usize::from(inner.height))
            .map(|&node| {
                threads.tree.line(
                    node,
                    self.folds,
                    width,
                    threads.now,
                    threads.offset,
                    self.theme,
                )
            })
            .collect();
        Paragraph::new(lines).render(inner, buf);
        let y = inner.y + u16::try_from(at - start).unwrap_or(u16::MAX);
        if y < inner.bottom() {
            buf.set_style(
                Rect {
                    y,
                    height: 1,
                    ..inner
                },
                REVERSED,
            );
        }
    }
}
