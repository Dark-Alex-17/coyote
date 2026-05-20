use serde_json::Value;
use std::collections::HashMap;

/// Published form of one branch's writes for the super-step merge phase.
/// Callers assemble these into a deterministically-ordered `Vec` keyed by
/// `(node_id, invocation_index)` before passing to
/// `StateManager::apply_branch_writes`. `invocation_index` is 0 for normal
/// branches and the input-list position for map sub-branches — so multiple
/// invocations of the same `branch:` node by a `map` are still totally ordered.
#[derive(Debug, Clone)]
pub struct BranchWrites {
    pub node_id: String,
    pub invocation_index: usize,
    pub writes: HashMap<String, Value>,
}
