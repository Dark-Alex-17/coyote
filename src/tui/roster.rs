//! The roster: one row per task the supervisor runs (spawned agents and
//! background jobs) and one per question an agent is waiting on the user
//! to answer, as a band over the composer on F3. The rows are read every
//! frame it is up from the supervisor and escalation queue the context
//! held when it was last back in the UI's hands, so they follow a turn
//! that is running.

use super::bottom_pane::{
    BAND_MAX_ROWS, first_line, render_header, rows_within_cap, step_highlight,
};
use super::cells::elapsed_text;
use super::{ChromeKind, DIM, chrome_style};
use crate::events::NoticeKind;
use crate::supervisor::escalation::EscalationQueue;
use crate::supervisor::{Supervisor, TaskKind, TaskRow, TaskRowStatus};
use parking_lot::RwLock;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

const HEADER: &str = "Tasks  j/k = move  x = cancel  o = open  Enter = answer  Esc = close";
const EMPTY: &str = "(no tasks and no questions)";

/// Where the roster reads its rows from: the context's supervisor and
/// escalation queue as of the last time the context was in hand.
#[derive(Clone, Default)]
pub(super) struct RosterSources {
    pub(super) supervisor: Option<Arc<RwLock<Supervisor>>>,
    pub(super) escalations: Option<Arc<EscalationQueue>>,
}

impl RosterSources {
    /// The tasks by id, a shared prefix's numbers in number order
    /// (`agent-2` before `agent-10`), then the questions by id.
    pub(super) fn rows(&self) -> Vec<RosterRow> {
        let mut tasks: Vec<TaskRow> = self
            .supervisor
            .as_ref()
            .map(|supervisor| supervisor.read().task_snapshot())
            .unwrap_or_default();
        tasks.sort_by(|a, b| natural_key(&a.id).cmp(&natural_key(&b.id)));
        let mut rows: Vec<RosterRow> = tasks.into_iter().map(RosterRow::Task).collect();
        if let Some(queue) = &self.escalations {
            let mut asks: Vec<Escalation> = queue
                .pending_summary()
                .iter()
                .filter_map(Escalation::from_summary)
                .collect();
            asks.sort_by(|a, b| a.id.cmp(&b.id));
            rows.extend(asks.into_iter().map(RosterRow::Escalation));
        }
        rows
    }
}

/// An id split as its text before a run of trailing digits, then the
/// digits compared by value: the count of digits first, then the digits.
fn natural_key(id: &str) -> (&str, usize, &str) {
    let digits_at = id
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_digit())
        .last()
        .map_or(id.len(), |(at, _)| at);
    let digits = id[digits_at..].trim_start_matches('0');
    (&id[..digits_at], digits.len(), digits)
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

impl Escalation {
    fn from_summary(entry: &Value) -> Option<Self> {
        let text = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            id: text("escalation_id")?,
            agent_id: text("from_agent_id")?,
            agent_name: text("from_agent_name").unwrap_or_default(),
            question: text("question").unwrap_or_default(),
            options: entry
                .get("options")
                .and_then(Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| option.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        })
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
    /// columns so the states line up, its state and how long a job has
    /// run by `now`; a question's first line and how many follow.
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
                if let Some(started) = task.started_at {
                    detail.push_str("  ");
                    detail.push_str(&elapsed_text(now.saturating_duration_since(started)));
                }
                detail
            }
            RosterRow::Escalation(ask) => {
                let (first, tail) = first_line(&ask.question);
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
    highlight: usize,
}

impl RosterState {
    pub(super) fn new(rows: Vec<RosterRow>) -> Self {
        Self { rows, highlight: 0 }
    }

    /// Takes a fresh read, keeping the highlight on its row while that
    /// row is still listed; a row that went leaves it where it was.
    pub(super) fn refresh(&mut self, rows: Vec<RosterRow>) {
        let current = self.focused().map(|row| row.id().to_string());
        self.rows = rows;
        let last = self.rows.len().saturating_sub(1);
        self.highlight = current
            .and_then(|id| self.rows.iter().position(|row| row.id() == id))
            .unwrap_or(self.highlight.min(last));
    }

    pub(super) fn move_highlight(&mut self, delta: i32) {
        self.highlight = step_highlight(self.highlight, delta, self.rows.len());
    }

    pub(super) fn focused(&self) -> Option<&RosterRow> {
        self.rows.get(self.highlight)
    }

    pub(super) fn band_rows(&self) -> u16 {
        1 + rows_within_cap(self.rows.len().max(1)).min(BAND_MAX_ROWS - 1)
    }
}

/// The roster's band: the key legend, then one row per task or question
/// with the highlighted one reversed, scrolled to keep it in view.
pub(super) struct RosterView<'a> {
    pub(super) state: &'a RosterState,
    pub(super) no_color: bool,
    /// What a job's running time is counted to.
    pub(super) now: Instant,
}

impl Widget for RosterView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let rows_area = render_header(Line::from(Span::styled(HEADER, DIM)), area, buf);
        if rows_area.is_empty() {
            return;
        }
        let rows = &self.state.rows;
        if rows.is_empty() {
            Paragraph::new(Line::from(Span::styled(EMPTY, DIM))).render(rows_area, buf);
            return;
        }
        let height = usize::from(rows_area.height);
        let start = self.state.highlight.saturating_sub(height - 1);
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
            let y = rows_area.y + u16::try_from(offset).unwrap_or(u16::MAX);
            let line_area = Rect::new(rows_area.x, y, rows_area.width, 1);
            let style = row.style(self.no_color);
            let label = row.label();
            let pad = label_width.saturating_sub(label.width());
            let line = Line::from(vec![
                Span::styled(format!("{label}{:pad$}", ""), style),
                Span::raw("  "),
                Span::styled(row.detail(id_width, self.now), DIM.patch(style)),
            ]);
            Paragraph::new(line).render(line_area, buf);
            if start + offset == self.state.highlight {
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
            agent_id: format!("{agent}-id"),
            agent_name: agent.to_string(),
            question: question.to_string(),
            options: vec!["yes".to_string(), "no".to_string()],
        })
    }

    fn mixed() -> RosterState {
        let mut state = RosterState::new(vec![
            task(
                "agent-1",
                TaskKind::Agent,
                "reviewer",
                TaskRowStatus::Running,
            ),
            task(
                "job-2",
                TaskKind::Job,
                "execute_command",
                TaskRowStatus::Failed,
            ),
            ask(
                "esc_1",
                "reviewer",
                "Ship the fix to main?\nIt touches the release.",
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
        assert!(buf[(0, 2)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(0, 1)].modifier.contains(Modifier::REVERSED));
        assert_eq!(buf[(0, 3)].fg, Color::Yellow);
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
        assert!(buf[(0, 2)].modifier.contains(Modifier::REVERSED));
    }

    /// The highlight follows its row through a refresh, and a band too
    /// short for every row keeps the highlighted one in view.
    #[test]
    fn a_refresh_keeps_the_highlight_on_its_row_and_the_view_follows_it() {
        let mut state = mixed();
        state.refresh(vec![
            task(
                "agent-0",
                TaskKind::Agent,
                "planner",
                TaskRowStatus::Running,
            ),
            task(
                "agent-1",
                TaskKind::Agent,
                "reviewer",
                TaskRowStatus::Running,
            ),
            task(
                "job-2",
                TaskKind::Job,
                "execute_command",
                TaskRowStatus::Failed,
            ),
        ]);
        assert_eq!(state.focused().map(RosterRow::id), Some("job-2"));
        state.refresh(vec![task(
            "agent-0",
            TaskKind::Agent,
            "planner",
            TaskRowStatus::Running,
        )]);
        assert_eq!(state.focused().map(RosterRow::id), Some("agent-0"));
        state.refresh(Vec::new());
        assert!(state.focused().is_none());
        assert_eq!(state.band_rows(), 2);

        let mut state = mixed();
        state.move_highlight(5);
        let terminal = draw(&state, 40, 3, false);
        let shown: String = terminal.backend().to_string();
        assert!(shown.contains("ask"), "{shown}");
        assert!(!shown.contains("agent"), "{shown}");
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

    /// Ids that share a prefix sort by their number, not their text.
    #[test]
    fn task_ids_sort_by_their_trailing_number() {
        let mut ids = vec!["agent-10", "job_ff", "agent-2", "agent-1", "agent"];
        ids.sort_by(|a, b| natural_key(a).cmp(&natural_key(b)));
        assert_eq!(ids, ["agent", "agent-1", "agent-2", "agent-10", "job_ff"]);
    }

    #[test]
    fn questions_come_from_the_queue_summary_with_their_options() {
        let queue = Arc::new(EscalationQueue::new());
        let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
        queue.submit(crate::supervisor::escalation::EscalationRequest {
            id: "esc_9".to_string(),
            from_agent_id: "agent-9".to_string(),
            from_agent_name: "planner".to_string(),
            question: "Which branch?".to_string(),
            options: Some(vec!["main".to_string(), "dev".to_string()]),
            reply_tx,
        });
        let sources = RosterSources {
            supervisor: None,
            escalations: Some(queue),
        };
        assert_eq!(
            sources.rows(),
            vec![RosterRow::Escalation(Escalation {
                id: "esc_9".to_string(),
                agent_id: "agent-9".to_string(),
                agent_name: "planner".to_string(),
                question: "Which branch?".to_string(),
                options: vec!["main".to_string(), "dev".to_string()],
            })]
        );
    }
}
