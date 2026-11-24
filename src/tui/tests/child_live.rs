use super::*;
use crate::tui::cells::{FoldedCall, NodeStatus};
use crate::tui::child_live::{CHILD_ROW_LIMIT, ChildLive, ChildThread};
use crate::tui::pager::ChildEntry;
use crate::tui::sink::ChildNodeState;

const AGENT: &str = "agent_graph_5d6e";
const GRANDCHILD: &str = "agent_coder_9c0d";
const GREAT_GRANDCHILD: &str = "agent_tester_3f4a";

fn started(harness: &mut Harness) {
    started_as(harness, AGENT, "planner", "sess_5d6e", None);
}

/// Starts `agent_id`, spawned by `parent` (its agent and session ids)
/// or by the root turn when that is None.
fn started_as(
    harness: &mut Harness,
    agent_id: &str,
    name: &str,
    session_id: &str,
    parent: Option<(&str, &str)>,
) {
    harness.app.handle_msg(UiMsg::AgentStarted {
        agent_id: agent_id.to_string(),
        name: name.to_string(),
        session_id: Some(session_id.to_string()),
        parent_agent_id: parent.map(|(agent, _)| agent.to_string()),
        parent_session_id: parent.map(|(_, session)| session.to_string()),
    });
}

/// The child, its grandchild and great-grandchild, each spawned by the
/// one before.
fn started_nested(harness: &mut Harness) {
    started(harness);
    started_as(
        harness,
        GRANDCHILD,
        "coder",
        "sess_9c0d",
        Some((AGENT, "sess_5d6e")),
    );
    started_as(
        harness,
        GREAT_GRANDCHILD,
        "tester",
        "sess_3f4a",
        Some((GRANDCHILD, "sess_9c0d")),
    );
}

fn child_tool(harness: &mut Harness, node: Option<&str>, call_id: &str, name: &str) {
    child_tool_of(harness, AGENT, node, call_id, name);
}

fn child_tool_of(
    harness: &mut Harness,
    agent_id: &str,
    node: Option<&str>,
    call_id: &str,
    name: &str,
) {
    harness.app.handle_msg(UiMsg::ChildToolStarted {
        agent_id: agent_id.to_string(),
        node: node.map(str::to_string),
        call_id: call_id.to_string(),
        name: name.to_string(),
        args: Some(serde_json::json!({"path": "src/lib.rs"})),
    });
}

fn child_tool_done(harness: &mut Harness, node: Option<&str>, call_id: &str, ok: bool) {
    child_tool_done_of(harness, AGENT, node, call_id, ok);
}

fn child_tool_done_of(
    harness: &mut Harness,
    agent_id: &str,
    node: Option<&str>,
    call_id: &str,
    ok: bool,
) {
    harness.app.handle_msg(UiMsg::ChildToolFinished {
        agent_id: agent_id.to_string(),
        node: node.map(str::to_string),
        call_id: call_id.to_string(),
        name: "fs_read".to_string(),
        args: Some(serde_json::json!({"path": "src/lib.rs"})),
        ok,
        error: (!ok).then(|| "denied".to_string()),
        duration_ms: Some(250),
    });
}

fn child_node(harness: &mut Harness, node_id: &str, state: ChildNodeState) {
    harness.app.handle_msg(UiMsg::ChildNode {
        agent_id: AGENT.to_string(),
        node_id: node_id.to_string(),
        node_type: "llm".to_string(),
        state,
    });
}

fn thread(harness: &Harness) -> &ChildThread {
    harness
        .app
        .child_live
        .by_agent(AGENT)
        .expect("the started child has a thread")
}

fn node_calls<'a>(cell: &'a Cell, id: &str) -> &'a [FoldedCall] {
    match cell {
        Cell::Node { node_id, row, .. } if node_id == id => &row.calls,
        _ => panic!("expected node row {id}"),
    }
}

/// A child's call outside any graph node is a top-level tool row, found
/// by agent id and by session id, that goes from running to ok.
#[test]
fn a_childs_top_level_call_goes_from_running_to_ok() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    child_tool(&mut harness, None, "call-t1", "fs_read");
    let rows = thread(&harness).rows();
    assert!(matches!(
        rows,
        [Cell::Tool { call_id, name, status: ToolStatus::Running { .. }, .. }]
            if call_id == "call-t1" && name == "fs_read"
    ));

    child_tool_done(&mut harness, None, "call-t1", true);
    let by_session = harness
        .app
        .child_live
        .by_session("sess_5d6e")
        .expect("the session id names the child");
    assert!(matches!(
        by_session.rows(),
        [Cell::Tool { status: ToolStatus::Ok { elapsed }, .. }]
            if *elapsed == Duration::from_millis(250)
    ));
}

/// Two nodes of one super-step interleave their calls; each call folds
/// under the node it ran inside, in node start order.
#[test]
fn interleaved_node_calls_fold_under_their_own_node() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    child_node(&mut harness, "left", ChildNodeState::Started);
    child_node(&mut harness, "right", ChildNodeState::Started);
    child_tool(&mut harness, Some("left"), "call-l1", "fs_read");
    child_tool(&mut harness, Some("right"), "call-r1", "fs_grep");
    child_tool(&mut harness, Some("left"), "call-l2", "fs_write");
    child_tool_done(&mut harness, Some("right"), "call-r1", false);
    child_tool_done(&mut harness, Some("left"), "call-l1", true);
    child_node(
        &mut harness,
        "left",
        ChildNodeState::Completed {
            duration_ms: Some(2000),
            route: Some("merge".to_string()),
        },
    );

    let rows = thread(&harness).rows();
    assert_eq!(rows.len(), 2);
    let left = node_calls(&rows[0], "left");
    let ids: Vec<&str> = left.iter().map(|call| call.call_id.as_str()).collect();
    assert_eq!(ids, ["call-l1", "call-l2"]);
    assert!(matches!(left[0].status, Some(ToolStatus::Ok { .. })));
    assert!(matches!(
        &left[1].status,
        Some(ToolStatus::Failed { error, .. }) if error == "node finished"
    ));
    let right = node_calls(&rows[1], "right");
    assert_eq!(right.len(), 1);
    assert!(matches!(
        &right[0].status,
        Some(ToolStatus::Failed { error, .. }) if error == "denied"
    ));
    assert!(matches!(
        &rows[0],
        Cell::Node { row, .. } if row.status == NodeStatus::Completed {
            elapsed: Some(Duration::from_secs(2)),
            route: Some("merge".to_string()),
        }
    ));
    assert!(matches!(
        &rows[1],
        Cell::Node {
            row: TaskRow {
                status: NodeStatus::Running { .. },
                ..
            },
            ..
        }
    ));
}

#[test]
fn a_failed_node_keeps_its_error() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    child_node(&mut harness, "build", ChildNodeState::Started);
    child_node(
        &mut harness,
        "build",
        ChildNodeState::Failed {
            error: "exit 3".to_string(),
        },
    );
    assert!(matches!(
        thread(&harness).rows(),
        [Cell::Node { row, .. }] if row.status == NodeStatus::Failed("exit 3".to_string())
    ));
}

/// A node that ends settles the calls under it whose finish never came,
/// with its error or that it finished; a call filed under a node that
/// already ended keeps the row running until it ends.
#[test]
fn a_node_that_ends_settles_its_running_calls() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    child_node(&mut harness, "plan", ChildNodeState::Started);
    child_tool(&mut harness, Some("plan"), "call-p1", "fs_read");
    child_node(&mut harness, "build", ChildNodeState::Started);
    child_tool(&mut harness, Some("build"), "call-b1", "fs_read");
    child_node(
        &mut harness,
        "plan",
        ChildNodeState::Completed {
            duration_ms: Some(10),
            route: None,
        },
    );
    child_node(
        &mut harness,
        "build",
        ChildNodeState::Failed {
            error: "exit 3".to_string(),
        },
    );
    let rows = thread(&harness).rows();
    for (row, id, said) in [
        (&rows[0], "plan", "node finished"),
        (&rows[1], "build", "exit 3"),
    ] {
        assert!(
            matches!(
                &node_calls(row, id)[0].status,
                Some(ToolStatus::Failed { error, .. }) if error == said
            ),
            "{id}"
        );
        assert!(!row.running(), "{id}");
    }

    child_tool(&mut harness, Some("plan"), "call-p2", "fs_read");
    assert!(thread(&harness).rows()[0].running());
    child_tool_done(&mut harness, Some("plan"), "call-p2", true);
    assert!(!thread(&harness).rows()[0].running());
}

/// A call that reports its node before the node's own start opens a row
/// for it; the start then fills that row in rather than adding another.
#[test]
fn a_nodes_start_after_its_first_call_fills_in_the_row_the_call_opened() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    child_tool(&mut harness, Some("plan"), "call-e1", "fs_read");
    child_node(&mut harness, "plan", ChildNodeState::Started);
    let rows = thread(&harness).rows();
    assert_eq!(rows.len(), 1, "the start added a second row");
    assert!(matches!(
        &rows[0],
        Cell::Node { node_type, row, .. } if node_type == "llm" && row.calls.len() == 1
    ));

    child_node(
        &mut harness,
        "plan",
        ChildNodeState::Completed {
            duration_ms: None,
            route: None,
        },
    );
    child_node(&mut harness, "plan", ChildNodeState::Started);
    assert_eq!(thread(&harness).rows().len(), 2, "a second visit has a row");
}

/// Calls with no id that run side by side are finished by their name and
/// arguments, the oldest running one first.
#[test]
fn a_call_with_no_id_finishes_the_oldest_running_call_like_it() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    let start = |harness: &mut Harness, name: &str| {
        harness.app.handle_msg(UiMsg::ChildToolStarted {
            agent_id: AGENT.to_string(),
            node: None,
            call_id: String::new(),
            name: name.to_string(),
            args: Some(serde_json::json!({"path": "src/lib.rs"})),
        });
    };
    start(&mut harness, "fs_read");
    start(&mut harness, "fs_grep");
    start(&mut harness, "fs_read");
    harness.app.handle_msg(UiMsg::ChildToolFinished {
        agent_id: AGENT.to_string(),
        node: None,
        call_id: String::new(),
        name: "fs_read".to_string(),
        args: Some(serde_json::json!({"path": "src/lib.rs"})),
        ok: true,
        error: None,
        duration_ms: Some(250),
    });
    let states: Vec<bool> = thread(&harness).rows().iter().map(Cell::running).collect();
    assert_eq!(states, [false, true, true]);
}

/// A child that ends with calls and nodes still running settles them
/// failed with why it ended, the calls with the time they ran; nothing
/// keeps the throbber turning after.
#[test]
#[serial]
fn a_child_that_ends_settles_what_it_left_running() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-child-ends-settled");
        for (outcome, said) in [
            (AgentOutcome::Interrupted, "interrupted"),
            (AgentOutcome::Failed("quota".to_string()), "quota"),
            (AgentOutcome::Failed(String::new()), "agent finished"),
            (AgentOutcome::Completed, "agent finished"),
        ] {
            // The roster lists the child, so its thread outlasts its end.
            let mut ctx = chat_ctx();
            let tasks = supervisor();
            tasks.write().register(finished_agent(AGENT)).unwrap();
            ctx.supervisor = Some(Arc::clone(&tasks));
            let mut harness = Harness::for_ctx(&ctx);
            harness.app.sync_from_ctx(&mut ctx);
            started(&mut harness);
            child_tool(&mut harness, None, "call-s1", "execute_command");
            child_node(&mut harness, "plan", ChildNodeState::Started);
            child_tool(&mut harness, Some("plan"), "call-s2", "fs_read");
            open_thread(&mut harness);
            harness.draw();
            harness.app.frame_drawn();
            assert!(harness.app.frames.due_at().is_some());

            harness.clock.advance(Duration::from_secs(3));
            harness.app.handle_msg(UiMsg::AgentFinished {
                agent_id: AGENT.to_string(),
                outcome: outcome.clone(),
            });
            let rows = thread(&harness).rows();
            assert!(
                matches!(
                    &rows[0],
                    Cell::Tool { status: ToolStatus::Failed { elapsed, error }, .. }
                        if *elapsed == Duration::from_secs(3) && error == said
                ),
                "{outcome:?}"
            );
            assert!(
                matches!(
                    &node_calls(&rows[1], "plan")[0].status,
                    Some(ToolStatus::Failed { error, .. }) if error == said
                ),
                "{outcome:?}"
            );
            assert!(
                matches!(
                    &rows[1],
                    Cell::Node { row, .. } if row.status == NodeStatus::Failed(said.to_string())
                ),
                "{outcome:?}"
            );
            let frame = harness.draw();
            assert!(frame.contains(&format!("3s  failed: {said}")), "{frame}");
            harness.app.frame_drawn();
            assert_eq!(harness.app.frames.due_at(), None, "{outcome:?}");
        }
    });
}

/// A child's output lands in the tail of the call it belongs to, folded
/// or not.
#[test]
fn a_childs_output_reaches_its_calls_tail() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    child_node(&mut harness, "plan", ChildNodeState::Started);
    child_tool(&mut harness, Some("plan"), "call-o1", "execute_command");
    harness.app.handle_msg(UiMsg::ChildToolOutput {
        agent_id: AGENT.to_string(),
        node: Some("plan".to_string()),
        call_id: "call-o1".to_string(),
        chunk: b"compiled\n".to_vec(),
    });
    let calls = node_calls(&thread(&harness).rows()[0], "plan");
    assert_eq!(calls[0].tail.lines, ["compiled"]);
    assert_eq!(
        harness.app.child_live.full_output_bytes(),
        "compiled\n".len()
    );
}

/// Events of an agent the UI never saw start, such as an agent node,
/// open no thread.
#[test]
fn events_of_an_unstarted_agent_open_no_thread() {
    let mut harness = Harness::standalone(80, 24);
    child_tool(&mut harness, None, "call-u1", "fs_read");
    child_node(&mut harness, "plan", ChildNodeState::Started);
    assert!(harness.app.child_live.by_agent(AGENT).is_none());
}

/// A finished child the roster no longer lists is forgotten; a running
/// one is kept although no roster lists it yet.
#[test]
fn a_finished_unlisted_child_is_forgotten() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    child_tool(&mut harness, None, "call-f1", "fs_read");
    harness.app.handle_msg(UiMsg::AgentStarted {
        agent_id: "agent_other_7a8b".to_string(),
        name: "other".to_string(),
        session_id: None,
        parent_agent_id: None,
        parent_session_id: None,
    });
    assert!(harness.app.child_live.by_agent(AGENT).is_some());

    harness.app.handle_msg(UiMsg::AgentFinished {
        agent_id: AGENT.to_string(),
        outcome: AgentOutcome::Completed,
    });
    assert!(harness.app.child_live.by_agent(AGENT).is_none());
    assert!(harness.app.child_live.by_session("sess_5d6e").is_none());
    assert!(
        harness
            .app
            .child_live
            .by_agent("agent_other_7a8b")
            .is_some()
    );
}

#[test]
fn a_child_keeps_only_its_newest_rows() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    for n in 0..=CHILD_ROW_LIMIT {
        child_tool(&mut harness, None, &format!("call-{n}"), "fs_read");
    }
    let rows = thread(&harness).rows();
    assert_eq!(rows.len(), CHILD_ROW_LIMIT);
    assert!(matches!(&rows[0], Cell::Tool { call_id, .. } if call_id == "call-1"));
}

/// Opens the started child's thread with no read of its session yet.
fn open_thread(harness: &mut Harness) {
    open_thread_of(harness, "sess_5d6e", "planner");
}

fn open_thread_of(harness: &mut Harness, session_id: &str, agent: &str) {
    harness.app.open_pager(Pager::child(
        session_id.to_string(),
        agent.to_string(),
        PathBuf::from(format!("/nonexistent/{session_id}.yaml")),
    ));
}

/// A call the child starts while its thread is open shows in the next
/// frame, with no flush of its session, and its finish turns it ok.
/// The throbber turns while the call runs, and the frames stop after.
#[test]
fn an_open_thread_shows_a_childs_call_at_once_and_its_finish() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    open_thread(&mut harness);
    harness.render();
    harness.app.frame_drawn();
    assert_eq!(harness.app.frames.due_at(), None);

    child_tool(&mut harness, None, "call-v1", "fs_read");
    assert!(harness.app.frames.due_at().is_some());
    let frame = harness.draw();
    assert!(
        frame.contains(&format!(
            "\u{25B8} fs_read({{\"path\":\"src/lib.rs\"}})  {} running 0s",
            SPINNER_FRAMES[0]
        )),
        "{frame}"
    );
    assert!(harness.app.take_control_requests().is_empty());
    harness.app.frame_drawn();
    assert_eq!(
        harness.app.frames.due_at(),
        Some(harness.clock.now() + THROBBER_INTERVAL)
    );
    harness.clock.advance(THROBBER_INTERVAL);
    let frame = harness.draw();
    assert!(
        frame.contains(&format!("  {} running 0s", SPINNER_FRAMES[1])),
        "{frame}"
    );

    child_tool_done(&mut harness, None, "call-v1", true);
    let frame = harness.draw();
    assert!(frame.contains("})  0s  ok"), "{frame}");
    harness.app.frame_drawn();
    assert_eq!(harness.app.frames.due_at(), None);
}

/// A thread on show from its end keeps its newest row in sight as the
/// child's calls pile up past the screen.
#[test]
fn an_open_thread_at_its_end_follows_new_calls() {
    let mut harness = Harness::standalone(80, 12);
    started(&mut harness);
    open_thread(&mut harness);
    harness.draw();
    for n in 0..20 {
        child_tool(
            &mut harness,
            None,
            &format!("call-{n}"),
            &format!("tool_{n:02}"),
        );
        harness.draw();
    }
    let frame = harness.draw();
    assert!(frame.contains("tool_19("), "{frame}");
    assert!(!frame.contains("tool_00("), "{frame}");
}

/// In a child's thread Ctrl+Up focuses the last foldable row, Enter and
/// a click open and close it; in the root thread the same keys do
/// nothing.
#[test]
fn the_fold_keys_work_in_a_child_thread_and_not_in_the_root_one() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    child_tool(&mut harness, None, "call-k1", "execute_command");
    harness.app.handle_msg(UiMsg::ChildToolOutput {
        agent_id: AGENT.to_string(),
        node: None,
        call_id: "call-k1".to_string(),
        chunk: b"compiled k1\n".to_vec(),
    });
    child_tool_done(&mut harness, None, "call-k1", true);
    open_thread(&mut harness);
    assert!(!harness.draw().contains("compiled k1"));

    harness
        .app
        .handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
    harness.press(KeyCode::Enter);
    assert!(harness.draw().contains("compiled k1"));
    harness
        .app
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL));
    harness.press(KeyCode::Enter);
    let frame = harness.draw();
    assert!(!frame.contains("compiled k1"), "{frame}");

    let row = screen_row(&harness, "\u{25B8} execute_command(");
    harness.app.handle_input(TermInput::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 4,
        row,
        modifiers: KeyModifiers::NONE,
    }));
    assert!(harness.draw().contains("compiled k1"));

    harness.press(KeyCode::Char('q'));
    harness.app.begin_turn("work", false);
    harness.app.handle_msg(UiMsg::ToolStarted {
        call_id: "call-root".to_string(),
        name: "fs_read".to_string(),
        args: None,
    });
    press_ctrl(&mut harness, 't');
    let before = harness.draw();
    harness
        .app
        .handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
    harness.press(KeyCode::Enter);
    assert_eq!(harness.draw(), before);
    assert!(!harness.app.transcript.focused());
}

/// The whole output a child's row lays out when opened is freed once
/// the pager closes, and once another thread takes the child's place.
#[test]
fn leaving_a_child_thread_frees_what_its_rows_laid_out() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    child_tool(&mut harness, None, "call-r1", "execute_command");
    harness.app.handle_msg(UiMsg::ChildToolOutput {
        agent_id: AGENT.to_string(),
        node: None,
        call_id: "call-r1".to_string(),
        chunk: b"compiled r1\n".to_vec(),
    });
    child_tool_done(&mut harness, None, "call-r1", true);
    let laid_out = |harness: &mut Harness| {
        let thread = harness
            .app
            .child_live
            .by_session_mut("sess_5d6e")
            .expect("the started child has a thread");
        cells::any_full_output_laid_out(thread.rows_mut())
    };
    open_thread(&mut harness);
    harness
        .app
        .handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
    harness.press(KeyCode::Enter);
    assert!(harness.draw().contains("compiled r1"));
    assert!(laid_out(&mut harness));
    harness.press(KeyCode::Char('q'));
    assert!(harness.app.pager().is_none());
    assert!(!laid_out(&mut harness));

    open_thread(&mut harness);
    assert!(harness.draw().contains("compiled r1"), "the row closed");
    assert!(laid_out(&mut harness));
    harness.app.open_pager(Pager::root());
    assert!(!laid_out(&mut harness));
}

/// With no turn running and a child-session root that does not list it,
/// the thread picker still lists a finished child the roster keeps,
/// known from its start.
#[test]
#[serial]
fn the_picker_lists_a_finished_child_known_only_from_its_start() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-picker-live-child");
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(ChildSessionRoot::Proc { pid: 1, start: 16 }));
        let tasks = supervisor();
        tasks.write().register(finished_agent(AGENT)).unwrap();
        ctx.supervisor = Some(Arc::clone(&tasks));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        started(&mut harness);
        harness.app.handle_msg(UiMsg::AgentFinished {
            agent_id: AGENT.to_string(),
            outcome: AgentOutcome::Completed,
        });

        press_ctrl(&mut harness, 't');
        harness.press(KeyCode::Tab);
        let frame = harness.draw();
        assert!(frame.contains("planner  completed"), "{frame}");
        assert!(frame.contains("sess_5d6e"), "{frame}");
        assert!(!frame.contains("No child sessions yet"), "{frame}");
    });
}

/// With no turn running, `t` in the tasks pop-up and `.threads` both open
/// the thread picker straight away, listing a finished child. Leaving the
/// picker opened from the pop-up goes back through the transcript view to
/// the pop-up.
#[test]
#[serial]
fn t_in_the_tasks_popup_and_dot_threads_open_the_picker_with_a_finished_child() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-picker-direct");
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(ChildSessionRoot::Proc { pid: 1, start: 17 }));
        let tasks = supervisor();
        tasks.write().register(finished_agent(AGENT)).unwrap();
        ctx.supervisor = Some(Arc::clone(&tasks));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        started(&mut harness);
        harness.app.handle_msg(UiMsg::AgentFinished {
            agent_id: AGENT.to_string(),
            outcome: AgentOutcome::Completed,
        });

        harness.app.handle_key(alt(KeyCode::Char('t')));
        harness.draw();
        harness.press(KeyCode::Char('t'));
        let frame = harness.draw();
        assert!(frame.contains("Enter open  Esc back"), "{frame}");
        assert!(frame.contains("planner  completed"), "{frame}");
        harness.press(KeyCode::Esc);
        assert!(harness.app.pager().is_some_and(Pager::shows_root));
        harness.press(KeyCode::Char('q'));
        assert!(
            harness.app.roster().is_some(),
            "the pop-up did not come back"
        );
        harness.press(KeyCode::Esc);
        assert!(harness.app.overlay.is_none());

        harness.type_text(".threads");
        assert_eq!(harness.press(KeyCode::Enter), None);
        assert_eq!(harness.app.composer_text(), "");
        let frame = harness.draw();
        assert!(frame.contains("Enter open  Esc back"), "{frame}");
        assert!(frame.contains("planner  completed"), "{frame}");
    });
}

/// With no child-session root there is no file to open a child from, so
/// the picker does not offer one known only from its start.
#[test]
fn without_a_child_root_the_picker_leaves_out_a_child_known_only_from_its_start() {
    let mut harness = Harness::standalone(80, 24);
    started(&mut harness);
    press_ctrl(&mut harness, 't');
    harness.press(KeyCode::Tab);
    let frame = harness.draw();
    assert!(!frame.contains("sess_5d6e"), "{frame}");
    assert!(frame.contains("No child sessions yet"), "{frame}");
}

fn agent_rows(harness: &Harness) -> Vec<&str> {
    harness
        .app
        .transcript
        .cells()
        .iter()
        .filter_map(|cell| match cell {
            Cell::Agent { agent_id, .. } => Some(agent_id.as_str()),
            _ => None,
        })
        .collect()
}

/// A grandchild's and a great-grandchild's calls show live in their own
/// threads and turn ok when they finish; only the direct child has a row
/// in the main transcript, and a deeper agent's end leaves it alone.
#[test]
fn nested_agents_stream_their_calls_into_their_own_threads() {
    let mut harness = Harness::standalone(80, 24);
    started_nested(&mut harness);
    assert_eq!(agent_rows(&harness), [AGENT]);
    assert_eq!(
        harness.app.child_live.ancestry(GREAT_GRANDCHILD),
        [AGENT, GRANDCHILD]
    );
    assert!(harness.app.child_live.ancestry(AGENT).is_empty());

    for (agent_id, session_id, name) in [
        (GRANDCHILD, "sess_9c0d", "coder"),
        (GREAT_GRANDCHILD, "sess_3f4a", "tester"),
    ] {
        open_thread_of(&mut harness, session_id, name);
        harness.render();
        harness.app.frame_drawn();
        child_tool_of(&mut harness, agent_id, None, "call-n1", "fs_read");
        assert!(harness.app.frames.due_at().is_some(), "{agent_id}");
        let frame = harness.draw();
        assert!(
            SPINNER_FRAMES
                .iter()
                .any(|spinner| frame.contains(&format!("}})  {spinner} running 0s"))),
            "{agent_id}: {frame}"
        );
        child_tool_done_of(&mut harness, agent_id, None, "call-n1", true);
        let frame = harness.draw();
        assert!(frame.contains("})  0s  ok"), "{agent_id}: {frame}");
        harness.press(KeyCode::Char('q'));
        assert!(harness.app.pager().is_none(), "{agent_id}");
    }

    harness.app.handle_msg(UiMsg::AgentFinished {
        agent_id: GREAT_GRANDCHILD.to_string(),
        outcome: AgentOutcome::Completed,
    });
    assert_eq!(agent_rows(&harness), [AGENT]);
    assert!(matches!(
        harness
            .app
            .transcript
            .cells()
            .iter()
            .find(|cell| matches!(cell, Cell::Agent { .. })),
        Some(Cell::Agent {
            row: TaskRow {
                status: AgentStatus::Running { .. },
                ..
            },
            ..
        })
    ));
}

/// A grandchild's thread opened, closed and opened again, each time with
/// its saved session read back, draws its call once.
#[test]
#[serial]
fn a_reopened_grandchild_thread_draws_its_call_once() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-grandchild-reopened");
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(ChildSessionRoot::Proc { pid: 1, start: 19 }));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        started_nested(&mut harness);
        child_tool_of(&mut harness, GRANDCHILD, None, "call-g1", "fs_read");
        child_tool_done_of(&mut harness, GRANDCHILD, None, "call-g1", true);

        for pass in 0..2 {
            harness
                .app
                .open_child("sess_9c0d".to_string(), "coder".to_string());
            let Some(ControlRequest::LoadChild {
                session_id,
                generation,
                ..
            }) = harness.app.take_control_requests().pop()
            else {
                panic!("opening the grandchild asks for a read");
            };
            harness.app.handle_msg(UiMsg::ChildLoaded {
                session_id,
                generation,
                result: Ok(vec![ChildEntry::Tool {
                    call_id: "call-g1".to_string(),
                    name: "fs_read".to_string(),
                    args: "{\"path\":\"src/lib.rs\"}".to_string(),
                    output: "fn main() {}\n".to_string(),
                }]),
            });
            let frame = harness.draw();
            assert_eq!(frame.matches("fs_read(").count(), 1, "pass {pass}: {frame}");
            harness.press(KeyCode::Char('q'));
        }
    });
}

/// With a child root that lists none of them, the picker nests a live
/// grandchild under its parent, ahead of the parent's later sibling.
#[test]
#[serial]
fn the_picker_nests_a_live_grandchild_under_its_parent() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-picker-grandchild");
        let mut ctx = chat_ctx();
        ctx.child_session_root = Some(Arc::new(ChildSessionRoot::Proc { pid: 1, start: 20 }));
        let mut harness = Harness::for_ctx(&ctx);
        harness.app.sync_from_ctx(&mut ctx);
        started(&mut harness);
        started_as(&mut harness, "agent_other_7a8b", "other", "sess_7a8b", None);
        started_as(
            &mut harness,
            GRANDCHILD,
            "coder",
            "sess_9c0d",
            Some((AGENT, "sess_5d6e")),
        );

        press_ctrl(&mut harness, 't');
        harness.press(KeyCode::Tab);
        let frame = harness.draw();
        let at = |label: &str| {
            frame
                .find(label)
                .unwrap_or_else(|| panic!("no {label} row: {frame}"))
        };
        assert!(
            at("\u{25BE} planner  running") < at("      coder  running"),
            "{frame}"
        );
        assert!(
            at("      coder  running") < at("    other  running"),
            "{frame}"
        );
    });
}

fn nested_live() -> ChildLive {
    let mut live = ChildLive::default();
    for (agent_id, session_id, parent) in [
        (AGENT, "sess_5d6e", None),
        (GRANDCHILD, "sess_9c0d", Some((AGENT, "sess_5d6e"))),
        (
            GREAT_GRANDCHILD,
            "sess_3f4a",
            Some((GRANDCHILD, "sess_9c0d")),
        ),
    ] {
        live.agent_started(
            agent_id.to_string(),
            String::new(),
            Some(session_id.to_string()),
            parent.map(|(agent, _): (&str, &str)| agent.to_string()),
            parent.map(|(_, session): (&str, &str)| session.to_string()),
            chrono::Utc::now(),
        );
    }
    live
}

/// A finished deeper agent outlives the roster's list while an ancestor
/// runs or is listed, and goes once neither holds.
#[test]
fn a_finished_grandchild_lives_while_an_ancestor_is_running_or_listed() {
    let mut live = nested_live();
    let now = Instant::now();
    for agent_id in [GREAT_GRANDCHILD, GRANDCHILD] {
        live.agent_finished(agent_id, &AgentOutcome::Completed, now);
    }
    live.retain_listed(|_| false);
    assert!(live.by_agent(GREAT_GRANDCHILD).is_some());

    live.agent_finished(AGENT, &AgentOutcome::Completed, now);
    live.retain_listed(|id| id == AGENT);
    for agent_id in [AGENT, GRANDCHILD, GREAT_GRANDCHILD] {
        assert!(live.by_agent(agent_id).is_some(), "{agent_id}");
    }

    live.retain_listed(|_| false);
    for agent_id in [AGENT, GRANDCHILD, GREAT_GRANDCHILD] {
        assert!(live.by_agent(agent_id).is_none(), "{agent_id}");
    }
    assert!(live.by_session("sess_9c0d").is_none());
}

/// The parent links read back by agent and by session, and a loop in
/// them does not hang the ancestry walk.
#[test]
fn ancestry_follows_parents_and_stops_at_a_loop() {
    let mut live = nested_live();
    assert_eq!(live.parent(GREAT_GRANDCHILD), Some(GRANDCHILD));
    assert_eq!(live.parent_of_session("sess_3f4a"), Some("sess_9c0d"));
    assert_eq!(live.parent_of_session("sess_5d6e"), None);

    live.agent_started(
        AGENT.to_string(),
        String::new(),
        Some("sess_5d6e".to_string()),
        Some(GREAT_GRANDCHILD.to_string()),
        Some("sess_3f4a".to_string()),
        chrono::Utc::now(),
    );
    assert_eq!(live.ancestry(GREAT_GRANDCHILD), [AGENT, GRANDCHILD]);
}

/// A harness with a child root and the nested children started, the
/// threads pop-up open on the root thread.
fn nested_picker(start: u64) -> Harness {
    let mut ctx = chat_ctx();
    ctx.child_session_root = Some(Arc::new(ChildSessionRoot::Proc { pid: 1, start }));
    let mut harness = Harness::for_ctx(&ctx);
    harness.app.sync_from_ctx(&mut ctx);
    started_nested(&mut harness);
    press_ctrl(&mut harness, 't');
    harness.press(KeyCode::Tab);
    harness
}

fn title(harness: &mut Harness) -> String {
    harness
        .draw()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Right and l step down to a thread three levels deep, which Enter
/// opens under its path. Left and h step back up and fold, Space folds
/// and unfolds, and the folds outlast the pop-up closing and the thread
/// changing; Enter on a hidden thread opens the folded one hiding it.
#[test]
#[serial]
fn the_thread_popup_keys_fold_step_and_open_a_thread_three_levels_down() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-thread-tree-keys");
        let mut harness = nested_picker(21);
        harness.press(KeyCode::Right);
        harness.press(KeyCode::Char('l'));
        harness.press(KeyCode::Right);
        harness.press(KeyCode::Char('l'));
        harness.press(KeyCode::Enter);
        assert_eq!(
            harness.app.pager().and_then(Pager::child_session),
            Some("sess_3f4a")
        );
        let top = title(&mut harness);
        assert!(
            top.contains("Thread: main > planner > coder > tester (read-only) sess_3f4a"),
            "{top}"
        );

        harness.press(KeyCode::Tab);
        harness.press(KeyCode::Char('h'));
        harness.press(KeyCode::Left);
        let frame = harness.draw();
        assert!(frame.contains("\u{25B8} coder"), "{frame}");
        assert!(!frame.contains("tester  running"), "{frame}");
        harness.press(KeyCode::Char('h'));
        harness.press(KeyCode::Char(' '));
        let frame = harness.draw();
        assert!(frame.contains("\u{25B8} planner"), "{frame}");
        assert!(!frame.contains("coder  running"), "{frame}");
        harness.press(KeyCode::Char(' '));
        harness.press(KeyCode::Esc);
        harness.press(KeyCode::Tab);
        let frame = harness.draw();
        assert!(frame.contains("\u{25BE} planner"), "{frame}");
        assert!(frame.contains("\u{25B8} coder"), "{frame}");
        assert!(!frame.contains("tester  running"), "{frame}");

        harness.press(KeyCode::Enter);
        assert_eq!(
            harness.app.pager().and_then(Pager::child_session),
            Some("sess_9c0d")
        );
        let top = title(&mut harness);
        assert!(
            top.contains("Thread: main > planner > coder (read-only)"),
            "{top}"
        );
        harness.press(KeyCode::Tab);
        let frame = harness.draw();
        assert!(frame.contains("\u{25B8} coder"), "{frame}");
    });
}

/// Where `needle` starts on screen.
fn screen_at(harness: &Harness, needle: &str) -> (u16, u16) {
    let buffer = harness.terminal.backend().buffer();
    for y in 0..buffer.area.height {
        let symbols: Vec<&str> = (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect();
        let text: String = symbols.concat();
        if let Some(at) = text.find(needle) {
            let x = symbols
                .iter()
                .scan(0, |end, symbol| {
                    *end += symbol.len();
                    Some(*end)
                })
                .position(|end| end > at)
                .unwrap();
            return (u16::try_from(x).unwrap(), y);
        }
    }
    panic!("{needle:?} is not on screen:\n{buffer:?}");
}

fn click(harness: &mut Harness, (column, row): (u16, u16)) {
    harness.app.handle_input(TermInput::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }));
}

/// A click on a thread's fold arrow folds it; a click on a row's name
/// highlights the row, which Enter then opens.
#[test]
#[serial]
fn a_click_folds_at_the_arrow_and_highlights_elsewhere_on_the_row() {
    run_async(async {
        let _guard = TestConfigDirGuard::new("tui-thread-tree-clicks");
        let mut harness = nested_picker(22);
        harness.draw();
        let arrow = screen_at(&harness, "\u{25BE} coder");
        click(&mut harness, arrow);
        let frame = harness.draw();
        assert!(frame.contains("\u{25B8} coder"), "{frame}");
        assert!(!frame.contains("tester  running"), "{frame}");
        let (column, row) = screen_at(&harness, "coder  running");
        click(&mut harness, (column + 2, row));
        let frame = harness.draw();
        assert!(
            frame.contains("\u{25B8} coder"),
            "a click on the name folded: {frame}"
        );
        harness.press(KeyCode::Enter);
        assert_eq!(
            harness.app.pager().and_then(Pager::child_session),
            Some("sess_9c0d")
        );
    });
}
