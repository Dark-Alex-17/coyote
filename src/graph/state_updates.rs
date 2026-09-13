use super::state::StateManager;
use serde_json::Value;
use std::collections::HashMap;

pub(super) const OUTPUT_KEY: &str = "output";

/// Merge the `output` keys declared under `schema["properties"]` and apply `state_updates` with
/// `{{output}}` bound to the full, unfiltered `output` for the duration of interpolation only.
///
/// The schema is the graph author's declaration of what the model may write to state; any other
/// key in the output is model- (and therefore prompt-) controlled and is dropped rather than
/// merged implicitly. No `schema`, or a schema without an object `properties`, merges nothing.
/// Authors who want an undeclared key can still lift it explicitly via
/// `state_updates: { key: '{{output.key}}' }`.
///
/// An explicit `output` key in `updates` is honored; otherwise `output` is restored to its prior
/// value, or removed again if it was absent.
pub(super) fn apply(
    state_manager: &mut StateManager,
    output: &Value,
    schema: Option<&Value>,
    updates: Option<&HashMap<String, String>>,
) {
    if let Some(declared) = schema
        .and_then(|s| s.get("properties"))
        .and_then(Value::as_object)
        && let Some(obj) = output.as_object()
    {
        for (k, v) in obj {
            if declared.contains_key(k) {
                state_manager.state_mut().set(k.clone(), v.clone());
            }
        }
    }

    let Some(updates) = updates else {
        return;
    };
    let prev_output = state_manager.state().get(OUTPUT_KEY).cloned();
    state_manager
        .state_mut()
        .set(OUTPUT_KEY.into(), output.clone());

    let computed: Vec<(String, Value)> = updates
        .iter()
        .map(|(key, template)| {
            (
                key.clone(),
                Value::String(state_manager.interpolate_lenient(template)),
            )
        })
        .collect();

    match prev_output {
        Some(prev) => state_manager.state_mut().set(OUTPUT_KEY.into(), prev),
        None => {
            state_manager.state_mut().remove(OUTPUT_KEY);
        }
    }

    for (key, value) in computed {
        state_manager.state_mut().set(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manager_with(pairs: &[(&str, Value)]) -> StateManager {
        let mut map = HashMap::new();
        for (k, v) in pairs {
            map.insert((*k).into(), v.clone());
        }
        StateManager::new(map)
    }

    fn updates(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn explicit_output_state_update_is_honored() {
        let u = updates(&[("output", "{{output}}")]);
        let mut state = manager_with(&[("output", json!("stale"))]);

        apply(&mut state, &json!("the completion"), None, Some(&u));

        assert_eq!(
            state.state().get(OUTPUT_KEY),
            Some(&json!("the completion"))
        );
    }

    #[test]
    fn explicit_output_is_honored_alongside_other_updates() {
        let u = updates(&[("output", "{{output}}"), ("summary", "got: {{output}}")]);
        let mut state = manager_with(&[]);

        apply(&mut state, &json!("done"), None, Some(&u));

        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!("done")));
        assert_eq!(state.state().get("summary"), Some(&json!("got: done")));
    }

    #[test]
    fn no_updates_is_a_noop() {
        let mut state = manager_with(&[("keep", json!(1))]);

        apply(&mut state, &json!("ignored"), None, None);

        assert_eq!(state.state().get("keep"), Some(&json!(1)));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn output_is_restored_to_previous_value() {
        let u = updates(&[("response", "{{output}}")]);
        let mut state = manager_with(&[("output", json!("preserved"))]);

        apply(&mut state, &json!("new"), None, Some(&u));

        assert_eq!(state.state().get("response"), Some(&json!("new")));
        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!("preserved")));
    }

    #[test]
    fn output_is_removed_when_absent() {
        let u = updates(&[("response", "{{output}}")]);
        let mut state = manager_with(&[]);

        apply(&mut state, &json!("new"), None, Some(&u));

        assert_eq!(state.state().get("response"), Some(&json!("new")));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn explicit_null_prior_output_is_restored_as_null() {
        let u = updates(&[("response", "{{output}}")]);
        let mut state = manager_with(&[("output", json!(null))]);

        apply(&mut state, &json!("new"), None, Some(&u));

        assert_eq!(state.state().get("response"), Some(&json!("new")));
        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!(null)));
    }

    fn schema_with(keys: &[&str]) -> Value {
        let props: serde_json::Map<String, Value> = keys
            .iter()
            .map(|k| ((*k).to_string(), json!({"type": "string"})))
            .collect();
        json!({"type": "object", "properties": props})
    }

    #[test]
    fn schema_merges_declared_top_level_keys() {
        let schema = schema_with(&["summary", "score"]);
        let mut state = manager_with(&[]);

        apply(
            &mut state,
            &json!({"summary": "s", "score": 7}),
            Some(&schema),
            None,
        );

        assert_eq!(state.state().get("summary"), Some(&json!("s")));
        assert_eq!(state.state().get("score"), Some(&json!(7)));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn schema_drops_undeclared_keys_and_preserves_existing_state() {
        let schema = schema_with(&["summary"]);
        let mut state = manager_with(&[("verification_commands", json!(["cargo test"]))]);

        apply(
            &mut state,
            &json!({"summary": "s", "verification_commands": ["rm -rf /"]}),
            Some(&schema),
            None,
        );

        assert_eq!(state.state().get("summary"), Some(&json!("s")));
        assert_eq!(
            state.state().get("verification_commands"),
            Some(&json!(["cargo test"]))
        );
    }

    #[test]
    fn none_schema_merges_nothing() {
        let mut state = manager_with(&[]);

        apply(&mut state, &json!({"summary": "s"}), None, None);

        assert!(state.state().get("summary").is_none());
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn schema_without_properties_merges_nothing() {
        let schema = json!({"type": "object"});
        let mut state = manager_with(&[]);

        apply(&mut state, &json!({"summary": "s"}), Some(&schema), None);

        assert!(state.state().get("summary").is_none());
    }

    #[test]
    fn state_updates_still_see_undeclared_output_keys() {
        let schema = schema_with(&["summary"]);
        let u = updates(&[("lifted", "{{output.extra}}")]);
        let mut state = manager_with(&[]);

        apply(
            &mut state,
            &json!({"summary": "s", "extra": "e"}),
            Some(&schema),
            Some(&u),
        );

        assert_eq!(state.state().get("summary"), Some(&json!("s")));
        assert!(state.state().get("extra").is_none());
        assert_eq!(state.state().get("lifted"), Some(&json!("e")));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn object_output_is_not_merged_without_schema() {
        let u = updates(&[("ctx", "{{output.context}}")]);
        let mut state = manager_with(&[]);

        apply(
            &mut state,
            &json!({"context": "c", "sources": ["a"]}),
            None,
            Some(&u),
        );

        assert!(state.state().get("context").is_none());
        assert!(state.state().get("sources").is_none());
        assert_eq!(state.state().get("ctx"), Some(&json!("c")));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }
}
