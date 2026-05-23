use super::MAX_STATE_SIZE_BYTES;
use super::reducer;
use super::staging::BranchWrites;
use super::types::{GraphState, Reducer};
use crate::utils::temp_file;
use anyhow::{Context, Result, bail};
use fancy_regex::Regex;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::fs::write;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

static TEMPLATE_VAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{([a-zA-Z0-9_\.\[\]]+)\}\}").expect("invalid template regex"));

pub struct StateManager {
    state: GraphState,
    temp_file: Option<PathBuf>,
}

impl StateManager {
    pub fn new(initial: HashMap<String, Value>) -> Self {
        Self {
            state: GraphState::new(initial),
            temp_file: None,
        }
    }

    pub fn state(&self) -> &GraphState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut GraphState {
        &mut self.state
    }

    pub fn interpolate(&self, template: &str) -> Result<String> {
        let mut missing = Vec::new();
        let result = self.interpolate_inner(template, |key| {
            missing.push(key.to_string());
            String::new()
        });
        if !missing.is_empty() {
            bail!(
                "Template interpolation failed: {} not found in state",
                missing
                    .iter()
                    .map(|k| format!("'{k}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(result)
    }

    pub fn interpolate_lenient(&self, template: &str) -> String {
        self.interpolate_inner(template, |_| String::new())
    }

    fn interpolate_inner<F>(&self, template: &str, mut on_missing: F) -> String
    where
        F: FnMut(&str) -> String,
    {
        let mut result = template.to_string();
        for captures in TEMPLATE_VAR_RE.captures_iter(template).flatten() {
            let full = captures.get(0).unwrap().as_str().to_string();
            let key = captures.get(1).unwrap().as_str();
            let replacement = match self.get_nested_value(key) {
                Some(value) => value_to_string(value),
                None => on_missing(key),
            };
            result = result.replace(&full, &replacement);
        }
        result
    }

    fn get_nested_value(&self, key: &str) -> Option<&Value> {
        let mut parts = key.split('.');
        let first = parts.next()?;
        let (root_key, root_indices) = split_indices(first)?;
        let mut current = self.state.get(root_key)?;
        for idx in root_indices {
            current = current.get(idx)?;
        }

        for part in parts {
            let (segment_key, indices) = split_indices(part)?;
            if !segment_key.is_empty() {
                current = current.get(segment_key)?;
            }
            for idx in indices {
                current = current.get(idx)?;
            }
        }

        Some(current)
    }

    pub fn serialize_state(&mut self) -> Result<StateRepresentation> {
        let json = self.state.to_json()?;
        if json.len() > MAX_STATE_SIZE_BYTES {
            let path = temp_file("-graph-state-", ".json");
            write(&path, json.as_bytes()).with_context(|| {
                format!("Failed to write state to temp file at '{}'", path.display())
            })?;
            self.temp_file = Some(path.clone());

            Ok(StateRepresentation::File(path))
        } else {
            Ok(StateRepresentation::Inline(json))
        }
    }

    #[cfg(test)]
    pub fn from_json_string(json: &str) -> Result<Self> {
        let data: HashMap<String, Value> =
            serde_json::from_str(json).context("Failed to parse state JSON")?;
        Ok(Self::new(data))
    }

    pub fn snapshot(&self) -> HashMap<String, Value> {
        self.state.data().clone()
    }

    pub fn size_bytes(&self) -> usize {
        self.state.size_bytes()
    }

    #[cfg(test)]
    pub fn is_large(&self) -> bool {
        self.size_bytes() > MAX_STATE_SIZE_BYTES
    }

    pub fn merge_script_output(&mut self, json_output: &str) -> Result<Option<String>> {
        let value: Value =
            serde_json::from_str(json_output).context("Script output must be valid JSON")?;
        let obj = value
            .as_object()
            .context("Script output must be a JSON object, not array or primitive")?;

        let next_node = obj.get("_next").and_then(|v| v.as_str()).map(String::from);

        let mut merged = serde_json::Map::new();
        for (k, v) in obj {
            if k != "_next" {
                merged.insert(k.clone(), v.clone());
            }
        }
        self.state.merge(&merged);

        Ok(next_node)
    }

    pub fn cleanup(&mut self) {
        if let Some(path) = self.temp_file.take() {
            let _ = fs::remove_file(path);
        }
    }

    pub fn fork_for_branch_state(&self) -> Self {
        Self {
            state: self.state.clone(),
            temp_file: None,
        }
    }

    pub fn diff_against(&self, snapshot: &GraphState) -> HashMap<String, Value> {
        let mut diff = HashMap::new();
        for (k, v) in self.state.data() {
            if snapshot.get(k) != Some(v) {
                diff.insert(k.clone(), v.clone());
            }
        }
        diff
    }

    pub fn read_snapshot(&self) -> Arc<GraphState> {
        Arc::new(self.state.clone())
    }

    pub fn apply_branch_writes(
        &mut self,
        writes: Vec<BranchWrites>,
        reducers: &HashMap<String, Reducer>,
    ) -> Result<()> {
        let mut by_key: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for branch in writes {
            for (key, value) in branch.writes {
                by_key.entry(key).or_default().push(value);
            }
        }

        for (key, values) in by_key {
            match reducers.get(&key).copied() {
                Some(r) => {
                    let mut current = self.state.get(&key).cloned();
                    for value in values {
                        current = Some(reducer::apply(r, current.as_ref(), value)?);
                    }
                    if let Some(final_value) = current {
                        self.state.set(key, final_value);
                    }
                }
                None if values.len() == 1 => {
                    self.state.set(key, values.into_iter().next().unwrap());
                }
                None => {
                    bail!(
                        "Key '{key}' was written by {} parallel branches but has no \
                         reducer declared. Add a reducer for '{key}' to the graph's \
                         `reducers:` block, or rename one writer.",
                        values.len()
                    );
                }
            }
        }

        Ok(())
    }

    pub fn interpolate_raw(&self, template: &str) -> Result<Value> {
        let trimmed = template.trim();
        if let Some(key) = single_reference_key(trimmed) {
            match self.get_nested_value(key) {
                Some(value) => Ok(value.clone()),
                None => bail!("Template interpolation failed: '{key}' not found in state"),
            }
        } else {
            Ok(Value::String(self.interpolate(template)?))
        }
    }
}

impl Drop for StateManager {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[derive(Debug, Clone)]
pub enum StateRepresentation {
    Inline(String),
    File(PathBuf),
}

#[cfg(test)]
impl StateRepresentation {
    pub fn as_string(&self) -> Result<String> {
        match self {
            StateRepresentation::Inline(s) => Ok(s.clone()),
            StateRepresentation::File(path) => fs::read_to_string(path)
                .with_context(|| format!("Failed to read state file at '{}'", path.display())),
        }
    }

    pub fn as_file_path(&self) -> Option<&PathBuf> {
        match self {
            StateRepresentation::File(path) => Some(path),
            StateRepresentation::Inline(_) => None,
        }
    }

    pub fn is_file(&self) -> bool {
        matches!(self, StateRepresentation::File(_))
    }
}

fn split_indices(segment: &str) -> Option<(&str, Vec<usize>)> {
    let bracket_start = segment.find('[');
    let key = match bracket_start {
        Some(i) => &segment[..i],
        None => return Some((segment, Vec::new())),
    };
    let mut indices = Vec::new();
    let mut rest = &segment[bracket_start.unwrap()..];

    while !rest.is_empty() {
        if !rest.starts_with('[') {
            return None;
        }
        let close = rest.find(']')?;
        let idx: usize = rest[1..close].parse().ok()?;
        indices.push(idx);
        rest = &rest[close + 1..];
    }

    Some((key, indices))
}

// Returns the inner key when `template` is exactly a single `{{key}}` reference
// (no surrounding text, no other braces).
fn single_reference_key(template: &str) -> Option<&str> {
    let inner = template.strip_prefix("{{")?.strip_suffix("}}")?;
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    let valid = !inner.is_empty()
        && inner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '[' || c == ']');
    valid.then_some(inner)
}

// Returns the root state keys referenced by any `{{...}}` expressions in the given template string. The "root key" is
// the identifier before the first `.` or `[`; e.g., for `{{user.name}}` the root is `user`, for `{{items[0]}}` the
// root is `items`. Used by the validator to compute the static read-set of a node's templated fields without
// depending on a runtime `StateManager`.
pub(super) fn template_root_keys(template: &str) -> Vec<String> {
    TEMPLATE_VAR_RE
        .captures_iter(template)
        .flatten()
        .filter_map(|c| c.get(1))
        .map(|m| {
            let inner = m.as_str();
            let cut = inner.find(['.', '[']).unwrap_or(inner.len());
            inner[..cut].to_string()
        })
        .collect()
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(_) => serde_json::to_string(value).unwrap_or_else(|_| String::from("[]")),
        Value::Object(_) => serde_json::to_string(value).unwrap_or_else(|_| String::from("{}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manager_with(pairs: &[(&str, Value)]) -> StateManager {
        let mut state = HashMap::new();
        for (k, v) in pairs {
            state.insert((*k).to_string(), v.clone());
        }
        StateManager::new(state)
    }

    #[test]
    fn simple_interpolation_replaces_top_level_keys() {
        let manager = manager_with(&[("name", json!("Alice")), ("age", json!(30))]);

        let result = manager
            .interpolate("Hello {{name}}, you are {{age}} years old")
            .unwrap();

        assert_eq!(result, "Hello Alice, you are 30 years old");
    }

    #[test]
    fn nested_interpolation_walks_objects() {
        let manager =
            manager_with(&[("user", json!({ "name": "Bob", "email": "bob@example.com" }))]);

        let result = manager
            .interpolate("User: {{user.name}} ({{user.email}})")
            .unwrap();

        assert_eq!(result, "User: Bob (bob@example.com)");
    }

    #[test]
    fn deep_nested_interpolation_handles_multiple_levels() {
        let manager = manager_with(&[(
            "config",
            json!({
                "api": { "key": "secret123", "endpoint": "https://api.example.com" }
            }),
        )]);

        let result = manager
            .interpolate("API: {{config.api.endpoint}} with key {{config.api.key}}")
            .unwrap();

        assert_eq!(result, "API: https://api.example.com with key secret123");
    }

    #[test]
    fn strict_interpolation_errors_on_missing_keys() {
        let manager = manager_with(&[]);

        let err = manager
            .interpolate("Hello {{name}}")
            .unwrap_err()
            .to_string();

        assert!(err.contains("not found"), "got: {err}");
        assert!(err.contains("name"), "got: {err}");
    }

    #[test]
    fn strict_interpolation_collects_all_missing_keys() {
        let manager = manager_with(&[]);

        let err = manager
            .interpolate("{{a}} and {{b}}")
            .unwrap_err()
            .to_string();

        assert!(err.contains("'a'") && err.contains("'b'"), "got: {err}");
    }

    #[test]
    fn lenient_interpolation_substitutes_empty_for_missing() {
        let manager = manager_with(&[("name", json!("Alice"))]);

        let result = manager.interpolate_lenient("Hello {{name}}, age: {{age}}");

        assert_eq!(result, "Hello Alice, age: ");
    }

    #[test]
    fn lenient_interpolation_handles_missing_intermediate() {
        let manager = manager_with(&[("user", json!({ "name": "Bob" }))]);

        let result = manager.interpolate_lenient("email: {{user.email}}");

        assert_eq!(result, "email: ");
    }

    #[test]
    fn interpolates_booleans_numbers_and_null() {
        let manager = manager_with(&[
            ("on", json!(true)),
            ("count", json!(42)),
            ("nothing", json!(null)),
        ]);

        assert_eq!(manager.interpolate("{{on}}").unwrap(), "true");
        assert_eq!(manager.interpolate("{{count}}").unwrap(), "42");
        assert_eq!(manager.interpolate("{{nothing}}").unwrap(), "null");
    }

    #[test]
    fn interpolates_arrays_as_json() {
        let manager = manager_with(&[("items", json!(["a", "b", "c"]))]);

        let result = manager.interpolate("{{items}}").unwrap();

        assert_eq!(result, r#"["a","b","c"]"#);
    }

    #[test]
    fn interpolates_objects_as_json() {
        let manager = manager_with(&[("data", json!({ "key": "value" }))]);

        let result = manager.interpolate("{{data}}").unwrap();

        assert_eq!(result, r#"{"key":"value"}"#);
    }

    #[test]
    fn interpolates_array_indices() {
        let manager = manager_with(&[("items", json!(["a", "b", "c"]))]);

        assert_eq!(manager.interpolate("{{items[0]}}").unwrap(), "a");
        assert_eq!(manager.interpolate("{{items[2]}}").unwrap(), "c");
    }

    #[test]
    fn interpolates_array_indices_inside_nested_paths() {
        let manager = manager_with(&[("outer", json!({ "inner": { "arr": ["x", "y", "z"] } }))]);

        let result = manager
            .interpolate("first={{outer.inner.arr[0]}} last={{outer.inner.arr[2]}}")
            .unwrap();

        assert_eq!(result, "first=x last=z");
    }

    #[test]
    fn interpolates_object_fields_after_array_index() {
        let manager = manager_with(&[("users", json!([{ "name": "Alice" }, { "name": "Bob" }]))]);

        let result = manager
            .interpolate("{{users[0].name}} and {{users[1].name}}")
            .unwrap();

        assert_eq!(result, "Alice and Bob");
    }

    #[test]
    fn interpolates_nested_array_indices() {
        let manager = manager_with(&[("matrix", json!([[1, 2], [3, 4]]))]);

        assert_eq!(manager.interpolate("{{matrix[0][1]}}").unwrap(), "2");
        assert_eq!(manager.interpolate("{{matrix[1][0]}}").unwrap(), "3");
    }

    #[test]
    fn out_of_bounds_array_index_is_missing() {
        let manager = manager_with(&[("items", json!(["a", "b"]))]);

        let err = manager.interpolate("{{items[5]}}").unwrap_err().to_string();

        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn replaces_all_occurrences_of_same_key() {
        let manager = manager_with(&[("n", json!("Alice"))]);

        let result = manager.interpolate("{{n}} and {{n}} again").unwrap();

        assert_eq!(result, "Alice and Alice again");
    }

    #[test]
    fn passes_through_templates_with_no_variables() {
        let manager = manager_with(&[]);

        let result = manager.interpolate("No variables here").unwrap();

        assert_eq!(result, "No variables here");
    }

    #[test]
    fn from_json_string_round_trips() {
        let json = r#"{"name": "Alice", "age": 30}"#;
        let manager = StateManager::from_json_string(json).unwrap();

        let result = manager.interpolate("{{name}} is {{age}}").unwrap();

        assert_eq!(result, "Alice is 30");
    }

    #[test]
    fn snapshot_clones_state_data() {
        let manager = manager_with(&[("k1", json!("v1")), ("k2", json!(42))]);

        let snap = manager.snapshot();

        assert_eq!(snap.len(), 2);
        assert_eq!(snap.get("k1"), Some(&json!("v1")));
        assert_eq!(snap.get("k2"), Some(&json!(42)));
    }

    #[test]
    fn small_state_serializes_inline() {
        let mut manager = manager_with(&[("key", json!("value"))]);

        let repr = manager.serialize_state().unwrap();

        assert!(matches!(repr, StateRepresentation::Inline(_)));
        assert!(!manager.is_large());
        assert!(!repr.is_file());
    }

    #[test]
    fn large_state_spills_to_temp_file() {
        let big = "x".repeat(MAX_STATE_SIZE_BYTES + 1024);
        let mut manager = manager_with(&[("blob", json!(big))]);
        assert!(manager.is_large());

        let repr = manager.serialize_state().unwrap();
        assert!(repr.is_file(), "expected file representation");
        let path = repr.as_file_path().unwrap().clone();
        assert!(path.exists());

        let contents = repr.as_string().unwrap();
        let parsed: Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(
            parsed.get("blob").unwrap().as_str().unwrap().len(),
            big.len()
        );

        drop(manager);
        assert!(!path.exists(), "temp file should be cleaned up on drop");
    }

    #[test]
    fn merge_script_output_merges_keys_into_state() {
        let mut manager = manager_with(&[]);
        let output = r#"{"quality_score": 0.85, "issues_found": 3, "status": "complete"}"#;

        let next = manager.merge_script_output(output).unwrap();

        assert_eq!(next, None);
        assert_eq!(manager.state().get("quality_score"), Some(&json!(0.85)));
        assert_eq!(manager.state().get("issues_found"), Some(&json!(3)));
        assert_eq!(manager.state().get("status"), Some(&json!("complete")));
    }

    #[test]
    fn merge_script_output_extracts_next_key_for_routing() {
        let mut manager = manager_with(&[]);
        let output = r#"{"_next": "approval_gate", "quality_score": 0.85}"#;

        let next = manager.merge_script_output(output).unwrap();

        assert_eq!(next.as_deref(), Some("approval_gate"));
        assert_eq!(manager.state().get("quality_score"), Some(&json!(0.85)));
        assert!(
            manager.state().get("_next").is_none(),
            "_next must not leak into state"
        );
    }

    #[test]
    fn merge_script_output_rejects_invalid_json() {
        let mut manager = manager_with(&[]);

        let err = manager
            .merge_script_output("not json")
            .unwrap_err()
            .to_string();

        assert!(err.contains("valid JSON"), "got: {err}");
    }

    #[test]
    fn merge_script_output_rejects_non_object() {
        let mut manager = manager_with(&[]);

        let err = manager
            .merge_script_output("[1, 2, 3]")
            .unwrap_err()
            .to_string();

        assert!(err.contains("must be a JSON object"), "got: {err}");
    }

    #[test]
    fn merge_script_output_overwrites_existing_state_keys() {
        let mut manager = manager_with(&[("status", json!("pending"))]);

        let _ = manager
            .merge_script_output(r#"{"status": "complete"}"#)
            .unwrap();

        assert_eq!(manager.state().get("status"), Some(&json!("complete")));
    }

    fn branch(node_id: &str, idx: usize, writes: &[(&str, Value)]) -> BranchWrites {
        let mut map = HashMap::new();
        for (k, v) in writes {
            map.insert((*k).into(), v.clone());
        }
        BranchWrites {
            node_id: node_id.into(),
            invocation_index: idx,
            writes: map,
        }
    }

    #[test]
    fn read_snapshot_returns_arc_with_current_state() {
        let manager = manager_with(&[("k", json!("v"))]);

        let snap = manager.read_snapshot();

        assert_eq!(snap.get("k"), Some(&json!("v")));
    }

    #[test]
    fn read_snapshot_is_independent_of_later_mutations() {
        let mut manager = manager_with(&[("count", json!(1))]);
        let snap = manager.read_snapshot();

        manager.state_mut().set("count".into(), json!(999));

        assert_eq!(snap.get("count"), Some(&json!(1)));
        assert_eq!(manager.state().get("count"), Some(&json!(999)));
    }

    #[test]
    fn apply_branch_writes_empty_is_noop() {
        let mut manager = manager_with(&[("k", json!("v"))]);
        let reducers = HashMap::new();

        manager.apply_branch_writes(vec![], &reducers).unwrap();

        assert_eq!(manager.state().get("k"), Some(&json!("v")));
    }

    #[test]
    fn apply_branch_writes_single_writer_no_reducer_overwrites() {
        let mut manager = manager_with(&[]);
        let reducers = HashMap::new();

        manager
            .apply_branch_writes(vec![branch("n", 0, &[("k", json!(42))])], &reducers)
            .unwrap();

        assert_eq!(manager.state().get("k"), Some(&json!(42)));
    }

    #[test]
    fn apply_branch_writes_disjoint_keys_all_land() {
        let mut manager = manager_with(&[]);
        let reducers = HashMap::new();

        manager
            .apply_branch_writes(
                vec![
                    branch("a", 0, &[("x", json!(1))]),
                    branch("b", 0, &[("y", json!(2))]),
                    branch("c", 0, &[("z", json!(3))]),
                ],
                &reducers,
            )
            .unwrap();

        assert_eq!(manager.state().get("x"), Some(&json!(1)));
        assert_eq!(manager.state().get("y"), Some(&json!(2)));
        assert_eq!(manager.state().get("z"), Some(&json!(3)));
    }

    #[test]
    fn apply_branch_writes_three_appends_preserve_input_order() {
        let mut manager = manager_with(&[]);
        let mut reducers = HashMap::new();
        reducers.insert("items".into(), Reducer::Append);

        manager
            .apply_branch_writes(
                vec![
                    branch("a", 0, &[("items", json!("first"))]),
                    branch("b", 0, &[("items", json!("second"))]),
                    branch("c", 0, &[("items", json!("third"))]),
                ],
                &reducers,
            )
            .unwrap();

        assert_eq!(
            manager.state().get("items"),
            Some(&json!(["first", "second", "third"]))
        );
    }

    #[test]
    fn apply_branch_writes_collision_without_reducer_bails() {
        let mut manager = manager_with(&[]);
        let reducers = HashMap::new();

        let err = manager
            .apply_branch_writes(
                vec![
                    branch("a", 0, &[("k", json!("first"))]),
                    branch("b", 0, &[("k", json!("second"))]),
                ],
                &reducers,
            )
            .unwrap_err()
            .to_string();

        assert!(err.contains("'k'"), "got: {err}");
        assert!(err.contains("no reducer"), "got: {err}");
        assert!(err.contains("2 parallel branches"), "got: {err}");
    }

    #[test]
    fn apply_branch_writes_sum_reducer_accumulates_with_existing_state() {
        let mut manager = manager_with(&[("cost", json!(10))]);
        let mut reducers = HashMap::new();
        reducers.insert("cost".into(), Reducer::Sum);

        manager
            .apply_branch_writes(
                vec![
                    branch("a", 0, &[("cost", json!(5))]),
                    branch("b", 0, &[("cost", json!(7))]),
                ],
                &reducers,
            )
            .unwrap();

        assert_eq!(manager.state().get("cost"), Some(&json!(22)));
    }

    #[test]
    fn apply_branch_writes_concat_respects_branch_order() {
        let mut manager = manager_with(&[]);
        let mut reducers = HashMap::new();
        reducers.insert("log".into(), Reducer::Concat);

        manager
            .apply_branch_writes(
                vec![
                    branch("a", 0, &[("log", json!("alpha"))]),
                    branch("b", 0, &[("log", json!("bravo"))]),
                ],
                &reducers,
            )
            .unwrap();

        assert_eq!(manager.state().get("log"), Some(&json!("alpha\nbravo")));
    }

    #[test]
    fn apply_branch_writes_mixed_keys_with_and_without_reducers() {
        let mut manager = manager_with(&[]);
        let mut reducers = HashMap::new();
        reducers.insert("results".into(), Reducer::Append);

        manager
            .apply_branch_writes(
                vec![
                    branch(
                        "a",
                        0,
                        &[("results", json!("x")), ("status", json!("ok_a"))],
                    ),
                    branch("b", 0, &[("results", json!("y"))]),
                ],
                &reducers,
            )
            .unwrap();

        assert_eq!(manager.state().get("results"), Some(&json!(["x", "y"])));
        assert_eq!(manager.state().get("status"), Some(&json!("ok_a")));
    }

    #[test]
    fn interpolate_raw_pure_ref_returns_typed_number() {
        let manager = manager_with(&[("count", json!(42))]);

        let result = manager.interpolate_raw("{{count}}").unwrap();

        assert_eq!(result, json!(42));
        assert!(result.is_i64());
    }

    #[test]
    fn interpolate_raw_pure_ref_returns_typed_array() {
        let manager = manager_with(&[("items", json!(["a", "b", "c"]))]);

        let result = manager.interpolate_raw("{{items}}").unwrap();

        assert_eq!(result, json!(["a", "b", "c"]));
        assert!(result.is_array());
    }

    #[test]
    fn interpolate_raw_pure_ref_returns_typed_object() {
        let manager = manager_with(&[("user", json!({ "name": "alice", "age": 30 }))]);

        let result = manager.interpolate_raw("{{user}}").unwrap();

        assert_eq!(result, json!({ "name": "alice", "age": 30 }));
        assert!(result.is_object());
    }

    #[test]
    fn interpolate_raw_pure_ref_returns_typed_bool() {
        let manager = manager_with(&[("flag", json!(true))]);

        let result = manager.interpolate_raw("{{flag}}").unwrap();

        assert_eq!(result, json!(true));
        assert!(result.is_boolean());
    }

    #[test]
    fn interpolate_raw_nested_path_returns_typed_value() {
        let manager = manager_with(&[("user", json!({ "email": "x@y.com" }))]);

        let result = manager.interpolate_raw("{{user.email}}").unwrap();

        assert_eq!(result, json!("x@y.com"));
        assert!(result.is_string());
    }

    #[test]
    fn interpolate_raw_array_index_returns_typed_value() {
        let manager = manager_with(&[("items", json!([10, 20, 30]))]);

        let result = manager.interpolate_raw("{{items[1]}}").unwrap();

        assert_eq!(result, json!(20));
        assert!(result.is_i64());
    }

    #[test]
    fn interpolate_raw_missing_pure_ref_errors() {
        let manager = manager_with(&[]);

        let err = manager
            .interpolate_raw("{{ghost}}")
            .unwrap_err()
            .to_string();

        assert!(err.contains("'ghost'"), "got: {err}");
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn interpolate_raw_mixed_template_falls_back_to_string() {
        let manager = manager_with(&[("name", json!("alice"))]);

        let result = manager.interpolate_raw("Hello {{name}}!").unwrap();

        assert_eq!(result, json!("Hello alice!"));
        assert!(result.is_string());
    }

    #[test]
    fn interpolate_raw_multiple_refs_fall_back_to_string() {
        let manager = manager_with(&[("a", json!(1)), ("b", json!(2))]);

        let result = manager.interpolate_raw("{{a}}{{b}}").unwrap();

        assert_eq!(result, json!("12"));
        assert!(result.is_string());
    }

    #[test]
    fn interpolate_raw_no_refs_is_literal_string() {
        let manager = manager_with(&[]);

        let result = manager.interpolate_raw("literal text").unwrap();

        assert_eq!(result, json!("literal text"));
    }

    #[test]
    fn interpolate_raw_whitespace_padding_still_resolves_pure_ref() {
        let manager = manager_with(&[("k", json!("v"))]);

        let result = manager.interpolate_raw("  {{k}}  ").unwrap();

        assert_eq!(result, json!("v"));
    }

    #[test]
    fn interpolate_raw_inner_spaces_treated_as_mixed() {
        let manager = manager_with(&[("k", json!("v"))]);
        let result = manager.interpolate_raw("{{ k }}").unwrap();
        assert_eq!(result, json!("{{ k }}"));
    }

    #[test]
    fn fork_for_branch_state_copies_data() {
        let parent = manager_with(&[("a", json!(1)), ("b", json!("x"))]);

        let fork = parent.fork_for_branch_state();

        assert_eq!(fork.state().get("a"), Some(&json!(1)));
        assert_eq!(fork.state().get("b"), Some(&json!("x")));
    }

    #[test]
    fn fork_for_branch_state_isolates_writes_from_parent() {
        let parent = manager_with(&[("count", json!(10))]);
        let mut fork = parent.fork_for_branch_state();

        fork.state_mut().set("count".into(), json!(999));

        assert_eq!(fork.state().get("count"), Some(&json!(999)));
        assert_eq!(parent.state().get("count"), Some(&json!(10)));
    }

    #[test]
    fn fork_for_branch_state_does_not_share_temp_file_lifecycle() {
        let parent = manager_with(&[("k", json!("v"))]);
        let fork = parent.fork_for_branch_state();

        assert!(fork.temp_file.is_none());
        // Dropping the fork must not affect the parent's data
        drop(fork);
        assert_eq!(parent.state().get("k"), Some(&json!("v")));
    }

    #[test]
    fn diff_against_returns_empty_when_unchanged() {
        let original = manager_with(&[("a", json!(1)), ("b", json!(2))]);
        let fork = original.fork_for_branch_state();

        let diff = fork.diff_against(original.state());

        assert!(diff.is_empty());
    }

    #[test]
    fn diff_against_reports_newly_written_keys() {
        let original = manager_with(&[]);
        let mut fork = original.fork_for_branch_state();
        fork.state_mut().set("new".into(), json!(42));

        let diff = fork.diff_against(original.state());

        assert_eq!(diff.len(), 1);
        assert_eq!(diff.get("new"), Some(&json!(42)));
    }

    #[test]
    fn diff_against_reports_changed_values_only() {
        let original = manager_with(&[("a", json!(1)), ("b", json!(2)), ("c", json!(3))]);
        let mut fork = original.fork_for_branch_state();
        fork.state_mut().set("b".into(), json!(99));

        let diff = fork.diff_against(original.state());

        assert_eq!(diff.len(), 1);
        assert_eq!(diff.get("b"), Some(&json!(99)));
        assert!(!diff.contains_key("a"));
        assert!(!diff.contains_key("c"));
    }

    #[test]
    fn diff_against_does_not_report_reverted_writes() {
        // Branch writes then writes back to the original value; net change = 0.
        let original = manager_with(&[("x", json!("initial"))]);
        let mut fork = original.fork_for_branch_state();
        fork.state_mut().set("x".into(), json!("modified"));
        fork.state_mut().set("x".into(), json!("initial"));

        let diff = fork.diff_against(original.state());

        assert!(diff.is_empty(), "reverted write should not appear in diff");
    }
}
