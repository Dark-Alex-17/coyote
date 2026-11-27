use super::pager::session_entries;
use super::sink::{LendJob, UiMsg};
use crate::client::Message;
use crate::events::NoticeKind;
use anyhow::{Result, anyhow, bail};
use std::process::Stdio;
#[cfg(unix)]
use std::sync::OnceLock;
use std::sync::{Mutex, mpsc};
use std::thread::ThreadId;
use tokio::sync::mpsc::UnboundedSender;

/// Who has the terminal: `Tui` while `run` owns it, the shell otherwise.
#[derive(Clone)]
pub(crate) enum Owner {
    Shell,
    Tui(Lender),
}

/// The loop's inbox, for work elsewhere in the process that needs the
/// terminal in its normal modes, and the thread the loop runs on: a lend
/// from that thread would wait on itself.
#[derive(Clone)]
pub(crate) struct Lender {
    tx: UnboundedSender<UiMsg>,
    thread: ThreadId,
}

static OWNER: Mutex<Owner> = Mutex::new(Owner::Shell);

/// A duplicate of the terminal taken before the screen was entered.
/// During a turn fds 1 and 2 are pointed at the capture pipe for the
/// whole process, so a child that should write to the terminal is given
/// this one explicitly. `None` when the duplicate could not be taken.
#[cfg(unix)]
static TTY_OUT: OnceLock<Option<std::os::fd::OwnedFd>> = OnceLock::new();

#[cfg(all(test, unix))]
pub(super) fn tty_out_fd_for_test() -> Option<std::os::fd::RawFd> {
    use std::os::fd::AsRawFd;
    TTY_OUT.get()?.as_ref().map(AsRawFd::as_raw_fd)
}

pub(crate) fn owner() -> Owner {
    OWNER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn replace_owner(owner: Owner) -> Owner {
    std::mem::replace(
        &mut *OWNER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        owner,
    )
}

/// Holds the terminal's owner for the guard's lifetime and puts `restore`
/// back on drop, including on panic.
pub(crate) struct OwnerGuard {
    restore: Owner,
}

impl OwnerGuard {
    /// Hands the terminal to the loop on the current thread, reachable
    /// through `tx`; the shell has it back once the guard drops.
    pub(super) fn tui(tx: UnboundedSender<UiMsg>) -> Self {
        Self {
            restore: replace_owner(Owner::Tui(Lender {
                tx,
                thread: std::thread::current().id(),
            })),
        }
    }

    /// A UI that owns the terminal but whose inbox is gone, so every lend
    /// fails as not lending; its loop thread is not the caller's, so never
    /// as a wait on itself. Tests using it must serialize (`#[serial]`).
    #[cfg(test)]
    pub(crate) fn tui_for_test() -> Self {
        static FOREIGN_THREAD: std::sync::OnceLock<ThreadId> = std::sync::OnceLock::new();
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let thread = *FOREIGN_THREAD.get_or_init(|| std::thread::spawn(|| {}).thread().id());
        Self::lending_for_test(tx, thread)
    }

    #[cfg(test)]
    pub(super) fn lending_for_test(tx: UnboundedSender<UiMsg>, thread: ThreadId) -> Self {
        Self {
            restore: replace_owner(Owner::Tui(Lender { tx, thread })),
        }
    }

    #[cfg(test)]
    pub(crate) fn shell_for_test() -> Self {
        Self {
            restore: replace_owner(Owner::Shell),
        }
    }
}

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        replace_owner(std::mem::replace(&mut self.restore, Owner::Shell));
    }
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
    if let Owner::Tui(lender) = owner()
        && lender.tx.is_closed()
    {
        bail!(NOT_LENDING);
    }
    Ok(())
}

/// Puts a notice in the transcript from anywhere in the process, the
/// loop's own thread included. False when no UI is running to show it.
pub(crate) fn notice(kind: NoticeKind, text: String) -> bool {
    match owner() {
        Owner::Tui(lender) => lender.tx.send(UiMsg::Notice { kind, text }).is_ok(),
        Owner::Shell => false,
    }
}

/// Hands `text` to the loop to put on the clipboard, from anywhere in
/// the process. False when no UI is running to take it.
pub(crate) fn request_copy(text: &str) -> bool {
    match owner() {
        Owner::Tui(lender) => lender.tx.send(UiMsg::Copy(text.to_string())).is_ok(),
        Owner::Shell => false,
    }
}

/// Hands a resumed session's history to the loop to show in the
/// transcript, from anywhere in the process; the rows are built here, off
/// the loop. False when no UI is running to show it.
pub(crate) fn show_history(compressed: &[Message], active: &[Message]) -> bool {
    match owner() {
        Owner::Tui(lender) => lender
            .tx
            .send(UiMsg::History(session_entries(compressed, active)))
            .is_ok(),
        Owner::Shell => false,
    }
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
    let Owner::Tui(Lender {
        tx,
        thread: loop_thread,
    }) = owner()
    else {
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
