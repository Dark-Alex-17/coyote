use super::bottom_pane::View;
use super::cells::{Cell, LineCache, ToolStatus};
use super::pager::{Pager, ThreadRow};
use super::redirect::FdRedirect;
use super::rewind::{is_undo, rewind_barrier, undo_opener};
use super::roster::{RosterRow, RosterSources};
use super::sink::UiMsg;
use super::{App, CONTROL_TASK_WAIT, TurnStatus, cells, pager, render};
use crate::config::RequestContext;
use crate::config::child_sessions::{ChildMeta, ChildSessionRoot, ChildStatus};
use crate::events::NoticeKind;
use crate::repl::{EXIT_CANCEL_WAIT, run_repl_command};
use crate::supervisor::{TaskKind, TaskRow};
use crate::utils::AbortSignal;
use crate::utils::process::ProcessStamp;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::{JoinHandle, JoinSet};

/// Test-only harness hook: when set to `1`, a turn's work task sleeps
/// past `EXIT_CANCEL_WAIT` after the turn is done, ignoring the abort,
/// so a pty test can check the bounded exit path that leaves a turn
/// behind. Debug builds only; a release binary ignores it.
#[cfg(debug_assertions)]
const STUBBORN_TURN_ENV: &str = "COYOTE_TUI_STUBBORN_TURN";

/// Test-only harness hook: when set to `1`, the exit work panics once its
/// output is captured, so a pty test can check the panic's message
/// reaches the restored terminal. Debug builds only; a release binary
/// ignores it.
#[cfg(debug_assertions)]
const PANIC_AT_EXIT_ENV: &str = "COYOTE_TUI_PANIC_AT_EXIT";

/// What one line produced on the work task.
#[derive(Debug, Default)]
pub(crate) struct TurnOutcome {
    /// `run_repl_command` asked to leave (`.exit`).
    pub(crate) exit: bool,
    pub(crate) error: Option<String>,
    /// Everything the turn printed to fds 1 and 2.
    pub(crate) captured: String,
}

pub(super) fn late_output(tx: UnboundedSender<UiMsg>) -> impl Fn(String) + Send + 'static {
    move |text| {
        let _ = tx.send(UiMsg::Notice {
            kind: NoticeKind::Routing,
            text: render::plain_text(&text),
        });
    }
}

/// Drives a work-task future to completion on a blocking-pool thread.
/// The returned handle joins like a `tokio::spawn` one, but the future
/// may block its thread freely; see [`spawn_turn`] for why it must.
pub(super) fn spawn_work<F>(future: F) -> JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || handle.block_on(future))
}

/// Runs one line the way the REPL would, on its own task, with fds 1 and 2
/// captured. The context comes back with the outcome.
///
/// Runtime rule: the work task must never sit on a runtime worker. The
/// turn blocks its thread for the whole of a tool call and of a sync
/// prompt, and on the multi-thread runtime the time driver is serviced
/// by parked workers, never by the UI loop's `block_on` thread. A worker
/// blocked in the turn can hold the driver up with it, and every
/// timer-driven frame then waits for the turn to end. So the body runs on
/// the blocking pool under a nested `block_on` ([`spawn_work`]).
pub(crate) fn spawn_turn(
    mut ctx: RequestContext,
    abort: AbortSignal,
    line: String,
    ui: UnboundedSender<UiMsg>,
) -> JoinHandle<(RequestContext, TurnOutcome)> {
    spawn_work(async move {
        let guard = match FdRedirect::begin(late_output(ui.clone())) {
            Ok(guard) => Some(guard),
            Err(err) => {
                let _ = ui.send(UiMsg::Notice {
                    kind: NoticeKind::ToolWarning,
                    text: format!("Running the turn without output capture: {err}"),
                });
                None
            }
        };
        let result = Box::pin(run_repl_command(&mut ctx, abort, &line)).await;
        let captured = guard.map(FdRedirect::finish).unwrap_or_default();
        #[cfg(debug_assertions)]
        if std::env::var(STUBBORN_TURN_ENV).as_deref() == Ok("1") {
            std::thread::sleep(2 * EXIT_CANCEL_WAIT);
        }
        let outcome = match result {
            Ok(exit) => TurnOutcome {
                exit,
                error: None,
                captured,
            },
            Err(err) => TurnOutcome {
                exit: false,
                // The REPL's shape: message, then its causes.
                error: Some(crate::utils::pretty_error(&err)),
                captured,
            },
        };
        (ctx, outcome)
    })
}

/// The REPL's teardown on the work task, under the same runtime rule as
/// [`spawn_turn`]: settles background agents, exits the session (which
/// may ask to save it) and closes the agent bracket.
pub(super) fn spawn_exit(
    mut ctx: RequestContext,
    controls: JoinSet<()>,
    abort: AbortSignal,
    ui: UnboundedSender<UiMsg>,
) -> JoinHandle<(RequestContext, Result<()>, String)> {
    spawn_work(async move {
        settle_control_tasks(controls, CONTROL_TASK_WAIT).await;
        // Late output (a panic's message among it) reaches the UI the way
        // a turn's does instead of dying in a discarded pipe.
        let guard = match FdRedirect::begin(late_output(ui.clone())) {
            Ok(guard) => Some(guard),
            Err(err) => {
                let _ = ui.send(UiMsg::Notice {
                    kind: NoticeKind::ToolWarning,
                    text: format!("Leaving without output capture: {err}"),
                });
                None
            }
        };
        #[cfg(debug_assertions)]
        if std::env::var(PANIC_AT_EXIT_ENV).as_deref() == Ok("1") {
            panic!("{PANIC_AT_EXIT_ENV} requested a panic inside the exit work");
        }
        ctx.cancel_background_tasks_and_wait(EXIT_CANCEL_WAIT).await;
        let result = ctx.exit_session();
        ctx.top_level_agent_finished(result.as_ref().err(), Some(&abort));
        let captured = guard.map(FdRedirect::finish).unwrap_or_default();
        (ctx, result, captured)
    })
}

/// Waits up to `wait` for every task in `controls` to end, then aborts
/// the ones still running, so no cancel or child read acts on a fork of
/// the context once the exit work owns it. Aborted tasks are not joined:
/// a child read that already started on a blocking thread cannot be
/// stopped, and joining it would hold the exit past its bound.
pub(super) async fn settle_control_tasks(mut controls: JoinSet<()>, wait: Duration) {
    let drained = async { while controls.join_next().await.is_some() {} };
    if tokio::time::timeout(wait, drained).await.is_err() {
        controls.abort_all();
        controls.detach_all();
    }
}

/// Work the roster or the pager asks of the control context. The loop
/// runs it on a task of its own and the result comes back as a `UiMsg`:
/// an agent's cancel waits seconds for the agent to stop, and a child's
/// session file is parsed whole.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ControlRequest {
    Cancel {
        id: String,
        kind: TaskKind,
        /// The child session root the UI holds, which a turn that spawned
        /// its first child resolved after the control context was taken.
        child_root: Option<Arc<ChildSessionRoot>>,
    },
    LoadChild {
        session_id: String,
        file: PathBuf,
        generation: u64,
    },
}

/// The fork of the control context a roster cancel runs on: it carries
/// the child session root the UI holds, so cancelling a child the running
/// turn spawned finishes that child's record under the right root.
pub(super) fn cancel_context(
    control: &RequestContext,
    child_root: Option<Arc<ChildSessionRoot>>,
) -> RequestContext {
    let mut ctx = control.fork_for_branch();
    if child_root.is_some() {
        ctx.child_session_root = child_root;
    }
    ctx
}

/// Runs `request` off the loop on a fork of the control context.
pub(super) fn spawn_control_request(
    request: ControlRequest,
    control: Arc<RequestContext>,
    tx: UnboundedSender<UiMsg>,
    tasks: &mut JoinSet<()>,
) {
    while tasks.try_join_next().is_some() {}
    match request {
        ControlRequest::Cancel {
            id,
            kind,
            child_root,
        } => {
            let mut ctx = cancel_context(&control, child_root);
            tasks.spawn(async move {
                let args = serde_json::json!({ "id": id });
                let result = catch_cancel(async {
                    match kind {
                        TaskKind::Job => {
                            crate::function::jobs::handle_job_tool(&mut ctx, "job__cancel", &args)
                                .await
                        }
                        TaskKind::Agent => {
                            crate::function::agents::handle_agent_tool(
                                &mut ctx,
                                "agent__cancel",
                                &args,
                            )
                            .await
                        }
                    }
                })
                .await;
                // A closed inbox means the UI is gone; nobody is left to
                // tell.
                let _ = tx.send(UiMsg::TaskCancelled { id, result });
            });
        }
        ControlRequest::LoadChild {
            session_id,
            file,
            generation,
        } => {
            tasks.spawn_blocking(move || {
                let result = catch_child_load(|| pager::load_child(&file, &control));
                let _ = tx.send(UiMsg::ChildLoaded {
                    session_id,
                    generation,
                    result,
                });
            });
        }
    }
}

/// Runs a child load, a panic inside it reported as the error the pager
/// shows rather than lost with the task.
pub(super) fn catch_child_load(
    load: impl FnOnce() -> Result<Vec<pager::ChildEntry>>,
) -> Result<Vec<pager::ChildEntry>, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(load)) {
        Ok(result) => result.map_err(|err| format!("{err:#}")),
        Err(_) => Err("child load panicked".to_string()),
    }
}

/// Runs a roster cancel, a panic inside it reported as the error the UI
/// shows rather than lost with the task.
pub(super) async fn catch_cancel(
    cancel: impl Future<Output = Result<Value>>,
) -> Result<Value, String> {
    use futures_util::FutureExt;
    match std::panic::AssertUnwindSafe(cancel).catch_unwind().await {
        Ok(result) => result.map_err(|err| format!("{err:#}")),
        Err(payload) => {
            let reason = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("no message");
            Err(format!("cancel task panicked: {reason}"))
        }
    }
}

/// A session id that can name a file under the child session root and
/// nowhere else: letters, digits, `_`, `.` and `-` only, never `..`, and
/// one plain path component on every platform (so not `C:x` either).
pub(super) fn is_plain_session_id(session_id: &str) -> bool {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    let mut components = Path::new(session_id).components();
    !session_id.is_empty()
        && session_id.chars().all(allowed)
        && !session_id.contains("..")
        && matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

/// The notice a roster cancel ends with: the tool's message when it
/// gave one, and a warning when it refused.
pub(super) fn cancel_report(id: &str, result: &Result<Value, String>) -> (NoticeKind, String) {
    match result {
        Ok(value) => {
            let message = value.get("message").and_then(Value::as_str);
            match value.get("status").and_then(Value::as_str) {
                Some("ok" | "cancelled") => (
                    NoticeKind::Interrupted,
                    message.map_or_else(|| format!("Cancelled {id}"), str::to_string),
                ),
                _ => (
                    NoticeKind::ToolWarning,
                    format!(
                        "Cancel of {id} refused: {}",
                        message.unwrap_or("no reason given")
                    ),
                ),
            }
        }
        Err(err) => (
            NoticeKind::ToolWarning,
            format!("Cancel of {id} failed: {err}"),
        ),
    }
}

impl App {
    /// Records that a line left for the work task. A command gets its cell
    /// now, so the transcript shows what is running under a modal the
    /// command raises; the output fills in when the turn ends. The view
    /// follows again so the line just sent is not out of sight, and a
    /// focused row lets go: the line opens a new context.
    pub(crate) fn begin_turn(&mut self, line: &str, command: bool) {
        self.transcript.clear_welcome();
        self.transcript.scroll_to_bottom();
        self.transcript.clear_focus();
        let first_cell = self.transcript.len();
        self.transcript.push(if command {
            Cell::command(line)
        } else {
            Cell::user(line)
        });
        self.turn = Some(TurnStatus {
            started: self.clock.now(),
            busy: None,
            thinking_since: None,
            tool: None,
            first_cell,
            requested: false,
        });
        self.schedule_frame();
    }

    /// Commits what the work task brought back.
    pub(crate) fn end_turn(&mut self, command: bool, outcome: &TurnOutcome) {
        let turn = self.turn.take();
        self.close_stream();
        if let Some(turn) = &turn {
            self.fail_unreported_tools(turn.first_cell);
        }
        if command {
            let turn = turn.expect("end_turn follows begin_turn");
            let index = turn.first_cell;
            let prefix = self.config.command_prefix;
            let Some(Cell::Command {
                name,
                output,
                opened,
                rewind_barrier: barrier,
                ..
            }) = self.transcript.cell_mut(index)
            else {
                panic!("begin_turn pushed a Command cell for a command turn");
            };
            *output = outcome.captured.clone();
            *opened = turn.requested;
            // A command that failed before asking the model changed
            // nothing the picker has to stop at; a macro that failed at a
            // later step already recorded the exchanges before it.
            *barrier = (outcome.error.is_none() || turn.requested)
                .then(|| rewind_barrier(name, prefix, &self.macros, turn.requested))
                .flatten();
            // `.undo` took the last exchange out of the context; the
            // transcript drops it too, back to the cell that opened it,
            // and the command cell with it.
            if outcome.error.is_none()
                && is_undo(name, prefix)
                && let Some(opener) = undo_opener(self.transcript.cells(), index, prefix)
            {
                self.transcript.truncate(opener);
            }
        } else {
            if let Some(turn) = &turn
                && let Some(Cell::User { opened, .. }) = self.transcript.cell_mut(turn.first_cell)
            {
                *opened = turn.requested;
            }
            if !outcome.captured.trim().is_empty() {
                self.transcript.push(Cell::notice(
                    NoticeKind::Routing,
                    render::plain_text(&outcome.captured).trim_end().to_string(),
                ));
            }
        }
        if let Some(error) = &outcome.error {
            // Errors arrive painted for the REPL like notices do.
            self.transcript.push(Cell::Error {
                text: render::plain_text(error),
                cache: LineCache::default(),
            });
        }
        self.schedule_frame();
    }

    pub(crate) fn begin_exit(&mut self) {
        // Called again (a signal while the exit work already runs) it
        // keeps the first call's start, or the exit's late output so far
        // would fall before it.
        if !self.exiting {
            self.exiting = true;
            self.transcript.clear_welcome();
            self.exit_first_cell = self.transcript.len();
            self.committed_end = self
                .turn
                .as_ref()
                .map_or(self.exit_first_cell, |turn| turn.first_cell);
            // The teardown owns the context now; nothing may act beside it.
            self.control = None;
            self.roster_sources = RosterSources::default();
        }
        self.schedule_frame();
    }

    /// A tool row still running when its turn ends never got its
    /// finish hook: the turn was interrupted or the task died under it.
    /// Job and agent rows are left alone; their work goes on in the
    /// background.
    fn fail_unreported_tools(&mut self, first_cell: usize) {
        let now = self.clock.now();
        for index in first_cell..self.transcript.len() {
            let running = matches!(
                self.transcript.cells()[index],
                Cell::Tool {
                    status: ToolStatus::Running { .. },
                    ..
                }
            );
            if running
                && let Some(Cell::Tool { status, .. }) = self.transcript.cell_mut(index)
                && let ToolStatus::Running { since } = *status
            {
                *status = ToolStatus::Failed {
                    elapsed: now.duration_since(since),
                    error: "interrupted".to_string(),
                };
            }
        }
    }

    pub(super) fn take_control_requests(&mut self) -> Vec<ControlRequest> {
        std::mem::take(&mut self.control_requests)
    }

    fn queue_control(&mut self, request: ControlRequest) {
        if self.control.is_none() {
            self.transcript.push_notice(
                self.notice_merge_from(),
                NoticeKind::ToolWarning,
                "Tasks cannot be reached while the session is ending".to_string(),
            );
            return;
        }
        self.control_requests.push(request);
    }

    pub(super) fn focused_task(&self) -> Option<TaskRow> {
        match &self.view {
            View::Roster(roster) => match roster.focused() {
                Some(RosterRow::Task(task)) => Some(task.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    pub(super) fn cancel_focused_task(&mut self) {
        if let Some(task) = self.focused_task() {
            self.queue_control(ControlRequest::Cancel {
                id: task.id,
                kind: task.kind,
                child_root: self.child_root.clone(),
            });
        }
    }

    /// Shows the highlighted task in the pager: an agent's own session,
    /// or the root thread from a job's row.
    pub(super) fn open_focused_task(&mut self) {
        let Some(task) = self.focused_task() else {
            return;
        };
        match task.kind {
            TaskKind::Agent => match task.session_id {
                Some(session_id) => self.open_child(session_id, task.name),
                None => self.transcript.push_notice(
                    self.notice_merge_from(),
                    NoticeKind::ToolWarning,
                    format!("Agent {} keeps no session to show", task.id),
                ),
            },
            TaskKind::Job => {
                let row = self.transcript.cells().iter().rposition(
                    |cell| matches!(cell, Cell::Job { job_id, .. } if *job_id == task.id),
                );
                match row {
                    Some(index) => self.open_pager(Pager::root_at(index)),
                    None => self.transcript.push_notice(
                        self.notice_merge_from(),
                        NoticeKind::ToolWarning,
                        format!("Job {} has no row in this transcript", task.id),
                    ),
                }
            }
        }
    }

    /// Opens a child's thread in the pager and asks for its first read.
    pub(super) fn open_child(&mut self, session_id: String, agent: String) {
        if self.control.is_none() {
            self.transcript.push_notice(
                self.notice_merge_from(),
                NoticeKind::ToolWarning,
                "Transcripts cannot be read while the session is ending".to_string(),
            );
            return;
        }
        if !is_plain_session_id(&session_id) {
            warn!("Refused to read a child session with the id {session_id:?}");
            self.transcript.push_notice(
                self.notice_merge_from(),
                NoticeKind::ToolWarning,
                format!(
                    "Cannot read the session of {agent}: its id {} is not a plain name",
                    render::plain_text(&session_id)
                ),
            );
            return;
        }
        let Some(root) = self.child_root.as_ref() else {
            self.transcript.push_notice(
                self.notice_merge_from(),
                NoticeKind::ToolWarning,
                format!("The session of {agent} has not been written yet"),
            );
            return;
        };
        let file = root.child_session_file(&session_id);
        if self.pager.as_ref().is_some_and(Pager::shows_root) {
            cells::release_full_output_caches(self.transcript.cells_for_layout().0);
        }
        self.open_pager(Pager::child(session_id.clone(), agent, file.clone()));
        self.read_open_child(session_id, file);
    }

    /// Asks for a read of the child on show. Nothing is read once the
    /// exit has begun; the pager keeps what it has.
    pub(super) fn read_open_child(&mut self, session_id: String, file: PathBuf) {
        let Some(pager) = self.pager.as_mut() else {
            return;
        };
        if self.control.is_none() {
            return;
        }
        self.child_reads += 1;
        pager.child_load_started(self.child_reads);
        self.control_requests.push(ControlRequest::LoadChild {
            session_id,
            file,
            generation: self.child_reads,
        });
    }

    pub(super) fn open_thread_picker(&mut self) {
        let mut rows = vec![ThreadRow::root()];
        let none_yet = || Some("No child sessions yet".to_string());
        let note = match self.child_root.as_deref() {
            None => none_yet(),
            Some(root) => match root.list_children() {
                Ok(children) if children.is_empty() => none_yet(),
                Ok(children) => {
                    let running: Vec<ProcessStamp> = children
                        .iter()
                        .filter(|meta| meta.status == ChildStatus::Running)
                        .map(ChildMeta::stamp)
                        .collect();
                    let alive = ProcessStamp::alive_set(&running);
                    rows.extend(children.iter().map(|meta| {
                        ThreadRow::child(
                            meta,
                            meta.effective_status_with(|stamp| alive.contains(stamp)),
                        )
                    }));
                    None
                }
                Err(err) => {
                    warn!("Failed to list the child sessions: {err:#}");
                    Some(format!("Cannot list the child sessions: {err:#}"))
                }
            },
        };
        if let Some(pager) = self.pager.as_mut() {
            pager.open_picker(rows, note);
        }
    }

    pub(super) fn choose_thread(&mut self) {
        let Some(row) = self.pager.as_mut().and_then(Pager::take_picked) else {
            return;
        };
        match row.session_id {
            Some(session_id) => self.open_child(session_id, row.agent),
            None => self.open_pager(Pager::root()),
        }
    }

    /// What the running turn printed past its capture, already taken in
    /// as notices. Nothing draws the app once the turn's task is lost,
    /// so a panic message that came this way is handed back to be printed
    /// on the restored terminal.
    pub(super) fn turn_late_output(&self) -> Vec<String> {
        let Some(turn) = &self.turn else {
            return Vec::new();
        };
        self.late_output_since(turn.first_cell)
    }

    /// The exit work's late output: every routing notice since `begin_exit`.
    pub(super) fn exit_late_output(&self) -> Vec<String> {
        self.late_output_since(self.exit_first_cell)
    }

    fn late_output_since(&self, first_cell: usize) -> Vec<String> {
        let cells = self.transcript.cells();
        cells[first_cell.min(cells.len())..]
            .iter()
            .filter_map(|cell| match cell {
                Cell::Notice {
                    kind: NoticeKind::Routing,
                    text,
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}
