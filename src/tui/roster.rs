//! The roster, which the UI calls tasks: every task the supervisors run
//! (spawned agents and background jobs, running and finished) as a tree,
//! each under the agent that started it, and then one row per question an
//! agent is waiting on the user to answer, as a pop-up over the transcript
//! on Alt+T. The rows are read every frame it is up from the supervisor
//! and escalation queue the context held when it was last back in the
//! UI's hands, so they follow a turn that is running.

use super::bottom_pane::{first_line, step_highlight};
use super::theme::Theme;
use super::thread_tree::{StatusClass, Tree, TreeItem, popup_area, toggle_fold, window_start};
use super::throbber::Tick;
use super::{ChromeKind, DIM, chrome_style};
use crate::events::NoticeKind;
use crate::supervisor::escalation::{EscalationQueue, PendingEscalation};
use crate::supervisor::{Supervisor, TaskRow, TaskRowStatus};
use chrono::{DateTime, FixedOffset, Utc};
use parking_lot::RwLock;
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
use std::collections::HashSet;
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

const TITLE: &str = "Tasks";
const HINT: &str = "j/k = move  h/l = fold  x = cancel  o = open  Enter = answer  Esc = close";
/// The hint on a frame too narrow for all of it: the bottom border cuts
/// it from the right, so the way out comes first.
const NARROW_HINT: &str = "Esc = close  j/k = move  h/l = fold  x = cancel  o = open";
const EMPTY: &str = "(no tasks and no questions)";
/// Where a question row's text starts, so it lines up with the names of
/// the tasks at the top of the tree.
const ASK_INDENT: &str = "  ";

/// Where the roster reads its rows from: the context's supervisor and
/// escalation queue as of the last time the context was in hand.
#[derive(Clone, Default)]
pub(super) struct RosterSources {
    pub(super) supervisor: Option<Arc<RwLock<Supervisor>>>,
    pub(super) escalations: Option<Arc<EscalationQueue>>,
}

impl RosterSources {
    /// Every task through every child agent's supervisor, its start put
    /// on the wall clock from `tick` and `now`, the wall time at
    /// `tick.now`; then the questions sorted by id.
    pub(super) fn rows(&self, tick: Tick, now: DateTime<Utc>) -> Vec<RosterRow> {
        let tasks: Vec<TaskRow> = self
            .supervisor
            .as_ref()
            .map(|supervisor| supervisor.read().task_tree_snapshot())
            .unwrap_or_default();
        let mut rows: Vec<RosterRow> = tasks
            .into_iter()
            .map(|task| RosterRow::Task(TaskNode::new(task, tick, now)))
            .collect();
        if let Some(queue) = &self.escalations {
            let mut asks: Vec<Escalation> =
                queue.pending().into_iter().map(Escalation::from).collect();
            asks.sort_by(|a, b| a.id.cmp(&b.id));
            rows.extend(asks.into_iter().map(RosterRow::Escalation));
        }
        rows
    }

    /// The ids of the tasks the supervisor tracks, finished ones included.
    pub(super) fn task_ids(&self) -> HashSet<String> {
        self.supervisor
            .as_ref()
            .map(|supervisor| {
                supervisor
                    .read()
                    .task_snapshot()
                    .into_iter()
                    .map(|task| task.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The running agents and jobs for the footer, counted the way the
    /// rows show them but through every child agent's supervisor too.
    pub(super) fn running_counts(&self) -> (usize, usize) {
        self.supervisor
            .as_ref()
            .map_or((0, 0), |supervisor| supervisor.read().running_counts())
    }
}

/// A task as a row of the tree, with its start on the wall clock and,
/// while it runs, the throbber's frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskNode {
    pub(super) task: TaskRow,
    started: Option<DateTime<Utc>>,
    throbber: &'static str,
}

impl TaskNode {
    /// `task` as of `tick`, whose `now` is the wall time `now`.
    pub(super) fn new(task: TaskRow, tick: Tick, now: DateTime<Utc>) -> Self {
        let started = task.started_at.map(|started| {
            now - chrono::Duration::from_std(tick.elapsed(started)).unwrap_or_default()
        });
        Self {
            task,
            started,
            throbber: tick.frame(),
        }
    }
}

impl TreeItem for TaskNode {
    fn key(&self) -> &str {
        &self.task.id
    }

    fn parent_key(&self) -> Option<&str> {
        self.task.parent_id.as_deref()
    }

    fn label(&self) -> &str {
        &self.task.name
    }

    fn status(&self) -> Option<(&'static str, StatusClass)> {
        Some(match self.task.status {
            TaskRowStatus::Running => ("running", StatusClass::Running),
            TaskRowStatus::Finished => ("finished", StatusClass::Completed),
            TaskRowStatus::Completed => ("completed", StatusClass::Completed),
            TaskRowStatus::Failed => ("failed", StatusClass::Failed),
        })
    }

    fn started(&self) -> Option<DateTime<Utc>> {
        self.started
    }

    /// The supervisor keeps no end time, so a finished task shows only
    /// when it started.
    fn ended(&self) -> Option<DateTime<Utc>> {
        None
    }

    fn id(&self) -> Option<&str> {
        Some(&self.task.id)
    }

    fn throbber(&self) -> Option<&str> {
        self.running().then_some(self.throbber)
    }
}

/// A question a child agent is blocked on until the user answers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Escalation {
    pub(super) id: String,
    pub(super) agent_id: String,
    pub(super) agent_name: String,
    pub(super) question: String,
    /// Empty when the agent asked for free text.
    pub(super) options: Vec<String>,
}

impl From<PendingEscalation> for Escalation {
    fn from(pending: PendingEscalation) -> Self {
        Self {
            id: pending.id,
            agent_id: pending.from_agent_id,
            agent_name: pending.from_agent_name,
            question: pending.question,
            options: pending.options.unwrap_or_default(),
        }
    }
}

impl Escalation {
    /// `ask`, the asker and the question's first line, without the
    /// `[kind] ` that names its modal, and how many lines follow.
    fn line(&self, no_color: bool) -> Line<'static> {
        let style = chrome_style(ChromeKind::Notice(NoticeKind::ToolWarning), no_color);
        let (_, question) = super::escalation_kind(&self.question);
        let (first, tail) = first_line(question);
        let mut text = format!(
            "{ASK_INDENT}ask {}  {first}",
            super::render::plain_text(&self.agent_name)
        );
        if !tail.is_empty() {
            text.push_str("  ");
            text.push_str(&tail);
        }
        Line::styled(text, style)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RosterRow {
    Task(TaskNode),
    Escalation(Escalation),
}

/// A row on show: a task by its index in the tree, or a question by its
/// index among the questions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    Task(usize),
    Ask(usize),
}

pub(super) struct RosterState {
    tree: Tree<TaskNode>,
    asks: Vec<Escalation>,
    /// The tasks folded shut, by id, kept while the pop-up is up.
    folds: HashSet<String>,
    /// The id of the focused row. None once that row has gone, until
    /// the user moves onto another, so no key acts on a row they did not
    /// pick.
    focus: Option<String>,
    /// The focused row went in a refresh. The focus stays clear, where a
    /// roster that never had a row focuses the first one it gets, until a
    /// move lands on a row.
    focus_lost: bool,
    /// Where the pop-up was last drawn over, which clicks are read
    /// against.
    area: Rect,
}

impl Default for RosterState {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl std::fmt::Debug for RosterState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RosterState")
            .field("tasks", &self.tree.len())
            .field("asks", &self.asks.len())
            .field("focus", &self.focus)
            .finish()
    }
}

fn split(rows: Vec<RosterRow>) -> (Tree<TaskNode>, Vec<Escalation>) {
    let mut tasks = Vec::new();
    let mut asks = Vec::new();
    for row in rows {
        match row {
            RosterRow::Task(task) => tasks.push(task),
            RosterRow::Escalation(ask) => asks.push(ask),
        }
    }
    (Tree::build(None, tasks), asks)
}

impl RosterState {
    pub(super) fn new(rows: Vec<RosterRow>) -> Self {
        let (tree, asks) = split(rows);
        let mut state = Self {
            tree,
            asks,
            folds: HashSet::new(),
            focus: None,
            focus_lost: false,
            area: Rect::ZERO,
        };
        state.focus = state.shown().first().map(|&row| state.id(row).to_string());
        state
    }

    /// Takes a fresh read, keeping the focus on its row while that row
    /// is still listed; a row that went takes the focus with it. With no
    /// focus that a refresh took, the first row is focused.
    pub(super) fn refresh(&mut self, rows: Vec<RosterRow>) {
        (self.tree, self.asks) = split(rows);
        if self.focus.is_none() && !self.focus_lost {
            self.focus = self.shown().first().map(|&row| self.id(row).to_string());
        } else if self.highlight().is_none() {
            self.focus = None;
            self.focus_lost = true;
        }
    }

    fn shown(&self) -> Vec<Shown> {
        self.tree
            .visible(&self.folds)
            .into_iter()
            .map(Shown::Task)
            .chain((0..self.asks.len()).map(Shown::Ask))
            .collect()
    }

    fn id(&self, row: Shown) -> &str {
        match row {
            Shown::Task(at) => self.tree.key(at),
            Shown::Ask(at) => &self.asks[at].id,
        }
    }

    fn focus_on(&mut self, row: Shown) {
        self.focus = Some(self.id(row).to_string());
        self.focus_lost = false;
    }

    /// With no focus, moving down picks the first row and up the last. A
    /// move on an empty roster lands nowhere, so a lost focus stays lost.
    pub(super) fn move_highlight(&mut self, delta: i32) {
        let shown = self.shown();
        let index = match self.highlight() {
            Some(index) => step_highlight(index, delta, shown.len()),
            None if delta < 0 => shown.len().saturating_sub(1),
            None => 0,
        };
        if let Some(&row) = shown.get(index) {
            self.focus_on(row);
        }
    }

    /// The highlighted row, when it is a task.
    fn focused_task(&self) -> Option<usize> {
        match self.shown().get(self.highlight()?) {
            Some(&Shown::Task(at)) => Some(at),
            _ => None,
        }
    }

    /// Unfolds the highlighted task; one already open hands the
    /// highlight to its first child.
    pub(super) fn unfold(&mut self) {
        let Some(at) = self.focused_task() else {
            return;
        };
        let Some(child) = self.tree.first_child(at) else {
            return;
        };
        if !self.folds.remove(self.tree.key(at)) {
            self.focus_on(Shown::Task(child));
        }
    }

    /// Folds the highlighted task; one folded already, or with no
    /// children, hands the highlight to its parent.
    pub(super) fn fold(&mut self) {
        let Some(at) = self.focused_task() else {
            return;
        };
        let key = self.tree.key(at).to_string();
        if self.tree.has_children(at) && !self.folds.contains(&key) {
            self.folds.insert(key);
        } else if let Some(parent) = self.tree.parent(at) {
            self.focus_on(Shown::Task(parent));
        }
    }

    pub(super) fn toggle(&mut self) {
        if let Some(at) = self.focused_task().filter(|&at| self.tree.has_children(at)) {
            toggle_fold(&mut self.folds, self.tree.key(at));
        }
    }

    /// The highlighted row's place among the rows on show; a task a fold
    /// hides is found at the folded task above it.
    fn highlight(&self) -> Option<usize> {
        let focus = self.focus.as_deref()?;
        let shown = self.shown();
        let target = match self.tree.shown_at(focus, &self.folds) {
            Some(at) => Shown::Task(at),
            None => Shown::Ask(self.asks.iter().position(|ask| ask.id == focus)?),
        };
        shown.iter().position(|&row| row == target)
    }

    pub(super) fn focused(&self) -> Option<RosterRow> {
        Some(match self.shown().get(self.highlight()?)? {
            Shown::Task(at) => RosterRow::Task(self.tree.node(*at).clone()),
            Shown::Ask(at) => RosterRow::Escalation(self.asks[*at].clone()),
        })
    }

    pub(super) fn set_area(&mut self, area: Rect) {
        self.area = area;
    }

    /// A left click at `column`, `row` on the pop-up as last drawn: it
    /// selects the row under it, and on a task's fold arrow folds or
    /// unfolds it too.
    pub(super) fn click(&mut self, column: u16, row: u16) {
        let shown = self.shown();
        let inner = popup_area(self.area, shown.len().max(1)).inner(super::help::MARGIN);
        if !inner.contains(Position::new(column, row)) {
            return;
        }
        let start = window_start(self.highlight().unwrap_or(0), usize::from(inner.height));
        let Some(&clicked) = shown.get(start + usize::from(row - inner.y)) else {
            return;
        };
        self.focus_on(clicked);
        if let Shown::Task(at) = clicked {
            let arrow = usize::from(column - inner.x).checked_sub(self.tree.arrow_column(at));
            if self.tree.has_children(at) && arrow.is_some_and(|offset| offset < 2) {
                toggle_fold(&mut self.folds, self.tree.key(at));
            }
        }
    }
}

/// The tasks pop-up over the transcript, framed like the threads pop-up
/// and sized the same: a title, the rows, and the keys on the bottom
/// border.
pub(super) struct RosterPopup<'a> {
    pub(super) state: &'a RosterState,
    pub(super) theme: &'a Theme,
    pub(super) no_color: bool,
    /// What a running task's time is counted to and the start times are
    /// shown in.
    pub(super) now: DateTime<Utc>,
    pub(super) offset: FixedOffset,
}

impl Widget for RosterPopup<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        let popup = popup_area(area, self.state.shown().len().max(1));
        if popup.height < 3 || popup.width < 3 {
            return;
        }
        let hint = if HINT.width() + 2 <= usize::from(popup.width) {
            HINT
        } else {
            NARROW_HINT
        };
        let block = Block::bordered()
            .title(Span::styled(TITLE, self.theme.heading))
            .title_bottom(Line::styled(hint, DIM));
        let inner = block.inner(popup);
        Clear.render(popup, buf);
        buf.set_style(popup, self.theme.surface_raised);
        block.render(popup, buf);
        RosterView {
            state: self.state,
            theme: self.theme,
            no_color: self.no_color,
            now: self.now,
            offset: self.offset,
        }
        .render(inner, buf);
    }
}

/// The roster's rows: the task tree, then a row per question, with the
/// highlighted one reversed and kept in view.
struct RosterView<'a> {
    state: &'a RosterState,
    theme: &'a Theme,
    no_color: bool,
    now: DateTime<Utc>,
    offset: FixedOffset,
}

impl Widget for RosterView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let state = self.state;
        let shown = state.shown();
        if shown.is_empty() {
            Paragraph::new(Line::from(Span::styled(EMPTY, DIM))).render(area, buf);
            return;
        }
        let height = usize::from(area.height);
        let highlight = state.highlight();
        let start = window_start(highlight.unwrap_or(0), height);
        let width = usize::from(area.width);
        let lines: Vec<Line<'static>> = shown
            .iter()
            .skip(start)
            .take(height)
            .map(|&row| match row {
                Shown::Task(at) => {
                    state
                        .tree
                        .line(at, &state.folds, width, self.now, self.offset, self.theme)
                }
                Shown::Ask(at) => state.asks[at].line(self.no_color),
            })
            .collect();
        Paragraph::new(lines).render(area, buf);
        if let Some(offset) = highlight.and_then(|at| at.checked_sub(start)) {
            let y = area.y + u16::try_from(offset).unwrap_or(u16::MAX);
            if y < area.bottom() {
                buf.set_style(
                    Rect::new(area.x, y, area.width, 1),
                    Style::new().add_modifier(Modifier::REVERSED),
                );
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::supervisor::TaskKind;
    use crate::utils::SPINNER_FRAMES;
    use chrono::TimeZone;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    /// The instant the test clock reads as `wall()`.
    fn origin() -> Instant {
        static ORIGIN: OnceLock<Instant> = OnceLock::new();
        *ORIGIN.get_or_init(Instant::now)
    }

    /// The wall time at `origin() + 1h`, when every frame is drawn.
    fn wall() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
    }

    fn tick() -> Tick {
        Tick {
            origin: origin(),
            now: origin() + Duration::from_secs(3600),
        }
    }

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    /// A task started `ago` seconds before the frame, under `parent`.
    pub(in crate::tui) fn task_under(
        id: &str,
        kind: TaskKind,
        name: &str,
        status: TaskRowStatus,
        parent: Option<&str>,
        ago: u64,
    ) -> RosterRow {
        RosterRow::Task(TaskNode::new(
            TaskRow {
                id: id.to_string(),
                kind,
                name: name.to_string(),
                started_at: Some(tick().now - Duration::from_secs(ago)),
                status,
                session_id: (kind == TaskKind::Agent).then(|| format!("{name}-session")),
                parent_id: parent.map(str::to_string),
            },
            tick(),
            wall(),
        ))
    }

    pub(in crate::tui) fn task(
        id: &str,
        kind: TaskKind,
        name: &str,
        status: TaskRowStatus,
    ) -> RosterRow {
        task_under(id, kind, name, status, None, 75)
    }

    pub(in crate::tui) fn ask(id: &str, agent: &str, question: &str) -> RosterRow {
        RosterRow::Escalation(Escalation {
            id: id.to_string(),
            agent_id: format!("agent_{agent}_5a6b7c8d"),
            agent_name: agent.to_string(),
            question: question.to_string(),
            options: vec!["yes".to_string(), "no".to_string()],
        })
    }

    /// Three agents deep with a job under the middle one, a finished and
    /// a failed task at the top, and a question.
    fn tree_rows() -> Vec<RosterRow> {
        use TaskKind::{Agent, Job};
        use TaskRowStatus::{Completed, Failed, Finished, Running};
        vec![
            task_under("agent_lead_1a2b3c4d", Agent, "lead", Running, None, 300),
            task_under(
                "agent_review_2b3c4d5e",
                Agent,
                "review",
                Running,
                Some("agent_lead_1a2b3c4d"),
                200,
            ),
            task_under(
                "agent_file_3c4d5e6f",
                Agent,
                "file-review",
                Finished,
                Some("agent_review_2b3c4d5e"),
                150,
            ),
            task_under(
                "job_4d5e6f7a",
                Job,
                "execute_command",
                Running,
                Some("agent_review_2b3c4d5e"),
                63,
            ),
            task_under("agent_old_5e6f7a8b", Agent, "planner", Finished, None, 900),
            task_under("job_6f7a8b9c", Job, "fetch", Failed, None, 400),
            task_under("job_7a8b9c0d", Job, "build", Completed, None, 500),
            ask(
                "esc_9c0d1e2f",
                "review",
                "[confirm] Ship the fix to main?\nIt touches the release.",
            ),
        ]
    }

    fn draw(state: &RosterState, width: u16, height: u16, no_color: bool) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    RosterPopup {
                        state,
                        theme: &Theme::fallback(no_color),
                        no_color,
                        now: wall(),
                        offset: utc(),
                    },
                    frame.area(),
                );
            })
            .unwrap();
        terminal
    }

    fn text(terminal: &Terminal<TestBackend>) -> String {
        terminal.backend().to_string()
    }

    fn focused_id(state: &RosterState) -> Option<String> {
        state.focused().map(|row| match row {
            RosterRow::Task(node) => node.task.id,
            RosterRow::Escalation(ask) => ask.id,
        })
    }

    /// The tree nests agents under their spawner three deep with the job
    /// under the middle one, running first then oldest first; the times
    /// come from the injected clock; the question follows the tree.
    #[test]
    fn the_tasks_tree_nests_orders_and_times_its_rows() {
        let mut state = RosterState::new(tree_rows());
        state.move_highlight(1);
        let terminal = draw(&state, 80, 24, false);
        insta::assert_snapshot!("tasks_tree_80x24", text(&terminal));
        let terminal = draw(&state, 40, 20, false);
        insta::assert_snapshot!("tasks_tree_40x20", text(&terminal));

        let ids: Vec<String> = state
            .shown()
            .into_iter()
            .map(|row| state.id(row).to_string())
            .collect();
        assert_eq!(
            ids,
            [
                "agent_lead_1a2b3c4d",
                "agent_review_2b3c4d5e",
                "job_4d5e6f7a",
                "agent_file_3c4d5e6f",
                "agent_old_5e6f7a8b",
                "job_7a8b9c0d",
                "job_6f7a8b9c",
                "esc_9c0d1e2f",
            ]
        );
        let shown = text(&draw(&state, 100, 24, false));
        let job = shown
            .lines()
            .find(|line| line.contains("execute_command"))
            .unwrap();
        assert!(
            job.contains(&format!("{} running  03:03:02  1m 3s", SPINNER_FRAMES[0])),
            "{job}"
        );
        let planner = shown.lines().find(|line| line.contains("planner")).unwrap();
        assert!(planner.contains("finished  02:49:05"), "{planner}");
    }

    /// Folds hide a subtree and keep the highlight on the folded task;
    /// Right on an open task steps to its first child, Left on a leaf to
    /// its parent; a click on an arrow folds and elsewhere selects.
    #[test]
    fn the_tasks_tree_folds_steps_and_takes_clicks() {
        let mut state = RosterState::new(tree_rows());
        state.move_highlight(1);
        assert_eq!(focused_id(&state).as_deref(), Some("agent_review_2b3c4d5e"));
        state.fold();
        assert_eq!(state.shown().len(), 6);
        state.unfold();
        assert_eq!(state.shown().len(), 8);
        state.unfold();
        assert_eq!(focused_id(&state).as_deref(), Some("job_4d5e6f7a"));
        state.fold();
        assert_eq!(focused_id(&state).as_deref(), Some("agent_review_2b3c4d5e"));
        state.toggle();
        assert_eq!(state.shown().len(), 6);
        state.toggle();
        state.fold();
        state.fold();
        assert_eq!(focused_id(&state).as_deref(), Some("agent_lead_1a2b3c4d"));
        state.refresh(tree_rows());
        assert_eq!(state.shown().len(), 6, "a refresh keeps the folds");

        let area = Rect::new(0, 0, 80, 24);
        state.set_area(area);
        let inner = |state: &RosterState| {
            popup_area(area, state.shown().len()).inner(super::super::help::MARGIN)
        };
        let rows = inner(&state);
        state.click(rows.x + 2, rows.y + 1);
        assert_eq!(focused_id(&state).as_deref(), Some("agent_review_2b3c4d5e"));
        assert_eq!(state.shown().len(), 8, "a click on the arrow unfolds");
        let rows = inner(&state);
        state.click(rows.x + 20, rows.y + 6);
        assert_eq!(focused_id(&state).as_deref(), Some("job_6f7a8b9c"));
        assert_eq!(state.shown().len(), 8, "a click off the arrow only selects");
        state.click(rows.x + 20, rows.y + 7);
        assert_eq!(focused_id(&state).as_deref(), Some("esc_9c0d1e2f"));
        state.click(rows.x, rows.y);
        assert_eq!(state.shown().len(), 5, "a click on the top arrow folds");
    }

    /// Under `NO_COLOR` the states keep their words and the focus its
    /// reverse, without a colour.
    #[test]
    fn the_tasks_tree_drops_its_colours_under_no_color() {
        let mut state = RosterState::new(tree_rows());
        state.move_highlight(1);
        let coloured = draw(&state, 80, 24, false);
        let buf = coloured.backend().buffer();
        let lead = text(&coloured)
            .lines()
            .position(|line| line.contains("lead"))
            .unwrap();
        let fg = (0..80).map(|x| buf[(x, lead as u16)].fg);
        assert!(fg.clone().any(|fg| fg != Color::Reset));

        let terminal = draw(&state, 80, 24, true);
        let shown = text(&terminal);
        for word in ["running", "finished", "failed", "completed"] {
            assert!(shown.contains(word), "{shown}");
        }
        let buf = terminal.backend().buffer();
        for y in 0..24 {
            for x in 0..80 {
                assert_eq!(buf[(x, y)].fg, Color::Reset, "({x}, {y}) is coloured");
            }
        }
        let review = shown
            .lines()
            .position(|line| line.contains("review "))
            .unwrap();
        assert!(
            buf[(12, review as u16)]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    /// The highlight follows its row through a refresh. When that row
    /// goes, nothing is focused or reversed until a move picks a row. An
    /// area too short for every row keeps the highlighted one in view.
    #[test]
    fn a_refresh_keeps_the_highlight_on_its_row_and_the_view_follows_it() {
        let planner = || {
            task(
                "agent_planner_0f1e2d3c",
                TaskKind::Agent,
                "planner",
                TaskRowStatus::Running,
            )
        };
        let mut state = RosterState::new(tree_rows());
        state.move_highlight(2);
        let mut rows = tree_rows();
        rows.push(planner());
        state.refresh(rows);
        assert_eq!(focused_id(&state).as_deref(), Some("job_4d5e6f7a"));
        state.refresh(vec![planner()]);
        assert!(state.focused().is_none());
        state.move_highlight(1);
        assert_eq!(
            focused_id(&state).as_deref(),
            Some("agent_planner_0f1e2d3c")
        );
        state.refresh(Vec::new());
        assert!(state.focused().is_none());

        let mut state = RosterState::new(tree_rows());
        state.move_highlight(7);
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 2));
        RosterView {
            state: &state,
            theme: &Theme::fallback(false),
            no_color: false,
            now: wall(),
            offset: utc(),
        }
        .render(Rect::new(0, 0, 60, 2), &mut buf);
        let shown: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(shown.contains("ask review"), "{shown}");
        assert!(!shown.contains("lead"), "{shown}");
    }

    /// A roster opened with no rows focuses the first row that arrives.
    #[test]
    fn a_roster_opened_empty_focuses_the_first_row_to_arrive() {
        let mut state = RosterState::new(Vec::new());
        assert!(state.focused().is_none());
        state.refresh(vec![task(
            "agent_planner_0f1e2d3c",
            TaskKind::Agent,
            "planner",
            TaskRowStatus::Running,
        )]);
        assert_eq!(
            focused_id(&state).as_deref(),
            Some("agent_planner_0f1e2d3c")
        );
    }

    /// A move while the roster is empty picks nothing, so the row that
    /// arrives next is not focused for the user.
    #[test]
    fn a_move_on_an_empty_roster_keeps_a_lost_focus_lost() {
        let mut state = RosterState::new(tree_rows());
        state.refresh(Vec::new());
        assert!(state.focus_lost);
        state.move_highlight(1);
        state.move_highlight(-1);
        assert!(state.focus_lost);
        state.refresh(vec![task(
            "agent_planner_0f1e2d3c",
            TaskKind::Agent,
            "planner",
            TaskRowStatus::Running,
        )]);
        assert!(state.focused().is_none());
        assert!(state.focus_lost);
    }

    #[test]
    fn the_roster_clamps_to_tiny_and_empty_areas() {
        let state = RosterState::new(tree_rows());
        let theme = Theme::fallback(false);
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        for area in [Rect::new(0, 0, 80, 24), Rect::new(0, 0, 0, 0)] {
            RosterPopup {
                state: &state,
                theme: &theme,
                no_color: false,
                now: wall(),
                offset: utc(),
            }
            .render(area, &mut buf);
        }
        let empty = RosterState::default();
        let terminal = draw(&empty, 40, 5, false);
        assert!(text(&terminal).contains("(no tasks"), "{}", text(&terminal));
    }

    /// A finished task stops its clock: two frames apart show the same
    /// row. A running one counts on.
    #[test]
    fn only_a_running_task_shows_how_long_it_has_run() {
        let row = |status, later: u64| {
            let mut tick = tick();
            tick.now += Duration::from_secs(later);
            let task = TaskRow {
                id: "job_5e6f7a8b".to_string(),
                kind: TaskKind::Job,
                name: "execute_command".to_string(),
                started_at: Some(origin() + Duration::from_secs(3600 - 75)),
                status,
                session_id: None,
                parent_id: None,
            };
            let now = wall() + chrono::Duration::seconds(later as i64);
            let tree = Tree::build(None, vec![TaskNode::new(task, tick, now)]);
            tree.line(0, &HashSet::new(), 100, now, utc(), &Theme::fallback(true))
                .to_string()
        };
        assert_eq!(
            row(TaskRowStatus::Failed, 0),
            "  execute_command  failed  03:02:50  job_5e6f7a8b"
        );
        assert_eq!(
            row(TaskRowStatus::Failed, 0),
            row(TaskRowStatus::Failed, 60)
        );
        assert_eq!(
            row(TaskRowStatus::Running, 0),
            format!(
                "  execute_command  {} running  03:02:50  1m 15s  job_5e6f7a8b",
                SPINNER_FRAMES[0]
            )
        );
        assert_ne!(
            row(TaskRowStatus::Running, 0),
            row(TaskRowStatus::Running, 60)
        );
    }

    #[test]
    fn questions_come_from_the_queue_with_their_options_and_without_their_kind() {
        let queue = Arc::new(EscalationQueue::new());
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        queue.submit(crate::supervisor::escalation::EscalationRequest {
            id: "esc_3a4b5c6d".to_string(),
            from_agent_id: "agent_planner_7d8e9f0a".to_string(),
            from_agent_name: "planner".to_string(),
            question: "[select] Which branch?".to_string(),
            options: Some(vec!["main".to_string(), "dev".to_string()]),
            reply_tx,
        });
        let sources = RosterSources {
            supervisor: None,
            escalations: Some(queue),
        };
        let rows = sources.rows(tick(), wall());
        let ask = Escalation {
            id: "esc_3a4b5c6d".to_string(),
            agent_id: "agent_planner_7d8e9f0a".to_string(),
            agent_name: "planner".to_string(),
            question: "[select] Which branch?".to_string(),
            options: vec!["main".to_string(), "dev".to_string()],
        };
        assert_eq!(rows, vec![RosterRow::Escalation(ask.clone())]);
        assert_eq!(ask.line(true).to_string(), "  ask planner  Which branch?");
    }

    #[test]
    fn a_question_whose_asker_is_gone_gets_no_row() {
        let queue = Arc::new(EscalationQueue::new());
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        queue.submit(crate::supervisor::escalation::EscalationRequest {
            id: "esc_3a4b5c6d".to_string(),
            from_agent_id: "agent_planner_7d8e9f0a".to_string(),
            from_agent_name: "planner".to_string(),
            question: "[select] Which branch?".to_string(),
            options: None,
            reply_tx,
        });
        drop(reply_rx);
        let sources = RosterSources {
            supervisor: None,
            escalations: Some(queue),
        };
        assert_eq!(sources.rows(tick(), wall()), vec![]);
    }
}
