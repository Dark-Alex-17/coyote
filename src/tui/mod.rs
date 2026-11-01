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
use crate::repl::{EXIT_CANCEL_WAIT, EXIT_HOOK_DRAIN_TIMEOUT, parse_command, run_repl_command};
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
use render::{RenderCache, Renderer};
use sink::{TuiPrompter, TuiSink, UiMsg};
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
/// restores the terminal. Coyote itself never sets it.
const PANIC_AT_START_ENV: &str = "COYOTE_TUI_PANIC_AT_START";

/// How long the composer must have been untouched before a bare hotkey
/// counts as a modal answer, so a keystroke meant for the composer never
/// confirms a prompt that appeared under it.
const HOTKEY_IDLE: Duration = Duration::from_secs(1);
const TOOL_OUTPUT_TAIL: usize = 20;
const COMPOSER_MIN_ROWS: u16 = 3;
const COMPOSER_MAX_ROWS: u16 = 8;
const CONFIRM_HINT: &str =
    "y = yes  n = no  a = yes for this session (tool prompts only)  Esc = cancel";
const INPUT_HINT: &str = "Enter = answer  Esc = cancel";

const DIM: Style = Style::new().add_modifier(Modifier::DIM);

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
            text,
        });
    }
}

/// Runs one line the way the REPL would, on its own task, with fds 1 and 2
/// captured. The context comes back with the outcome.
pub(crate) fn spawn_turn(
    mut ctx: RequestContext,
    abort: AbortSignal,
    line: String,
    ui: UnboundedSender<UiMsg>,
) -> JoinHandle<(RequestContext, TurnOutcome)> {
    tokio::spawn(async move {
        let guard = match FdRedirect::begin(late_output(ui)) {
            Ok(guard) => Some(guard),
            Err(err) => {
                warn!("Running the turn without output capture: {err}");
                None
            }
        };
        let result = Box::pin(run_repl_command(&mut ctx, abort, &line)).await;
        let captured = guard.map(FdRedirect::finish).unwrap_or_default();
        let outcome = match result {
            Ok(exit) => TurnOutcome {
                exit,
                error: None,
                captured,
            },
            Err(err) => TurnOutcome {
                exit: false,
                error: Some(format!("{err:?}")),
                captured,
            },
        };
        (ctx, outcome)
    })
}

/// The REPL's teardown on the work task: settles background agents, exits
/// the session (which may ask to save it) and closes the agent bracket.
fn spawn_exit(
    mut ctx: RequestContext,
    abort: AbortSignal,
) -> JoinHandle<(RequestContext, Result<()>, String)> {
    tokio::spawn(async move {
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
                let style = match kind {
                    NoticeKind::ToolWarning => Style::new().fg(Color::Yellow),
                    NoticeKind::Routing => Style::new(),
                    _ => DIM,
                };
                cache.get(width, || render::plain_lines(text, width, style))
            }
            Cell::Error { text, cache } => cache.get(width, || {
                render::plain_lines(text, width, Style::new().fg(Color::Red))
            }),
            Cell::Command {
                name,
                output,
                lines,
            } => lines
                .get_or_insert_with(|| {
                    let mut lines = vec![Line::from(Span::styled(format!("> {name}"), DIM))];
                    lines.extend(render::preformatted_lines(output));
                    lines
                })
                .clone(),
            Cell::Text { lines, .. } => render::preformatted_lines(&lines.join("\n"))
                .into_iter()
                .map(|line| line.patch_style(DIM))
                .collect(),
        }
    }
}

struct TurnStatus {
    started: Instant,
    busy: Option<String>,
    thinking_since: Option<Instant>,
    tool: Option<String>,
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

/// Everything drawn, and the rules for changing it.
pub(crate) struct App {
    cells: Vec<Cell>,
    composer: TextArea<'static>,
    prompts: VecDeque<PendingPrompt>,
    turn: Option<TurnStatus>,
    footer: FooterInfo,
    usage: String,
    last_keystroke: Option<Instant>,
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
        let mut composer = new_textarea();
        composer.set_placeholder_text("Type a prompt or a .command; Enter sends");
        composer.set_placeholder_style(DIM);
        Ok(Self {
            cells: Vec::new(),
            composer,
            prompts: VecDeque::new(),
            turn: None,
            footer,
            usage: String::new(),
            last_keystroke: None,
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
        composer.set_placeholder_text(self.composer.placeholder_text().to_string());
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

    /// Records that a line left for the work task.
    pub(crate) fn begin_turn(&mut self, line: &str, command: bool) {
        if !command {
            self.cells.push(Cell::user(line));
        }
        self.turn = Some(TurnStatus {
            started: self.clock.now(),
            busy: None,
            thinking_since: None,
            tool: None,
        });
        self.schedule_frame();
    }

    /// Commits what the work task brought back.
    pub(crate) fn end_turn(&mut self, line: &str, command: bool, outcome: &TurnOutcome) {
        self.turn = None;
        self.close_stream();
        if command {
            self.cells.push(Cell::Command {
                name: line.to_string(),
                output: outcome.captured.clone(),
                lines: None,
            });
        } else if !outcome.captured.trim().is_empty() {
            self.cells.push(Cell::notice(
                NoticeKind::Routing,
                outcome.captured.trim_end().to_string(),
            ));
        }
        if let Some(error) = &outcome.error {
            self.cells.push(Cell::Error {
                text: error.clone(),
                cache: LineCache::default(),
            });
        }
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
        KeyContext {
            turn_running: self.turn.is_some(),
            composer_empty: self.composer.is_empty(),
            modal: self.prompts.front().map(PendingPrompt::kind),
            idle_ok,
        }
    }

    pub(crate) fn handle_input(&mut self, input: TermInput) -> Option<Command> {
        match input {
            TermInput::Key(key) => self.handle_key(key),
            TermInput::Paste(text) => {
                self.last_keystroke = Some(self.clock.now());
                match self.prompts.front_mut().and_then(|p| p.field.as_mut()) {
                    Some(field) => field.insert_str(text.replace('\n', " ")),
                    None => self.composer.insert_str(text),
                };
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
        let command = match action {
            Action::Submit => {
                let text = self.composer_text();
                if text.trim().is_empty() {
                    None
                } else {
                    self.composer = new_textarea();
                    self.composer
                        .set_placeholder_text("Type a prompt or a .command; Enter sends");
                    self.composer.set_placeholder_style(DIM);
                    Some(Command::Submit(text.trim().to_string()))
                }
            }
            Action::InsertNewline => {
                self.last_keystroke = Some(self.clock.now());
                self.composer.insert_newline();
                None
            }
            Action::ClearComposer => {
                self.set_composer_text("");
                None
            }
            Action::Interrupt => Some(Command::Interrupt),
            Action::Quit => Some(Command::Quit),
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
                self.last_keystroke = Some(self.clock.now());
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
                Some(Cell::Assistant { done, .. }) if !*done => *done = true,
                _ => self.cells.push(Cell::assistant(text, true)),
            },
            UiMsg::Notice { kind, text } => self.cells.push(Cell::notice(kind, text)),
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
                        "Prompt kind {} is not supported in the TUI yet",
                        req.action.name()
                    ),
                ));
                let _ = reply.send(PromptAnswer::Cancelled);
                return;
            }
        };
        self.prompts.push_back(PendingPrompt { req, reply, field });
    }

    fn append_tool_output(&mut self, call_id: Option<String>, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let index = self
            .cells
            .iter()
            .rposition(|cell| matches!(cell, Cell::Text { call_id: id, .. } if *id == call_id));
        let index = match index {
            Some(index) => index,
            None => {
                self.cells.push(Cell::Text {
                    call_id,
                    lines: Vec::new(),
                    open_line: false,
                });
                self.cells.len() - 1
            }
        };
        let Cell::Text {
            lines, open_line, ..
        } = &mut self.cells[index]
        else {
            return;
        };
        for (i, piece) in text.split('\n').enumerate() {
            let last_is_open = i == text.split('\n').count() - 1;
            if i == 0 && *open_line {
                if let Some(last) = lines.last_mut() {
                    last.push_str(piece);
                }
            } else if !(last_is_open && piece.is_empty()) {
                lines.push(piece.to_string());
            }
            *open_line = last_is_open && !piece.is_empty();
        }
        if lines.len() > TOOL_OUTPUT_TAIL {
            lines.drain(..lines.len() - TOOL_OUTPUT_TAIL);
        }
    }

    /// Answers every queued prompt `Cancelled`; used when the UI is about
    /// to go away without a human to ask.
    fn cancel_prompts(&mut self) {
        while let Some(prompt) = self.prompts.pop_front() {
            let _ = prompt.reply.send(PromptAnswer::Cancelled);
        }
    }

    fn status_text(&self) -> Option<String> {
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
        Some(format!("{state}  Esc to interrupt"))
    }

    fn bottom_pane_rows(&self) -> u16 {
        match self.prompts.front() {
            Some(_) => 3,
            None => u16::try_from(self.composer.lines().len())
                .unwrap_or(COMPOSER_MAX_ROWS)
                .clamp(COMPOSER_MIN_ROWS, COMPOSER_MAX_ROWS),
        }
    }

    /// One frame from state; `frame.area()` is the only size consulted.
    pub(crate) fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let [transcript, status, bottom, footer] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(self.bottom_pane_rows()),
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

        match self.prompts.front() {
            Some(prompt) => frame.render_widget(PromptModal(prompt), bottom),
            None => frame.render_widget(&self.composer, bottom),
        }

        frame.render_widget(
            FooterBar {
                left: self.footer.text(),
                right: render::styled_line(&self.usage),
            },
            footer,
        );

        if self.turn.is_some() {
            self.frames.schedule_frame_in(Duration::from_secs(1));
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

/// A prompt drawn where the composer was: question, origin, hint.
struct PromptModal<'a>(&'a PendingPrompt);

impl Widget for PromptModal<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let [question, middle, hint] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let req = &self.0.req;
        Paragraph::new(Line::from(Span::styled(
            req.question.clone(),
            Style::new().add_modifier(Modifier::BOLD),
        )))
        .render(question, buf);
        match &self.0.field {
            Some(field) => field.render(middle, buf),
            None => {
                if let Some(tool) = &req.tool {
                    Paragraph::new(Line::from(Span::styled(format!("tool: {tool}"), DIM)))
                        .render(middle, buf);
                }
            }
        }
        let hint_text = if self.0.field.is_some() {
            INPUT_HINT
        } else {
            CONFIRM_HINT
        };
        Paragraph::new(Line::from(Span::styled(hint_text, DIM))).render(hint, buf);
    }
}

enum Task {
    Turn {
        handle: JoinHandle<(RequestContext, TurnOutcome)>,
        line: String,
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
}

#[cfg(unix)]
impl Termination {
    fn arm() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.hup.recv() => {}
        }
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
    /// prompt, so the teardown runs after the screen is restored.
    Abandoned,
    /// A work task panicked and took the context with it.
    Lost(tokio::task::JoinError),
}

fn editor_command() -> String {
    std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .filter(|editor| !editor.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string())
}

/// Edits `text` in `$VISUAL`/`$EDITOR` through a temp file.
fn edit_in_editor(text: &str) -> std::io::Result<String> {
    let path = std::env::temp_dir().join(format!("coyote-compose-{}.md", std::process::id()));
    std::fs::write(&path, text)?;
    let editor = editor_command();
    let mut parts = shell_words::split(&editor)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))?;
    if parts.is_empty() {
        parts.push("vi".to_string());
    }
    let status = std::process::Command::new(&parts[0])
        .args(&parts[1..])
        .arg(&path)
        .status();
    let edited = std::fs::read_to_string(&path);
    let _ = std::fs::remove_file(&path);
    status?;
    Ok(edited?.trim_end_matches('\n').to_string())
}

/// Reads and drops whatever the terminal queued while the editor ran.
async fn flush_typeahead(stream: &mut EventStream) {
    while tokio::time::timeout(Duration::ZERO, stream.next())
        .await
        .is_ok_and(|event| event.is_some())
    {}
}

pub async fn run(mut ctx: RequestContext) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("No TTY for TUI")
    }
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
    if std::env::var(PANIC_AT_START_ENV).as_deref() == Ok("1") {
        panic!("{PANIC_AT_START_ENV} requested a panic after entering the alternate screen");
    }
    let mut termination = Termination::arm()?;
    let mut stream = EventStream::new();
    app.schedule_frame();

    let (leave, ctx) = run_loop(
        &mut app,
        &mut screen,
        &mut stream,
        &mut termination,
        rx,
        tx,
        ctx,
        &abort,
    )
    .await;

    if let Err(err) = screen.leave() {
        warn!("Failed to restore the terminal: {err}");
    }
    let result = match leave {
        Leave::Finished(result, captured) => {
            print!("{captured}");
            result
        }
        Leave::Abandoned => {
            auto_cancel.store(true, Ordering::SeqCst);
            match ctx {
                Some(ctx) => match spawn_exit(ctx, abort.clone()).await {
                    Ok((_, result, captured)) => {
                        print!("{captured}");
                        result
                    }
                    Err(err) => Err(err.into()),
                },
                None => Ok(()),
            }
        }
        Leave::Lost(err) => Err(err.into()),
    };
    if let Err(err) = &result {
        eprintln!(
            "{}",
            crate::utils::error_text(&crate::utils::pretty_error(err))
        );
    }
    crate::hooks::drain_pending(EXIT_HOOK_DRAIN_TIMEOUT).await;
    result.map(|_| ())
}

#[allow(clippy::too_many_arguments)]
async fn run_loop(
    app: &mut App,
    screen: &mut Screen,
    stream: &mut EventStream,
    termination: &mut Termination,
    mut rx: UnboundedReceiver<UiMsg>,
    tx: UnboundedSender<UiMsg>,
    ctx: RequestContext,
    abort: &AbortSignal,
) -> (Leave, Option<RequestContext>) {
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
                if let Some(Task::Turn { handle, .. }) = task.take() {
                    abort.set_ctrlc();
                    if let Ok(Ok((returned, _))) =
                        tokio::time::timeout(EXIT_CANCEL_WAIT, handle).await
                    {
                        ctx = Some(returned);
                    }
                }
                app.cancel_prompts();
                return (Leave::Abandoned, ctx);
            }
            Step::Input(Some(Ok(event))) => {
                let command = app.handle_input(events::classify(event));
                match command {
                    Some(Command::Submit(line)) => {
                        if let Some(ctx) = ctx.take() {
                            abort.reset();
                            let command =
                                parse_command(&line, ctx.app.config.command_prefix).is_some();
                            app.begin_turn(&line, command);
                            task = Some(Task::Turn {
                                handle: spawn_turn(ctx, abort.clone(), line.clone(), tx.clone()),
                                line,
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
                            if let Err(err) = released {
                                warn!("Failed to re-enter the alternate screen: {err}");
                            }
                            app.schedule_frame();
                        }
                    }
                    Some(Command::OpenEditor) => {
                        *stream = EventStream::new();
                        let text = app.composer_text();
                        match screen.with_terminal_released(|| edit_in_editor(&text)) {
                            Ok(Ok(edited)) => app.set_composer_text(&edited),
                            Ok(Err(err)) => app.handle_msg(UiMsg::Notice {
                                kind: NoticeKind::ToolWarning,
                                text: format!("Editor failed: {err}"),
                            }),
                            Err(err) => warn!("Failed to re-enter the alternate screen: {err}"),
                        }
                        flush_typeahead(stream).await;
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
                app.frames.frame_drawn();
            }
            Step::Work(done) => match *done {
                Done::Turn(joined) => {
                    let Some(Task::Turn { line, command, .. }) = task.take() else {
                        continue;
                    };
                    match joined {
                        Ok((mut returned, outcome)) => {
                            app.end_turn(&line, command, &outcome);
                            app.sync_from_ctx(&mut returned);
                            if outcome.exit {
                                quit_requested = true;
                            }
                            ctx = Some(returned);
                        }
                        Err(err) => {
                            app.cancel_prompts();
                            return (Leave::Lost(err), None);
                        }
                    }
                }
                Done::Exit(joined) => {
                    app.cancel_prompts();
                    return match joined {
                        Ok((returned, result, captured)) => {
                            (Leave::Finished(result, captured), Some(returned))
                        }
                        Err(err) => (Leave::Lost(err), None),
                    };
                }
            },
        }
        if quit_requested
            && task.is_none()
            && let Some(ctx) = ctx.take()
        {
            task = Some(Task::Exit(spawn_exit(ctx, abort.clone())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ClientConfig, Model};
    use crate::config::{AppState, Session, SessionScope, WorkingMode};
    use crate::events::Origin;
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
                "hello",
                false,
                &TurnOutcome {
                    captured: String::new(),
                    ..outcome
                },
            );
            harness.app.sync_from_ctx(&mut returned);
            let final_frame = harness.draw();
            insta::assert_snapshot!("stream_final", final_frame.clone());
            assert_ne!(first_delta, final_frame);
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
            harness.app.end_turn(".info", true, &outcome);
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
            ".info",
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

    /// A tool sleeping on the work task never stalls the composer: keys
    /// and a resize are handled and drawn while the tool is still running.
    #[cfg(unix)]
    #[test]
    #[serial]
    fn the_composer_stays_responsive_while_a_tool_sleeps() {
        run_async(async {
            use crate::function::{ToolIo, run_llm_function};
            use std::collections::HashMap;
            use std::fs;
            use std::os::unix::fs::PermissionsExt;

            let _guard = TestConfigDirGuard::new("tui-sleep");
            let bin = crate::config::paths::functions_bin_dir();
            fs::create_dir_all(&bin).unwrap();
            let tool = bin.join("sleep_x7x");
            fs::write(&tool, "#!/usr/bin/env bash\nsleep 2\n").unwrap();
            fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();

            let (tx, _rx) = unbounded_channel();
            let mut harness = Harness::standalone(80, 24);
            harness.app.begin_turn("run the tool", false);
            let started = Instant::now();
            let handle = tokio::task::spawn_blocking(move || {
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

            harness.type_text("hello");
            harness.draw();
            harness.resize(60, 20);
            let rendered = harness.draw();
            let typed_at = started.elapsed();
            assert!(!handle.is_finished(), "the tool finished in {typed_at:?}");
            assert!(typed_at < Duration::from_secs(2), "{typed_at:?}");
            assert_eq!(harness.app.composer_text(), "hello");
            insta::assert_snapshot!("sleeping_tool_composer_responsive", rendered);

            handle.await.unwrap().unwrap();
            assert!(started.elapsed() >= Duration::from_secs(2));
        });
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
        harness
            .app
            .end_turn("explain", false, &TurnOutcome::default());
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
}
