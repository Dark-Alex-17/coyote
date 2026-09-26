use super::RequestContext;
use crate::mesh::snapshot::{BriefState, MeshSnapshot, PlanRef, RepoInfo, SessionInfo, TurnState};

use std::env;
use std::path::PathBuf;
use std::time::SystemTime;

impl RequestContext {
    /// Copies out everything the mesh may serve about this session. Owned data only, so the
    /// result outlives the caller's lock on the context. Plans are looked up under the repo
    /// root when one is found, so a turn run from a subdirectory still sees `plans/`.
    pub fn mesh_snapshot(&self, state: TurnState) -> MeshSnapshot {
        self.mesh_snapshot_at(env::current_dir().unwrap_or_default(), state)
    }

    /// `mesh_snapshot` as seen from `cwd`.
    pub fn mesh_snapshot_at(&self, cwd: PathBuf, state: TurnState) -> MeshSnapshot {
        let repo = RepoInfo::discover(&cwd);
        let plan_root = repo.as_ref().map(|r| r.root.as_path()).unwrap_or(&cwd);
        let goal = self.todo_list.goal.trim();
        let brief = self.app.mesh.brief();
        MeshSnapshot {
            objective: (!goal.is_empty()).then(|| goal.to_string()),
            state,
            plan: PlanRef::discover(plan_root),
            repo,
            todo: self.todo_list.clone(),
            brief: BriefState {
                mode: self.app.config.mesh.brief,
                text: brief.as_ref().map(|brief| brief.text.clone()),
                digest_generated_at: brief.as_ref().and_then(|brief| brief.digest_generated_at),
            },
            cwd,
            captured_at: SystemTime::now(),
            session: SessionInfo {
                name: self.session.as_ref().map(|s| s.name().to_string()),
                model: self.current_model().id(),
                role: self
                    .session
                    .as_ref()
                    .and_then(|s| s.role_name())
                    .map(str::to_string)
                    .or_else(|| self.agent.as_ref().map(|a| a.name().to_string()))
                    .or_else(|| self.role.as_ref().map(|r| r.name().to_string())),
            },
        }
    }
}

pub fn publish_mesh_snapshot(ctx: &RequestContext, state: TurnState) {
    ctx.app.mesh.publish(ctx.mesh_snapshot(state));
}

/// Re-captures the session data without changing what the turn is doing: for progress ticks
/// inside a turn, where only the outer boundary may say idle.
pub fn refresh_mesh_snapshot(ctx: &RequestContext) {
    let state = ctx
        .app
        .mesh
        .snapshot()
        .map(|snapshot| snapshot.state)
        .unwrap_or_else(TurnState::working_now);
    publish_mesh_snapshot(ctx, state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Agent, AgentConfig, AppState, Role, Session, WorkingMode};
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
        assert_eq!(snap.session.model, ctx.current_model().id());
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
    fn capture_at_a_directory_finds_its_repo_branch_and_active_plan() {
        let tmp = crate::mesh::test_support::TempDir::new("mesh-snapshot-at");
        let root = tmp.path.join("root");
        write(&root.join(".git/HEAD"), "ref: refs/heads/main\n");
        write(
            &root.join("plans/PLAN-x.md"),
            "---\nstatus: active\ntitle: Probe\n---\n",
        );
        let cwd = root.join("sub/dir");
        fs::create_dir_all(&cwd).unwrap();
        let ctx = create_test_ctx();

        let snap = ctx.mesh_snapshot_at(cwd.clone(), TurnState::idle_now());

        assert_eq!(snap.cwd, cwd);
        let repo = snap.repo.unwrap();
        assert_eq!(repo.root, root);
        assert_eq!(repo.branch.as_deref(), Some("main"));
        assert_eq!(snap.plan.unwrap().title, "Probe");
    }

    #[test]
    fn capture_outside_a_repo_still_finds_the_plan_under_cwd() {
        let tmp = crate::mesh::test_support::TempDir::new("mesh-snapshot-no-repo");
        let cwd = tmp.path.clone();
        write(
            &cwd.join("plans/PLAN-x.md"),
            "---\nstatus: active\ntitle: Loose\n---\n",
        );
        let ctx = create_test_ctx();

        let snap = ctx.mesh_snapshot_at(cwd, TurnState::idle_now());

        assert!(
            RepoInfo::discover(&tmp.path).is_none(),
            "temp dir {:?} lies inside a git checkout; cannot test the no-repo branch",
            tmp.path
        );
        assert!(snap.repo.is_none());
        assert_eq!(snap.plan.unwrap().title, "Loose");
    }

    #[test]
    fn session_name_and_role_come_from_the_session_when_set() {
        let mut ctx = create_test_ctx();
        ctx.role = Some(Role::new("stale", "prompt"));
        let mut session = Session::default();
        session.set_name("my-session".into());
        session.set_role(Role::new("session-role", "prompt"));
        ctx.session = Some(session);

        let snap = ctx.mesh_snapshot(TurnState::idle_now());

        assert_eq!(snap.session.name.as_deref(), Some("my-session"));
        assert_eq!(snap.session.role.as_deref(), Some("session-role"));
    }

    #[test]
    fn role_prefers_the_active_agent_over_a_stale_role() {
        let mut ctx = create_test_ctx();
        ctx.role = Some(Role::new("stale", "prompt"));
        ctx.agent = Some(Agent::test_new(AgentConfig {
            name: "the-agent".to_string(),
            ..AgentConfig::default()
        }));

        let snap = ctx.mesh_snapshot(TurnState::idle_now());

        assert_eq!(snap.session.name, None);
        assert_eq!(snap.session.role.as_deref(), Some("the-agent"));
    }

    #[test]
    fn role_falls_back_to_the_standalone_role() {
        let mut ctx = create_test_ctx();
        ctx.role = Some(Role::new("myrole", "prompt"));
        let snap = ctx.mesh_snapshot(TurnState::idle_now());
        assert_eq!(snap.session.role.as_deref(), Some("myrole"));
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
        let run_repl = ["run_repl_", "command("].concat();
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
