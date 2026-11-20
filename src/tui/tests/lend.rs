use super::*;
use crate::tui::lend::{NOT_LENDING, OwnerGuard};
#[cfg(unix)]
use crate::tui::lend::{dup_tty_out, remember_tty, tty_out_fd_for_test};
#[cfg(unix)]
use std::sync::atomic::Ordering;

/// Registers a lender on its own thread that runs every job it is
/// sent, the way the loop does with the screen released, and counts
/// them; the previous owner is back when the guard drops.
#[cfg(unix)]
fn test_lender() -> (OwnerGuard, Arc<std::sync::atomic::AtomicUsize>) {
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
    (OwnerGuard::lending_for_test(tx, lender.thread().id()), lent)
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

/// The terminal duplicate is taken once per process: a later
/// `remember_tty`, after the owner has come and gone, keeps the same
/// descriptor, or keeps none if none could be taken.
#[cfg(unix)]
#[test]
#[serial]
fn the_tty_duplicate_is_taken_once_and_outlives_owner_changes() {
    remember_tty();
    let first = tty_out_fd_for_test();
    drop(OwnerGuard::tui_for_test());
    remember_tty();
    assert_eq!(tty_out_fd_for_test(), first);
}

/// The loop's own guard makes the UI the owner with an open inbox, and
/// hands the terminal back to the shell on drop.
#[test]
#[serial]
fn the_loop_guard_owns_the_terminal_until_dropped() {
    let (tx, _rx) = unbounded_channel();
    let guard = OwnerGuard::tui(tx);
    assert!(active());
    assert!(ensure_terminal_lendable().is_ok());
    assert!(notice(NoticeKind::Routing, "hello".to_string()));
    drop(guard);
    assert!(!active());
}

#[test]
#[serial]
fn nested_owner_guards_restore_the_owner_they_replaced() {
    let outer = OwnerGuard::tui_for_test();
    let inner = OwnerGuard::shell_for_test();
    assert!(!active());
    drop(inner);
    assert!(active());
    drop(outer);
    assert!(!active());
}

#[test]
#[serial]
fn an_owner_guard_restores_the_owner_when_its_scope_panics() {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _tui_on = OwnerGuard::tui_for_test();
        assert!(active());
        panic!("unwinding through the guard");
    }));
    assert!(outcome.is_err());
    assert!(!active());
}

/// With the UI owning the terminal but its inbox closed (as during
/// teardown), `.edit` on a file fails outright: no editor is spawned
/// onto the capture pipe (the name given is not a program at all).
#[test]
#[serial]
fn edit_file_without_a_lender_fails_without_spawning_an_editor() {
    let _tui_on = OwnerGuard::tui_for_test();
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
    let _tui_on = OwnerGuard::tui_for_test();
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
