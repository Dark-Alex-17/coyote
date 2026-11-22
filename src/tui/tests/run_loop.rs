use super::*;
use crate::tui::run_loop::{
    AfterTurn, DumpRoute, ImmediateCommands, Join, Leave, TYPEAHEAD_LIMIT, Task, Unfinished,
    cancel_command, dump_route, finish_turn, flush_typeahead, join_unattended, purge_turn_queue,
    quit_abort,
};
#[cfg(unix)]
use crate::tui::run_loop::{Termination, edit_in_editor};
use crate::tui::run_now::{CommandOutput, replay_deferred};

/// The loop's half of the drop: the user's lines come off the queue
/// the running turn reads, a task's notification stays for the exit
/// work, and no task at all is nothing to purge.
#[tokio::test]
#[serial]
async fn purging_the_turn_queue_takes_only_the_users_lines() {
    let queue = Arc::new(NotificationQueue::new());
    queued::queue_user_message(&queue, "later");
    queue.push(crate::supervisor::notification::job_notification(
        "job-1", "tool", true,
    ));
    let task = Some(Task::Turn {
        handle: tokio::spawn(std::future::pending()),
        command: false,
        queue: Arc::clone(&queue),
    });
    purge_turn_queue(&task);
    purge_turn_queue(&None);
    let left = queue.drain();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id(), Some("job-1"));
}

/// Once the UI has given up, a question already in the inbox and one
/// raised while the work task is still being waited for are both
/// answered unattended; the asker never sits on the reply until the
/// deadline. The second question is only asked once the first has
/// its answer, which the drain gives it, so it has to reach the wait.
#[tokio::test]
async fn prompts_raised_after_the_ui_gave_up_are_answered_unattended() {
    let (tx, mut rx) = unbounded_channel();
    let prompter = Arc::new(TuiPrompter::new(tx, Arc::new(AtomicBool::new(false))));

    let queued = Arc::clone(&prompter);
    let early = std::thread::spawn(move || queued.prompt(confirm_request(None)));
    while rx.is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let late = Arc::clone(&prompter);
    let handle = tokio::task::spawn_blocking(move || {
        let early_answer = early.join().unwrap();
        let answer = late.prompt(PromptRequest {
            default: Some(serde_json::json!(true)),
            ..confirm_request(None)
        });
        (answer, early_answer)
    });
    let started = Instant::now();
    let (Join::Finished((late_answer, early_answer)), _) =
        join_unattended(handle, &mut rx, EXIT_CANCEL_WAIT).await
    else {
        panic!("the work task finished well inside the deadline");
    };
    assert!(started.elapsed() < EXIT_CANCEL_WAIT);
    assert_eq!(early_answer.unwrap(), PromptAnswer::Confirmed(false));
    assert_eq!(late_answer.unwrap(), PromptAnswer::Confirmed(false));
}

/// Esc ends a turn by latching ctrl-c on the shared signal and nothing
/// clears it afterwards; a quit from the idle composer must not inherit
/// it, or the exit work would report a deliberate quit as interrupted
/// (no `agent.completed`). A quit during a turn is the interruption.
#[test]
fn an_idle_quit_after_an_interrupted_turn_starts_the_exit_with_a_clean_signal() {
    let abort = create_abort_signal();
    // Esc during a turn, the turn ends, the user quits idle.
    abort.set_ctrlc();
    assert!(abort.aborted_ctrlc());
    quit_abort(&abort, false);
    assert!(
        !abort.aborted_ctrlc(),
        "the idle quit carried the earlier interrupt into the exit"
    );
    assert!(!abort.aborted());
}

#[test]
fn a_quit_during_a_turn_interrupts_it() {
    let abort = create_abort_signal();
    quit_abort(&abort, true);
    assert!(abort.aborted_ctrlc());
}

/// A work task that never finishes is given up on at the deadline:
/// the wait reports a timeout rather than following the task. The
/// caller then has a task still running and no context, which is the
/// case `run` ends the process for.
#[tokio::test(start_paused = true)]
async fn join_unattended_gives_up_at_the_deadline() {
    let (_tx, mut rx) = unbounded_channel();
    let handle = tokio::spawn(std::future::pending::<()>());
    let deadline = Duration::from_millis(50);
    let started = tokio::time::Instant::now();
    assert!(matches!(
        join_unattended(handle, &mut rx, deadline).await,
        (Join::TimedOut, _)
    ));
    assert!(started.elapsed() >= deadline);
}

/// A work task that panics while the UI waits on it is told apart
/// from one that ran out of time: the panic comes back as such, with
/// its message, instead of being reported as a task left behind.
#[tokio::test]
#[serial]
async fn join_unattended_reports_a_panicking_task_as_panicked() {
    let (_tx, mut rx) = unbounded_channel();
    let handle = tokio::task::spawn_blocking(|| -> () { panic!("boom-j4p") });
    let (Join::Panicked(err), _) = join_unattended(handle, &mut rx, EXIT_CANCEL_WAIT).await else {
        panic!("the panic was not reported as one");
    };
    assert!(err.is_panic(), "{err}");
    let payload = err.into_panic();
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"boom-j4p"));
}

/// A notice sent right before the task ends (the shape of a panic's
/// message through the capture) comes back with the outcome: the
/// channel is read ahead of the task, so nothing lands between the
/// two is lost, and a prompt raised meanwhile is still answered.
#[tokio::test]
#[serial]
async fn join_unattended_keeps_the_notices_that_arrive_during_the_wait() {
    let (tx, mut rx) = unbounded_channel();
    let early = late_output(tx.clone());
    early("already queued n1j".to_string());
    let handle = tokio::task::spawn_blocking(move || {
        let notice = late_output(tx);
        notice("thread 'main' panicked at exit n2j".to_string());
        panic!("boom-n3j")
    });
    let (joined, late) = join_unattended(handle, &mut rx, EXIT_CANCEL_WAIT).await;
    assert!(matches!(joined, Join::Panicked(_)));
    assert_eq!(
        late,
        ["already queued n1j", "thread 'main' panicked at exit n2j"]
    );
}

/// A SIGTERM latched while the terminal was released survives the
/// discard that follows a shell-out, which is for the editor's Ctrl+C
/// only, and still ends the loop at the next select. The handler is
/// armed before the raise, so the signal cannot reach the default
/// disposition.
#[cfg(unix)]
#[tokio::test]
#[serial]
async fn discard_pending_keeps_a_latched_sigterm() {
    let mut termination = Termination::arm().unwrap();
    // SAFETY: raise takes a signal number and no pointers.
    unsafe { libc::raise(libc::SIGTERM) };
    tokio::time::sleep(Duration::from_millis(100)).await;
    termination.discard_pending();
    tokio::time::timeout(Duration::from_secs(2), termination.recv())
        .await
        .expect("the SIGTERM was discarded along with the editor's Ctrl+C");
}

/// An editor that exits non-zero has not finished the edit: the text
/// stays as it was, and the temp directory is gone either way, even
/// when the editor left files of its own in it.
#[test]
#[cfg(unix)]
fn a_failing_editor_leaves_the_text_as_it_was() {
    use std::os::unix::fs::PermissionsExt;
    let script = crate::utils::temp_file("-editor-", ".sh");
    // The script records the buffer's own directory, so the check is
    // for this call's directory and not for whatever a sibling case
    // has live at the same moment.
    let recorded = crate::utils::temp_file("-editor-dir-", ".txt");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\ndirname \"$1\" > '{}'\nprintf changed > \"$1\"\ntouch \"$1.swp\"\nexit 3\n",
            recorded.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();

    let edited = edit_in_editor(script.to_str().unwrap(), "draft").unwrap();
    std::fs::remove_file(&script).unwrap();
    let dir = std::fs::read_to_string(&recorded).unwrap();
    std::fs::remove_file(&recorded).unwrap();

    assert_eq!(edited, "draft");
    let dir = std::path::Path::new(dir.trim());
    assert!(
        dir.file_name()
            .is_some_and(|n| n.to_string_lossy().contains("-tui-edit-"))
    );
    assert!(
        !dir.exists(),
        "the editor's temp directory was left behind: {}",
        dir.display()
    );
    let gone = edit_in_editor("true", "kept\n").unwrap();
    assert_eq!(gone, "kept");
}

/// The buffer lives in a private directory and is private itself, so
/// an editor that saves by write-then-rename (its new file in its own
/// mode) still leaves nothing another user can read.
#[test]
#[cfg(unix)]
fn the_editor_buffer_sits_in_a_private_directory() {
    use std::os::unix::fs::PermissionsExt;
    let script = crate::utils::temp_file("-editor-", ".sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nstat -f '%Lp' \"$(dirname \"$1\")\" \"$1\" >\"$1.modes\" 2>/dev/null \
         || stat -c '%a' \"$(dirname \"$1\")\" \"$1\" >\"$1.modes\"\n\
         tr '\\n' ' ' <\"$1.modes\" >\"$1\"\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let modes = edit_in_editor(script.to_str().unwrap(), "x").unwrap();
    std::fs::remove_file(&script).unwrap();
    let modes: Vec<&str> = modes.split_whitespace().collect();
    assert_eq!(modes, ["700", "600"], "{modes:?}");
}

/// A terminal that never goes quiet cannot hold the loop after the
/// editor: the flush stops at its limit.
#[tokio::test]
async fn the_typeahead_flush_stops_at_its_limit() {
    let mut polled = 0;
    let mut stream = futures_util::stream::repeat_with(|| {
        polled += 1;
        Ok::<_, std::io::Error>(crossterm::event::Event::FocusGained)
    });
    flush_typeahead(&mut stream).await;
    drop(stream);
    assert_eq!(polled, TYPEAHEAD_LIMIT);
}

/// Only a task left running keeps fds 1 and 2 captured; its dump goes
/// through the screen's descriptor, every other one to fd 1.
#[test]
fn the_dump_goes_round_a_task_left_running() {
    assert_eq!(
        dump_route(&Leave::Outlived(Unfinished::Turn)),
        DumpRoute::ScreenFd
    );
    assert_eq!(
        dump_route(&Leave::Outlived(Unfinished::Exit)),
        DumpRoute::ScreenFd
    );
    assert_eq!(
        dump_route(&Leave::Finished(Ok(()), String::new())),
        DumpRoute::Terminal
    );
}

/// The keyboard-enhancement probe inside `Screen::enter` reads the tty
/// and blocks, so the screen is entered before the signal streams are
/// armed (a hang-up during the probe must still kill the process) and
/// before the event stream takes stdin.
#[test]
fn run_enters_the_screen_then_arms_signals_and_starts_the_event_stream() {
    let run = fn_body(include_str!("../run_loop.rs"), "pub async fn run(");
    let arm = run.find("Termination::arm()").unwrap();
    let enter = run.find("Screen::enter()").unwrap();
    let stream = run.find("EventStream::new()").unwrap();
    assert!(enter < arm, "signals armed before the screen was entered");
    assert!(
        enter < stream,
        "the event stream took stdin before the probe"
    );
}

/// Two event streams alive at once both read stdin, so the one being
/// replaced must be dropped before its successor is created.
#[test]
fn renew_stream_drops_the_old_stream_before_opening_the_new_one() {
    let renew = fn_body(include_str!("../run_loop.rs"), "fn renew_stream(");
    let dropped = renew.find("drop(stream.take())").unwrap();
    let opened = renew.find("EventStream::new()").unwrap();
    assert!(
        dropped < opened,
        "the new event stream was opened while the old one still held stdin"
    );
}

fn command_result(msg: UiMsg) -> (String, Result<CommandOutput, String>) {
    let UiMsg::CommandRan { line, result } = msg else {
        panic!("not a command result");
    };
    (line, result)
}

/// Commands typed during a turn run one at a time on the fork taken as
/// the turn started, each handing it back for the next; a turn that
/// ended meanwhile keeps it, and what is left waits for an idle turn.
#[test]
#[serial]
fn commands_beside_a_turn_share_its_snapshot_one_at_a_time() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-run-now-snapshot");
        let ctx = chat_ctx();
        let sysinfo = ctx.sysinfo(&ctx.app.config).unwrap();
        let mut immediate = ImmediateCommands::default();
        immediate.turn_started(&ctx);
        immediate.push(".info".to_string());
        immediate.push(".info bogus".to_string());
        immediate.start_next();
        assert!(immediate.is_running());
        assert_eq!(immediate.take_idle_line(), None);
        // The second line waits while the first has the snapshot out.
        immediate.start_next();
        let ran = immediate.join().await;
        let (line, result) = command_result(immediate.finish(ran));
        assert_eq!(line, ".info");
        assert_eq!(result, Ok(CommandOutput::Text(sysinfo)));
        assert!(!immediate.is_running());

        immediate.start_next();
        assert!(immediate.is_running(), "the snapshot did not come back");
        let ran = immediate.join().await;
        let (line, result) = command_result(immediate.finish(ran));
        assert_eq!(line, ".info bogus");
        let err = result.unwrap_err();
        assert!(err.contains("Unknown command"), "{err}");

        immediate.push(".help".to_string());
        immediate.start_next();
        immediate.turn_ended();
        let ran = immediate.join().await;
        let (line, result) = command_result(immediate.finish(ran));
        assert_eq!(line, ".help");
        assert!(result.is_ok());
        immediate.push(".info".to_string());
        immediate.start_next();
        assert!(
            !immediate.is_running(),
            "a finished turn's snapshot was reused"
        );
        assert_eq!(immediate.take_idle_line().as_deref(), Some(".info"));
    });
}

/// A setting typed during a turn is checked on the snapshot and kept for
/// the turn's end only when it held; replayed on the context the turn
/// hands back, it is there before anything else runs on that context.
#[test]
#[serial]
fn a_setting_beside_a_turn_reaches_the_returned_context() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-run-now-deferred");
        let ctx = chat_ctx();
        let mut immediate = ImmediateCommands::default();
        immediate.turn_started(&ctx);
        immediate.push(".set bogus 1".to_string());
        immediate.push(".set temperature 0.5".to_string());
        immediate.start_next();
        let ran = immediate.join().await;
        let (_, result) = command_result(immediate.finish(ran));
        assert!(result.is_err());
        assert!(immediate.take_deferred().is_empty());

        immediate.start_next();
        let ran = immediate.join().await;
        let (line, result) = command_result(immediate.finish(ran));
        assert_eq!(line, ".set temperature 0.5");
        assert_eq!(result, Ok(CommandOutput::Deferred));
        immediate.turn_ended();

        let mut returned = ctx;
        assert_eq!(returned.app.config.temperature, None);
        let lines = immediate.take_deferred();
        assert_eq!(lines, [".set temperature 0.5"]);
        assert!(replay_deferred(&mut returned, lines).await.is_empty());
        assert_eq!(returned.app.config.temperature, Some(0.5));
        assert!(immediate.take_deferred().is_empty());
    });
}

/// A turn on `ctx` with `setting` typed during it and checked beside it,
/// as the loop runs one: the setting's cell is on screen when it returns.
async fn turn_with_setting(
    harness: &mut Harness,
    ctx: &RequestContext,
    setting: &str,
) -> ImmediateCommands {
    let mut immediate = ImmediateCommands::default();
    harness.app.begin_turn("explain", false);
    immediate.turn_started(ctx);
    harness.type_text(setting);
    assert_eq!(
        harness.press(KeyCode::Enter),
        Some(Command::RunNow(setting.to_string()))
    );
    immediate.push(setting.to_string());
    immediate.start_next();
    let ran = immediate.join().await;
    harness.app.handle_msg(immediate.finish(ran));
    immediate
}

/// A setting taken during a turn is on the context the held line's turn
/// starts with.
#[test]
#[serial]
fn a_finished_turn_hands_its_settings_to_the_next_held_line() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-finish-turn-held");
        let ctx = chat_ctx();
        let mut harness = Harness::for_ctx(&ctx);
        let mut immediate = turn_with_setting(&mut harness, &ctx, ".set temperature 0.5").await;
        let queue = NotificationQueue::new();
        harness.type_text("held line");
        let Some(Command::Queue(text)) = harness.press(KeyCode::Enter) else {
            panic!("a line typed during a turn was not held");
        };
        queued::queue_user_message(&queue, &text);

        let after = finish_turn(
            &mut harness.app,
            &mut immediate,
            ctx,
            false,
            &TurnOutcome::default(),
            &queue,
            false,
        )
        .await;
        let AfterTurn::Next(next, line) = after else {
            panic!("the held line did not run next");
        };
        assert_eq!(line, "held line");
        assert_eq!(next.app.config.temperature, Some(0.5));
        assert!(immediate.take_deferred().is_empty());
    });
}

/// With nothing held, the context parked for the next idle line has the
/// setting.
#[test]
#[serial]
fn a_finished_turn_parks_its_context_with_the_settings() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-finish-turn-idle");
        let ctx = chat_ctx();
        let mut harness = Harness::for_ctx(&ctx);
        let mut immediate = turn_with_setting(&mut harness, &ctx, ".set temperature 0.5").await;
        let after = finish_turn(
            &mut harness.app,
            &mut immediate,
            ctx,
            false,
            &TurnOutcome::default(),
            &NotificationQueue::new(),
            false,
        )
        .await;
        let AfterTurn::Idle(parked) = after else {
            panic!("a turn with nothing held started another");
        };
        assert_eq!(parked.app.config.temperature, Some(0.5));
    });
}

/// A setting the snapshot took but the returned context refuses is named
/// in a notice, and the context keeps its old value.
#[test]
#[serial]
fn a_setting_the_returned_context_refuses_says_it_did_not_apply() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-finish-turn-refused");
        let snapshot = chat_ctx();
        let mut harness = Harness::for_ctx(&snapshot);
        harness.resize(120, 30);
        let mut immediate =
            turn_with_setting(&mut harness, &snapshot, ".set auto_continue true").await;
        let mut returned = chat_ctx();
        returned
            .update("function_calling_support false", create_abort_signal())
            .await
            .unwrap();
        let after = finish_turn(
            &mut harness.app,
            &mut immediate,
            returned,
            false,
            &TurnOutcome::default(),
            &NotificationQueue::new(),
            false,
        )
        .await;
        let AfterTurn::Idle(parked) = after else {
            panic!("a turn with nothing held started another");
        };
        assert!(!parked.app.config.auto_continue);
        let frame = harness.draw();
        assert!(
            frame.contains(".set auto_continue true did not apply"),
            "{frame}"
        );
    });
}

/// A quit while a setting is still being checked cancels the check
/// without waiting for it: the context the exit work gets never takes
/// the setting, and nothing held runs.
#[test]
#[serial]
fn a_quit_drops_the_settings_taken_during_the_turn() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-finish-turn-quit");
        let ctx = chat_ctx();
        let mut harness = Harness::for_ctx(&ctx);
        let mut immediate = ImmediateCommands::default();
        harness.app.begin_turn("explain", false);
        immediate.turn_started(&ctx);
        immediate.push(".set temperature 0.5".to_string());
        immediate.push(".info".to_string());
        immediate.start_next();
        assert!(immediate.is_running());
        immediate.clear();
        assert!(!immediate.is_running(), "the quit waited on the check");
        assert!(!immediate.checking_setting());
        harness.app.queued.push(QueuedLine {
            text: ".agent x".to_string(),
            command: true,
        });

        let after = finish_turn(
            &mut harness.app,
            &mut immediate,
            ctx,
            false,
            &TurnOutcome::default(),
            &NotificationQueue::new(),
            true,
        )
        .await;
        let AfterTurn::Idle(parked) = after else {
            panic!("a held line ran after the quit");
        };
        assert_eq!(parked.app.config.temperature, None);
        assert!(harness.app.queued.is_empty());
        assert!(immediate.take_deferred().is_empty());
        assert_eq!(immediate.take_idle_line(), None);
    });
}

/// The turn's end waits on a setting still being checked, so the line
/// held behind the turn starts with it; a reading command does not hold
/// the turn's end.
#[test]
#[serial]
fn a_turn_end_waits_for_a_setting_still_being_checked() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-finish-turn-late-setting");
        let ctx = chat_ctx();
        let mut harness = Harness::for_ctx(&ctx);
        let mut immediate = ImmediateCommands::default();
        harness.app.begin_turn("explain", false);
        immediate.turn_started(&ctx);
        immediate.push(".info".to_string());
        immediate.start_next();
        assert!(!immediate.checking_setting());
        let ran = immediate.join().await;
        harness.app.handle_msg(immediate.finish(ran));

        immediate.push(".set temperature 0.5".to_string());
        immediate.start_next();
        assert!(
            immediate.checking_setting(),
            "the turn could end with the setting still out"
        );
        let queue = NotificationQueue::new();
        harness.type_text("held line");
        let Some(Command::Queue(text)) = harness.press(KeyCode::Enter) else {
            panic!("a line typed during a turn was not held");
        };
        queued::queue_user_message(&queue, &text);

        let ran = immediate.join().await;
        harness.app.handle_msg(immediate.finish(ran));
        assert!(!immediate.checking_setting());
        let after = finish_turn(
            &mut harness.app,
            &mut immediate,
            ctx,
            false,
            &TurnOutcome::default(),
            &queue,
            false,
        )
        .await;
        let AfterTurn::Next(next, line) = after else {
            panic!("the held line did not run next");
        };
        assert_eq!(line, "held line");
        assert_eq!(next.app.config.temperature, Some(0.5));
    });
}

/// Cancelling the running command drops it with a notice, and the line
/// typed after it runs next on the turn's fork.
#[test]
#[serial]
fn a_cancelled_command_is_dropped_and_the_next_line_runs() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-run-now-cancel");
        let ctx = chat_ctx();
        let mut harness = Harness::for_ctx(&ctx);
        harness.resize(120, 30);
        let mut immediate = ImmediateCommands::default();
        harness.app.begin_turn("explain", false);
        immediate.turn_started(&ctx);
        immediate.push(".info".to_string());
        immediate.push(".help".to_string());
        immediate.start_next();
        assert!(immediate.is_running());

        cancel_command(&mut harness.app, &mut immediate);
        assert!(!immediate.is_running());
        let frame = harness.draw();
        assert!(frame.contains(".info cancelled"), "{frame}");

        immediate.start_next();
        assert!(immediate.is_running(), "the cancel took the fork with it");
        let ran = immediate.join().await;
        let (line, result) = command_result(immediate.finish(ran));
        assert_eq!(line, ".help");
        assert!(result.is_ok());
        assert_eq!(immediate.cancel(), None);
    });
}

/// Esc during a turn interrupts it, which cancels the command beside
/// it too; once the turn is over Esc cancels a command it left running.
#[test]
fn esc_cancels_a_command_left_running_after_the_turn() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    assert_eq!(harness.press(KeyCode::Esc), Some(Command::Interrupt));
    harness.app.end_turn(false, &TurnOutcome::default());
    harness.app.command_running = true;
    assert_eq!(harness.press(KeyCode::Esc), Some(Command::Cancel));
    harness.app.command_running = false;
    assert_eq!(harness.press(KeyCode::Esc), None);
}
