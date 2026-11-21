//! The roster, which the UI calls tasks: one row per task the supervisor
//! runs (spawned agents and background jobs) and one per question an agent
//! is waiting on the user to answer, as a pop-up over the transcript on
//! Alt+T. The rows are read every frame it is up from the supervisor and
//! escalation queue the context held when it was last back in the UI's
//! hands, so they follow a turn that is running.

use super::bottom_pane::{first_line, step_highlight};
use super::cells::elapsed_text;
use super::help::MARGIN;
use super::theme::Theme;
use super::{ChromeKind, DIM, chrome_style};
use crate::events::NoticeKind;
use crate::supervisor::escalation::{EscalationQueue, PendingEscalation};
use crate::supervisor::{Supervisor, TaskKind, TaskRow, TaskRowStatus};
use parking_lot::RwLock;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Widget};
use std::sync::Arc;
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

const TITLE: &str = "Tasks";
const HINT: &str = "j/k = move  x = cancel  o = open  Enter = answer  Esc = close";
/// The hint on a frame too narrow for all of it: the bottom border cuts
/// it from the right, so the way out comes first.
const NARROW_HINT: &str = "Esc = close  j/k = move  x = cancel  o = open  Enter = answer";
const EMPTY: &str = "(no tasks and no questions)";

/// Where the roster reads its rows from: the context's supervisor and
/// escalation queue as of the last time the context was in hand.
#[derive(Clone, Default)]
pub(super) struct RosterSources {
    pub(super) supervisor: Option<Arc<RwLock<Supervisor>>>,
    pub(super) escalations: Option<Arc<EscalationQueue>>,
}

impl RosterSources {
    /// The tasks sorted by id, then the questions sorted by id. Ids end
    /// in random hex, so this is not the order the tasks were spawned or
    /// the questions asked in; it is only stable, so a row stays put
    /// between frames while others come and go.
    pub(super) fn rows(&self) -> Vec<RosterRow> {
        let mut tasks: Vec<TaskRow> = self
            .supervisor
            .as_ref()
            .map(|supervisor| supervisor.read().task_snapshot())
            .unwrap_or_default();
        tasks.sort_by(|a, b| a.id.cmp(&b.id));
        let mut rows: Vec<RosterRow> = tasks.into_iter().map(RosterRow::Task).collect();
        if let Some(queue) = &self.escalations {
            let mut asks: Vec<Escalation> =
                queue.pending().into_iter().map(Escalation::from).collect();
            asks.sort_by(|a, b| a.id.cmp(&b.id));
            rows.extend(asks.into_iter().map(RosterRow::Escalation));
        }
        rows
    }

    /// The running agents and jobs for the footer, counted the way the
    /// rows show them but through every child agent's supervisor too.
    pub(super) fn running_counts(&self) -> (usize, usize) {
        self.supervisor
            .as_ref()
            .map_or((0, 0), |supervisor| supervisor.read().running_counts())
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RosterRow {
    Task(TaskRow),
    Escalation(Escalation),
}

impl RosterRow {
    fn id(&self) -> &str {
        match self {
            RosterRow::Task(task) => &task.id,
            RosterRow::Escalation(ask) => &ask.id,
        }
    }

    fn label(&self) -> String {
        let (kind, name) = match self {
            RosterRow::Task(TaskRow {
                kind: TaskKind::Agent,
                name,
                ..
            }) => ("agent", name),
            RosterRow::Task(TaskRow {
                kind: TaskKind::Job,
                name,
                ..
            }) => ("job", name),
            RosterRow::Escalation(ask) => ("ask", &ask.agent_name),
        };
        super::render::plain_text(&format!("{kind:<5} {name}"))
    }

    /// The row's text after its label: a task's id, padded to `id_width`
    /// columns so the states line up, its state and, while it runs, how
    /// long it has run by `now`; a question's first line, without the
    /// `[kind] ` that names its modal, and how many follow.
    fn detail(&self, id_width: usize, now: Instant) -> String {
        match self {
            RosterRow::Task(task) => {
                let status = match task.status {
                    TaskRowStatus::Running => "running",
                    TaskRowStatus::Finished => "finished",
                    TaskRowStatus::Completed => "completed",
                    TaskRowStatus::Failed => "failed",
                };
                let id = super::render::plain_text(&task.id);
                let pad = id_width.saturating_sub(id.width());
                let mut detail = format!("{id}{:pad$}  {status}", "");
                if task.status == TaskRowStatus::Running
                    && let Some(started) = task.started_at
                {
                    detail.push_str("  ");
                    detail.push_str(&elapsed_text(now.saturating_duration_since(started)));
                }
                detail
            }
            RosterRow::Escalation(ask) => {
                let (_, question) = super::escalation_kind(&ask.question);
                let (first, tail) = first_line(question);
                if tail.is_empty() {
                    first
                } else {
                    format!("{first}  {tail}")
                }
            }
        }
    }

    /// A question waits on the user and a failed task needs a look; both
    /// stand out, in colour only while colour is allowed.
    fn style(&self, no_color: bool) -> Style {
        match self {
            RosterRow::Escalation(_) => {
                chrome_style(ChromeKind::Notice(NoticeKind::ToolWarning), no_color)
            }
            RosterRow::Task(TaskRow {
                status: TaskRowStatus::Failed,
                ..
            }) => chrome_style(ChromeKind::Error, no_color),
            RosterRow::Task(_) => Style::new(),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct RosterState {
    rows: Vec<RosterRow>,
    /// The id of the focused row. None once that row has gone, until
    /// the user moves onto another, so no key acts on a row they did not
    /// pick.
    focus: Option<String>,
    /// The focused row went in a refresh. The focus stays clear, where a
    /// roster that never had a row focuses the first one it gets, until a
    /// move lands on a row.
    focus_lost: bool,
}

impl RosterState {
    pub(super) fn new(rows: Vec<RosterRow>) -> Self {
        let focus = rows.first().map(|row| row.id().to_string());
        Self {
            rows,
            focus,
            focus_lost: false,
        }
    }

    /// Takes a fresh read, keeping the focus on its row while that row
    /// is still listed; a row that went takes the focus with it. With no
    /// focus that a refresh took, the first row is focused.
    pub(super) fn refresh(&mut self, rows: Vec<RosterRow>) {
        self.rows = rows;
        if self.focus.is_none() && !self.focus_lost {
            self.focus = self.rows.first().map(|row| row.id().to_string());
        } else if self.highlight().is_none() {
            self.focus = None;
            self.focus_lost = true;
        }
    }

    /// With no focus, moving down picks the first row and up the last. A
    /// move on an empty roster lands nowhere, so a lost focus stays lost.
    pub(super) fn move_highlight(&mut self, delta: i32) {
        let index = match self.highlight() {
            Some(index) => step_highlight(index, delta, self.rows.len()),
            None if delta < 0 => self.rows.len().saturating_sub(1),
            None => 0,
        };
        self.focus = self.rows.get(index).map(|row| row.id().to_string());
        if self.focus.is_some() {
            self.focus_lost = false;
        }
    }

    fn highlight(&self) -> Option<usize> {
        let focus = self.focus.as_deref()?;
        self.rows.iter().position(|row| row.id() == focus)
    }

    pub(super) fn focused(&self) -> Option<&RosterRow> {
        self.rows.get(self.highlight()?)
    }
}

/// The tasks pop-up over the transcript, framed like the key help: a
/// title, the rows, and the keys on the bottom border. It is as tall as
/// its rows within `area` less the margin, and centred in it.
pub(super) struct RosterPopup<'a> {
    pub(super) state: &'a RosterState,
    pub(super) theme: &'a Theme,
    pub(super) no_color: bool,
    /// What a job's running time is counted to.
    pub(super) now: Instant,
}

impl Widget for RosterPopup<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area()).inner(MARGIN);
        if area.is_empty() {
            return;
        }
        let rows = u16::try_from(self.state.rows.len().max(1)).unwrap_or(u16::MAX);
        let height = rows.saturating_add(2).min(area.height);
        let area = Rect::new(
            area.x,
            area.y + (area.height - height) / 2,
            area.width,
            height,
        );
        let hint = if HINT.width() + 2 <= usize::from(area.width) {
            HINT
        } else {
            NARROW_HINT
        };
        let block = Block::bordered()
            .title(Span::styled(TITLE, self.theme.heading))
            .title_bottom(Line::styled(hint, DIM));
        let inner = block.inner(area);
        Clear.render(area, buf);
        block.render(area, buf);
        RosterView {
            state: self.state,
            no_color: self.no_color,
            now: self.now,
        }
        .render(inner, buf);
    }
}

/// The roster's rows: one per task or question with the highlighted one
/// reversed, scrolled to keep it in view.
struct RosterView<'a> {
    state: &'a RosterState,
    no_color: bool,
    /// What a job's running time is counted to.
    now: Instant,
}

impl Widget for RosterView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let rows = &self.state.rows;
        if rows.is_empty() {
            Paragraph::new(Line::from(Span::styled(EMPTY, DIM))).render(area, buf);
            return;
        }
        let height = usize::from(area.height);
        let highlight = self.state.highlight();
        let start = highlight.unwrap_or(0).saturating_sub(height - 1);
        // Widths over every row, so scrolling does not move the columns.
        let label_width = rows
            .iter()
            .map(|row| row.label().width())
            .max()
            .unwrap_or(0);
        let id_width = rows
            .iter()
            .filter_map(|row| match row {
                RosterRow::Task(task) => Some(super::render::plain_text(&task.id).width()),
                RosterRow::Escalation(_) => None,
            })
            .max()
            .unwrap_or(0);
        let visible = &rows[start..(start + height).min(rows.len())];
        for (offset, row) in visible.iter().enumerate() {
            let y = area.y + u16::try_from(offset).unwrap_or(u16::MAX);
            let line_area = Rect::new(area.x, y, area.width, 1);
            let style = row.style(self.no_color);
            let label = row.label();
            let pad = label_width.saturating_sub(label.width());
            let line = Line::from(vec![
                Span::styled(format!("{label}{:pad$}", ""), style),
                Span::raw("  "),
                Span::styled(row.detail(id_width, self.now), DIM.patch(style)),
            ]);
            Paragraph::new(line).render(line_area, buf);
            if highlight == Some(start + offset) {
                buf.set_style(line_area, Style::new().add_modifier(Modifier::REVERSED));
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;
    use std::sync::OnceLock;
    use std::time::Duration;

    /// When every test job started; frames count its running time from it.
    fn started() -> Instant {
        static STARTED: OnceLock<Instant> = OnceLock::new();
        *STARTED.get_or_init(Instant::now)
    }

    fn frame_time() -> Instant {
        started() + Duration::from_secs(75)
    }

    pub(in crate::tui) fn task(
        id: &str,
        kind: TaskKind,
        name: &str,
        status: TaskRowStatus,
    ) -> RosterRow {
        RosterRow::Task(TaskRow {
            id: id.to_string(),
            kind,
            name: name.to_string(),
            started_at: (kind == TaskKind::Job).then(started),
            status,
            session_id: (kind == TaskKind::Agent).then(|| format!("{name}-session")),
        })
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

    fn mixed() -> RosterState {
        let mut state = RosterState::new(vec![
            task(
                "agent_reviewer_1a2b3c4d",
                TaskKind::Agent,
                "reviewer",
                TaskRowStatus::Running,
            ),
            task(
                "job_5e6f7a8b",
                TaskKind::Job,
                "execute_command",
                TaskRowStatus::Failed,
            ),
            ask(
                "esc_9c0d1e2f",
                "reviewer",
                "[confirm] Ship the fix to main?\nIt touches the release.",
            ),
        ]);
        state.move_highlight(1);
        state
    }

    fn draw(state: &RosterState, width: u16, height: u16, no_color: bool) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    RosterView {
                        state,
                        no_color,
                        now: frame_time(),
                    },
                    frame.area(),
                );
            })
            .unwrap();
        terminal
    }

    #[test]
    fn the_roster_lists_agents_jobs_and_questions_with_the_focus_reversed() {
        let state = mixed();
        let terminal = draw(&state, 80, 24, false);
        insta::assert_snapshot!("roster_mixed_80x24", terminal.backend().to_string());
        let buf = terminal.backend().buffer();
        assert!(buf[(0, 1)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(0, 2)].fg, Color::Yellow);
        let terminal = draw(&state, 40, 12, false);
        insta::assert_snapshot!("roster_mixed_40x12", terminal.backend().to_string());
    }

    /// Under `NO_COLOR` the question and the failure keep their rows and
    /// the reverse of the focus, without a colour.
    #[test]
    fn the_roster_drops_its_colours_under_no_color() {
        let state = mixed();
        let terminal = draw(&state, 80, 24, true);
        insta::assert_snapshot!(
            "roster_mixed_no_color_80x24",
            terminal.backend().to_string()
        );
        let buf = terminal.backend().buffer();
        for y in 0..5 {
            for x in 0..80 {
                assert_eq!(buf[(x, y)].fg, Color::Reset, "({x}, {y}) is coloured");
            }
        }
        assert!(buf[(0, 1)].modifier.contains(Modifier::REVERSED));
    }

    /// The highlight follows its row through a refresh. When that row
    /// goes, nothing is focused or reversed until a move picks a row. An
    /// area too short for every row keeps the highlighted one in view.
    #[test]
    fn a_refresh_keeps_the_highlight_on_its_row_and_the_view_follows_it() {
        let mut state = mixed();
        state.refresh(vec![
            task(
                "agent_planner_0f1e2d3c",
                TaskKind::Agent,
                "planner",
                TaskRowStatus::Running,
            ),
            task(
                "agent_reviewer_1a2b3c4d",
                TaskKind::Agent,
                "reviewer",
                TaskRowStatus::Running,
            ),
            task(
                "job_5e6f7a8b",
                TaskKind::Job,
                "execute_command",
                TaskRowStatus::Failed,
            ),
        ]);
        assert_eq!(state.focused().map(RosterRow::id), Some("job_5e6f7a8b"));
        state.refresh(vec![task(
            "agent_planner_0f1e2d3c",
            TaskKind::Agent,
            "planner",
            TaskRowStatus::Running,
        )]);
        assert!(state.focused().is_none());
        let terminal = draw(&state, 40, 2, false);
        let buf = terminal.backend().buffer();
        assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
        state.move_highlight(1);
        assert_eq!(
            state.focused().map(RosterRow::id),
            Some("agent_planner_0f1e2d3c")
        );
        state.refresh(Vec::new());
        assert!(state.focused().is_none());

        let mut state = mixed();
        state.move_highlight(5);
        let terminal = draw(&state, 40, 2, false);
        let shown: String = terminal.backend().to_string();
        assert!(shown.contains("ask"), "{shown}");
        assert!(!shown.contains("agent"), "{shown}");
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
            state.focused().map(RosterRow::id),
            Some("agent_planner_0f1e2d3c")
        );
    }

    /// A move while the roster is empty picks nothing, so the row that
    /// arrives next is not focused for the user.
    #[test]
    fn a_move_on_an_empty_roster_keeps_a_lost_focus_lost() {
        let mut state = mixed();
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
        let state = mixed();
        let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
        RosterView {
            state: &state,
            no_color: false,
            now: frame_time(),
        }
        .render(Rect::new(0, 0, 80, 24), &mut buf);
        RosterView {
            state: &state,
            no_color: false,
            now: frame_time(),
        }
        .render(Rect::new(0, 0, 0, 0), &mut buf);
        let empty = RosterState::default();
        let terminal = draw(&empty, 40, 3, false);
        assert!(terminal.backend().to_string().contains("(no tasks"));
    }

    /// A job that is no longer running stops its clock: two frames apart
    /// show the same row. A running one counts on.
    #[test]
    fn only_a_running_task_shows_how_long_it_has_run() {
        let failed = task(
            "job_5e6f7a8b",
            TaskKind::Job,
            "execute_command",
            TaskRowStatus::Failed,
        );
        let later = frame_time() + Duration::from_secs(60);
        assert_eq!(failed.detail(12, frame_time()), failed.detail(12, later));
        assert_eq!(failed.detail(12, later), "job_5e6f7a8b  failed");
        let running = task(
            "job_5e6f7a8b",
            TaskKind::Job,
            "execute_command",
            TaskRowStatus::Running,
        );
        assert_eq!(
            running.detail(12, frame_time()),
            "job_5e6f7a8b  running  75.0s"
        );
        assert_ne!(running.detail(12, frame_time()), running.detail(12, later));
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
        let rows = sources.rows();
        assert_eq!(
            rows,
            vec![RosterRow::Escalation(Escalation {
                id: "esc_3a4b5c6d".to_string(),
                agent_id: "agent_planner_7d8e9f0a".to_string(),
                agent_name: "planner".to_string(),
                question: "[select] Which branch?".to_string(),
                options: vec!["main".to_string(), "dev".to_string()],
            })]
        );
        assert_eq!(rows[0].detail(0, frame_time()), "Which branch?");
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
        assert_eq!(sources.rows(), vec![]);
    }
}
