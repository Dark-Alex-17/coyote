use super::state::StateManager;
use super::structured;
use super::types::AgentNode;
use crate::config::RequestContext;
use crate::function::supervisor::run_agent_for_graph;
use crate::utils::dimmed_text;
use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;

const OUTPUT_KEY: &str = "output";
const DEFAULT_TIMEOUT_SECS: u64 = 300;

pub struct AgentNodeExecutor;

impl AgentNodeExecutor {
    pub async fn execute(
        node: &AgentNode,
        state_manager: &mut StateManager,
        parent_ctx: &mut RequestContext,
    ) -> Result<String> {
        let prompt = state_manager
            .interpolate(&node.prompt)
            .with_context(|| format!("Failed to interpolate prompt for agent '{}'", node.agent))?;

        eprintln!(
            "{}",
            dimmed_text(&format!("▸   spawning agent '{}' with prompt:", node.agent))
        );
        for line in indent_prompt(&prompt, 6) {
            eprintln!("{}", dimmed_text(&line));
        }

        let timeout_dur = Duration::from_secs(node.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS));

        let raw = timeout(
            timeout_dur,
            run_agent_for_graph(parent_ctx, &node.agent, &prompt),
        )
        .await
        .with_context(|| {
            format!(
                "Agent '{}' timed out after {}s",
                node.agent,
                timeout_dur.as_secs()
            )
        })?
        .with_context(|| format!("Agent '{}' failed", node.agent))?;

        let output_value = match &node.output_schema {
            Some(schema) => structured::extract(&raw, schema, parent_ctx)
                .await
                .with_context(|| {
                    format!(
                        "Agent '{}' output failed structured-output extraction",
                        node.agent
                    )
                })?,
            None => Value::String(raw.clone()),
        };

        apply_state_updates(node, state_manager, &output_value);

        Ok(raw)
    }
}

fn indent_prompt(prompt: &str, prefix_spaces: usize) -> Vec<String> {
    const MAX_LINES: usize = 12;
    let pad = " ".repeat(prefix_spaces);
    let mut out: Vec<String> = prompt
        .lines()
        .take(MAX_LINES)
        .map(|line| format!("{pad}{line}"))
        .collect();
    let total = prompt.lines().count();
    if total > MAX_LINES {
        out.push(format!("{pad}... ({} more lines)", total - MAX_LINES));
    }
    out
}

fn apply_state_updates(node: &AgentNode, state_manager: &mut StateManager, output: &Value) {
    if node.output_schema.is_some()
        && let Some(obj) = output.as_object()
    {
        for (k, v) in obj {
            state_manager.state_mut().set(k.clone(), v.clone());
        }
    }

    let Some(updates) = &node.state_updates else {
        return;
    };
    let prev_output = state_manager.state().get(OUTPUT_KEY).cloned();
    state_manager
        .state_mut()
        .set(OUTPUT_KEY.into(), output.clone());

    for (key, template) in updates {
        let value = state_manager.interpolate_lenient(template);
        state_manager
            .state_mut()
            .set(key.clone(), Value::String(value));
    }

    match prev_output {
        Some(v) => state_manager.state_mut().set(OUTPUT_KEY.into(), v),
        None => {
            state_manager
                .state_mut()
                .set(OUTPUT_KEY.into(), Value::Null);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::AgentNode;
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn manager_with(pairs: &[(&str, Value)]) -> StateManager {
        let mut map = HashMap::new();
        for (k, v) in pairs {
            map.insert((*k).into(), v.clone());
        }
        StateManager::new(map)
    }

    fn node_with(prompt: &str, updates: Option<HashMap<String, String>>) -> AgentNode {
        AgentNode {
            agent: "test_agent".into(),
            prompt: prompt.into(),
            state_updates: updates,
            output_schema: None,
            timeout: None,
        }
    }

    #[test]
    fn state_updates_use_output_placeholder() {
        let node = {
            let mut u = HashMap::new();
            u.insert("findings".into(), "{{output}}".into());
            node_with("hi", Some(u))
        };
        let mut state = manager_with(&[]);

        apply_state_updates(&node, &mut state, &json!("agent finished its work"));

        assert_eq!(
            state.state().get("findings"),
            Some(&json!("agent finished its work"))
        );
    }

    #[test]
    fn state_updates_can_reference_existing_keys_and_output() {
        let node = {
            let mut u = HashMap::new();
            u.insert("summary".into(), "{{topic}}: {{output}}".into());
            node_with("hi", Some(u))
        };
        let mut state = manager_with(&[("topic", json!("auth"))]);

        apply_state_updates(&node, &mut state, &json!("JWT vs sessions"));

        assert_eq!(
            state.state().get("summary"),
            Some(&json!("auth: JWT vs sessions"))
        );
    }

    #[test]
    fn output_key_is_cleaned_up_after_state_updates() {
        let node = {
            let mut u = HashMap::new();
            u.insert("findings".into(), "{{output}}".into());
            node_with("hi", Some(u))
        };
        let mut state = manager_with(&[]);

        apply_state_updates(&node, &mut state, &json!("anything"));

        assert_eq!(state.state().get("output"), Some(&Value::Null));
    }

    #[test]
    fn pre_existing_output_value_is_preserved() {
        let node = {
            let mut u = HashMap::new();
            u.insert("greeting".into(), "{{output}}".into());
            node_with("hi", Some(u))
        };
        let mut state = manager_with(&[("output", json!("preserved"))]);

        apply_state_updates(&node, &mut state, &json!("new agent output"));

        assert_eq!(
            state.state().get("greeting"),
            Some(&json!("new agent output"))
        );
        assert_eq!(state.state().get("output"), Some(&json!("preserved")));
    }

    #[test]
    fn no_state_updates_is_a_noop() {
        let node = node_with("hi", None);
        let mut state = manager_with(&[("k", json!("v"))]);

        apply_state_updates(&node, &mut state, &json!("ignored"));

        assert_eq!(state.state().get("k"), Some(&json!("v")));
        assert!(state.state().get("output").is_none());
    }

    #[test]
    fn interpolate_lenient_on_state_updates_handles_missing_keys() {
        let node = {
            let mut u = HashMap::new();
            u.insert("decorated".into(), "[{{missing}}] {{output}}".into());
            node_with("hi", Some(u))
        };
        let mut state = manager_with(&[]);

        apply_state_updates(&node, &mut state, &json!("DATA"));

        assert_eq!(state.state().get("decorated"), Some(&json!("[] DATA")));
    }

    fn node_with_schema(
        prompt: &str,
        updates: Option<HashMap<String, String>>,
        schema: Value,
    ) -> AgentNode {
        let mut n = node_with(prompt, updates);
        n.output_schema = Some(schema);
        n
    }

    #[test]
    fn output_schema_auto_merges_top_level_keys() {
        let node = node_with_schema("hi", None, json!({"type": "object"}));
        let mut state = manager_with(&[]);
        let output = json!({"goal": "do X", "summary": "details"});

        apply_state_updates(&node, &mut state, &output);

        assert_eq!(state.state().get("goal"), Some(&json!("do X")));
        assert_eq!(state.state().get("summary"), Some(&json!("details")));
    }

    #[test]
    fn output_schema_preserves_nested_value_types() {
        let node = node_with_schema("hi", None, json!({"type": "object"}));
        let mut state = manager_with(&[]);
        let output = json!({
            "tags": ["a", "b"],
            "config": { "key": "value" },
            "count": 42
        });

        apply_state_updates(&node, &mut state, &output);

        assert_eq!(state.state().get("tags"), Some(&json!(["a", "b"])));
        assert_eq!(state.state().get("config"), Some(&json!({"key": "value"})));
        assert_eq!(state.state().get("count"), Some(&json!(42)));
    }

    #[test]
    fn output_schema_explicit_state_updates_override_auto_merge() {
        let mut u = HashMap::new();
        u.insert("goal".into(), "renamed-{{output.goal}}".into());
        let node = node_with_schema("hi", Some(u), json!({"type": "object"}));
        let mut state = manager_with(&[]);
        let output = json!({"goal": "do X"});

        apply_state_updates(&node, &mut state, &output);

        assert_eq!(state.state().get("goal"), Some(&json!("renamed-do X")));
    }

    #[test]
    fn no_schema_does_not_auto_merge() {
        let node = node_with("hi", None);
        let mut state = manager_with(&[]);
        let output = json!({"goal": "do X"});

        apply_state_updates(&node, &mut state, &output);

        assert!(state.state().get("goal").is_none());
    }
}
