use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct BranchWrites {
    pub node_id: String,
    pub invocation_index: usize,
    pub writes: HashMap<String, Value>,
}
