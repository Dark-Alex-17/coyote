use crate::hooks::McpServerHooks;
use crate::mcp::{
    ConnectedServer, HttpAuth, JsonField, McpAuthRequired, McpServer, McpTransportType,
    is_auth_required_error, resolve_http_auth, spawn_mcp_server,
};

use anyhow::Result;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Weak};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct McpServerKey {
    pub name: String,
    pub transport: McpTransportKey,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum McpTransportKey {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Remote {
        transport_type: McpTransportType,
        url: String,
        headers: Vec<(String, String)>,
    },
}

impl McpServerKey {
    pub fn from_spec(name: &str, spec: &McpServer) -> Self {
        let transport = if spec.is_remote() {
            let url = spec.url.clone().unwrap_or_default();
            let mut headers: Vec<(String, String)> = spec
                .headers
                .as_ref()
                .map(|h| h.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            headers.sort();
            McpTransportKey::Remote {
                transport_type: spec.transport_type.clone(),
                url,
                headers,
            }
        } else {
            let command = spec.command.clone().unwrap_or_default();
            let mut args = spec.args.clone().unwrap_or_default();
            args.sort();
            let mut env: Vec<(String, String)> = spec
                .env
                .as_ref()
                .map(|e| {
                    e.iter()
                        .map(|(k, v)| {
                            let v_str = match v {
                                JsonField::Str(s) => s.clone(),
                                JsonField::Bool(b) => b.to_string(),
                                JsonField::Int(i) => i.to_string(),
                            };
                            (k.clone(), v_str)
                        })
                        .collect()
                })
                .unwrap_or_default();
            env.sort();
            McpTransportKey::Stdio { command, args, env }
        };
        Self {
            name: name.into(),
            transport,
        }
    }
}

#[derive(Default)]
pub struct McpFactory {
    active: Mutex<HashMap<McpServerKey, Weak<ConnectedServer>>>,
}

impl McpFactory {
    pub fn try_get_active(&self, key: &McpServerKey) -> Option<Arc<ConnectedServer>> {
        let map = self.active.lock();
        map.get(key).and_then(|weak| weak.upgrade())
    }

    pub fn insert_active(&self, key: McpServerKey, handle: &Arc<ConnectedServer>) {
        let mut map = self.active.lock();
        map.insert(key, Arc::downgrade(handle));
    }

    pub async fn acquire(
        &self,
        name: &str,
        spec: &McpServer,
        log_path: Option<&Path>,
        hooks: &McpServerHooks,
    ) -> Result<Arc<ConnectedServer>> {
        let key = McpServerKey::from_spec(name, spec);

        // Reuse of a live server fires no hooks; only a real spawn below
        // reports anything.
        if let Some(existing) = self.try_get_active(&key) {
            return Ok(existing);
        }

        // The live probe failed, so an entry still present for this key is a
        // dead weak: the key was connected earlier in this process and the
        // spawn below is a reconnect.
        let reconnect = self.active.lock().contains_key(&key);

        let transport = transport_label(&spec.transport_type);
        let handle = match spawn_server(name, spec, log_path).await {
            Ok(handle) => handle,
            Err(e) => {
                hooks.fire_failed(name, transport, &e, is_auth_required_error(&e));
                return Err(e);
            }
        };
        self.insert_active(key, &handle);
        hooks.fire_connected(name, transport, reconnect);
        Ok(handle)
    }
}

fn transport_label(transport: &McpTransportType) -> &'static str {
    match transport {
        McpTransportType::Stdio => "stdio",
        McpTransportType::Http => "http",
        McpTransportType::Sse => "sse",
    }
}

async fn spawn_server(
    name: &str,
    spec: &McpServer,
    log_path: Option<&Path>,
) -> Result<Arc<ConnectedServer>> {
    let (auth, auth_reason) = resolve_http_auth(name, spec).await;
    spawn_transport(spec, log_path, auth).await.map_err(|e| {
        if is_auth_required_error(&e) {
            e.context(McpAuthRequired {
                server: name.to_string(),
                reason: auth_reason,
            })
        } else {
            e
        }
    })
}

async fn spawn_transport(
    spec: &McpServer,
    log_path: Option<&Path>,
    auth: HttpAuth,
) -> Result<Arc<ConnectedServer>> {
    #[cfg(test)]
    if let Some(result) = STUB_SPAWNS.lock().pop() {
        return result;
    }
    spawn_mcp_server(spec, log_path, auth).await
}

/// Injected spawn outcomes, popped instead of spawning a real transport:
/// nothing in a test environment speaks MCP over stdio or HTTP, so the
/// connect path is exercised with fixture handles. Empty means spawn for
/// real.
#[cfg(test)]
static STUB_SPAWNS: Mutex<Vec<Result<Arc<ConnectedServer>>>> = Mutex::new(Vec::new());

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_fixtures::FixtureServer;
    use crate::config::{AppConfig, AppState, RequestContext, WorkingMode};
    use crate::hooks::{HookDef, HooksMap, test_sink};
    use crate::mcp::{JsonField, McpServer, McpTransportType};
    use anyhow::anyhow;
    use indexmap::IndexMap;
    use rmcp::ServiceExt;
    use serial_test::serial;
    use std::collections::HashMap;

    fn stdio_spec(
        command: &str,
        args: Option<Vec<String>>,
        env: Option<IndexMap<String, JsonField>>,
    ) -> McpServer {
        McpServer {
            transport_type: McpTransportType::Stdio,
            command: Some(command.to_string()),
            args,
            env,
            cwd: None,
            url: None,
            headers: None,
            oauth: None,
            allowed_tools: None,
        }
    }

    fn remote_spec(
        transport: McpTransportType,
        url: &str,
        headers: Option<IndexMap<String, String>>,
    ) -> McpServer {
        McpServer {
            transport_type: transport,
            command: None,
            args: None,
            env: None,
            cwd: None,
            url: Some(url.to_string()),
            headers,
            oauth: None,
            allowed_tools: None,
        }
    }

    #[test]
    fn key_from_stdio_spec_captures_command_args_env() {
        let mut env = IndexMap::new();
        env.insert("TOKEN".into(), JsonField::Str("abc".into()));
        let spec = stdio_spec("npx", Some(vec!["-y".into(), "server".into()]), Some(env));
        let key = McpServerKey::from_spec("my-server", &spec);

        assert_eq!(key.name, "my-server");
        match &key.transport {
            McpTransportKey::Stdio { command, args, env } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &["-y", "server"]);
                assert_eq!(env, &[("TOKEN".to_string(), "abc".to_string())]);
            }
            _ => panic!("expected Stdio transport key"),
        }
    }

    #[test]
    fn key_from_stdio_spec_sorts_args_and_env() {
        let mut env = IndexMap::new();
        env.insert("Z_VAR".into(), JsonField::Str("z".into()));
        env.insert("A_VAR".into(), JsonField::Int(42));
        let spec = stdio_spec(
            "cmd",
            Some(vec!["charlie".into(), "alpha".into(), "bravo".into()]),
            Some(env),
        );
        let key = McpServerKey::from_spec("s", &spec);

        match &key.transport {
            McpTransportKey::Stdio { args, env, .. } => {
                assert_eq!(args, &["alpha", "bravo", "charlie"]);
                assert_eq!(env[0].0, "A_VAR");
                assert_eq!(env[0].1, "42");
                assert_eq!(env[1].0, "Z_VAR");
                assert_eq!(env[1].1, "z");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn key_from_stdio_spec_defaults_empty_when_none() {
        let spec = stdio_spec("echo", None, None);
        let key = McpServerKey::from_spec("bare", &spec);

        match &key.transport {
            McpTransportKey::Stdio { command, args, env } => {
                assert_eq!(command, "echo");
                assert!(args.is_empty());
                assert!(env.is_empty());
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn key_from_remote_http_spec() {
        let spec = remote_spec(McpTransportType::Http, "http://localhost:8080", None);
        let key = McpServerKey::from_spec("http-srv", &spec);

        assert_eq!(key.name, "http-srv");
        match &key.transport {
            McpTransportKey::Remote {
                transport_type,
                url,
                headers,
            } => {
                assert_eq!(*transport_type, McpTransportType::Http);
                assert_eq!(url, "http://localhost:8080");
                assert!(headers.is_empty());
            }
            _ => panic!("expected Remote"),
        }
    }

    #[test]
    fn key_from_remote_sse_spec_with_sorted_headers() {
        let mut hdrs = IndexMap::new();
        hdrs.insert("Z-Key".into(), "z-val".into());
        hdrs.insert("A-Key".into(), "a-val".into());
        let spec = remote_spec(McpTransportType::Sse, "http://sse.example.com", Some(hdrs));
        let key = McpServerKey::from_spec("sse-srv", &spec);

        match &key.transport {
            McpTransportKey::Remote { headers, .. } => {
                assert_eq!(headers[0], ("A-Key".to_string(), "a-val".to_string()));
                assert_eq!(headers[1], ("Z-Key".to_string(), "z-val".to_string()));
            }
            _ => panic!("expected Remote"),
        }
    }

    #[test]
    fn key_equality_same_spec_produces_equal_keys() {
        let spec = stdio_spec("npx", Some(vec!["a".into()]), None);
        let k1 = McpServerKey::from_spec("s", &spec);
        let k2 = McpServerKey::from_spec("s", &spec);
        assert_eq!(k1, k2);
    }

    #[test]
    fn key_inequality_different_names() {
        let spec = stdio_spec("npx", None, None);
        let k1 = McpServerKey::from_spec("a", &spec);
        let k2 = McpServerKey::from_spec("b", &spec);
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_inequality_different_commands() {
        let s1 = stdio_spec("npx", None, None);
        let s2 = stdio_spec("node", None, None);
        let k1 = McpServerKey::from_spec("s", &s1);
        let k2 = McpServerKey::from_spec("s", &s2);
        assert_ne!(k1, k2);
    }

    #[test]
    fn key_env_bool_and_int_coerce_to_string() {
        let mut env = IndexMap::new();
        env.insert("FLAG".into(), JsonField::Bool(true));
        env.insert("PORT".into(), JsonField::Int(3000));
        let spec = stdio_spec("cmd", None, Some(env));
        let key = McpServerKey::from_spec("s", &spec);

        match &key.transport {
            McpTransportKey::Stdio { env, .. } => {
                let map: HashMap<&str, &str> =
                    env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                assert_eq!(map["FLAG"], "true");
                assert_eq!(map["PORT"], "3000");
            }
            _ => panic!("expected Stdio"),
        }
    }

    #[test]
    fn factory_try_get_active_returns_none_when_empty() {
        let factory = McpFactory::default();
        let spec = stdio_spec("cmd", None, None);
        let key = McpServerKey::from_spec("s", &spec);
        assert!(factory.try_get_active(&key).is_none());
    }

    #[test]
    fn factory_try_get_active_returns_none_for_unknown_key() {
        let factory = McpFactory::default();
        let spec = stdio_spec("cmd", None, None);
        let key = McpServerKey::from_spec("s", &spec);
        assert!(factory.try_get_active(&key).is_none());
    }

    #[test]
    fn factory_default_has_empty_active_map() {
        let factory = McpFactory::default();
        let map = factory.active.lock();
        assert!(map.is_empty());
    }

    /// Clears injected spawn outcomes on drop so a panicking test cannot
    /// leak stubs into later tests.
    struct StubSpawnsGuard;

    impl StubSpawnsGuard {
        fn push(result: Result<Arc<ConnectedServer>>) -> Self {
            STUB_SPAWNS.lock().push(result);
            StubSpawnsGuard
        }
    }

    impl Drop for StubSpawnsGuard {
        fn drop(&mut self) {
            STUB_SPAWNS.lock().clear();
        }
    }

    /// An in-process client/server pair; the returned server half must stay
    /// alive for the client handle to keep working.
    async fn fixture_handle() -> (Arc<ConnectedServer>, impl Sized) {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (server, client) = tokio::join!(
            FixtureServer::default().serve(server_io),
            ().serve(client_io)
        );
        (Arc::new(client.unwrap()), server.unwrap())
    }

    fn ctx_with_mcp_hooks(marker: &str) -> RequestContext {
        let mut hooks_map = HooksMap::default();
        for event in ["mcp.server.connected", "mcp.server.failed"] {
            hooks_map.insert(
                event.to_string(),
                vec![HookDef {
                    name: format!("{marker}-{event}"),
                    command: "true".to_string(),
                }],
            );
        }
        let mut app = AppState::test_default();
        app.config = Arc::new(AppConfig {
            hooks: hooks_map,
            ..Default::default()
        });
        RequestContext::new(Arc::new(app), WorkingMode::Cmd)
    }

    fn mcp_captures(marker: &str) -> Vec<test_sink::Capture> {
        test_sink::snapshot()
            .into_iter()
            .filter(|capture| capture.hook_name.starts_with(marker))
            .collect()
    }

    #[tokio::test]
    #[serial]
    async fn acquire_fires_connected_only_on_real_spawns_and_marks_reconnects() {
        let _sink = test_sink::install();
        let marker = "mcp-spawn-k7d";
        let ctx = ctx_with_mcp_hooks(marker);
        let hooks = McpServerHooks::resolve(&ctx);
        let factory = McpFactory::default();
        let spec = stdio_spec("fixture-server-cmd", None, None);

        let (first, _first_server) = fixture_handle().await;
        let stub = StubSpawnsGuard::push(Ok(first.clone()));
        let connected = factory.acquire("srv", &spec, None, &hooks).await.unwrap();
        drop(stub);
        assert!(Arc::ptr_eq(&connected, &first));
        let captures = mcp_captures(marker);
        assert_eq!(captures.len(), 1, "{captures:?}");
        let capture = &captures[0];
        assert_eq!(capture.hook_name, format!("{marker}-mcp.server.connected"));
        assert_eq!(
            capture.envs.get("COYOTE_MCP_SERVER").map(String::as_str),
            Some("srv")
        );
        assert_eq!(
            capture.envs.get("COYOTE_MCP_TRANSPORT").map(String::as_str),
            Some("stdio")
        );
        assert!(
            !capture.envs.contains_key("COYOTE_MCP_RECONNECT"),
            "a first connect must not be marked as a reconnect"
        );

        let reused = factory.acquire("srv", &spec, None, &hooks).await.unwrap();
        assert!(Arc::ptr_eq(&reused, &first));
        assert_eq!(
            mcp_captures(marker).len(),
            1,
            "reusing a live server must fire nothing"
        );

        // Every holder dropped: the weak in the factory dies, and the next
        // acquire is a real spawn again — now marked as a reconnect.
        drop(connected);
        drop(reused);
        drop(first);
        let (second, _second_server) = fixture_handle().await;
        let _stub = StubSpawnsGuard::push(Ok(second.clone()));
        let respawned = factory.acquire("srv", &spec, None, &hooks).await.unwrap();
        assert!(Arc::ptr_eq(&respawned, &second));
        let captures = mcp_captures(marker);
        assert_eq!(captures.len(), 2, "{captures:?}");
        assert_eq!(
            captures[1]
                .envs
                .get("COYOTE_MCP_RECONNECT")
                .map(String::as_str),
            Some("true")
        );
    }

    #[tokio::test]
    #[serial]
    async fn acquire_spawn_failure_fires_failed_with_the_error() {
        let _sink = test_sink::install();
        let marker = "mcp-fail-v2q";
        let ctx = ctx_with_mcp_hooks(marker);
        let hooks = McpServerHooks::resolve(&ctx);
        let factory = McpFactory::default();
        // No stub: the nonexistent binary drives the real spawn-error path.
        let spec = stdio_spec("coyote-nonexistent-mcp-binary-v2q", None, None);

        let result = factory.acquire("bad-srv", &spec, None, &hooks).await;

        assert!(result.is_err());
        let captures = mcp_captures(marker);
        assert_eq!(captures.len(), 1, "{captures:?}");
        let capture = &captures[0];
        assert_eq!(capture.hook_name, format!("{marker}-mcp.server.failed"));
        assert_eq!(
            capture.envs.get("COYOTE_MCP_SERVER").map(String::as_str),
            Some("bad-srv")
        );
        assert_eq!(
            capture.envs.get("COYOTE_MCP_TRANSPORT").map(String::as_str),
            Some("stdio")
        );
        assert!(
            capture
                .envs
                .get("COYOTE_ERROR")
                .is_some_and(|error| !error.is_empty())
        );
        assert!(
            !capture.envs.contains_key("COYOTE_MCP_AUTH_REQUIRED"),
            "a plain spawn failure must not be flagged as auth-required"
        );
    }

    #[tokio::test]
    #[serial]
    async fn acquire_auth_failure_sets_the_auth_required_flag() {
        let _sink = test_sink::install();
        let marker = "mcp-auth-h4n";
        let ctx = ctx_with_mcp_hooks(marker);
        let hooks = McpServerHooks::resolve(&ctx);
        let factory = McpFactory::default();
        let spec = stdio_spec("oauth-cmd", None, None);
        let _stub = StubSpawnsGuard::push(Err(anyhow!("Auth required: no stored token")));

        let err = factory
            .acquire("oauth-srv", &spec, None, &hooks)
            .await
            .unwrap_err();

        assert!(
            err.downcast_ref::<McpAuthRequired>().is_some(),
            "the auth error must be wrapped in McpAuthRequired context"
        );
        let captures = mcp_captures(marker);
        assert_eq!(captures.len(), 1, "{captures:?}");
        let capture = &captures[0];
        assert_eq!(capture.hook_name, format!("{marker}-mcp.server.failed"));
        assert_eq!(
            capture
                .envs
                .get("COYOTE_MCP_AUTH_REQUIRED")
                .map(String::as_str),
            Some("true")
        );
    }
}
