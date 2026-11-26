use super::*;
use crate::config::child_sessions::{ChildMeta, ChildStatus};
use crate::tui::control::{
    cancel_context, catch_cancel, catch_child_load, settle_control_tasks, spawn_exit,
};
use crate::tui::run_loop::{EXIT_TEARDOWN_WAIT, Join, join_unattended};
use crate::utils::AbortSignal;
use chrono::{TimeZone, Utc};

/// The exit waits for control tasks only up to its bound: one that
/// finishes in time is joined, one that never does is aborted, and
/// the wait returns either way.
#[tokio::test(start_paused = true)]
async fn the_exit_aborts_control_tasks_still_running_at_its_bound() {
    let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let mut controls = JoinSet::new();
    controls.spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = finished_tx.send(());
    });
    controls.spawn(async move {
        let _held = held_tx;
        std::future::pending::<()>().await;
    });
    let bound = Duration::from_millis(50);
    let started = tokio::time::Instant::now();
    settle_control_tasks(controls, bound).await;
    assert!(started.elapsed() >= bound);
    assert_eq!(finished_rx.await, Ok(()));
    assert!(held_rx.await.is_err(), "the pending task was not aborted");
}

/// A supervised agent that never finishes. The exit work waits
/// `EXIT_CANCEL_WAIT` on it before it saves the session.
fn register_stuck_agent(ctx: &mut RequestContext) {
    use crate::supervisor::{AgentHandle, AgentResult, Supervisor};
    let join_handle = tokio::spawn(std::future::pending::<Result<AgentResult>>());
    let supervisor = Arc::new(parking_lot::RwLock::new(Supervisor::new(4, 3)));
    supervisor
        .write()
        .register(AgentHandle {
            id: "stuck-e6w".to_string(),
            agent_name: "explore".to_string(),
            depth: 1,
            inbox: Arc::new(crate::supervisor::mailbox::Inbox::new()),
            abort_signal: create_abort_signal(),
            join_handle,
            child_supervisor: None,
            session_id: None,
            child_session_root: None,
            succeeded: Default::default(),
            started_at: std::time::Instant::now(),
        })
        .unwrap();
    ctx.supervisor = Some(supervisor);
}

/// A background agent that ignores cancellation costs the exit its
/// `EXIT_CANCEL_WAIT`, not the session save: the wait on the exit
/// work outlasts the wait inside it, so `exit_session` still runs and
/// the session reaches disk. Under a wait equal to the inner one the
/// save would be lost to the bounded exit.
#[test]
#[serial]
fn a_stuck_background_agent_does_not_cost_the_exit_its_session_save() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-exit-save");
        std::fs::create_dir_all(crate::config::paths::local_dir("sessions")).unwrap();
        let abort = create_abort_signal();
        let mut saved_under_equal_wait = None;
        // The roomy wait first: the equal wait leaves its exit work
        // running past the test, under a session name of its own.
        for (name, wait, expect_saved) in [
            ("exit-save-roomy-e6w", EXIT_TEARDOWN_WAIT, true),
            ("exit-save-equal-e6w", EXIT_CANCEL_WAIT, false),
        ] {
            let mut ctx = chat_ctx();
            let app = Arc::clone(&ctx.app.config);
            ctx.use_session(&app, Some(name), abort.clone())
                .await
                .unwrap();
            ctx.set_save_session_this_time().unwrap();
            ctx.init_todo_list("persist me e6w");
            let session_files = [
                ctx.session_file_for(name, crate::config::SessionScope::Global),
                ctx.session_file_for(name, crate::config::SessionScope::Workspace),
            ];
            assert!(session_files.iter().all(|file| !file.exists()));
            register_stuck_agent(&mut ctx);
            let (tx, mut rx) = unbounded_channel();
            let (joined, _) = join_unattended(
                spawn_exit(ctx, JoinSet::new(), abort.clone(), tx),
                &mut rx,
                wait,
            )
            .await;
            let saved = session_files.iter().any(|file| {
                std::fs::read_to_string(file).is_ok_and(|text| text.contains("persist me e6w"))
            });
            match expect_saved {
                true => {
                    assert!(matches!(joined, Join::Finished((_, Ok(()), _))));
                    assert!(saved, "the session was not saved before the exit returned");
                }
                false => saved_under_equal_wait = Some((matches!(joined, Join::TimedOut), saved)),
            }
        }
        // The equal wait is the shape the bug had: it gives up first.
        assert_eq!(saved_under_equal_wait, Some((true, false)));
    });
}

/// One worker, and the work task blocks its thread for longer than the
/// frame is due in: the frame still fires on time, because the task
/// runs on the blocking pool and the lone worker stays free to service
/// the time driver. Spawned onto the worker instead, the frame would
/// wait out the whole block.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_blocking_work_task_does_not_starve_the_frame_timer() {
    const BLOCK: Duration = Duration::from_millis(300);
    let mut frames = FrameScheduler::new(Arc::new(SystemClock));
    let work = spawn_work(async { std::thread::sleep(BLOCK) });
    frames.schedule_frame_in(Duration::from_millis(10));
    let started = Instant::now();
    frames.next_frame().await;
    let waited = started.elapsed();
    assert!(
        waited < BLOCK / 2,
        "the frame waited {waited:?} for a {BLOCK:?} block on the work task"
    );
    work.await.unwrap();
}

/// A panic message reaches the loop as late output while the task is
/// still unwinding, and the loop usually feeds it to the app before
/// the join error arrives. The app hands back exactly those notices,
/// and only the lost turn's, so the restored terminal still shows them.
#[test]
fn the_lost_turns_late_output_is_handed_back_for_printing() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.handle_msg(UiMsg::Notice {
        kind: NoticeKind::Routing,
        text: "from an earlier turn".to_string(),
    });
    assert!(harness.app.turn_late_output().is_empty());

    harness.app.begin_turn("explode", false);
    let (tx, mut rx) = unbounded_channel();
    late_output(tx)("thread 'tokio' panicked at src/x.rs:1:1:\nboom-r7x\n".to_string());
    harness.app.handle_msg(rx.try_recv().unwrap());
    harness.app.handle_msg(UiMsg::Notice {
        kind: NoticeKind::ToolWarning,
        text: "unrelated".to_string(),
    });
    let late = harness.app.turn_late_output();
    assert_eq!(late.len(), 1, "{late:?}");
    assert!(late[0].contains("boom-r7x"), "{late:?}");
    assert!(late[0].contains("panicked"), "{late:?}");
}

/// What the exit work prints after `begin_exit` (a panic's message
/// among it) is the exit's late output, read from the same notices
/// a turn's would be, and never confused with a turn's.
#[test]
fn the_exit_work_has_late_output_of_its_own() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("hi", false);
    harness.app.end_turn(false, &TurnOutcome::default());
    let (tx, mut rx) = unbounded_channel();
    let late = late_output(tx);
    late("before the exit b2x".to_string());
    harness.app.handle_msg(rx.try_recv().unwrap());
    harness.app.begin_exit();
    late("thread 'main' panicked at exit e2x".to_string());
    harness.app.handle_msg(rx.try_recv().unwrap());
    assert_eq!(
        harness.app.exit_late_output(),
        ["thread 'main' panicked at exit e2x"]
    );
    assert!(harness.app.turn_late_output().is_empty());
}

/// A leftover writer's chunks fold into one cell that keeps its
/// newest text, so a chatty child cannot grow the transcript by a
/// cell per write.
#[test]
fn late_output_chunks_share_one_bounded_cell() {
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("explain", false);
    let (tx, mut rx) = unbounded_channel();
    let late = late_output(tx);
    late("first-k1q\n".to_string());
    late("second-k1q\n".to_string());
    while let Ok(msg) = rx.try_recv() {
        harness.app.handle_msg(msg);
    }
    assert_eq!(harness.app.transcript.len(), 2);
    assert_eq!(harness.app.turn_late_output(), ["first-k1q\nsecond-k1q"]);

    late("x".repeat(LATE_OUTPUT_LIMIT));
    harness.app.handle_msg(rx.try_recv().unwrap());
    let kept = harness.app.turn_late_output();
    assert_eq!(harness.app.transcript.len(), 2);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].len(), LATE_OUTPUT_LIMIT);
    assert!(!kept[0].contains("k1q"));
}

/// A tool row still running when its turn ends (interrupted, or the
/// task died under it) is marked failed rather than left spinning;
/// a job row, whose work goes on, is not.
#[test]
fn a_turn_ending_fails_its_unreported_tool_rows_and_spares_jobs() {
    let mut harness = Harness::standalone(80, 24);
    running_fs_read(&mut harness);
    harness.app.handle_msg(UiMsg::JobStarted {
        job_id: "job_1a2b3c4d".to_string(),
        tool: "execute_command".to_string(),
    });
    harness.clock.advance(Duration::from_secs(3));
    harness.app.end_turn(
        false,
        &TurnOutcome {
            error: Some("interrupted".to_string()),
            ..TurnOutcome::default()
        },
    );
    let cells = harness.app.transcript.cells();
    assert!(matches!(
        &cells[1],
        Cell::Tool {
            status: ToolStatus::Failed { elapsed, error },
            ..
        } if error == "interrupted" && *elapsed == Duration::from_secs(3)
    ));
    assert!(matches!(
        &cells[2],
        Cell::Job {
            row: TaskRow {
                status: JobStatus::Running { .. },
                ..
            },
            ..
        }
    ));
    let frame = harness.draw();
    assert!(frame.contains("3s  failed: interrupted"), "{frame}");
}

/// The loop drains the channel before `end_turn`, the way the Work arm
/// does, so a report that lost the select to the join still lands
/// ahead of the sweep.
#[test]
fn a_tool_report_queued_before_the_turn_ends_beats_the_sweep() {
    let mut harness = Harness::standalone(80, 24);
    running_fs_read(&mut harness);
    let (tx, mut rx) = unbounded_channel();
    tx.send(UiMsg::ToolFinished {
        call_id: "call-1".to_string(),
        ok: true,
        error: None,
        duration_ms: Some(1200),
    })
    .unwrap();
    while let Ok(msg) = rx.try_recv() {
        harness.app.handle_msg(msg);
    }
    harness.app.end_turn(false, &TurnOutcome::default());
    assert!(matches!(
        &harness.app.transcript.cells()[1],
        Cell::Tool {
            status: ToolStatus::Ok { elapsed },
            ..
        } if *elapsed == Duration::from_millis(1200)
    ));
}

/// `x` cancels a running job through the same `job__cancel` the model
/// calls, mid-turn included: the control context reaches the
/// supervisor the running turn has. The row goes from the roster and
/// the outcome lands in the transcript. On a finished agent `x` sends
/// no cancel: it is refused with a notice and the agent stays listed.
#[test]
#[serial]
fn x_cancels_a_running_job_and_refuses_a_finished_agent() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-roster-cancel");
        let mut ctx = chat_ctx();
        let tasks = supervisor();
        tasks.write().register(running_job("job-x7x")).unwrap();
        tasks.write().register(finished_agent("agent-x7x")).unwrap();
        ctx.supervisor = Some(Arc::clone(&tasks));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        harness.app.begin_turn("work on", false);

        harness.app.handle_key(alt(KeyCode::Char('t')));
        let frame = harness.draw();
        assert!(frame.contains("agent-x7x"), "{frame}");
        assert!(frame.contains("job-x7x"), "{frame}");
        harness.press(KeyCode::Char('x'));
        run_control_requests(&mut harness).await;
        assert!(!tasks.read().has_job("job-x7x"));
        let frame = harness.draw();
        assert!(
            !frame
                .lines()
                .any(|line| line.contains("job-x7x") && line.contains("running")),
            "{frame}"
        );

        assert_eq!(
            harness.app.focused_task().map(|task| task.id),
            None,
            "the cancelled row took the focus with it"
        );
        harness.press(KeyCode::Down);
        harness.press(KeyCode::Char('x'));
        assert!(harness.app.take_control_requests().is_empty());
        assert!(tasks.read().has_agent("agent-x7x"));
        harness.press(KeyCode::Esc);
        let frame = harness.draw();
        assert!(frame.contains("Cancelled job-x7x"), "{frame}");
        assert!(
            frame.contains("Task agent-x7x has finished; there is nothing to cancel"),
            "{frame}"
        );
    });
}

/// The tasks under `agent_lead_1a2b`'s child supervisor: a running job,
/// a running agent whose task ends when it is aborted, and a finished
/// agent. Returns the root supervisor, the child one and the two abort
/// signals.
fn nested_tasks() -> (
    Arc<parking_lot::RwLock<crate::supervisor::Supervisor>>,
    Arc<parking_lot::RwLock<crate::supervisor::Supervisor>>,
    AbortSignal,
    AbortSignal,
) {
    use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult};
    let job = running_job("job-n4q");
    let job_signal = job.abort_signal.clone();
    let agent_signal = create_abort_signal();
    let watched = agent_signal.clone();
    let lead_sup = supervisor();
    lead_sup.write().register(job).unwrap();
    lead_sup
        .write()
        .register(AgentHandle {
            abort_signal: agent_signal.clone(),
            join_handle: tokio::spawn(async move {
                while !watched.aborted() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Ok(AgentResult {
                    id: "agent_review_2b3c".to_string(),
                    agent_name: "review".to_string(),
                    output: String::new(),
                    exit_status: AgentExitStatus::Completed,
                    session_id: None,
                })
            }),
            ..agent_named("agent_review_2b3c", "review", Some("review-n4q"))
        })
        .unwrap();
    lead_sup
        .write()
        .register(finished_agent("agent_done_4d5e"))
        .unwrap();
    let tasks = supervisor();
    tasks
        .write()
        .register(AgentHandle {
            child_supervisor: Some(Arc::clone(&lead_sup)),
            ..agent_named("agent_lead_1a2b", "lead", None)
        })
        .unwrap();
    (tasks, lead_sup, job_signal, agent_signal)
}

/// Opens the tasks pop-up if it is closed and moves the focus down to
/// the row `id`.
fn focus_task(harness: &mut Harness, id: &str) {
    if harness.app.roster().is_none() {
        harness.app.handle_key(alt(KeyCode::Char('t')));
    }
    harness.draw();
    for _ in 0..8 {
        if harness.app.focused_task().is_some_and(|task| task.id == id) {
            return;
        }
        harness.press(KeyCode::Char('j'));
    }
    panic!("no row {id} in {}", harness.draw());
}

/// `x` on a job or an agent run by a spawned agent's own supervisor
/// cancels it there: the cancel goes to the supervisor that holds it,
/// not to the root one, which does not.
#[test]
#[serial]
fn x_cancels_a_running_job_and_agent_under_a_spawned_agent() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-roster-cancel-nested");
        let mut ctx = chat_ctx();
        let (tasks, lead_sup, job_signal, agent_signal) = nested_tasks();
        ctx.supervisor = Some(tasks);
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);

        focus_task(&mut harness, "job-n4q");
        harness.press(KeyCode::Char('x'));
        run_control_requests(&mut harness).await;
        assert!(job_signal.aborted());
        assert!(!lead_sup.read().has_job("job-n4q"));

        focus_task(&mut harness, "agent_review_2b3c");
        harness.press(KeyCode::Char('x'));
        run_control_requests(&mut harness).await;
        assert!(agent_signal.aborted());
        assert!(!lead_sup.read().has_agent("agent_review_2b3c"));

        harness.press(KeyCode::Esc);
        let frame = harness.draw();
        assert!(frame.contains("Cancelled job-n4q"), "{frame}");
        assert!(frame.contains("Cancelled agent 'review'"), "{frame}");
        assert!(!frame.contains("refused"), "{frame}");
    });
}

/// `x` on a finished row under a spawned agent is refused with the
/// notice and sends no cancel.
#[test]
#[serial]
fn x_refuses_a_finished_task_under_a_spawned_agent() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-roster-cancel-nested-done");
        let mut ctx = chat_ctx();
        let (tasks, lead_sup, _, _) = nested_tasks();
        while lead_sup.read().is_finished("agent_done_4d5e") != Some(true) {
            tokio::task::yield_now().await;
        }
        ctx.supervisor = Some(tasks);
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);

        focus_task(&mut harness, "agent_done_4d5e");
        harness.press(KeyCode::Char('x'));
        assert!(harness.app.take_control_requests().is_empty());
        assert!(lead_sup.read().has_agent("agent_done_4d5e"));
        harness.press(KeyCode::Esc);
        let frame = harness.draw();
        assert!(
            frame.contains("Task agent_done_4d5e has finished; there is nothing to cancel"),
            "{frame}"
        );
    });
}

/// `o` on an agent under a spawned agent opens that agent's thread, and
/// `t` with such a row focused opens the threads pop-up.
#[test]
#[serial]
fn o_and_t_work_on_a_row_under_a_spawned_agent() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-roster-open-nested");
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(ChildSessionRoot::Proc { pid: 1, start: 7 }));
        let (tasks, _, _, _) = nested_tasks();
        ctx.supervisor = Some(tasks);
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);

        focus_task(&mut harness, "agent_review_2b3c");
        harness.press(KeyCode::Char('o'));
        assert!(harness.app.pager().is_some_and(|pager| !pager.shows_root()));
        let requests = harness.app.take_control_requests();
        assert!(
            matches!(
                requests.as_slice(),
                [ControlRequest::LoadChild { session_id, .. }] if session_id == "review-n4q"
            ),
            "{requests:?}"
        );
        harness.draw();
        harness.press(KeyCode::Char('q'));
        assert_eq!(
            harness.app.focused_task().map(|task| task.id).as_deref(),
            Some("agent_review_2b3c")
        );

        harness.press(KeyCode::Char('t'));
        let frame = harness.draw();
        assert!(frame.contains("Enter open  Esc back"), "{frame}");
        assert!(frame.contains("Threads"), "{frame}");
    });
}

/// A focused row that leaves the roster takes the focus with it: `x`
/// then cancels nothing until a move picks a row.
#[test]
#[serial]
fn x_does_nothing_once_the_focused_row_has_gone() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-roster-lost-focus");
        let mut ctx = chat_ctx();
        let tasks = supervisor();
        tasks.write().register(running_job("job-a7f")).unwrap();
        tasks.write().register(running_job("job-b7f")).unwrap();
        ctx.supervisor = Some(Arc::clone(&tasks));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);

        harness.app.handle_key(alt(KeyCode::Char('t')));
        harness.draw();
        harness.press(KeyCode::Char('j'));
        let focused = |harness: &Harness| harness.app.focused_task().map(|task| task.id);
        assert_eq!(focused(&harness).as_deref(), Some("job-b7f"));
        tasks.write().take_job("job-b7f");
        harness.draw();
        assert_eq!(focused(&harness), None);
        harness.press(KeyCode::Char('x'));
        assert!(harness.app.take_control_requests().is_empty());

        harness.press(KeyCode::Char('j'));
        harness.press(KeyCode::Char('x'));
        let requests = harness.app.take_control_requests();
        let [ControlRequest::Cancel { id, .. }] = requests.as_slice() else {
            panic!("{requests:?}");
        };
        assert_eq!(id, "job-a7f");
    });
}

/// A cancel from the roster runs with the child session root the
/// turn resolved after the control context was taken; a child's
/// flush never changes it.
#[test]
#[serial]
fn a_roster_cancel_runs_with_the_child_root_the_turn_reported() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-roster-cancel-root");
        let mut ctx = chat_ctx();
        let tasks = supervisor();
        tasks.write().register(running_job("job_0a1b2c3d")).unwrap();
        ctx.supervisor = Some(Arc::clone(&tasks));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        harness.app.begin_turn("work on", false);
        let root = Arc::new(ChildSessionRoot::Proc { pid: 1, start: 11 });
        harness
            .app
            .handle_msg(UiMsg::ChildSessionRootResolved(Arc::clone(&root)));
        harness.app.handle_msg(UiMsg::ChildTranscriptFlushed {
            session_id: "stale-x7x".to_string(),
        });

        harness.app.handle_key(alt(KeyCode::Char('t')));
        harness.draw();
        harness.press(KeyCode::Char('x'));
        let requests = harness.app.take_control_requests();
        let [ControlRequest::Cancel { child_root, .. }] = requests.as_slice() else {
            panic!("{requests:?}");
        };
        assert_eq!(child_root.as_deref(), Some(&*root));
        let control = harness.app.control.clone().expect("a control context");
        assert!(control.child_session_root.is_none());
        let fork = cancel_context(&control, "job_0a1b2c3d", child_root.clone());
        assert_eq!(fork.child_session_root.as_deref(), Some(&*root));
        assert!(
            cancel_context(&control, "job_0a1b2c3d", None)
                .child_session_root
                .is_none()
        );
    });
}

#[test]
fn a_refused_or_failed_cancel_is_reported_as_a_warning() {
    let refused = serde_json::json!({"status": "error", "message": "No job 'j' is registered"});
    assert_eq!(
        cancel_report("j", &Ok(refused)),
        (
            NoticeKind::ToolWarning,
            "Cancel of j refused: No job 'j' is registered".to_string()
        )
    );
    assert_eq!(
        cancel_report("j", &Err("No supervisor active".to_string())),
        (
            NoticeKind::ToolWarning,
            "Cancel of j failed: No supervisor active".to_string()
        )
    );
}

#[tokio::test]
#[serial]
async fn a_panicking_cancel_is_reported_as_a_failed_cancel() {
    let result = catch_cancel(async { panic!("the cancel broke c4p") }).await;
    assert_eq!(
        result,
        Err("cancel task panicked: the cancel broke c4p".to_string())
    );
    assert_eq!(
        cancel_report("j", &result),
        (
            NoticeKind::ToolWarning,
            "Cancel of j failed: cancel task panicked: the cancel broke c4p".to_string()
        )
    );
    let done = serde_json::json!({"status": "ok"});
    assert_eq!(catch_cancel(async { Ok(done.clone()) }).await, Ok(done));
}

/// A child's thread is read again when the child says it wrote its
/// file, and only then; a file that no longer parses leaves an error
/// line next to what was last read.
#[test]
#[serial]
fn an_open_child_thread_is_read_again_when_the_child_flushes() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-pager-child");
        let root = ChildSessionRoot::Proc { pid: 1, start: 7 };
        write_child(&root, "reviewer-x7x", &child_yaml("first draft v1"));
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(root.clone()));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);

        harness
            .app
            .open_child("reviewer-x7x".to_string(), "reviewer".to_string());
        run_control_requests(&mut harness).await;
        let frame = harness.draw();
        insta::assert_snapshot!("pager_child_thread_80x24", &frame);
        assert!(frame.contains("first draft v1"), "{frame}");

        write_child(&root, "reviewer-x7x", &child_yaml("second draft v2"));
        harness.app.handle_msg(UiMsg::ChildTranscriptFlushed {
            session_id: "planner-other".to_string(),
        });
        assert!(harness.app.take_control_requests().is_empty());
        harness.app.handle_msg(UiMsg::ChildTranscriptFlushed {
            session_id: "reviewer-x7x".to_string(),
        });
        run_control_requests(&mut harness).await;
        let frame = harness.draw();
        assert!(frame.contains("second draft v2"), "{frame}");
        assert!(!frame.contains("first draft v1"), "{frame}");

        write_child(&root, "reviewer-x7x", "model: [unclosed\nmessages: {");
        harness.app.handle_msg(UiMsg::ChildTranscriptFlushed {
            session_id: "reviewer-x7x".to_string(),
        });
        run_control_requests(&mut harness).await;
        let frame = harness.draw();
        assert!(frame.contains("Cannot read this transcript"), "{frame}");
        assert!(frame.contains("second draft v2"), "{frame}");
    });
}

/// A child whose file never parsed shows the error alone, and one
/// with no file at all says so the same way.
#[test]
#[serial]
fn a_malformed_or_missing_child_file_degrades_to_an_error_line() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-pager-bad-child");
        let root = ChildSessionRoot::Proc { pid: 1, start: 8 };
        write_child(&root, "broken-x7x", "messages: - [");
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(root));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        for session_id in ["broken-x7x", "missing-x7x"] {
            harness
                .app
                .open_child(session_id.to_string(), "reviewer".to_string());
            run_control_requests(&mut harness).await;
            let frame = harness.draw();
            assert!(frame.contains("Cannot read this transcript"), "{frame}");
            assert!(!frame.contains("Loading..."), "{frame}");
        }
    });
}

/// A loader that panics reaches the pager as an error line, and the
/// read is no longer shown as loading.
#[test]
#[serial]
fn a_child_load_that_panics_degrades_to_an_error_line() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-pager-panicking-child");
        let root = ChildSessionRoot::Proc { pid: 1, start: 9 };
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(root));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        harness
            .app
            .open_child("panics-x7x".to_string(), "reviewer".to_string());
        let Some(ControlRequest::LoadChild {
            session_id,
            generation,
            ..
        }) = harness.app.take_control_requests().pop()
        else {
            panic!("opening a child asks for a read");
        };
        let result =
            tokio::task::spawn_blocking(|| catch_child_load(|| panic!("the loader broke")))
                .await
                .expect("the panic is caught inside the task");
        assert_eq!(result, Err("child load panicked".to_string()));
        harness.app.handle_msg(UiMsg::ChildLoaded {
            session_id,
            generation,
            result,
        });
        let frame = harness.draw();
        assert!(frame.contains("child load panicked"), "{frame}");
        assert!(!frame.contains("Loading..."), "{frame}");
    });
}

fn save_running_child(root: &ChildSessionRoot, session_id: &str) {
    let meta = crate::config::child_sessions::ChildMeta::new_running(
        session_id,
        "reviewer",
        "agent-x7x",
        crate::utils::process::ProcessStamp::current(),
        None,
        None,
        "review it",
        None,
    );
    std::fs::create_dir_all(root.children_dir()).unwrap();
    root.save_child_meta(&meta).unwrap();
}

/// A child's flush names no root; the root the turn resolves is the
/// one the picker lists from, and an open picker lists again at once.
#[test]
#[serial]
fn the_picker_lists_from_the_root_the_turn_resolves() {
    let _guard = TestConfigDirGuard::new("tui-pager-picker");
    let root = Arc::new(ChildSessionRoot::Proc { pid: 1, start: 9 });
    save_running_child(&root, "reviewer-x7x");
    let mut harness = Harness::standalone(80, 24);
    harness.app.begin_turn("review it", false);
    press_ctrl(&mut harness, 't');
    harness.app.handle_msg(UiMsg::ChildTranscriptFlushed {
        session_id: "reviewer-x7x".to_string(),
    });
    harness.press(KeyCode::Tab);
    assert!(harness.draw().contains("No child sessions yet"));

    harness
        .app
        .handle_msg(UiMsg::ChildSessionRootResolved(Arc::clone(&root)));
    let frame = harness.draw();
    assert!(frame.contains("reviewer  running"), "{frame}");
    assert!(frame.contains("reviewer-x7x"), "{frame}");
    assert!(!frame.contains("No child sessions yet"), "{frame}");
}

/// A root equal by value to the one the picker listed from, as a
/// sessionless fork resolves in an Arc of its own, leaves the open
/// picker as it is: no fresh listing and the highlight kept.
#[test]
#[serial]
fn an_equal_root_leaves_the_open_picker_and_its_highlight_alone() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-pager-picker-equal-root");
        let root = ChildSessionRoot::Proc { pid: 1, start: 13 };
        save_running_child(&root, "reviewer-e3e");
        write_child(&root, "reviewer-e3e", &child_yaml("kid wrote e3e"));
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(root.clone()));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        press_ctrl(&mut harness, 't');
        harness.press(KeyCode::Tab);
        harness.press(KeyCode::Down);
        save_running_child(&root, "reviewer-n9n");

        harness
            .app
            .handle_msg(UiMsg::ChildSessionRootResolved(Arc::new(root)));
        let frame = harness.draw();
        assert!(frame.contains("reviewer-e3e"), "{frame}");
        assert!(!frame.contains("reviewer-n9n"), "{frame}");
        harness.press(KeyCode::Enter);
        let requests = harness.app.take_control_requests();
        assert!(
            matches!(
                requests.as_slice(),
                [ControlRequest::LoadChild { session_id, .. }] if session_id == "reviewer-e3e"
            ),
            "{requests:?}"
        );
    });
}

/// Ctrl+T, Tab, Down, Enter opens a child's thread from the picker,
/// read-only, with what the child wrote. Leaving the root thread, by
/// closing the pager or by opening the child, frees the lines the
/// root's whole outputs were laid out to.
#[test]
#[serial]
fn the_picker_opens_a_childs_thread_through_the_keys() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-pager-picker-keys");
        let root = ChildSessionRoot::Proc { pid: 1, start: 12 };
        save_running_child(&root, "reviewer-k4k");
        write_child(&root, "reviewer-k4k", &child_yaml("kid wrote k4k"));
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(root));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        harness.app.handle_msg(UiMsg::ToolOutput {
            call_id: Some("job-call".to_string()),
            bytes: b"root output\n".to_vec(),
        });
        let laid_out = |harness: &mut Harness| {
            cells::any_full_output_laid_out(harness.app.transcript.cells_for_layout().0)
        };

        press_ctrl(&mut harness, 't');
        harness.draw();
        assert!(laid_out(&mut harness));
        harness.press(KeyCode::Esc);
        assert!(harness.app.pager().is_none());
        assert!(!laid_out(&mut harness));

        press_ctrl(&mut harness, 't');
        harness.draw();
        assert!(laid_out(&mut harness));
        harness.press(KeyCode::Tab);
        harness.press(KeyCode::Down);
        harness.press(KeyCode::Enter);
        assert!(!laid_out(&mut harness));
        run_control_requests(&mut harness).await;
        let frame = harness.draw();
        assert!(frame.contains("kid wrote k4k"), "{frame}");
        assert!(frame.contains("(read-only)"), "{frame}");
    });
}

/// A finished child, cancelled or completed, opens from the thread
/// pop-up like a running one: Enter asks for its transcript and the
/// pager shows it read-only.
#[test]
#[serial]
fn the_picker_opens_a_cancelled_and_a_completed_childs_thread() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-pager-picker-finished");
        let root = ChildSessionRoot::Proc { pid: 1, start: 14 };
        std::fs::create_dir_all(root.children_dir()).unwrap();
        let children = [
            ("scout-c5c", 0, ChildStatus::Cancelled, "kid wrote c5c"),
            ("planner-p6p", 1, ChildStatus::Completed, "kid wrote p6p"),
        ];
        for (session_id, minute, status, reply) in children {
            let at = Utc.with_ymd_and_hms(2026, 1, 2, 3, minute, 5).unwrap();
            let mut meta = ChildMeta::new_running_at(
                at,
                session_id,
                session_id.split('-').next().unwrap(),
                "agent-x7x",
                crate::utils::process::ProcessStamp::current(),
                None,
                None,
                "do it",
                None,
            );
            meta.finish_at(at, status, None);
            root.save_child_meta(&meta).unwrap();
            write_child(&root, session_id, &child_yaml(reply));
        }
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(root));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);

        press_ctrl(&mut harness, 't');
        for (row, (session_id, _, _, reply)) in children.into_iter().enumerate() {
            harness.press(KeyCode::Tab);
            for _ in 0..=row {
                harness.press(KeyCode::Down);
            }
            harness.press(KeyCode::Enter);
            assert!(
                matches!(
                    harness.app.control_requests.as_slice(),
                    [ControlRequest::LoadChild { session_id: asked, .. }] if asked == session_id
                ),
                "{:?}",
                harness.app.control_requests
            );
            run_control_requests(&mut harness).await;
            let frame = harness.draw();
            let title = frame.lines().next().unwrap_or_default();
            assert!(title.contains("(read-only)"), "{frame}");
            assert!(title.contains(session_id), "{frame}");
            assert!(frame.contains(reply), "{frame}");
            harness.press(KeyCode::Esc);
            assert!(harness.app.pager().is_some_and(Pager::shows_root));
        }
    });
}

/// Two flushes during the first read leave that read alone in flight
/// and ask for exactly one more, which shows the newest file.
#[test]
#[serial]
fn flushes_during_a_child_read_queue_one_more_read() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-pager-child-flushes");
        let root = ChildSessionRoot::Proc { pid: 1, start: 10 };
        write_child(&root, "reviewer-x7x", &child_yaml("first draft v1"));
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(root.clone()));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        harness
            .app
            .open_child("reviewer-x7x".to_string(), "reviewer".to_string());
        let first = harness.app.take_control_requests();
        assert_eq!(first.len(), 1, "{first:?}");
        for _ in 0..2 {
            harness.app.handle_msg(UiMsg::ChildTranscriptFlushed {
                session_id: "reviewer-x7x".to_string(),
            });
        }
        assert!(harness.app.take_control_requests().is_empty());
        write_child(&root, "reviewer-x7x", &child_yaml("third draft v3"));
        let (tx, mut rx) = unbounded_channel();
        let control = harness.app.control.clone().expect("a control context");
        for request in first {
            spawn_control_request(
                request,
                Arc::clone(&control),
                tx.clone(),
                &mut harness.app.control_tasks,
            );
        }
        let msg = rx.recv().await.expect("the read reports back");
        harness.app.handle_msg(msg);
        run_control_requests(&mut harness).await;
        assert!(harness.app.take_control_requests().is_empty());
        let frame = harness.draw();
        assert!(frame.contains("third draft v3"), "{frame}");
    });
}

/// A session id that could leave the child root is never read, from
/// the roster or from a flush.
#[test]
fn a_session_id_with_a_path_in_it_is_refused() {
    for id in [
        "../etc",
        "a/b",
        "a\\b",
        "nul\0",
        "",
        "..",
        ".",
        "C:x",
        "a b",
        "id\n",
        "caf\u{e9}",
    ] {
        assert!(!is_plain_session_id(id), "{id:?}");
    }
    for id in ["reviewer-x7x", "ok_id-1.x", "reviewer_20260102_0a1b2c3d"] {
        assert!(is_plain_session_id(id), "{id:?}");
    }
    let mut ctx = chat_ctx();
    ctx.child_session_root = Some(Arc::new(ChildSessionRoot::Proc { pid: 1, start: 11 }));
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);
    harness
        .app
        .open_child("../../etc".to_string(), "reviewer".to_string());
    assert!(harness.app.pager().is_none());
    assert!(harness.app.take_control_requests().is_empty());
}

/// Dropping a supervisor aborts its tasks without a word to the UI; the
/// next return of the context settles the rows of tasks it no longer
/// tracks, so none of them keeps the throbber ticking.
#[test]
fn a_return_settles_the_rows_of_tasks_the_supervisor_dropped() {
    run_async(async {
        let mut ctx = chat_ctx();
        let tasks = supervisor();
        tasks.write().register(running_job("job-v1d")).unwrap();
        tasks.write().register(running_agent("agent-v2d")).unwrap();
        ctx.supervisor = Some(tasks);
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.handle_msg(UiMsg::JobStarted {
            job_id: "job-v1d".to_string(),
            tool: "execute_command".to_string(),
        });
        harness.app.handle_msg(UiMsg::AgentStarted {
            agent_id: "agent-v2d".to_string(),
            name: "reviewer".to_string(),
            session_id: None,
            parent_agent_id: None,
            parent_session_id: None,
        });
        harness.app.sync_from_ctx(&mut ctx);
        let running: Vec<bool> = harness
            .app
            .transcript
            .cells()
            .iter()
            .map(Cell::running)
            .collect();
        assert_eq!(running, [true, true], "tracked rows keep running");

        ctx.supervisor = None;
        harness.app.sync_from_ctx(&mut ctx);
        let cells = harness.app.transcript.cells();
        let [Cell::Job { row: job, .. }, Cell::Agent { row: agent, .. }] = cells else {
            panic!("not a job row and an agent row");
        };
        assert_eq!(job.status, JobStatus::Cancelled);
        assert_eq!(agent.status, AgentStatus::Interrupted);
        let frame = harness.draw();
        assert!(frame.contains("job-v1d  cancelled"), "{frame}");
        harness.app.frame_drawn();
        assert_eq!(harness.app.frames.due_at(), None);
    });
}
