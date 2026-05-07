use crate::mcp::{ConnectedServer, JsonField, McpServer, McpTransportType, spawn_mcp_server};

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
    ) -> Result<Arc<ConnectedServer>> {
        let key = McpServerKey::from_spec(name, spec);

        if let Some(existing) = self.try_get_active(&key) {
            return Ok(existing);
        }

        let handle = spawn_mcp_server(spec, log_path).await?;
        self.insert_active(key, &handle);
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{JsonField, McpServer, McpTransportType};
    use std::collections::HashMap;

    fn stdio_spec(
        command: &str,
        args: Option<Vec<String>>,
        env: Option<HashMap<String, JsonField>>,
    ) -> McpServer {
        McpServer {
            transport_type: McpTransportType::Stdio,
            command: Some(command.to_string()),
            args,
            env,
            cwd: None,
            url: None,
            headers: None,
        }
    }

    fn remote_spec(
        transport: McpTransportType,
        url: &str,
        headers: Option<HashMap<String, String>>,
    ) -> McpServer {
        McpServer {
            transport_type: transport,
            command: None,
            args: None,
            env: None,
            cwd: None,
            url: Some(url.to_string()),
            headers,
        }
    }

    #[test]
    fn key_from_stdio_spec_captures_command_args_env() {
        let mut env = HashMap::new();
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
        let mut env = HashMap::new();
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
        let mut hdrs = HashMap::new();
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
        let mut env = HashMap::new();
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
}
