use crate::config::{RequestContext, ensure_parent_exists, paths};
use crate::function::write_file_atomic;

use anyhow::{Result, anyhow};
use chrono::{SecondsFormat, Utc};
use indexmap::IndexMap;
use rand::distr::{Alphanumeric, SampleString};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Notify;

#[derive(Embed)]
#[folder = "assets/hooks/"]
struct HookAssets;

pub fn install_builtin_hooks(force: bool) -> Result<()> {
    info!(
        "Installing built-in example hooks in {}",
        paths::hooks_dir().display()
    );

    let mut wrote_any = false;
    for file in HookAssets::iter() {
        debug!("Processing hook file: {}", file.as_ref());

        let embedded_file = HookAssets::get(&file)
            .ok_or_else(|| anyhow!("Failed to load embedded hook file: {}", file.as_ref()))?;
        let content = unsafe { std::str::from_utf8_unchecked(&embedded_file.data) };
        let file_path = paths::hooks_dir().join(file.as_ref());

        if file_path.exists() && !force {
            debug!(
                "Hook file already exists, skipping: {}",
                file_path.display()
            );
            continue;
        }

        ensure_parent_exists(&file_path)?;
        info!("Creating hook file: {}", file_path.display());
        write_file_atomic(&file_path, content, Some(0o755))?;
        wrote_any = true;
    }

    if wrote_any && paths::hooks_dir() != paths::config_dir().join("hooks") {
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
    AgentFailed,
    GraphNodeStarted,
    GraphNodeCompleted,
    GraphNodeFailed,
    JobStarted,
    JobCompleted,
    JobFailed,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::TurnStarted => "turn.started",
            HookEvent::TurnCompleted => "turn.completed",
            HookEvent::TurnInterrupted => "turn.interrupted",
            HookEvent::TurnFailed => "turn.failed",
            HookEvent::SessionStarted => "session.started",
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
            HookEvent::AgentFailed => "agent.failed",
            HookEvent::GraphNodeStarted => "graph.node.started",
            HookEvent::GraphNodeCompleted => "graph.node.completed",
            HookEvent::GraphNodeFailed => "graph.node.failed",
            HookEvent::JobStarted => "job.started",
            HookEvent::JobCompleted => "job.completed",
            HookEvent::JobFailed => "job.failed",
        }
    }
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

    if let Some(defs) = global_hooks.get(event_name) {
        let cwd = paths::config_dir();
        for def in defs {
            let full_name = format!("{event_name}.{}", def.name);
            let admitted = match agent_gate {
                None => true,
                Some((gate, _)) => gate.contains(&full_name),
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

/// The single construction site for the base env set every hook receives.
/// [`base_envs`] feeds it from a live context; detached call sites that
/// dispatch pre-resolved snapshots after their context is gone pass the
/// names they captured with the snapshot. The timestamp is taken here, at
/// fire time, so it reflects when the event actually happened.
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

async fn write_payload_file(path: &Path, json: &str) -> std::io::Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    file.write_all(json.as_bytes()).await?;
    file.flush().await
}

#[cfg(test)]
static PAYLOAD_DIR_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

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
    use crate::config::{AppConfig, AppState, Role, WorkingMode};
    use serial_test::serial;
    use std::env;
    use std::sync::Arc;

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

    #[test]
    #[serial]
    fn install_builtin_hooks_installs_executable_scripts_and_honors_force() {
        let env_name = crate::utils::get_env_name("hooks_dir");
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("coyote-hooks-install-{unique}"));
        let env_guard = crate::testing::EnvVarGuard::set(&env_name, &root);

        // Capture every outcome first and assert only after cleanup: the
        // guard restores the env var even on panic, but the temp dir removal
        // below still has to run before any assertion can bail out.
        let notify = root.join("notify.sh");
        let log_events = root.join("log-events.sh");
        let fresh = install_builtin_hooks(false);
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
        let no_force = install_builtin_hooks(false);
        let after_no_force = std::fs::read_to_string(&notify);
        let force = install_builtin_hooks(true);
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
        let echo = content.find("echo \"[$title]").expect("echo fallback");
        assert!(notify_send < osascript);
        assert!(osascript < echo);
    }

    #[test]
    fn log_events_script_defaults_to_tmp_log_behind_env_override() {
        let embedded = HookAssets::get("log-events.sh").unwrap();
        let content = std::str::from_utf8(&embedded.data).unwrap();
        assert!(content.contains("COYOTE_HOOK_LOG"));
        assert!(content.contains("/tmp/coyote-hooks.log"));
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
        // An empty PATH hides notify-send and osascript; echo is a bash
        // builtin, so only the fallback branch can produce output.
        let mut cmd = std::process::Command::new("/bin/bash");
        cmd.arg(&script)
            .env("PATH", "")
            .env("COYOTE_EVENT", "turn.completed")
            .env("COYOTE_TOOL_NAME", "demo");
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

        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("turn.completed"), "{stdout}");
        assert!(stdout.contains("tool=demo"), "{stdout}");
    }

    #[test]
    fn hook_event_names_are_dotted() {
        let cases = [
            (HookEvent::TurnStarted, "turn.started"),
            (HookEvent::TurnCompleted, "turn.completed"),
            (HookEvent::TurnInterrupted, "turn.interrupted"),
            (HookEvent::TurnFailed, "turn.failed"),
            (HookEvent::SessionStarted, "session.started"),
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
            (HookEvent::AgentFailed, "agent.failed"),
            (HookEvent::GraphNodeStarted, "graph.node.started"),
            (HookEvent::GraphNodeCompleted, "graph.node.completed"),
            (HookEvent::GraphNodeFailed, "graph.node.failed"),
            (HookEvent::JobStarted, "job.started"),
            (HookEvent::JobCompleted, "job.completed"),
            (HookEvent::JobFailed, "job.failed"),
        ];
        for (event, name) in cases {
            assert_eq!(event.as_str(), name);
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
    fn unknown_whitelist_entries_log_debug_and_stay_inert() {
        crate::testing::install_log_collector();
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
        let debugs = crate::testing::debug_snapshot();
        assert!(debugs.iter().any(|message| {
            message.contains("turn.started.nonexistent-marker-xyz")
                && message.contains("for agent 'gate-agent-xyz'")
        }));
    }

    #[test]
    fn empty_global_map_short_circuits_whitelist_diagnostics() {
        crate::testing::install_log_collector();
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
        let debugs = crate::testing::debug_snapshot();
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
        crate::testing::install_log_collector();
        let pending = SpawnAckGuard::register();

        drain_pending(Duration::from_millis(50)).await;

        let debugs = crate::testing::debug_snapshot();
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
        crate::testing::install_log_collector();
        let hook = ResolvedHook {
            name: "no-runtime-marker-a7c".to_string(),
            full_name: "turn.failed.no-runtime-marker-a7c".to_string(),
            command: "true".to_string(),
            cwd: env::temp_dir(),
        };

        fire_resolved(HookEvent::TurnFailed, vec![hook], Vec::new(), &[], None);

        let debugs = crate::testing::debug_snapshot();
        assert!(debugs.iter().any(|message| {
            message.contains("no-runtime-marker-a7c")
                && message.contains("no Tokio runtime on this thread")
        }));
    }

    #[test]
    #[serial]
    fn fire_without_runtime_skips_instead_of_panicking() {
        crate::testing::install_log_collector();
        let ctx = ctx_with_global_hooks(hooks_map(
            "turn.failed",
            &[("fire-no-runtime-marker-b8d", "true")],
        ));

        fire(HookEvent::TurnFailed, &ctx, &[], None);

        let debugs = crate::testing::debug_snapshot();
        assert!(debugs.iter().any(|message| {
            message.contains("fire-no-runtime-marker-b8d")
                && message.contains("no Tokio runtime on this thread")
        }));
    }

    #[test]
    fn debug_capture_excludes_non_hooks_targets() {
        crate::testing::install_log_collector();
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

        let debugs = crate::testing::debug_snapshot();
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
        crate::testing::install_log_collector();
        std::thread::spawn(|| {
            let _held = crate::testing::debug_messages().lock().unwrap();
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

        let mut debugs = crate::testing::debug_messages()
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

    #[cfg(unix)]
    mod dispatch {
        use super::*;
        use crate::utils::get_env_name;
        use std::fs::{create_dir_all, remove_dir_all};
        use std::path::PathBuf;
        use std::time::{Duration, SystemTime, UNIX_EPOCH};

        struct TestConfigDirGuard {
            _env: crate::testing::EnvVarGuard,
            path: PathBuf,
        }

        impl TestConfigDirGuard {
            fn new() -> Self {
                let key = get_env_name("config_dir");
                let unique = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                let path = env::temp_dir().join(format!("coyote-hooks-tests-{unique}"));
                create_dir_all(&path).unwrap();
                Self {
                    _env: crate::testing::EnvVarGuard::set(key, &path),
                    path,
                }
            }
        }

        impl Drop for TestConfigDirGuard {
            fn drop(&mut self) {
                let _ = remove_dir_all(&self.path);
            }
        }

        /// Points `run_hook`'s payload directory at `path` for the guard's
        /// lifetime, so payload writes can be aimed at a nonexistent
        /// directory without touching the process-global `TMPDIR`.
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

        async fn wait_for(what: &str, cond: impl Fn() -> bool) {
            for _ in 0..400 {
                if cond() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("timed out waiting for {what}");
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn fire_spawns_detached_with_envs() {
            let guard = TestConfigDirGuard::new();
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
            let guard = TestConfigDirGuard::new();
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
            let guard = TestConfigDirGuard::new();
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
            let guard = TestConfigDirGuard::new();
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
        async fn error_paths_log_debug_only() {
            crate::testing::install_log_collector();
            let empty_cwd = env::temp_dir();
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
                    cwd: PathBuf::from("/nonexistent/coyote-badcwd-marker-f5b"),
                },
            ];

            fire_resolved(HookEvent::TurnFailed, hooks, Vec::new(), &[], None);
            drain_pending(Duration::from_secs(10)).await;

            let debugs = crate::testing::debug_snapshot();
            let empty_cwd_display = empty_cwd.display().to_string();
            assert!(debugs.iter().any(|message| {
                message.contains("empty-marker-f5b")
                    && message.contains("empty command")
                    && message.contains(empty_cwd_display.as_str())
            }));
            assert!(debugs.iter().any(|message| {
                message.contains("Failed to spawn hook 'turn.failed.badcwd-marker-f5b'")
                    && message.contains("/nonexistent/coyote-badcwd-marker-f5b")
            }));
            let warns = crate::testing::warn_snapshot();
            assert!(warns.iter().all(|message| !message.contains("marker-f5b")));
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn failing_hook_never_disturbs_the_engine() {
            crate::testing::install_log_collector();
            let guard = TestConfigDirGuard::new();
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

            let warns = crate::testing::warn_snapshot();
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
            let guard = TestConfigDirGuard::new();
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
            let guard = TestConfigDirGuard::new();

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
            crate::testing::install_log_collector();
            let guard = TestConfigDirGuard::new();
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
            let debugs = crate::testing::debug_snapshot();
            assert!(debugs.iter().any(|message| message.contains(
                "Failed to write payload file for hook 'tool.started.payload-fallback-marker'"
            )));
        }

        #[tokio::test(flavor = "multi_thread")]
        #[serial]
        async fn payload_paths_differ_across_dispatches() {
            let guard = TestConfigDirGuard::new();
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
            let guard = TestConfigDirGuard::new();
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
            let _guard = TestConfigDirGuard::new();
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

            ctx.use_agent(&app, agent_name, None, crate::utils::create_abort_signal())
                .await
                .unwrap();

            let resolved = ctx.resolved_hooks(HookEvent::ToolStarted);
            assert_eq!(names(&resolved), ["own"]);
            assert_eq!(resolved[0].command, "agent-cmd");
            assert_eq!(resolved[0].cwd, paths::agent_data_dir(agent_name));
        }
    }
}
