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
mod sink;
mod terminal;
mod todo;
mod transcript;

use crate::config::child_sessions::ChildSessionRoot;
use crate::config::{AppConfig, RequestContext, jobs_enabled};
use crate::events::{EventSink, NoticeKind, SubscriptionId};
use crate::function::agents::CANCEL_CLEANUP_WAIT;
use crate::function::jobs::JOB_KILL_GRACE;
use crate::prompt::{PromptAction, PromptAnswer, PromptMemory};
use crate::repl::{EXIT_CANCEL_WAIT, EXIT_HOOK_DRAIN_TIMEOUT, is_command};
use crate::supervisor::notification::NotificationQueue;
use crate::utils::{AbortSignal, create_abort_signal};
use anyhow::{Result, bail};
use bottom_pane::{
    PaletteSnapshot, PaletteState, PaletteView, RewindTarget, RewindView, SearchState, SearchView,
    View,
};
use cells::{
    AgentStatus, Cell, FoldedCall, FullOutput, JobStatus, LineCache, OutputTarget, ToolStatus,
    append_output,
};
use composer::{Composer, field_background};
use control::{
    ControlRequest, TurnOutcome, cancel_report, is_plain_session_id, spawn_control_request,
    spawn_exit, spawn_turn,
};
use crossterm::event::{EventStream, KeyCode, KeyEvent, MouseButton, MouseEventKind};
use events::{Clock, FrameScheduler, SystemClock, TermInput};
use futures_util::StreamExt;
use help::Help;
use history::ComposerHistory;
use keys::{Action, KeyContext, PagerMode, PopupKind, ViMode};
pub(crate) use lend::{TtyHandles, ensure_terminal_lendable, lend_terminal, notice};
use lend::{remember_tty, set_lender};
use modal::{PendingPrompt, PromptModal};
use pager::{Pager, PagerView};
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
pub(crate) use render::plain_text;
use render::{RenderCache, Renderer};
use rewind::{apply_rewind, opens_exchange};
use roster::{RosterRow, RosterSources, RosterState, RosterView};
use serde_json::Value;
use sink::{AgentOutcome, LendJob, TuiPrompter, TuiSink, UiMsg, unattended_answer};
use std::collections::VecDeque;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use terminal::Screen;
use todo::TodoPanel;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::{JoinHandle, JoinSet};
use transcript::{Transcript, WHEEL_LINES};

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

/// How long the UI waits on the exit work. The work itself waits
/// `CONTROL_TASK_WAIT` on control tasks and `EXIT_CANCEL_WAIT` on
/// background tasks before it saves the session and runs the finished
/// hook, so the wait on it has to outlast both or a task that ignores
/// cancellation would cost the user the save.
const EXIT_TEARDOWN_WAIT: Duration = CONTROL_TASK_WAIT
    .saturating_add(EXIT_CANCEL_WAIT)
    .saturating_add(EXIT_HOOK_DRAIN_TIMEOUT);
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
/// How long the status row waits between two ticks of its elapsed time.
const STATUS_TICK: Duration = Duration::from_secs(1);
/// How long `flush_typeahead` waits for one more queued event, and how
/// many it drops at most, so a terminal that keeps talking cannot hold the
/// loop.
const TYPEAHEAD_GRACE: Duration = Duration::from_millis(5);
const TYPEAHEAD_LIMIT: usize = 50;
/// The UI's own command, spelled without its sigil: it opens the key help
/// and never reaches the REPL.
const KEYS_COMMAND: &str = "keys";
/// The status row between turns, naming the chords of the panels that
/// nothing else on screen points to.
const IDLE_STATUS: &str = "Alt+T = tasks  Ctrl+X = todo";
/// The status row when `?` would open the key help. The key that lists
/// the rest comes first, so a narrow frame clips the panel chords
/// before it.
const IDLE_STATUS_WITH_HELP: &str = "? = keys  Alt+T = tasks  Ctrl+X = todo";

const DIM: Style = Style::new().add_modifier(Modifier::DIM);
const IDLE_HINT: Style = DIM.add_modifier(Modifier::ITALIC);
/// The completion popup in the REPL's colours: command names green as its
/// highlighter paints a known command, descriptions yellow and the
/// selected row green, bold and reversed as its completion menu draws them.
const PALETTE_NAME: Style = Style::new().fg(Color::Green);
const PALETTE_KIND: Style = Style::new().fg(Color::Cyan);
const PALETTE_DESCRIPTION: Style = Style::new().fg(Color::Yellow);
const PALETTE_SELECTED: Style = Style::new()
    .fg(Color::Green)
    .add_modifier(Modifier::BOLD)
    .add_modifier(Modifier::REVERSED);

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

    fn text(&self, (agents, jobs): (usize, usize)) -> String {
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
    /// End of the cells printed on the way out, fixed when the exit
    /// began: a turn cut short then is left out with the exit's cells.
    committed_end: usize,
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
    /// The rows the front prompt's origin received in the last frame,
    /// None until it has been drawn.
    origin_area: Option<Rect>,
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
    /// Ctrl+T's full-screen view, drawn over everything while open.
    pager: Option<Pager>,
    /// The key help, drawn over everything while open.
    help: Option<Help>,
    /// The generation of the newest child read asked for.
    child_reads: u64,
    roster_sources: RosterSources,
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
            committed_end: 0,
            footer,
            usage: String::new(),
            last_keystroke: None,
            front_since: None,
            frame_area: None,
            question_area: None,
            origin_area: None,
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
            pager: None,
            help: None,
            roster_sources: RosterSources::default(),
            control: None,
            child_root: None,
            control_requests: Vec::new(),
            control_tasks: JoinSet::new(),
            child_reads: 0,
            escalation_answers: Vec::new(),
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
                    self.transcript.forget_layout();
                    if let Some(pager) = self.pager.as_mut() {
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
                View::Roster(_) => Some(PopupKind::Roster),
            },
            vi: self.vi_mode,
            pager: self.pager.as_ref().map(Pager::mode),
            help: self.help.is_some(),
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
        if self.take_keys_command() {
            return None;
        }
        self.take_composer_line().map(Command::Submit)
    }

    fn queue_composer(&mut self) -> Option<Command> {
        if self.exiting {
            return None;
        }
        self.hold_composer_line().map(Command::Queue)
    }

    /// Takes a draft that is the `.keys` command and nothing else out of
    /// the composer, as any sent line is, and opens the key help in
    /// place of sending it. The plain REPL shares the history file and
    /// does not know `.keys`, so the line is kept in this session's
    /// history only. An enabled macro named `keys` wins: the line is
    /// left for the REPL, which runs the macro.
    fn take_keys_command(&mut self) -> bool {
        let text = self.composer_text();
        let command = self.config.command_prefix.normalize(text.trim());
        if command
            .as_deref()
            .and_then(|command| command.strip_prefix('.'))
            != Some(KEYS_COMMAND)
            || self.macros.iter().any(|(name, _)| name == KEYS_COMMAND)
        {
            return false;
        }
        if let Some(line) = self.clear_sent_draft() {
            self.history.push(&line);
        }
        self.help = Some(Help::default());
        true
    }

    /// Holds the draft for the running turn. A prompt is returned for the
    /// loop, which offers it to the model; a command only waits, and
    /// `.keys` opens the help without waiting.
    fn hold_composer_line(&mut self) -> Option<String> {
        if self.take_keys_command() {
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
            self.push_notice(
                NoticeKind::Interrupted,
                format!("Dropped {dropped} queued line(s)"),
            );
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
            View::Roster(roster) => {
                if let Some(RosterRow::Escalation(ask)) = roster.focused().cloned() {
                    self.ask_escalation(ask);
                }
                None
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
                if let Some(pager) = self.pager.as_mut() {
                    // Only the find field takes it; the widgets under the
                    // pager are out of sight.
                    pager.paste(&text);
                    self.schedule_frame();
                    return None;
                }
                // The help has the keyboard, so a paste reaches nothing
                // under it, as a typed key would not.
                if self.help.is_some() {
                    return None;
                }
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
                if let Some(pager) = self.pager.as_mut() {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => pager.scroll(-WHEEL_LINES),
                        MouseEventKind::ScrollDown => pager.scroll(WHEEL_LINES),
                        _ => return None,
                    }
                    self.schedule_frame();
                    return None;
                }
                if let Some(help) = self.help.as_mut() {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => help.scroll(-WHEEL_LINES),
                        MouseEventKind::ScrollDown => help.scroll(WHEEL_LINES),
                        _ => return None,
                    }
                    self.schedule_frame();
                    return None;
                }
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
                    View::Roster(roster) => roster.move_highlight(delta),
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
                self.view = match self.view {
                    View::Roster(_) => View::Composer,
                    _ => View::Roster(RosterState::new(self.roster_sources.rows())),
                };
                None
            }
            Action::ToggleHelp => {
                self.help = match self.help {
                    Some(_) => None,
                    None => Some(Help::default()),
                };
                None
            }
            Action::HelpScroll(delta) => {
                if let Some(help) = self.help.as_mut() {
                    help.scroll(delta);
                }
                None
            }
            Action::HelpPage(direction) => {
                if let Some(help) = self.help.as_mut() {
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
        if let Some(pager) = self.pager.as_mut() {
            f(pager);
        }
        None
    }

    /// Closes the pager and frees the lines it laid out.
    fn close_pager(&mut self) {
        if self.pager.take().is_some() {
            cells::release_full_output_caches(self.transcript.cells_for_layout().0);
        }
    }

    /// The pager covers the whole screen, key help pop-up included, and
    /// its keys come first, so the help never stays open under it.
    fn open_pager(&mut self, pager: Pager) {
        self.help = None;
        self.pager = Some(pager);
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
                    cache: LineCache::default(),
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
                    cache: LineCache::default(),
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
            UiMsg::ChildTranscriptFlushed { session_id } => {
                if !is_plain_session_id(&session_id) {
                    warn!("Ignored a flush of a child session with the id {session_id:?}");
                    self.schedule_frame();
                    return;
                }
                let reload = self
                    .pager
                    .as_mut()
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
                        .pager
                        .as_ref()
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
                    .pager
                    .as_mut()
                    .and_then(|pager| pager.set_child_entries(&session_id, generation, result));
                if let Some(file) = again {
                    self.read_open_child(session_id, file);
                }
            }
            UiMsg::TaskCancelled { id, result } => {
                let (kind, text) = cancel_report(&id, &result);
                self.push_notice(kind, text);
            }
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
        let Some(Cell::Tool { status, full, .. }) = self.transcript.cell_mut(index) else {
            return;
        };
        let grew = full.settle();
        if let ToolStatus::Running { since } = *status {
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
        self.transcript
            .account_full_output(grew, FULL_OUTPUT_BUDGET);
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
                    full,
                    ..
                } = self.transcript.replace(index, row)
                else {
                    unreachable!("the index was found holding a tool row");
                };
                let moved = full.text().len();
                if let Some(Cell::Job { calls, .. } | Cell::Agent { calls, .. }) =
                    self.transcript.cell_mut(index)
                {
                    calls.push(FoldedCall {
                        call_id,
                        name,
                        output,
                        open_line,
                        pending,
                        full,
                    });
                    self.transcript.account_full_output(
                        isize::try_from(moved).unwrap_or(isize::MAX),
                        FULL_OUTPUT_BUDGET,
                    );
                }
            }
            _ => self.transcript.push(row),
        }
    }

    fn set_job_status(&mut self, job_id: &str, status: JobStatus) {
        let row = self
            .transcript
            .cells()
            .iter()
            .rposition(|cell| matches!(cell, Cell::Job { job_id: id, .. } if id == job_id));
        if let Some(Cell::Job {
            status: current, ..
        }) = row.and_then(|row| self.transcript.cell_mut(row))
        {
            *current = status;
        }
    }

    fn set_agent_status(&mut self, agent_id: &str, status: AgentStatus) {
        let row =
            self.transcript.cells().iter().rposition(
                |cell| matches!(cell, Cell::Agent { agent_id: id, .. } if id == agent_id),
            );
        if let Some(Cell::Agent {
            status: current, ..
        }) = row.and_then(|row| self.transcript.cell_mut(row))
        {
            *current = status;
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
                ..
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
        if let Some((target, full)) = target {
            let grew = full.push(&append_output(target, bytes));
            self.transcript
                .account_full_output(grew, FULL_OUTPUT_BUDGET);
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
                full: FullOutput::default(),
                cache: LineCache::default(),
            });
        }
        let Some(Cell::Text {
            lines,
            open_line,
            pending,
            full,
            ..
        }) = self.transcript.last_mut()
        else {
            return;
        };
        let grew = full.push(&append_output((lines, open_line, pending), bytes));
        self.transcript
            .account_full_output(grew, FULL_OUTPUT_BUDGET);
    }

    /// The output lines of the call `call_id`: a tool row's own, within
    /// this turn, or those of a call folded into a job's or agent's row,
    /// which a later turn's check reaches back to.
    fn tool_output_target(
        &mut self,
        first_cell: usize,
        call_id: &str,
    ) -> Option<(OutputTarget<'_>, &mut FullOutput)> {
        let index = self
            .transcript
            .cells()
            .iter()
            .enumerate()
            .rev()
            .filter(|(index, cell)| {
                *index >= first_cell || matches!(cell, Cell::Job { .. } | Cell::Agent { .. })
            })
            .find(|(_, cell)| cell.takes_output_of(call_id))?
            .0;
        self.transcript.cell_mut(index)?.output_for(call_id)
    }

    /// The output lines of a call without an id, within this turn: the
    /// newest running tool row without one (a finished one has had all
    /// its output), or the newest such call folded into a running job's
    /// or agent's row; a finished job or agent takes nothing more.
    fn anonymous_tool_output_target(
        &mut self,
        first_cell: usize,
    ) -> Option<(OutputTarget<'_>, &mut FullOutput)> {
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

    /// Every state is dim and the idle hint is italic as well. The
    /// terminal backend only rewrites cells whose symbol or style changed,
    /// so a shared style would let a turn's first frame skip the letters
    /// it has in common with the hint and leave a torn word in the output
    /// stream. Modifiers survive `NO_COLOR`, so this holds there too.
    fn status_text(&self) -> Option<Span<'static>> {
        if self.exiting {
            return Some(Span::styled("Leaving...", DIM));
        }
        let Some(turn) = self.turn.as_ref() else {
            // The key help takes the chords the idle hint names.
            if !self.prompts.is_empty() || self.help.is_some() {
                return None;
            }
            let status = if self.key_context().question_mark_opens_help() {
                IDLE_STATUS_WITH_HELP
            } else {
                IDLE_STATUS
            };
            return Some(Span::styled(status, IDLE_HINT));
        };
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
        if self.prompts.is_empty() && self.help.is_none() {
            Some(Span::styled(format!("{state}  Esc to interrupt"), DIM))
        } else {
            Some(Span::styled(state, DIM))
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
        debug_assert!(
            self.help.is_none() || self.pager.is_none(),
            "the key help is open under the pager"
        );
        if let Some(pager) = self.pager.as_mut() {
            let generation = self.transcript.generation();
            let (cells, revisions) = self.transcript.cells_for_layout();
            let text = pager.text(cells, revisions, generation, area.width, &mut self.renderer);
            pager.settle(&text, Pager::text_rows(area.height));
            frame.render_widget(PagerView { pager, text: &text }, area);
            return;
        }
        // The roster is read on every frame it is up: the tasks move on
        // whether or not the context is in hand.
        if let View::Roster(roster) = &mut self.view {
            roster.refresh(self.roster_sources.rows());
        }
        let [upper, status, bottom, footer] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(self.bottom_pane_rows(area)),
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
        let view = self.transcript.view(transcript, &mut self.renderer);
        frame.render_widget(view, transcript);
        if let Some(todo) = todo {
            frame.render_widget(&self.todo, todo);
        }
        if let Some(help) = self.help.as_mut() {
            frame.render_widget(help, upper);
        }

        frame.render_widget(StatusStrip(self.status_text()), status);

        let modal_areas = match self.prompts.front() {
            Some(prompt) => {
                let [origin, question, ..] = prompt.areas(bottom.intersection(area), area);
                frame.render_widget(PromptModal(prompt), bottom);
                Some((origin, question))
            }
            None => {
                let [band, field] = Layout::vertical([
                    Constraint::Length(self.band_rows(area)),
                    Constraint::Min(0),
                ])
                .areas(bottom);
                match &self.view {
                    View::Composer => frame.render_widget(QueuedPreview(&self.queued), band),
                    View::Palette(palette) => frame.render_widget(
                        PaletteView {
                            state: palette,
                            no_color: self.renderer.no_color,
                        },
                        band,
                    ),
                    View::HistorySearch(search) => frame.render_widget(SearchView(search), band),
                    View::Rewind(rewind) => frame.render_widget(RewindView(rewind), band),
                    View::Roster(roster) => frame.render_widget(
                        RosterView {
                            state: roster,
                            no_color: self.renderer.no_color,
                            now: self.clock.now(),
                        },
                        band,
                    ),
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
        self.origin_area = modal_areas.map(|(origin, _)| origin);
        self.question_area = modal_areas.map(|(_, question)| question);

        frame.render_widget(
            FooterBar {
                left: self.footer.text(self.roster_sources.running_counts()),
                right: render::styled_line(&self.usage),
            },
            footer,
        );
    }

    /// Records the draw; while a turn runs the elapsed-time row asks for
    /// its next tick here, after the scheduler has forgotten the frame
    /// that was just drawn, and so does an open roster for its next read
    /// unless the pager covers it. The footer counts tick too while any
    /// task runs: an agent reports finished before its join handle does,
    /// and nested spawns send no message. Nothing can spawn while the
    /// counts are zero and no turn runs, so idle stops there.
    pub(crate) fn frame_drawn(&mut self) {
        self.frames.frame_drawn();
        if self.turn.is_some()
            || (self.pager.is_none()
                && (matches!(self.view, View::Roster(_))
                    || self.roster_sources.running_counts() != (0, 0)))
        {
            self.frames.schedule_frame_in(STATUS_TICK);
        }
    }
}

struct StatusStrip(Option<Span<'static>>);

impl Widget for StatusStrip {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        if let Some(text) = self.0 {
            Paragraph::new(Line::from(text)).render(area, buf);
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

#[cfg(windows)]
struct Termination;

#[cfg(windows)]
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

/// Where the transcript printed on the way out goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DumpRoute {
    /// Fd 1, back to the terminal once the work task let go of it.
    Terminal,
    /// The screen's own descriptor: a task left running still holds fds
    /// 1 and 2 captured, and what went there would never be seen.
    ScreenFd,
}

fn dump_route(leave: &Leave) -> DumpRoute {
    match leave {
        Leave::Outlived(_) => DumpRoute::ScreenFd,
        Leave::Finished(..) | Leave::Abandoned(_) | Leave::Lost(..) => DumpRoute::Terminal,
    }
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
    // One pass of the select can find the channel empty and then the task
    // done, when the task's thread sends its last notice and ends between
    // the two polls; whatever it sent is queued by then.
    while let Ok(msg) = rx.try_recv() {
        answer_unattended(msg, &mut late);
    }
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
    #[cfg(windows)]
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
    if let Some(reason) = remember_tty() {
        app.push_notice(NoticeKind::ToolWarning, reason);
    }
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
    // The transcript goes first, on every way out, so what the exit
    // itself printed, or the message of a task that died, follows it.
    if app.config.tui_dump_on_exit {
        let text = dump::transcript_text(app.committed_cells(), &app.config);
        if !text.is_empty() {
            match dump_route(&leave) {
                DumpRoute::Terminal => write_to_terminal(&text),
                DumpRoute::ScreenFd => {
                    let _ = screen.print_line(text.strip_suffix('\n').unwrap_or(&text));
                }
            }
        }
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
                spawn_exit(
                    *ctx,
                    std::mem::take(&mut app.control_tasks),
                    abort.clone(),
                    tx.clone(),
                ),
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
        let requests = app.take_control_requests();
        if let Some(control) = &app.control {
            for request in requests {
                spawn_control_request(
                    request,
                    Arc::clone(control),
                    tx.clone(),
                    &mut app.control_tasks,
                );
            }
        }
        if quit_requested
            && task.is_none()
            && let Some(ctx) = ctx.take()
        {
            app.begin_exit();
            let controls = std::mem::take(&mut app.control_tasks);
            task = Some(Task::Exit(spawn_exit(
                ctx,
                controls,
                abort.clone(),
                tx.clone(),
            )));
        }
    }
}

#[cfg(test)]
mod tests;
