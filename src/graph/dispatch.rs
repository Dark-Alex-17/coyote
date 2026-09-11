use super::{GraphExecutor, GraphParser, agent_has_graph};
use crate::config::RequestContext;
use crate::config::paths;
use crate::utils::AbortSignal;
use anyhow::{Context, Result, anyhow};
use log::info;
use serde_json::Value;
use std::collections::HashMap;

pub fn active_agent_graph_name(ctx: &RequestContext) -> Option<String> {
    let name = ctx.agent.as_ref()?.name().to_string();
    agent_has_graph(&name).then_some(name)
}

pub async fn run_active_agent_graph(
    ctx: &mut RequestContext,
    prompt: &str,
    abort_signal: AbortSignal,
) -> Result<String> {
    run_active_agent_graph_with_inputs(ctx, prompt, abort_signal, None).await
}

/// Runs the active agent's graph with `inputs` overlaid on its
/// `initial_state` before the executor starts. `None` seeds exactly what
/// `run_active_agent_graph` does: the prompt under `initial_prompt`.
pub async fn run_active_agent_graph_with_inputs(
    ctx: &mut RequestContext,
    prompt: &str,
    abort_signal: AbortSignal,
    inputs: Option<HashMap<String, Value>>,
) -> Result<String> {
    let agent_name =
        active_agent_graph_name(ctx).ok_or_else(|| anyhow!("Active agent has no graph.yaml"))?;

    info!("Agent '{agent_name}' has graph.yaml; routing to graph executor");

    let agent_dir = paths::agent_data_dir(&agent_name);
    let graph_path = paths::agent_graph_file(&agent_name);

    let parser = GraphParser::new(&agent_dir);
    let mut graph = parser
        .load_from_file(&graph_path)
        .with_context(|| format!("Failed to load graph.yaml for agent '{agent_name}'"))?;

    seed_initial_state(&mut graph.initial_state, prompt, inputs);

    let executor = GraphExecutor::new(graph, agent_dir);
    let output = executor
        .execute(ctx, abort_signal)
        .await
        .with_context(|| format!("Graph execution failed for agent '{agent_name}'"))?;

    if let Some(supervisor) = ctx.supervisor.clone() {
        supervisor.read().cancel_all();
    }

    Ok(output)
}

/// Seeds a child graph's `initial_state`, later writers winning: the graph's
/// own YAML defaults, then the caller's `inputs`, then `initial_prompt`,
/// which the dispatcher always owns.
pub fn seed_initial_state(
    initial_state: &mut HashMap<String, Value>,
    prompt: &str,
    inputs: Option<HashMap<String, Value>>,
) {
    if let Some(inputs) = inputs {
        initial_state.extend(inputs);
    }
    initial_state.insert("initial_prompt".into(), Value::String(prompt.to_string()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn yaml_defaults() -> HashMap<String, Value> {
        HashMap::from([
            ("width".to_string(), json!(4)),
            ("mode".to_string(), json!("fast")),
        ])
    }

    #[test]
    fn seed_initial_state_without_inputs_only_adds_initial_prompt() {
        let mut state = yaml_defaults();

        seed_initial_state(&mut state, "hello", None);

        let mut expected = yaml_defaults();
        expected.insert("initial_prompt".into(), json!("hello"));
        assert_eq!(state, expected);
    }

    #[test]
    fn seed_initial_state_without_inputs_on_empty_state_inserts_exactly_one_key() {
        let mut state = HashMap::new();

        seed_initial_state(&mut state, "hello", None);

        assert_eq!(
            state,
            HashMap::from([("initial_prompt".to_string(), json!("hello"))])
        );
    }

    #[test]
    fn seed_initial_state_inputs_override_yaml_defaults_and_keep_the_rest() {
        let mut state = yaml_defaults();
        let inputs = HashMap::from([
            ("width".to_string(), json!(2)),
            ("extra".to_string(), json!(["a", "b"])),
        ]);

        seed_initial_state(&mut state, "hello", Some(inputs));

        assert_eq!(state.get("width"), Some(&json!(2)));
        assert_eq!(state.get("mode"), Some(&json!("fast")));
        assert_eq!(state.get("extra"), Some(&json!(["a", "b"])));
        assert_eq!(state.get("initial_prompt"), Some(&json!("hello")));
        assert_eq!(state.len(), 4);
    }

    #[test]
    fn seed_initial_state_dispatcher_prompt_beats_an_initial_prompt_input() {
        let mut state = HashMap::from([("initial_prompt".to_string(), json!("from yaml"))]);
        let inputs = HashMap::from([("initial_prompt".to_string(), json!("from inputs"))]);

        seed_initial_state(&mut state, "from dispatcher", Some(inputs));

        assert_eq!(state.get("initial_prompt"), Some(&json!("from dispatcher")));
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn seed_initial_state_null_input_overlays_a_default() {
        let mut state = yaml_defaults();
        let inputs = HashMap::from([("width".to_string(), Value::Null)]);

        seed_initial_state(&mut state, "hello", Some(inputs));

        assert_eq!(state.get("width"), Some(&Value::Null));
    }
}
