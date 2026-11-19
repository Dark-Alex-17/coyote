use super::active;
use super::sink::{LendJob, UiMsg};
use crate::events::NoticeKind;
use anyhow::{Result, anyhow, bail};
use std::process::Stdio;
#[cfg(unix)]
use std::sync::OnceLock;
use std::sync::{Mutex, mpsc};
use tokio::sync::mpsc::UnboundedSender;

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

pub(super) fn set_lender(tx: Option<(UnboundedSender<UiMsg>, std::thread::ThreadId)>) {
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

/// Takes the terminal duplicate for `TtyHandles::stdio`. Returns the
/// reason when it could not be taken, for the transcript as well as the
/// log: lent children then write into the turn's capture instead.
#[cfg(unix)]
pub(super) fn remember_tty() -> Option<String> {
    let mut degraded = None;
    TTY_OUT.get_or_init(|| match dup_tty_out() {
        Ok(fd) => Some(fd),
        Err(err) => {
            let reason =
                format!("Children lent the terminal will inherit the turn's output capture: {err}");
            warn!("{reason}");
            degraded = Some(reason);
            None
        }
    });
    degraded
}

/// Duplicates fd 1 close-on-exec: the copy is for the loop to hand out
/// through `TtyHandles::stdio`, never for a child to inherit on its own.
#[cfg(unix)]
pub(super) fn dup_tty_out() -> std::io::Result<std::os::fd::OwnedFd> {
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
pub(super) fn remember_tty() -> Option<String> {
    None
}

/// What a job lent the terminal spawns its child on.
pub(crate) struct TtyHandles {
    pub(super) _private: (),
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

pub(super) const NOT_LENDING: &str = "the TUI is not lending its terminal";

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
/// the UI owns the terminal; fails when no UI is lending it, and on the
/// loop thread itself, where the wait would never end.
pub(crate) fn lend_terminal<T: Send + 'static>(
    job: impl FnOnce(&TtyHandles) -> T + Send + 'static,
) -> Result<T> {
    let Some((tx, loop_thread)) = lender() else {
        bail!(NOT_LENDING);
    };
    if std::thread::current().id() == loop_thread {
        bail!("lend_terminal on the loop thread would wait on itself");
    }
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
