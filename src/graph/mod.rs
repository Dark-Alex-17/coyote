pub mod agent;
pub mod dispatch;
pub mod executor;
pub mod llm;
pub mod logging;
pub mod parser;
pub mod rag;
pub mod script;
pub mod state;
pub mod structured;
pub mod types;
pub mod user_interaction;
pub mod validator;

pub use dispatch::{active_agent_graph_name, run_active_agent_graph};
pub use executor::GraphExecutor;
pub use parser::{GraphParser, agent_has_graph};
pub use types::{Graph, NodeType};

pub const GRAPH_SCHEMA_VERSION: &str = "1.0";

pub const DEFAULT_MAX_LOOP_ITERATIONS: usize = 100;

pub const MAX_STATE_SIZE_BYTES: usize = 32 * 1024;
