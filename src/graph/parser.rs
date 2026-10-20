use super::types::{Graph, NodeType, SessionBlock};
use super::validator::{denied_chat_tool_message, is_chat_tool_denied, write_set_of};
use crate::config::INPUT_PLACEHOLDER;
use crate::config::paths;
use anyhow::{Context, Error, Result, anyhow, bail};
use serde::Deserialize;
use serde_yaml::Value;
use std::collections::HashSet;
use std::fs::read_to_string;
use std::path::{Path, PathBuf};

const SUPPORTED_VERSIONS: &[&str] = &["1.0"];

pub struct GraphParser {
    base_dir: PathBuf,
}

impl GraphParser {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

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

    if let Some(block) = &graph.session {
        validate_session_block(graph, block)?;
    }

    Ok(())
}

fn validate_session_block(graph: &Graph, block: &SessionBlock) -> Result<()> {
    if block.chat.instructions.trim().is_empty() {
        bail!("session.chat.instructions must be non-empty");
    }

    if block.chat.instructions.contains(INPUT_PLACEHOLDER) {
        bail!("session.chat.instructions must not contain '{INPUT_PLACEHOLDER}'");
    }

    let route_instructions = block.route.as_ref().and_then(|r| r.instructions.as_deref());
    if route_instructions.is_some_and(|text| text.contains(INPUT_PLACEHOLDER)) {
        bail!("session.route.instructions must not contain '{INPUT_PLACEHOLDER}'");
    }

    for tool in &block.chat.tools {
        if is_chat_tool_denied(tool) {
            bail!("{}", denied_chat_tool_message(tool));
        }
    }

    let mut seen = HashSet::new();
    for (i, key) in block.expose.iter().enumerate() {
        if key.trim().is_empty() {
            bail!("session.expose entry #{i} is blank");
        }
        if !seen.insert(key.as_str()) {
            bail!("session.expose key '{key}' is listed more than once");
        }
        if !state_key_is_declared(graph, key) {
            bail!(
                "session.expose key '{key}' is not declared anywhere (add it to initial_state, or have a node write it via state_updates / output_schema)"
            );
        }
    }

    Ok(())
}

/// Map nodes are not covered by `write_set_of` (it returns an empty set for
/// them), so `collect_into` is checked here directly.
fn state_key_is_declared(graph: &Graph, key: &str) -> bool {
    graph.initial_state.contains_key(key)
        || graph.variables.iter().any(|v| v.name == key)
        || graph
            .nodes
            .values()
            .any(|node| write_set_of(node).is_some_and(|writes| writes.contains(key)))
        || graph
            .nodes
            .values()
            .any(|node| matches!(&node.node_type, NodeType::Map(m) if m.collect_into == key))
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
         Valid node types: agent, script, approval, input, llm, rag, end.\n\
         Optional top-level block: `session` (keys: expose, route, chat)."
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

pub fn agent_has_graph(agent_name: &str) -> bool {
    paths::agent_graph_file(agent_name).exists()
}

/// Reads only the `session` key so the check does not depend on the rest of
/// the file being a valid graph.
#[derive(Deserialize)]
struct SessionProbe {
    #[serde(default)]
    session: Option<Value>,
}

pub fn graph_supports_sessions(agent_name: &str) -> bool {
    read_to_string(paths::agent_graph_file(agent_name))
        .ok()
        .and_then(|yaml| serde_yaml::from_str::<SessionProbe>(&yaml).ok())
        .is_some_and(|probe| probe.session.is_some())
}

pub fn no_session_block_text(agent_name: &str) -> String {
    format!(
        "Graph-based agent '{agent_name}' has no `session:` block in its graph.yaml, \
         so it cannot hold a session. Add a `session:` block (see the Graph Agents \
         wiki page, \"Sessions\") to enable one."
    )
}

#[cfg(test)]
mod tests {
    use super::super::GRAPH_SCHEMA_VERSION;
    use super::super::types::NodeType;
    use super::*;
    use crate::testing::TestConfigDirGuard;
    use indoc::formatdoc;
    use serial_test::serial;
    use std::fs::File;
    use std::io::Write;
    use std::{env, fs, process};

    fn parser() -> GraphParser {
        GraphParser::new(env::current_dir().unwrap())
    }

    #[test]
    fn parses_a_simple_graph() {
        let yaml = formatdoc! {r#"
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
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        assert_eq!(graph.name, "simple_graph");
        assert_eq!(graph.start, "node1");
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(
            graph.nodes.get("node1").unwrap().next_target(),
            Some("node2")
        );
    }

    #[test]
    fn auto_fills_node_ids_from_keys() {
        let yaml = formatdoc! {r#"
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
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        assert_eq!(graph.nodes.get("node1").unwrap().id, "node1");
        assert_eq!(graph.nodes.get("node2").unwrap().id, "node2");
    }

    #[test]
    fn rejects_missing_start_node() {
        let yaml = formatdoc! {r#"
            name: bad_graph
            version: "1.0"
            start: nonexistent
            nodes:
              node1:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(
            err.contains("Start node 'nonexistent' not found"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_empty_graph_name() {
        let yaml = formatdoc! {r#"
            name: ""
            version: "1.0"
            start: node1
            nodes:
              node1:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(err.contains("non-empty 'name'"), "got: {err}");
    }

    #[test]
    fn rejects_no_nodes() {
        let yaml = formatdoc! {r#"
            name: empty_graph
            version: "1.0"
            start: node1
            nodes: {}
            "#, "{}"};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(err.contains("no nodes defined"), "got: {err}");
    }

    #[test]
    fn rejects_unsupported_version() {
        let yaml = formatdoc! {r#"
            name: future_graph
            version: "2.0"
            start: node1
            nodes:
              node1:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(
            err.contains("Unsupported graph schema version"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_node_id_mismatch() {
        let yaml = formatdoc! {r#"
            name: mismatch_graph
            version: "1.0"
            start: node1
            nodes:
              node1:
                id: different_id
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(err.contains("Node ID mismatch"), "got: {err}");
    }

    #[test]
    fn parses_approval_node_with_routes() {
        let yaml = formatdoc! {r#"
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
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

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
        let yaml = formatdoc! {r#"
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
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        assert_eq!(graph.settings.max_loop_iterations, 50);
        assert_eq!(graph.settings.timeout, Some(300));
        assert!(!graph.settings.log_state_snapshots);
        assert!(graph.settings.validate_before_run);
    }

    #[test]
    fn parses_initial_state() {
        let yaml = formatdoc! {r#"
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
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

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
        let yaml = formatdoc! {r#"
            name: no_version
            start: node1
            nodes:
              node1:
                type: end
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        assert_eq!(graph.version, GRAPH_SCHEMA_VERSION);
    }

    #[test]
    fn rejects_unknown_node_type_with_hint() {
        let yaml = formatdoc! {r#"
            name: bad_type
            version: "1.0"
            start: node1
            nodes:
              node1:
                type: nonsense
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

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
        let yaml = formatdoc! {r#"
            name: missing_start
            version: "1.0"
            nodes:
              node1:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(err.contains("Hint"), "got: {err}");
    }

    #[test]
    fn load_from_file_reads_disk() {
        let dir = env::temp_dir();
        let path = dir.join(format!("coyote_graph_parser_test_{}.yaml", process::id()));
        let yaml = formatdoc! {r#"
            name: disk_graph
            version: "1.0"
            start: only
            nodes:
              only:
                type: end
                output: ok
            "#};
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(yaml.as_bytes()).unwrap();
        }

        let graph = GraphParser::new(dir).load_from_file(&path).unwrap();

        assert_eq!(graph.name, "disk_graph");

        let _ = fs::remove_file(&path);
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

    #[test]
    fn parses_minimal_session_block() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              chat:
                instructions: "Answer questions about the last run."
            nodes:
              e:
                type: end
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        let block = graph.session.unwrap();
        assert_eq!(
            block.chat.instructions,
            "Answer questions about the last run."
        );
        assert!(block.expose.is_empty());
        assert!(block.route.is_none());
    }

    #[test]
    fn parses_full_session_block() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            initial_state:
              report: ""
            session:
              expose: [report]
              route:
                model: some-model
                instructions: "Prefer chat for follow-up questions."
              chat:
                model: some-model
                instructions: "Answer from the last run."
                memory: true
                tools: [fs_read]
            nodes:
              e:
                type: end
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        let block = graph.session.unwrap();
        assert_eq!(block.expose, vec!["report".to_string()]);
        let route = block.route.unwrap();
        assert_eq!(route.model.as_deref(), Some("some-model"));
        assert_eq!(
            route.instructions.as_deref(),
            Some("Prefer chat for follow-up questions.")
        );
        assert_eq!(block.chat.model.as_deref(), Some("some-model"));
        assert!(block.chat.memory);
        assert_eq!(block.chat.tools, vec!["fs_read".to_string()]);
    }

    #[test]
    fn rejects_session_block_without_chat() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              expose: []
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(err.contains("missing field `chat`"), "got: {err}");
    }

    #[test]
    fn rejects_empty_chat_instructions() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              chat:
                instructions: "   "
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(
            err.contains("session.chat.instructions must be non-empty"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_input_placeholder_in_chat_instructions() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              chat:
                instructions: "Answer this: {INPUT_PLACEHOLDER}"
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(
            err.contains("session.chat.instructions must not contain '__INPUT__'"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_input_placeholder_in_route_instructions() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              route:
                instructions: "Route this: {INPUT_PLACEHOLDER}"
              chat:
                instructions: "Answer."
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(
            err.contains("session.route.instructions must not contain '__INPUT__'"),
            "got: {err}"
        );
    }

    #[test]
    fn loads_a_formerly_named_tool_in_chat_tools() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              chat:
                instructions: "Answer."
                tools: [fs_write, execute_command]
            nodes:
              e:
                type: end
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        assert_eq!(
            graph.session.unwrap().chat.tools,
            vec!["fs_write".to_string(), "execute_command".to_string()]
        );
    }

    #[test]
    fn rejects_denied_tool_prefix_in_chat_tools() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              chat:
                instructions: "Answer."
                tools: [user__select]
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(
            err.contains(
                "session.chat.tools entry 'user__select' is not allowed in the chat turn (the agent__, job__, todo__, user__ and memory__ built-in families are refused)"
            ),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_denied_agent_prefix_in_chat_tools() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              chat:
                instructions: "Answer."
                tools: [fs_read, agent__spawn]
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(
            err.contains(
                "session.chat.tools entry 'agent__spawn' is not allowed in the chat turn (the agent__, job__, todo__, user__ and memory__ built-in families are refused)"
            ),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_unknown_key_in_session_block() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              exposed: [report]
              chat:
                instructions: "Answer."
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(err.contains("unknown field"), "got: {err}");
        assert!(
            err.contains("Optional top-level block: `session` (keys: expose, route, chat)."),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_unknown_key_in_route_block() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              route:
                prompt: "Route."
              chat:
                instructions: "Answer."
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(err.contains("unknown field"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_key_in_chat_block() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              chat:
                instructions: "Answer."
                temperature: 0.5
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(err.contains("unknown field"), "got: {err}");
    }

    #[test]
    fn rejects_undeclared_expose_key() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              expose: [report]
              chat:
                instructions: "Answer."
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(
            err.contains(
                "session.expose key 'report' is not declared anywhere (add it to initial_state, or have a node write it via state_updates / output_schema)"
            ),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_duplicate_expose_key() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            initial_state:
              report: ""
            session:
              expose: [report, report]
              chat:
                instructions: "Answer."
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(
            err.contains("session.expose key 'report' is listed more than once"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_blank_expose_key() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              expose: ["  "]
              chat:
                instructions: "Answer."
            nodes:
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(
            err.contains("session.expose entry #0 is blank"),
            "got: {err}"
        );
    }

    #[test]
    fn expose_key_declared_via_initial_state_loads() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            initial_state:
              report: ""
            session:
              expose: [report]
              chat:
                instructions: "Answer."
            nodes:
              e:
                type: end
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        assert_eq!(graph.session.unwrap().expose, vec!["report".to_string()]);
    }

    #[test]
    fn expose_key_declared_via_state_updates_loads() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: s
            session:
              expose: [report]
              chat:
                instructions: "Answer."
            nodes:
              s:
                type: script
                script: "echo hi"
                state_updates:
                  report: "{}"
                next: e
              e:
                type: end
            "#, "{{output}}"};

        let graph = parser().load_from_string(&yaml).unwrap();

        assert_eq!(graph.session.unwrap().expose, vec!["report".to_string()]);
    }

    #[test]
    fn expose_key_declared_via_output_schema_loads() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: l
            session:
              expose: [report]
              chat:
                instructions: "Answer."
            nodes:
              l:
                type: llm
                prompt: "summarise"
                output_schema:
                  type: object
                  properties:
                    report:
                      type: string
                next: e
              e:
                type: end
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        assert_eq!(graph.session.unwrap().expose, vec!["report".to_string()]);
    }

    #[test]
    fn expose_key_declared_via_variables_loads() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            variables:
              - name: report
                description: seeded into state at run
                default: ""
            session:
              expose: [report]
              chat:
                instructions: "Answer."
            nodes:
              e:
                type: end
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        assert_eq!(graph.session.unwrap().expose, vec!["report".to_string()]);
    }

    #[test]
    fn expose_key_declared_via_map_collect_into_loads() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: fan_out
            session:
              expose: [report]
              chat:
                instructions: "Answer."
            nodes:
              fan_out:
                type: map
                over: "{{{{items}}}}"
                as: item
                branch: worker
                collect_into: report
                next: e
              worker:
                type: llm
                prompt: "summarise"
              e:
                type: end
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        assert_eq!(graph.session.unwrap().expose, vec!["report".to_string()]);
    }

    fn write_probe_graph(name: &str, yaml: &str) {
        let path = paths::agent_graph_file(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, yaml).unwrap();
    }

    #[test]
    #[serial]
    fn graph_supports_sessions_true_for_block_graph() {
        let _guard = TestConfigDirGuard::new("graph-sessions-probe");
        let name = "probe_block_graph";
        write_probe_graph(
            name,
            "name: g\nstart: e\nsession:\n  chat:\n    instructions: hi\nnodes:\n  e:\n    type: end\n",
        );

        assert!(graph_supports_sessions(name));
    }

    #[test]
    #[serial]
    fn graph_supports_sessions_false_for_blockless_graph() {
        let _guard = TestConfigDirGuard::new("graph-sessions-probe");
        let name = "probe_blockless_graph";
        write_probe_graph(name, "name: g\nstart: e\nnodes:\n  e:\n    type: end\n");

        assert!(!graph_supports_sessions(name));
    }

    #[test]
    #[serial]
    fn graph_supports_sessions_false_for_missing_file() {
        let _guard = TestConfigDirGuard::new("graph-sessions-probe");

        assert!(!graph_supports_sessions("probe_missing_graph"));
    }

    #[test]
    #[serial]
    fn graph_supports_sessions_false_for_malformed_yaml() {
        let _guard = TestConfigDirGuard::new("graph-sessions-probe");
        let name = "probe_malformed_graph";
        write_probe_graph(name, "name: [unclosed");

        assert!(!graph_supports_sessions(name));
    }

    #[test]
    fn no_session_block_text_is_the_teaching_sentence() {
        assert_eq!(
            no_session_block_text("x"),
            "Graph-based agent 'x' has no `session:` block in its graph.yaml, so it cannot hold a session. Add a `session:` block (see the Graph Agents wiki page, \"Sessions\") to enable one."
        );
    }

    #[test]
    fn unknown_key_hint_names_the_session_block_for_route_and_chat_too() {
        let with_route_typo = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              route:
                prompt: "Route."
              chat:
                instructions: "Answer."
            nodes:
              e:
                type: end
            "#};
        let with_chat_typo = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
              chat:
                instructions: "Answer."
                temperature: 0.5
            nodes:
              e:
                type: end
            "#};
        for yaml in [with_route_typo, with_chat_typo] {
            let err = parser().load_from_string(&yaml).unwrap_err().to_string();

            assert!(err.contains("unknown field"), "got: {err}");
            assert!(
                err.contains("Optional top-level block: `session` (keys: expose, route, chat)."),
                "got: {err}"
            );
            assert!(err.is_ascii(), "non-ASCII error text: {err}");
        }
    }

    #[test]
    fn null_session_key_is_no_block() {
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: e
            session:
            nodes:
              e:
                type: end
            "#};

        let graph = parser().load_from_string(&yaml).unwrap();

        assert!(graph.session.is_none());
    }

    #[test]
    fn undeclared_expose_key_is_rejected_even_beside_a_script_without_a_write_set() {
        // `write_set_of` yields `None` (unknown writes) for a script node with
        // no `state_updates`; that must not read as "declares everything".
        let yaml = formatdoc! {r#"
            name: session_graph
            version: "1.0"
            start: s
            session:
              expose: [report]
              chat:
                instructions: "Answer."
            nodes:
              s:
                type: script
                script: "true"
                next: e
              e:
                type: end
            "#};

        let err = parser().load_from_string(&yaml).unwrap_err().to_string();

        assert!(
            err.contains("session.expose key 'report' is not declared anywhere"),
            "got: {err}"
        );
    }

    #[test]
    fn session_block_errors_are_ascii_and_name_the_failing_field() {
        let cases = [
            (
                "chat:\n    instructions: \"\"",
                "session.chat.instructions must be non-empty",
            ),
            (
                "chat:\n    instructions: \"x __INPUT__ y\"",
                "session.chat.instructions must not contain '__INPUT__'",
            ),
            (
                "route:\n    instructions: __INPUT__\n  chat:\n    instructions: ok",
                "session.route.instructions must not contain '__INPUT__'",
            ),
            (
                "expose: [\"\"]\n  chat:\n    instructions: ok",
                "session.expose entry #0 is blank",
            ),
            (
                "expose: [a, b, b]\n  chat:\n    instructions: ok",
                "session.expose key 'b' is listed more than once",
            ),
        ];
        for (block, expected) in cases {
            let yaml = formatdoc! {r#"
                name: session_graph
                version: "1.0"
                start: e
                initial_state:
                  a: ""
                  b: ""
                session:
                  {block}
                nodes:
                  e:
                    type: end
                "#};

            let err = parser().load_from_string(&yaml).unwrap_err().to_string();

            assert!(err.contains(expected), "block {block:?}: got {err}");
            assert!(err.is_ascii(), "non-ASCII error text: {err}");
        }
    }

    #[test]
    #[serial]
    fn graph_supports_sessions_follows_the_loader_on_a_null_session_key() {
        let _guard = TestConfigDirGuard::new("graph-sessions-probe");
        let name = "probe_null_session_graph";
        write_probe_graph(
            name,
            "name: g\nstart: e\nsession:\nnodes:\n  e:\n    type: end\n",
        );

        assert!(!graph_supports_sessions(name));
    }

    #[test]
    #[serial]
    fn graph_supports_sessions_reports_presence_not_validity() {
        let _guard = TestConfigDirGuard::new("graph-sessions-probe");
        let name = "probe_empty_session_graph";
        write_probe_graph(
            name,
            "name: g\nstart: e\nsession: {}\nnodes:\n  e:\n    type: end\n",
        );

        assert!(graph_supports_sessions(name));
        assert!(
            parser()
                .load_from_file(paths::agent_graph_file(name))
                .is_err()
        );
    }

    #[test]
    #[serial]
    fn graph_supports_sessions_false_for_non_mapping_yaml_and_directory_in_place_of_file() {
        let _guard = TestConfigDirGuard::new("graph-sessions-probe");

        let list = "probe_list_graph";
        write_probe_graph(list, "- not\n- a\n- mapping\n");
        assert!(!graph_supports_sessions(list));

        let dir = "probe_dir_graph";
        fs::create_dir_all(paths::agent_graph_file(dir)).unwrap();
        assert!(!graph_supports_sessions(dir));
    }
}
