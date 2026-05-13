//! Graph-based agent orchestration. Declarative YAML workflows over a shared
//! JSON state, composed of agent/script/approval/input/end nodes.

pub mod agent;
pub mod parser;
pub mod script;
pub mod state;
pub mod types;
pub mod validator;

pub use agent::AgentNodeExecutor;
pub use parser::{GraphParser, agent_has_graph, load_agent_graph};
pub use script::ScriptExecutor;
pub use state::{StateManager, StateRepresentation};
pub use types::{
    AgentNode, ApprovalNode, EndNode, Graph, GraphSettings, GraphState, InputNode, Node, NodeType,
    ScriptNode,
};
pub use validator::{GraphValidator, ValidationError, ValidationResult};

pub const GRAPH_SCHEMA_VERSION: &str = "1.0";

pub const DEFAULT_MAX_LOOP_ITERATIONS: usize = 100;

/// Serialized-state size above which scripts receive state via a temp file
/// instead of an env var.
pub const MAX_STATE_SIZE_BYTES: usize = 32 * 1024;
