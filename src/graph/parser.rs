//! YAML parsing for graph definitions.

use super::types::Graph;
use crate::config::paths;
use anyhow::{Context, Error, Result, anyhow, bail};
use std::fs::read_to_string;
use std::path::{Path, PathBuf};

const SUPPORTED_VERSIONS: &[&str] = &["1.0"];

/// Parser for graph YAML files. The `base_dir` is used to resolve relative
/// paths passed to [`GraphParser::load_from_file`], and is typically an
/// agent directory.
pub struct GraphParser {
    base_dir: PathBuf,
}

impl GraphParser {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Load and validate a graph from a YAML file. Relative paths are
    /// resolved against `base_dir`.
    pub fn load_from_file(&self, path: impl AsRef<Path>) -> Result<Graph> {
        let path = path.as_ref();
        let full_path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.base_dir.join(path)
        };

        let contents = read_to_string(&full_path)
            .with_context(|| format!("Failed to read graph file at '{}'", full_path.display()))?;

        self.load_from_string(&contents)
            .with_context(|| format!("Failed to parse graph file at '{}'", full_path.display()))
    }

    /// Load and validate a graph from a YAML string.
    pub fn load_from_string(&self, yaml: &str) -> Result<Graph> {
        let mut graph: Graph = serde_yaml::from_str(yaml).map_err(enhance_yaml_error)?;

        validate_schema_version(&graph.version)?;

        for (key, node) in &mut graph.nodes {
            if node.id.is_empty() {
                node.id = key.clone();
            } else if &node.id != key {
                bail!(
                    "Node ID mismatch: key '{}' does not match node.id '{}'",
                    key,
                    node.id
                );
            }
        }

        validate_structure(&graph)?;

        Ok(graph)
    }
}

fn validate_schema_version(version: &str) -> Result<()> {
    if !SUPPORTED_VERSIONS.contains(&version) {
        bail!(
            "Unsupported graph schema version '{}'. Supported versions: {}",
            version,
            SUPPORTED_VERSIONS.join(", ")
        );
    }
    Ok(())
}

fn validate_structure(graph: &Graph) -> Result<()> {
    if graph.name.is_empty() {
        bail!("Graph must have a non-empty 'name' field");
    }

    if graph.nodes.is_empty() {
        bail!("Graph '{}' has no nodes defined", graph.name);
    }

    if !graph.has_node(&graph.start) {
        bail!(
            "Start node '{}' not found in graph '{}'. Available nodes: {}",
            graph.start,
            graph.name,
            graph.node_ids().join(", ")
        );
    }

    Ok(())
}

fn enhance_yaml_error(error: serde_yaml::Error) -> Error {
    let msg = error.to_string();

    let hint = if msg.contains("missing field") {
        "\n\nHint: Check that all required fields are present.\n\
         Top-level required fields: `name`, `start`, `nodes`.\n\
         Each node requires `type` plus that type's fields:\n\
         - agent:    `agent`, `prompt`\n\
         - script:   `script`\n\
         - approval: `question`, `options`, `routes`, `on_other`\n\
         - input:    `question`\n\
         - llm:      `prompt`\n\
         - rag:      `documents`\n\
         - end:      (no required fields)"
    } else if msg.contains("unknown field") || msg.contains("unknown variant") {
        "\n\nHint: Check for typos in field names or `type:` values.\n\
         Valid node types: agent, script, approval, input, llm, rag, end."
    } else if msg.contains("invalid type") {
        "\n\nHint: Check that field values have the correct type.\n\
         - Strings should be quoted if they contain special characters\n\
         - Numbers should not be quoted\n\
         - Lists use YAML array syntax (- item1)\n\
         - Maps use YAML object syntax (key: value)"
    } else {
        ""
    };

    anyhow!("YAML parsing error: {}{}", msg, hint)
}

/// Returns true if the named agent has a `graph.yaml` in its data directory.
pub fn agent_has_graph(agent_name: &str) -> bool {
    paths::agent_graph_file(agent_name).exists()
}

#[cfg(test)]
mod tests {
    use super::super::types::NodeType;
    use super::*;
    use std::env;

    fn parser() -> GraphParser {
        GraphParser::new(env::current_dir().unwrap())
    }

    #[test]
    fn parses_a_simple_graph() {
        let yaml = r#"
name: simple_graph
version: "1.0"
start: node1
nodes:
  node1:
    id: node1
    type: agent
    agent: test_agent
    prompt: "Hello world"
    next: node2
  node2:
    id: node2
    type: end
    output: done
"#;
        let graph = parser().load_from_string(yaml).unwrap();
        assert_eq!(graph.name, "simple_graph");
        assert_eq!(graph.start, "node1");
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(
            graph.nodes.get("node1").unwrap().next.as_deref(),
            Some("node2")
        );
    }

    #[test]
    fn auto_fills_node_ids_from_keys() {
        let yaml = r#"
name: auto_id_graph
version: "1.0"
start: node1
nodes:
  node1:
    type: agent
    agent: test_agent
    prompt: Test
    next: node2
  node2:
    type: end
    output: done
"#;
        let graph = parser().load_from_string(yaml).unwrap();
        assert_eq!(graph.nodes.get("node1").unwrap().id, "node1");
        assert_eq!(graph.nodes.get("node2").unwrap().id, "node2");
    }

    #[test]
    fn rejects_missing_start_node() {
        let yaml = r#"
name: bad_graph
version: "1.0"
start: nonexistent
nodes:
  node1:
    type: end
"#;
        let err = parser().load_from_string(yaml).unwrap_err().to_string();
        assert!(
            err.contains("Start node 'nonexistent' not found"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_empty_graph_name() {
        let yaml = r#"
name: ""
version: "1.0"
start: node1
nodes:
  node1:
    type: end
"#;
        let err = parser().load_from_string(yaml).unwrap_err().to_string();
        assert!(err.contains("non-empty 'name'"), "got: {err}");
    }

    #[test]
    fn rejects_no_nodes() {
        let yaml = r#"
name: empty_graph
version: "1.0"
start: node1
nodes: {}
"#;
        let err = parser().load_from_string(yaml).unwrap_err().to_string();
        assert!(err.contains("no nodes defined"), "got: {err}");
    }

    #[test]
    fn rejects_unsupported_version() {
        let yaml = r#"
name: future_graph
version: "2.0"
start: node1
nodes:
  node1:
    type: end
"#;
        let err = parser().load_from_string(yaml).unwrap_err().to_string();
        assert!(
            err.contains("Unsupported graph schema version"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_node_id_mismatch() {
        let yaml = r#"
name: mismatch_graph
version: "1.0"
start: node1
nodes:
  node1:
    id: different_id
    type: end
"#;
        let err = parser().load_from_string(yaml).unwrap_err().to_string();
        assert!(err.contains("Node ID mismatch"), "got: {err}");
    }

    #[test]
    fn parses_approval_node_with_routes() {
        let yaml = r#"
name: approval_graph
version: "1.0"
start: approval1
nodes:
  approval1:
    type: approval
    question: "Proceed with deployment?"
    options:
      - "Yes"
      - "No"
    routes:
      "Yes": deploy
      "No": cancel
    on_other: cancel
  deploy:
    type: end
  cancel:
    type: end
"#;
        let graph = parser().load_from_string(yaml).unwrap();
        let approval = graph.nodes.get("approval1").unwrap();
        match &approval.node_type {
            NodeType::Approval(a) => {
                assert_eq!(a.options.len(), 2);
                assert_eq!(a.routes.len(), 2);
                assert_eq!(a.routes.get("Yes").map(|s| s.as_str()), Some("deploy"));
            }
            _ => panic!("expected approval node"),
        }
    }

    #[test]
    fn parses_settings_overrides() {
        let yaml = r#"
name: settings_graph
version: "1.0"
start: node1
settings:
  max_loop_iterations: 50
  timeout: 300
  log_state_snapshots: false
nodes:
  node1:
    type: end
"#;
        let graph = parser().load_from_string(yaml).unwrap();
        assert_eq!(graph.settings.max_loop_iterations, 50);
        assert_eq!(graph.settings.timeout, Some(300));
        assert!(!graph.settings.log_state_snapshots);
        assert!(graph.settings.validate_before_run);
    }

    #[test]
    fn parses_initial_state() {
        let yaml = r#"
name: state_graph
version: "1.0"
start: node1
initial_state:
  user_name: "Alice"
  count: 42
  enabled: true
nodes:
  node1:
    type: end
"#;
        let graph = parser().load_from_string(yaml).unwrap();
        assert_eq!(graph.initial_state.len(), 3);
        assert_eq!(graph.initial_state.get("user_name").unwrap(), "Alice");
        assert_eq!(
            graph.initial_state.get("count").unwrap(),
            &serde_json::json!(42)
        );
        assert_eq!(
            graph.initial_state.get("enabled").unwrap(),
            &serde_json::json!(true)
        );
    }

    #[test]
    fn uses_default_version_when_absent() {
        let yaml = r#"
name: no_version
start: node1
nodes:
  node1:
    type: end
"#;
        let graph = parser().load_from_string(yaml).unwrap();
        assert_eq!(graph.version, super::super::GRAPH_SCHEMA_VERSION);
    }

    #[test]
    fn rejects_unknown_node_type_with_hint() {
        let yaml = r#"
name: bad_type
version: "1.0"
start: node1
nodes:
  node1:
    type: nonsense
"#;
        let err = parser().load_from_string(yaml).unwrap_err().to_string();
        assert!(
            err.contains("Valid node types") || err.contains("unknown variant"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_malformed_yaml() {
        let yaml = "name: bad\n  bad: indent\nstart: a";
        let result = parser().load_from_string(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn missing_required_fields_have_a_hint() {
        let yaml = r#"
name: missing_start
version: "1.0"
nodes:
  node1:
    type: end
"#;
        let err = parser().load_from_string(yaml).unwrap_err().to_string();
        assert!(err.contains("Hint"), "got: {err}");
    }

    #[test]
    fn load_from_file_reads_disk() {
        use std::io::Write;
        let dir = env::temp_dir();
        let path = dir.join(format!(
            "loki_graph_parser_test_{}.yaml",
            std::process::id()
        ));
        let yaml = r#"
name: disk_graph
version: "1.0"
start: only
nodes:
  only:
    type: end
    output: ok
"#;
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(yaml.as_bytes()).unwrap();
        }

        let graph = GraphParser::new(dir).load_from_file(&path).unwrap();
        assert_eq!(graph.name, "disk_graph");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_from_file_errors_on_missing_path() {
        let err = parser()
            .load_from_file("/definitely/not/a/real/path/to_any_graph.yaml")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Failed to read graph file"), "got: {err}");
    }

    #[test]
    fn agent_has_graph_false_for_unknown_agent() {
        assert!(!agent_has_graph("__nonexistent_agent_for_test__"));
    }
}
