//! The full-screen UI. One loop on the main task owns the terminal and the
//! `RequestContext` while idle; a submitted line moves the context into a
//! spawned work task that runs the same REPL command path the line editor
//! would, and everything the turn says comes back through `UiMsg`s. The
//! transcript is redrawn from its sources on every frame, so a resize is
//! just another frame. A `!cmd` passthrough or an editor borrows the
//! terminal: the loop leaves the screen and runs the job synchronously on
//! its own thread, so nothing is drawn until the job returns.
//! The roster (Alt+T) and the pager (Ctrl+T) act through a control context,
//! a fork of the context taken each time it comes back idle: it shares
//! the supervisor, the escalation queue and the child session root with
//! the context a turn has, so a task can be cancelled or a question
//! answered mid-turn. It never runs a turn.

mod bottom_pane;
mod cells;
mod child_live;
mod command_timing;
mod composer;
mod control;
mod dump;
mod events;
mod help;
mod history;
mod keys;
mod lend;
mod modal;
mod pager;
mod queued;
mod redirect;
mod render;
mod rewind;
mod roster;
mod run_loop;
mod run_now;
mod sink;
mod terminal;
mod theme;
mod throbber;
mod todo;
mod transcript;

use crate::config::child_sessions::ChildSessionRoot;
use crate::config::{AppConfig, RequestContext, jobs_enabled};
use crate::events::NoticeKind;
use crate::function::agents::CANCEL_CLEANUP_WAIT;
use crate::function::jobs::JOB_KILL_GRACE;
use crate::prompt::{PromptAction, PromptAnswer, PromptMemory};
use crate::repl::is_command;
use anyhow::Result;
use bottom_pane::{
    PaletteSnapshot, PaletteState, PaletteView, RewindTarget, RewindView, SearchState, SearchView,
    View,
};
use cells::{AgentStatus, Cell, JobStatus, LineCache, TaskRow, elapsed_text};
use child_live::{ChildLive, ChildThread, ToolEnd};
use command_timing::Timing;
use composer::Composer;
use control::{ControlRequest, cancel_report, is_plain_session_id};
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEventKind};
use events::{Clock, FrameScheduler, TermInput};
use help::Help;
use history::ComposerHistory;
use keys::{Action, KeyContext, Layer, OverlayKind, PagerMode, PopupKind, ViMode};
#[cfg(test)]
pub(crate) use lend::OwnerGuard;
use lend::{Owner, owner};
pub(crate) use lend::{TtyHandles, ensure_terminal_lendable, lend_terminal, notice, request_copy};
use modal::{PendingPrompt, PromptModal};
use pager::{Pager, PagerView};
use queued::{QueuedLine, QueuedPreview, remove_sent};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui_textarea::TextArea;
use render::Renderer;
pub(crate) use render::plain_text;
use rewind::opens_exchange;
use roster::{RosterPopup, RosterRow, RosterSources, RosterState};
pub use run_loop::{ensure_tty, run};
use sink::{AgentOutcome, LendJob, UiMsg};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use terminal::MOUSE_CAPTURE_DEFAULT;
use theme::{ChromeKind, DIM, Theme, chrome_style, field_background};
use throbber::{THROBBER_INTERVAL, Tick};
use todo::TodoPanel;
use tokio::task::JoinSet;
use transcript::{Transcript, WHEEL_LINES};

pub(crate) fn active() -> bool {
    matches!(owner(), Owner::Tui(_))
}

/// The transcript's width on the last frame that showed it; 0 before one.
static TABLE_WIDTH: AtomicU16 = AtomicU16::new(0);

/// The width a command's table must fit to show whole in the transcript,
/// while the full-screen UI has the terminal; None otherwise.
pub(crate) fn table_width() -> Option<u16> {
    let width = TABLE_WIDTH.load(Ordering::Relaxed);
    (width > 0 && active()).then_some(width)
}

/// How long the composer must have been untouched before a bare hotkey
/// counts as a modal answer, so a keystroke meant for the composer never
/// confirms a prompt that appeared under it.
const HOTKEY_IDLE: Duration = Duration::from_secs(1);
/// How long the exit work waits for the roster's and the pager's control
/// tasks before it takes the context; those still running are aborted.
/// A roster cancel of an agent waits up to `CANCEL_CLEANUP_WAIT` for the
/// agent to join and one of a job sits through up to two `JOB_KILL_GRACE`
/// joins, so this outlasts the longer rather than abort that wait.
const CONTROL_TASK_WAIT: Duration = longer(CANCEL_CLEANUP_WAIT, JOB_KILL_GRACE.saturating_mul(2))
    .saturating_add(Duration::from_secs(1));
const _: () = assert!(
    CONTROL_TASK_WAIT.as_nanos() > CANCEL_CLEANUP_WAIT.as_nanos()
        && CONTROL_TASK_WAIT.as_nanos() > JOB_KILL_GRACE.saturating_mul(2).as_nanos()
);

/// `Ord::max` for a const context.
const fn longer(a: Duration, b: Duration) -> Duration {
    if a.as_nanos() >= b.as_nanos() { a } else { b }
}

const TOOL_OUTPUT_TAIL: usize = 20;
/// Most bytes of one call's output kept for the pager; past it the
/// oldest go and the pager says how many.
const FULL_OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
/// Most bytes of whole output kept over all calls; past it the oldest
/// calls' whole output goes first.
const FULL_OUTPUT_BUDGET: usize = 32 * 1024 * 1024;
/// Most text one cell of late command output holds; a leftover child that
/// keeps writing sees its oldest lines dropped rather than the transcript grow.
const LATE_OUTPUT_LIMIT: usize = 64 * 1024;
/// Transcript rows a modal's question may not take from the frame, so the
/// narration being approved stays in view above it. A question that still
/// does not fit is clipped with a marker and cannot be approved until the
/// terminal is tall enough.
const TRANSCRIPT_MIN_ROWS: u16 = 4;
/// Rows of the frame drawn whatever else shows: the status row, the
/// footer and the rule over the status row. The rule yields its row only
/// on a frame with no transcript row left, so the transcript's minimum
/// is counted with it, and a question too tall for the rest is cut (and
/// waits for a taller terminal) rather than take that minimum.
const FIXED_ROWS: u16 = 3;
/// How long the status row waits between two ticks of its elapsed time.
const STATUS_TICK: Duration = Duration::from_secs(1);
/// The UI's own commands, spelled without their sigil: one opens the key
/// help, the other the thread picker, and neither reaches the REPL.
const KEYS_COMMAND: &str = "keys";
const THREADS_COMMAND: &str = "threads";
/// The status row between turns, naming the chords of the panels that
/// nothing else on screen points to.
const IDLE_STATUS: &str = "Alt+T = tasks  Ctrl+X = todo";
/// The status row when `?` would open the key help. The key that lists
/// the rest comes first, so a narrow frame clips the panel chords
/// before it.
const IDLE_STATUS_WITH_HELP: &str = "? = keys  Alt+T = tasks  Ctrl+X = todo";

/// The idle hint as key and label spans: each `key = label` entry's key
/// in one style, the rest in the other.
fn hint_spans(hint: &'static str, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, entry) in hint.split("  ").enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", theme.hint_label));
        }
        let (key, label) = entry.split_at(entry.find(" = ").unwrap_or(entry.len()));
        spans.push(Span::styled(key, theme.hint_key));
        if !label.is_empty() {
            spans.push(Span::styled(label, theme.hint_label));
        }
    }
    spans
}

/// Rows of the rule under the transcript on a frame `area_height` tall
/// whose bottom pane takes `bottom_rows`: none when drawing it would leave
/// the transcript no row, so a short frame gives the row to the bottom pane.
fn rule_rows(area_height: u16, bottom_rows: u16) -> u16 {
    u16::from(area_height > FIXED_ROWS + bottom_rows)
}

/// The footer's left half, read from the context only while the UI holds
/// it; the task counts after it are read live when it is drawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FooterInfo {
    who: Option<String>,
    model: String,
    session: Option<String>,
}

impl FooterInfo {
    pub(crate) fn from_ctx(ctx: &RequestContext) -> Self {
        let who = ctx
            .agent
            .as_ref()
            .map(|agent| agent.name().to_string())
            .or_else(|| ctx.role.as_ref().map(|role| role.name().to_string()));
        Self {
            who,
            model: ctx.model.id(),
            session: ctx.session.as_ref().map(|s| s.name().to_string()),
        }
    }

    fn parts(&self, (agents, jobs): (usize, usize)) -> Vec<String> {
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
        parts.push(format!("agents {agents}"));
        parts.push(format!("jobs {jobs}"));
        parts
    }

    /// The fields joined by ` . `, which is drawn in a style of its own.
    fn line(&self, counts: (usize, usize), theme: &Theme) -> Line<'static> {
        let mut spans = Vec::new();
        for (i, part) in self.parts(counts).into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" . ", theme.footer_separator));
            }
            spans.push(Span::styled(part, theme.footer_field));
        }
        Line::from(spans)
    }
}

/// The footer's right half: the mouse mode, then the usage line dimmed.
fn footer_right(mouse_capture: bool, usage: &str, theme: &Theme) -> Line<'static> {
    let mode = if mouse_capture {
        "mouse scroll"
    } else {
        "mouse select"
    };
    let mut spans = vec![Span::styled(mode, theme.footer_field)];
    let usage = render::styled_line(usage);
    if usage.width() > 0 {
        spans.push(Span::styled(" . ", theme.footer_separator));
        let base = usage.style.patch(DIM);
        spans.extend(usage.spans.into_iter().map(|mut span| {
            span.style = base.patch(span.style);
            span
        }));
    }
    Line::from(spans)
}

/// What is drawn over the transcript while open: Ctrl+T's full-screen
/// pager, or the key help or the tasks pop-up over the transcript's rows.
enum Overlay {
    Pager(Box<Pager>),
    Help(Help),
    Tasks(RosterState),
}

impl Overlay {
    fn kind(&self) -> OverlayKind {
        match self {
            Overlay::Pager(pager) => OverlayKind::Pager(pager.mode()),
            Overlay::Help(_) => OverlayKind::Help,
            Overlay::Tasks(_) => OverlayKind::Tasks,
        }
    }

    fn scroll(&mut self, delta: i32) {
        match self {
            Overlay::Pager(pager) => pager.scroll(delta),
            Overlay::Help(help) => help.scroll(delta),
            // A wheel notch steps one row, as a key does, not a scroll's lines.
            Overlay::Tasks(roster) => roster.move_highlight(delta.signum()),
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
    /// Whether a request reached the model: the first sign of a reply, or
    /// the busy indicator and stream end that the transports raise only
    /// once the request is on the wire. The session records an exchange
    /// for every such request, reply or not.
    requested: bool,
}

/// Something the loop, not the app, has to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Submit(String),
    /// A prompt typed during a turn; the loop offers it to the running
    /// turn's model.
    Queue(String),
    /// A command typed during a turn that runs at once on the context
    /// the turn started with, without waiting for the turn. A setting
    /// among them reaches the live context when the turn ends.
    RunNow(String),
    /// A held line taken back into the composer: the loop withdraws the
    /// prompt from the running turn and offers the draft it displaced,
    /// each `None` when it was a command the turn never saw.
    Unqueue {
        held: Option<String>,
        draft: Option<String>,
    },
    Interrupt,
    /// Cancel the command still running from a turn that has ended.
    Cancel,
    Quit,
    Suspend,
    OpenEditor,
    Redraw,
    /// Switch the terminal's mouse reports on or off.
    SetMouseCapture(bool),
    /// Put a row's output on the clipboard; the notice of how it went
    /// names `from` and the `dropped` bytes the row no longer held.
    Copy {
        text: String,
        from: String,
        dropped: usize,
    },
    /// Take the chosen exchanges out of the session and the transcript
    /// and give the oldest one's prompt back to the composer.
    Rewind(RewindTarget),
}

/// The kind of question an escalated `question` asks, from the `[kind] `
/// an agent's user tool puts in front of it, and the question without it.
fn escalation_kind(question: &str) -> (Option<PromptAction>, &str) {
    [
        PromptAction::Confirm,
        PromptAction::Select,
        PromptAction::Checkbox,
        PromptAction::Input,
    ]
    .into_iter()
    .find_map(|action| {
        question
            .strip_prefix('[')
            .and_then(|rest| rest.strip_prefix(action.name()))
            .and_then(|rest| rest.strip_prefix("] "))
            .map(|rest| (Some(action), rest))
    })
    .unwrap_or((None, question))
}

fn new_textarea() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_cursor_line_style(Style::new());
    textarea
}

/// A question's field, wrapping its text as the composer does; a
/// password's shows a `*` per character.
fn new_field(masked: bool) -> TextArea<'static> {
    let mut field = new_textarea();
    field.set_wrap_mode(ratatui_textarea::WrapMode::WordOrGlyph);
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
    /// Text waiting for the loop to copy; see [`UiMsg::Copy`].
    copy_requests: Vec<String>,
    turn: Option<TurnStatus>,
    /// The teardown task owns the context: nothing new can be submitted,
    /// interrupted or quit until it reports back.
    exiting: bool,
    /// Index of the first cell the exit work may add; what came after it
    /// is the exit's late output.
    exit_first_cell: usize,
    /// End of the cells printed on the way out, fixed when the exit
    /// began: a turn cut short then is left out with the exit's cells.
    committed_end: usize,
    footer: FooterInfo,
    usage: String,
    /// Whether the terminal sends mouse reports: the wheel and clicks
    /// reach the UI, and the terminal's own text selection does not work.
    mouse_capture: bool,
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
    /// The rows the front prompt's origin received in the last frame,
    /// None until it has been drawn.
    origin_area: Option<Rect>,
    clock: Arc<dyn Clock>,
    /// What the throbber counts its frames from, fixed at start so the
    /// frame a given instant shows depends on the clock alone.
    throbber_origin: Instant,
    frames: FrameScheduler,
    renderer: Renderer,
    theme: Theme,
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
    overlay: Option<Overlay>,
    /// The generation of the newest child read asked for.
    child_reads: u64,
    roster_sources: RosterSources,
    /// Each spawned child's tool calls and graph nodes as they happen.
    child_live: ChildLive,
    /// A fork of the context as it last came back idle, for what the
    /// roster and the pager do while a turn has the context: cancelling
    /// a task, replying to an agent's question and reading a child's
    /// session. The fork shares the supervisor, the escalation queue and
    /// the child session root with the context the turn holds, so those
    /// act on the turn's live tasks; it never runs a turn of its own.
    /// None before the first sync and once the exit has begun.
    control: Option<Arc<RequestContext>>,
    /// Where child sessions live: the context's handle as it last came
    /// back, or the one a child's first write named during the turn
    /// that spawned it.
    child_root: Option<Arc<ChildSessionRoot>>,
    /// Work the roster and the pager asked for; the loop runs each off
    /// itself after the step that queued it.
    control_requests: Vec<ControlRequest>,
    /// The control requests still running, which the exit work waits on.
    control_tasks: JoinSet<()>,
    /// Whether a command typed during a turn is still running beside it
    /// or after it; Esc cancels it. The loop keeps it current.
    command_running: bool,
    /// Escalations put to the user through the prompt modal, by id, with
    /// the kind of modal asked and where its answer arrives.
    escalation_answers: Vec<(String, PromptAction, mpsc::Receiver<PromptAnswer>)>,
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
        let theme = Theme::new(renderer.markdown_render(), renderer.no_color);
        let history_file = history_file(&config, footer.session.as_deref());
        Ok(Self {
            transcript: Transcript::new(footer.model.clone(), config.command_prefix),
            todo: TodoPanel::default(),
            composer: Composer::new(background, config.command_prefix),
            queued: Vec::new(),
            prompts: VecDeque::new(),
            lend_requests: Vec::new(),
            copy_requests: Vec::new(),
            turn: None,
            exiting: false,
            exit_first_cell: 0,
            committed_end: 0,
            footer,
            usage: String::new(),
            mouse_capture: MOUSE_CAPTURE_DEFAULT,
            last_keystroke: None,
            front_since: None,
            frame_area: None,
            question_area: None,
            origin_area: None,
            throbber_origin: clock.now(),
            frames: FrameScheduler::new(Arc::clone(&clock)),
            clock,
            renderer,
            theme,
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
            overlay: None,
            roster_sources: RosterSources::default(),
            child_live: ChildLive::default(),
            control: None,
            child_root: None,
            control_requests: Vec::new(),
            control_tasks: JoinSet::new(),
            command_running: false,
            child_reads: 0,
            escalation_answers: Vec::new(),
        })
    }

    pub(crate) fn schedule_frame(&mut self) {
        self.frames.schedule_frame();
    }

    fn tick(&self) -> Tick {
        Tick {
            origin: self.throbber_origin,
            now: self.clock.now(),
        }
    }

    #[cfg(test)]
    fn set_no_color(&mut self, no_color: bool) {
        self.renderer.no_color = no_color;
        self.theme = Theme::new(self.renderer.markdown_render(), no_color);
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
                    self.theme = Theme::new(renderer.markdown_render(), renderer.no_color);
                    self.renderer = renderer;
                    self.transcript.forget_layout();
                    self.child_live.forget_layout();
                    for prompt in &mut self.prompts {
                        prompt.forget_question_layout();
                    }
                    if let Some(pager) = self.pager_mut() {
                        pager.forget_layout();
                    }
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
        // Read again on every return, never kept from the start: `.agent`
        // swaps the supervisor and drops the queue. The queue is made here
        // rather than by the first spawn, so the one a turn hands to its
        // children is the one the roster reads.
        ctx.ensure_root_escalation_queue();
        // The same goes for a supervisor `job__start` would otherwise make
        // mid-turn, out of the footer's and the roster's sight.
        if jobs_enabled(ctx.agent.as_ref(), &ctx.app.config) {
            ctx.ensure_supervisor();
        }
        self.roster_sources = RosterSources {
            supervisor: ctx.supervisor.clone(),
            escalations: ctx.escalation_queue.clone(),
        };
        self.settle_vanished_tasks();
        self.child_root = ctx.child_session_root.clone();
        self.control = Some(Arc::new(ctx.fork_for_branch()));
        self.schedule_frame();
    }

    fn close_stream(&mut self) {
        if let Some(Cell::Assistant { done, .. }) = self.transcript.close_stream() {
            *done = true;
        }
    }

    fn turn_first_cell(&self) -> usize {
        self.turn.as_ref().map_or(0, |turn| turn.first_cell)
    }

    fn notice_merge_from(&self) -> usize {
        // The exit's output never folds into a cell from before it, so it
        // stays readable as the exit's own.
        if self.exiting {
            self.exit_first_cell
        } else {
            0
        }
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
        // short for the layout squeezes the question below its cap, or
        // drops the origin row that says a tool is asking.
        let question_fully_shown = match (self.prompts.front(), self.frame_area) {
            (Some(prompt), Some(area)) => {
                prompt.question_fits_cap(area.width, area.height)
                    && self
                        .question_area
                        .is_none_or(|given| prompt.question_fits_rows(given.width, given.height))
                    && self
                        .origin_area
                        .is_none_or(|given| given.height >= prompt.origin_rows())
            }
            _ => true,
        };
        KeyContext {
            turn_running: self.turn.is_some() || self.exiting,
            command_running: self.command_running,
            composer_empty: self.composer.is_empty(),
            single_line: self.composer.lines().len() <= 1,
            modal: self.prompts.front().map(PendingPrompt::kind),
            idle_ok,
            field_settled,
            question_fully_shown,
            scrolled: self.transcript.scrolled(),
            focused: self.transcript.focused(),
            popup: self.popup(),
            vi: self.vi_mode,
            overlay: self.overlay.as_ref().map(Overlay::kind),
            child_thread: self.pager().is_some_and(|pager| !pager.shows_root()),
        }
    }

    fn popup(&self) -> Option<PopupKind> {
        match self.view {
            View::Composer => None,
            View::Palette(_) => Some(PopupKind::Palette),
            View::HistorySearch(_) => Some(PopupKind::HistorySearch),
            View::Rewind(_) => Some(PopupKind::Rewind),
        }
    }

    fn layer(&self) -> Layer {
        keys::layer(
            self.overlay.as_ref().map(Overlay::kind),
            self.prompts.front().map(PendingPrompt::kind),
            self.popup(),
        )
    }

    fn pager(&self) -> Option<&Pager> {
        match &self.overlay {
            Some(Overlay::Pager(pager)) => Some(pager),
            _ => None,
        }
    }

    fn pager_mut(&mut self) -> Option<&mut Pager> {
        match &mut self.overlay {
            Some(Overlay::Pager(pager)) => Some(pager),
            _ => None,
        }
    }

    fn help_mut(&mut self) -> Option<&mut Help> {
        match &mut self.overlay {
            Some(Overlay::Help(help)) => Some(help),
            _ => None,
        }
    }

    fn roster(&self) -> Option<&RosterState> {
        match &self.overlay {
            Some(Overlay::Tasks(roster)) => Some(roster),
            _ => None,
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
        let line = self.clear_sent_draft()?;
        self.record_history(&line);
        Some(line)
    }

    /// Empties the composer the way sending a line does and returns the
    /// trimmed draft; None when there is nothing but whitespace.
    fn clear_sent_draft(&mut self) -> Option<String> {
        let text = self.composer_text();
        if text.trim().is_empty() {
            return None;
        }
        self.composer.clear();
        self.palette_dirty = true;
        self.transcript.clear_focus();
        // The next line is typed, so the vi layer starts it in insert.
        self.vi_mode = self.vi_mode.map(|_| ViMode::Insert);
        Some(text.trim().to_string())
    }

    fn submit_composer(&mut self) -> Option<Command> {
        if self.take_ui_command() {
            return None;
        }
        self.take_composer_line().map(Command::Submit)
    }

    fn queue_composer(&mut self) -> Option<Command> {
        if self.exiting {
            return None;
        }
        if self.take_ui_command() {
            return None;
        }
        // `.keys` or `.threads` gets this far only when a macro of that
        // name shadows it, and the macro waits like any other.
        let now = matches!(
            command_timing::lookup(&self.composer_text(), self.config.command_prefix),
            Some((name, Timing::Now | Timing::Deferred))
                if !matches!(name.strip_prefix('.'), Some(KEYS_COMMAND | THREADS_COMMAND))
        );
        if now {
            return self.take_composer_line().map(Command::RunNow);
        }
        self.hold_composer_line().map(Command::Queue)
    }

    /// Takes a draft that is the `.keys` or `.threads` command and nothing
    /// else out of the composer, as any sent line is, and opens the key
    /// help or the thread picker in place of sending it. The plain REPL
    /// shares the history file and knows neither, so the line is kept in
    /// this session's history only. An enabled macro of the same name
    /// wins: the line is left for the REPL, which runs the macro.
    fn take_ui_command(&mut self) -> bool {
        let text = self.composer_text();
        let command = self.config.command_prefix.normalize(text.trim());
        let Some(name) = command
            .as_deref()
            .and_then(|command| command.strip_prefix('.'))
            .and_then(|name| {
                [KEYS_COMMAND, THREADS_COMMAND]
                    .into_iter()
                    .find(|ui| *ui == name)
            })
        else {
            return false;
        };
        if self.macros.iter().any(|(macro_name, _)| macro_name == name) {
            return false;
        }
        if let Some(line) = self.clear_sent_draft() {
            self.history.push(&line);
        }
        if name == KEYS_COMMAND {
            self.overlay = Some(Overlay::Help(Help::default()));
        } else {
            self.open_threads();
        }
        true
    }

    /// Holds the draft for the running turn. A prompt is returned for the
    /// loop, which offers it to the model; a command only waits, and
    /// `.keys` and `.threads` open their view without waiting.
    fn hold_composer_line(&mut self) -> Option<String> {
        if self.take_ui_command() {
            return None;
        }
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
            self.transcript.push_notice(
                self.notice_merge_from(),
                NoticeKind::Interrupted,
                format!("Dropped {dropped} queued line(s)"),
            );
        }
    }

    /// The target row's whole output for the loop to put on the
    /// clipboard, or a notice saying why there is none.
    fn copy_tool_output(&mut self) -> Option<Command> {
        let copied = cells::copy_target(self.transcript.cells(), self.transcript.focus())
            .and_then(Cell::copied_output);
        let notice = match copied {
            None => "No tool call to copy: focus one with Ctrl+Up or a click".to_string(),
            Some(copied) if copied.text.is_empty() => {
                format!("Nothing to copy from {}", copied.from)
            }
            Some(copied) => {
                return Some(Command::Copy {
                    text: copied.text,
                    from: copied.from,
                    dropped: copied.dropped,
                });
            }
        };
        self.transcript
            .push_notice(self.notice_merge_from(), NoticeKind::ToolWarning, notice);
        None
    }

    /// Takes back an Alt+M the terminal never got, so the footer shows
    /// the mode the terminal is still in.
    pub(crate) fn mouse_capture_failed(&mut self, prev: bool, err: &std::io::Error) {
        self.mouse_capture = prev;
        self.transcript.push_notice(
            self.notice_merge_from(),
            NoticeKind::ToolWarning,
            format!("Could not switch the mouse mode: {err}"),
        );
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
                match self.layer() {
                    // Only the find field takes it; the widgets under the
                    // pager are out of sight.
                    Layer::Overlay(OverlayKind::Pager(_)) => {
                        if let Some(pager) = self.pager_mut() {
                            pager.paste(&text);
                        }
                    }
                    // The help and the tasks pop-up have the keyboard, so a
                    // paste reaches nothing under them, as a typed key would not.
                    Layer::Overlay(OverlayKind::Help | OverlayKind::Tasks) => return None,
                    // A confirm has no field, and the composer is hidden
                    // under the modal: a paste there would land where the
                    // user cannot see it.
                    Layer::Modal(_) => {
                        if let Some(field) = self
                            .prompts
                            .front_mut()
                            .and_then(|prompt| prompt.field.as_mut())
                        {
                            field.insert_str(text.replace('\n', " "));
                        }
                    }
                    Layer::Band(_) | Layer::Composer => {
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
                match self.layer() {
                    Layer::Overlay(OverlayKind::Pager(PagerMode::Browse))
                        if mouse.kind == MouseEventKind::Down(MouseButton::Left) =>
                    {
                        if let (Some(Overlay::Pager(pager)), Some(area)) =
                            (self.overlay.as_mut(), self.frame_area)
                        {
                            pager.click(&mut self.child_live, mouse.column, mouse.row, area);
                        }
                    }
                    Layer::Overlay(_) => {
                        let delta = match mouse.kind {
                            MouseEventKind::ScrollUp => -WHEEL_LINES,
                            MouseEventKind::ScrollDown => WHEEL_LINES,
                            _ => return None,
                        };
                        if let Some(overlay) = self.overlay.as_mut() {
                            overlay.scroll(delta);
                        }
                    }
                    Layer::Modal(_) | Layer::Band(_) | Layer::Composer => match mouse.kind {
                        MouseEventKind::ScrollUp => self.transcript.scroll_lines(-WHEEL_LINES),
                        MouseEventKind::ScrollDown => self.transcript.scroll_lines(WHEEL_LINES),
                        MouseEventKind::Down(MouseButton::Left) => {
                            self.transcript.click(mouse.column, mouse.row)
                        }
                        _ => return None,
                    },
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
            Action::DeleteToLineHead => {
                if let Some(field) = self.prompts.front_mut().and_then(|p| p.field.as_mut()) {
                    self.last_keystroke = Some(self.clock.now());
                    field.delete_line_by_head();
                } else {
                    self.composer.delete_line_by_head();
                    self.palette_dirty = true;
                }
                None
            }
            Action::Undo => {
                if let Some(field) = self.prompts.front_mut().and_then(|p| p.field.as_mut()) {
                    self.last_keystroke = Some(self.clock.now());
                    field.undo();
                } else {
                    self.composer.undo();
                    self.palette_dirty = true;
                }
                None
            }
            Action::Interrupt => {
                self.transcript.clear_focus();
                self.drop_queued();
                (!self.exiting).then_some(Command::Interrupt)
            }
            Action::CancelCommand => (!self.exiting).then_some(Command::Cancel),
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
            Action::ToggleMouse => {
                self.mouse_capture = !self.mouse_capture;
                Some(Command::SetMouseCapture(self.mouse_capture))
            }
            Action::CopyToolOutput => self.copy_tool_output(),
            Action::Answer(answer) => {
                self.answer_front(answer);
                None
            }
            Action::AnswerYesForSession => {
                if let Some(prompt) = self.prompts.front() {
                    self.prompt_memory.remember(&prompt.req);
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
                        // A closed select with no options has no row to
                        // land on and no custom row to fall back to: the
                        // caller asked an unanswerable question.
                        None if prompt.req.closed => self.answer_front(PromptAnswer::Cancelled),
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
            Action::ToggleRoster => {
                self.overlay = match self.overlay {
                    Some(Overlay::Tasks(_)) => None,
                    _ => Some(Overlay::Tasks(RosterState::new(self.roster_sources.rows()))),
                };
                None
            }
            Action::ToggleHelp => {
                self.overlay = match self.overlay {
                    Some(Overlay::Help(_)) => None,
                    _ => Some(Overlay::Help(Help::default())),
                };
                None
            }
            Action::HelpScroll(delta) => {
                if let Some(help) = self.help_mut() {
                    help.scroll(delta);
                }
                None
            }
            Action::HelpPage(direction) => {
                if let Some(help) = self.help_mut() {
                    help.page(direction);
                }
                None
            }
            Action::RosterCancel => {
                self.cancel_focused_task();
                None
            }
            Action::RosterOpen => {
                self.open_focused_task();
                None
            }
            Action::OpenThreads => {
                self.open_threads();
                None
            }
            Action::RosterMove(delta) => {
                if let Some(Overlay::Tasks(roster)) = self.overlay.as_mut() {
                    roster.move_highlight(delta);
                }
                None
            }
            Action::RosterAnswer => {
                if let Some(RosterRow::Escalation(ask)) =
                    self.roster().and_then(RosterState::focused).cloned()
                {
                    self.ask_escalation(ask);
                }
                None
            }
            Action::OpenPager => {
                self.open_pager(Pager::root());
                None
            }
            Action::ClosePager => {
                self.close_pager();
                None
            }
            Action::OpenThreadPicker => {
                self.open_thread_picker();
                None
            }
            Action::ThreadPickerChoose => {
                self.choose_thread();
                None
            }
            Action::PagerScroll(delta) => self.with_pager(|pager| pager.scroll(delta)),
            Action::PagerPage(direction) => self.with_pager(|pager| pager.page(direction)),
            Action::PagerTop => self.with_pager(Pager::jump_to_top),
            Action::PagerBottom => self.with_pager(Pager::jump_to_bottom),
            Action::PagerFindOpen => self.with_pager(Pager::open_find),
            Action::PagerFindChar(c) => self.with_pager(|pager| pager.find_char(c)),
            Action::PagerFindBackspace => self.with_pager(Pager::find_backspace),
            Action::PagerFindNext { forward } => self.with_pager(|pager| pager.find_next(forward)),
            Action::PagerFindSubmit => self.with_pager(Pager::submit_find),
            Action::PagerFindCancel => self.with_pager(Pager::cancel_find),
            Action::ThreadPickerMove(delta) => self.with_pager(|pager| pager.move_picker(delta)),
            Action::CloseThreadPicker => self.with_pager(Pager::close_picker),
            Action::PagerFocusPrev => {
                if let Some(Overlay::Pager(pager)) = self.overlay.as_mut() {
                    pager.step_focus(&self.child_live, false);
                }
                None
            }
            Action::PagerFocusNext => {
                if let Some(Overlay::Pager(pager)) = self.overlay.as_mut() {
                    pager.step_focus(&self.child_live, true);
                }
                None
            }
            Action::PagerToggleFocused => {
                if let Some(Overlay::Pager(pager)) = self.overlay.as_mut() {
                    pager.toggle_focused(&mut self.child_live);
                }
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
        self.settle_escalation_answers();
        self.schedule_frame();
        command
    }

    fn with_pager(&mut self, f: impl FnOnce(&mut Pager)) -> Option<Command> {
        if let Some(pager) = self.pager_mut() {
            f(pager);
        }
        None
    }

    /// Settles the running job and agent rows of tasks the supervisor no
    /// longer tracks. Only called with the context in hand, when no turn
    /// can be registering a task its row already shows.
    fn settle_vanished_tasks(&mut self) {
        let tracked = self.roster_sources.task_ids();
        self.transcript
            .settle_vanished(&|id: &str| tracked.contains(id));
    }

    /// Forgets the live thread of every finished child the roster no
    /// longer lists, so collected children free what they held.
    fn prune_child_threads(&mut self) {
        let listed = self.roster_sources.task_ids();
        self.child_live.retain_listed(|id| listed.contains(id));
    }

    /// The live rows of child `agent_id` changed: its thread, if on show,
    /// is laid out again at the next frame.
    fn child_rows_changed(&mut self, agent_id: &str) {
        let Some(Overlay::Pager(pager)) = self.overlay.as_mut() else {
            return;
        };
        if pager
            .child_session()
            .is_some_and(|session_id| self.child_live.session_is_of(session_id, agent_id))
        {
            pager.child_rows_changed();
        }
    }

    /// Readies the running rows of the child thread on show, if any, for
    /// the throbber's next frame; whether it has one.
    fn touch_open_child_running(&mut self) -> bool {
        let Some(Overlay::Pager(pager)) = self.overlay.as_mut() else {
            return false;
        };
        let running = pager
            .child_session()
            .and_then(|session_id| self.child_live.by_session_mut(session_id))
            .is_some_and(ChildThread::forget_running_lines);
        if running {
            pager.child_rows_changed();
        }
        running
    }

    /// Closes the pager and frees the lines it laid out; a pager opened
    /// from the tasks pop-up gives the pop-up back.
    fn close_pager(&mut self) {
        let Some(pager) = self.pager_mut() else {
            return;
        };
        self.overlay = pager.take_tasks().map(Overlay::Tasks);
        cells::release_full_output_caches(self.transcript.cells_for_layout().0);
        self.child_live.release_full_output_caches();
        // Rows that ran under a child's thread hold the throbber frame
        // they were last drawn with.
        self.transcript.touch_running();
    }

    /// Takes the overlay's place. Over the tasks pop-up, or in place of a
    /// pager opened from it, the pager keeps the pop-up to give back.
    fn open_pager(&mut self, mut pager: Pager) {
        pager.set_tasks(match self.overlay.take() {
            Some(Overlay::Tasks(roster)) => Some(roster),
            Some(Overlay::Pager(mut previous)) => {
                // A child's thread giving way to another thread, or to
                // the root, frees what its rows laid out.
                if !previous.shows_root() {
                    self.child_live.release_full_output_caches();
                }
                previous.take_tasks()
            }
            Some(Overlay::Help(_)) | None => None,
        });
        self.overlay = Some(Overlay::Pager(Box::new(pager)));
    }

    /// The pager on the root thread with the thread picker open over it.
    fn open_threads(&mut self) {
        self.open_pager(Pager::root());
        self.open_thread_picker();
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
            UiMsg::TextDelta(delta) => self.transcript.stream_delta(&delta),
            UiMsg::StreamDone => self.close_stream(),
            UiMsg::AssistantText(text) => self.transcript.finish_stream(text),
            UiMsg::Notice { kind, text } => {
                self.transcript
                    .push_notice(self.notice_merge_from(), kind, text);
            }
            UiMsg::ToolNarration { call_id, line } => {
                // The todo panel shows what a todo call did; its row
                // would only repeat it.
                if !cells::narrated_tool_name(&line).starts_with("todo__") {
                    self.transcript
                        .push(Cell::tool(call_id, line, self.clock.now()));
                }
            }
            UiMsg::ToolOutput { call_id, bytes } => self.transcript.append_tool_output(
                self.turn_first_cell(),
                self.turn.is_some(),
                call_id,
                &bytes,
            ),
            UiMsg::ToolStarted {
                call_id,
                name,
                args,
            } => self
                .transcript
                .tool_started(self.turn_first_cell(), call_id, name, args),
            UiMsg::ToolFinished {
                call_id,
                ok,
                error,
                duration_ms,
            } => {
                let now = self.clock.now();
                self.transcript.tool_finished(
                    self.turn_first_cell(),
                    now,
                    &call_id,
                    ok,
                    error,
                    duration_ms,
                );
                self.dismiss_prompts(|prompt| prompt.asked_by_call(&call_id));
            }
            UiMsg::ChildToolStarted {
                agent_id,
                node,
                call_id,
                name,
                args,
            } => {
                let now = self.clock.now();
                self.child_live
                    .tool_started(&agent_id, node, call_id, name, args, now);
                self.child_rows_changed(&agent_id);
            }
            UiMsg::ChildToolFinished {
                agent_id,
                node,
                call_id,
                name,
                args,
                ok,
                error,
                duration_ms,
            } => {
                let now = self.clock.now();
                let end = ToolEnd {
                    name,
                    args,
                    ok,
                    error,
                    duration_ms,
                };
                self.child_live
                    .tool_finished(&agent_id, node.as_deref(), &call_id, end, now);
                self.child_rows_changed(&agent_id);
            }
            UiMsg::ChildToolOutput {
                agent_id,
                node,
                call_id,
                chunk,
            } => {
                self.child_live
                    .tool_output(&agent_id, node.as_deref(), &call_id, &chunk);
                self.child_rows_changed(&agent_id);
            }
            UiMsg::ChildNode {
                agent_id,
                node_id,
                node_type,
                state,
            } => {
                let now = self.clock.now();
                self.child_live
                    .node(&agent_id, node_id, node_type, state, now);
                self.child_rows_changed(&agent_id);
            }
            UiMsg::JobStarted { job_id, tool } => self.transcript.start_row(
                self.turn_first_cell(),
                Cell::Job {
                    job_id,
                    tool,
                    row: TaskRow::new(JobStatus::Running {
                        since: self.clock.now(),
                    }),
                    cache: LineCache::default(),
                },
                "job__start",
            ),
            UiMsg::JobFinished { job_id, ok, error } => {
                self.transcript.set_job_status(
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
                self.transcript
                    .set_job_status(&job_id, JobStatus::Cancelled);
                self.dismiss_prompts(|prompt| prompt.asked_by_job(&job_id));
            }
            UiMsg::AgentStarted {
                agent_id,
                name,
                session_id,
            } => {
                self.prune_child_threads();
                self.child_live
                    .agent_started(agent_id.clone(), name.clone(), session_id);
                self.child_rows_changed(&agent_id);
                self.transcript.start_row(
                    self.turn_first_cell(),
                    Cell::Agent {
                        agent_id,
                        name,
                        row: TaskRow::new(AgentStatus::Running {
                            since: self.clock.now(),
                        }),
                        cache: LineCache::default(),
                    },
                    "agent__spawn",
                );
            }
            UiMsg::AgentFinished { agent_id, outcome } => {
                self.child_live
                    .agent_finished(&agent_id, &outcome, self.clock.now());
                let status = match outcome {
                    AgentOutcome::Completed => AgentStatus::Completed,
                    AgentOutcome::Failed(error) => AgentStatus::Failed(error),
                    AgentOutcome::Interrupted => AgentStatus::Interrupted,
                };
                self.transcript.set_agent_status(&agent_id, status);
                // Before the prune, which may forget the child and the
                // session that leads to it.
                self.child_rows_changed(&agent_id);
                self.prune_child_threads();
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
            UiMsg::Copy(text) => self.copy_requests.push(text),
            UiMsg::ChildTranscriptFlushed { session_id } => {
                if !is_plain_session_id(&session_id) {
                    warn!("Ignored a flush of a child session with the id {session_id:?}");
                    self.schedule_frame();
                    return;
                }
                let reload = self
                    .pager_mut()
                    .and_then(|pager| pager.child_flushed(&session_id));
                if let Some(file) = reload {
                    self.read_open_child(session_id, file);
                }
            }
            UiMsg::ChildSessionRootResolved(root) => {
                // By value: a sessionless fork resolves its own Arc of the
                // same process-stamp root the App may already hold.
                let changed = self.child_root.as_deref() != Some(&*root);
                self.child_root = Some(root);
                if changed
                    && self
                        .pager()
                        .is_some_and(|pager| pager.mode() == PagerMode::Picker)
                {
                    self.open_thread_picker();
                }
            }
            UiMsg::ChildLoaded {
                session_id,
                generation,
                result,
            } => {
                if let Err(err) = &result {
                    warn!("Failed to read the session of child {session_id}: {err}");
                }
                let again = self
                    .pager_mut()
                    .and_then(|pager| pager.set_child_entries(&session_id, generation, result));
                if let Some(file) = again {
                    self.read_open_child(session_id, file);
                }
            }
            UiMsg::TaskCancelled { id, result } => {
                let (kind, text) = cancel_report(&id, &result);
                self.transcript
                    .push_notice(self.notice_merge_from(), kind, text);
            }
            UiMsg::CommandRan { line, result } => self.push_command_result(&line, result),
        }
        self.schedule_frame();
    }

    fn take_lend_requests(&mut self) -> Vec<LendJob> {
        std::mem::take(&mut self.lend_requests)
    }

    fn take_copy_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.copy_requests)
    }

    /// The cells of the turns that ended: none of the exit's own, nor of
    /// a turn whose task was lost under it.
    fn committed_cells(&self) -> &[Cell] {
        let cells = self.transcript.cells();
        let end = if self.exiting {
            self.committed_end
        } else {
            self.turn
                .as_ref()
                .map_or(cells.len(), |turn| turn.first_cell)
        };
        &cells[..end.min(cells.len())]
    }

    /// Every turn state is dim and the idle hint is italic instead; the
    /// throbber in front of a turn state is neither, and no hint has its
    /// glyphs. The terminal backend only rewrites cells whose symbol or
    /// style changed, so a shared style would let a turn's first frame
    /// skip the letters it has in common with the hint and leave a torn
    /// word in the output stream. Modifiers survive `NO_COLOR`, so this
    /// holds there too.
    fn status_text(&self) -> Option<Line<'static>> {
        if self.exiting {
            return Some(Line::from(Span::styled(
                "Leaving...",
                self.theme.status_runtime,
            )));
        }
        let Some(turn) = self.turn.as_ref() else {
            // The key help and the tasks pop-up take the chords the idle
            // hint names.
            if !self.prompts.is_empty() || self.covers_transcript() {
                return None;
            }
            let status = if self.key_context().question_mark_opens_help() {
                IDLE_STATUS_WITH_HELP
            } else {
                IDLE_STATUS
            };
            return Some(Line::from(hint_spans(status, &self.theme)));
        };
        let tick = self.tick();
        let elapsed = |since: Instant| elapsed_text(tick.elapsed(since));
        let state = if let Some(tool) = &turn.tool {
            format!(" Working . {} . {tool}", elapsed(turn.started))
        } else if let Some(since) = turn.thinking_since {
            format!(" Thinking for {}", elapsed(since))
        } else {
            format!(
                " {} . {}",
                turn.busy.as_deref().unwrap_or("Working"),
                elapsed(turn.started)
            )
        };
        let mut spans = vec![
            Span::styled(tick.frame(), self.theme.throbber),
            Span::styled(state, self.theme.status_runtime),
        ];
        if self.prompts.is_empty() && !self.covers_transcript() {
            spans.push(Span::styled("  Esc to interrupt", self.theme.status_key));
        }
        Some(Line::from(spans))
    }

    /// Whether the key help or the tasks pop-up is open over the
    /// transcript, taking Esc and the panel chords.
    fn covers_transcript(&self) -> bool {
        matches!(self.overlay, Some(Overlay::Help(_) | Overlay::Tasks(_)))
    }

    fn bottom_pane_rows(&self, area: Rect) -> u16 {
        match self.prompts.front() {
            Some(prompt) => prompt.rows(area.width, area.height),
            None => self.composer.rows(area.width) + self.band_rows(area),
        }
    }

    /// The rows the band above the composer takes from the transcript:
    /// what the view asks for, or the queued-lines preview when no view
    /// is open, within what the frame can give once the composer, the
    /// fixed rows and the transcript's minimum have theirs.
    fn band_rows(&self, area: Rect) -> u16 {
        let room = area
            .height
            .saturating_sub(self.composer.rows(area.width) + FIXED_ROWS + TRANSCRIPT_MIN_ROWS);
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
        self.renderer.tick = self.tick();
        // Matched on the field, not through `layer`, so the pager can be
        // borrowed beside the transcript and the renderer.
        if let Some(Overlay::Pager(pager)) = self.overlay.as_mut() {
            let generation = self.transcript.generation();
            let (cells, revisions) = self.transcript.cells_for_layout();
            let text = pager.text(
                cells,
                revisions,
                generation,
                &mut self.child_live,
                area.width,
                &mut self.renderer,
            );
            pager.settle(&text, Pager::text_rows(area.height));
            frame.render_widget(PagerView { pager, text: &text }, area);
            return;
        }
        // The roster is read on every frame it is up: the tasks move on
        // whether or not the context is in hand.
        if let Some(Overlay::Tasks(roster)) = self.overlay.as_mut() {
            roster.refresh(self.roster_sources.rows());
        }
        if let Some(prompt) = self.prompts.front_mut() {
            prompt.layout_question(area.width, &mut self.renderer);
        }
        let bottom_rows = self.bottom_pane_rows(area);
        let [upper, rule, status, bottom, footer] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(rule_rows(area.height, bottom_rows)),
            Constraint::Length(1),
            Constraint::Length(bottom_rows),
            Constraint::Length(1),
        ])
        .areas(area);

        // The todo panel shares the transcript's rows: beside it on a
        // wide frame, in its place on a narrow one.
        let (transcript, todo) = match (self.todo.visible(), TodoPanel::width_beside(area.width)) {
            (false, _) => (upper, None),
            (true, Some(width)) => {
                let [transcript, todo] =
                    Layout::horizontal([Constraint::Min(0), Constraint::Length(width)])
                        .areas(upper);
                (transcript, Some(todo))
            }
            (true, None) => (Rect::ZERO, Some(upper)),
        };
        if transcript.width > 0 {
            TABLE_WIDTH.store(transcript.width, Ordering::Relaxed);
        }
        let view = self.transcript.view(transcript, &mut self.renderer);
        frame.render_widget(view, transcript);
        if let Some(todo) = todo {
            frame.render_widget(self.todo.view(&self.theme), todo);
        }
        match self.overlay.as_mut() {
            Some(Overlay::Help(help)) => frame.render_widget(help.view(&self.theme), upper),
            Some(Overlay::Tasks(roster)) => frame.render_widget(
                RosterPopup {
                    state: roster,
                    theme: &self.theme,
                    no_color: self.renderer.no_color,
                    tick: self.renderer.tick,
                },
                upper,
            ),
            Some(Overlay::Pager(_)) | None => {}
        }

        // A line between the transcript and the status row, so the
        // transcript's last line never reads as part of the hint under it.
        frame.render_widget(
            Block::new()
                .borders(Borders::TOP)
                .border_style(self.theme.rule),
            rule,
        );
        frame.render_widget(StatusStrip(self.status_text()), status);

        let modal_areas = match self.prompts.front() {
            Some(prompt) => {
                let [_, origin, question, ..] = prompt.areas(bottom.intersection(area), area);
                frame.render_widget(
                    PromptModal {
                        prompt,
                        theme: &self.theme,
                    },
                    bottom,
                );
                Some((origin, question))
            }
            None => {
                let [band, field] = Layout::vertical([
                    Constraint::Length(self.band_rows(area)),
                    Constraint::Min(0),
                ])
                .areas(bottom);
                match &self.view {
                    View::Composer => frame.render_widget(
                        QueuedPreview {
                            queued: &self.queued,
                            theme: &self.theme,
                        },
                        band,
                    ),
                    View::Palette(palette) => frame.render_widget(
                        PaletteView {
                            state: palette,
                            theme: &self.theme,
                        },
                        band,
                    ),
                    View::HistorySearch(search) => frame.render_widget(SearchView(search), band),
                    View::Rewind(rewind) => frame.render_widget(RewindView(rewind), band),
                }
                let macro_names: Vec<&str> =
                    self.macros.iter().map(|(name, _)| name.as_str()).collect();
                frame.render_widget(
                    self.composer.view(
                        self.config.command_prefix,
                        &macro_names,
                        &self.theme,
                        self.vi_mode == Some(ViMode::Normal),
                    ),
                    field,
                );
                None
            }
        };
        self.origin_area = modal_areas.map(|(origin, _)| origin);
        self.question_area = modal_areas.map(|(_, question)| question);

        frame.render_widget(
            FooterBar {
                left: self
                    .footer
                    .line(self.roster_sources.running_counts(), &self.theme),
                right: footer_right(self.mouse_capture, &self.usage, &self.theme),
            },
            footer,
        );
    }

    /// Records the draw and asks for the next tick here, after the
    /// scheduler has forgotten the frame that was just drawn. While
    /// anything animates (a turn, a running row of the transcript while
    /// no child's thread covers it or of the child's thread on show, a
    /// running task in the tasks pop-up) the throbber's next frame is
    /// due; the running rows are counted as changed so that frame draws
    /// them anew. Otherwise an open tasks pop-up asks for its next read,
    /// and the footer counts tick while any task runs, unless the pager
    /// covers them: an agent reports finished before its join handle
    /// does, and nested spawns send no message. Nothing can spawn while
    /// the counts are zero and no turn runs, so idle stops there.
    pub(crate) fn frame_drawn(&mut self) {
        self.frames.frame_drawn();
        let transcript_shown = self.pager().is_none_or(Pager::shows_root);
        let rows_run = transcript_shown && self.transcript.touch_running();
        let child_runs = self.touch_open_child_running();
        let tasks_run = self.roster_sources.running_counts() != (0, 0);
        let roster = matches!(self.overlay, Some(Overlay::Tasks(_)));
        if self.turn.is_some() || rows_run || child_runs || (roster && tasks_run) {
            self.frames.schedule_frame_in(THROBBER_INTERVAL);
        } else if roster || (self.pager().is_none() && tasks_run) {
            self.frames.schedule_frame_in(STATUS_TICK);
        }
    }
}

struct StatusStrip(Option<Line<'static>>);

impl Widget for StatusStrip {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        if let Some(text) = self.0 {
            Paragraph::new(text).render(area, buf);
        }
    }
}

struct FooterBar {
    left: Line<'static>,
    right: Line<'static>,
}

impl Widget for FooterBar {
    /// The right half is laid out first, so the left half is the one cut
    /// short; within the right half the mouse mode comes first, so a long
    /// usage line loses its end before the mode loses any of it.
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let right_width = u16::try_from(self.right.width())
            .unwrap_or(u16::MAX)
            .min(area.width);
        let left_width = area.width.saturating_sub(right_width.saturating_add(1));
        Paragraph::new(self.left).render(
            Rect {
                width: left_width,
                ..area
            },
            buf,
        );
        Paragraph::new(self.right).render(
            Rect {
                x: area.right() - right_width,
                width: right_width,
                ..area
            },
            buf,
        );
    }
}

#[cfg(test)]
mod tests;
