use super::{AbortSignal, IS_STDOUT_TERMINAL, poll_abort_signal, wait_abort_signal};

use anyhow::{Result, anyhow};
use crossterm::{cursor, queue, style, terminal};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{
    future::Future,
    io::{Write, stdout},
    sync::{Arc, Mutex, mpsc as std_mpsc},
    time::Duration,
};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver},
    oneshot,
};

const FRAME_INTERVAL: Duration = Duration::from_millis(50);
const STOP_ACK_DEADLINE: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
pub struct SpinnerInner {
    index: usize,
    message: String,
    #[cfg(test)]
    cleared: Option<Arc<AtomicUsize>>,
}

impl SpinnerInner {
    const DATA: [&'static str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

    #[cfg(test)]
    fn observed(cleared: Arc<AtomicUsize>) -> Self {
        Self {
            index: 0,
            message: String::new(),
            cleared: Some(cleared),
        }
    }

    fn step(&mut self) -> Result<()> {
        if !*IS_STDOUT_TERMINAL || self.message.is_empty() {
            return Ok(());
        }
        let mut writer = stdout();
        let frame = Self::DATA[self.index % Self::DATA.len()];
        let dots = ".".repeat((self.index / 5) % 4);
        let line = format!("{frame}{}{:<3}", self.message, dots);
        queue!(writer, cursor::MoveToColumn(0), style::Print(line),)?;
        if self.index == 0 {
            queue!(writer, cursor::Hide)?;
        }
        writer.flush()?;
        self.index += 1;
        Ok(())
    }

    fn set_message(&mut self, message: String) -> Result<()> {
        self.clear_message()?;
        if !message.is_empty() {
            self.message = format!(" {message}");
        }
        Ok(())
    }

    fn clear_message(&mut self) -> Result<()> {
        if self.message.is_empty() {
            return Ok(());
        }
        self.message.clear();
        #[cfg(test)]
        if let Some(cleared) = &self.cleared {
            cleared.fetch_add(1, Ordering::SeqCst);
        }
        if !*IS_STDOUT_TERMINAL {
            return Ok(());
        }
        let mut writer = stdout();
        queue!(
            writer,
            cursor::MoveToColumn(0),
            terminal::Clear(terminal::ClearType::FromCursorDown),
            cursor::Show
        )?;
        writer.flush()?;
        Ok(())
    }
}

/// Restores the terminal when the spinner goes away by any route, not just the
/// loop's normal exit: a future cancelled mid-spin (the surrounding task was
/// aborted) or an early `?` return would otherwise leave the last frame on
/// screen and the cursor hidden. `clear_message` is a no-op once the message
/// is gone, so a normal-path clear leaves nothing for the drop to redo.
impl Drop for SpinnerInner {
    fn drop(&mut self) {
        let _ = self.clear_message();
    }
}

#[derive(Clone)]
enum SpinnerTx {
    Std(std_mpsc::Sender<SpinnerEvent>),
    Tokio(mpsc::UnboundedSender<SpinnerEvent>),
}

impl SpinnerTx {
    fn send(&self, event: SpinnerEvent) -> Result<()> {
        match self {
            Self::Std(tx) => tx.send(event)?,
            Self::Tokio(tx) => tx.send(event)?,
        }
        Ok(())
    }
}

/// Handle to a running spinner. A `spawn_spinner` one lives on its own
/// thread and acknowledges `stop` once the terminal is clear; a `create`
/// one is stepped by whichever `abortable_run_with_spinner*` call consumes
/// its receiver, so its stop needs no ack.
#[derive(Clone)]
pub struct Spinner {
    tx: SpinnerTx,
    cleared: Option<Arc<Mutex<std_mpsc::Receiver<()>>>>,
}

impl Spinner {
    pub fn create(message: &str) -> (Self, UnboundedReceiver<SpinnerEvent>) {
        let (tx, spinner_rx) = mpsc::unbounded_channel();
        let spinner = Spinner {
            tx: SpinnerTx::Tokio(tx),
            cleared: None,
        };
        let _ = spinner.set_message(message.to_string());
        (spinner, spinner_rx)
    }

    pub fn set_message(&self, message: String) -> Result<()> {
        self.tx.send(SpinnerEvent::SetMessage(message))
    }

    /// Returns once the spinner's line is gone, or after `STOP_ACK_DEADLINE`
    /// if the thread never answers; the caller may print straight after.
    pub fn stop(&self) {
        let _ = self.tx.send(SpinnerEvent::Stop);
        if let Some(cleared) = &self.cleared {
            let _ = cleared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv_timeout(STOP_ACK_DEADLINE);
        }
    }
}

pub enum SpinnerEvent {
    SetMessage(String),
    Stop,
}

/// Runs the spinner on its own thread, off the tokio runtime, so a frame
/// lands every `FRAME_INTERVAL` whatever the async workers are doing and
/// `stop` can block on the clear without a runtime in scope.
pub fn spawn_spinner(message: &str) -> Spinner {
    spawn_spinner_with(SpinnerInner::default(), message)
}

/// Returns a dark handle when the thread cannot be spawned: sends fail
/// harmlessly and `stop` has no ack to wait for.
fn spawn_spinner_with(mut inner: SpinnerInner, message: &str) -> Spinner {
    let (tx, spinner_rx) = std_mpsc::channel();
    let (cleared_tx, cleared_rx) = std_mpsc::channel();
    let mut spinner = Spinner {
        tx: SpinnerTx::Std(tx),
        cleared: Some(Arc::new(Mutex::new(cleared_rx))),
    };
    let _ = spinner.set_message(message.to_string());
    let spawned = std::thread::Builder::new()
        .name("spinner".into())
        .spawn(move || {
            loop {
                match spinner_rx.recv_timeout(FRAME_INTERVAL) {
                    Ok(SpinnerEvent::SetMessage(message)) => {
                        if inner.set_message(message).is_err() {
                            break;
                        }
                    }
                    Ok(SpinnerEvent::Stop) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                        break;
                    }
                    Err(std_mpsc::RecvTimeoutError::Timeout) => {
                        let _ = inner.step();
                    }
                }
            }
            let _ = inner.clear_message();
            let _ = cleared_tx.send(());
        });
    if let Err(err) = spawned {
        debug!("Failed to spawn the spinner thread: {err}");
        spinner.cleared = None;
    }
    spinner
}

/// Whether the calling context is the one the user's Ctrl-C is addressed to.
///
/// tokio broadcasts SIGINT to every pending `ctrl_c()` future in the process,
/// so a wrapper that listened unconditionally would abort every in-flight
/// completion at once, including the background children a first Ctrl-C is
/// meant to leave running. Only an `Owned` run installs the listener; a
/// `NotOwned` run ends when its task finishes or its own abort signal is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserInterrupt {
    Owned,
    NotOwned,
}

pub async fn abortable_run_with_spinner<F, T>(
    task: F,
    message: &str,
    abort_signal: AbortSignal,
    policy: UserInterrupt,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let (_, spinner_rx) = Spinner::create(message);
    abortable_run_with_spinner_rx(task, spinner_rx, abort_signal, policy).await
}

pub async fn abortable_run_with_spinner_rx<F, T>(
    task: F,
    spinner_rx: UnboundedReceiver<SpinnerEvent>,
    abort_signal: AbortSignal,
    policy: UserInterrupt,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    if *IS_STDOUT_TERMINAL {
        let (done_tx, done_rx) = oneshot::channel();
        let (task_ret, spinner_ret) = tokio::join!(
            run_abortable(task, &abort_signal, policy, done_tx),
            run_abortable_spinner(spinner_rx, done_rx, abort_signal.clone())
        );
        spinner_ret?;
        task_ret
    } else {
        task.await
    }
}

/// The SIGINT listener exists only for an `Owned` run; see `UserInterrupt`.
/// Returning `None` rather than a never-ready future keeps the listener from
/// being registered at all. The arm resolves only on a delivered SIGINT: a
/// failed handler registration is not an interrupt, so an `Err` from
/// `ctrl_c()` parks the arm forever, as `wait_user_interrupt` does.
fn interrupt_arm(policy: UserInterrupt) -> Option<impl Future<Output = ()>> {
    match policy {
        UserInterrupt::Owned => Some(async {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        }),
        UserInterrupt::NotOwned => None,
    }
}

/// Signals `done_tx` here rather than from a wrapping async block: every layer
/// that moves `task` into a new future keeps another copy of it in the state
/// machine, and the completion futures this wraps are large enough for one
/// extra copy to overflow a test thread's stack.
async fn run_abortable<F, T>(
    task: F,
    abort_signal: &AbortSignal,
    policy: UserInterrupt,
    done_tx: oneshot::Sender<()>,
) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    let ret = match interrupt_arm(policy) {
        Some(ctrl_c) => {
            tokio::select! {
                ret = task => ret,
                _ = ctrl_c => {
                    abort_signal.set_ctrlc();
                    Err(anyhow!("Aborted!"))
                },
                _ = wait_abort_signal(abort_signal) => Err(anyhow!("Aborted.")),
            }
        }
        None => {
            tokio::select! {
                ret = task => ret,
                _ = wait_abort_signal(abort_signal) => Err(anyhow!("Aborted.")),
            }
        }
    };
    let _ = done_tx.send(());
    ret
}

async fn run_abortable_spinner(
    mut spinner_rx: UnboundedReceiver<SpinnerEvent>,
    mut done_rx: oneshot::Receiver<()>,
    abort_signal: AbortSignal,
) -> Result<()> {
    let mut spinner = SpinnerInner::default();
    loop {
        if abort_signal.aborted() {
            break;
        }

        tokio::time::sleep(Duration::from_millis(25)).await;

        match done_rx.try_recv() {
            Ok(_) | Err(oneshot::error::TryRecvError::Closed) => {
                break;
            }
            _ => {}
        }

        match spinner_rx.try_recv() {
            Ok(SpinnerEvent::SetMessage(message)) => {
                spinner.set_message(message)?;
            }
            Ok(SpinnerEvent::Stop) => {
                spinner.clear_message()?;
            }
            Err(_) => {}
        }

        if poll_abort_signal(&abort_signal)? {
            break;
        }

        spinner.step()?;
    }

    spinner.clear_message()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::create_abort_signal;
    use serial_test::serial;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn observed(message: &str) -> (SpinnerInner, Arc<AtomicUsize>) {
        let cleared = Arc::new(AtomicUsize::new(0));
        let mut spinner = SpinnerInner::observed(cleared.clone());
        spinner.set_message(message.to_string()).unwrap();
        (spinner, cleared)
    }

    #[test]
    #[serial]
    fn drop_with_active_message_clears_once() {
        let (spinner, cleared) = observed("Thinking");
        assert_eq!(cleared.load(Ordering::SeqCst), 0);
        drop(spinner);
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[serial]
    fn explicit_clear_then_drop_clears_exactly_once() {
        let (mut spinner, cleared) = observed("Thinking");
        spinner.clear_message().unwrap();
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
        drop(spinner);
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn drop_without_message_is_silent() {
        let (spinner, cleared) = observed("");

        drop(spinner);

        assert_eq!(cleared.load(Ordering::SeqCst), 0);
    }

    #[test]
    #[serial]
    fn set_message_replaces_and_clears_previous_once() {
        let (mut spinner, cleared) = observed("Thinking");
        spinner.set_message("Fetching".to_string()).unwrap();
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
        drop(spinner);
        assert_eq!(cleared.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    #[serial]
    async fn cancelled_task_restores_terminal_on_drop() {
        let (spinner, cleared) = observed("Thinking");
        let task = tokio::spawn(async move {
            let _spinner = spinner;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(cleared.load(Ordering::SeqCst), 0);

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
    }

    /// No runtime here on purpose: the thread-backed spinner must stop from
    /// plain sync code. The counter, not the clock, proves the clear ran
    /// before `stop` returned; the timing bound only shows it was the ack
    /// that released the call rather than the deadline.
    #[test]
    #[serial]
    fn spawned_spinner_stop_returns_after_the_clear() {
        let cleared = Arc::new(AtomicUsize::new(0));
        let spinner = spawn_spinner_with(SpinnerInner::observed(cleared.clone()), "Thinking");
        std::thread::sleep(FRAME_INTERVAL * 2);
        assert_eq!(cleared.load(Ordering::SeqCst), 0);

        let started = std::time::Instant::now();
        spinner.stop();
        let elapsed = started.elapsed();

        assert_eq!(cleared.load(Ordering::SeqCst), 1);
        assert!(elapsed < STOP_ACK_DEADLINE / 2, "stop took {elapsed:?}");

        spinner.stop();
        assert_eq!(cleared.load(Ordering::SeqCst), 1);
    }

    fn set_ctrlc_soon(abort_signal: &AbortSignal) {
        let abort_signal = abort_signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            abort_signal.set_ctrlc();
        });
    }

    async fn run<F, T>(task: F, abort_signal: &AbortSignal, policy: UserInterrupt) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        let (done_tx, mut done_rx) = oneshot::channel();
        let ret = run_abortable(task, abort_signal, policy, done_tx).await;
        assert!(
            done_rx.try_recv().is_ok(),
            "the spinner must be told to stop"
        );
        ret
    }

    #[tokio::test]
    async fn not_owned_run_aborts_when_its_own_signal_is_set() {
        let abort_signal = create_abort_signal();
        set_ctrlc_soon(&abort_signal);

        let err = run(
            std::future::pending::<Result<()>>(),
            &abort_signal,
            UserInterrupt::NotOwned,
        )
        .await
        .unwrap_err();

        assert_eq!(err.to_string(), "Aborted.");
    }

    #[tokio::test]
    async fn not_owned_run_returns_the_task_result() {
        let abort_signal = create_abort_signal();

        let ret = run(async { Ok(42) }, &abort_signal, UserInterrupt::NotOwned)
            .await
            .unwrap();

        assert_eq!(ret, 42);
        assert!(!abort_signal.aborted());
    }

    #[tokio::test]
    async fn owned_run_aborts_when_its_own_signal_is_set() {
        let abort_signal = create_abort_signal();
        set_ctrlc_soon(&abort_signal);

        let err = run(
            std::future::pending::<Result<()>>(),
            &abort_signal,
            UserInterrupt::Owned,
        )
        .await
        .unwrap_err();

        assert_eq!(err.to_string(), "Aborted.");
    }

    #[tokio::test]
    async fn owned_run_returns_the_task_result() {
        let abort_signal = create_abort_signal();

        let ret = run(async { Ok(7) }, &abort_signal, UserInterrupt::Owned)
            .await
            .unwrap();

        assert_eq!(ret, 7);
    }

    #[test]
    fn not_owned_run_installs_no_sigint_listener() {
        assert!(interrupt_arm(UserInterrupt::NotOwned).is_none());
        assert!(interrupt_arm(UserInterrupt::Owned).is_some());
    }

    /// The policy only holds if `interrupt_arm` is the one place a SIGINT
    /// listener can come from; a second `ctrl_c()` anywhere in this file
    /// would let a `NotOwned` run regain one. The same body must park on a
    /// failed registration rather than resolve, or the run would be
    /// "interrupted" by a handler that never installed.
    #[test]
    fn interrupt_arm_is_the_only_sigint_listener_in_this_file() {
        let source = include_str!("spinner.rs").replace("\r\n", "\n");
        let listener = ["tokio::signal::", "ctrl_c()"].concat();
        assert_eq!(source.matches(&listener).count(), 1);

        let start = source.find("fn interrupt_arm(").unwrap();
        let body = &source[start..];
        let end = body.find("\n}\n").unwrap();
        assert!(body[..end].contains(&listener));
        assert!(body[..end].contains("std::future::pending"));
    }

    fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    fn production_half(source: &str) -> &str {
        source
            .find("\n#[cfg(test)]\nmod tests")
            .map_or(source, |end| &source[..end])
    }

    /// The top-level arguments of the call whose opening paren starts `rest`.
    fn call_args(rest: &str) -> Vec<String> {
        let mut depth = 0usize;
        let mut args = Vec::new();
        let mut current = String::new();
        for ch in rest.chars() {
            match ch {
                '(' | '[' | '{' => {
                    depth += 1;
                    if depth > 1 {
                        current.push(ch);
                    }
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    current.push(ch);
                }
                ',' if depth == 1 => {
                    args.push(current.trim().to_string());
                    current.clear();
                }
                _ => current.push(ch),
            }
        }
        if !current.trim().is_empty() {
            args.push(current.trim().to_string());
        }
        args
    }

    /// The policy is derived from the ctx at exactly the three sites a
    /// spawned child can reach: a completion, `rebuild_tool_scope` and a
    /// graph `rag` node. Every other spinner is foreground work of the root
    /// turn and says so with the `Owned` literal, or forwards its caller's
    /// `policy`; a new call site has to pick one of those on purpose. The
    /// `NotOwned` literal itself belongs only to the derivation in
    /// `request_context.rs` and the match in this file. The census assumes
    /// the production half of a file ends at a `\n#[cfg(test)]\nmod tests`
    /// marker, and `call_args` is not string-literal aware, so a paren or
    /// comma inside a quoted argument miscounts.
    #[test]
    fn interrupt_policy_is_derived_at_three_sites_and_owned_literally_elsewhere() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&src, &mut files);
        files.sort();
        assert!(files.len() > 50, "walked {} files", files.len());

        let mut derivations = BTreeMap::new();
        let mut not_owned_literals = BTreeMap::new();
        let mut unpinned = Vec::new();
        for path in &files {
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let source = std::fs::read_to_string(path).unwrap().replace("\r\n", "\n");
            let production = production_half(&source);
            let count = production.matches(".interrupt_policy()").count();
            if count > 0 {
                derivations.insert(rel.clone(), count);
            }
            let count = production.matches("UserInterrupt::NotOwned").count();
            if count > 0 {
                not_owned_literals.insert(rel.clone(), count);
            }
            for needle in [
                "abortable_run_with_spinner(",
                "abortable_run_with_spinner_rx(",
            ] {
                for (start, _) in production.match_indices(needle) {
                    let args = call_args(&production[start + needle.len() - 1..]);
                    let last = args.last().map(String::as_str).unwrap_or_default();
                    let pinned = last == "policy"
                        || last.ends_with(".interrupt_policy()")
                        || last == "UserInterrupt::Owned";
                    if !pinned {
                        unpinned.push(format!("{rel}: {needle}{})", args.join(", ")));
                    }
                }
            }
        }

        assert_eq!(
            derivations,
            BTreeMap::from([
                ("client/common.rs".to_string(), 1),
                ("config/request_context.rs".to_string(), 1),
                ("graph/rag.rs".to_string(), 1),
            ])
        );
        assert_eq!(
            not_owned_literals,
            BTreeMap::from([
                ("config/request_context.rs".to_string(), 1),
                ("utils/spinner.rs".to_string(), 1),
            ])
        );
        assert!(unpinned.is_empty(), "{}", unpinned.join("\n"));
    }
}
