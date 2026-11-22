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
/// descriptor, or keeps none if none could be taken. It stays taken for
/// the rest of the test process.
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
fn the_loop_guard_restores_the_owner_it_replaced() {
    let outer = OwnerGuard::tui_for_test();
    let (tx, _rx) = unbounded_channel();
    let inner = OwnerGuard::tui(tx);
    assert!(ensure_terminal_lendable().is_ok());
    drop(inner);
    assert!(active());
    assert!(ensure_terminal_lendable().is_err());
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

/// With the UI owning the terminal but its inbox gone, `.edit` on a file
/// fails outright: no editor is spawned onto the capture pipe (the name
/// given is not a program at all).
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

/// Under the TUI a resumed session's history, which `.session` and
/// `.agent` replay, goes to the loop as rows rather than printed into the
/// command's capture; the transcript shows every message under the
/// divider. With no TUI the replay hands nothing over.
#[test]
#[serial]
fn a_replayed_history_under_the_tui_goes_to_the_transcript_as_rows() {
    use crate::client::{Message, MessageContent, MessageRole};
    let ctx = chat_ctx();
    let text = |role, text: &str| Message::new(role, MessageContent::Text(text.to_string()));
    let compressed = [text(MessageRole::User, "an early question e1q")];
    let active = [
        text(MessageRole::User, "the last question q4r"),
        text(MessageRole::Assistant, "the last answer a9z"),
    ];
    assert!(!crate::tui::show_history(&compressed, &active));

    let (tx, mut rx) = unbounded_channel();
    let owner = OwnerGuard::tui(tx);
    crate::repl::replay::render(&ctx.app.config, &compressed, &active).unwrap();
    drop(owner);
    let msg = rx.try_recv().expect("the history did not reach the loop");
    assert!(
        matches!(&msg, UiMsg::History(entries) if entries.len() == 4),
        "{msg:?}"
    );

    let mut harness = Harness::standalone(80, 24);
    harness.app.handle_msg(msg);
    let frame = harness.draw();
    for shown in [
        "(earlier rounds, compressed)",
        "> an early question e1q",
        "> the last question q4r",
        "the last answer a9z",
        crate::tui::transcript::HISTORY_DIVIDER,
    ] {
        assert!(frame.contains(shown), "{shown:?} is not shown:\n{frame}");
    }
}

/// Under the TUI `.copy` hands the reply to the loop, which copies it
/// `.redraw session` under the TUI adds no second copy of the history:
/// While the TUI holds the terminal, a `.list` table is fitted to the
/// width of the transcript it last drew: a long description wraps inside
/// its cell and every row fits. With no TUI the width is not set.
#[test]
#[serial]
fn a_list_table_under_the_tui_fits_the_transcripts_width() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-list-width");
        let agent = crate::config::paths::agent_config_file("wrapprobe");
        std::fs::create_dir_all(agent.parent().unwrap()).unwrap();
        std::fs::write(
            &agent,
            "name: wrapprobe\ndescription: a probe agent whose description is far too long \
             to sit on one row of an eighty column transcript, so that its cell must wrap \
             onto the rows below it\nversion: 0.1.0\ninstructions: Reply briefly.\n",
        )
        .unwrap();
        let ctx = chat_ctx();
        let mut harness = Harness::standalone(80, 24);
        harness.draw();
        assert_eq!(crate::tui::table_width(), None, "no TUI holds the terminal");

        let (tx, _rx) = unbounded_channel();
        let owner = OwnerGuard::tui(tx);
        harness.draw();
        assert_eq!(crate::tui::table_width(), Some(80));
        let listing = crate::repl::list_output(&ctx, Some("agents"))
            .await
            .unwrap();
        drop(owner);
        let table: Vec<&str> = listing
            .lines()
            .filter(|line| line.contains('\u{2502}'))
            .collect();
        assert!(table.len() > 3, "{listing}");
        for line in &table {
            let plain = crate::tui::plain_text(line);
            assert!(
                unicode_width::UnicodeWidthStr::width(plain.as_str()) <= 80,
                "{plain:?} is wider than the transcript:\n{listing}"
            );
        }
        assert!(
            listing.contains("below it"),
            "the description lost its end:\n{listing}"
        );
    });
}

/// `.redraw session` under the TUI adds no second copy of the history:
/// the transcript already shows it, so nothing reaches the loop.
#[test]
#[serial]
fn dot_redraw_session_under_the_tui_does_not_add_the_history_again() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-redraw-session");
        let mut ctx = chat_ctx();
        let mut session = crate::config::Session::default();
        let input = Input::from_str(&ctx, "question", None).unwrap();
        session.add_message(&input, "reply").unwrap();
        ctx.session = Some(session);
        let (tx, mut rx) = unbounded_channel();
        let owner = OwnerGuard::tui(tx);
        Box::pin(crate::repl::run_repl_command(
            &mut ctx,
            create_abort_signal(),
            ".redraw session",
        ))
        .await
        .unwrap();
        drop(owner);
        while let Ok(msg) = rx.try_recv() {
            assert!(
                !matches!(msg, UiMsg::History(_)),
                "the history was sent again"
            );
        }
    });
}

/// Under the TUI `.copy` hands the reply to the loop, which copies it
/// over the screen's descriptor, rather than writing an OSC 52 request
/// into the turn's capture; the app keeps it for the loop to take.
#[test]
#[serial]
fn dot_copy_under_the_tui_hands_the_reply_to_the_loop() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-dot-copy");
        let mut ctx = chat_ctx();
        let input = Input::from_str(&ctx, "question", None).unwrap();
        ctx.last_message = Some(crate::config::LastMessage::new(
            input,
            "the reply c7y".to_string(),
        ));
        let (tx, mut rx) = unbounded_channel();
        let owner = OwnerGuard::tui(tx);
        Box::pin(crate::repl::run_repl_command(
            &mut ctx,
            create_abort_signal(),
            ".copy",
        ))
        .await
        .unwrap();
        drop(owner);
        let msg = rx.try_recv().expect("the reply did not reach the loop");
        assert!(matches!(&msg, UiMsg::Copy(text) if text == "the reply c7y"));

        let mut harness = Harness::for_ctx(&ctx);
        harness.app.handle_msg(msg);
        assert_eq!(harness.app.take_copy_requests(), ["the reply c7y"]);
    });
}
