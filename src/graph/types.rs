use anyhow::{Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::slice;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Graph {
    pub name: String,

    #[serde(default)]
    pub description: String,

    #[serde(default = "default_schema_version")]
    pub version: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,

    #[serde(default)]
    pub global_tools: Vec<String>,

    #[serde(default)]
    pub mcp_servers: Vec<String>,

    #[serde(default)]
    pub conversation_starters: Vec<String>,

    #[serde(default)]
    pub settings: GraphSettings,

    #[serde(default)]
    pub initial_state: HashMap<String, Value>,

    #[serde(default)]
    pub reducers: HashMap<String, Reducer>,

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

    pub fn has_agent_node(&self) -> bool {
        self.nodes
            .values()
            .any(|n| matches!(n.node_type, NodeType::Agent(_)))
    }
}

fn default_schema_version() -> String {
    super::GRAPH_SCHEMA_VERSION.to_string()
}

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

    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
}

impl Default for GraphSettings {
    fn default() -> Self {
        Self {
            max_loop_iterations: default_max_loop_iterations(),
            timeout: None,
            log_state_snapshots: true,
            validate_before_run: true,
            max_concurrency: default_max_concurrency(),
        }
    }
}

fn default_max_loop_iterations() -> usize {
    super::DEFAULT_MAX_LOOP_ITERATIONS
}

fn default_true() -> bool {
    true
}

fn default_max_concurrency() -> usize {
    8
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Node {
    #[serde(default)]
    pub id: String,

    #[serde(default)]
    pub description: String,

    #[serde(flatten)]
    pub node_type: NodeType,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<NextTargets>,
}

impl Node {
    /// Returns the single next target as a string slice, or `None` if no next is
    /// declared or if a multi-target fan-out is declared. Use this for read-only
    /// inspection (e.g. tests). For execution paths that require single-target
    /// semantics, use `next_single()` — it errors explicitly when a fan-out is
    /// declared so the caller can surface a clear failure instead of skipping it.
    #[allow(dead_code)]
    pub fn next_target(&self) -> Option<&str> {
        match &self.next {
            None => None,
            Some(NextTargets::One(s)) => Some(s),
            Some(NextTargets::Many(v)) if v.len() == 1 => Some(&v[0]),
            Some(NextTargets::Many(_)) => None,
        }
    }

    /// Returns the single next target as a string slice, or an explicit error if
    /// the node declares a multi-target fan-out (which is not yet supported
    /// pre-Phase-D). Returns `Ok(None)` when no next is declared at all.
    pub fn next_single(&self) -> Result<Option<&str>> {
        match &self.next {
            None => Ok(None),
            Some(targets) => Ok(Some(targets.single()?.as_str())),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum NextTargets {
    One(String),
    Many(Vec<String>),
}

impl NextTargets {
    /// View as a slice of node ids. `One(s)` returns a single-element slice.
    pub fn as_slice(&self) -> &[String] {
        match self {
            NextTargets::One(s) => slice::from_ref(s),
            NextTargets::Many(v) => v.as_slice(),
        }
    }

    /// True if this declares more than one parallel target (i.e., a real fan-out).
    #[allow(dead_code)]
    pub fn is_fan_out(&self) -> bool {
        matches!(self, NextTargets::Many(v) if v.len() > 1)
    }

    /// Returns the single target if exactly one is declared, else errors with a
    /// clear "not yet supported" message. Used by the v1 executor until parallel
    /// branch execution lands in Phase D.
    pub fn single(&self) -> Result<&String> {
        match self {
            NextTargets::One(s) => Ok(s),
            NextTargets::Many(v) if v.len() == 1 => Ok(&v[0]),
            NextTargets::Many(_) => bail!(
                "Parallel fan-out (`next: [a, b, ...]`) is declared, but parallel \
                 branch execution is not yet implemented in this build."
            ),
        }
    }
}

impl From<String> for NextTargets {
    fn from(s: String) -> Self {
        NextTargets::One(s)
    }
}

impl From<&str> for NextTargets {
    fn from(s: &str) -> Self {
        NextTargets::One(s.to_string())
    }
}

impl From<Vec<String>> for NextTargets {
    fn from(v: Vec<String>) -> Self {
        NextTargets::Many(v)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum NodeType {
    Agent(AgentNode),
    Script(ScriptNode),
    Approval(ApprovalNode),
    Input(InputNode),
    Llm(LlmNode),
    Rag(RagNode),
    End(EndNode),
    Map(MapNode),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentNode {
    pub agent: String,

    pub prompt: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScriptNode {
    pub script: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,

    #[serde(default = "default_script_timeout")]
    pub timeout: u64,
}

fn default_script_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApprovalNode {
    pub question: String,

    pub options: Vec<String>,

    pub routes: HashMap<String, String>,

    pub on_other: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InputNode {
    pub question: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LlmNode {
    pub prompt: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,

    #[serde(default = "default_llm_max_attempts")]
    pub max_attempts: u32,

    #[serde(default = "default_llm_max_iterations")]
    pub max_iterations: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

fn default_llm_max_attempts() -> u32 {
    1
}

fn default_llm_max_iterations() -> u32 {
    10
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RagNode {
    pub documents: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_size: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_overlap: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reranker_model: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EndNode {
    #[serde(default)]
    pub output: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_updates: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MapNode {
    /// Template expression that must resolve (via `interpolate_raw`, added in
    /// Phase B) to a JSON array. Each item in the array is one branch invocation.
    pub over: String,

    /// The name to bind each item under, accessible as `{{<as_name>}}` inside
    /// the branch node's templates. YAML field is `as:`.
    #[serde(rename = "as")]
    pub as_name: String,

    /// Node id to invoke once per item in the resolved list.
    pub branch: String,

    /// State key that the branch node writes; the map collects this key's value
    /// across invocations. Defaults to "output".
    #[serde(default = "default_map_output_key")]
    pub output_key: String,

    /// State key to receive the array of per-branch outputs, in input-list order.
    pub collect_into: String,

    /// Optional cap on simultaneously-running sub-branches. Falls back to
    /// `settings.max_concurrency` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
}

fn default_map_output_key() -> String {
    "output".to_string()
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Reducer {
    Append,
    Extend,
    Concat,
    Sum,
    Max,
    Min,
    Merge,
    Overwrite,
}

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

    pub fn merge(&mut self, json_obj: &serde_json::Map<String, Value>) {
        for (key, value) in json_obj {
            self.data.insert(key.clone(), value.clone());
        }
    }

    pub fn data(&self) -> &HashMap<String, Value> {
        &self.data
    }

    pub fn visit_node(&mut self, node_id: &str) {
        self.history.push(node_id.to_string());
        *self.loop_counts.entry(node_id.to_string()).or_insert(0) += 1;
    }

    pub fn loop_count(&self, node_id: &str) -> usize {
        self.loop_counts.get(node_id).copied().unwrap_or(0)
    }

    #[cfg(test)]
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
    on_other: e
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
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();
        let next_target = node.next_target().map(str::to_string);
        let input = match node.node_type {
            NodeType::Input(i) => i,
            _ => panic!("expected Input variant"),
        };
        assert_eq!(input.question, "Enter your API key:");
        assert_eq!(input.default.as_deref(), Some("{{previous_api_key}}"));
        assert_eq!(input.validation.as_deref(), Some("len(input) > 0"));
        let updates = input.state_updates.unwrap();
        assert_eq!(
            updates.get("api_key").map(|s| s.as_str()),
            Some("{{input}}")
        );
        assert_eq!(next_target.as_deref(), Some("configure"));
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
on_other: edit_loop
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
        assert_eq!(state.history.len(), 3);
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
        assert!(state.history.is_empty());
    }

    #[test]
    fn llm_node_with_all_fields() {
        let yaml = r#"
id: classify
type: llm
instructions: "You are a classifier."
prompt: "Classify: {{input_text}}"
tools:
  - read_query
  - "mcp:pubmed-search"
model: anthropic:claude-3-5-haiku-20241022
temperature: 0.0
top_p: 0.5
fallback: skip_classify
max_attempts: 3
max_iterations: 5
state_updates:
  category: "{{output}}"
timeout: 30
next: review
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();
        let next_target = node.next_target().map(str::to_string);
        let llm = match node.node_type {
            NodeType::Llm(l) => l,
            _ => panic!("expected Llm variant"),
        };
        assert_eq!(llm.instructions.as_deref(), Some("You are a classifier."));
        assert_eq!(llm.prompt, "Classify: {{input_text}}");
        let tools = llm.tools.unwrap();
        assert_eq!(tools, vec!["read_query", "mcp:pubmed-search"]);
        assert_eq!(
            llm.model.as_deref(),
            Some("anthropic:claude-3-5-haiku-20241022")
        );
        assert_eq!(llm.temperature, Some(0.0));
        assert_eq!(llm.top_p, Some(0.5));
        assert_eq!(llm.fallback.as_deref(), Some("skip_classify"));
        assert_eq!(llm.max_attempts, 3);
        assert_eq!(llm.max_iterations, 5);
        assert_eq!(llm.timeout, Some(30));
        assert!(llm.state_updates.is_some());
        assert_eq!(next_target.as_deref(), Some("review"));
    }

    #[test]
    fn llm_node_minimal_fields_use_defaults() {
        let yaml = r#"
id: pure_text
type: llm
instructions: "System."
prompt: "User."
next: done
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();
        let llm = match node.node_type {
            NodeType::Llm(l) => l,
            _ => panic!("expected Llm variant"),
        };
        assert_eq!(llm.instructions.as_deref(), Some("System."));
        assert_eq!(llm.prompt, "User.");
        assert!(llm.tools.is_none());
        assert!(llm.model.is_none());
        assert!(llm.fallback.is_none());
        assert_eq!(llm.max_attempts, 1);
        assert_eq!(llm.max_iterations, 10);
    }

    #[test]
    fn llm_node_with_just_prompt_succeeds() {
        let yaml = r#"
id: pure
type: llm
prompt: "User-only — no system prompt."
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();
        let llm = match node.node_type {
            NodeType::Llm(l) => l,
            _ => panic!("expected Llm variant"),
        };
        assert!(llm.instructions.is_none());
        assert_eq!(llm.prompt, "User-only — no system prompt.");
    }

    #[test]
    fn llm_node_missing_prompt_fails() {
        let yaml = r#"
id: bad
type: llm
instructions: "System only — no user prompt."
"#;
        let result: std::result::Result<Node, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn graph_parses_agent_level_top_level_fields() {
        let yaml = r#"
name: single_file
start: e
model: anthropic:claude-sonnet-4-6
temperature: 0.2
top_p: 0.9
global_tools:
  - web_search_loki.sh
mcp_servers:
  - pubmed-search
conversation_starters:
  - "Look up 2160-0"
nodes:
  e:
    id: e
    type: end
    output: done
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(graph.model.as_deref(), Some("anthropic:claude-sonnet-4-6"));
        assert_eq!(graph.temperature, Some(0.2));
        assert_eq!(graph.top_p, Some(0.9));
        assert_eq!(graph.global_tools, vec!["web_search_loki.sh"]);
        assert_eq!(graph.mcp_servers, vec!["pubmed-search"]);
        assert_eq!(graph.conversation_starters, vec!["Look up 2160-0"]);
    }

    #[test]
    fn graph_agent_level_fields_default_when_absent() {
        let yaml = "name: g\nstart: x\nnodes:\n  x:\n    id: x\n    type: end\n    output: ok\n";
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        assert!(graph.model.is_none());
        assert!(graph.temperature.is_none());
        assert!(graph.top_p.is_none());
        assert!(graph.global_tools.is_empty());
        assert!(graph.mcp_servers.is_empty());
        assert!(graph.conversation_starters.is_empty());
    }

    #[test]
    fn node_ids_lists_nodes_in_order() {
        let yaml = r#"
name: g
start: first
nodes:
  first:
    id: first
    type: agent
    agent: helper
    prompt: hi
    next: last
  last:
    id: last
    type: end
    output: done
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(graph.node_ids(), vec!["first", "last"]);
    }

    #[test]
    fn has_agent_node_detects_agent_nodes() {
        let with_agent = r#"
name: g
start: a
nodes:
  a:
    id: a
    type: agent
    agent: helper
    prompt: hi
    next: e
  e:
    id: e
    type: end
    output: done
"#;
        let graph: Graph = serde_yaml::from_str(with_agent).unwrap();
        assert!(graph.has_agent_node());
    }

    #[test]
    fn has_agent_node_false_without_agent_nodes() {
        let yaml = "name: g\nstart: x\nnodes:\n  x:\n    id: x\n    type: end\n    output: ok\n";
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        assert!(!graph.has_agent_node());
    }

    #[test]
    fn parses_static_fan_out_as_many_next_targets() {
        let yaml = r#"
id: triage
type: llm
prompt: Classify
next: [retrieve_local, retrieve_web, retrieve_docs]
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();

        let targets = node.next.as_ref().expect("next should be present");

        assert!(targets.is_fan_out());
        assert_eq!(
            targets.as_slice(),
            &[
                "retrieve_local".to_string(),
                "retrieve_web".to_string(),
                "retrieve_docs".to_string()
            ]
        );
    }

    #[test]
    fn parses_single_target_next_as_one_variant() {
        let yaml = r#"
id: triage
type: llm
prompt: Classify
next: retrieve
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();

        let targets = node.next.as_ref().expect("next should be present");

        assert!(!targets.is_fan_out());
        assert_eq!(node.next_target(), Some("retrieve"));
    }

    #[test]
    fn next_single_errors_on_real_fan_out_with_clear_message() {
        let yaml = r#"
id: triage
type: llm
prompt: Classify
next: [a, b]
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();

        let err = node.next_single().unwrap_err().to_string();

        assert!(err.contains("Parallel fan-out"), "got: {err}");
        assert!(err.contains("not yet implemented"), "got: {err}");
    }

    #[test]
    fn next_single_accepts_many_containing_exactly_one_target() {
        let yaml = r#"
id: triage
type: llm
prompt: Classify
next: [retrieve]
"#;

        let node: Node = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(node.next_single().unwrap(), Some("retrieve"));
        assert_eq!(node.next_target(), Some("retrieve"));
    }

    #[test]
    fn next_targets_round_trips_through_yaml_for_both_variants() {
        let one: NextTargets = serde_yaml::from_str(r#""foo""#).unwrap();
        let reparsed: NextTargets =
            serde_yaml::from_str(&serde_yaml::to_string(&one).unwrap()).unwrap();
        assert_eq!(reparsed.as_slice(), &["foo".to_string()]);

        let many: NextTargets = serde_yaml::from_str("[a, b, c]").unwrap();
        let reparsed: NextTargets =
            serde_yaml::from_str(&serde_yaml::to_string(&many).unwrap()).unwrap();
        assert_eq!(
            reparsed.as_slice(),
            &["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn parses_reducers_block_with_all_builtins() {
        let yaml = r#"
name: g
start: e
reducers:
  sources: append
  findings: extend
  context: concat
  cost_usd: sum
  high_score: max
  low_score: min
  config: merge
  forced: overwrite
nodes:
  e:
    type: end
    output: ok
"#;

        let graph: Graph = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(graph.reducers.len(), 8);
        assert_eq!(graph.reducers.get("sources"), Some(&Reducer::Append));
        assert_eq!(graph.reducers.get("findings"), Some(&Reducer::Extend));
        assert_eq!(graph.reducers.get("context"), Some(&Reducer::Concat));
        assert_eq!(graph.reducers.get("cost_usd"), Some(&Reducer::Sum));
        assert_eq!(graph.reducers.get("high_score"), Some(&Reducer::Max));
        assert_eq!(graph.reducers.get("low_score"), Some(&Reducer::Min));
        assert_eq!(graph.reducers.get("config"), Some(&Reducer::Merge));
        assert_eq!(graph.reducers.get("forced"), Some(&Reducer::Overwrite));
    }

    #[test]
    fn reducers_default_to_empty_when_block_absent() {
        let yaml = "name: g\nstart: x\nnodes:\n  x:\n    type: end\n";

        let graph: Graph = serde_yaml::from_str(yaml).unwrap();

        assert!(graph.reducers.is_empty());
    }

    #[test]
    fn max_concurrency_defaults_to_eight() {
        let yaml = "name: g\nstart: x\nnodes:\n  x:\n    type: end\n";

        let graph: Graph = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(graph.settings.max_concurrency, 8);
    }

    #[test]
    fn max_concurrency_can_be_overridden() {
        let yaml = r#"
name: g
start: x
settings:
  max_concurrency: 16
nodes:
  x:
    type: end
"#;

        let graph: Graph = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(graph.settings.max_concurrency, 16);
    }

    #[test]
    fn parses_map_node_with_all_fields() {
        let yaml = r#"
id: fan_out
type: map
over: "{{subjects}}"
as: subject
branch: research_subject
output_key: research_result
collect_into: research_results
max_concurrency: 5
next: rank
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();

        let map = match node.node_type {
            NodeType::Map(m) => m,
            _ => panic!("expected Map variant"),
        };

        assert_eq!(map.over, "{{subjects}}");
        assert_eq!(map.as_name, "subject");
        assert_eq!(map.branch, "research_subject");
        assert_eq!(map.output_key, "research_result");
        assert_eq!(map.collect_into, "research_results");
        assert_eq!(map.max_concurrency, Some(5));
    }

    #[test]
    fn map_node_uses_default_output_key_and_no_concurrency_cap() {
        let yaml = r#"
id: fan_out
type: map
over: "{{items}}"
as: item
branch: process
collect_into: results
"#;
        let node: Node = serde_yaml::from_str(yaml).unwrap();

        let map = match node.node_type {
            NodeType::Map(m) => m,
            _ => panic!("expected Map variant"),
        };

        assert_eq!(map.output_key, "output");
        assert!(map.max_concurrency.is_none());
    }

    #[test]
    fn full_graph_with_all_new_phase_a_fields_parses() {
        let yaml = r#"
name: deep_research
start: triage
settings:
  max_concurrency: 4
reducers:
  sources: append
  cost_usd: sum
nodes:
  triage:
    type: llm
    prompt: Classify
    next: [retrieve_local, retrieve_web]
  retrieve_local:
    type: rag
    documents: ["./docs"]
    next: synthesize
  retrieve_web:
    type: llm
    prompt: Search web
    next: synthesize
  synthesize:
    type: end
    output: done
"#;
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(graph.settings.max_concurrency, 4);
        assert_eq!(graph.reducers.len(), 2);
        let triage = graph.get_node("triage").unwrap();
        assert!(triage.next.as_ref().unwrap().is_fan_out());
        assert_eq!(triage.next.as_ref().unwrap().as_slice().len(), 2);
    }
}
