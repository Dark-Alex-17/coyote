use super::*;
use crate::tui::lend::NOT_LENDING;
#[cfg(unix)]
use crate::tui::lend::{dup_tty_out, set_lender};

/// Registers a lender on its own thread that runs every job it is
/// sent, the way the loop does with the screen released, and counts
/// them; unregistered when the guard drops.
#[cfg(unix)]
fn test_lender() -> (LenderGuard, Arc<std::sync::atomic::AtomicUsize>) {
    let (tx, mut rx) = unbounded_channel();
    let lent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&lent);
    let lender = std::thread::spawn(move || {
        while let Some(msg) = rx.blocking_recv() {
            if let UiMsg::LendTerminal(LendJob(job)) = msg {
                counter.fetch_add(1, Ordering::SeqCst);
                job(&TtyHandles { _private: () });
            }
        }
    });
    set_lender(Some((tx, lender.thread().id())));
    (LenderGuard, lent)
}

#[cfg(unix)]
struct LenderGuard;

#[cfg(unix)]
impl Drop for LenderGuard {
    fn drop(&mut self) {
        set_lender(None);
    }
}

/// The loop's copy of the terminal is close-on-exec: a tool child
/// spawned during a turn must not inherit a second handle on the tty
/// next to the capture pipe it was given.
#[cfg(unix)]
#[test]
fn the_lent_tty_fd_is_cloexec() {
    use std::os::fd::AsRawFd;
    let fd = match dup_tty_out() {
        Ok(fd) => fd,
        // fd 1 is closed in this process; there is nothing to pin.
        Err(err) if err.raw_os_error() == Some(libc::EBADF) => return,
        Err(err) => panic!("dup of fd 1 failed: {err}"),
    };
    // SAFETY: fcntl with F_GETFD takes no pointers.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(flags, -1, "{}", std::io::Error::last_os_error());
    assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
}

/// With the UI flag up but no loop lending the terminal (the flag
/// outlives the loop during teardown), `.edit` on a file fails
/// outright: no editor is spawned onto the capture pipe (the name
/// given is not a program at all).
#[test]
#[serial]
fn edit_file_without_a_lender_fails_without_spawning_an_editor() {
    let _tui_on = crate::testing::AtomicFlagGuard::set(&TUI_ACTIVE, true);
    let err = crate::utils::edit_file(
        "definitely-not-an-editor-x7x",
        std::path::Path::new("/dev/null"),
    )
    .unwrap_err();
    assert_eq!(err.to_string(), NOT_LENDING);
}

/// `!cmd` fails the same way without a lender.
#[test]
#[serial]
fn run_command_without_a_lender_fails_without_spawning_a_child() {
    let _tui_on = crate::testing::AtomicFlagGuard::set(&TUI_ACTIVE, true);
    let err =
        crate::utils::run_command("definitely-not-a-shell-x7x", &["-c", "true"], None).unwrap_err();
    assert_eq!(err.to_string(), NOT_LENDING);
}

/// With a lender registered, `!cmd` and `.edit` run their child
/// through it: the child runs (its exit code comes back, the file it
/// was given is touched) and the lender saw the job.
#[cfg(unix)]
#[test]
#[serial]
fn run_command_and_edit_file_run_through_the_lender_under_the_tui() {
    let _tui_on = crate::testing::AtomicFlagGuard::set(&TUI_ACTIVE, true);
    let (_lender, lent) = test_lender();
    let code = crate::utils::run_command("sh", &["-c", "exit 7"], None).unwrap();
    assert_eq!(code, 7);
    assert_eq!(lent.load(Ordering::SeqCst), 1);

    let path = crate::utils::temp_file("-lent-editor-", ".txt");
    crate::utils::edit_file("touch", &path).unwrap();
    let touched = path.is_file();
    let _ = std::fs::remove_file(&path);
    assert!(touched, "{}", path.display());
    assert_eq!(lent.load(Ordering::SeqCst), 2);
}
