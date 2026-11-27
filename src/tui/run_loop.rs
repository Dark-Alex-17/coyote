use super::command_timing::{self, Timing};
use super::context::{self, SAVED_SESSION_LINE};
use super::control::{TurnOutcome, spawn_control_request, spawn_exit, spawn_run_now, spawn_turn};
use super::events::SystemClock;
use super::lend::{OwnerGuard, TtyHandles, remember_tty};
use super::pager::session_entries;
use super::queued::{carry_over, reconcile_queue, take_user_messages};
use super::rewind::apply_rewind;
use super::run_now::{CommandOutput, replay_deferred};
use super::sink::{LendJob, TuiPrompter, TuiSink, UiMsg, unattended_answer};
use super::terminal::Screen;
use super::{App, CONTROL_TASK_WAIT, Command, FooterInfo, events, queued, redirect, render};
use crate::client::Model;
use crate::config::{CommandPrefix, ExitedSession, RequestContext};
use crate::events::{EventSink, NoticeKind, SubscriptionId};
use crate::repl::{
    EXIT_CANCEL_WAIT, EXIT_HOOK_DRAIN_TIMEOUT, PreparedPaste, is_command, prepare_clipboard_paste,
};
use crate::supervisor::notification::{NotificationQueue, SystemNotification};
use crate::utils::{AbortSignal, ClipboardContent, create_abort_signal};
use anyhow::{Result, bail};
use crossterm::event::EventStream;
use futures_util::StreamExt;
use std::collections::VecDeque;
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::JoinHandle;

/// Test-only harness hook: when set to `1`, `run` panics right after
/// entering the alternate screen so a pty test can check the panic hook
/// restores the terminal. Debug builds only; a release binary ignores it.
#[cfg(debug_assertions)]
const PANIC_AT_START_ENV: &str = "COYOTE_TUI_PANIC_AT_START";

/// How long the UI waits on the exit work. The work itself waits
/// `CONTROL_TASK_WAIT` on control tasks and `EXIT_CANCEL_WAIT` on
/// background tasks before it saves the session and runs the finished
/// hook, so the wait on it has to outlast both or a task that ignores
/// cancellation would cost the user the save.
pub(super) const EXIT_TEARDOWN_WAIT: Duration = CONTROL_TASK_WAIT
    .saturating_add(EXIT_CANCEL_WAIT)
    .saturating_add(EXIT_HOOK_DRAIN_TIMEOUT);
/// How long `flush_typeahead` waits for one more queued event, and how
/// many it drops at most, so a terminal that keeps talking cannot hold the
/// loop.
const TYPEAHEAD_GRACE: Duration = Duration::from_millis(5);
pub(super) const TYPEAHEAD_LIMIT: usize = 50;
/// The command the resume hint on the way out tells the user to run.
const BINARY_NAME: &str = "coyote";

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

pub(super) enum Task {
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

/// The snapshot back from a command, with what it left to show or its error.
type CommandRun = (RequestContext, Result<CommandOutput, String>);
type RanCommand = Result<CommandRun, tokio::task::JoinError>;

/// The commands typed during a turn that run beside it rather than
/// after it: one at a time, in the order typed, each on a fork of the
/// context taken as the turn started. A command runs on a copy of that
/// fork, which replaces it when the command finishes, so a cancelled
/// command takes nothing the lines behind it need.
#[derive(Default)]
pub(super) struct ImmediateCommands {
    /// The fork, while its turn runs.
    snapshot: Option<RequestContext>,
    /// Counts the turns started, so a command that outlived its turn
    /// cannot hand its copy to the next turn's fork.
    turn: u64,
    /// How the lines typed during the turn are read; set as it starts.
    prefix: CommandPrefix,
    running: Option<RunningCommand>,
    waiting: VecDeque<String>,
    /// Settings the snapshot took, in the order typed, for the live
    /// context to take when the turn ends.
    deferred: Vec<String>,
}

struct RunningCommand {
    line: String,
    turn: u64,
    /// Whether the line is a setting being checked, which the live
    /// context takes when the turn ends.
    setting: bool,
    abort: AbortSignal,
    handle: JoinHandle<CommandRun>,
}

impl ImmediateCommands {
    pub(super) fn turn_started(&mut self, ctx: &RequestContext) {
        self.turn += 1;
        self.prefix = ctx.command_prefix();
        self.snapshot = Some(ctx.fork_for_branch());
    }

    pub(super) fn turn_ended(&mut self) {
        self.snapshot = None;
    }

    pub(super) fn push(&mut self, line: String) {
        self.waiting.push_back(line);
    }

    /// Forgets the lines still to run and the settings still to apply,
    /// for a session that is ending, and cancels the running command
    /// without waiting for it.
    pub(super) fn clear(&mut self) {
        self.cancel();
        self.waiting.clear();
        self.deferred.clear();
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Whether a setting is still being checked or waits its turn to be:
    /// the turn's end waits for it, or the line held behind the turn
    /// would start without it.
    pub(super) fn setting_pending(&self) -> bool {
        self.running.as_ref().is_some_and(|running| running.setting)
            || self
                .waiting
                .iter()
                .any(|line| is_setting(line, self.prefix))
    }

    /// Trips the running command's abort signal and lets go of it; what
    /// it returns is never shown. Returns its line. The lines behind it
    /// keep their order and run next.
    pub(super) fn cancel(&mut self) -> Option<String> {
        let running = self.running.take()?;
        running.abort.set_ctrlc();
        running.handle.abort();
        Some(running.line)
    }

    pub(super) fn take_deferred(&mut self) -> Vec<String> {
        std::mem::take(&mut self.deferred)
    }

    /// Sends the oldest waiting line off on a copy of the snapshot, when
    /// no other command runs.
    pub(super) fn start_next(&mut self) {
        if self.running.is_some() {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let Some(line) = self.waiting.pop_front() else {
            return;
        };
        let setting = is_setting(&line, self.prefix);
        let abort = create_abort_signal();
        let handle = spawn_run_now(snapshot.fork_for_branch(), line.clone(), abort.clone());
        self.running = Some(RunningCommand {
            line,
            turn: self.turn,
            setting,
            abort,
            handle,
        });
    }

    /// The oldest waiting line, once nothing runs: with no turn left to
    /// run beside, it runs as a turn like any idle line.
    pub(super) fn take_idle_line(&mut self) -> Option<String> {
        if self.running.is_some() {
            return None;
        }
        self.waiting.pop_front()
    }

    pub(super) async fn join(&mut self) -> RanCommand {
        let running = self
            .running
            .as_mut()
            .expect("polled only while a command runs");
        (&mut running.handle).await
    }

    /// The message the finished command's cell comes from. Its copy
    /// replaces the snapshot only while the turn it was taken for still
    /// runs: a turn that ended took its snapshot with it, and the next one
    /// brought its own. A setting the copy took is kept for the live
    /// context either way.
    pub(super) fn finish(&mut self, joined: RanCommand) -> UiMsg {
        let RunningCommand { line, turn, .. } = self
            .running
            .take()
            .expect("finish follows a command's join");
        let result = match joined {
            Ok((copy, result)) => {
                if turn == self.turn && self.snapshot.is_some() {
                    self.snapshot = Some(copy);
                }
                result
            }
            Err(err) => Err(format!("The command did not finish: {err}")),
        };
        if result == Ok(CommandOutput::Deferred) {
            self.deferred.push(line.clone());
        }
        UiMsg::CommandRan { line, result }
    }
}

fn is_setting(line: &str, prefix: CommandPrefix) -> bool {
    matches!(
        command_timing::lookup(line, prefix),
        Some((_, Timing::Deferred))
    )
}

/// Sends `line` off with the context as a turn, keeping a fork of the
/// context for the commands that run beside it. `images` are the paths
/// pasted into the line, which the REPL dispatch resolves its
/// `[Image #N]` placeholders against.
fn start_turn(
    app: &mut App,
    mut ctx: RequestContext,
    immediate: &mut ImmediateCommands,
    abort: &AbortSignal,
    tx: &UnboundedSender<UiMsg>,
    line: String,
    images: Vec<String>,
) -> Task {
    abort.reset();
    ctx.pasted_images = images;
    let command = is_command(&line, ctx.app.config.command_prefix);
    app.begin_turn(&line, command);
    immediate.turn_started(&ctx);
    let queue = Arc::clone(&ctx.notification_queue);
    Task::Turn {
        handle: spawn_turn(ctx, abort.clone(), line, tx.clone()),
        command,
        queue,
    }
}

/// Hands the live context the settings the snapshot took during the
/// turn, then refreshes what the screen shows of them. Runs on an
/// interrupted turn too: the settings are the user's, not the turn's.
/// It runs on the loop thread, which holds only because the timing table
/// queues every key whose update does more than change memory.
async fn apply_deferred(
    app: &mut App,
    immediate: &mut ImmediateCommands,
    ctx: &mut RequestContext,
) {
    let lines = immediate.take_deferred();
    if lines.is_empty() {
        return;
    }
    for (line, err) in replay_deferred(ctx, lines).await {
        let text = format!(
            "{} did not apply: {}",
            line.trim(),
            crate::utils::pretty_error(&err)
        );
        app.transcript.push_notice(
            app.notice_merge_from(),
            NoticeKind::ToolWarning,
            render::plain_text(&text),
        );
    }
    app.sync_from_ctx(ctx);
    app.refresh_palette(Some(ctx));
}

/// Where a finished turn leaves the context it handed back.
pub(super) enum AfterTurn {
    /// Back in hand: nothing is held, or the session is ending.
    Idle(RequestContext),
    /// The oldest held line runs next on it.
    Next(RequestContext, String),
}

/// Closes a finished turn on screen, hands the returned context the
/// settings taken during it unless the session is ending, and picks
/// what runs next on that context.
pub(super) async fn finish_turn(
    app: &mut App,
    immediate: &mut ImmediateCommands,
    mut returned: RequestContext,
    command: bool,
    outcome: &TurnOutcome,
    queue: &NotificationQueue,
    quit_requested: bool,
) -> AfterTurn {
    immediate.turn_ended();
    app.end_turn(command, outcome);
    app.sync_from_ctx(&mut returned);
    app.refresh_palette(Some(&returned));
    if !quit_requested {
        apply_deferred(app, immediate, &mut returned).await;
    }
    reconcile_queue(&mut app.queued, &take_user_messages(queue));
    // An `.exit` that ran from the queue ends the session for the lines
    // and settings behind it as well.
    if quit_requested {
        immediate.clear();
        app.drop_queued();
    }
    if quit_requested || app.queued.is_empty() {
        return AfterTurn::Idle(returned);
    }
    // What the model did not get runs as turns of its own, the rest
    // offered to each of them in turn, on the returned context's queue:
    // an agent switch during the turn left the finished turn's behind.
    let next = app.queued.remove(0);
    carry_over(&returned.notification_queue, &app.queued);
    AfterTurn::Next(returned, next.text)
}

#[cfg(unix)]
pub(super) struct Termination {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
    /// Raw mode turns Ctrl+C into a key, so SIGINT here is always a
    /// `kill -INT` and gets the same orderly leave as SIGTERM.
    int: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl Termination {
    pub(super) fn arm() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
            int: signal(SignalKind::interrupt())?,
        })
    }

    pub(super) async fn recv(&mut self) {
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
    pub(super) fn discard_pending(&mut self) {
        use futures_util::FutureExt;
        while self.int.recv().now_or_never().flatten().is_some() {}
    }
}

#[cfg(windows)]
pub(super) struct Termination;

#[cfg(windows)]
impl Termination {
    pub(super) fn arm() -> std::io::Result<Self> {
        Ok(Self)
    }

    pub(super) async fn recv(&mut self) {
        std::future::pending().await
    }

    pub(super) fn discard_pending(&mut self) {}
}

enum Step {
    Input(Option<std::io::Result<crossterm::event::Event>>),
    Msg(Option<UiMsg>),
    Frame,
    /// Boxed: the context rides back inside the join result.
    Work(Box<Done>),
    /// A command that ran beside the turn finished; boxed for the same
    /// reason.
    Ran(Box<RanCommand>),
    Signal,
}

/// Why the loop stopped.
pub(super) enum Leave {
    /// The teardown task ran on the alternate screen; its result and
    /// whatever it printed come along, with how to resume the session it
    /// left on disk.
    Finished(Result<()>, String, Option<ExitedSession>),
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
pub(super) enum Unfinished {
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
pub(super) async fn join_unattended<T>(
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
pub(super) enum Join<T> {
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

/// The line the exit leaves on the restored terminal for a session it left
/// on disk; nothing for no session or a discarded one. The names are the
/// user's text, so they are cleaned as the rest of the exit output is.
pub(super) fn saved_session_line(session: Option<&ExitedSession>) -> Option<String> {
    session.map(|ExitedSession { name, agent }| {
        render::plain_text(&match agent {
            Some(agent) => {
                format!("Session saved: {name} (resume: {BINARY_NAME} -a {agent} -s {name})")
            }
            None => format!("Session saved: {name} (resume: {BINARY_NAME} -s {name})"),
        })
    })
}

/// What the exit printed goes to the restored terminal with its colours
/// but without anything that could move the cursor or change a mode, and
/// without the REPL's own save confirmation, which the resume line
/// replaces. A terminal that is already gone (EIO after a hang-up) must
/// not turn the write into a panic ahead of the hook drain.
fn print_exit_output(captured: &str, session: Option<&ExitedSession>) {
    let kept = context::without_lines_holding(captured, SAVED_SESSION_LINE);
    write_to_terminal(&render::sanitize_ansi(kept.as_deref().unwrap_or(captured)));
    if let Some(line) = saved_session_line(session) {
        write_to_terminal(&format!("{line}\n"));
    }
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
pub(super) fn edit_in_editor(editor: &str, text: &str) -> std::io::Result<String> {
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
pub(super) async fn flush_typeahead<S>(stream: &mut S)
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
    let _owner = OwnerGuard::tui(tx.clone());
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
    // A session resumed at launch shows its history, as the REPL prints it.
    if let Some(session) = &ctx.session {
        let (compressed, active) = crate::repl::replay::snapshot(session);
        app.transcript
            .push_history(session_entries(&compressed, &active));
    }
    app.start_welcome_sweep();

    // Fd 1 is still the terminal here; a turn's capture takes it over.
    if let Some(reason) = remember_tty() {
        app.transcript
            .push_notice(app.notice_merge_from(), NoticeKind::ToolWarning, reason);
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
    let result = match leave {
        Leave::Finished(result, captured, session) => {
            print_exit_output(&captured, session.as_ref());
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
                Join::Finished((returned, result, captured)) => {
                    print_exit_output(&captured, returned.exited_session.as_ref());
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
pub(super) fn quit_abort(abort: &AbortSignal, turn_running: bool) {
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
pub(super) fn purge_turn_queue(task: &Option<Task>) {
    if let Some(Task::Turn { queue, .. }) = task {
        take_user_messages(queue);
    }
}

/// Takes the newest copy of a held line back for the composer: from the
/// lines an Esc held back first, then from the turn's queue. False when
/// neither has it, the driver having handed it to the model already.
pub(super) fn unqueue_held(
    queue: &NotificationQueue,
    held_back: &mut Vec<String>,
    text: &str,
) -> bool {
    if let Some(at) = held_back.iter().rposition(|held| held == text) {
        held_back.remove(at);
        return true;
    }
    queued::unqueue_user_message(queue, text)
}

/// Puts the lines an Esc held back on the ended turn's queue, ahead of any
/// typed since, in the order they were typed, so the end of the turn
/// finds them unsent and runs them next.
pub(super) fn restore_held_back(queue: &NotificationQueue, held_back: &mut Vec<String>) {
    if held_back.is_empty() {
        return;
    }
    let later = take_user_messages(queue);
    for text in held_back.drain(..).chain(later) {
        queue.push(SystemNotification::UserMessage { text });
    }
}

/// Where a copy went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CopyOutcome {
    Native,
    /// Sent to the terminal over OSC 52; `kept_lines` counts the newest
    /// lines kept when the whole text was too long for the request.
    Osc52 {
        kept_lines: Option<usize>,
    },
    Failed(String),
}

/// Puts `text` on the system clipboard through `native` or, when that
/// finds none, asks the terminal to take it over `writer`. The loop
/// passes the screen's own descriptor: fd 1 may be a turn's capture pipe.
pub(super) fn copy_to_clipboard_with(
    writer: &mut impl std::io::Write,
    text: &str,
    native: impl FnOnce(&str) -> bool,
) -> CopyOutcome {
    if native(text) {
        return CopyOutcome::Native;
    }
    let kept = crate::utils::osc52_payload_cap(text);
    let sent = writer
        .write_all(crate::utils::osc52_sequence(kept).as_bytes())
        .and_then(|()| writer.flush());
    match sent {
        Ok(()) => CopyOutcome::Osc52 {
            kept_lines: (kept.len() < text.len()).then(|| kept.lines().count()),
        },
        Err(err) => CopyOutcome::Failed(err.to_string()),
    }
}

/// Copies `text` over the screen's descriptor and says in the transcript
/// where it went.
fn copy_and_report(app: &mut App, screen: &mut Screen, text: &str, from: &str, dropped: usize) {
    let outcome = copy_to_clipboard_with(screen.writer(), text, crate::utils::set_text_native);
    let (kind, text) = copy_notice(&outcome, text.lines().count(), from, dropped);
    app.handle_msg(UiMsg::Notice { kind, text });
}

/// The notice after a copy of `lines` lines from `from`, which had
/// already dropped its oldest `dropped` bytes.
pub(super) fn copy_notice(
    outcome: &CopyOutcome,
    lines: usize,
    from: &str,
    dropped: usize,
) -> (NoticeKind, String) {
    let count = |n: usize| format!("{n} {}", if n == 1 { "line" } else { "lines" });
    let mut notice = match outcome {
        CopyOutcome::Failed(err) => {
            return (NoticeKind::ToolWarning, format!("Copy failed: {err}"));
        }
        CopyOutcome::Native => format!("Copied {} from {from}", count(lines)),
        CopyOutcome::Osc52 {
            kept_lines: Some(kept),
        } => format!(
            "Sent the last {} from {from} to the terminal clipboard; only those fit",
            count(*kept)
        ),
        CopyOutcome::Osc52 { kept_lines: None } => {
            format!(
                "Sent {} from {from} to the terminal clipboard",
                count(lines)
            )
        }
    };
    if dropped > 0 {
        notice.push_str(&format!(
            "; the oldest {dropped} bytes were already dropped"
        ));
    }
    // The kind only picks the style: dim, like the engine's own status
    // notices.
    (NoticeKind::Checkpoint, notice)
}

/// Reads the clipboard through `read` as the REPL's paste key does. The
/// model is checked only while the context is in hand; during a turn the
/// send checks it instead.
pub(super) fn read_clipboard_paste(
    model: Option<&Model>,
    read: impl FnOnce() -> Result<ClipboardContent>,
) -> PreparedPaste {
    match read() {
        Ok(content) => prepare_clipboard_paste(content, model),
        Err(err) => PreparedPaste::Error(format!("Clipboard unavailable: {err:#}")),
    }
}

/// Reads the clipboard off the loop's thread, since encoding and saving
/// a large screenshot would stall drawing and keys; the result comes back
/// as [`UiMsg::ClipboardPasted`].
fn spawn_clipboard_paste(tx: &UnboundedSender<UiMsg>, model: Option<Model>) {
    let tx = tx.clone();
    tokio::task::spawn_blocking(move || {
        let prepared = read_clipboard_paste(model.as_ref(), crate::utils::read_for_paste);
        // A closed inbox means the UI is gone; nobody is left to paste into.
        let _ = tx.send(UiMsg::ClipboardPasted(prepared));
    });
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
    let mut immediate = ImmediateCommands::default();
    let mut quit_requested = false;
    let mut frame_failing = false;
    // The user's lines an Esc took off the stopping turn's queue, so that
    // turn cannot hand them to its model, and those typed while it stops;
    // they go back once it has ended.
    let mut held_back: Vec<String> = Vec::new();
    let mut stopping = false;
    loop {
        let step = tokio::select! {
            event = stream.as_mut().expect("the event stream is renewed in place").next() => Step::Input(event),
            msg = rx.recv() => Step::Msg(msg),
            _ = app.frames.next_frame() => Step::Frame,
            // A turn that ends while a setting typed during it is still
            // being checked, or waits behind another command to be, is
            // joined once the check is in: the line held behind the turn
            // starts with the setting, as its notice said.
            done = join(&mut task), if task.is_some() && !immediate.setting_pending() => Step::Work(Box::new(done)),
            ran = immediate.join(), if immediate.is_running() => Step::Ran(Box::new(ran)),
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
                immediate.clear();
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
                            Join::Finished((returned, result, captured)) => {
                                Leave::Finished(result, captured, returned.exited_session)
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
                    Some(Command::Submit(line, images)) => {
                        if let Some(ctx) = ctx.take() {
                            task = Some(start_turn(
                                app,
                                ctx,
                                &mut immediate,
                                abort,
                                tx,
                                line,
                                images,
                            ));
                        } else {
                            // Submit is not mapped while a task holds the
                            // context; should that ever change, the draft
                            // goes back rather than vanishing.
                            debug_assert!(false, "a submit while the context was away");
                            app.set_composer_text(&line);
                            app.pasted_images = images;
                        }
                    }
                    Some(Command::Queue(text)) => {
                        // After an Esc the stopping turn must not hand a
                        // line to its model either; it joins the held ones.
                        if stopping {
                            held_back.push(text);
                        } else if let Some(Task::Turn { queue, .. }) = &task {
                            queued::queue_user_message(queue, &text);
                        }
                    }
                    Some(Command::RunNow(line)) => immediate.push(line),
                    Some(Command::Unqueue { held, draft }) => {
                        if let Some(Task::Turn { queue, .. }) = &task {
                            if let Some(text) = held
                                && !unqueue_held(queue, &mut held_back, &text)
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
                        if let Some(Task::Turn { queue, .. }) = &task {
                            stopping = true;
                            held_back.extend(take_user_messages(queue));
                        }
                        cancel_command(app, &mut immediate);
                    }
                    Some(Command::InterruptDroppingHeld) => {
                        abort.set_ctrlc();
                        // What is typed while the turn stops still waits
                        // for its end, as after Esc; only what was held
                        // before the Ctrl+C is dropped.
                        stopping = matches!(task, Some(Task::Turn { .. }));
                        held_back.clear();
                        purge_turn_queue(&task);
                        cancel_command(app, &mut immediate);
                    }
                    Some(Command::Cancel) => cancel_command(app, &mut immediate),
                    Some(Command::Quit) => {
                        quit_requested = true;
                        immediate.clear();
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
                    Some(Command::Copy {
                        text,
                        from,
                        dropped,
                    }) => copy_and_report(app, screen, &text, &from, dropped),
                    Some(Command::PasteImage) => {
                        spawn_clipboard_paste(
                            tx,
                            ctx.as_ref().map(|ctx| ctx.current_model().clone()),
                        );
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
            Step::Ran(ran) => {
                app.handle_msg(immediate.finish(*ran));
            }
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
                    stopping = false;
                    restore_held_back(&queue, &mut held_back);
                    match joined {
                        Ok((returned, outcome)) => {
                            // The select is unbiased: a tool's last report
                            // may still sit in the channel next to the join
                            // that won, and the end-of-turn sweep would
                            // otherwise fail its row as unreported.
                            while let Ok(msg) = rx.try_recv() {
                                app.handle_msg(msg);
                            }
                            quit_requested |= outcome.exit;
                            let after = finish_turn(
                                app,
                                &mut immediate,
                                returned,
                                command,
                                &outcome,
                                &queue,
                                quit_requested,
                            )
                            .await;
                            match after {
                                AfterTurn::Idle(returned) => ctx = Some(returned),
                                AfterTurn::Next(returned, line) => {
                                    task = Some(start_turn(
                                        app,
                                        returned,
                                        &mut immediate,
                                        abort,
                                        tx,
                                        line,
                                        Vec::new(),
                                    ));
                                }
                            }
                        }
                        Err(err) => {
                            let late = app.turn_late_output();
                            app.abandon_prompts();
                            immediate.clear();
                            return Leave::Lost(err, late);
                        }
                    }
                }
                Done::Exit(joined) => {
                    app.abandon_prompts();
                    return match joined {
                        Ok((returned, result, captured)) => {
                            Leave::Finished(result, captured, returned.exited_session)
                        }
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
        for text in app.take_copy_requests() {
            copy_and_report(app, screen, &text, "the last reply", 0);
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
        immediate.start_next();
        if task.is_none()
            && !quit_requested
            && ctx.is_some()
            && let Some(line) = immediate.take_idle_line()
            && let Some(idle) = ctx.take()
        {
            task = Some(start_turn(
                app,
                idle,
                &mut immediate,
                abort,
                tx,
                line,
                Vec::new(),
            ));
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
        app.command_running = immediate.is_running();
    }
}

/// Cancels the command running beside the turn, if any, and says so.
pub(super) fn cancel_command(app: &mut App, immediate: &mut ImmediateCommands) {
    if let Some(line) = immediate.cancel() {
        app.handle_msg(UiMsg::Notice {
            kind: NoticeKind::ToolWarning,
            text: format!("{} cancelled", line.trim()),
        });
    }
}
