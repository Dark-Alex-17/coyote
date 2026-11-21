use super::*;
use crate::tui::cells::{FoldedCall, NodeStatus};
use crate::tui::child_live::{CHILD_ROW_LIMIT, ChildThread};
use crate::tui::sink::ChildNodeState;

const AGENT: &str = "agent_graph_5d6e";

fn started(harness: &mut Harness) {
    harness.app.handle_msg(UiMsg::AgentStarted {
        agent_id: AGENT.to_string(),
        name: "planner".to_string(),
        session_id: Some("sess_5d6e".to_string()),
    });
}

fn child_tool(harness: &mut Harness, node: Option<&str>, call_id: &str, name: &str) {
    harness.app.handle_msg(UiMsg::ChildToolStarted {
        agent_id: AGENT.to_string(),
        node: node.map(str::to_string),
        call_id: call_id.to_string(),
        name: name.to_string(),
        args: Some(serde_json::json!({"path": "src/lib.rs"})),
    });
}

fn child_tool_done(harness: &mut Harness, node: Option<&str>, call_id: &str, ok: bool) {
    harness.app.handle_msg(UiMsg::ChildToolFinished {
        agent_id: AGENT.to_string(),
        node: node.map(str::to_string),
        call_id: call_id.to_string(),
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
    assert!(matches!(left[1].status, Some(ToolStatus::Running { .. })));
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

/// Events of an agent the UI never saw start (a grandchild, an agent
/// node) open no thread.
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
    harness.app.open_pager(Pager::child(
        "sess_5d6e".to_string(),
        "planner".to_string(),
        PathBuf::from("/nonexistent/sess_5d6e.yaml"),
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
        assert!(frame.contains("planner  sess_5d6e  finished"), "{frame}");
        assert!(!frame.contains("No child sessions yet"), "{frame}");
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
