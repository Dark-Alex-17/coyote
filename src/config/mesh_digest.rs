//! The shareable digest: a model-written summary of the whole session that the brief hands
//! to trusted peers in place of the transcript. It borrows the compression path's chunking
//! and fold-forward helpers but never its entry point, since compression leaves the last
//! turns unsummarised and a digest must cover every one of them.
//!
//! Lock rule: `mesh_digest_request` runs under the caller's context guard and copies out
//! the messages and settings the model calls need; `run_mesh_digest` then chunks, renders
//! and calls with no guard at all. The transcript enters the model request and nothing
//! else: not the digest, not a log line.

use super::request_context::{
    SUMMARIZATION_CHUNK_BUDGET_RATIO, SUMMARIZATION_WINDOW_FALLBACK_TOKENS,
    compose_summarization_request, render_summarization_chunk, slice_summarization_chunks,
};
use super::{Input, RequestContext, RoleLike, SUMMARY_CONTEXT_PROMPT, Session};
use crate::client::{Message, MessageRole, Model, ModelType};
use crate::config::mesh_config::MeshBrief;
use crate::mesh::MeshSlot;
use crate::mesh::brief::{DIGEST_MAX_CHARS, Digest, sanitize_block};

use anyhow::{Result, anyhow};
use log::{debug, warn};
use parking_lot::RwLock;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};
use tokio::task::JoinHandle;

/// Session messages that must have arrived since the last digest before a new one is
/// generated; fewer and the turn boundary spends no tokens.
pub(crate) const MESH_DIGEST_MIN_NEW_MESSAGES: usize = 4;
/// Floor between two generation attempts, measured from the last attempt whether or not
/// it succeeded, so a failing model is not retried at every turn.
pub(crate) const MESH_DIGEST_MIN_INTERVAL: Duration = Duration::from_secs(60);
/// Ceiling on one model call of the digest; a call past it fails that generation.
pub(crate) const MESH_DIGEST_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Everything one digest generation needs, copied out of the session context so the model
/// calls can run without it. `messages` is the transcript still in the session and
/// `prior_summary` the recap a compression left in its place; both go nowhere but into
/// the requests. `session_model` estimates tokens for the `chunk_budget` slicing.
/// `template` is a detached input on the session model; `model`, when set, is the
/// configured brief model to try first.
pub(crate) struct DigestRequest {
    pub messages: Vec<Message>,
    pub prior_summary: String,
    pub session_model: Model,
    pub chunk_budget: usize,
    pub template: Input,
    pub model: Option<Model>,
    pub prompt: String,
    pub covered_messages: usize,
}

impl RequestContext {
    /// Copies every message of the session (`foldable_messages(0)`: only a leading system
    /// message is skipped) together with the recap of anything compression has already
    /// folded away. `None` when there is no session or it has no user messages yet.
    /// Synchronous, so the caller's guard is held for the copy alone.
    pub(crate) fn mesh_digest_request(&self) -> Option<DigestRequest> {
        let session = self
            .session
            .as_ref()
            .filter(|session| session.has_user_messages())?;
        let model = resolve_digest_model(self);
        let window = model
            .as_ref()
            .and_then(|model| model.max_input_tokens())
            .or(session.model().max_input_tokens())
            .unwrap_or(SUMMARIZATION_WINDOW_FALLBACK_TOKENS);
        let chunk_budget = (window as f32 * SUMMARIZATION_CHUNK_BUDGET_RATIO) as usize;
        Some(DigestRequest {
            messages: session.foldable_messages(0).to_vec(),
            prior_summary: prior_summary(
                session,
                self.app.config.summary_context_prompt.as_deref(),
            ),
            session_model: session.model().clone(),
            chunk_budget,
            template: Input::detached_text(self, ""),
            model,
            prompt: self.app.config.mesh.digest_prompt().to_string(),
            covered_messages: digest_message_count(session),
        })
    }
}

/// The recap compression left as the session's leading system message, with the original
/// system prompt and todo prefix ahead of the recap marker dropped. The configured
/// `summary_context_prompt` is tried first, then the code-owned default, since a saved
/// session keeps the recap written under whatever the marker was at the time. The system
/// prompt compression carried into the message is skipped before the search, so a marker
/// quoted inside it cannot seed the prompt's tail. Empty for a session that was never
/// compressed, and empty when no marker is found: the message would then carry the
/// system prompt and todo list, which are not the recap.
fn prior_summary(session: &Session, configured_marker: Option<&str>) -> String {
    if session.compressed_messages().is_empty() {
        return String::new();
    }
    let Some(text) = session
        .messages()
        .first()
        .filter(|message| message.role == MessageRole::System)
        .map(|message| message.content.to_text())
    else {
        return String::new();
    };
    let skip = session
        .compressed_system_prompt()
        .filter(|prefix| !prefix.is_empty() && text.starts_with(prefix))
        .map_or(0, str::len);
    let tail = &text[skip..];
    let markers = configured_marker
        .filter(|marker| !marker.is_empty())
        .into_iter()
        .chain([SUMMARY_CONTEXT_PROMPT]);
    for marker in markers {
        if let Some(at) = tail.find(marker) {
            return tail[at + marker.len()..].to_string();
        }
    }
    debug!("Mesh digest: compression recap marker not found; digesting the kept turns only");
    String::new()
}

/// How many messages a digest of `session` covers. Compression never lowers this sum (it
/// can raise it by one, since the leading system message it retires is counted once
/// archived), so only a history edit reads as a shrink.
fn digest_message_count(session: &Session) -> usize {
    session.compressed_messages().len() + session.foldable_messages(0).len()
}

/// The configured brief model when it differs from the session model and resolves;
/// `None` means the session model does the digest. A configured model that cannot be
/// resolved is logged and degraded to `None`, never a failed digest.
pub(crate) fn resolve_digest_model(ctx: &RequestContext) -> Option<Model> {
    let current = ctx.current_model().id();
    let model_id = ctx.brief_model().filter(|id| *id != current)?;
    match Model::retrieve_model(ctx.app.config.as_ref(), &model_id, ModelType::Chat) {
        Ok(model) => Some(model),
        Err(err) => {
            warn!(
                "Mesh brief model '{model_id}' could not be used ({err:#}); falling back to the session model '{current}' for the digest"
            );
            None
        }
    }
}

/// Chunks the messages as compression would and folds them forward, oldest first, each
/// call carrying the summary of everything before it, starting from the prior recap. A
/// configured model that fails a step is logged and that step is retried on the session
/// model. The whole digest is regenerated every time: covering every message means
/// re-folding them all. Fails when the model's answer sanitises to nothing.
pub(crate) async fn run_mesh_digest<F, Fut>(request: DigestRequest, fetch: F) -> Result<Digest>
where
    F: Fn(Input) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let DigestRequest {
        messages,
        prior_summary,
        session_model,
        chunk_budget,
        template,
        model,
        prompt,
        covered_messages,
    } = request;
    let chunks: Vec<String> = slice_summarization_chunks(&session_model, &messages, chunk_budget)
        .into_iter()
        .map(render_summarization_chunk)
        .collect();
    let mut summary = prior_summary;
    for chunk in &chunks {
        let mut input = template.clone();
        input.set_text(compose_summarization_request(&prompt, &summary, chunk));
        summary = match model.as_ref() {
            Some(model) => {
                let mut routed = input.clone();
                routed.set_role_model(model.clone());
                match fetch(routed).await {
                    Ok(summary) => summary,
                    Err(err) => {
                        warn!(
                            "Mesh brief model '{}' failed: {err:#}; falling back to the session model '{}' for this digest step",
                            model.id(),
                            template.role().model().id()
                        );
                        fetch(input).await?
                    }
                }
            }
            None => fetch(input).await?,
        };
    }
    let text = sanitize_block(&summary, DIGEST_MAX_CHARS)
        .ok_or_else(|| anyhow!("The model returned an empty digest"))?;
    Ok(Digest {
        text,
        generated_at: SystemTime::now(),
        covered_messages,
    })
}

async fn timed_fetch(input: Input) -> Result<String> {
    tokio::time::timeout(MESH_DIGEST_REQUEST_TIMEOUT, input.fetch_chat_text())
        .await
        .map_err(|_| {
            anyhow!(
                "Mesh digest LLM call timed out after {} s",
                MESH_DIGEST_REQUEST_TIMEOUT.as_secs()
            )
        })?
}

/// Clears the in-flight flag when the generation task ends, however it ends: an aborted
/// task drops its guard too, so the next boundary is never locked out.
struct InFlightGuard(Arc<AtomicBool>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// What a digest is bound to. The session name alone does not do: every unsaved session
/// is named `temp` and two agents may each have a session of the same name, so the agent
/// and the session's mesh instance id (minted when the node is installed, re-minted on
/// fork) are part of it. Two sessions that share a name and have no id yet are told apart
/// by nothing here.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionFingerprint {
    agent: Option<String>,
    session: Option<String>,
    mesh_instance_id: Option<String>,
}

impl SessionFingerprint {
    fn of(ctx: &RequestContext) -> Self {
        let session = ctx.session.as_ref();
        Self {
            agent: ctx.agent.as_ref().map(|agent| agent.name().to_string()),
            session: session.map(|session| session.name().to_string()),
            mesh_instance_id: session
                .and_then(Session::mesh_instance_id)
                .map(str::to_string),
        }
    }
}

/// Decides at each REPL turn boundary whether the digest is worth regenerating and runs
/// the generation on its own task. Held by the REPL, which is the only caller: headless
/// runs and the ACP server publish snapshots but never digest. One generation runs at a
/// time; the task handle is kept so exit can abort it rather than orphan a model call.
/// Each generation also carries the digest epoch it started under, since an abort cannot
/// reach a task that is past its last await: the slot refuses a digest from a closed epoch.
/// `pending_covered` is the message count the running generation covers, so a history
/// edit made while it runs reads as a shrink against it and not only against the digest
/// already served.
/// The REPL also reports each turn's session through `observe_session` under its own
/// guard, since the boundary's `try_read` is skipped while the idle driver holds the
/// context and a session switch must not slip past the fence.
pub(crate) struct MeshDigestDriver {
    in_flight: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
    pending_covered: Option<usize>,
    last_attempt: Option<Instant>,
    last_session: Option<SessionFingerprint>,
}

impl MeshDigestDriver {
    pub(crate) fn new() -> Self {
        Self {
            in_flight: Arc::new(AtomicBool::new(false)),
            task: None,
            pending_covered: None,
            last_attempt: None,
            last_session: None,
        }
    }

    /// Production entry, called at the REPL Idle turn boundary with no context guard held.
    pub(crate) fn maybe_refresh(&mut self, ctx: &Arc<RwLock<RequestContext>>) {
        self.maybe_refresh_with(ctx, Instant::now(), timed_fetch);
    }

    /// Fences the digest at a session boundary: when the agent or session differs from
    /// the one last seen, the generation in flight is aborted and the digest cleared
    /// under a new epoch. Takes the context by reference so the caller can hold whatever
    /// guard it already has. Spawns nothing.
    pub(crate) fn observe_session(&mut self, ctx: &RequestContext) {
        let fingerprint = SessionFingerprint::of(ctx);
        if self.last_session.as_ref() == Some(&fingerprint) {
            return;
        }
        self.abort_in_flight();
        ctx.app.mesh.clear_digest_for_new_epoch();
        self.last_session = Some(fingerprint);
    }

    /// `maybe_refresh` with the model call and the clock injectable. The decision runs
    /// under one short `try_read`; a context that is busy means this boundary is skipped,
    /// not waited for. A session change, a brief mode that does not use a digest, an
    /// emptied session and a transcript shorter than the digest covers each abort the
    /// generation in flight and clear the digest at once, ahead of every gate, so a digest
    /// is never served stale or for another session. Generation is then spawned only when
    /// the mesh is on, the brief is `auto`, the transcript grew by
    /// `MESH_DIGEST_MIN_NEW_MESSAGES` over the served digest (or shrank below what the
    /// served or the pending digest covers, after a history edit),
    /// `MESH_DIGEST_MIN_INTERVAL` has passed since the last attempt and nothing is in
    /// flight.
    pub(crate) fn maybe_refresh_with<F, Fut>(
        &mut self,
        ctx: &Arc<RwLock<RequestContext>>,
        now: Instant,
        fetch: F,
    ) where
        F: Fn(Input) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send,
    {
        let Some((request, mesh, epoch, guard)) = self.plan(ctx, now) else {
            return;
        };
        self.last_attempt = Some(now);
        self.pending_covered = Some(request.covered_messages);
        self.task = Some(tokio::spawn(async move {
            let _guard = guard;
            match run_mesh_digest(request, fetch).await {
                Ok(digest) => {
                    if !mesh.publish_digest_at(epoch, digest) {
                        debug!(
                            "Mesh digest discarded: the session changed while it was being generated"
                        );
                    }
                }
                Err(err) => warn!("Mesh digest generation failed: {err:#}"),
            }
        }));
    }

    fn plan(
        &mut self,
        ctx: &Arc<RwLock<RequestContext>>,
        now: Instant,
    ) -> Option<(DigestRequest, Arc<MeshSlot>, u64, InFlightGuard)> {
        let Some(ctx) = ctx.try_read() else {
            debug!("Mesh digest skipped this turn boundary: the session context is busy");
            return None;
        };
        let app = &ctx.app;
        if !app.config.mesh.enabled {
            return None;
        }
        self.observe_session(&ctx);
        if app.config.mesh.brief != MeshBrief::Auto {
            self.clear(&app.mesh);
            return None;
        }
        let len = ctx.session.as_ref().map(digest_message_count).unwrap_or(0);
        let covered = app
            .mesh
            .digest()
            .map(|digest| digest.covered_messages)
            .unwrap_or(0);
        if len == 0 {
            self.clear(&app.mesh);
            return None;
        }
        let grew = len >= covered + MESH_DIGEST_MIN_NEW_MESSAGES;
        let shrunk = len < covered.max(self.pending_covered.unwrap_or(0));
        if shrunk {
            self.clear(&app.mesh);
        }
        if !grew && !shrunk {
            return None;
        }
        if self
            .last_attempt
            .is_some_and(|last| now.duration_since(last) < MESH_DIGEST_MIN_INTERVAL)
        {
            return None;
        }
        if self
            .in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        let guard = InFlightGuard(Arc::clone(&self.in_flight));
        let request = ctx.mesh_digest_request()?;
        let epoch = app.mesh.digest_epoch();
        Some((request, Arc::clone(&app.mesh), epoch, guard))
    }

    /// Drops the served digest and the generation in flight under a new epoch, so
    /// neither the digest nor a late result of that generation is served. With nothing
    /// to drop the epoch is left alone.
    fn clear(&mut self, mesh: &MeshSlot) {
        if self.task.is_none() && mesh.digest().is_none() {
            return;
        }
        self.abort_in_flight();
        mesh.clear_digest_for_new_epoch();
    }

    /// The aborted task clears the flag it was spawned with when the runtime drops it;
    /// a fresh flag lets the next boundary spawn without waiting for that.
    fn abort_in_flight(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.pending_covered = None;
        self.in_flight = Arc::new(AtomicBool::new(false));
    }

    /// Aborts any generation in flight and waits for its task to finish, so the REPL exits
    /// with no model call still running.
    pub(crate) async fn shutdown(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

/// The synchronous part of `shutdown`, so an early return from the REPL that never reaches
/// it still aborts the generation rather than orphan a model call. Aborting a finished task
/// changes nothing.
impl Drop for MeshDigestDriver {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ModelData;
    use crate::config::mesh_config::{MESH_DIGEST_PROMPT, MeshConfig};
    use crate::config::mesh_snapshot::publish_mesh_snapshot;
    use crate::config::{AppConfig, AppState, Role, Session, WorkingMode};
    use crate::mesh::brief::digest_objective_for;
    use crate::mesh::card::{CardSource, StatusHandler, build_card};
    use crate::mesh::snapshot::TurnState;
    use crate::testing::{install_log_collector, warn_snapshot};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::Weak;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::Notify;

    const FIXED_DIGEST: &str = "- Working on the widget\n- Decided on X";

    fn test_ctx(mode: MeshBrief) -> RequestContext {
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            compression_keep_last: 2,
            mesh: MeshConfig {
                enabled: true,
                brief: mode,
                ..MeshConfig::default()
            },
            ..AppConfig::default()
        });
        let mut ctx = RequestContext::new(Arc::new(app), WorkingMode::Cmd);
        ctx.session = Some(Session::default());
        ctx
    }

    fn add_turns_as(ctx: &mut RequestContext, role: Role, texts: &[&str]) {
        for text in texts {
            let input = Input::from_str(ctx, text, Some(role.clone())).unwrap();
            ctx.session
                .as_mut()
                .unwrap()
                .add_message(&input, "reply")
                .unwrap();
        }
    }

    fn add_turns(ctx: &mut RequestContext, texts: &[&str]) {
        add_turns_as(ctx, Role::new("", ""), texts);
    }

    fn turns(count: usize) -> Vec<String> {
        (0..count).map(|i| format!("turn-{i:02}")).collect()
    }

    fn add_n_turns(ctx: &mut RequestContext, count: usize) {
        let texts = turns(count);
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        add_turns(ctx, &texts);
    }

    fn named_model(name: &str) -> Model {
        Model::from_config("openai", &[ModelData::new(name)]).remove(0)
    }

    /// Stands in for the model: records every request text, counts calls, optionally
    /// waits at `gate` before answering, fails the first call when `fail_first_call` is
    /// set, and otherwise answers `reply`.
    #[derive(Default)]
    struct FakeModel {
        requests: parking_lot::Mutex<Vec<String>>,
        calls: AtomicUsize,
        gate: Option<Arc<Notify>>,
        fail_first_call: bool,
    }

    type BoxedReply = Pin<Box<dyn Future<Output = Result<String>> + Send>>;

    impl FakeModel {
        fn gated() -> (Arc<Self>, Arc<Notify>) {
            let gate = Arc::new(Notify::new());
            let model = Arc::new(Self {
                gate: Some(Arc::clone(&gate)),
                ..Self::default()
            });
            (model, gate)
        }

        fn failing_first() -> Arc<Self> {
            Arc::new(Self {
                fail_first_call: true,
                ..Self::default()
            })
        }

        fn fetch(
            self: &Arc<Self>,
            reply: &'static str,
        ) -> impl Fn(Input) -> BoxedReply + Clone + Send + Sync + 'static {
            let model = Arc::clone(self);
            move |input: Input| {
                let model = Arc::clone(&model);
                Box::pin(async move {
                    model.requests.lock().push(input.text());
                    let call = model.calls.fetch_add(1, Ordering::SeqCst);
                    if let Some(gate) = &model.gate {
                        gate.notified().await;
                    }
                    if model.fail_first_call && call == 0 {
                        return Err(anyhow!("the model is down"));
                    }
                    Ok(reply.to_string())
                })
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().clone()
        }
    }

    async fn wait_for_task(driver: &mut MeshDigestDriver) {
        driver
            .task
            .take()
            .expect("a generation was spawned")
            .await
            .unwrap();
    }

    async fn wait_for_call(model: &FakeModel, expected: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while model.calls() < expected {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the fake model was never called");
    }

    /// Waits for the task spawned with `in_flight` to release it, however that task ended.
    async fn wait_until_cleared(in_flight: &AtomicBool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while in_flight.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the in-flight flag was never released");
    }

    fn message_count(ctx: &Arc<RwLock<RequestContext>>) -> usize {
        digest_message_count(ctx.read().session.as_ref().unwrap())
    }

    #[tokio::test]
    async fn digest_covers_the_most_recent_turn() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_turns(
            &mut ctx,
            &[
                "turn-00",
                "turn-01",
                "turn-02",
                "turn-03",
                "turn-04",
                "turn-05 MARKER-LAST-TURN-9f3a",
            ],
        );
        let session_len = ctx.session.as_ref().unwrap().foldable_messages(0).len();
        assert_eq!(ctx.compression_keep_last(), 2);
        assert!(
            !ctx.session
                .as_ref()
                .unwrap()
                .foldable_messages(ctx.compression_keep_last())
                .iter()
                .any(|m| m.content.to_text().contains("MARKER-LAST-TURN-9f3a")),
            "compression would leave the marker out; the digest must not"
        );
        let model = Arc::new(FakeModel::default());

        let request = ctx.mesh_digest_request().unwrap();
        assert_eq!(request.covered_messages, session_len);
        assert_eq!(request.messages.len(), session_len);
        assert!(request.prior_summary.is_empty());
        let digest = run_mesh_digest(request, model.fetch(FIXED_DIGEST))
            .await
            .unwrap();

        let requests = model.requests();
        assert!(!requests.is_empty());
        assert!(
            requests
                .iter()
                .any(|text| text.contains("MARKER-LAST-TURN-9f3a")),
            "{requests:?}"
        );
        assert!(requests[0].contains("turn-00"));
        assert_eq!(digest.covered_messages, session_len);
        assert_eq!(digest.text, FIXED_DIGEST);
    }

    #[test]
    fn digest_module_never_calls_the_compression_entry_point() {
        let source = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/config/mesh_digest.rs"
        ))
        .unwrap();
        // Assembled at runtime so this test's own text does not match the probes.
        let needles = [
            ["compress_session", "_impl("].concat(),
            ["compress_", "session("].concat(),
        ];
        for needle in &needles {
            assert!(
                !source.contains(needle),
                "mesh_digest.rs must not call {needle}"
            );
        }
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

    #[test]
    fn the_repl_turn_boundary_is_the_only_refresh_site() {
        // Assembled at runtime so this test's own text does not match the probes.
        let refresh_call = [".maybe_", "refresh("].concat();
        let refresh = ["maybe_", "refresh"].concat();
        let driver = ["MeshDigest", "Driver"].concat();
        let turn = ["run_repl_", "command(&mut ctx, self.abort_signal"].concat();
        let observe = ["digest.observe_", "session("].concat();
        let shutdown = ["digest.shut", "down().await"].concat();
        let idle_publish = ["idle_", "now()"].concat();
        let idle_stop = ["idle.stop", "().await"].concat();

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources = Vec::new();
        rust_sources(&src, &mut sources);
        let this_file = src.join("config/mesh_digest.rs");
        let repl_rs = src.join("repl/mod.rs");
        let never_digest = [
            src.join("main.rs"),
            src.join("acp/server.rs"),
            src.join("repl/idle.rs"),
        ];
        for file in never_digest.iter().chain([&this_file, &repl_rs]) {
            assert!(sources.contains(file), "{} not scanned", file.display());
        }

        let mut call_sites = Vec::new();
        for path in &sources {
            if *path == this_file {
                continue;
            }
            let source = String::from_utf8_lossy(&fs::read(path).unwrap()).into_owned();
            if never_digest.contains(path) {
                assert!(!source.contains(&refresh), "{}", path.display());
                assert!(!source.contains(&driver), "{}", path.display());
            }
            for (idx, line) in source.lines().enumerate() {
                if line.contains(&refresh_call) {
                    call_sites.push((path.clone(), idx));
                }
            }
        }
        assert_eq!(
            call_sites.len(),
            1,
            "{refresh_call} must be called from one place: {call_sites:?}"
        );
        let (path, refresh_at) = &call_sites[0];
        assert_eq!(*path, repl_rs);

        let repl = String::from_utf8_lossy(&fs::read(&repl_rs).unwrap()).into_owned();
        let repl_lines: Vec<&str> = repl.lines().collect();
        let turn_sites = line_indices(&repl_lines, &turn);
        assert_eq!(turn_sites.len(), 1, "{turn_sites:?}");
        let turn_at = turn_sites[0];
        assert!(
            *refresh_at > turn_at && *refresh_at <= turn_at + 12,
            "{refresh_call} at line {} is not within 12 lines after the turn at line {}",
            refresh_at + 1,
            turn_at + 1
        );

        let observe_sites = line_indices(&repl_lines, &observe);
        assert_eq!(observe_sites.len(), 1, "{observe_sites:?}");
        let observe_at = observe_sites[0];
        assert!(
            observe_at > turn_at && observe_at <= turn_at + 8,
            "{observe} at line {} is not within 8 lines after the turn at line {}",
            observe_at + 1,
            turn_at + 1
        );

        // The turn is guarded: the observe and the idle publish share its block, in that
        // order, and the block closes before the refresh.
        let block_end = (turn_at..*refresh_at)
            .find(|idx| repl_lines[*idx].trim() == "};")
            .expect("the turn's block closes before the refresh");
        assert!(
            observe_at < block_end,
            "{observe} at line {} must be inside the block closing at line {}",
            observe_at + 1,
            block_end + 1
        );
        let idle_sites: Vec<usize> = line_indices(&repl_lines, &idle_publish)
            .into_iter()
            .filter(|idx| (turn_at..block_end).contains(idx))
            .collect();
        assert_eq!(idle_sites.len(), 1, "{idle_sites:?}");
        assert!(
            observe_at < idle_sites[0],
            "{observe} at line {} must precede the idle publish at line {}",
            observe_at + 1,
            idle_sites[0] + 1
        );

        let exit_gate: Vec<usize> = (observe_at..*refresh_at)
            .filter(|idx| {
                repl_lines[*idx].contains("Ok(true)")
                    && repl_lines[*idx].contains("aborted_ctrld()")
            })
            .collect();
        assert_eq!(
            exit_gate.len(),
            1,
            "one exit and Ctrl-D gate must lie between {observe} and {refresh_call}: {exit_gate:?}"
        );

        let stop_sites = line_indices(&repl_lines, &idle_stop);
        assert_eq!(stop_sites.len(), 1, "{stop_sites:?}");
        let shutdown_sites = line_indices(&repl_lines, &shutdown);
        assert_eq!(shutdown_sites.len(), 1, "{shutdown_sites:?}");
        assert!(
            shutdown_sites[0] < stop_sites[0],
            "{shutdown} at line {} must come before {idle_stop} at line {}",
            shutdown_sites[0] + 1,
            stop_sites[0] + 1
        );
        assert!(
            shutdown_sites[0] > *refresh_at,
            "{shutdown} at line {} must come after the last {refresh_call} at line {}",
            shutdown_sites[0] + 1,
            refresh_at + 1
        );
    }

    fn line_indices(lines: &[&str], needle: &str) -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains(needle))
            .map(|(idx, _)| idx)
            .collect()
    }

    #[tokio::test]
    async fn digest_request_is_none_without_user_messages() {
        let ctx = test_ctx(MeshBrief::Auto);
        assert!(ctx.mesh_digest_request().is_none());
        let mut ctx = ctx;
        ctx.session = None;
        assert!(ctx.mesh_digest_request().is_none());
    }

    #[tokio::test]
    async fn configured_digest_prompt_replaces_the_default() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        ctx.app = Arc::new(AppState {
            config: Arc::new(AppConfig {
                mesh: MeshConfig {
                    digest_prompt: Some("CUSTOM-PROMPT-77".into()),
                    ..ctx.app.config.mesh.clone()
                },
                ..(*ctx.app.config).clone()
            }),
            ..(*ctx.app).clone()
        });
        add_n_turns(&mut ctx, 3);
        let model = Arc::new(FakeModel::default());

        run_mesh_digest(
            ctx.mesh_digest_request().unwrap(),
            model.fetch(FIXED_DIGEST),
        )
        .await
        .unwrap();

        let default_opening = MESH_DIGEST_PROMPT.split('.').next().unwrap();
        let requests = model.requests();
        assert!(!requests.is_empty());
        for request in &requests {
            assert!(request.ends_with("CUSTOM-PROMPT-77"), "{request}");
            assert!(!request.contains(default_opening), "{request}");
        }
    }

    #[tokio::test]
    async fn default_prompt_is_the_code_owned_one() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 1);
        let model = Arc::new(FakeModel::default());
        run_mesh_digest(
            ctx.mesh_digest_request().unwrap(),
            model.fetch(FIXED_DIGEST),
        )
        .await
        .unwrap();
        assert!(model.requests()[0].ends_with(MESH_DIGEST_PROMPT));
    }

    #[tokio::test]
    async fn a_blank_digest_prompt_uses_the_code_owned_default() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        ctx.app = Arc::new(AppState {
            config: Arc::new(AppConfig {
                mesh: MeshConfig {
                    digest_prompt: Some("  \n".into()),
                    ..ctx.app.config.mesh.clone()
                },
                ..(*ctx.app.config).clone()
            }),
            ..(*ctx.app).clone()
        });
        add_n_turns(&mut ctx, 1);
        let model = Arc::new(FakeModel::default());
        run_mesh_digest(
            ctx.mesh_digest_request().unwrap(),
            model.fetch(FIXED_DIGEST),
        )
        .await
        .unwrap();
        assert!(model.requests()[0].ends_with(MESH_DIGEST_PROMPT));
    }

    #[tokio::test]
    async fn an_empty_answer_is_an_error_not_a_digest() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 1);
        let model = Arc::new(FakeModel::default());
        let err = run_mesh_digest(ctx.mesh_digest_request().unwrap(), model.fetch(" \n"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn resolve_digest_model_collapses_when_configured_equals_current() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        ctx.session
            .as_mut()
            .unwrap()
            .set_model(named_model("digest-model"));
        let current = ctx.current_model().id();
        assert_eq!(current, "openai:digest-model");
        ctx.app = Arc::new(AppState {
            config: Arc::new(AppConfig {
                mesh: MeshConfig {
                    brief_model: Some(current),
                    ..ctx.app.config.mesh.clone()
                },
                ..(*ctx.app.config).clone()
            }),
            ..(*ctx.app).clone()
        });
        assert!(resolve_digest_model(&ctx).is_none());
    }

    #[test]
    fn resolve_digest_model_falls_back_on_unknown_model() {
        install_log_collector();
        let mut ctx = test_ctx(MeshBrief::Auto);
        ctx.app = Arc::new(AppState {
            config: Arc::new(AppConfig {
                mesh: MeshConfig {
                    brief_model: Some("nowhere:no-such-model-3f1c".into()),
                    ..ctx.app.config.mesh.clone()
                },
                ..(*ctx.app.config).clone()
            }),
            ..(*ctx.app).clone()
        });
        assert!(resolve_digest_model(&ctx).is_none());
        assert!(
            warn_snapshot()
                .iter()
                .any(|line| line.contains("nowhere:no-such-model-3f1c")
                    && line.contains("falling back")),
            "{:?}",
            warn_snapshot()
        );
        let request = ctx.mesh_digest_request();
        assert!(request.is_none(), "no user messages yet");
    }

    #[tokio::test]
    async fn resolve_digest_model_returns_a_resolvable_brief_model() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        ctx.app = Arc::new(AppState {
            config: Arc::new(AppConfig {
                mesh: MeshConfig {
                    brief_model: Some("test-seeded:digest-model".into()),
                    ..ctx.app.config.mesh.clone()
                },
                ..(*ctx.app.config).clone()
            }),
            ..(*ctx.app).clone()
        });
        add_n_turns(&mut ctx, 1);
        assert_ne!(ctx.current_model().id(), "test-seeded:digest-model");

        let request = ctx.mesh_digest_request().unwrap();
        assert_eq!(
            request.model.as_ref().map(Model::id).as_deref(),
            Some("test-seeded:digest-model")
        );
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let fetch = {
            let seen = Arc::clone(&seen);
            move |input: Input| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock().push(input.role().model().id());
                    Ok(FIXED_DIGEST.to_string())
                }
            }
        };
        run_mesh_digest(request, fetch).await.unwrap();

        let seen = seen.lock().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0], "test-seeded:digest-model");
    }

    #[tokio::test]
    async fn a_failing_brief_model_step_is_retried_on_the_session_model() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 1);
        let mut request = ctx.mesh_digest_request().unwrap();
        request.model = Some(named_model("brief-model"));
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let fetch = {
            let calls = Arc::clone(&calls);
            let seen = Arc::clone(&seen);
            move |input: Input| {
                let calls = Arc::clone(&calls);
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock().push(input.role().model().id());
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(anyhow!("boom"))
                    } else {
                        Ok(FIXED_DIGEST.to_string())
                    }
                }
            }
        };

        let digest = run_mesh_digest(request, fetch).await.unwrap();

        assert_eq!(digest.text, FIXED_DIGEST);
        let seen = seen.lock().clone();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], "openai:brief-model");
        assert_ne!(seen[1], "openai:brief-model");
    }

    fn shared(ctx: RequestContext) -> Arc<RwLock<RequestContext>> {
        Arc::new(RwLock::new(ctx))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn below_threshold_makes_no_call() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 1);
        let ctx = shared(ctx);
        let model = Arc::new(FakeModel::default());
        let mut driver = MeshDigestDriver::new();

        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));

        assert!(driver.task.is_none());
        assert_eq!(model.calls(), 0);
        assert!(ctx.read().app.mesh.digest().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn crossing_the_threshold_makes_one_call() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 4);
        publish_mesh_snapshot(&ctx, TurnState::idle_now());
        let ctx = shared(ctx);
        let model = Arc::new(FakeModel::default());
        let mut driver = MeshDigestDriver::new();

        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        wait_for_task(&mut driver).await;

        assert_eq!(model.calls(), 1);
        let mesh = Arc::clone(&ctx.read().app.mesh);
        let digest = mesh.digest().expect("the digest landed");
        assert_eq!(digest.covered_messages, 8);
        assert_eq!(digest.text, FIXED_DIGEST);
        let brief = mesh.brief().expect("the brief was reassembled");
        assert!(
            brief.text.contains("## Digest\n- Working on the widget"),
            "{}",
            brief.text
        );
        assert_eq!(brief.digest_generated_at, Some(digest.generated_at));
        assert!(!driver.in_flight.load(Ordering::SeqCst));

        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        assert!(driver.task.is_none(), "nothing new to digest");
        assert_eq!(model.calls(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn racing_boundaries_make_one_call() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 4);
        let ctx = shared(ctx);
        let (model, gate) = FakeModel::gated();
        let mut driver = MeshDigestDriver::new();

        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        wait_for_call(&model, 1).await;
        let first = driver.task.take().expect("the first generation is running");
        driver.maybe_refresh_with(
            &ctx,
            Instant::now() + MESH_DIGEST_MIN_INTERVAL,
            model.fetch(FIXED_DIGEST),
        );
        assert!(driver.task.is_none(), "a second generation must not start");

        gate.notify_one();
        first.await.unwrap();
        assert_eq!(model.calls(), 1);
        assert!(ctx.read().app.mesh.digest().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interval_gates_a_second_generation() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 4);
        let ctx = shared(ctx);
        let model = Arc::new(FakeModel::default());
        let mut driver = MeshDigestDriver::new();
        let start = Instant::now();

        driver.maybe_refresh_with(&ctx, start, model.fetch(FIXED_DIGEST));
        wait_for_task(&mut driver).await;
        assert_eq!(model.calls(), 1);

        add_n_turns(&mut ctx.write(), 4);
        driver.maybe_refresh_with(
            &ctx,
            start + MESH_DIGEST_MIN_INTERVAL / 6,
            model.fetch(FIXED_DIGEST),
        );
        assert!(driver.task.is_none());
        assert_eq!(model.calls(), 1);

        driver.maybe_refresh_with(
            &ctx,
            start + MESH_DIGEST_MIN_INTERVAL + Duration::from_secs(1),
            model.fetch(FIXED_DIGEST),
        );
        wait_for_task(&mut driver).await;
        assert_eq!(model.calls(), 2);
        assert_eq!(ctx.read().app.mesh.digest().unwrap().covered_messages, 16);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_generation_releases_in_flight() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 4);
        let ctx = shared(ctx);
        let model = FakeModel::failing_first();
        let mut driver = MeshDigestDriver::new();
        let start = Instant::now();

        driver.maybe_refresh_with(&ctx, start, model.fetch(FIXED_DIGEST));
        wait_for_task(&mut driver).await;
        assert_eq!(model.calls(), 1);
        assert!(!driver.in_flight.load(Ordering::SeqCst));
        assert!(ctx.read().app.mesh.digest().is_none());

        driver.maybe_refresh_with(
            &ctx,
            start + MESH_DIGEST_MIN_INTERVAL,
            model.fetch(FIXED_DIGEST),
        );
        wait_for_task(&mut driver).await;
        assert_eq!(model.calls(), 2);
        assert_eq!(
            ctx.read().app.mesh.digest().unwrap().text,
            FIXED_DIGEST,
            "the retry after the interval lands"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compression_keeps_the_digest_covering_the_whole_session() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_turns_as(
            &mut ctx,
            Role::new("", "SYSTEM-PROMPT-MARKER"),
            &["turn-00 EARLY-MARKER"],
        );
        add_turns(
            &mut ctx,
            &["turn-01", "turn-02", "turn-03", "turn-04", "turn-05"],
        );
        let ctx = shared(ctx);
        assert_eq!(message_count(&ctx), 12, "six turns behind a system prompt");
        let model = Arc::new(FakeModel::default());
        let mut driver = MeshDigestDriver::new();
        let start = Instant::now();
        driver.maybe_refresh_with(&ctx, start, model.fetch(FIXED_DIGEST));
        wait_for_task(&mut driver).await;
        assert_eq!(model.calls(), 1);
        assert_eq!(ctx.read().app.mesh.digest().unwrap().covered_messages, 12);

        ctx.write()
            .session
            .as_mut()
            .unwrap()
            .compress(format!("{SUMMARY_CONTEXT_PROMPT}recap RECAP-MARKER-77"), 2);
        {
            let session = ctx.read();
            let session = session.session.as_ref().unwrap();
            assert_eq!(session.foldable_messages(0).len(), 2);
            assert_eq!(session.compressed_messages().len(), 11);
        }
        assert_eq!(
            message_count(&ctx),
            13,
            "the system prompt now counts as a compressed message"
        );
        add_turns(
            &mut ctx.write(),
            &["late-00", "late-01", "late-02", "late-03 NEWEST-MARKER"],
        );
        driver.maybe_refresh_with(
            &ctx,
            start + MESH_DIGEST_MIN_INTERVAL,
            model.fetch(FIXED_DIGEST),
        );
        wait_for_task(&mut driver).await;

        assert_eq!(model.calls(), 2);
        let request = &model.requests()[1];
        assert!(request.contains("RECAP-MARKER-77"), "{request}");
        assert!(request.contains("NEWEST-MARKER"), "{request}");
        assert!(request.contains("turn-05"), "{request}");
        assert!(
            !request.contains("SYSTEM-PROMPT-MARKER"),
            "the original system prompt is not part of the recap: {request}"
        );
        assert!(
            !request.contains("EARLY-MARKER"),
            "compressed turns reach the digest through the recap only: {request}"
        );
        let covered = ctx.read().app.mesh.digest().unwrap().covered_messages;
        assert_eq!(covered, message_count(&ctx));
        assert_eq!(covered, 21);
    }

    fn set_summary_marker(ctx: &mut RequestContext, marker: &str) {
        ctx.app = Arc::new(AppState {
            config: Arc::new(AppConfig {
                summary_context_prompt: Some(marker.into()),
                ..(*ctx.app.config).clone()
            }),
            ..(*ctx.app).clone()
        });
    }

    #[tokio::test]
    async fn a_recap_with_an_unknown_marker_is_not_seeded() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        set_summary_marker(&mut ctx, "OTHER: ");
        add_turns_as(
            &mut ctx,
            Role::new("", "SYSTEM-PROMPT-MARKER"),
            &["turn-00 EARLY-MARKER"],
        );
        add_turns(&mut ctx, &["turn-01", "turn-02"]);
        ctx.session.as_mut().unwrap().compress(
            "[ACTIVE TODO LIST]\ntodo-secret\n\nCUSTOM-RECAP: recap RECAP-MARKER".into(),
            2,
        );
        let recap = ctx.session.as_ref().unwrap().messages()[0]
            .content
            .to_text();
        assert!(recap.contains("SYSTEM-PROMPT-MARKER"), "{recap}");
        assert!(recap.contains("RECAP-MARKER"), "{recap}");
        let model = Arc::new(FakeModel::default());

        let request = ctx.mesh_digest_request().unwrap();
        assert_eq!(request.prior_summary, "");
        run_mesh_digest(request, model.fetch(FIXED_DIGEST))
            .await
            .unwrap();

        let requests = model.requests();
        assert!(!requests.is_empty());
        for request in &requests {
            assert!(request.contains("turn-02"), "{request}");
            for leaked in [
                "SYSTEM-PROMPT-MARKER",
                "RECAP-MARKER",
                "[ACTIVE TODO LIST]",
                "todo-secret",
                "EARLY-MARKER",
            ] {
                assert!(!request.contains(leaked), "{leaked} in {request}");
            }
        }
    }

    #[tokio::test]
    async fn a_recap_under_the_default_marker_is_found_when_the_config_differs() {
        for configured in ["OTHER: ", ""] {
            let mut ctx = test_ctx(MeshBrief::Auto);
            set_summary_marker(&mut ctx, configured);
            add_turns_as(
                &mut ctx,
                Role::new("", "SYSTEM-PROMPT-MARKER"),
                &["turn-00"],
            );
            add_turns(&mut ctx, &["turn-01", "turn-02"]);
            ctx.session
                .as_mut()
                .unwrap()
                .compress(format!("{SUMMARY_CONTEXT_PROMPT}recap RECAP-MARKER"), 2);
            let model = Arc::new(FakeModel::default());

            let request = ctx.mesh_digest_request().unwrap();
            assert_eq!(
                request.prior_summary, "recap RECAP-MARKER",
                "{configured:?}"
            );
            run_mesh_digest(request, model.fetch(FIXED_DIGEST))
                .await
                .unwrap();

            let first = &model.requests()[0];
            assert!(first.contains("RECAP-MARKER"), "{configured:?}: {first}");
            assert!(
                !first.contains("SYSTEM-PROMPT-MARKER"),
                "{configured:?}: {first}"
            );
        }
    }

    #[tokio::test]
    async fn a_recap_under_the_configured_marker_is_seeded() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        set_summary_marker(&mut ctx, "OTHER: ");
        add_turns_as(
            &mut ctx,
            Role::new("", "SYSTEM-PROMPT-MARKER"),
            &["turn-00 EARLY-MARKER"],
        );
        add_turns(&mut ctx, &["turn-01", "turn-02"]);
        ctx.session
            .as_mut()
            .unwrap()
            .compress("OTHER: recap RECAP-MARKER".into(), 2);
        let recap = ctx.session.as_ref().unwrap().messages()[0]
            .content
            .to_text();
        assert!(recap.contains("SYSTEM-PROMPT-MARKER"), "{recap}");
        assert!(!recap.contains(SUMMARY_CONTEXT_PROMPT), "{recap}");
        let model = Arc::new(FakeModel::default());

        let request = ctx.mesh_digest_request().unwrap();
        assert_eq!(request.prior_summary, "recap RECAP-MARKER");
        run_mesh_digest(request, model.fetch(FIXED_DIGEST))
            .await
            .unwrap();

        let requests = model.requests();
        assert!(!requests.is_empty());
        assert!(requests[0].contains("RECAP-MARKER"), "{}", requests[0]);
        for request in &requests {
            assert!(!request.contains("SYSTEM-PROMPT-MARKER"), "{request}");
            assert!(!request.contains("EARLY-MARKER"), "{request}");
        }
    }

    #[tokio::test]
    async fn a_marker_quoted_in_the_system_prompt_does_not_seed_its_tail() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_turns_as(
            &mut ctx,
            Role::new(
                "",
                &format!("SYSTEM-PROMPT-MARKER {SUMMARY_CONTEXT_PROMPT}DECOY-TAIL"),
            ),
            &["turn-00"],
        );
        add_turns(&mut ctx, &["turn-01", "turn-02"]);
        ctx.session
            .as_mut()
            .unwrap()
            .compress(format!("{SUMMARY_CONTEXT_PROMPT}recap RECAP-MARKER"), 2);
        let session = ctx.session.as_ref().unwrap();
        assert!(
            session
                .compressed_system_prompt()
                .unwrap()
                .contains(SUMMARY_CONTEXT_PROMPT)
        );
        let recap = session.messages()[0].content.to_text();
        assert_eq!(recap.matches(SUMMARY_CONTEXT_PROMPT).count(), 2, "{recap}");

        let request = ctx.mesh_digest_request().unwrap();
        assert_eq!(request.prior_summary, "recap RECAP-MARKER");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_history_edit_that_shrinks_the_transcript_regenerates() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 6);
        let ctx = shared(ctx);
        let model = Arc::new(FakeModel::default());
        let mut driver = MeshDigestDriver::new();
        let start = Instant::now();
        driver.maybe_refresh_with(&ctx, start, model.fetch(FIXED_DIGEST));
        wait_for_task(&mut driver).await;
        assert_eq!(ctx.read().app.mesh.digest().unwrap().covered_messages, 12);

        {
            let mut guard = ctx.write();
            let session = guard.session.as_mut().unwrap();
            session.pop_last_exchange().unwrap();
            session.pop_last_exchange().unwrap();
        }
        assert_eq!(message_count(&ctx), 8);
        driver.maybe_refresh_with(
            &ctx,
            start + MESH_DIGEST_MIN_INTERVAL,
            model.fetch(FIXED_DIGEST),
        );
        wait_for_task(&mut driver).await;

        assert_eq!(model.calls(), 2);
        assert_eq!(ctx.read().app.mesh.digest().unwrap().covered_messages, 8);
        assert!(!model.requests()[1].contains("turn-05"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_history_edit_clears_the_digest_at_once() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 6);
        let ctx = shared(ctx);
        let model = Arc::new(FakeModel::default());
        let mut driver = MeshDigestDriver::new();
        let start = Instant::now();
        driver.maybe_refresh_with(&ctx, start, model.fetch(FIXED_DIGEST));
        wait_for_task(&mut driver).await;
        let mesh = Arc::clone(&ctx.read().app.mesh);
        assert_eq!(mesh.digest().unwrap().covered_messages, 12);
        let epoch = mesh.digest_epoch();

        ctx.write()
            .session
            .as_mut()
            .unwrap()
            .pop_last_exchange()
            .unwrap();
        assert_eq!(message_count(&ctx), 10);
        driver.maybe_refresh_with(
            &ctx,
            start + Duration::from_secs(1),
            model.fetch(FIXED_DIGEST),
        );

        assert!(
            driver.task.is_none(),
            "the interval still gates regeneration"
        );
        assert_eq!(model.calls(), 1);
        assert!(
            mesh.digest().is_none(),
            "a digest of retracted turns must not wait out the interval"
        );
        assert_eq!(mesh.digest_epoch(), epoch + 1);

        driver.maybe_refresh_with(
            &ctx,
            start + MESH_DIGEST_MIN_INTERVAL,
            model.fetch(FIXED_DIGEST),
        );
        wait_for_task(&mut driver).await;
        assert_eq!(model.calls(), 2);
        assert_eq!(mesh.digest().unwrap().covered_messages, 10);

        ctx.write().session.as_mut().unwrap().clear_messages();
        assert_eq!(message_count(&ctx), 0);
        driver.maybe_refresh_with(
            &ctx,
            start + MESH_DIGEST_MIN_INTERVAL + Duration::from_secs(1),
            model.fetch(FIXED_DIGEST),
        );

        assert!(driver.task.is_none());
        assert_eq!(model.calls(), 2);
        assert!(
            mesh.digest().is_none(),
            "an emptied session serves no digest"
        );
        assert_eq!(mesh.digest_epoch(), epoch + 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_history_edit_during_generation_fences_the_late_result() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 8);
        let ctx = shared(ctx);
        let (model, gate) = FakeModel::gated();
        let mut driver = MeshDigestDriver::new();
        let start = Instant::now();
        driver.maybe_refresh_with(&ctx, start, model.fetch(FIXED_DIGEST));
        wait_for_call(&model, 1).await;
        assert_eq!(driver.pending_covered, Some(16));
        let old_flag = Arc::clone(&driver.in_flight);
        let mesh = Arc::clone(&ctx.read().app.mesh);
        let epoch = mesh.digest_epoch();

        ctx.write()
            .session
            .as_mut()
            .unwrap()
            .pop_last_exchange()
            .unwrap();
        assert_eq!(message_count(&ctx), 14);
        driver.maybe_refresh_with(
            &ctx,
            start + Duration::from_secs(1),
            model.fetch(FIXED_DIGEST),
        );

        assert!(
            driver.task.is_none(),
            "the generation covering retracted turns was aborted"
        );
        assert_eq!(driver.pending_covered, None);
        assert_eq!(mesh.digest_epoch(), epoch + 1);

        gate.notify_one();
        wait_until_cleared(&old_flag).await;
        assert_eq!(model.calls(), 1);
        assert!(
            mesh.digest().is_none(),
            "a digest of retracted turns must not land"
        );
        assert!(!old_flag.load(Ordering::SeqCst));
        assert!(!driver.in_flight.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generation_holds_no_ctx_lock() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 4);
        let ctx = shared(ctx);
        let (model, gate) = FakeModel::gated();
        let mut driver = MeshDigestDriver::new();

        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        wait_for_call(&model, 1).await;

        assert!(
            ctx.try_write().is_some(),
            "the context must be free while the model call is in flight"
        );
        gate.notify_one();
        wait_for_task(&mut driver).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_busy_ctx_skips_the_boundary() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 4);
        let ctx = shared(ctx);
        let model = Arc::new(FakeModel::default());
        let mut driver = MeshDigestDriver::new();

        let held = ctx.write();
        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        drop(held);

        assert!(driver.task.is_none());
        assert_eq!(model.calls(), 0);
    }

    fn set_brief_mode(ctx: &Arc<RwLock<RequestContext>>, mode: MeshBrief) {
        let mut ctx = ctx.write();
        ctx.app = Arc::new(AppState {
            config: Arc::new(AppConfig {
                mesh: MeshConfig {
                    brief: mode,
                    ..ctx.app.config.mesh.clone()
                },
                ..(*ctx.app.config).clone()
            }),
            ..(*ctx.app).clone()
        });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manual_and_off_make_no_call_and_clear_the_digest() {
        for mode in [MeshBrief::Manual, MeshBrief::Off] {
            let mut ctx = test_ctx(MeshBrief::Auto);
            add_n_turns(&mut ctx, 6);
            let ctx = shared(ctx);
            let (model, gate) = FakeModel::gated();
            let mut driver = MeshDigestDriver::new();
            driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
            wait_for_call(&model, 1).await;
            let old_flag = Arc::clone(&driver.in_flight);
            let mesh = Arc::clone(&ctx.read().app.mesh);
            mesh.publish_digest(Some(Digest {
                text: "stale".into(),
                generated_at: SystemTime::now(),
                covered_messages: 2,
            }));
            let epoch = mesh.digest_epoch();

            set_brief_mode(&ctx, mode);
            driver.maybe_refresh_with(
                &ctx,
                Instant::now() + MESH_DIGEST_MIN_INTERVAL,
                model.fetch(FIXED_DIGEST),
            );
            assert!(
                driver.task.is_none(),
                "{mode:?}: the generation was aborted"
            );
            assert!(mesh.digest().is_none(), "{mode:?}: cleared at once");
            assert_eq!(mesh.digest_epoch(), epoch + 1, "{mode:?}");

            gate.notify_one();
            wait_until_cleared(&old_flag).await;
            driver.maybe_refresh_with(
                &ctx,
                Instant::now() + 2 * MESH_DIGEST_MIN_INTERVAL,
                model.fetch(FIXED_DIGEST),
            );

            assert!(driver.task.is_none(), "{mode:?}");
            assert_eq!(model.calls(), 1, "{mode:?}");
            assert!(mesh.digest().is_none(), "{mode:?}");
            assert_eq!(
                mesh.digest_epoch(),
                epoch + 1,
                "{mode:?}: nothing left to clear"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mesh_disabled_makes_no_call() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        ctx.app = Arc::new(AppState {
            config: Arc::new(AppConfig {
                mesh: MeshConfig {
                    enabled: false,
                    ..ctx.app.config.mesh.clone()
                },
                ..(*ctx.app.config).clone()
            }),
            ..(*ctx.app).clone()
        });
        add_n_turns(&mut ctx, 6);
        let ctx = shared(ctx);
        let model = Arc::new(FakeModel::default());
        let mut driver = MeshDigestDriver::new();

        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));

        assert!(driver.task.is_none());
        assert_eq!(model.calls(), 0);
    }

    fn switch_session(ctx: &Arc<RwLock<RequestContext>>) {
        let mut guard = ctx.write();
        let mut other = Session::default();
        other.set_name("other-session".into());
        guard.session = Some(other);
        add_n_turns(&mut guard, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_change_clears_the_digest() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 4);
        let ctx = shared(ctx);
        let model = Arc::new(FakeModel::default());
        let mut driver = MeshDigestDriver::new();
        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        wait_for_task(&mut driver).await;
        assert!(ctx.read().app.mesh.digest().is_some());

        switch_session(&ctx);
        driver.maybe_refresh_with(
            &ctx,
            Instant::now() + MESH_DIGEST_MIN_INTERVAL,
            model.fetch(FIXED_DIGEST),
        );

        assert!(driver.task.is_none());
        assert_eq!(model.calls(), 1);
        assert!(
            ctx.read().app.mesh.digest().is_none(),
            "a digest of one session must not be served for another"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_change_aborts_an_in_flight_generation() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 4);
        let ctx = shared(ctx);
        let (model, gate) = FakeModel::gated();
        let mut driver = MeshDigestDriver::new();
        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        wait_for_call(&model, 1).await;
        assert!(driver.in_flight.load(Ordering::SeqCst));
        let old_flag = Arc::clone(&driver.in_flight);

        switch_session(&ctx);
        driver.maybe_refresh_with(
            &ctx,
            Instant::now() + MESH_DIGEST_MIN_INTERVAL,
            model.fetch(FIXED_DIGEST),
        );
        assert!(
            driver.task.is_none(),
            "the old generation was taken and aborted"
        );
        gate.notify_one();
        wait_until_cleared(&old_flag).await;

        assert_eq!(model.calls(), 1);
        assert!(
            ctx.read().app.mesh.digest().is_none(),
            "a generation started for one session must not land for another"
        );
    }

    /// Replaces the session with one of the same name, telling them apart by the mesh
    /// instance id alone, as two unsaved `temp` sessions would be.
    fn switch_to_same_named_session(ctx: &Arc<RwLock<RequestContext>>) -> String {
        let mut guard = ctx.write();
        let name = guard.session.as_ref().unwrap().name().to_string();
        let mut other = Session::default();
        other.set_name(name);
        let id = other.ensure_mesh_instance_id().to_string();
        guard.session = Some(other);
        add_n_turns(&mut guard, 1);
        id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_same_name_session_switch_aborts_an_in_flight_generation() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        let first_id = ctx
            .session
            .as_mut()
            .unwrap()
            .ensure_mesh_instance_id()
            .to_string();
        add_n_turns(&mut ctx, 4);
        let ctx = shared(ctx);
        let (model, gate) = FakeModel::gated();
        let mut driver = MeshDigestDriver::new();
        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        wait_for_call(&model, 1).await;
        let old_flag = Arc::clone(&driver.in_flight);
        let mesh = Arc::clone(&ctx.read().app.mesh);
        let epoch = mesh.digest_epoch();

        let second_id = switch_to_same_named_session(&ctx);
        assert_ne!(first_id, second_id);
        driver.maybe_refresh_with(
            &ctx,
            Instant::now() + MESH_DIGEST_MIN_INTERVAL,
            model.fetch(FIXED_DIGEST),
        );
        assert!(
            driver.task.is_none(),
            "the old generation was taken and aborted"
        );
        assert_eq!(mesh.digest_epoch(), epoch + 1);
        gate.notify_one();
        wait_until_cleared(&old_flag).await;

        assert_eq!(model.calls(), 1);
        assert!(mesh.digest().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_completed_generation_from_a_previous_session_is_discarded() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        ctx.session.as_mut().unwrap().ensure_mesh_instance_id();
        add_n_turns(&mut ctx, 4);
        publish_mesh_snapshot(&ctx, TurnState::idle_now());
        let ctx = shared(ctx);
        let (model, gate) = FakeModel::gated();
        let mut driver = MeshDigestDriver::new();
        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        wait_for_call(&model, 1).await;
        let old_flag = Arc::clone(&driver.in_flight);
        let mesh = Arc::clone(&ctx.read().app.mesh);
        let started_under = mesh.digest_epoch();

        switch_to_same_named_session(&ctx);
        driver.maybe_refresh_with(
            &ctx,
            Instant::now() + MESH_DIGEST_MIN_INTERVAL,
            model.fetch(FIXED_DIGEST),
        );
        assert_eq!(mesh.digest_epoch(), started_under + 1);

        // Stands in for the aborted task finishing its model call before the abort
        // reached it: the publish is refused by the epoch, not by the abort.
        let late = Digest {
            text: FIXED_DIGEST.into(),
            generated_at: SystemTime::now(),
            covered_messages: 8,
        };
        assert!(!mesh.publish_digest_at(started_under, late));
        assert!(mesh.digest().is_none());
        assert!(
            !mesh
                .brief()
                .is_some_and(|brief| brief.text.contains("## Digest")),
            "{:?}",
            mesh.brief()
        );

        gate.notify_one();
        wait_until_cleared(&old_flag).await;
        assert_eq!(model.calls(), 1);
        assert!(mesh.digest().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_switch_observed_under_a_held_guard_fences_the_late_digest() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 4);
        let ctx = shared(ctx);
        let (model, gate) = FakeModel::gated();
        let mut driver = MeshDigestDriver::new();
        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        wait_for_call(&model, 1).await;
        let old_flag = Arc::clone(&driver.in_flight);
        let mesh = Arc::clone(&ctx.read().app.mesh);
        let epoch = mesh.digest_epoch();

        {
            let mut guard = ctx.write();
            let mut other = Session::default();
            other.set_name("other-session".into());
            guard.session = Some(other);
            add_n_turns(&mut guard, 4);
            driver.observe_session(&guard);
        }
        assert!(driver.task.is_none(), "the old generation was aborted");
        assert_eq!(mesh.digest_epoch(), epoch + 1);
        assert!(mesh.digest().is_none());

        driver.maybe_refresh_with(
            &ctx,
            Instant::now() + MESH_DIGEST_MIN_INTERVAL,
            model.fetch("- Second session"),
        );
        assert!(
            driver.task.is_some(),
            "the new session's first boundary spawns without waiting for the aborted task"
        );
        wait_for_call(&model, 2).await;

        gate.notify_one();
        gate.notify_one();
        wait_until_cleared(&old_flag).await;
        wait_for_task(&mut driver).await;

        assert_eq!(model.calls(), 2);
        let digest = mesh.digest().expect("the second session's digest landed");
        assert_eq!(digest.text, "- Second session");
        assert_eq!(digest.covered_messages, 8);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_aborts_an_in_flight_generation() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 4);
        let ctx = shared(ctx);
        let (model, _gate) = FakeModel::gated();
        let mut driver = MeshDigestDriver::new();

        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        wait_for_call(&model, 1).await;
        assert!(driver.in_flight.load(Ordering::SeqCst));

        tokio::time::timeout(Duration::from_secs(5), driver.shutdown())
            .await
            .expect("shutdown returns without the gate ever opening");

        assert!(driver.task.is_none());
        assert!(!driver.in_flight.load(Ordering::SeqCst));
        assert!(ctx.read().app.mesh.digest().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_driver_aborts_an_in_flight_generation() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        add_n_turns(&mut ctx, 4);
        let ctx = shared(ctx);
        let (model, _gate) = FakeModel::gated();
        let mut driver = MeshDigestDriver::new();
        let in_flight = Arc::clone(&driver.in_flight);

        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        wait_for_call(&model, 1).await;
        drop(driver);

        wait_until_cleared(&in_flight).await;
        assert!(ctx.read().app.mesh.digest().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn transcript_never_reaches_the_brief_or_the_card() {
        let mut ctx = test_ctx(MeshBrief::Auto);
        ctx.todo_list.goal = String::new();
        add_turns(
            &mut ctx,
            &[
                "SECRET-TRANSCRIPT-MARKER-4c1e first",
                "second SECRET-TRANSCRIPT-MARKER-4c1e",
                "third",
                "SECRET-TRANSCRIPT-MARKER-4c1e fourth",
            ],
        );
        publish_mesh_snapshot(&ctx, TurnState::idle_now());
        let ctx = shared(ctx);
        let model = Arc::new(FakeModel::default());
        let mut driver = MeshDigestDriver::new();

        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        wait_for_task(&mut driver).await;
        assert_eq!(model.calls(), 1);
        assert!(
            model
                .requests()
                .iter()
                .all(|text| text.contains("SECRET-TRANSCRIPT-MARKER-4c1e")),
            "the transcript reaches the model and nowhere else"
        );
        publish_mesh_snapshot(&ctx.read(), TurnState::idle_now());

        let mesh: Arc<MeshSlot> = Arc::clone(&ctx.read().app.mesh);
        let brief = mesh.brief().expect("brief served");
        assert!(
            !brief.text.contains("SECRET-TRANSCRIPT-MARKER-4c1e"),
            "{}",
            brief.text
        );
        assert!(brief.text.contains("Decided on X"), "{}", brief.text);
        let snapshot = mesh.snapshot().unwrap();
        assert!(
            !snapshot
                .brief
                .text
                .as_deref()
                .unwrap()
                .contains("SECRET-TRANSCRIPT-MARKER-4c1e")
        );

        let handler = StatusHandler::new(Arc::downgrade(&mesh) as Weak<dyn CardSource>);
        let now = SystemTime::now();
        let card = handler.card(now);
        assert_eq!(card.objective.as_deref(), Some("Working on the widget"));
        let encoded = format!("{card:?}");
        assert!(
            !encoded.contains("SECRET-TRANSCRIPT-MARKER-4c1e"),
            "{encoded}"
        );
        assert_eq!(
            card,
            build_card(
                Some(&snapshot),
                None,
                digest_objective_for(&snapshot, mesh.digest().as_deref()).as_deref(),
                None,
                now
            ),
            "the served card is the digest-aware one"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn brief_off_spends_no_tokens() {
        let mut ctx = test_ctx(MeshBrief::Off);
        ctx.todo_list.goal = String::new();
        add_turns(
            &mut ctx,
            &[
                "SECRET-TRANSCRIPT-MARKER-4c1e first",
                "second",
                "third",
                "fourth",
            ],
        );
        publish_mesh_snapshot(&ctx, TurnState::idle_now());
        let ctx = shared(ctx);
        let model = Arc::new(FakeModel::default());
        let mut driver = MeshDigestDriver::new();

        driver.maybe_refresh_with(&ctx, Instant::now(), model.fetch(FIXED_DIGEST));
        driver.maybe_refresh_with(
            &ctx,
            Instant::now() + MESH_DIGEST_MIN_INTERVAL,
            model.fetch(FIXED_DIGEST),
        );

        assert!(driver.task.is_none());
        assert_eq!(model.calls(), 0);
        let mesh: Arc<MeshSlot> = Arc::clone(&ctx.read().app.mesh);
        assert!(mesh.brief().is_none());
        let handler = StatusHandler::new(Arc::downgrade(&mesh) as Weak<dyn CardSource>);
        let card = handler.card(SystemTime::now());
        assert_eq!(card.objective, None);
        assert_eq!(card.state.code, crate::mesh::card::STATE_IDLE);
        assert!(!format!("{card:?}").contains("SECRET-TRANSCRIPT-MARKER-4c1e"));
    }
}
