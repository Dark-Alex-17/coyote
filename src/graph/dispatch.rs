use super::{GraphExecutor, GraphParser, agent_has_graph};
use crate::config::RequestContext;
use crate::config::paths;
use crate::utils::AbortSignal;
use anyhow::{Context, Result, anyhow};
use log::info;
use serde_json::Value;

pub fn active_agent_graph_name(ctx: &RequestContext) -> Option<String> {
    let name = ctx.agent.as_ref()?.name().to_string();
    agent_has_graph(&name).then_some(name)
}

pub async fn run_active_agent_graph(
    ctx: &mut RequestContext,
    prompt: &str,
    abort_signal: AbortSignal,
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
