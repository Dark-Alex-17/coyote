use crate::config::conflict::{self, InstallMode, StickyMode};
use crate::config::{
    RequestContext, builtin_manifest, ensure_parent_exists, paths, set_executable_bit_if_script,
};
use crate::function::write_file_atomic;

use anyhow::{Result, anyhow};
use chrono::{SecondsFormat, Utc};
use indexmap::IndexMap;
use rand::distr::{Alphanumeric, SampleString};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Notify;

#[derive(Embed)]
#[folder = "assets/hooks/"]
struct HookAssets;

pub fn install_builtin_hooks(mode: InstallMode, sticky: &mut StickyMode) -> Result<()> {
    info!(
        "Installing built-in example hooks in {}",
        paths::hooks_dir().display()
    );

    let mut wrote_any = false;
    let mut written = BTreeSet::new();
    for file in HookAssets::iter() {
        debug!("Processing hook file: {}", file.as_ref());

        let embedded_file = HookAssets::get(&file)
            .ok_or_else(|| anyhow!("Failed to load embedded hook file: {}", file.as_ref()))?;
        let content = std::str::from_utf8(&embedded_file.data)
            .expect("bundled hook asset is not valid UTF-8");
        let file_path = paths::hooks_dir().join(file.as_ref());

        if file_path.exists()
            && !conflict::should_replace_existing(&file_path, content, "hooks", mode, sticky)?
        {
            debug!(
                "Hook file already exists, skipping: {}",
                file_path.display()
            );
            continue;
        }

        ensure_parent_exists(&file_path)?;
        info!("Creating hook file: {}", file_path.display());
        write_file_atomic(&file_path, content, None)?;
        set_executable_bit_if_script(&file_path)?;
        wrote_any = true;
        if !file.as_ref().contains('/') {
            written.insert(file.as_ref().to_string());
        }
    }

    let shipped: BTreeSet<String> = HookAssets::iter()
        .map(|file| file.as_ref().to_string())
        .filter(|name| !name.contains('/'))
        .collect();
    if let Err(err) =
        builtin_manifest::reconcile_builtin_dir(&paths::hooks_dir(), &shipped, &written)
    {
        warn!(
            "Failed to reconcile builtin hooks in {}: {err}",
            paths::hooks_dir().display()
        );
    }

    let hooks_dir = canonical_or_original(&paths::hooks_dir());
    let default_hooks_dir = canonical_or_original(&paths::config_dir().join("hooks"));
    if wrote_any && hooks_dir != default_hooks_dir {
        warn!(
            "{} overrides the hooks dir: example scripts install to {}, but relative \
             global-hook commands resolve against {}",
            crate::utils::get_env_name("hooks_dir"),
            paths::hooks_dir().display(),
            paths::config_dir().display()
        );
    }

    Ok(())
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// A single named hook: an external command to run when its event fires.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HookDef {
    pub name: String,
    pub command: String,
}

/// Ordered mapping of event name -> hook definitions, shared by every config scope.
pub type HooksMap = IndexMap<String, Vec<HookDef>>;

/// Every event hooks can be attached to. `as_str` yields the dotted name used
/// as the key in `hooks:` maps and in `global_hooks` whitelist entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    TurnStarted,
    TurnCompleted,
    TurnInterrupted,
    TurnFailed,
    SessionStarted,
    SessionResumed,
    SessionEnded,
    SessionCompressed,
    ToolStarted,
    ToolCompleted,
    ToolFailed,
    LlmRequestStarted,
    LlmRequestCompleted,
    LlmRequestFailed,
    AgentStarted,
    AgentCompleted,
    AgentInterrupted,
    AgentFailed,
    EscalationRaised,
    EscalationAnswered,
    GraphNodeStarted,
    GraphNodeCompleted,
    GraphNodeFailed,
    JobStarted,
    JobCompleted,
    JobFailed,
    RagSyncStarted,
    RagSyncCompleted,
    RagSyncFailed,
    McpServerConnected,
    McpServerFailed,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::TurnStarted => "turn.started",
            HookEvent::TurnCompleted => "turn.completed",
            HookEvent::TurnInterrupted => "turn.interrupted",
            HookEvent::TurnFailed => "turn.failed",
            HookEvent::SessionStarted => "session.started",
            HookEvent::SessionResumed => "session.resumed",
            HookEvent::SessionEnded => "session.ended",
            HookEvent::SessionCompressed => "session.compressed",
            HookEvent::ToolStarted => "tool.started",
            HookEvent::ToolCompleted => "tool.completed",
            HookEvent::ToolFailed => "tool.failed",
            HookEvent::LlmRequestStarted => "llm.request.started",
            HookEvent::LlmRequestCompleted => "llm.request.completed",
            HookEvent::LlmRequestFailed => "llm.request.failed",
            HookEvent::AgentStarted => "agent.started",
            HookEvent::AgentCompleted => "agent.completed",
            HookEvent::AgentInterrupted => "agent.interrupted",
            HookEvent::AgentFailed => "agent.failed",
            HookEvent::EscalationRaised => "escalation.raised",
            HookEvent::EscalationAnswered => "escalation.answered",
            HookEvent::GraphNodeStarted => "graph.node.started",
            HookEvent::GraphNodeCompleted => "graph.node.completed",
            HookEvent::GraphNodeFailed => "graph.node.failed",
            HookEvent::JobStarted => "job.started",
            HookEvent::JobCompleted => "job.completed",
            HookEvent::JobFailed => "job.failed",
            HookEvent::RagSyncStarted => "rag.sync.started",
            HookEvent::RagSyncCompleted => "rag.sync.completed",
            HookEvent::RagSyncFailed => "rag.sync.failed",
            HookEvent::McpServerConnected => "mcp.server.connected",
            HookEvent::McpServerFailed => "mcp.server.failed",
        }
    }

    const ALL: [HookEvent; 31] = [
        HookEvent::TurnStarted,
        HookEvent::TurnCompleted,
        HookEvent::TurnInterrupted,
        HookEvent::TurnFailed,
        HookEvent::SessionStarted,
        HookEvent::SessionResumed,
        HookEvent::SessionEnded,
        HookEvent::SessionCompressed,
        HookEvent::ToolStarted,
        HookEvent::ToolCompleted,
        HookEvent::ToolFailed,
        HookEvent::LlmRequestStarted,
        HookEvent::LlmRequestCompleted,
        HookEvent::LlmRequestFailed,
        HookEvent::AgentStarted,
        HookEvent::AgentCompleted,
        HookEvent::AgentInterrupted,
        HookEvent::AgentFailed,
        HookEvent::EscalationRaised,
        HookEvent::EscalationAnswered,
        HookEvent::GraphNodeStarted,
        HookEvent::GraphNodeCompleted,
        HookEvent::GraphNodeFailed,
        HookEvent::JobStarted,
        HookEvent::JobCompleted,
        HookEvent::JobFailed,
        HookEvent::RagSyncStarted,
        HookEvent::RagSyncCompleted,
        HookEvent::RagSyncFailed,
        HookEvent::McpServerConnected,
        HookEvent::McpServerFailed,
    ];
}

/// An owned snapshot of one hook to run. Detached dispatch tasks, job
/// snapshots, and forked graph-branch contexts carry it without borrowing the
/// context it was resolved from.
#[derive(Debug, Clone)]
pub struct ResolvedHook {
    pub name: String,
    /// Whitelist form `<event>.<name>`, e.g. `tool.started.notify`.
    pub full_name: String,
    pub command: String,
    pub cwd: PathBuf,
}

/// Pre-resolved `rag.sync.*` hooks plus the names `base_envs_parts` needs,
/// carried by value into the context-free RAG build funnels (`Rag::init`,
/// `Rag::init_with_config`, `Rag::refresh_document_paths`). Resolution
/// happens where the caller's identity is known; dispatch happens inside the
/// funnel, bracketing the one real sync it performs — so cache hits, pure
/// loads, and attaches (which never enter a funnel) fire nothing.
#[derive(Debug, Clone, Default)]
pub struct RagSyncHooks {
    started: Vec<ResolvedHook>,
    completed: Vec<ResolvedHook>,
    failed: Vec<ResolvedHook>,
    session_name: Option<String>,
    agent_name: Option<String>,
}

impl RagSyncHooks {
    pub fn resolve(ctx: &RequestContext) -> Self {
        Self {
            started: ctx.resolved_hooks(HookEvent::RagSyncStarted),
            completed: ctx.resolved_hooks(HookEvent::RagSyncCompleted),
            failed: ctx.resolved_hooks(HookEvent::RagSyncFailed),
            session_name: ctx
                .session
                .as_ref()
                .map(|session| session.name().to_string()),
            agent_name: ctx.agent.as_ref().map(|agent| agent.name().to_string()),
        }
    }

    /// Resolution for `Agent::init`, which builds agent and graph-node RAGs
    /// before any context carries the agent: the whitelist gate and the
    /// agent's own hooks come straight from the config being constructed.
    /// No session or role exists at that point.
    pub fn resolve_for_agent(
        global_hooks: &HooksMap,
        gate: &[String],
        agent_hooks: &HooksMap,
        agent_name: &str,
    ) -> Self {
        let resolve = |event| {
            resolve_hooks(
                event,
                global_hooks,
                Some((gate, agent_name)),
                None,
                Some((agent_hooks, agent_name)),
            )
        };
        Self {
            started: resolve(HookEvent::RagSyncStarted),
            completed: resolve(HookEvent::RagSyncCompleted),
            failed: resolve(HookEvent::RagSyncFailed),
            session_name: None,
            agent_name: Some(agent_name.to_string()),
        }
    }

    pub fn fire_started(&self, rag_name: &str, rag_path: &str) {
        self.fire(
            HookEvent::RagSyncStarted,
            &self.started,
            rag_name,
            rag_path,
            &[],
        );
    }

    pub fn fire_completed(&self, rag_name: &str, rag_path: &str, doc_count: usize) {
        self.fire(
            HookEvent::RagSyncCompleted,
            &self.completed,
            rag_name,
            rag_path,
            &[("COYOTE_RAG_DOC_COUNT", doc_count.to_string())],
        );
    }

    /// A user abort mid-sync counts as a failure: the funnel's sync came
    /// back `Err` either way, and the knowledge base was not (re)built.
    pub fn fire_failed(&self, rag_name: &str, rag_path: &str, error: &anyhow::Error) {
        self.fire(
            HookEvent::RagSyncFailed,
            &self.failed,
            rag_name,
            rag_path,
            &[("COYOTE_ERROR", format!("{error:#}"))],
        );
    }

    fn fire(
        &self,
        event: HookEvent,
        resolved: &[ResolvedHook],
        rag_name: &str,
        rag_path: &str,
        extras: &[(&str, String)],
    ) {
        if resolved.is_empty() {
            return;
        }
        let base_envs = base_envs_parts(
            event,
            self.session_name.as_deref(),
            self.agent_name.as_deref(),
        );
        let mut envs = vec![
            ("COYOTE_RAG_NAME", rag_name.to_string()),
            ("COYOTE_RAG_PATH", rag_path.to_string()),
        ];
        envs.extend(extras.iter().map(|(key, value)| (*key, value.clone())));
        fire_resolved(event, resolved.to_vec(), base_envs, &envs, None);
    }
}

/// Pre-resolved `mcp.server.*` hooks plus the names `base_envs_parts` needs,
/// carried into the context-free `McpFactory::acquire`. Dispatch happens at
/// the spawn itself — one event per real spawn — so handing back an
/// already-live server fires nothing.
#[derive(Debug, Clone, Default)]
pub struct McpServerHooks {
    connected: Vec<ResolvedHook>,
    failed: Vec<ResolvedHook>,
    session_name: Option<String>,
    agent_name: Option<String>,
}

impl McpServerHooks {
    pub fn resolve(ctx: &RequestContext) -> Self {
        Self {
            connected: ctx.resolved_hooks(HookEvent::McpServerConnected),
            failed: ctx.resolved_hooks(HookEvent::McpServerFailed),
            session_name: ctx
                .session
                .as_ref()
                .map(|session| session.name().to_string()),
            agent_name: ctx.agent.as_ref().map(|agent| agent.name().to_string()),
        }
    }

    /// `reconnect` marks a spawn for a server key that was live earlier in
    /// this process; the variable is omitted entirely on a first connect.
    pub fn fire_connected(&self, server: &str, transport: &str, reconnect: bool) {
        let mut extras = vec![
            ("COYOTE_MCP_SERVER", server.to_string()),
            ("COYOTE_MCP_TRANSPORT", transport.to_string()),
        ];
        if reconnect {
            extras.push(("COYOTE_MCP_RECONNECT", "true".to_string()));
        }
        self.fire(HookEvent::McpServerConnected, &self.connected, &extras);
    }

    /// `auth_required` marks an `McpAuthRequired` failure; the variable is
    /// omitted on every other error.
    pub fn fire_failed(
        &self,
        server: &str,
        transport: &str,
        error: &anyhow::Error,
        auth_required: bool,
    ) {
        let mut extras = vec![
            ("COYOTE_MCP_SERVER", server.to_string()),
            ("COYOTE_MCP_TRANSPORT", transport.to_string()),
            ("COYOTE_ERROR", format!("{error:#}")),
        ];
        if auth_required {
            extras.push(("COYOTE_MCP_AUTH_REQUIRED", "true".to_string()));
        }
        self.fire(HookEvent::McpServerFailed, &self.failed, &extras);
    }

    fn fire(&self, event: HookEvent, resolved: &[ResolvedHook], extras: &[(&str, String)]) {
        if resolved.is_empty() {
            return;
        }
        let base_envs = base_envs_parts(
            event,
            self.session_name.as_deref(),
            self.agent_name.as_deref(),
        );
        fire_resolved(event, resolved.to_vec(), base_envs, extras, None);
    }
}

impl RequestContext {
    /// Hooks that apply to this context for `event`, in firing order: global
    /// (whitelisted through `global_hooks` when an agent is active), then
    /// role, then agent. Duplicate names across scopes all fire.
    ///
    /// Role hooks resolve from the role held directly on the context or,
    /// when the active role was moved into (or restored by) a session, from
    /// the hooks snapshot the session captured via `Session::set_role` /
    /// `Session::load_from_ctx`.
    pub fn resolved_hooks(&self, event: HookEvent) -> Vec<ResolvedHook> {
        resolve_hooks(
            event,
            &self.app.config.hooks,
            self.agent
                .as_ref()
                .map(|agent| (agent.global_hooks(), agent.name())),
            self.role
                .as_ref()
                .and_then(|role| role.hooks())
                .or_else(|| {
                    self.session
                        .as_ref()
                        .and_then(|session| session.role_hooks())
                }),
            self.agent
                .as_ref()
                .map(|agent| (agent.hooks(), agent.name())),
        )
    }
}

fn resolve_hooks(
    event: HookEvent,
    global_hooks: &HooksMap,
    agent_gate: Option<(&[String], &str)>,
    role_hooks: Option<&HooksMap>,
    agent_hooks: Option<(&HooksMap, &str)>,
) -> Vec<ResolvedHook> {
    let event_name = event.as_str();
    let mut resolved = Vec::new();

    let agent_gate = match event {
        HookEvent::EscalationRaised | HookEvent::EscalationAnswered => None,
        _ => agent_gate,
    };

    if let Some(defs) = global_hooks.get(event_name) {
        let cwd = paths::config_dir();
        for def in defs {
            let full_name = format!("{event_name}.{}", def.name);
            let admitted = match agent_gate {
                None => true,
                Some((gate, _)) => gate
                    .iter()
                    .any(|entry| gate_entry_admits(entry, event_name, &def.name, &full_name)),
            };
            if admitted {
                resolved.push(ResolvedHook {
                    name: def.name.clone(),
                    full_name,
                    command: def.command.clone(),
                    cwd: cwd.clone(),
                });
            }
        }
    }
    if let Some((gate, agent_name)) = agent_gate
        && !global_hooks.is_empty()
    {
        let prefix = format!("{event_name}.");
        for entry in gate {
            if entry.contains('*') {
                if !entry_is_valid_wildcard(entry) {
                    debug!(
                        "Ignoring invalid glob global hook whitelist entry '{entry}' for agent '{agent_name}'"
                    );
                }
                continue;
            }
            if let Some(name) = entry.strip_prefix(&prefix) {
                let known = global_hooks
                    .get(event_name)
                    .is_some_and(|defs| defs.iter().any(|def| def.name == name));
                if !known {
                    debug!(
                        "Ignoring unknown global hook whitelist entry '{entry}' for agent '{agent_name}'"
                    );
                }
            }
        }
    }

    if let Some(defs) = role_hooks.and_then(|hooks| hooks.get(event_name)) {
        push_defs(event_name, defs, &paths::roles_dir(), &mut resolved);
    }

    if let Some((hooks, agent_name)) = agent_hooks
        && let Some(defs) = hooks.get(event_name)
    {
        push_defs(
            event_name,
            defs,
            &paths::agent_data_dir(agent_name),
            &mut resolved,
        );
    }

    resolved
}

fn gate_entry_admits(entry: &str, event_name: &str, hook_name: &str, full_name: &str) -> bool {
    if entry == full_name || entry == "*" || entry == "*.*" {
        return true;
    }
    if let Some(name) = entry.strip_prefix("*.") {
        return name == hook_name;
    }
    entry.strip_suffix(".*") == Some(event_name)
}

fn entry_is_valid_wildcard(entry: &str) -> bool {
    if entry == "*" || entry == "*.*" {
        return true;
    }
    if let Some(name) = entry.strip_prefix("*.") {
        return !name.contains('*');
    }
    entry
        .strip_suffix(".*")
        .is_some_and(|event| HookEvent::ALL.iter().any(|known| known.as_str() == event))
}

fn push_defs(event_name: &str, defs: &[HookDef], cwd: &Path, out: &mut Vec<ResolvedHook>) {
    for def in defs {
        out.push(ResolvedHook {
            name: def.name.clone(),
            full_name: format!("{event_name}.{}", def.name),
            command: def.command.clone(),
            cwd: cwd.to_path_buf(),
        });
    }
}

pub fn fire(
    event: HookEvent,
    ctx: &RequestContext,
    extras: &[(&str, String)],
    payload: Option<String>,
) {
    let resolved = ctx.resolved_hooks(event);
    if resolved.is_empty() {
        return;
    }

    fire_resolved(event, resolved, base_envs(event, ctx), extras, payload);
}

/// Dispatches already-resolved hooks, for call sites that outlive the context
/// borrow they resolved from. Same fire-and-forget semantics as [`fire`].
pub fn fire_resolved(
    event: HookEvent,
    resolved: Vec<ResolvedHook>,
    base_envs: Vec<(String, String)>,
    extras: &[(&str, String)],
    payload: Option<String>,
) {
    let runtime = tokio::runtime::Handle::try_current();
    for hook in resolved {
        if hook.command.trim().is_empty() {
            debug!(
                "Skipping hook '{}' (cwd '{}'): empty command",
                hook.full_name,
                hook.cwd.display()
            );
            continue;
        }

        let mut envs = base_envs.clone();
        envs.push(("COYOTE_HOOK_NAME".to_string(), hook.name.clone()));
        envs.extend(
            extras
                .iter()
                .map(|(key, value)| (key.to_string(), value.clone())),
        );
        let envs: Vec<(String, String)> = envs
            .into_iter()
            .map(|(key, mut value)| {
                // Values like escalation questions carry agent-authored text;
                // an interior NUL makes `Command::spawn` fail on Unix, which
                // would let a child suppress its own hooks. Strip them.
                if value.contains('\0') {
                    value.retain(|c| c != '\0');
                }
                // `end` is a char boundary by construction, so `truncate` cannot panic.
                let end = truncate_env_value(&value).len();
                value.truncate(end);
                (key, value)
            })
            .collect();

        #[cfg(test)]
        if test_sink::record(event, &hook, &envs, payload.as_deref()) {
            continue;
        }

        let Ok(runtime) = &runtime else {
            debug!(
                "Skipping hook '{}': no Tokio runtime on this thread",
                hook.full_name
            );
            continue;
        };

        let payload = payload.clone();
        drop(runtime.spawn(run_hook(
            event,
            hook,
            envs,
            payload,
            SpawnAckGuard::register(),
        )));
    }
}

static PENDING_SPAWNS: AtomicUsize = AtomicUsize::new(0);
static SPAWN_NOTIFY: Notify = Notify::const_new();

/// Marks one dispatch as pending until its spawn attempt resolves. The
/// decrement lives in `Drop` so it also fires on panic or when the dispatch
/// task is dropped before it was ever polled.
struct SpawnAckGuard;

impl SpawnAckGuard {
    fn register() -> Self {
        PENDING_SPAWNS.fetch_add(1, Ordering::SeqCst);
        SpawnAckGuard
    }
}

impl Drop for SpawnAckGuard {
    fn drop(&mut self) {
        PENDING_SPAWNS.fetch_sub(1, Ordering::SeqCst);
        SPAWN_NOTIFY.notify_waiters();
    }
}

/// Waits until every in-flight dispatch has finished its spawn attempt —
/// payload file written and `Command::spawn` returned, successfully or not —
/// or until `timeout` elapses. For exit-path call sites only (session end /
/// final turn): this waits for child spawn, never completion. Hook children
/// deliberately outlive coyote as orphans.
pub async fn drain_pending(timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while PENDING_SPAWNS.load(Ordering::SeqCst) > 0 {
        let mut notified = std::pin::pin!(SPAWN_NOTIFY.notified());
        notified.as_mut().enable();
        if PENDING_SPAWNS.load(Ordering::SeqCst) == 0 {
            return;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            debug!(
                "Gave up waiting for {} pending hook dispatch(es) after {timeout:?}",
                PENDING_SPAWNS.load(Ordering::SeqCst)
            );
            return;
        }
    }
}

fn base_envs(event: HookEvent, ctx: &RequestContext) -> Vec<(String, String)> {
    base_envs_parts(
        event,
        ctx.session.as_ref().map(|session| session.name()),
        ctx.agent.as_ref().map(|agent| agent.name()),
    )
}

pub(crate) fn base_envs_parts(
    event: HookEvent,
    session_name: Option<&str>,
    agent_name: Option<&str>,
) -> Vec<(String, String)> {
    let mut envs = vec![
        ("COYOTE_EVENT".to_string(), event.as_str().to_string()),
        (
            "COYOTE_CONFIG_DIR".to_string(),
            paths::config_dir().display().to_string(),
        ),
        (
            "COYOTE_EVENT_TIMESTAMP".to_string(),
            Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        ),
    ];
    if let Some(name) = session_name {
        envs.push(("COYOTE_SESSION_ID".to_string(), name.to_string()));
    }
    if let Some(name) = agent_name {
        envs.push(("COYOTE_AGENT_NAME".to_string(), name.to_string()));
    }
    envs
}

/// `COYOTE_ROLE` extras for the `session.*` and `turn.*` fire sites — the
/// only two families that carry the variable. Resolved at fire time, because
/// roles change mid-session (`.role`, `.exit role`, temp roles): the role
/// held directly on the context wins, then the name of a role a session has
/// absorbed. Derived roles have no name and report nothing.
pub fn role_extras(ctx: &RequestContext) -> Vec<(&'static str, String)> {
    ctx.role
        .as_ref()
        .map(|role| role.name())
        .filter(|name| !name.is_empty())
        .or_else(|| ctx.session.as_ref().and_then(|session| session.role_name()))
        .map(|name| vec![("COYOTE_ROLE", name.to_string())])
        .unwrap_or_default()
}

async fn write_payload_file(path: &Path, json: &str) -> std::io::Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    let written = write_payload_contents(&mut file, json).await;
    if written.is_err() {
        // `create_new` succeeded, so the file is ours: never leave a partial
        // payload orphaned in the shared directory.
        drop(file);
        let _ = tokio::fs::remove_file(path).await;
    }
    written
}

async fn write_payload_contents(file: &mut tokio::fs::File, json: &str) -> std::io::Result<()> {
    #[cfg(test)]
    if PAYLOAD_WRITE_FAILURE.load(Ordering::SeqCst) {
        return Err(std::io::Error::other("injected payload write failure"));
    }
    file.write_all(json.as_bytes()).await?;
    file.flush().await
}

#[cfg(test)]
static PAYLOAD_DIR_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
static PAYLOAD_WRITE_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn payload_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = PAYLOAD_DIR_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return dir;
    }
    std::env::temp_dir()
}

const STALE_PAYLOAD_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub fn sweep_stale_payload_files() {
    sweep_payload_files_older_than(STALE_PAYLOAD_MAX_AGE);
}

fn sweep_payload_files_older_than(max_age: Duration) {
    let dir = payload_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) => {
            debug!(
                "Skipping stale hook-payload sweep in {}: {err}",
                dir.display()
            );
            return;
        }
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("coyote-hook-") || !name.ends_with(".json") {
            continue;
        }
        // DirEntry::metadata does not traverse symlinks, so a planted link
        // is skipped rather than followed.
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age >= max_age);
        if !stale {
            continue;
        }

        match std::fs::remove_file(entry.path()) {
            Ok(()) => debug!("Removed stale hook payload file {}", entry.path().display()),
            Err(err) => debug!(
                "Failed to remove stale hook payload file {}: {err}",
                entry.path().display()
            ),
        }
    }
}

async fn run_hook(
    event: HookEvent,
    hook: ResolvedHook,
    mut envs: Vec<(String, String)>,
    payload: Option<String>,
    ack: SpawnAckGuard,
) {
    let payload_file = match payload {
        Some(json) => {
            let nonce = Alphanumeric.sample_string(&mut rand::rng(), 16);
            let path = payload_dir().join(format!("coyote-hook-{}-{nonce}.json", event.as_str()));
            match write_payload_file(&path, &json).await {
                Ok(()) => Some(path),
                Err(err) => {
                    debug!(
                        "Failed to write payload file for hook '{}': {err}",
                        hook.full_name
                    );
                    None
                }
            }
        }
        None => None,
    };
    if let Some(path) = &payload_file {
        envs.push((
            "COYOTE_HOOK_PAYLOAD_FILE".to_string(),
            path.display().to_string(),
        ));
    }

    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new("sh");
        command.arg("-c").arg(&hook.command);
        command
    };
    // cmd.exe does not parse CommandLineToArgvW quoting, so the command line
    // must be passed verbatim rather than through arg()'s quoting rules.
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("cmd");
        command.arg("/C");
        command.as_std_mut().raw_arg(&hook.command);
        command
    };

    command
        .envs(envs)
        .current_dir(&hook.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A fresh process group keeps a terminal Ctrl-C aimed at coyote from
    // signalling hook children, which deliberately outlive coyote.
    #[cfg(unix)]
    command.process_group(0);
    match command.spawn() {
        Ok(mut child) => {
            drop(ack);
            let _ = child.wait().await;
        }
        Err(err) => {
            debug!(
                "Failed to spawn hook '{}' in '{}': {err}",
                hook.full_name,
                hook.cwd.display()
            );
            drop(ack);
        }
    }

    if let Some(path) = payload_file {
        let _ = tokio::fs::remove_file(path).await;
    }
}

/// Truncates an env value to 2048 bytes without splitting a UTF-8 character.
/// `fire_resolved` applies it to every env value it dispatches, with one
/// exemption: `COYOTE_HOOK_PAYLOAD_FILE` is pushed by `run_hook` after the
/// truncation pass, because that value is engine-generated, inherently short,
/// and a truncated path would be corrupt rather than merely trimmed. Exported
/// for call sites that need the bound before dispatch.
pub fn truncate_env_value(value: &str) -> &str {
    const MAX_ENV_VALUE_BYTES: usize = 2048;
    if value.len() <= MAX_ENV_VALUE_BYTES {
        return value;
    }
    let mut end = MAX_ENV_VALUE_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
pub(crate) mod test_sink {
    use super::{HookEvent, ResolvedHook};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static INSTALLED: AtomicUsize = AtomicUsize::new(0);
    // A test that panics while holding the buffer poisons its mutex; every
    // lock site recovers via `into_inner` so later tests keep working.
    pub(super) static CAPTURES: Mutex<Vec<Capture>> = Mutex::new(Vec::new());

    /// One dispatch the sink recorded instead of spawning.
    #[derive(Debug, Clone)]
    pub struct Capture {
        pub event: HookEvent,
        pub hook_name: String,
        pub envs: HashMap<String, String>,
        /// The tool-arguments JSON as handed to dispatch; `None` for events
        /// that carry no payload.
        pub payload: Option<String>,
        /// The working directory the hook would have spawned in.
        pub cwd: std::path::PathBuf,
    }

    /// Suppresses process spawning and records every dispatch synchronously
    /// until the returned guard drops. The capture buffer is process-global
    /// and shared by all tests, so count assertions must either use hook
    /// names unique to the test fixture and filter captures, or serialize
    /// with `#[serial_test::serial]`. Tests that rely on real spawning must
    /// also serialize against tests that install the sink.
    #[must_use]
    pub fn install() -> SinkGuard {
        INSTALLED.fetch_add(1, Ordering::SeqCst);
        SinkGuard
    }

    pub struct SinkGuard;

    impl Drop for SinkGuard {
        fn drop(&mut self) {
            INSTALLED.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub(super) fn record(
        event: HookEvent,
        hook: &ResolvedHook,
        envs: &[(String, String)],
        payload: Option<&str>,
    ) -> bool {
        if INSTALLED.load(Ordering::SeqCst) == 0 {
            return false;
        }
        CAPTURES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Capture {
                event,
                hook_name: hook.name.clone(),
                envs: envs.iter().cloned().collect(),
                payload: payload.map(str::to_string),
                cwd: hook.cwd.clone(),
            });
        true
    }

    /// Removes and returns everything recorded so far.
    pub fn drain() -> Vec<Capture> {
        std::mem::take(
            &mut *CAPTURES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }

    /// A copy of everything recorded so far, for tests that filter by a hook
    /// name unique to their fixture.
    pub fn snapshot() -> Vec<Capture> {
        CAPTURES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::conflict::prompt_script;
    use crate::config::{AppConfig, AppState, Role, WorkingMode};
    use crate::{testing, utils};
    use serial_test::serial;
    use std::env;
    use std::sync::Arc;
    use std::time::UNIX_EPOCH;

    fn hooks_map(event: &str, defs: &[(&str, &str)]) -> HooksMap {
        let defs = defs
            .iter()
            .map(|(name, command)| HookDef {
                name: name.to_string(),
                command: command.to_string(),
            })
            .collect();
        IndexMap::from([(event.to_string(), defs)])
    }

    fn names(resolved: &[ResolvedHook]) -> Vec<&str> {
        resolved.iter().map(|hook| hook.name.as_str()).collect()
    }

    fn ctx_with_global_hooks(hooks: HooksMap) -> RequestContext {
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            hooks,
            ..Default::default()
        });
        RequestContext::new(Arc::new(app), WorkingMode::Cmd)
    }

    /// Points the hooks dir at a fresh temp directory for the guard's
    /// lifetime and removes it on drop. Tests using it must serialize.
    struct HooksDirGuard {
        _env: testing::EnvVarGuard,
        root: PathBuf,
    }

    impl HooksDirGuard {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = env::temp_dir().join(format!("coyote-{label}-{unique}"));
            Self {
                _env: testing::EnvVarGuard::set(utils::get_env_name("hooks_dir"), &root),
                root,
            }
        }
    }

    impl Drop for HooksDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Points `run_hook`'s payload directory at `path` for the guard's
    /// lifetime, so payload writes can be aimed at a controlled directory
    /// without touching the process-global `TMPDIR`.
    struct PayloadDirOverrideGuard;

    impl PayloadDirOverrideGuard {
        fn new(path: PathBuf) -> Self {
            *PAYLOAD_DIR_OVERRIDE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
            Self
        }
    }

    impl Drop for PayloadDirOverrideGuard {
        fn drop(&mut self) {
            *PAYLOAD_DIR_OVERRIDE
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
    }

    /// Makes `write_payload_file` fail after the `create_new` open succeeds,
    /// simulating a write error on the already-created file.
    struct PayloadWriteFailureGuard;

    impl PayloadWriteFailureGuard {
        fn install() -> Self {
            PAYLOAD_WRITE_FAILURE.store(true, Ordering::SeqCst);
            Self
        }
    }

    impl Drop for PayloadWriteFailureGuard {
        fn drop(&mut self) {
            PAYLOAD_WRITE_FAILURE.store(false, Ordering::SeqCst);
        }
    }

    fn fresh_payload_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("coyote-{label}-{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    #[serial]
    async fn payload_write_failure_after_creation_unlinks_the_orphan() {
        let dir = fresh_payload_dir("payload-orphan");
        let path = dir.join("coyote-hook-tool.started-orphan.json");
        let _fail = PayloadWriteFailureGuard::install();

        let result = write_payload_file(&path, r#"{"probe":true}"#).await;

        assert!(result.is_err());
        assert!(
            !path.exists(),
            "a payload file created before the write failed must be unlinked"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn stale_payload_sweep_removes_only_matching_stale_files() {
        let dir = fresh_payload_dir("payload-sweep");
        let _payload_dir = PayloadDirOverrideGuard::new(dir.clone());
        let stale = dir.join("coyote-hook-tool.started-abc123.json");
        std::fs::write(&stale, "{}").unwrap();
        // Everything outside the exact `coyote-hook-*.json` pattern survives,
        // whatever its age: the payload dir is shared.
        let bystanders = [
            dir.join("coyote-hook-tool.started-abc123.json.bak"),
            dir.join("coyote-hookless.json"),
            dir.join("other.json"),
            dir.join("coyote-hook-note.txt"),
        ];
        for path in &bystanders {
            std::fs::write(path, "keep").unwrap();
        }
        std::fs::create_dir_all(dir.join("coyote-hook-decoy-dir.json")).unwrap();

        // Zero max age marks every matching file stale without depending on
        // the wall clock.
        sweep_payload_files_older_than(Duration::ZERO);

        assert!(!stale.exists(), "a stale payload file must be removed");
        for path in &bystanders {
            assert!(path.exists(), "{} must survive the sweep", path.display());
        }
        assert!(dir.join("coyote-hook-decoy-dir.json").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn stale_payload_sweep_keeps_files_younger_than_the_threshold() {
        let dir = fresh_payload_dir("payload-sweep-young");
        let _payload_dir = PayloadDirOverrideGuard::new(dir.clone());
        let young = dir.join("coyote-hook-tool.started-young.json");
        std::fs::write(&young, "{}").unwrap();

        sweep_payload_files_older_than(STALE_PAYLOAD_MAX_AGE);

        assert!(
            young.exists(),
            "a freshly written payload file must never be swept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[serial]
    fn stale_payload_sweep_tolerates_a_missing_directory() {
        let dir = env::temp_dir().join("coyote-payload-sweep-missing-dir");
        let _ = std::fs::remove_dir_all(&dir);
        let _payload_dir = PayloadDirOverrideGuard::new(dir);

        sweep_stale_payload_files();
    }

    #[test]
    #[serial]
    fn install_builtin_hooks_installs_executable_scripts_and_honors_force() {
        let env_name = utils::get_env_name("hooks_dir");
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-hooks-install-{unique}"));
        let env_guard = testing::EnvVarGuard::set(&env_name, &root);

        // Capture every outcome first and assert only after cleanup: the
        // guard restores the env var even on panic, but the temp dir removal
        // below still has to run before any assertion can bail out.
        let notify = root.join("notify.sh");
        let log_events = root.join("log-events.sh");
        let fresh = install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None);
        let installed = (notify.is_file(), log_events.is_file());
        #[cfg(unix)]
        let modes: Vec<std::io::Result<u32>> = {
            use std::os::unix::fs::PermissionsExt;
            [&notify, &log_events]
                .iter()
                .map(|path| std::fs::metadata(path).map(|meta| meta.permissions().mode() & 0o777))
                .collect()
        };
        let modified = std::fs::write(&notify, "modified");
        let no_force = install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None);
        let after_no_force = std::fs::read_to_string(&notify);
        let force = install_builtin_hooks(InstallMode::Force, &mut StickyMode::None);
        let after_force = std::fs::read_to_string(&notify);

        drop(env_guard);
        let _ = std::fs::remove_dir_all(&root);

        fresh.unwrap();
        assert!(installed.0, "notify.sh must be installed");
        assert!(installed.1, "log-events.sh must be installed");
        #[cfg(unix)]
        for (path, mode) in [&notify, &log_events].into_iter().zip(modes) {
            assert_eq!(
                mode.unwrap(),
                0o755,
                "{} must be executable",
                path.display()
            );
        }
        modified.unwrap();
        no_force.unwrap();
        assert_eq!(after_no_force.unwrap(), "modified");
        force.unwrap();
        assert!(after_force.unwrap().starts_with("#!/usr/bin/env bash"));
    }

    #[test]
    #[serial]
    fn install_builtin_hooks_skip_mode_never_prompts_even_on_a_terminal() {
        let guard = HooksDirGuard::new("hooks-skip-no-prompt");
        install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();
        let notify = guard.root.join("notify.sh");
        std::fs::write(&notify, "modified").unwrap();

        // A forced terminal with no scripted answers: any prompt would panic
        // and the counter would move.
        let _script = prompt_script::install(&[]);
        install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();

        assert_eq!(prompt_script::prompts_asked(), 0);
        assert_eq!(std::fs::read_to_string(&notify).unwrap(), "modified");
    }

    #[test]
    #[serial]
    fn install_builtin_hooks_prompt_mode_asks_only_for_differing_files() {
        let guard = HooksDirGuard::new("hooks-prompt-conflict");
        install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();
        let notify = guard.root.join("notify.sh");
        std::fs::write(&notify, "modified").unwrap();

        // log-events.sh is identical to the embed, so only notify.sh asks.
        let script = prompt_script::install(&["keep"]);
        install_builtin_hooks(InstallMode::Prompt, &mut StickyMode::None).unwrap();
        assert_eq!(prompt_script::prompts_asked(), 1);
        assert_eq!(std::fs::read_to_string(&notify).unwrap(), "modified");
        drop(script);

        let script = prompt_script::install(&["replace"]);
        install_builtin_hooks(InstallMode::Prompt, &mut StickyMode::None).unwrap();
        assert_eq!(prompt_script::prompts_asked(), 1);
        assert!(
            std::fs::read_to_string(&notify)
                .unwrap()
                .starts_with("#!/usr/bin/env bash")
        );
        drop(script);
    }

    #[test]
    #[serial]
    fn install_builtin_hooks_prompt_mode_writes_missing_files_without_asking() {
        let guard = HooksDirGuard::new("hooks-prompt-missing");
        let _script = prompt_script::install(&[]);

        install_builtin_hooks(InstallMode::Prompt, &mut StickyMode::None).unwrap();

        assert_eq!(prompt_script::prompts_asked(), 0);
        assert!(guard.root.join("notify.sh").is_file());
        assert!(guard.root.join("log-events.sh").is_file());
    }

    #[test]
    #[serial]
    fn install_builtin_hooks_prompt_mode_keeps_local_files_without_a_terminal() {
        let guard = HooksDirGuard::new("hooks-prompt-non-tty");
        install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();
        let notify = guard.root.join("notify.sh");
        std::fs::write(&notify, "modified").unwrap();

        let _script = prompt_script::install_non_interactive();
        install_builtin_hooks(InstallMode::Prompt, &mut StickyMode::None).unwrap();

        assert_eq!(prompt_script::prompts_asked(), 0);
        assert_eq!(std::fs::read_to_string(&notify).unwrap(), "modified");
    }

    #[test]
    #[serial]
    fn install_builtin_hooks_keep_all_covers_every_remaining_conflict() {
        let guard = HooksDirGuard::new("hooks-prompt-keep-all");
        install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();
        let notify = guard.root.join("notify.sh");
        let log_events = guard.root.join("log-events.sh");
        std::fs::write(&notify, "modified notify").unwrap();
        std::fs::write(&log_events, "modified log-events").unwrap();

        let _script = prompt_script::install(&["keep-all"]);
        install_builtin_hooks(InstallMode::Prompt, &mut StickyMode::None).unwrap();

        assert_eq!(prompt_script::prompts_asked(), 1);
        assert_eq!(std::fs::read_to_string(&notify).unwrap(), "modified notify");
        assert_eq!(
            std::fs::read_to_string(&log_events).unwrap(),
            "modified log-events"
        );
    }

    #[test]
    #[serial]
    fn install_builtin_hooks_removes_stale_shipped_hooks_via_manifest() {
        use crate::config::builtin_manifest::BUILTIN_MANIFEST_FILE;

        let guard = HooksDirGuard::new("hooks-manifest-stale");
        install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();

        // Simulate an upgrade from a release that also shipped old-hook.sh:
        // the manifest records it, so reinstall removes it — while a user
        // script absent from the manifest survives.
        std::fs::write(
            guard.root.join(BUILTIN_MANIFEST_FILE),
            "log-events.sh\nnotify.sh\nold-hook.sh\n",
        )
        .unwrap();
        std::fs::write(guard.root.join("old-hook.sh"), "stale").unwrap();
        std::fs::write(guard.root.join("user-hook.sh"), "user-owned").unwrap();

        install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();

        assert!(!guard.root.join("old-hook.sh").exists());
        assert_eq!(
            std::fs::read_to_string(guard.root.join("user-hook.sh")).unwrap(),
            "user-owned"
        );
        assert_eq!(
            std::fs::read_to_string(guard.root.join(BUILTIN_MANIFEST_FILE)).unwrap(),
            "log-events.sh\nnotify.sh\n"
        );
    }

    #[test]
    #[serial]
    fn install_builtin_hooks_malformed_manifest_deletes_nothing() {
        use crate::config::builtin_manifest::BUILTIN_MANIFEST_FILE;

        let guard = HooksDirGuard::new("hooks-manifest-malformed");
        install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();
        std::fs::write(guard.root.join(BUILTIN_MANIFEST_FILE), [0xff, 0xfe, 0x00]).unwrap();
        std::fs::write(guard.root.join("old-hook.sh"), "keep").unwrap();

        install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();

        assert!(
            guard.root.join("old-hook.sh").exists(),
            "an unreadable manifest must fail safe toward keeping files"
        );
    }

    #[test]
    #[serial]
    fn install_builtin_hooks_respelled_default_hooks_dir_does_not_warn() {
        testing::install_log_collector();
        let guard = testing::TestConfigDirGuard::new("hooks-warn-respelled");
        let guard_path_display = guard.path.display().to_string();
        let env_name = utils::get_env_name("hooks_dir");

        // An override that reaches the default `config_dir()/hooks` through a
        // `..` hop is the default path in disguise: the canonicalized compare
        // must stay quiet even though the strings differ.
        let alias = guard.path.join("hooks").join("..").join("hooks");
        let env_guard = testing::EnvVarGuard::set(&env_name, &alias);
        install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();
        drop(env_guard);

        let default_notify = guard.path.join("hooks").join("notify.sh");
        assert!(
            default_notify.is_file(),
            "scripts must land in the default dir, so wrote_any was true"
        );

        // Same disguise via a symlink. One shipped script is removed first so
        // the run rewrites it: a no-op install would skip the warning check
        // entirely and prove nothing about the compare.
        #[cfg(unix)]
        {
            let link = guard.path.join("hooks-link");
            std::os::unix::fs::symlink(guard.path.join("hooks"), &link).unwrap();
            std::fs::remove_file(&default_notify).unwrap();
            let _env_guard = testing::EnvVarGuard::set(&env_name, &link);
            install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();
            assert!(
                default_notify.is_file(),
                "the removed script must be reinstalled, so wrote_any was true"
            );
        }

        // The warn buffer is process-global, so scope the check to messages
        // naming this test's unique directory.
        let warns = testing::warn_snapshot();
        assert!(
            warns.iter().all(|message| {
                !(message.contains("overrides the hooks dir")
                    && message.contains(&guard_path_display))
            }),
            "a respelled default hooks dir must not warn: {warns:?}"
        );
    }

    #[test]
    #[serial]
    fn install_builtin_hooks_genuine_override_warns_when_default_dir_is_missing() {
        testing::install_log_collector();
        let guard = testing::TestConfigDirGuard::new("hooks-warn-genuine");
        let override_dir = guard.path.join("elsewhere-hooks");
        let _env_guard = testing::EnvVarGuard::set(utils::get_env_name("hooks_dir"), &override_dir);

        install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();

        // Fresh install: nothing ever created the default dir, so its
        // canonicalization fails and the compare falls back to the original
        // path. A genuine override must still be detected.
        assert!(
            !guard.path.join("hooks").exists(),
            "the default hooks dir must not exist for this scenario"
        );
        assert!(
            override_dir.join("notify.sh").is_file(),
            "scripts must land in the override dir, so wrote_any was true"
        );
        let override_display = override_dir.display().to_string();
        let warns = testing::warn_snapshot();
        assert!(
            warns.iter().any(|message| {
                message.contains("overrides the hooks dir") && message.contains(&override_display)
            }),
            "a genuine override must warn even without a default dir: {warns:?}"
        );
    }

    /// The example hooks are plain scripts, not argc tools: they live outside
    /// `assets/functions/tools/`, so the argc regeneration that runs during
    /// tests must never have stamped them with an ARGC-BUILD block.
    #[test]
    fn builtin_hook_assets_are_not_argc_tools() {
        let files: Vec<String> = HookAssets::iter()
            .map(|file| file.as_ref().to_string())
            .collect();
        assert!(files.contains(&"notify.sh".to_string()), "{files:?}");
        assert!(files.contains(&"log-events.sh".to_string()), "{files:?}");

        for file in HookAssets::iter() {
            let embedded = HookAssets::get(&file).unwrap();
            let content = std::str::from_utf8(&embedded.data).unwrap();
            assert!(
                !content.contains("ARGC-BUILD"),
                "{} must not contain an ARGC-BUILD block",
                file.as_ref()
            );
            assert!(
                !content.contains("# @cmd"),
                "{} must not use argc annotations",
                file.as_ref()
            );
        }
    }

    #[test]
    fn notify_script_prefers_notify_send_then_osascript_then_echo() {
        let embedded = HookAssets::get("notify.sh").unwrap();
        let content = std::str::from_utf8(&embedded.data).unwrap();
        let notify_send = content.find("notify-send").expect("notify-send branch");
        let osascript = content.find("osascript").expect("osascript branch");
        let echo = content.find("echo \"$safe_line\"").expect("echo fallback");
        assert!(notify_send < osascript);
        assert!(osascript < echo);
    }

    #[test]
    fn log_events_script_defaults_to_xdg_state_log_behind_env_override() {
        let embedded = HookAssets::get("log-events.sh").unwrap();
        let content = std::str::from_utf8(&embedded.data).unwrap();
        assert!(content.contains("COYOTE_HOOK_LOG"));
        assert!(content.contains("${XDG_STATE_HOME:-$HOME/.local/state}/coyote/hooks.log"));
    }

    #[cfg(unix)]
    #[test]
    fn log_events_script_appends_event_and_env_snapshot() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/hooks/log-events.sh");
        let log =
            env::temp_dir().join(format!("coyote-log-events-test-{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);

        let status = std::process::Command::new("bash")
            .arg(&script)
            .env("COYOTE_HOOK_LOG", &log)
            .env("COYOTE_EVENT", "turn.completed")
            .env("COYOTE_AGENT_NAME", "demo-agent")
            .env("COYOTE_SECRET_DEMO", "hunter2")
            .status()
            .unwrap();

        assert!(status.success());
        let logged = std::fs::read_to_string(&log).unwrap();
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&log).unwrap().permissions().mode()
        };
        let _ = std::fs::remove_file(&log);
        assert!(logged.contains("turn.completed"), "{logged}");
        assert!(logged.contains("COYOTE_EVENT=turn.completed"), "{logged}");
        assert!(logged.contains("COYOTE_AGENT_NAME=demo-agent"), "{logged}");
        assert!(!logged.contains("COYOTE_SECRET_"), "{logged}");
        assert!(!logged.contains("hunter2"), "{logged}");
        assert_eq!(mode & 0o777, 0o600, "log file should be created 0600");
    }

    #[cfg(unix)]
    #[test]
    fn notify_script_falls_back_to_echo_without_notifiers() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/hooks/notify.sh");
        // A PATH exposing only tr (needed by the fallback's sanitizer) hides
        // notify-send and osascript; echo is a bash builtin, so only the
        // fallback branch can produce output.
        let tr = ["/usr/bin/tr", "/bin/tr"]
            .into_iter()
            .map(Path::new)
            .find(|path| path.exists())
            .expect("tr binary");
        let bin =
            env::temp_dir().join(format!("coyote-notify-fallback-bin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bin);
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(tr, bin.join("tr")).unwrap();
        let mut cmd = std::process::Command::new("/bin/bash");
        cmd.arg(&script)
            .env("PATH", &bin)
            .env("COYOTE_EVENT", "turn.completed")
            .env("COYOTE_TOOL_NAME", "demo\u{1b}]0;evil\u{7}");
        // Detach from any controlling terminal so the script cannot open
        // /dev/tty and must fall back to captured stdout.
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let output = cmd.output().unwrap();
        let _ = std::fs::remove_dir_all(&bin);

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("turn.completed"), "{stdout}");
        assert!(stdout.contains("tool=demo"), "{stdout}");
        assert!(
            !stdout.contains('\u{1b}') && !stdout.contains('\u{7}'),
            "the fallback must strip control bytes before echoing: {stdout:?}"
        );
    }

    #[test]
    fn hook_event_names_are_dotted() {
        let cases = [
            (HookEvent::TurnStarted, "turn.started"),
            (HookEvent::TurnCompleted, "turn.completed"),
            (HookEvent::TurnInterrupted, "turn.interrupted"),
            (HookEvent::TurnFailed, "turn.failed"),
            (HookEvent::SessionStarted, "session.started"),
            (HookEvent::SessionResumed, "session.resumed"),
            (HookEvent::SessionEnded, "session.ended"),
            (HookEvent::SessionCompressed, "session.compressed"),
            (HookEvent::ToolStarted, "tool.started"),
            (HookEvent::ToolCompleted, "tool.completed"),
            (HookEvent::ToolFailed, "tool.failed"),
            (HookEvent::LlmRequestStarted, "llm.request.started"),
            (HookEvent::LlmRequestCompleted, "llm.request.completed"),
            (HookEvent::LlmRequestFailed, "llm.request.failed"),
            (HookEvent::AgentStarted, "agent.started"),
            (HookEvent::AgentCompleted, "agent.completed"),
            (HookEvent::AgentInterrupted, "agent.interrupted"),
            (HookEvent::AgentFailed, "agent.failed"),
            (HookEvent::EscalationRaised, "escalation.raised"),
            (HookEvent::EscalationAnswered, "escalation.answered"),
            (HookEvent::GraphNodeStarted, "graph.node.started"),
            (HookEvent::GraphNodeCompleted, "graph.node.completed"),
            (HookEvent::GraphNodeFailed, "graph.node.failed"),
            (HookEvent::JobStarted, "job.started"),
            (HookEvent::JobCompleted, "job.completed"),
            (HookEvent::JobFailed, "job.failed"),
            (HookEvent::RagSyncStarted, "rag.sync.started"),
            (HookEvent::RagSyncCompleted, "rag.sync.completed"),
            (HookEvent::RagSyncFailed, "rag.sync.failed"),
            (HookEvent::McpServerConnected, "mcp.server.connected"),
            (HookEvent::McpServerFailed, "mcp.server.failed"),
        ];
        for (event, name) in cases {
            assert_eq!(event.as_str(), name);
            assert!(
                HookEvent::ALL.contains(&event),
                "HookEvent::ALL is missing {name}"
            );
        }
        assert_eq!(HookEvent::ALL.len(), cases.len());
    }

    #[test]
    fn rag_and_mcp_wildcard_whitelist_entries_are_valid() {
        for event in [
            "rag.sync.started",
            "rag.sync.completed",
            "rag.sync.failed",
            "mcp.server.connected",
            "mcp.server.failed",
        ] {
            assert!(
                entry_is_valid_wildcard(&format!("{event}.*")),
                "'{event}.*' must validate as an <event>.* wildcard"
            );
        }
        // Family prefixes are not events: like `tool.*`, these are partial
        // globs that admit nothing.
        assert!(!entry_is_valid_wildcard("rag.sync.*"));
        assert!(!entry_is_valid_wildcard("mcp.server.*"));
    }

    #[test]
    fn rag_and_mcp_events_stay_behind_the_agent_whitelist_gate() {
        let mut global = hooks_map("rag.sync.started", &[("watch", "cmd-rag")]);
        global.extend(hooks_map("mcp.server.connected", &[("watch", "cmd-mcp")]));

        for event in [HookEvent::RagSyncStarted, HookEvent::McpServerConnected] {
            let resolved = resolve_hooks(event, &global, Some((&[], "gated-agent")), None, None);
            assert!(
                resolved.is_empty(),
                "{} must stay whitelist-gated for agents",
                event.as_str()
            );
        }

        let gate = vec![
            "rag.sync.started.*".to_string(),
            "mcp.server.connected.*".to_string(),
        ];
        for event in [HookEvent::RagSyncStarted, HookEvent::McpServerConnected] {
            let resolved = resolve_hooks(event, &global, Some((&gate, "gated-agent")), None, None);
            assert_eq!(
                names(&resolved),
                ["watch"],
                "an `<event>.*` whitelist entry must admit {}",
                event.as_str()
            );
        }
    }

    #[test]
    fn root_context_gets_all_global_hooks() {
        let mut global = hooks_map("turn.started", &[("a", "cmd-a"), ("b", "cmd-b")]);
        global.extend(hooks_map("turn.completed", &[("other", "cmd-other")]));

        let resolved = resolve_hooks(HookEvent::TurnStarted, &global, None, None, None);

        assert_eq!(names(&resolved), ["a", "b"]);
        assert_eq!(resolved[0].full_name, "turn.started.a");
    }

    #[test]
    fn agent_without_whitelist_gets_no_global_hooks() {
        let global = hooks_map("turn.started", &[("a", "cmd-a")]);
        let agent = hooks_map("turn.started", &[("own", "agent-cmd")]);

        let resolved = resolve_hooks(
            HookEvent::TurnStarted,
            &global,
            Some((&[], "plain-agent")),
            None,
            Some((&agent, "plain-agent")),
        );

        assert_eq!(names(&resolved), ["own"]);
    }

    #[test]
    fn escalation_events_bypass_the_agent_whitelist_gate() {
        let mut global = hooks_map("escalation.raised", &[("watch", "cmd-raised")]);
        global.extend(hooks_map(
            "escalation.answered",
            &[("watch", "cmd-answered")],
        ));
        global.extend(hooks_map("tool.started", &[("watch", "cmd-tool")]));
        let mut role = hooks_map("escalation.raised", &[("role-watch", "role-raised")]);
        role.extend(hooks_map(
            "escalation.answered",
            &[("role-watch", "role-answered")],
        ));
        let mut agent = hooks_map("escalation.raised", &[("own-watch", "agent-raised")]);
        agent.extend(hooks_map(
            "escalation.answered",
            &[("own-watch", "agent-answered")],
        ));

        for event in [HookEvent::EscalationRaised, HookEvent::EscalationAnswered] {
            let resolved = resolve_hooks(
                event,
                &global,
                Some((&[], "gated-agent")),
                Some(&role),
                Some((&agent, "gated-agent")),
            );
            assert_eq!(
                names(&resolved),
                ["watch", "role-watch", "own-watch"],
                "{} must resolve global, role, and agent hooks without a whitelist entry",
                event.as_str()
            );
        }

        let resolved = resolve_hooks(
            HookEvent::ToolStarted,
            &global,
            Some((&[], "gated-agent")),
            None,
            None,
        );
        assert!(
            resolved.is_empty(),
            "non-escalation events stay behind the whitelist gate"
        );
    }

    #[test]
    fn whitelist_admits_only_exact_global_entries() {
        let global = hooks_map(
            "turn.started",
            &[("a", "cmd-a"), ("b", "cmd-b"), ("c", "cmd-c")],
        );
        let empty = HooksMap::default();
        let gate = vec![
            "turn.started.b".to_string(),
            "turn.started.missing".to_string(),
            "other.event.x".to_string(),
        ];

        let resolved = resolve_hooks(
            HookEvent::TurnStarted,
            &global,
            Some((&gate, "gated-agent")),
            None,
            Some((&empty, "gated-agent")),
        );

        assert_eq!(names(&resolved), ["b"]);
    }

    #[test]
    fn event_star_wildcard_admits_every_hook_of_that_event() {
        let mut global = hooks_map("turn.started", &[("a", "cmd-a"), ("b", "cmd-b")]);
        global.extend(hooks_map("turn.completed", &[("other", "cmd-other")]));
        let gate = vec!["turn.started.*".to_string()];

        let started = resolve_hooks(
            HookEvent::TurnStarted,
            &global,
            Some((&gate, "gated-agent")),
            None,
            None,
        );
        assert_eq!(names(&started), ["a", "b"]);

        let completed = resolve_hooks(
            HookEvent::TurnCompleted,
            &global,
            Some((&gate, "gated-agent")),
            None,
            None,
        );
        assert!(
            completed.is_empty(),
            "'turn.started.*' must not admit hooks of other events"
        );
    }

    #[test]
    fn star_name_wildcard_admits_that_hook_on_every_event() {
        let mut global = hooks_map("turn.started", &[("notify", "cmd-1"), ("other", "cmd-2")]);
        global.extend(hooks_map("tool.completed", &[("notify", "cmd-3")]));
        let gate = vec!["*.notify".to_string()];

        for event in [HookEvent::TurnStarted, HookEvent::ToolCompleted] {
            let resolved = resolve_hooks(event, &global, Some((&gate, "gated-agent")), None, None);
            assert_eq!(
                names(&resolved),
                ["notify"],
                "'*.notify' must admit exactly that hook name on {}",
                event.as_str()
            );
        }
    }

    #[test]
    fn star_and_star_dot_star_admit_everything() {
        let mut global = hooks_map("turn.started", &[("a", "cmd-a"), ("b", "cmd-b")]);
        global.extend(hooks_map("llm.request.failed", &[("c", "cmd-c")]));

        for entry in ["*", "*.*"] {
            let gate = vec![entry.to_string()];
            let started = resolve_hooks(
                HookEvent::TurnStarted,
                &global,
                Some((&gate, "gated-agent")),
                None,
                None,
            );
            assert_eq!(names(&started), ["a", "b"], "gate entry '{entry}'");

            let failed = resolve_hooks(
                HookEvent::LlmRequestFailed,
                &global,
                Some((&gate, "gated-agent")),
                None,
                None,
            );
            assert_eq!(names(&failed), ["c"], "gate entry '{entry}'");
        }
    }

    // Event names contain dots, so `tool.*` names a nonexistent event
    // `tool`, not a prefix over `tool.started`/`tool.completed` — like any
    // other partial glob it admits nothing.
    #[test]
    fn partial_globs_admit_nothing() {
        let global = hooks_map("tool.started", &[("st", "cmd-st"), ("stat", "cmd-stat")]);
        let gate = vec![
            "tool.st*".to_string(),
            "tool.started.st*".to_string(),
            "tool.*".to_string(),
        ];

        let resolved = resolve_hooks(
            HookEvent::ToolStarted,
            &global,
            Some((&gate, "gated-agent")),
            None,
            None,
        );

        assert!(resolved.is_empty(), "partial globs are not wildcards");
    }

    #[test]
    fn escalation_events_stay_ungated_under_wildcard_whitelists() {
        let mut global = hooks_map("escalation.raised", &[("watch", "cmd-raised")]);
        global.extend(hooks_map(
            "escalation.answered",
            &[("watch", "cmd-answered")],
        ));
        // A whitelist naming only unrelated events must not narrow the
        // escalation resolution: the gate is nulled before any matching.
        let gate = vec!["turn.started.*".to_string()];

        for event in [HookEvent::EscalationRaised, HookEvent::EscalationAnswered] {
            let resolved = resolve_hooks(event, &global, Some((&gate, "gated-agent")), None, None);
            assert_eq!(
                names(&resolved),
                ["watch"],
                "{} must resolve ungated regardless of whitelist content",
                event.as_str()
            );
        }
    }

    #[test]
    fn wildcard_whitelist_entries_skip_unknown_entry_diagnostics() {
        testing::install_log_collector();
        let global = hooks_map("turn.started", &[("a", "cmd-a")]);
        let gate = vec![
            "turn.started.*".to_string(),
            "*".to_string(),
            "*.*".to_string(),
            "*.some-name".to_string(),
        ];

        let resolved = resolve_hooks(
            HookEvent::TurnStarted,
            &global,
            Some((&gate, "wildcard-exempt-agent-q9z")),
            None,
            None,
        );

        assert_eq!(names(&resolved), ["a"]);
        let debugs = testing::debug_snapshot();
        assert!(
            debugs
                .iter()
                .all(|message| !message.contains("wildcard-exempt-agent-q9z")),
            "wildcard entries must not be flagged as unknown"
        );
    }

    #[test]
    fn partial_glob_whitelist_entries_stay_flagged_as_unknown() {
        testing::install_log_collector();
        let global = hooks_map("turn.started", &[("a", "cmd-a")]);
        let gate = vec!["turn.started.st*-marker-k4w".to_string()];

        let resolved = resolve_hooks(
            HookEvent::TurnStarted,
            &global,
            Some((&gate, "glob-flag-agent-k4w")),
            None,
            None,
        );

        assert!(resolved.is_empty(), "an invalid glob must admit nothing");
        let debugs = testing::debug_snapshot();
        assert!(debugs.iter().any(|message| {
            message.contains("turn.started.st*-marker-k4w")
                && message.contains("for agent 'glob-flag-agent-k4w'")
        }));
    }

    #[test]
    fn partial_glob_over_event_prefix_logs_invalid_glob_diagnostic() {
        testing::install_log_collector();
        let global = hooks_map("tool.started", &[("st", "cmd-st")]);
        // `tool.st*` extends no full event name (`tool` is not an event), so
        // the event-prefix diagnostic can never see it; the glob check must
        // flag it instead.
        let gate = vec!["tool.st*".to_string()];

        let resolved = resolve_hooks(
            HookEvent::ToolStarted,
            &global,
            Some((&gate, "prefix-glob-agent-t7c")),
            None,
            None,
        );

        assert!(resolved.is_empty(), "a partial glob admits nothing");
        let debugs = testing::debug_snapshot();
        assert!(debugs.iter().any(|message| {
            message.contains("tool.st*") && message.contains("for agent 'prefix-glob-agent-t7c'")
        }));
    }

    #[test]
    fn inner_glob_entries_log_invalid_glob_diagnostic() {
        testing::install_log_collector();
        let global = hooks_map("agent.completed", &[("mark", "cmd-mark")]);
        let gate = vec!["ag*.completed.mark".to_string()];

        let resolved = resolve_hooks(
            HookEvent::AgentCompleted,
            &global,
            Some((&gate, "inner-glob-agent-m2r")),
            None,
            None,
        );

        assert!(resolved.is_empty(), "an inner glob admits nothing");
        let debugs = testing::debug_snapshot();
        assert!(debugs.iter().any(|message| {
            message.contains("ag*.completed.mark")
                && message.contains("for agent 'inner-glob-agent-m2r'")
        }));
    }

    #[test]
    fn unknown_whitelist_entries_log_debug_and_stay_inert() {
        testing::install_log_collector();
        let global = hooks_map("turn.started", &[("a", "cmd-a")]);
        let gate = vec![
            "turn.started.a".to_string(),
            "turn.started.nonexistent-marker-xyz".to_string(),
        ];

        let resolved = resolve_hooks(
            HookEvent::TurnStarted,
            &global,
            Some((&gate, "gate-agent-xyz")),
            None,
            None,
        );

        assert_eq!(names(&resolved), ["a"]);
        let debugs = testing::debug_snapshot();
        assert!(debugs.iter().any(|message| {
            message.contains("turn.started.nonexistent-marker-xyz")
                && message.contains("for agent 'gate-agent-xyz'")
        }));
    }

    #[test]
    fn empty_global_map_short_circuits_whitelist_diagnostics() {
        testing::install_log_collector();
        let global = HooksMap::default();
        let role = hooks_map("turn.started", &[("role-only", "role-cmd")]);
        let gate = vec!["turn.started.x-shortcircuit-marker-p3q".to_string()];

        let resolved = resolve_hooks(
            HookEvent::TurnStarted,
            &global,
            Some((&gate, "short-circuit-agent")),
            Some(&role),
            None,
        );

        assert_eq!(names(&resolved), ["role-only"]);
        let debugs = testing::debug_snapshot();
        assert!(
            debugs
                .iter()
                .all(|message| !message.contains("shortcircuit-marker-p3q"))
        );
    }

    #[test]
    fn role_hooks_are_always_additive() {
        let global = HooksMap::default();
        let role = hooks_map("session.ended", &[("archive", "role-cmd")]);
        let empty = HooksMap::default();

        let gated = resolve_hooks(
            HookEvent::SessionEnded,
            &global,
            Some((&[], "gated-agent")),
            Some(&role),
            Some((&empty, "gated-agent")),
        );
        assert_eq!(names(&gated), ["archive"]);

        let root = resolve_hooks(HookEvent::SessionEnded, &global, None, Some(&role), None);
        assert_eq!(names(&root), ["archive"]);
    }

    #[test]
    #[serial]
    fn scopes_resolve_in_global_role_agent_order() {
        let global = hooks_map("tool.completed", &[("notify", "global-cmd")]);
        let role = hooks_map("tool.completed", &[("notify", "role-cmd")]);
        let agent = hooks_map("tool.completed", &[("notify", "agent-cmd")]);
        let gate = vec!["tool.completed.notify".to_string()];

        let resolved = resolve_hooks(
            HookEvent::ToolCompleted,
            &global,
            Some((&gate, "order-agent")),
            Some(&role),
            Some((&agent, "order-agent")),
        );

        let commands: Vec<&str> = resolved.iter().map(|hook| hook.command.as_str()).collect();
        assert_eq!(commands, ["global-cmd", "role-cmd", "agent-cmd"]);
        assert!(
            resolved
                .iter()
                .all(|hook| hook.full_name == "tool.completed.notify")
        );
        assert_eq!(resolved[0].cwd, paths::config_dir());
        assert_eq!(resolved[1].cwd, paths::roles_dir());
        assert_eq!(resolved[2].cwd, paths::agent_data_dir("order-agent"));
    }

    #[test]
    fn resolved_hooks_reads_global_config_from_context() {
        let ctx = ctx_with_global_hooks(hooks_map("session.started", &[("smoke", "true")]));

        let resolved = ctx.resolved_hooks(HookEvent::SessionStarted);

        assert_eq!(names(&resolved), ["smoke"]);
        assert_eq!(resolved[0].full_name, "session.started.smoke");
        assert!(ctx.resolved_hooks(HookEvent::SessionEnded).is_empty());
    }

    #[test]
    #[serial]
    fn empty_command_is_skipped_without_error() {
        let _guard = test_sink::install();
        let cwd = env::temp_dir();
        let hooks = vec![
            ResolvedHook {
                name: "gap".to_string(),
                full_name: "turn.failed.gap".to_string(),
                command: "   ".to_string(),
                cwd: cwd.clone(),
            },
            ResolvedHook {
                name: "after-gap".to_string(),
                full_name: "turn.failed.after-gap".to_string(),
                command: "true".to_string(),
                cwd,
            },
        ];

        fire_resolved(HookEvent::TurnFailed, hooks, Vec::new(), &[], None);

        let captures = test_sink::drain();
        assert!(captures.iter().all(|capture| capture.hook_name != "gap"));
        assert!(
            captures
                .iter()
                .any(|capture| capture.hook_name == "after-gap")
        );
    }

    #[test]
    #[serial]
    fn sink_captures_synchronously_and_suppresses_spawn() {
        let marker =
            env::temp_dir().join(format!("coyote-hook-sink-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let touch = format!("touch \"{}\"", marker.display());
        let ctx = ctx_with_global_hooks(hooks_map("job.completed", &[("sink-only", &touch)]));

        let _guard = test_sink::install();
        fire(
            HookEvent::JobCompleted,
            &ctx,
            &[("HOOK_EXTRA", "extra-value".to_string())],
            None,
        );

        let captures = test_sink::drain();
        let capture = captures
            .iter()
            .find(|capture| capture.hook_name == "sink-only")
            .expect("sink must record synchronously before fire returns");
        assert_eq!(capture.event, HookEvent::JobCompleted);
        assert_eq!(
            capture.envs.get("COYOTE_EVENT").map(String::as_str),
            Some("job.completed")
        );
        assert_eq!(
            capture.envs.get("COYOTE_HOOK_NAME").map(String::as_str),
            Some("sink-only")
        );
        assert!(capture.envs.contains_key("COYOTE_CONFIG_DIR"));
        assert!(capture.envs.contains_key("COYOTE_EVENT_TIMESTAMP"));
        assert_eq!(
            capture.envs.get("HOOK_EXTRA").map(String::as_str),
            Some("extra-value")
        );
        assert!(
            !marker.exists(),
            "no process may spawn while the sink is installed"
        );
    }

    #[test]
    #[serial]
    fn fire_resolved_truncates_every_env_value() {
        let _guard = test_sink::install();
        let oversized = "x".repeat(3000);
        let hook = ResolvedHook {
            name: "truncate-probe".to_string(),
            full_name: "turn.failed.truncate-probe".to_string(),
            command: "true".to_string(),
            cwd: env::temp_dir(),
        };

        fire_resolved(
            HookEvent::TurnFailed,
            vec![hook],
            vec![("COYOTE_BIG_BASE".to_string(), oversized.clone())],
            &[("COYOTE_BIG_EXTRA", oversized)],
            None,
        );

        let captures = test_sink::drain();
        let capture = captures
            .iter()
            .find(|capture| capture.hook_name == "truncate-probe")
            .expect("sink must record the dispatch");
        assert_eq!(capture.envs.get("COYOTE_BIG_BASE").unwrap().len(), 2048);
        assert_eq!(capture.envs.get("COYOTE_BIG_EXTRA").unwrap().len(), 2048);
    }

    #[test]
    #[serial]
    fn fire_resolved_strips_nul_bytes_from_env_values() {
        let _guard = test_sink::install();
        let hook = ResolvedHook {
            name: "nul-strip-probe-k4v".to_string(),
            full_name: "escalation.raised.nul-strip-probe-k4v".to_string(),
            command: "true".to_string(),
            cwd: env::temp_dir(),
        };

        fire_resolved(
            HookEvent::EscalationRaised,
            vec![hook],
            Vec::new(),
            &[(
                "COYOTE_ESCALATION_QUESTION",
                "before\u{0000}after".to_string(),
            )],
            None,
        );

        let captures = test_sink::drain();
        let capture = captures
            .iter()
            .find(|capture| capture.hook_name == "nul-strip-probe-k4v")
            .expect("a NUL in an env value must not suppress the hook");
        assert_eq!(
            capture
                .envs
                .get("COYOTE_ESCALATION_QUESTION")
                .map(String::as_str),
            Some("beforeafter")
        );
    }

    #[test]
    #[serial]
    fn sink_recovers_after_capture_buffer_poisoning() {
        let _guard = test_sink::install();
        test_sink::drain();
        std::thread::spawn(|| {
            let _held = test_sink::CAPTURES.lock().unwrap();
            panic!("poison the capture buffer");
        })
        .join()
        .unwrap_err();

        let hook = ResolvedHook {
            name: "poison-recovery-marker-j8r".to_string(),
            full_name: "turn.failed.poison-recovery-marker-j8r".to_string(),
            command: "true".to_string(),
            cwd: env::temp_dir(),
        };
        assert!(test_sink::record(HookEvent::TurnFailed, &hook, &[], None));

        let captures = test_sink::drain();
        assert!(
            captures
                .iter()
                .any(|capture| capture.hook_name == "poison-recovery-marker-j8r")
        );
    }

    #[test]
    fn base_envs_parts_reflects_session_and_agent() {
        let envs = base_envs_parts(HookEvent::TurnStarted, Some("sess-1"), Some("helper"));
        let find = |key: &str| {
            envs.iter()
                .find(|(k, _)| k == key)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(find("COYOTE_SESSION_ID"), Some("sess-1"));
        assert_eq!(find("COYOTE_AGENT_NAME"), Some("helper"));
        let timestamp = find("COYOTE_EVENT_TIMESTAMP").expect("timestamp env");
        let parsed = chrono::DateTime::parse_from_rfc3339(timestamp).unwrap();
        assert_eq!(parsed.offset().local_minus_utc(), 0);

        let envs = base_envs_parts(HookEvent::TurnStarted, None, None);
        assert!(
            envs.iter()
                .all(|(key, _)| key != "COYOTE_SESSION_ID" && key != "COYOTE_AGENT_NAME")
        );
    }

    #[tokio::test]
    #[serial]
    async fn drain_pending_returns_immediately_when_idle() {
        let start = std::time::Instant::now();
        drain_pending(Duration::from_secs(5)).await;
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    #[serial]
    async fn drain_pending_logs_when_the_timeout_expires() {
        testing::install_log_collector();
        let pending = SpawnAckGuard::register();

        drain_pending(Duration::from_millis(50)).await;

        let debugs = testing::debug_snapshot();
        assert!(
            debugs
                .iter()
                .any(|message| message.contains("Gave up waiting"))
        );
        drop(pending);
    }

    #[test]
    fn truncate_env_value_keeps_short_values() {
        assert_eq!(truncate_env_value("short"), "short");
        let exact = "a".repeat(2048);
        assert_eq!(truncate_env_value(&exact), exact);
    }

    #[test]
    fn truncate_env_value_respects_char_boundaries() {
        let value = format!("{}€", "a".repeat(2047));
        let truncated = truncate_env_value(&value);
        assert_eq!(truncated.len(), 2047);
        assert!(truncated.chars().all(|c| c == 'a'));
    }

    #[test]
    #[serial]
    fn fire_resolved_without_runtime_skips_instead_of_panicking() {
        testing::install_log_collector();
        let hook = ResolvedHook {
            name: "no-runtime-marker-a7c".to_string(),
            full_name: "turn.failed.no-runtime-marker-a7c".to_string(),
            command: "true".to_string(),
            cwd: env::temp_dir(),
        };

        fire_resolved(HookEvent::TurnFailed, vec![hook], Vec::new(), &[], None);

        let debugs = testing::debug_snapshot();
        assert!(debugs.iter().any(|message| {
            message.contains("no-runtime-marker-a7c")
                && message.contains("no Tokio runtime on this thread")
        }));
    }

    #[test]
    #[serial]
    fn fire_without_runtime_skips_instead_of_panicking() {
        testing::install_log_collector();
        let ctx = ctx_with_global_hooks(hooks_map(
            "turn.failed",
            &[("fire-no-runtime-marker-b8d", "true")],
        ));

        fire(HookEvent::TurnFailed, &ctx, &[], None);

        let debugs = testing::debug_snapshot();
        assert!(debugs.iter().any(|message| {
            message.contains("fire-no-runtime-marker-b8d")
                && message.contains("no Tokio runtime on this thread")
        }));
    }

    #[test]
    fn debug_capture_excludes_non_hooks_targets() {
        testing::install_log_collector();
        let hook = ResolvedHook {
            name: "included-marker-k2v".to_string(),
            full_name: "turn.failed.included-marker-k2v".to_string(),
            command: "  ".to_string(),
            cwd: env::temp_dir(),
        };

        log::debug!(
            target: concat!(env!("CARGO_CRATE_NAME"), "::not_hooks_marker_k2v"),
            "excluded-marker-k2v"
        );
        log::debug!(
            target: concat!(env!("CARGO_CRATE_NAME"), "::hooksbogus_marker_k2v"),
            "boundary-excluded-marker-k2v"
        );
        fire_resolved(HookEvent::TurnFailed, vec![hook], Vec::new(), &[], None);

        let debugs = testing::debug_snapshot();
        assert!(
            debugs
                .iter()
                .all(|message| !message.contains("excluded-marker-k2v"))
        );
        assert!(
            debugs
                .iter()
                .all(|message| !message.contains("boundary-excluded-marker-k2v"))
        );
        assert!(
            debugs
                .iter()
                .any(|message| message.contains("included-marker-k2v"))
        );
    }

    #[test]
    #[serial]
    fn debug_capture_recovers_after_buffer_poisoning() {
        testing::install_log_collector();
        std::thread::spawn(|| {
            let _held = testing::debug_messages().lock().unwrap();
            panic!("poison the debug buffer");
        })
        .join()
        .unwrap_err();

        let hook = ResolvedHook {
            name: "poisoned-buffer-marker-w4t".to_string(),
            full_name: "turn.failed.poisoned-buffer-marker-w4t".to_string(),
            command: "  ".to_string(),
            cwd: env::temp_dir(),
        };
        fire_resolved(HookEvent::TurnFailed, vec![hook], Vec::new(), &[], None);

        let mut debugs = testing::debug_messages()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(debugs.iter().any(|message| {
            message.contains("poisoned-buffer-marker-w4t") && message.contains("empty command")
        }));
        debugs.retain(|message| !message.contains("poisoned-buffer-marker-w4t"));
    }

    #[test]
    #[serial]
    fn resolved_hooks_adds_role_hooks_through_context() {
        let mut ctx = ctx_with_global_hooks(hooks_map(
            "turn.completed",
            &[("global-probe", "global-cmd")],
        ));
        let content = "---\nhooks:\n  turn.completed:\n    - name: role-probe\n      command: role-cmd\n---\nPrompt";
        ctx.role = Some(Role::new("test", content));

        let resolved = ctx.resolved_hooks(HookEvent::TurnCompleted);

        assert_eq!(names(&resolved), ["global-probe", "role-probe"]);
        assert_eq!(resolved[1].command, "role-cmd");
        assert_eq!(resolved[1].cwd, paths::roles_dir());
    }

    async fn wait_for(what: &str, cond: impl Fn() -> bool) {
        for _ in 0..400 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Dispatch tests whose hooks fail before any shell is involved (bad
    /// cwd, empty command), so they run identically on every OS.
    mod dispatch_portable {
        use super::*;
        use crate::testing::TestConfigDirGuard;
        use std::fs::create_dir_all;
        use std::time::Duration;

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn spawn_failure_removes_the_payload_file() {
            testing::install_log_collector();
            let guard = TestConfigDirGuard::new("hooks-dispatch");
            let payload_dir = guard.path.join("payloads");
            create_dir_all(&payload_dir).unwrap();
            let _payload_dir = PayloadDirOverrideGuard::new(payload_dir.clone());
            let hooks = vec![ResolvedHook {
                name: "spawnfail-marker-c9d".to_string(),
                full_name: "tool.started.spawnfail-marker-c9d".to_string(),
                command: "true".to_string(),
                cwd: guard.path.join("nonexistent-spawnfail-marker-c9d"),
            }];

            fire_resolved(
                HookEvent::ToolStarted,
                hooks,
                Vec::new(),
                &[],
                Some(r#"{"probe":true}"#.to_string()),
            );
            drain_pending(Duration::from_secs(10)).await;

            // The unspawnable cwd must have driven the spawn-error branch,
            // and the payload write before it must have succeeded: only then
            // does the empty directory below demonstrate the orphan unlink
            // rather than a payload file that never existed.
            wait_for("spawn failure log", || {
                testing::debug_snapshot().iter().any(|message| {
                    message.contains("Failed to spawn hook 'tool.started.spawnfail-marker-c9d'")
                })
            })
            .await;
            let debugs = testing::debug_snapshot();
            assert!(debugs.iter().all(|message| {
                !(message.contains("spawnfail-marker-c9d")
                    && message.contains("Failed to write payload file"))
            }));

            wait_for("payload file removal", || {
                std::fs::read_dir(&payload_dir)
                    .unwrap()
                    .flatten()
                    .next()
                    .is_none()
            })
            .await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn error_paths_log_debug_only() {
            testing::install_log_collector();
            let empty_cwd = env::temp_dir();
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let bad_cwd = env::temp_dir().join(format!("coyote-badcwd-marker-f5b-{unique}"));
            let hooks = vec![
                ResolvedHook {
                    name: "empty-marker-f5b".to_string(),
                    full_name: "turn.failed.empty-marker-f5b".to_string(),
                    command: "  ".to_string(),
                    cwd: empty_cwd.clone(),
                },
                ResolvedHook {
                    name: "badcwd-marker-f5b".to_string(),
                    full_name: "turn.failed.badcwd-marker-f5b".to_string(),
                    command: "true".to_string(),
                    cwd: bad_cwd.clone(),
                },
            ];

            fire_resolved(HookEvent::TurnFailed, hooks, Vec::new(), &[], None);
            drain_pending(Duration::from_secs(10)).await;

            let debugs = testing::debug_snapshot();
            let empty_cwd_display = empty_cwd.display().to_string();
            assert!(debugs.iter().any(|message| {
                message.contains("empty-marker-f5b")
                    && message.contains("empty command")
                    && message.contains(empty_cwd_display.as_str())
            }));
            let bad_cwd_display = bad_cwd.display().to_string();
            assert!(debugs.iter().any(|message| {
                message.contains("Failed to spawn hook 'turn.failed.badcwd-marker-f5b'")
                    && message.contains(bad_cwd_display.as_str())
            }));
            let warns = testing::warn_snapshot();
            assert!(warns.iter().all(|message| !message.contains("marker-f5b")));
        }
    }

    #[cfg(unix)]
    mod dispatch {
        use super::*;
        use crate::testing::TestConfigDirGuard;
        use std::fs::create_dir_all;
        use std::path::PathBuf;
        use std::time::Duration;

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn fire_spawns_detached_with_envs() {
            let guard = TestConfigDirGuard::new("hooks-dispatch");
            let out = guard.path.join("env-out");
            let gate = guard.path.join("gate");
            let done = guard.path.join("done");
            let command = r#"env > "$HOOK_OUT.tmp" && mv "$HOOK_OUT.tmp" "$HOOK_OUT"; until [ -e "$HOOK_GATE" ]; do sleep 0.05; done; : > "$HOOK_DONE""#;
            let ctx = ctx_with_global_hooks(hooks_map("turn.completed", &[("envdump", command)]));

            fire(
                HookEvent::TurnCompleted,
                &ctx,
                &[
                    ("HOOK_OUT", out.display().to_string()),
                    ("HOOK_GATE", gate.display().to_string()),
                    ("HOOK_DONE", done.display().to_string()),
                ],
                None,
            );

            wait_for("hook env dump", || out.exists()).await;
            assert!(
                !done.exists(),
                "dispatch must return before the hook completes"
            );

            let env_dump = std::fs::read_to_string(&out).unwrap();
            let lines: Vec<&str> = env_dump.lines().collect();
            assert!(lines.contains(&"COYOTE_EVENT=turn.completed"));
            assert!(lines.contains(&"COYOTE_HOOK_NAME=envdump"));
            let config_dir_line = format!("COYOTE_CONFIG_DIR={}", guard.path.display());
            assert!(lines.contains(&config_dir_line.as_str()));
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("COYOTE_EVENT_TIMESTAMP="))
            );
            let gate_line = format!("HOOK_GATE={}", gate.display());
            assert!(lines.contains(&gate_line.as_str()));

            std::fs::write(&gate, "").unwrap();
            wait_for("hook completion", || done.exists()).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn hook_stdin_is_null_not_inherited() {
            let guard = TestConfigDirGuard::new("hooks-dispatch");
            let out = guard.path.join("stdin-out");
            let command = r#"if read -t 5 line; then echo open > "$HOOK_OUT.tmp"; else echo eof > "$HOOK_OUT.tmp"; fi; mv "$HOOK_OUT.tmp" "$HOOK_OUT""#;
            let ctx = ctx_with_global_hooks(hooks_map("turn.started", &[("stdin-probe", command)]));
            let start = std::time::Instant::now();

            fire(
                HookEvent::TurnStarted,
                &ctx,
                &[("HOOK_OUT", out.display().to_string())],
                None,
            );

            wait_for("stdin probe output", || out.exists()).await;
            assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "eof");
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "read must hit EOF immediately, not time out"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn payload_file_is_written_and_removed() {
            let guard = TestConfigDirGuard::new("hooks-dispatch");
            let out = guard.path.join("payload-out");
            let path_out = guard.path.join("payload-path");
            let command = r#"cp "$COYOTE_HOOK_PAYLOAD_FILE" "$HOOK_OUT.tmp" && mv "$HOOK_OUT.tmp" "$HOOK_OUT"; printf '%s' "$COYOTE_HOOK_PAYLOAD_FILE" > "$HOOK_PATH.tmp" && mv "$HOOK_PATH.tmp" "$HOOK_PATH""#;
            let ctx = ctx_with_global_hooks(hooks_map("tool.started", &[("payload", command)]));
            let payload = r#"{"path":"/tmp/target","recursive":true}"#;

            fire(
                HookEvent::ToolStarted,
                &ctx,
                &[
                    ("HOOK_OUT", out.display().to_string()),
                    ("HOOK_PATH", path_out.display().to_string()),
                ],
                Some(payload.to_string()),
            );

            wait_for("payload copy", || out.exists() && path_out.exists()).await;
            assert_eq!(std::fs::read_to_string(&out).unwrap(), payload);

            let payload_file = PathBuf::from(std::fs::read_to_string(&path_out).unwrap());
            let file_name = payload_file.file_name().unwrap().to_string_lossy();
            assert!(file_name.starts_with("coyote-hook-tool.started-"));
            wait_for("payload file removal", || !payload_file.exists()).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn drain_pending_waits_for_spawn_not_completion() {
            let guard = TestConfigDirGuard::new("hooks-dispatch");
            let out = guard.path.join("spawned");
            let gate = guard.path.join("gate");
            let done = guard.path.join("done");
            let command = r#": > "$HOOK_OUT"; until [ -e "$HOOK_GATE" ]; do sleep 0.05; done; : > "$HOOK_DONE""#;
            let ctx =
                ctx_with_global_hooks(hooks_map("session.ended", &[("drain-probe", command)]));

            fire(
                HookEvent::SessionEnded,
                &ctx,
                &[
                    ("HOOK_OUT", out.display().to_string()),
                    ("HOOK_GATE", gate.display().to_string()),
                    ("HOOK_DONE", done.display().to_string()),
                ],
                None,
            );

            drain_pending(Duration::from_secs(10)).await;

            assert!(!done.exists(), "drain must not wait for hook completion");
            wait_for("hook start marker", || out.exists()).await;
            assert!(!done.exists());

            std::fs::write(&gate, "").unwrap();
            wait_for("hook completion", || done.exists()).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn failing_hook_never_disturbs_the_engine() {
            testing::install_log_collector();
            let guard = TestConfigDirGuard::new("hooks-dispatch");
            let first = guard.path.join("first-out");
            let second = guard.path.join("second-out");
            let command = r#"echo err >&2; : > "$HOOK_OUT"; exit 1"#;
            let ctx = ctx_with_global_hooks(hooks_map(
                "tool.failed",
                &[("stderr-exit1-marker", command)],
            ));

            fire(
                HookEvent::ToolFailed,
                &ctx,
                &[("HOOK_OUT", first.display().to_string())],
                None,
            );
            wait_for("first hook output", || first.exists()).await;

            fire(
                HookEvent::ToolFailed,
                &ctx,
                &[("HOOK_OUT", second.display().to_string())],
                None,
            );
            wait_for("second hook output", || second.exists()).await;

            let warns = testing::warn_snapshot();
            assert!(
                warns
                    .iter()
                    .all(|message| !message.contains("stderr-exit1-marker"))
            );
        }

        #[tokio::test]
        #[serial]
        async fn payload_file_is_created_with_owner_only_mode() {
            use std::os::unix::fs::PermissionsExt;
            let guard = TestConfigDirGuard::new("hooks-dispatch");
            let path = guard.path.join("payload.json");

            write_payload_file(&path, r#"{"probe":true}"#)
                .await
                .unwrap();

            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "payload file mode was {mode:o}");
        }

        #[tokio::test]
        #[serial]
        async fn payload_file_write_refuses_existing_paths() {
            let guard = TestConfigDirGuard::new("hooks-dispatch");

            let plain = guard.path.join("existing.json");
            std::fs::write(&plain, "original").unwrap();
            assert!(write_payload_file(&plain, "{}").await.is_err());
            assert_eq!(std::fs::read_to_string(&plain).unwrap(), "original");

            let target = guard.path.join("symlink-target");
            std::fs::write(&target, "original").unwrap();
            let link = guard.path.join("existing-link.json");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(write_payload_file(&link, "{}").await.is_err());
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn payload_write_failure_still_runs_hook_without_payload_env() {
            testing::install_log_collector();
            let guard = TestConfigDirGuard::new("hooks-dispatch");
            let out = guard.path.join("env-out");
            let _payload_dir = PayloadDirOverrideGuard::new(guard.path.join("missing-payload-dir"));
            let command = r#"env > "$HOOK_OUT.tmp" && mv "$HOOK_OUT.tmp" "$HOOK_OUT""#;
            let ctx = ctx_with_global_hooks(hooks_map(
                "tool.started",
                &[("payload-fallback-marker", command)],
            ));

            fire(
                HookEvent::ToolStarted,
                &ctx,
                &[("HOOK_OUT", out.display().to_string())],
                Some(r#"{"probe":true}"#.to_string()),
            );

            wait_for("hook env dump", || out.exists()).await;
            let env_dump = std::fs::read_to_string(&out).unwrap();
            assert!(
                env_dump
                    .lines()
                    .all(|line| !line.starts_with("COYOTE_HOOK_PAYLOAD_FILE="))
            );
            let debugs = testing::debug_snapshot();
            assert!(debugs.iter().any(|message| message.contains(
                "Failed to write payload file for hook 'tool.started.payload-fallback-marker'"
            )));
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn payload_paths_differ_across_dispatches() {
            let guard = TestConfigDirGuard::new("hooks-dispatch");
            let first = guard.path.join("first-path");
            let second = guard.path.join("second-path");
            let command = r#"printf '%s' "$COYOTE_HOOK_PAYLOAD_FILE" > "$HOOK_PATH.tmp" && mv "$HOOK_PATH.tmp" "$HOOK_PATH""#;
            let ctx = ctx_with_global_hooks(hooks_map("tool.started", &[("nonce-probe", command)]));

            fire(
                HookEvent::ToolStarted,
                &ctx,
                &[("HOOK_PATH", first.display().to_string())],
                Some("{}".to_string()),
            );
            wait_for("first payload path", || first.exists()).await;

            fire(
                HookEvent::ToolStarted,
                &ctx,
                &[("HOOK_PATH", second.display().to_string())],
                Some("{}".to_string()),
            );
            wait_for("second payload path", || second.exists()).await;

            let first_path = std::fs::read_to_string(&first).unwrap();
            let second_path = std::fs::read_to_string(&second).unwrap();
            assert!(!first_path.is_empty());
            assert_ne!(first_path, second_path);
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn payload_paths_differ_within_one_fire() {
            let guard = TestConfigDirGuard::new("hooks-dispatch");
            let first = guard.path.join("first-path");
            let second = guard.path.join("second-path");
            let command_first = r#"printf '%s' "$COYOTE_HOOK_PAYLOAD_FILE" > "$HOOK_PATH_FIRST.tmp" && mv "$HOOK_PATH_FIRST.tmp" "$HOOK_PATH_FIRST""#;
            let command_second = r#"printf '%s' "$COYOTE_HOOK_PAYLOAD_FILE" > "$HOOK_PATH_SECOND.tmp" && mv "$HOOK_PATH_SECOND.tmp" "$HOOK_PATH_SECOND""#;
            let ctx = ctx_with_global_hooks(hooks_map(
                "tool.started",
                &[
                    ("same-fire-a", command_first),
                    ("same-fire-b", command_second),
                ],
            ));

            fire(
                HookEvent::ToolStarted,
                &ctx,
                &[
                    ("HOOK_PATH_FIRST", first.display().to_string()),
                    ("HOOK_PATH_SECOND", second.display().to_string()),
                ],
                Some("{}".to_string()),
            );
            wait_for("both payload paths", || first.exists() && second.exists()).await;

            let first_path = std::fs::read_to_string(&first).unwrap();
            let second_path = std::fs::read_to_string(&second).unwrap();
            assert!(!first_path.is_empty());
            assert!(!second_path.is_empty());
            assert_ne!(
                first_path, second_path,
                "each hook of one fire must get its own payload file"
            );
        }

        #[tokio::test]
        #[serial]
        async fn resolved_hooks_gates_globals_and_adds_agent_hooks_through_context() {
            let _guard = TestConfigDirGuard::new("hooks-dispatch");
            let mut ctx =
                ctx_with_global_hooks(hooks_map("tool.started", &[("notify", "global-cmd")]));
            let app = ctx.app.config.clone();
            let agent_name = "hooks-gate-probe";
            let agent_dir = paths::agent_data_dir(agent_name);
            create_dir_all(&agent_dir).unwrap();
            std::fs::write(
                agent_dir.join("config.yaml"),
                format!(
                    "name: {agent_name}\ninstructions: hi\nhooks:\n  tool.started:\n    - name: own\n      command: agent-cmd\nglobal_hooks: []\n"
                ),
            )
            .unwrap();

            ctx.use_agent(&app, agent_name, None, utils::create_abort_signal())
                .await
                .unwrap();

            let resolved = ctx.resolved_hooks(HookEvent::ToolStarted);
            assert_eq!(names(&resolved), ["own"]);
            assert_eq!(resolved[0].command, "agent-cmd");
            assert_eq!(resolved[0].cwd, paths::agent_data_dir(agent_name));
        }
    }

    /// Dispatch through the real `cmd /C` engine branch, plus the Windows
    /// posture of install and payload placement.
    #[cfg(windows)]
    mod windows_dispatch {
        use super::*;
        use crate::testing::TestConfigDirGuard;
        use std::path::PathBuf;

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn fire_spawns_via_cmd_with_envs() {
            let guard = TestConfigDirGuard::new("hooks-win-env");
            let out = guard.path.join("env-out");
            let command = r#"set > "%HOOK_OUT%.tmp" && move /Y "%HOOK_OUT%.tmp" "%HOOK_OUT%""#;
            let ctx =
                ctx_with_global_hooks(hooks_map("turn.completed", &[("win-envdump", command)]));

            fire(
                HookEvent::TurnCompleted,
                &ctx,
                &[("HOOK_OUT", out.display().to_string())],
                None,
            );

            wait_for("hook env dump", || out.exists()).await;
            let env_dump = std::fs::read_to_string(&out).unwrap();
            let lines: Vec<&str> = env_dump.lines().map(str::trim_end).collect();
            assert!(lines.contains(&"COYOTE_EVENT=turn.completed"));
            assert!(lines.contains(&"COYOTE_HOOK_NAME=win-envdump"));
            let config_dir_line = format!("COYOTE_CONFIG_DIR={}", guard.path.display());
            assert!(lines.contains(&config_dir_line.as_str()));
            assert!(
                lines
                    .iter()
                    .any(|line| line.starts_with("COYOTE_EVENT_TIMESTAMP="))
            );
            let out_line = format!("HOOK_OUT={}", out.display());
            assert!(lines.contains(&out_line.as_str()));
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn fire_returns_while_the_hook_still_runs() {
            let guard = TestConfigDirGuard::new("hooks-win-detach");
            let bat = guard.path.join("gate-wait.bat");
            let out = guard.path.join("started");
            let gate = guard.path.join("gate");
            let done = guard.path.join("done");
            // cmd one-liners cannot loop, so the gate wait lives in a batch
            // file; `ping -n 2` is the batch idiom for a short sleep.
            std::fs::write(
                &bat,
                concat!(
                    "@echo off\r\n",
                    "type nul > \"%HOOK_OUT%\"\r\n",
                    ":wait\r\n",
                    "if exist \"%HOOK_GATE%\" goto done\r\n",
                    "ping -n 2 127.0.0.1 >nul\r\n",
                    "goto wait\r\n",
                    ":done\r\n",
                    "type nul > \"%HOOK_DONE%\"\r\n",
                ),
            )
            .unwrap();
            let ctx = ctx_with_global_hooks(hooks_map(
                "turn.completed",
                &[("win-gate-probe", r#"call "%HOOK_BAT%""#)],
            ));

            fire(
                HookEvent::TurnCompleted,
                &ctx,
                &[
                    ("HOOK_BAT", bat.display().to_string()),
                    ("HOOK_OUT", out.display().to_string()),
                    ("HOOK_GATE", gate.display().to_string()),
                    ("HOOK_DONE", done.display().to_string()),
                ],
                None,
            );

            wait_for("hook start marker", || out.exists()).await;
            assert!(
                !done.exists(),
                "dispatch must return before the hook completes"
            );

            std::fs::write(&gate, "").unwrap();
            wait_for("hook completion", || done.exists()).await;
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn payload_file_is_written_and_removed() {
            let guard = TestConfigDirGuard::new("hooks-win-payload");
            let out = guard.path.join("payload-out");
            let path_out = guard.path.join("payload-path");
            let command = r#"copy /Y "%COYOTE_HOOK_PAYLOAD_FILE%" "%HOOK_OUT%" >nul && (echo %COYOTE_HOOK_PAYLOAD_FILE%)> "%HOOK_PATH%""#;
            let ctx = ctx_with_global_hooks(hooks_map("tool.started", &[("win-payload", command)]));
            let payload = r#"{"path":"C:/target","recursive":true}"#;

            fire(
                HookEvent::ToolStarted,
                &ctx,
                &[
                    ("HOOK_OUT", out.display().to_string()),
                    ("HOOK_PATH", path_out.display().to_string()),
                ],
                Some(payload.to_string()),
            );

            // `echo` terminates the path with CRLF, so a complete read of the
            // path file also proves the earlier `copy` finished.
            wait_for("payload path echo", || {
                std::fs::read_to_string(&path_out).is_ok_and(|path| path.ends_with("\r\n"))
            })
            .await;
            assert_eq!(std::fs::read_to_string(&out).unwrap(), payload);

            let payload_file =
                PathBuf::from(std::fs::read_to_string(&path_out).unwrap().trim_end());
            let file_name = payload_file.file_name().unwrap().to_string_lossy();
            assert!(file_name.starts_with("coyote-hook-tool.started-"));
            // No PayloadDirOverrideGuard here: the engine must fall through
            // to the per-user temp dir, which is what scopes payload files to
            // the owning user on Windows.
            assert!(payload_file.starts_with(env::temp_dir()));
            wait_for("payload file removal", || !payload_file.exists()).await;
        }

        #[test]
        #[serial]
        fn install_builtin_hooks_lands_under_a_backslashed_override() {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let hooks_dir = PathBuf::from(format!(
                "{}\\coyote-win-bs-{unique}\\hooks",
                env::temp_dir().display()
            ));
            let _env = testing::EnvVarGuard::set(utils::get_env_name("hooks_dir"), &hooks_dir);

            let result = install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None);

            // Capture outcomes first and assert only after the temp dir is
            // removed, so a failed assertion cannot leak it.
            let installed = (
                hooks_dir.join("notify.sh").is_file(),
                hooks_dir.join("log-events.sh").is_file(),
            );
            let _ = std::fs::remove_dir_all(hooks_dir.parent().unwrap());

            result.unwrap();
            assert!(installed.0, "notify.sh must land under the backslashed dir");
            assert!(
                installed.1,
                "log-events.sh must land under the backslashed dir"
            );
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn hook_cwds_resolve_per_scope_on_windows_paths() {
            let guard = TestConfigDirGuard::new("hooks-win-cwd");
            let global = hooks_map("turn.completed", &[("win-cwd-global", "global-cmd")]);
            let role = hooks_map("turn.completed", &[("win-cwd-role", "role-cmd")]);
            let agent = hooks_map("turn.completed", &[("win-cwd-agent", "agent-cmd")]);

            let resolved = resolve_hooks(
                HookEvent::TurnCompleted,
                &global,
                None,
                Some(&role),
                Some((&agent, "win-cwd-probe")),
            );

            assert_eq!(
                names(&resolved),
                ["win-cwd-global", "win-cwd-role", "win-cwd-agent"]
            );
            assert_eq!(resolved[0].cwd, paths::config_dir());
            assert_eq!(resolved[1].cwd, paths::roles_dir());
            assert_eq!(resolved[2].cwd, paths::agent_data_dir("win-cwd-probe"));

            let out = guard.path.join("cwd-out");
            let ctx = ctx_with_global_hooks(hooks_map(
                "turn.completed",
                &[("win-cwd-echo", r#"(echo %CD%)> "%HOOK_OUT%""#)],
            ));
            fire(
                HookEvent::TurnCompleted,
                &ctx,
                &[("HOOK_OUT", out.display().to_string())],
                None,
            );

            wait_for("hook cwd echo", || {
                std::fs::read_to_string(&out).is_ok_and(|cwd| cwd.ends_with("\r\n"))
            })
            .await;
            // %CD% can surface 8.3 short names or different casing;
            // canonicalize both sides before comparing.
            let echoed = std::fs::read_to_string(&out).unwrap();
            assert_eq!(
                std::fs::canonicalize(echoed.trim_end()).unwrap(),
                std::fs::canonicalize(paths::config_dir()).unwrap()
            );
        }

        #[test]
        #[serial]
        fn install_completes_without_the_unix_exec_bit() {
            let guard = HooksDirGuard::new("win-hooks-exec-noop");

            install_builtin_hooks(InstallMode::Skip, &mut StickyMode::None).unwrap();

            let notify = guard.root.join("notify.sh");
            assert!(notify.is_file());
            let embedded = HookAssets::get("notify.sh").unwrap();
            assert_eq!(
                std::fs::read_to_string(&notify).unwrap(),
                std::str::from_utf8(&embedded.data).unwrap()
            );
        }
    }
}
