//! Helpers for running the active agent through its `graph.yaml` instead
//! of the LLM loop. Used at every agent-execution entry point: top-level
//! CLI (`start_directive`), REPL (`ask`), and child-agent spawn
//! (`run_child_agent`).

use super::{GraphExecutor, GraphParser, agent_has_graph};
use crate::config::RequestContext;
use crate::config::paths;
use crate::utils::AbortSignal;
use anyhow::{Context, Result};
use serde_json::Value;

/// If the active agent owns a `graph.yaml`, returns its name. Lets
/// callers branch between graph and LLM-loop execution without
/// re-implementing the lookup.
pub fn active_agent_graph_name(ctx: &RequestContext) -> Option<String> {
    let name = ctx.agent.as_ref()?.name().to_string();
    agent_has_graph(&name).then_some(name)
}

/// Run the active agent's graph end-to-end and return the resolved
/// End-node output. The caller's prompt is seeded into the graph state
/// as `initial_prompt` so nodes can reference it via
/// `{{initial_prompt}}`. Any sub-agents the graph spawned via the
/// supervisor are cancelled on return.
pub async fn run_active_agent_graph(
    ctx: &mut RequestContext,
    prompt: &str,
    abort_signal: AbortSignal,
) -> Result<String> {
    let agent_name = active_agent_graph_name(ctx)
        .ok_or_else(|| anyhow::anyhow!("Active agent has no graph.yaml"))?;

    log::info!("Agent '{agent_name}' has graph.yaml; routing to graph executor");

    let agent_dir = paths::agent_data_dir(&agent_name);
    let graph_path = paths::agent_graph_path(&agent_name);

    let parser = GraphParser::new(&agent_dir);
    let mut graph = parser
        .load_from_file(&graph_path)
        .with_context(|| format!("Failed to load graph.yaml for agent '{agent_name}'"))?;

    graph
        .initial_state
        .insert("initial_prompt".into(), Value::String(prompt.to_string()));

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
