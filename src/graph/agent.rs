//! Execution of `agent`-type graph nodes.
//!
//! Spawns a child agent via `function::supervisor::run_agent_for_graph`,
//! interpolating the prompt against current graph state. After the agent
//! finishes, applies the node's `state_updates` (templates can reference
//! `{{output}}` for the agent's stdout).

use super::state::StateManager;
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
    /// Interpolate the node's prompt, spawn the agent, wait for it to
    /// finish, then apply `state_updates`. Returns the agent's full output.
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

        let output = timeout(
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

        apply_state_updates(node, state_manager, &output);

        Ok(output)
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

/// Exposes the agent's output as `{{output}}` for template evaluation, then
/// applies every key/template in `state_updates`. The temporary `output`
/// state key is removed at the end so it doesn't leak into subsequent
/// nodes' templates.
fn apply_state_updates(node: &AgentNode, state_manager: &mut StateManager, output: &str) {
    let Some(updates) = &node.state_updates else {
        return;
    };
    let prev_output = state_manager.state().get(OUTPUT_KEY).cloned();
    state_manager
        .state_mut()
        .set(OUTPUT_KEY.into(), Value::String(output.to_string()));

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
        apply_state_updates(&node, &mut state, "agent finished its work");
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
        apply_state_updates(&node, &mut state, "JWT vs sessions");
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
        apply_state_updates(&node, &mut state, "anything");
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
        apply_state_updates(&node, &mut state, "new agent output");
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
        apply_state_updates(&node, &mut state, "ignored");
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
        apply_state_updates(&node, &mut state, "DATA");
        assert_eq!(state.state().get("decorated"), Some(&json!("[] DATA")));
    }
}
