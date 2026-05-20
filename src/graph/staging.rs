use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Default, Clone)]
pub struct StagingArea {
    writes: HashMap<String, Value>,
}

#[allow(dead_code)]
impl StagingArea {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write(&mut self, key: impl Into<String>, value: Value) {
        self.writes.insert(key.into(), value);
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.writes.get(key)
    }

    pub fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.writes.len()
    }

    pub fn into_writes(self) -> HashMap<String, Value> {
        self.writes
    }
}

/// Published form of one branch's writes for the super-step merge phase.
/// Callers assemble these into a deterministically-ordered `Vec` keyed by
/// `(node_id, invocation_index)` before passing to
/// `StateManager::apply_branch_writes`. `invocation_index` is 0 for normal
/// branches and the input-list position for map sub-branches — so multiple
/// invocations of the same `branch:` node by a `map` are still totally ordered.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct BranchWrites {
    pub node_id: String,
    pub invocation_index: usize,
    pub writes: HashMap<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn new_staging_area_is_empty() {
        let s = StagingArea::new();

        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn write_stores_value_under_key() {
        let mut s = StagingArea::new();

        s.write("key", json!("value"));

        assert_eq!(s.get("key"), Some(&json!("value")));
        assert_eq!(s.len(), 1);
        assert!(!s.is_empty());
    }

    #[test]
    fn write_overwrites_existing_key() {
        let mut s = StagingArea::new();

        s.write("k", json!(1));
        s.write("k", json!(2));

        assert_eq!(s.get("k"), Some(&json!(2)));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn into_writes_consumes_and_yields_map() {
        let mut s = StagingArea::new();
        s.write("a", json!(1));
        s.write("b", json!(2));

        let writes = s.into_writes();

        assert_eq!(writes.len(), 2);
        assert_eq!(writes.get("a"), Some(&json!(1)));
        assert_eq!(writes.get("b"), Some(&json!(2)));
    }
}
