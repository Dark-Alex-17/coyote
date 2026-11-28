use super::RequestContext;
use crate::mesh::card::CardSource;
use crate::mesh::snapshot::{BriefState, MeshSnapshot, RepoInfo, TurnState};

use std::env;
use std::path::PathBuf;
use std::time::SystemTime;
use tokio::time::Instant;

impl RequestContext {
    /// Copies out everything the mesh may serve about this session. Owned data only, so the
    /// result outlives the caller's lock on the context.
    pub fn mesh_snapshot(&self, state: TurnState) -> MeshSnapshot {
        self.mesh_snapshot_at(env::current_dir().unwrap_or_default(), state)
    }

    /// `mesh_snapshot` as seen from `cwd`.
    pub fn mesh_snapshot_at(&self, cwd: PathBuf, state: TurnState) -> MeshSnapshot {
        let repo = RepoInfo::discover(&cwd);
        let goal = self.todo_list.goal.trim();
        let brief = self.app.mesh.brief();
        MeshSnapshot {
            objective: (!goal.is_empty()).then(|| goal.to_string()),
            state,
            repo,
            todo: self.todo_list.clone(),
            brief: BriefState {
                mode: self.app.config.mesh.brief,
                text: brief.as_ref().map(|brief| brief.text.clone()),
                digest_generated_at: brief.as_ref().and_then(|brief| brief.digest_generated_at),
            },
            session: self.app.mesh.shared_session(),
            cwd,
            captured_at: SystemTime::now(),
        }
    }

    /// Whether this context is the session the mesh describes, so its progress may be
    /// published. Admitted: the REPL's root context, a top-level agent (graph and
    /// architect agents included), the branches a top-level graph run forks (their todo
    /// list is a copy of the root's), and a non-isolated macro, which runs on the outer
    /// context. Refused: an envoy run and every spawned or graph-node child agent (depth
    /// above zero, an agent id of their own), and an isolated macro's context, whose
    /// steps are published from the outer context instead.
    pub(crate) fn owns_mesh_session(&self) -> bool {
        self.current_depth == 0
            && self.self_agent_id.is_none()
            && (!self.macro_flag || self.macro_non_isolated)
    }
}

pub fn publish_mesh_snapshot(ctx: &RequestContext, state: TurnState) {
    ctx.app.mesh.publish(ctx.mesh_snapshot(state));
}

/// Re-captures the session data without changing what the turn is doing: for progress ticks
/// inside a turn, where only the outer boundary may say idle.
pub fn refresh_mesh_snapshot(ctx: &RequestContext) {
    publish_mesh_snapshot(ctx, published_turn_state(ctx));
}

fn published_turn_state(ctx: &RequestContext) -> TurnState {
    ctx.app
        .mesh
        .snapshot()
        .map(|snapshot| snapshot.state)
        .unwrap_or_else(TurnState::working_now)
}

/// A progress point inside a turn: a tool batch has completed, which is where the todo
/// tools, `agent__collect` and `job__collect` run. Refreshes the published snapshot at
/// most once per `MESH_PROGRESS_MIN_INTERVAL`. A point inside the interval defers its
/// snapshot to the slot's timer instead, so the last change before a long model call is
/// published when the interval ends. Either way the context is then offered to the digest
/// hook, whose own interval bounds how often that costs a model call.
/// Nothing happens while no mesh node is running, for a context that does not own the
/// session, or before a turn boundary has published the first snapshot: until then there
/// is no turn state to keep.
/// Works from the context it is handed and the slot alone, never the turn's lock.
pub(crate) fn mesh_progress_point(ctx: &RequestContext) {
    mesh_progress_point_at(ctx, Instant::now());
}

fn mesh_progress_point_at(ctx: &RequestContext, now: Instant) {
    if !ctx.app.mesh.takes_progress_points()
        || !ctx.owns_mesh_session()
        || ctx.app.mesh.snapshot().is_none()
    {
        return;
    }
    if ctx.app.mesh.progress_capture_due_at(now) {
        refresh_mesh_snapshot(ctx);
    } else {
        ctx.app
            .mesh
            .defer_progress(ctx.mesh_snapshot(published_turn_state(ctx)));
    }
    if let Some(hook) = ctx.app.mesh.digest_hook() {
        hook.on_progress(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppState, WorkingMode};
    use crate::mesh::MeshSlot;
    use crate::mesh::brief::Digest;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn create_test_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn capture_maps_the_context_fields() {
        let mut ctx = create_test_ctx();
        ctx.todo_list.goal = "ship it".into();
        ctx.todo_list.add("write the seam");
        let state = TurnState::working_now();

        let snap = ctx.mesh_snapshot(state);

        assert_eq!(snap.objective.as_deref(), Some("ship it"));
        assert_eq!(snap.todo, ctx.todo_list);
        assert_eq!(snap.state, state);
        assert_eq!(snap.brief.mode, ctx.app.config.mesh.brief);
        assert_eq!(snap.brief.text, None);
        assert_eq!(
            snap.age(snap.captured_at + Duration::from_secs(3)),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn blank_goal_means_no_objective() {
        let mut ctx = create_test_ctx();
        ctx.todo_list.goal = "   ".into();
        assert_eq!(ctx.mesh_snapshot(TurnState::idle_now()).objective, None);
    }

    #[test]
    fn capture_at_a_directory_finds_its_repo_branch() {
        let tmp = crate::mesh::test_support::TempDir::new("mesh-snapshot-at");
        let root = tmp.path.join("root");
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        let cwd = root.join("sub/dir");
        fs::create_dir_all(&cwd).unwrap();
        let ctx = create_test_ctx();

        let snap = ctx.mesh_snapshot_at(cwd.clone(), TurnState::idle_now());

        assert_eq!(snap.cwd, cwd);
        let repo = snap.repo.unwrap();
        assert_eq!(repo.root, root);
        assert_eq!(repo.branch.as_deref(), Some("main"));
    }

    #[test]
    fn capture_outside_a_repo_reports_no_repo() {
        let tmp = crate::mesh::test_support::TempDir::new("mesh-snapshot-no-repo");
        let cwd = tmp.path.clone();
        let ctx = create_test_ctx();

        let snap = ctx.mesh_snapshot_at(cwd, TurnState::idle_now());

        assert!(
            RepoInfo::discover(&tmp.path).is_none(),
            "temp dir {:?} lies inside a git checkout; cannot test the no-repo branch",
            tmp.path
        );
        assert!(snap.repo.is_none());
    }

    #[test]
    fn publish_stores_into_the_shared_slot() {
        let ctx = create_test_ctx();
        assert!(ctx.app.mesh.snapshot().is_none());
        let state = TurnState::idle_now();
        publish_mesh_snapshot(&ctx, state);
        assert_eq!(ctx.app.mesh.snapshot().unwrap().state, state);
    }

    #[test]
    fn refresh_keeps_the_published_turn_state() {
        let mut ctx = create_test_ctx();
        let working = TurnState::working_now();
        publish_mesh_snapshot(&ctx, working);
        ctx.todo_list.goal = "new goal".into();

        refresh_mesh_snapshot(&ctx);

        let snap = ctx.app.mesh.snapshot().unwrap();
        assert_eq!(snap.state, working);
        assert_eq!(snap.objective.as_deref(), Some("new goal"));
    }

    #[test]
    fn refresh_without_a_snapshot_reports_working() {
        let ctx = create_test_ctx();
        assert!(ctx.app.mesh.snapshot().is_none());
        refresh_mesh_snapshot(&ctx);
        let snap = ctx.app.mesh.snapshot().unwrap();
        assert!(matches!(snap.state, TurnState::Working { .. }));
    }

    #[test]
    fn publish_folds_in_the_current_brief_text() {
        let ctx = create_test_ctx();
        publish_mesh_snapshot(&ctx, TurnState::idle_now());
        ctx.app.mesh.set_user_brief(Some("hello".into()));
        ctx.app.mesh.publish_digest(Some(Digest {
            text: "- Working on the widget".into(),
            generated_at: SystemTime::now() - Duration::from_secs(30),
            covered_messages: 4,
        }));
        let live = ctx.app.mesh.brief().unwrap();
        assert!(live.text.contains("hello"), "{}", live.text);
        assert!(live.digest_generated_at.is_some());
        publish_mesh_snapshot(&ctx, TurnState::idle_now());
        let snap = ctx.app.mesh.snapshot().unwrap();
        assert_eq!(snap.brief.text.as_deref(), Some(live.text.as_str()));
        assert_eq!(snap.brief.digest_generated_at, live.digest_generated_at);
    }

    /// Spawns a thread that takes and holds the context write lock, the way the REPL does
    /// for a whole turn. Returns once the lock is held; dropping the sender releases it.
    fn hold_write_lock(
        ctx: &Arc<parking_lot::RwLock<RequestContext>>,
    ) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let holder = {
            let ctx = Arc::clone(ctx);
            thread::spawn(move || {
                let _guard = ctx.write();
                locked_tx.send(()).unwrap();
                let _ = release_rx.recv();
            })
        };
        locked_rx.recv().unwrap();
        (release_tx, holder)
    }

    #[test]
    fn harness_holds_the_ctx_write_lock_until_released() {
        let ctx = Arc::new(parking_lot::RwLock::new(create_test_ctx()));
        let (release, holder) = hold_write_lock(&ctx);
        assert!(ctx.try_read().is_none());
        drop(release);
        holder.join().unwrap();
        assert!(ctx.try_read().is_some());
    }

    #[test]
    fn snapshot_read_completes_while_a_turn_holds_the_ctx_write_lock() {
        let ctx = create_test_ctx();
        publish_mesh_snapshot(&ctx, TurnState::idle_now());
        let slot: Arc<MeshSlot> = Arc::clone(&ctx.app.mesh);
        let ctx = Arc::new(parking_lot::RwLock::new(ctx));
        let (release, holder) = hold_write_lock(&ctx);

        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            let snap = slot.snapshot();
            done_tx.send(snap.map(|s| s.captured_at)).unwrap();
        });
        let read = done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("snapshot read must not wait on the ctx lock");

        assert!(read.is_some());
        assert!(
            ctx.try_read().is_none(),
            "the write lock must still be held when the read completed"
        );
        drop(release);
        holder.join().unwrap();
    }

    fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// The lines before the file's `#[cfg(test)] mod tests` block, or all of them. The test
    /// region is assumed to start at the first exact `#[cfg(test)]` line immediately followed
    /// by a line starting with `mod tests`.
    fn production_lines(source: &str) -> Vec<&str> {
        let lines: Vec<&str> = source.lines().collect();
        let test_start = lines
            .windows(2)
            .position(|pair| pair[0] == "#[cfg(test)]" && pair[1].starts_with("mod tests"));
        lines[..test_start.unwrap_or(lines.len())].to_vec()
    }

    /// `line` with any `//` comment removed, so commented-out calls never match.
    fn code(line: &str) -> &str {
        line.split("//").next().unwrap_or("")
    }

    /// Indices of the lines calling `site`: the `fn` definition, `use` lines and anything
    /// after `//` do not count.
    fn call_sites(lines: &[&str], site: &str) -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                let code = code(line);
                code.contains(site)
                    && !code.contains(&["fn ", site].concat())
                    && !code.trim_start().starts_with("use ")
            })
            .map(|(idx, _)| idx)
            .collect()
    }

    /// Whether any of `needles` appears in the `window` lines after `idx`, comments aside.
    fn followed_by(lines: &[&str], idx: usize, needles: &[&str], window: usize) -> bool {
        let end = (idx + 1 + window).min(lines.len());
        lines[idx + 1..end]
            .iter()
            .any(|l| needles.iter().any(|needle| code(l).contains(needle)))
    }

    fn assert_followed_by(
        path: &Path,
        lines: &[&str],
        idx: usize,
        needles: &[&str],
        window: usize,
    ) {
        assert!(
            followed_by(lines, idx, needles, window),
            "{}:{}: {} is not followed by any of {needles:?} within {window} lines",
            path.display(),
            idx + 1,
            lines[idx].trim()
        );
    }

    fn assert_preceded_by(path: &Path, lines: &[&str], idx: usize, needle: &str, window: usize) {
        let start = idx.saturating_sub(window);
        assert!(
            lines[start..idx].iter().any(|l| code(l).contains(needle)),
            "{}:{}: {} is not preceded by {needle} within {window} lines",
            path.display(),
            idx + 1,
            lines[idx].trim()
        );
    }

    /// The publish needles name the two functions that store into the slot, so a bare
    /// capture (`mesh_snapshot(` or `mesh_snapshot_at(`) after a turn does not count.
    fn publish_needles() -> [String; 2] {
        [
            ["publish_mesh_", "snapshot("].concat(),
            ["refresh_mesh_", "snapshot("].concat(),
        ]
    }

    #[test]
    fn a_bare_capture_after_a_turn_is_not_a_publish() {
        let needles = publish_needles();
        let needles: Vec<&str> = needles.iter().map(String::as_str).collect();
        let turn = ["run_repl_", "command(&mut ctx, signal, &line).await;"].concat();
        for (follower, publishes) in [
            ("ctx.mesh_snapshot(state);", false),
            ("ctx.mesh_snapshot_at(cwd, state);", false),
            ("publish_mesh_snapshot(&ctx, state);", true),
            ("refresh_mesh_snapshot(&ctx);", true),
            ("// refresh_mesh_snapshot(&ctx);", false),
            ("foo(); // refresh_mesh_snapshot(&ctx);", false),
        ] {
            let lines = [turn.as_str(), follower];
            assert_eq!(followed_by(&lines, 0, &needles, 6), publishes, "{follower}");
        }
    }

    #[test]
    fn every_turn_site_publishes_a_snapshot_afterwards() {
        // Assembled at runtime so this test's own text does not match the probes.
        // The bare needle covers both the wrapper that reads the configured prefix and the
        // form that takes one.
        let run_repl = ["run_repl_", "command"].concat();
        let run_repl_wrapper = ["fn ", &run_repl, "("].concat();
        let any_publish = publish_needles();
        let any_publish: Vec<&str> = any_publish.iter().map(String::as_str).collect();
        let working = ["working_", "now()"].concat();
        let idle = ["idle_", "now()"].concat();

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources = Vec::new();
        rust_sources(&src, &mut sources);
        let main_rs = src.join("main.rs");
        let repl_rs = src.join("repl/mod.rs");
        let acp_rs = src.join("acp/server.rs");
        for file in [&main_rs, &repl_rs, &acp_rs] {
            assert!(sources.contains(file), "{} not scanned", file.display());
        }

        // Outer turn boundaries: each brackets its turn with working before and idle after.
        let outer_sites = [
            (
                &repl_rs,
                ["run_repl_", "command(&mut ctx, self.abort_signal"].concat(),
                1,
            ),
            (&main_rs, ["macro_", "execute(&mut ctx"].concat(), 1),
            (&main_rs, ["shell_", "execute(&mut ctx"].concat(), 1),
            (&main_rs, ["start_", "directive(&mut ctx"].concat(), 1),
            (&acp_rs, ["run_prompt_", "turn(ctx"].concat(), 1),
        ];

        let mut inner_sites = 0;
        for path in &sources {
            let source = String::from_utf8_lossy(&fs::read(path).unwrap()).into_owned();
            let lines = production_lines(&source);
            for idx in call_sites(&lines, &run_repl) {
                // The wrapper forwards to the form that takes a prefix and returns its
                // result; the turn site is whoever called the wrapper.
                let start = idx.saturating_sub(8);
                if lines[start..idx]
                    .iter()
                    .any(|l| code(l).contains(&run_repl_wrapper))
                {
                    continue;
                }
                inner_sites += 1;
                assert_followed_by(path, &lines, idx, &any_publish, 6);
            }
            for (file, site, expected) in &outer_sites {
                if path != *file {
                    continue;
                }
                let sites = call_sites(&lines, site);
                assert_eq!(
                    sites.len(),
                    *expected,
                    "{}: expected {expected} site(s) of {site}",
                    path.display()
                );
                for idx in sites {
                    assert_preceded_by(path, &lines, idx, &working, 6);
                    assert_followed_by(path, &lines, idx, &[&idle], 8);
                }
            }
        }
        assert!(
            inner_sites >= 3,
            "found only {inner_sites} REPL command sites"
        );
    }
}

#[cfg(test)]
mod progress_point_tests {
    use super::*;
    use crate::config::{AppConfig, AppState, WorkingMode};
    use crate::function::{ToolCall, eval_tool_calls};
    use crate::mesh::{MESH_PROGRESS_MIN_INTERVAL, MeshSlot};
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    /// A root context that may use the todo tools, on a slot that takes progress points
    /// with no node running.
    fn todo_ctx() -> RequestContext {
        todo_ctx_on(MeshSlot::without_node_gate())
    }

    fn todo_ctx_on(mesh: MeshSlot) -> RequestContext {
        let app = AppState {
            config: Arc::new(AppConfig {
                auto_continue: true,
                ..Default::default()
            }),
            mesh: Arc::new(mesh),
            ..AppState::test_default()
        };
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Repl);
        ctx.tool_scope.functions.append_todo_functions();
        ctx
    }

    async fn add_todo_batch(ctx: &mut RequestContext, task: &str) {
        let call = ToolCall::new(
            "todo__add".into(),
            json!({ "task": task }),
            Some(format!("call-{task}")),
        );
        eval_tool_calls(ctx, vec![call]).await.unwrap();
    }

    fn published(ctx: &RequestContext) -> Arc<MeshSnapshot> {
        ctx.app.mesh.snapshot().expect("a snapshot is published")
    }

    #[test]
    fn captures_are_at_least_the_interval_apart() {
        let mut ctx = todo_ctx();
        publish_mesh_snapshot(&ctx, TurnState::working_now());
        let start = Instant::now();

        ctx.todo_list.add("first");
        mesh_progress_point_at(&ctx, start);
        assert_eq!(published(&ctx).todo.todos.len(), 1);

        ctx.todo_list.add("second");
        mesh_progress_point_at(
            &ctx,
            start + MESH_PROGRESS_MIN_INTERVAL - Duration::from_millis(1),
        );
        assert_eq!(published(&ctx).todo.todos.len(), 1, "too soon to capture");

        mesh_progress_point_at(&ctx, start + MESH_PROGRESS_MIN_INTERVAL);
        assert_eq!(published(&ctx).todo.todos.len(), 2);
    }

    #[test]
    fn a_capture_keeps_the_published_turn_state() {
        let mut ctx = todo_ctx();
        let working = TurnState::working_now();
        publish_mesh_snapshot(&ctx, working);
        ctx.todo_list.add("first");

        mesh_progress_point_at(&ctx, Instant::now());

        assert_eq!(published(&ctx).state, working);
    }

    #[test]
    fn nothing_is_published_before_the_first_turn_boundary() {
        let ctx = todo_ctx();
        mesh_progress_point_at(&ctx, Instant::now());
        assert!(ctx.app.mesh.snapshot().is_none());
    }

    #[tokio::test]
    async fn a_batch_with_no_node_leaves_the_slot_alone() {
        let mut ctx = todo_ctx_on(MeshSlot::default());
        publish_mesh_snapshot(&ctx, TurnState::working_now());
        let boundary = published(&ctx);

        add_todo_batch(&mut ctx, "first").await;
        add_todo_batch(&mut ctx, "second").await;

        assert!(Arc::ptr_eq(&published(&ctx), &boundary));
        assert!(ctx.app.mesh.pending_progress_for_tests().is_none());
        assert!(
            ctx.app.mesh.progress_capture_due_at(Instant::now()),
            "a batch claimed the progress clock"
        );
        assert_eq!(ctx.todo_list.todos.len(), 2);
    }

    #[tokio::test]
    async fn a_tight_tool_loop_publishes_once_and_defers_the_rest() {
        let mut ctx = todo_ctx();
        publish_mesh_snapshot(&ctx, TurnState::working_now());
        let mut last = published(&ctx);
        let mut captures = 0;

        for n in 0..10 {
            add_todo_batch(&mut ctx, &format!("task-{n}")).await;
            let now = published(&ctx);
            if !Arc::ptr_eq(&now, &last) {
                captures += 1;
            }
            last = now;
        }

        assert_eq!(captures, 1);
        assert_eq!(
            last.todo.todos.len(),
            1,
            "only the first batch was captured"
        );
        assert_eq!(ctx.todo_list.todos.len(), 10);
        let pending = ctx
            .app
            .mesh
            .pending_progress_for_tests()
            .expect("the throttled batches left a snapshot for the timer");
        assert_eq!(
            pending.todo.todos.len(),
            10,
            "the timer publishes the latest"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_throttled_change_is_published_when_the_interval_ends() {
        let mut ctx = todo_ctx();
        publish_mesh_snapshot(&ctx, TurnState::working_now());
        ctx.todo_list.add("first");
        mesh_progress_point_at(&ctx, Instant::now());
        let first = published(&ctx);
        assert_eq!(first.todo.todos.len(), 1);

        tokio::time::advance(Duration::from_secs(2)).await;
        ctx.todo_list.add("second");
        mesh_progress_point_at(&ctx, Instant::now());
        assert!(Arc::ptr_eq(&published(&ctx), &first), "too soon to publish");

        tokio::time::advance(MESH_PROGRESS_MIN_INTERVAL - Duration::from_secs(2)).await;
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        let trailing = published(&ctx);
        assert_eq!(trailing.todo.todos.len(), 2, "no further batch was needed");
        assert_eq!(trailing.state, first.state);
        assert!(ctx.app.mesh.pending_progress_for_tests().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_turn_boundary_supersedes_a_deferred_snapshot() {
        let mut ctx = todo_ctx();
        publish_mesh_snapshot(&ctx, TurnState::working_now());
        mesh_progress_point_at(&ctx, Instant::now());
        ctx.todo_list.add("first");
        mesh_progress_point_at(&ctx, Instant::now());
        assert!(ctx.app.mesh.pending_progress_for_tests().is_some());

        let idle = TurnState::idle_now();
        publish_mesh_snapshot(&ctx, idle);
        assert!(ctx.app.mesh.pending_progress_for_tests().is_none());
        let boundary = published(&ctx);

        tokio::time::advance(MESH_PROGRESS_MIN_INTERVAL * 2).await;
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        assert!(Arc::ptr_eq(&published(&ctx), &boundary));
        assert_eq!(boundary.state, idle);
    }

    #[test]
    fn only_the_owning_session_publishes_progress() {
        let root = todo_ctx();
        assert!(root.owns_mesh_session(), "the REPL root");

        let mut non_isolated_macro = todo_ctx();
        non_isolated_macro.macro_flag = true;
        non_isolated_macro.macro_non_isolated = true;
        assert!(
            non_isolated_macro.owns_mesh_session(),
            "a non-isolated macro"
        );

        let mut envoy = todo_ctx();
        envoy.current_depth = 1;
        envoy.self_agent_id = Some("envoy-1".into());
        assert!(!envoy.owns_mesh_session(), "an envoy run");

        let mut child = todo_ctx();
        child.current_depth = 2;
        child.self_agent_id = Some("child-1".into());
        assert!(!child.owns_mesh_session(), "a spawned child");

        let mut isolated_macro = todo_ctx();
        isolated_macro.macro_flag = true;
        assert!(!isolated_macro.owns_mesh_session(), "an isolated macro");
    }

    #[test]
    fn a_forked_branch_publishes_the_roots_todo_list() {
        let mut root = todo_ctx();
        root.todo_list.add("the root's task");

        let branch = root.fork_for_branch();

        assert!(branch.owns_mesh_session());
        assert!(Arc::ptr_eq(&branch.app.mesh, &root.app.mesh));
        // Todo tools are refused in graph llm nodes, so a branch never edits its copy.
        assert_eq!(branch.todo_list, root.todo_list);
    }

    #[tokio::test]
    async fn a_real_child_context_never_reaches_the_roots_slot() {
        let mut root = todo_ctx();
        root.todo_list.add("the root's task");
        publish_mesh_snapshot(&root, TurnState::working_now());
        let before = published(&root);
        let mut child = RequestContext::new_for_child(
            crate::function::agents::child_app_state(&root.app),
            &root,
            1,
            Arc::new(crate::supervisor::mailbox::Inbox::new()),
            "child-1".into(),
        );
        child.tool_scope.functions.append_todo_functions();
        assert!(!child.owns_mesh_session());
        assert!(!Arc::ptr_eq(&child.app.mesh, &root.app.mesh));

        add_todo_batch(&mut child, "the child's task").await;

        assert_eq!(child.todo_list.todos.len(), 1, "the child ran its batch");
        assert!(Arc::ptr_eq(&published(&root), &before));
        assert!(child.app.mesh.snapshot().is_none());
    }

    #[tokio::test]
    async fn an_envoy_or_child_batch_on_the_shared_slot_leaves_the_snapshot_alone() {
        let mut root = todo_ctx();
        root.todo_list.add("the root's task");
        publish_mesh_snapshot(&root, TurnState::working_now());
        let before = published(&root);

        for (depth, agent_id) in [(1, "envoy-1"), (2, "child-1")] {
            let mut other = RequestContext::new(Arc::clone(&root.app), WorkingMode::Cmd);
            other.tool_scope.functions.append_todo_functions();
            other.current_depth = depth;
            other.self_agent_id = Some(agent_id.into());
            assert!(Arc::ptr_eq(&other.app.mesh, &root.app.mesh));

            add_todo_batch(&mut other, agent_id).await;

            assert_eq!(other.todo_list.todos.len(), 1, "{agent_id} ran its batch");
            let after = published(&root);
            assert!(Arc::ptr_eq(&after, &before), "{agent_id} published");
            assert_eq!(after.todo, before.todo);
            assert_eq!(after.captured_at, before.captured_at);
        }
    }
}
