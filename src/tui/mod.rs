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
                right: self.usage.clone(),
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
    right: String,
}

impl Widget for FooterBar {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        Paragraph::new(Line::from(Span::styled(self.left, DIM))).render(area, buf);
        if !self.right.is_empty() {
            Paragraph::new(Line::from(Span::styled(self.right, DIM)))
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
