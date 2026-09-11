use super::state::StateManager;
use serde_json::Value;
use std::collections::HashMap;

pub(super) const OUTPUT_KEY: &str = "output";

/// Merge `output_schema` top-level keys (when `has_schema`) and apply `state_updates` with
/// `{{output}}` bound to `output` for the duration of interpolation only. An explicit `output`
/// key in `updates` is honored; otherwise `output` is restored to its prior value, or removed
/// again if it was absent.
pub(super) fn apply(
    state_manager: &mut StateManager,
    output: &Value,
    has_schema: bool,
    updates: Option<&HashMap<String, String>>,
) {
    if has_schema && let Some(obj) = output.as_object() {
        for (k, v) in obj {
            state_manager.state_mut().set(k.clone(), v.clone());
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

        apply(&mut state, &json!("the completion"), false, Some(&u));

        assert_eq!(
            state.state().get(OUTPUT_KEY),
            Some(&json!("the completion"))
        );
    }

    #[test]
    fn explicit_output_is_honored_alongside_other_updates() {
        let u = updates(&[("output", "{{output}}"), ("summary", "got: {{output}}")]);
        let mut state = manager_with(&[]);

        apply(&mut state, &json!("done"), false, Some(&u));

        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!("done")));
        assert_eq!(state.state().get("summary"), Some(&json!("got: done")));
    }

    #[test]
    fn no_updates_is_a_noop() {
        let mut state = manager_with(&[("keep", json!(1))]);

        apply(&mut state, &json!("ignored"), false, None);

        assert_eq!(state.state().get("keep"), Some(&json!(1)));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn output_is_restored_to_previous_value() {
        let u = updates(&[("response", "{{output}}")]);
        let mut state = manager_with(&[("output", json!("preserved"))]);

        apply(&mut state, &json!("new"), false, Some(&u));

        assert_eq!(state.state().get("response"), Some(&json!("new")));
        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!("preserved")));
    }

    #[test]
    fn output_is_removed_when_absent() {
        let u = updates(&[("response", "{{output}}")]);
        let mut state = manager_with(&[]);

        apply(&mut state, &json!("new"), false, Some(&u));

        assert_eq!(state.state().get("response"), Some(&json!("new")));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn explicit_null_prior_output_is_restored_as_null() {
        let u = updates(&[("response", "{{output}}")]);
        let mut state = manager_with(&[("output", json!(null))]);

        apply(&mut state, &json!("new"), false, Some(&u));

        assert_eq!(state.state().get("response"), Some(&json!("new")));
        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!(null)));
    }

    #[test]
    fn schema_merges_top_level_keys_when_has_schema() {
        let mut state = manager_with(&[]);

        apply(&mut state, &json!({"summary": "s", "score": 7}), true, None);

        assert_eq!(state.state().get("summary"), Some(&json!("s")));
        assert_eq!(state.state().get("score"), Some(&json!(7)));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn object_output_is_not_merged_without_schema() {
        let u = updates(&[("ctx", "{{output.context}}")]);
        let mut state = manager_with(&[]);

        apply(
            &mut state,
            &json!({"context": "c", "sources": ["a"]}),
            false,
            Some(&u),
        );

        assert!(state.state().get("context").is_none());
        assert!(state.state().get("sources").is_none());
        assert_eq!(state.state().get("ctx"), Some(&json!("c")));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }
}
