use super::agent::AgentNodeExecutor;
use super::executor::StepContext;
use super::llm::LlmNodeExecutor;
use super::rag::RagNodeExecutor;
use super::state::StateManager;
use super::types::{ConcurrencyCap, MapNode, NodeType};
use crate::config::{RenderMode, RequestContext};
use crate::graph::type_name;
use crate::supervisor::mailbox::{Inbox, PeerAssignment, PeerRegistry, graph_agent_id};
use anyhow::{Context, Result, anyhow};
use futures_util::future::join_all;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

pub(super) struct MapNodeExecutor;

impl MapNodeExecutor {
    pub(super) async fn execute(
        node: &MapNode,
        state: &mut StateManager,
        ctx: &mut RequestContext,
        step_ctx: &StepContext<'_>,
        node_id: &str,
    ) -> Result<()> {
        let over_value = state
            .interpolate_raw(&node.over)
            .with_context(|| format!("map node '{node_id}': evaluating `over` template"))?;

        let items = over_value.as_array().ok_or_else(|| {
            anyhow!(
                "map node '{}': `over` template '{}' must resolve to an array, got {}",
                node_id,
                node.over,
                type_name(&over_value)
            )
        })?;
        let items = items.clone();

        let branch_node = step_ctx
            .graph
            .get_node(&node.branch)
            .ok_or_else(|| {
                anyhow!(
                    "map node '{node_id}': branch '{}' not found in graph",
                    node.branch
                )
            })?
            .clone();

        let max_conc = resolve_max_concurrency(node, state, step_ctx.max_concurrency, node_id)?;
        let semaphore = Arc::new(Semaphore::new(max_conc));
        let mut sub_tasks = Vec::with_capacity(items.len());

        let peers = provision_map_peers(&branch_node.node_type, &node.branch, items.len());

        for (idx, item) in items.iter().enumerate() {
            let item = item.clone();
            let as_name = node.as_name.clone();
            let branch_clone = branch_node.clone();
            let mut sub_state = state.fork_for_branch_state();
            let mut sub_ctx = ctx.fork_for_branch();
            sub_ctx.render_mode = RenderMode::Silent;
            if let Some((registry, assignments)) = &peers {
                sub_ctx.peer_registry = Some(Arc::clone(registry));
                sub_ctx.peer_assignment = Some(assignments[idx].clone());
            }
            let script_clone = step_ctx.script_executor.clone();
            let sub_branch_id = node.branch.clone();
            let sem = semaphore.clone();
            let abort = step_ctx.abort_signal.clone();

            sub_state.state_mut().set(as_name, item);

            let task = tokio::spawn(async move {
                let _permit = sem
                    .acquire()
                    .await
                    .expect("map semaphore should not be closed");
                if abort.aborted() {
                    return (
                        idx,
                        sub_state,
                        Err(anyhow!("map sub-branch [{idx}] aborted")),
                    );
                }
                let mut state = sub_state;
                let mut ctx = sub_ctx;

                let exec_result: Result<()> = match &branch_clone.node_type {
                    NodeType::Llm(n) => {
                        LlmNodeExecutor::execute(&branch_clone.id, n, &mut state, &mut ctx)
                            .await
                            .map(|_| ())
                    }
                    NodeType::Agent(n) => AgentNodeExecutor::execute(n, &mut state, &mut ctx)
                        .await
                        .map(|_| ()),
                    NodeType::Rag(n) => {
                        RagNodeExecutor::execute(n, &sub_branch_id, &mut state, &mut ctx).await
                    }
                    NodeType::Script(n) => script_clone.execute(n, &mut state).await.map(|_| ()),
                    _ => Err(anyhow!(
                        "map branch '{}' has type that cannot run inside a map \
                         (validator should have caught this; internal error)",
                        branch_clone.id
                    )),
                };

                (idx, state, exec_result)
            });
            sub_tasks.push(task);
        }

        let joined = join_all(sub_tasks).await;

        // Collect outputs keyed by input index so order is preserved regardless of finish order.
        let mut outputs: HashMap<usize, Value> = HashMap::new();
        for join_result in joined {
            let (idx, sub_state, exec_result) =
                join_result.map_err(|e| anyhow!("map sub-branch panicked: {e}"))?;

            exec_result
                .with_context(|| format!("map node '{node_id}': sub-branch [{idx}] failed"))?;

            let output_value = sub_state
                .state()
                .get(&node.output_key)
                .cloned()
                .ok_or_else(|| {
                    anyhow!(
                        "map node '{node_id}': sub-branch [{idx}] did not write \
                         `output_key` '{}'",
                        node.output_key
                    )
                })?;

            outputs.insert(idx, output_value);
        }

        let mut collected = Vec::with_capacity(items.len());
        for idx in 0..items.len() {
            let value = outputs.remove(&idx).ok_or_else(|| {
                anyhow!(
                    "map node '{node_id}': internal error: missing result for sub-branch [{idx}]"
                )
            })?;
            collected.push(value);
        }

        state
            .state_mut()
            .set(node.collect_into.clone(), Value::Array(collected));

        Ok(())
    }
}

/// A templated cap is resolved against the parent state right before the
/// fan-out. Scripts often emit numbers as strings, so a numeric string is
/// accepted; anything else — including 0 — is the author's bug and surfaces
/// as an error rather than being clamped.
fn resolve_max_concurrency(
    node: &MapNode,
    state: &StateManager,
    default: usize,
    node_id: &str,
) -> Result<usize> {
    let template = match &node.max_concurrency {
        None => return Ok(default.max(1)),
        Some(ConcurrencyCap::Fixed(n)) => return Ok((*n).max(1)),
        Some(ConcurrencyCap::Template(t)) => t,
    };

    let value = state
        .interpolate_raw(template)
        .with_context(|| format!("map node '{node_id}': evaluating `max_concurrency` template"))?;

    let resolved = match &value {
        Value::Number(n) => n.as_u64().and_then(|n| usize::try_from(n).ok()),
        Value::String(s) => s.trim().parse::<usize>().ok(),
        _ => None,
    };

    match resolved {
        Some(n) if n >= 1 => Ok(n),
        _ => Err(anyhow!(
            "map node '{node_id}': max_concurrency template \"{template}\" resolved to \
             {value} ({}); expected a positive integer",
            type_name(&value)
        )),
    }
}

/// Pre-provision teammate identities before any branch starts, so a fast
/// sibling can message one still waiting on the semaphore (the message
/// queues in the pre-created inbox). A single-item fan-out has no peers, so
/// it gets no registry.
fn provision_map_peers(
    branch: &NodeType,
    branch_id: &str,
    item_count: usize,
) -> Option<(Arc<PeerRegistry>, Vec<PeerAssignment>)> {
    match branch {
        NodeType::Agent(n) if n.teammates && item_count >= 2 => {
            let registry = Arc::new(PeerRegistry::new());
            let assignments = (0..item_count)
                .map(|i| {
                    let id = graph_agent_id(&n.agent);
                    let inbox = Arc::new(Inbox::new());
                    registry.insert(id.clone(), format!("{branch_id}[{i}]"), inbox.clone());
                    (id, inbox)
                })
                .collect();
            Some((registry, assignments))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::{AgentNode, EndNode};
    use super::*;

    fn agent_branch(teammates: bool) -> NodeType {
        NodeType::Agent(AgentNode {
            agent: "worker".into(),
            prompt: "p".into(),
            state_updates: None,
            output_schema: None,
            timeout: None,
            teammates,
        })
    }

    #[test]
    fn provision_map_peers_registers_every_branch() {
        let (registry, assignments) =
            provision_map_peers(&agent_branch(true), "shards", 3).expect("should provision");

        assert_eq!(assignments.len(), 3);
        let roster = registry.roster();
        assert_eq!(roster.len(), 3);
        for (i, (id, inbox)) in assignments.iter().enumerate() {
            assert_eq!(roster[i].0, *id);
            assert_eq!(roster[i].1, format!("shards[{i}]"));
            let resolved = registry.get(id).expect("assigned id should resolve");
            assert!(Arc::ptr_eq(&resolved, inbox));
        }
        assert_ne!(assignments[0].0, assignments[1].0);
        assert_ne!(assignments[1].0, assignments[2].0);
    }

    #[test]
    fn provision_map_peers_single_item_gets_no_registry() {
        assert!(provision_map_peers(&agent_branch(true), "shards", 1).is_none());
    }

    #[test]
    fn provision_map_peers_unflagged_gets_no_registry() {
        assert!(provision_map_peers(&agent_branch(false), "shards", 3).is_none());
    }

    #[test]
    fn provision_map_peers_non_agent_branch_gets_no_registry() {
        let branch = NodeType::End(EndNode {
            output: String::new(),
            state_updates: None,
        });
        assert!(provision_map_peers(&branch, "shards", 3).is_none());
    }

    fn map_with_cap(cap: Option<ConcurrencyCap>) -> MapNode {
        MapNode {
            over: "{{items}}".into(),
            as_name: "item".into(),
            branch: "br".into(),
            output_key: "output".into(),
            collect_into: "results".into(),
            max_concurrency: cap,
        }
    }

    fn state_with(key: &str, value: Value) -> StateManager {
        StateManager::new(HashMap::from([(key.to_string(), value)]))
    }

    fn resolve(cap: Option<ConcurrencyCap>, state: &StateManager) -> Result<usize> {
        resolve_max_concurrency(&map_with_cap(cap), state, 8, "m")
    }

    #[test]
    fn resolve_max_concurrency_fixed_uses_literal() {
        let state = StateManager::new(HashMap::new());
        assert_eq!(resolve(Some(ConcurrencyCap::Fixed(4)), &state).unwrap(), 4);
    }

    #[test]
    fn resolve_max_concurrency_none_falls_back_to_step_default() {
        let state = StateManager::new(HashMap::new());
        assert_eq!(resolve(None, &state).unwrap(), 8);
    }

    #[test]
    fn resolve_max_concurrency_template_accepts_integer_value() {
        let state = state_with("budget", Value::from(3));
        let cap = Some(ConcurrencyCap::Template("{{budget}}".into()));
        assert_eq!(resolve(cap, &state).unwrap(), 3);
    }

    #[test]
    fn resolve_max_concurrency_template_accepts_numeric_string() {
        let state = state_with("budget", Value::from("3"));
        let cap = Some(ConcurrencyCap::Template("{{budget}}".into()));
        assert_eq!(resolve(cap, &state).unwrap(), 3);
    }

    #[test]
    fn resolve_max_concurrency_template_rejects_non_numeric_string() {
        let state = state_with("budget", Value::from("high"));
        let cap = Some(ConcurrencyCap::Template("{{budget}}".into()));
        let msg = resolve(cap, &state).unwrap_err().to_string();
        assert!(msg.contains("map node 'm'"), "{msg}");
        assert!(msg.contains("\"{{budget}}\""), "{msg}");
        assert!(msg.contains("resolved to \"high\" (string)"), "{msg}");
        assert!(msg.contains("expected a positive integer"), "{msg}");
    }

    #[test]
    fn resolve_max_concurrency_template_rejects_zero() {
        let state = state_with("budget", Value::from(0));
        let cap = Some(ConcurrencyCap::Template("{{budget}}".into()));
        let msg = resolve(cap, &state).unwrap_err().to_string();
        assert!(msg.contains("resolved to 0 (number)"), "{msg}");
        assert!(msg.contains("expected a positive integer"), "{msg}");
    }

    #[test]
    fn resolve_max_concurrency_template_rejects_non_integer_number() {
        let state = state_with("budget", Value::from(2.5));
        let cap = Some(ConcurrencyCap::Template("{{budget}}".into()));
        let msg = resolve(cap, &state).unwrap_err().to_string();
        assert!(msg.contains("resolved to 2.5 (number)"), "{msg}");
    }

    #[test]
    fn resolve_max_concurrency_template_rejects_array() {
        let state = state_with("budget", Value::Array(vec![Value::from(3)]));
        let cap = Some(ConcurrencyCap::Template("{{budget}}".into()));
        let msg = resolve(cap, &state).unwrap_err().to_string();
        assert!(msg.contains("(array)"), "{msg}");
    }

    #[test]
    fn resolve_max_concurrency_template_missing_key_names_map_node() {
        let state = StateManager::new(HashMap::new());
        let cap = Some(ConcurrencyCap::Template("{{budget}}".into()));
        let err = resolve(cap, &state).unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("map node 'm': evaluating `max_concurrency` template"),
            "{chain}"
        );
        assert!(chain.contains("budget"), "{chain}");
    }
}
