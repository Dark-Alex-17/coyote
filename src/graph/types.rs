//! Core data structures for graph-based agent orchestration.

use anyhow::Result;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// A graph definition loaded from YAML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Graph {
    pub name: String,

    #[serde(default)]
    pub description: String,

    #[serde(default = "default_schema_version")]
    pub version: String,

    #[serde(default)]
    pub settings: GraphSettings,

    #[serde(default)]
    pub initial_state: HashMap<String, Value>,

    pub start: String,

    pub nodes: IndexMap<String, Node>,
}

impl Graph {
    pub fn get_node(&self, id: &str) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn has_node(&self, id: &str) -> bool {
        self.nodes.contains_key(id)
    }

    pub fn node_ids(&self) -> Vec<&str> {
        self.nodes.keys().map(|s| s.as_str()).collect()
    }
}

fn default_schema_version() -> String {
    super::GRAPH_SCHEMA_VERSION.to_string()
}

/// Graph-level settings.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GraphSettings {
    #[serde(default = "default_max_loop_iterations")]
    pub max_loop_iterations: usize,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,

    #[serde(default = "default_true")]
    pub log_state_snapshots: bool,

    #[serde(default = "default_true")]
    pub validate_before_run: bool,
}

impl Default for GraphSettings {
    fn default() -> Self {
        Self {
            max_loop_iterations: default_max_loop_iterations(),
            timeout: None,
            log_state_snapshots: true,
            validate_before_run: true,
        }
    }
}

fn default_max_loop_iterations() -> usize {
    super::DEFAULT_MAX_LOOP_ITERATIONS
}

fn default_true() -> bool {
    true
}

/// A node in the graph. `node_type` is flattened into the YAML, so a node's
/// variant-specific fields live alongside `id`, `description`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Node {
    /// Unique node identifier. May be omitted in YAML; the parser fills it
    /// in from the surrounding `nodes:` map key.
    #[serde(default)]
    pub id: String,

    #[serde(default)]
    pub description: String,

    #[serde(flatten)]
    pub node_type: NodeType,

    /// Static next-node routing. Used by agent/input nodes.
    /// Approval nodes use their `routes` map instead.
    /// Script nodes: this is populated by `_next` in JSON output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// The supported node variants. YAML uses an internal `type` tag in lowercase
/// (e.g. `type: agent`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum NodeType {
    Agent(AgentNode),
    Script(ScriptNode),
    Approval(ApprovalNode),
    Input(InputNode),
    End(EndNode),
}

/// `agent`-type node: spawn an agent with a templated prompt. Agent tools
/// come from the agent's own `config.yaml`; create agent variants for
/// different tool sets rather than overriding here.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentNode {
    pub agent: String,

    pub prompt: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// `script`-type node: run a Python/TypeScript/Bash script that prints a
/// JSON object on stdout. Keys merge into state; the special `_next` key
/// overrides routing and is not merged.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScriptNode {
    pub script: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,

    /// Fallback node to route to if the script fails to run or returns empty
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,

    #[serde(default = "default_script_timeout")]
    pub timeout: u64,
}

fn default_script_timeout() -> u64 {
    30
}

/// `approval`-type node: prompt the user with `options` and route based on
/// their choice via the `routes` map.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApprovalNode {
    pub question: String,

    pub options: Vec<String>,

    pub routes: HashMap<String, String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_timeout: Option<String>,
}

/// `input`-type node: collect free-form text from the user. Routes via the
/// top-level `next` field; the user's text is exposed to templates as
/// `{{input}}` in `state_updates`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InputNode {
    pub question: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_timeout: Option<String>,
}

/// `end`-type node: terminate execution; `output` (templated) is returned
/// as the graph's final result.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EndNode {
    #[serde(default)]
    pub output: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,
}

/// Runtime state for a graph execution: KV store plus visit history.
#[derive(Debug, Clone, Default)]
pub struct GraphState {
    data: HashMap<String, Value>,
    history: Vec<String>,
    loop_counts: HashMap<String, usize>,
}

impl GraphState {
    pub fn new(initial: HashMap<String, Value>) -> Self {
        Self {
            data: initial,
            history: Vec::new(),
            loop_counts: HashMap::new(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.data.get(key)
    }

    pub fn set(&mut self, key: String, value: Value) {
        self.data.insert(key, value);
    }

    /// Merge a JSON object into state. Existing keys are overwritten.
    pub fn merge(&mut self, json_obj: &serde_json::Map<String, Value>) {
        for (key, value) in json_obj {
            self.data.insert(key.clone(), value.clone());
        }
    }

    pub fn data(&self) -> &HashMap<String, Value> {
        &self.data
    }

    /// Record that a node has been entered. Updates both history and loop
    /// counts.
    pub fn visit_node(&mut self, node_id: &str) {
        self.history.push(node_id.to_string());
        *self.loop_counts.entry(node_id.to_string()).or_insert(0) += 1;
    }

    pub fn loop_count(&self, node_id: &str) -> usize {
        self.loop_counts.get(node_id).copied().unwrap_or(0)
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn current_node(&self) -> Option<&str> {
        self.history.last().map(|s| s.as_str())
    }

    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(&self.data)
            .map_err(|e| anyhow::anyhow!("Failed to serialize graph state: {}", e))
    }

    pub fn size_bytes(&self) -> usize {
        self.to_json().map(|s| s.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_a_simple_graph() {
        let yaml = r#"
name: test_graph
description: A test graph
version: "1.0"
start: node1
nodes:
  node1:
    id: node1
    type: agent
    agent: test_agent
    prompt: "Hello {{name}}"
    state_updates:
      result: "{{output}}"
    next: node2
  node2:
    id: node2
    type: end
    output: "{{result}}"
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(graph.name, "test_graph");
        assert_eq!(graph.start, "node1");
        assert_eq!(graph.nodes.len(), 2);
        assert!(graph.has_node("node1"));
        assert!(graph.has_node("node2"));
        assert!(!graph.has_node("missing"));

        let node1 = graph.get_node("node1").unwrap();
        assert!(matches!(node1.node_type, NodeType::Agent(_)));

        let node2 = graph.get_node("node2").unwrap();
        match &node2.node_type {
            NodeType::End(end) => assert_eq!(end.output, "{{result}}"),
            _ => panic!("expected End variant"),
        }
    }

    #[test]
    fn deserializes_every_node_type() {
        let yaml = r#"
name: all_types
start: a
nodes:
  a:
    id: a
    type: agent
    agent: helper
    prompt: hi
    next: s
  s:
    id: s
    type: script
    script: scripts/decide.py
    next: ap
  ap:
    id: ap
    type: approval
    question: ok?
    options: [yes, no]
    routes:
      yes: i
      no: e
  i:
    id: i
    type: input
    question: name?
    state_updates:
      name: "{{input}}"
    next: e
  e:
    id: e
    type: end
    output: done
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        assert!(matches!(
            graph.get_node("a").unwrap().node_type,
            NodeType::Agent(_)
        ));
        assert!(matches!(
            graph.get_node("s").unwrap().node_type,
            NodeType::Script(_)
        ));
        assert!(matches!(
            graph.get_node("ap").unwrap().node_type,
            NodeType::Approval(_)
        ));
        assert!(matches!(
            graph.get_node("i").unwrap().node_type,
            NodeType::Input(_)
        ));
        assert!(matches!(
            graph.get_node("e").unwrap().node_type,
            NodeType::End(_)
        ));
    }

    #[test]
    fn graph_settings_have_sensible_defaults() {
        let yaml = "name: g\nstart: x\nnodes:\n  x:\n    id: x\n    type: end\n    output: ok\n";
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(graph.version, super::super::GRAPH_SCHEMA_VERSION);
        assert_eq!(
            graph.settings.max_loop_iterations,
            super::super::DEFAULT_MAX_LOOP_ITERATIONS
        );
        assert!(graph.settings.log_state_snapshots);
        assert!(graph.settings.validate_before_run);
        assert!(graph.settings.timeout.is_none());
        assert!(graph.initial_state.is_empty());
        assert_eq!(graph.description, "");
    }

    #[test]
    fn input_node_with_all_fields() {
        let yaml = r#"
id: get_key
type: input
question: "Enter your API key:"
default: "{{previous_api_key}}"
validation: "len(input) > 0"
state_updates:
  api_key: "{{input}}"
next: configure
timeout: 300
on_timeout: skip
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();
        let input = match node.node_type {
            NodeType::Input(i) => i,
            _ => panic!("expected Input variant"),
        };
        assert_eq!(input.question, "Enter your API key:");
        assert_eq!(input.default.as_deref(), Some("{{previous_api_key}}"));
        assert_eq!(input.validation.as_deref(), Some("len(input) > 0"));
        assert_eq!(input.timeout, Some(300));
        assert_eq!(input.on_timeout.as_deref(), Some("skip"));
        let updates = input.state_updates.unwrap();
        assert_eq!(
            updates.get("api_key").map(|s| s.as_str()),
            Some("{{input}}")
        );
        assert_eq!(node.next.as_deref(), Some("configure"));
    }

    #[test]
    fn input_node_with_minimal_fields() {
        let yaml = r#"
id: ask
type: input
question: "Describe the feature:"
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();
        let input = match node.node_type {
            NodeType::Input(i) => i,
            _ => panic!("expected Input variant"),
        };
        assert_eq!(input.question, "Describe the feature:");
        assert!(input.default.is_none());
        assert!(input.validation.is_none());
        assert!(input.state_updates.is_none());
        assert!(input.timeout.is_none());
        assert!(input.on_timeout.is_none());
        assert!(node.next.is_none());
    }

    #[test]
    fn script_node_defaults_timeout_to_30() {
        let yaml = r#"
id: s
type: script
script: scripts/decide.py
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();
        let script = match node.node_type {
            NodeType::Script(s) => s,
            _ => panic!("expected Script variant"),
        };
        assert_eq!(script.timeout, 30);
        assert!(script.fallback.is_none());
        assert!(script.state_updates.is_none());
    }

    #[test]
    fn approval_node_carries_routes() {
        let yaml = r#"
id: approve
type: approval
question: "Approve {{filename}}?"
options: [approve, reject, edit]
routes:
  approve: apply
  reject: end_reject
  edit: edit_loop
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();
        let approval = match node.node_type {
            NodeType::Approval(a) => a,
            _ => panic!("expected Approval variant"),
        };
        assert_eq!(approval.options.len(), 3);
        assert_eq!(
            approval.routes.get("approve").map(|s| s.as_str()),
            Some("apply")
        );
        assert_eq!(
            approval.routes.get("reject").map(|s| s.as_str()),
            Some("end_reject")
        );
    }

    #[test]
    fn graph_state_basic_operations() {
        let mut state = GraphState::new(HashMap::new());
        state.set("key1".to_string(), json!("value1"));
        assert_eq!(state.get("key1"), Some(&json!("value1")));

        state.visit_node("node1");
        state.visit_node("node2");
        state.visit_node("node1");

        assert_eq!(state.loop_count("node1"), 2);
        assert_eq!(state.loop_count("node2"), 1);
        assert_eq!(state.loop_count("never"), 0);
        assert_eq!(state.history().len(), 3);
        assert_eq!(state.current_node(), Some("node1"));
    }

    #[test]
    fn graph_state_merge_overwrites_existing_keys() {
        let mut state = GraphState::new(HashMap::new());
        state.set("existing".to_string(), json!("value"));
        state.set("kept".to_string(), json!("untouched"));

        let mut obj = serde_json::Map::new();
        obj.insert("new_key".to_string(), json!("new_value"));
        obj.insert("count".to_string(), json!(42));
        obj.insert("existing".to_string(), json!("replaced"));

        state.merge(&obj);

        assert_eq!(state.get("existing"), Some(&json!("replaced")));
        assert_eq!(state.get("kept"), Some(&json!("untouched")));
        assert_eq!(state.get("new_key"), Some(&json!("new_value")));
        assert_eq!(state.get("count"), Some(&json!(42)));
    }

    #[test]
    fn graph_state_serializes_to_json() {
        let mut initial = HashMap::new();
        initial.insert("k".to_string(), json!("v"));
        let state = GraphState::new(initial);
        let serialized = state.to_json().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed.get("k"), Some(&json!("v")));
        assert!(state.size_bytes() > 0);
    }

    #[test]
    fn graph_state_initial_values_are_seeded() {
        let mut initial = HashMap::new();
        initial.insert("user".to_string(), json!("alice"));
        let state = GraphState::new(initial);
        assert_eq!(state.get("user"), Some(&json!("alice")));
        assert!(state.history().is_empty());
    }
}
