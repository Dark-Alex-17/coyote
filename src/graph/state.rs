//! State management and template interpolation for graph execution.

use super::MAX_STATE_SIZE_BYTES;
use super::types::GraphState;
use crate::utils::temp_file;
use anyhow::{Context, Result, bail};
use fancy_regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::fs::{read_to_string, write};
use std::path::PathBuf;
use std::sync::LazyLock;

static TEMPLATE_VAR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{\{([a-zA-Z0-9_\.]+)\}\}").expect("invalid template regex"));

/// Wraps [`GraphState`] with template interpolation, script-output merging,
/// and a large-state temp-file fallback for use with scripts.
///
/// Template syntax: `{{key}}` for top-level keys, `{{a.b.c}}` for nested
/// JSON paths. Use [`StateManager::interpolate`] for strict interpolation
/// (errors on missing keys) or [`StateManager::interpolate_lenient`] for
/// best-effort (missing keys become empty strings).
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

    /// Replace every `{{key}}` in `template` with its state value. Returns
    /// an error if any referenced key is missing.
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

    /// Same as [`Self::interpolate`] but missing keys are silently replaced
    /// with an empty string.
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
        let root = parts.next()?;
        let mut current = self.state.get(root)?;
        for part in parts {
            current = current.get(part)?;
        }
        Some(current)
    }

    /// Serialize the state for transport to a script. State larger than
    /// [`MAX_STATE_SIZE_BYTES`] is written to a unique temp file; the file
    /// is cleaned up when the `StateManager` is dropped.
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

    pub fn to_json_string(&self) -> Result<String> {
        self.state.to_json()
    }

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

    pub fn is_large(&self) -> bool {
        self.size_bytes() > MAX_STATE_SIZE_BYTES
    }

    /// Merge a script's JSON-object stdout into state. The reserved `_next`
    /// key is extracted (used by the executor for routing) and is not stored
    /// in state. Errors if the output is not a JSON object.
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

    /// Remove the temp file backing this state, if any. Called automatically
    /// on drop.
    pub fn cleanup(&mut self) {
        if let Some(path) = self.temp_file.take() {
            let _ = fs::remove_file(path);
        }
    }
}

impl Drop for StateManager {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// How serialized state is delivered to a script: inline JSON for small
/// state, or a file path for state above [`MAX_STATE_SIZE_BYTES`].
#[derive(Debug, Clone)]
pub enum StateRepresentation {
    Inline(String),
    File(PathBuf),
}

impl StateRepresentation {
    pub fn as_string(&self) -> Result<String> {
        match self {
            StateRepresentation::Inline(s) => Ok(s.clone()),
            StateRepresentation::File(path) => read_to_string(path)
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

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        Value::Array(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| String::from("[]"))
        }
        Value::Object(_) => {
            serde_json::to_string(value).unwrap_or_else(|_| String::from("{}"))
        }
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
        let parsed: serde_json::Value = serde_json::from_str(&contents).unwrap();
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
}
