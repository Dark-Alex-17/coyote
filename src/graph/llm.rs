//! Execution of `llm`-type graph nodes — one-shot LLM calls with a
//! bounded tool-call loop, an opt-in tool whitelist, and per-node
//! overrides for model/temperature/top_p.
//!
//! See `docs/implementation/graph-agents/10.5-llm-nodes.md` for the
//! design. The current implementation provides the routing and
//! state-update plumbing; the actual call_chat_completions loop lives
//! in `run_llm_once` and is the next implementation step. Calling
//! `LlmNodeExecutor::execute` today produces a controlled error so the
//! tolerant-fail routing in the executor still flows.

use super::state::StateManager;
use super::types::LlmNode;
use crate::config::RequestContext;
use crate::utils::dimmed_text;
use anyhow::{Context, Result, bail};
use serde_json::Value;

const OUTPUT_KEY: &str = "output";

pub struct LlmNodeExecutor;

impl LlmNodeExecutor {
    /// Interpolate the node's templates, run the LLM call, then return
    /// the model's final response. State updates are applied by the
    /// graph executor (which knows whether to use the success path or
    /// the failure path).
    pub async fn execute(
        node: &LlmNode,
        state_manager: &mut StateManager,
        _parent_ctx: &mut RequestContext,
    ) -> Result<String> {
        let _instructions = state_manager
            .interpolate(&node.instructions)
            .context("Failed to interpolate llm node instructions")?;
        let _prompt = state_manager
            .interpolate(&node.prompt)
            .context("Failed to interpolate llm node prompt")?;

        eprintln!(
            "{}",
            dimmed_text(&format!(
                "▸   llm call: model={} tools={}",
                node.model.as_deref().unwrap_or("<active>"),
                describe_tools_filter(node.tools.as_deref())
            ))
        );

        bail!(
            "llm node execution body not yet implemented — see \
             docs/implementation/graph-agents/10.5-llm-nodes.md \
             (steps 3 & 5 of the implementation order)"
        );
    }
}

/// Expose the LLM call's final output as `{{output}}` for the duration
/// of `state_updates` evaluation, then restore the prior value (or set
/// it to `Null` if there wasn't one). Same pattern as
/// `AgentNodeExecutor`'s `{{output}}` scoping.
pub fn apply_state_updates_with_output(
    node: &LlmNode,
    state_manager: &mut StateManager,
    output: &str,
) {
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

fn describe_tools_filter(tools: Option<&[String]>) -> String {
    match tools {
        None => "<none>".into(),
        Some(t) if t.is_empty() => "<none>".into(),
        Some(t) => t.join(","),
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::*;
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

    fn node_with(updates: Option<HashMap<String, String>>) -> LlmNode {
        LlmNode {
            instructions: "sys".into(),
            prompt: "user".into(),
            tools: None,
            model: None,
            temperature: None,
            top_p: None,
            fallback: None,
            max_attempts: 1,
            max_iterations: 10,
            state_updates: updates,
            timeout: None,
        }
    }

    #[test]
    fn state_updates_expose_output_during_evaluation() {
        let mut u = HashMap::new();
        u.insert("response".into(), "{{output}}".into());
        let node = node_with(Some(u));
        let mut state = manager_with(&[]);
        apply_state_updates_with_output(&node, &mut state, "the answer");
        assert_eq!(state.state().get("response"), Some(&json!("the answer")));
    }

    #[test]
    fn state_updates_can_mix_existing_keys_with_output() {
        let mut u = HashMap::new();
        u.insert("summary".into(), "{{topic}}: {{output}}".into());
        let node = node_with(Some(u));
        let mut state = manager_with(&[("topic", json!("LOINC"))]);
        apply_state_updates_with_output(&node, &mut state, "abc");
        assert_eq!(state.state().get("summary"), Some(&json!("LOINC: abc")));
    }

    #[test]
    fn output_key_is_cleared_after_state_updates() {
        let mut u = HashMap::new();
        u.insert("k".into(), "{{output}}".into());
        let node = node_with(Some(u));
        let mut state = manager_with(&[]);
        apply_state_updates_with_output(&node, &mut state, "anything");
        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!(null)));
    }

    #[test]
    fn pre_existing_output_value_is_restored() {
        let mut u = HashMap::new();
        u.insert("greeting".into(), "{{output}}".into());
        let node = node_with(Some(u));
        let mut state = manager_with(&[("output", json!("preserved"))]);
        apply_state_updates_with_output(&node, &mut state, "new");
        assert_eq!(state.state().get("greeting"), Some(&json!("new")));
        assert_eq!(state.state().get(OUTPUT_KEY), Some(&json!("preserved")));
    }

    #[test]
    fn no_state_updates_is_a_noop() {
        let node = node_with(None);
        let mut state = manager_with(&[("k", json!("v"))]);
        apply_state_updates_with_output(&node, &mut state, "x");
        assert_eq!(state.state().get("k"), Some(&json!("v")));
        assert!(state.state().get(OUTPUT_KEY).is_none());
    }

    #[test]
    fn describe_tools_filter_renders_each_case() {
        assert_eq!(describe_tools_filter(None), "<none>");
        assert_eq!(describe_tools_filter(Some(&[])), "<none>");
        let tools = vec!["a".to_string(), "b".to_string()];
        assert_eq!(describe_tools_filter(Some(&tools)), "a,b");
    }
}
