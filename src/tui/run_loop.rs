use super::control::{TurnOutcome, spawn_control_request, spawn_exit, spawn_turn};
use super::events::SystemClock;
use super::lend::{TtyHandles, remember_tty, set_lender};
use super::queued::{carry_over, reconcile_queue, take_user_messages};
use super::rewind::apply_rewind;
use super::sink::{LendJob, TuiPrompter, TuiSink, UiMsg, unattended_answer};
use super::terminal::Screen;
use super::{
    App, CONTROL_TASK_WAIT, Command, FooterInfo, TUI_ACTIVE, dump, events, queued, redirect, render,
};
use crate::config::RequestContext;
use crate::events::{EventSink, NoticeKind, SubscriptionId};
use crate::repl::{EXIT_CANCEL_WAIT, EXIT_HOOK_DRAIN_TIMEOUT, is_command};
use crate::supervisor::notification::NotificationQueue;
use crate::utils::{AbortSignal, create_abort_signal};
use anyhow::{Result, bail};
use crossterm::event::EventStream;
use futures_util::StreamExt;
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
    Signal,
}

/// Why the loop stopped.
pub(super) enum Leave {
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
pub(super) enum DumpRoute {
    /// Fd 1, back to the terminal once the work task let go of it.
    Terminal,
    /// The screen's own descriptor: a task left running still holds fds
    /// 1 and 2 captured, and what went there would never be seen.
    ScreenFd,
}

pub(super) fn dump_route(leave: &Leave) -> DumpRoute {
    match leave {
        Leave::Outlived(_) => DumpRoute::ScreenFd,
        Leave::Finished(..) | Leave::Abandoned(_) | Leave::Lost(..) => DumpRoute::Terminal,
    }
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
