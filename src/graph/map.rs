use super::executor::{StepContext, StepResult, step};
use super::state::StateManager;
use super::types::{ConcurrencyCap, Graph, MapNode, NodeType};
use super::validator::branch_subgraph;
use crate::config::{RenderMode, RequestContext};
use crate::graph::type_name;
use crate::supervisor::mailbox::{Inbox, PeerAssignment, PeerRegistry, graph_agent_id};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::{BoxFuture, join_all};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Semaphore;

pub(super) struct MapNodeExecutor;

impl MapNodeExecutor {
    /// Boxed because item chains call back into `step`, which calls this;
    /// the concrete return type breaks the otherwise-recursive opaque future.
    pub(super) fn execute<'a>(
        node: &'a MapNode,
        state: &'a mut StateManager,
        ctx: &'a mut RequestContext,
        step_ctx: &'a StepContext<'a>,
        node_id: &'a str,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(run_map(node, state, ctx, step_ctx, node_id))
    }
}

async fn run_map(
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

    if step_ctx.graph.get_node(&node.branch).is_none() {
        bail!(
            "map node '{node_id}': branch '{}' not found in graph",
            node.branch
        );
    }
    let subgraph = Arc::new(branch_subgraph(&step_ctx.graph, &node.branch));

    let max_conc = resolve_max_concurrency(node, state, step_ctx.max_concurrency, node_id)?;
    let semaphore = Arc::new(Semaphore::new(max_conc));
    let mut sub_tasks = Vec::with_capacity(items.len());

    let peers = provision_map_peers(&step_ctx.graph, &subgraph, &node.branch, items.len());

    for (idx, item) in items.iter().enumerate() {
        let item = item.clone();
        let as_name = node.as_name.clone();
        let mut sub_state = state.fork_for_branch_state();
        // The chain must write the result itself; an inherited parent
        // value would otherwise be collected as if the item produced it.
        sub_state.state_mut().remove(&node.output_key);
        sub_state.state_mut().set(as_name, item);
        let mut sub_ctx = ctx.fork_for_branch();
        sub_ctx.render_mode = RenderMode::Silent;
        let item_peers: Option<(Arc<PeerRegistry>, PeerAssignment)> = peers
            .as_ref()
            .map(|(registry, assignments)| (Arc::clone(registry), assignments[idx].clone()));
        let script_clone = step_ctx.script_executor.clone();
        let graph = Arc::clone(&step_ctx.graph);
        let subgraph = Arc::clone(&subgraph);
        let entry = node.branch.clone();
        let map_id = node_id.to_string();
        let max_concurrency = step_ctx.max_concurrency;
        let sem = semaphore.clone();
        let abort = step_ctx.abort_signal.clone();

        let task = tokio::spawn(async move {
            let _permit = sem
                .acquire()
                .await
                .expect("map semaphore should not be closed");
            let mut state = sub_state;
            let mut ctx = sub_ctx;
            let step_ctx = StepContext {
                graph,
                script_executor: &script_clone,
                max_concurrency,
                abort_signal: &abort,
                branch_mode: true,
            };
            let chain = ItemChain {
                map_id: &map_id,
                entry: &entry,
                subgraph: &subgraph,
                idx,
                peers: item_peers.as_ref(),
            };
            let result = run_item_chain(&chain, &mut state, &mut ctx, &step_ctx).await;
            (idx, state, result)
        });
        sub_tasks.push(task);
    }

    let joined = join_all(sub_tasks).await;

    // Collect outputs keyed by input index so order is preserved regardless of finish order.
    let mut outputs: HashMap<usize, Value> = HashMap::new();
    for join_result in joined {
        let (idx, sub_state, exec_result) =
            join_result.map_err(|e| anyhow!("map sub-branch panicked: {e}"))?;

        exec_result?;

        let output_value = sub_state
            .state()
            .get(&node.output_key)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "map node '{node_id}': sub-branch [{idx}] did not write output_key '{}'; \
                     write it via state_updates, output_schema, or a script's JSON output \
                     (the parent's value is not inherited inside a map branch)",
                    node.output_key
                )
            })?;

        outputs.insert(idx, output_value);
    }

    let mut collected = Vec::with_capacity(items.len());
    for idx in 0..items.len() {
        let value = outputs.remove(&idx).ok_or_else(|| {
            anyhow!("map node '{node_id}': internal error: missing result for sub-branch [{idx}]")
        })?;
        collected.push(value);
    }

    state
        .state_mut()
        .set(node.collect_into.clone(), Value::Array(collected));

    Ok(())
}

/// One item's slice of a map fan-out: where its chain starts, which nodes
/// it may visit, and (when the fan-out is a team) the identity it speaks as.
struct ItemChain<'a> {
    map_id: &'a str,
    entry: &'a str,
    subgraph: &'a HashSet<String>,
    idx: usize,
    peers: Option<&'a (Arc<PeerRegistry>, PeerAssignment)>,
}

/// Runs one item's chain to completion on its forked state and ctx. Owns the
/// item's teammate identity for the whole chain: arms it only before
/// `teammates: true` agent steps and retires it when the chain exits,
/// successfully or not.
async fn run_item_chain(
    chain: &ItemChain<'_>,
    state: &mut StateManager,
    ctx: &mut RequestContext,
    step_ctx: &StepContext<'_>,
) -> Result<()> {
    let result = run_chain_steps(chain, state, ctx, step_ctx).await;
    if let Some((registry, (id, _))) = chain.peers {
        registry.mark_finished(id);
    }
    result
}

async fn run_chain_steps(
    chain: &ItemChain<'_>,
    state: &mut StateManager,
    ctx: &mut RequestContext,
    step_ctx: &StepContext<'_>,
) -> Result<()> {
    let mut current = chain.entry.to_string();
    let mut step_no = 0usize;
    loop {
        step_no += 1;
        let next = run_chain_step(chain, state, ctx, step_ctx, &current)
            .await
            .with_context(|| {
                format!(
                    "map node '{}': sub-branch [{}] failed at node '{current}' (step {step_no})",
                    chain.map_id, chain.idx
                )
            })?;
        match next {
            Some(target) => current = target,
            None => return Ok(()),
        }
    }
}

/// Runs a single node of the chain and returns the node to visit next, or
/// `None` once the chain has ended.
async fn run_chain_step(
    chain: &ItemChain<'_>,
    state: &mut StateManager,
    ctx: &mut RequestContext,
    step_ctx: &StepContext<'_>,
    current: &str,
) -> Result<Option<String>> {
    let ItemChain { map_id, idx, .. } = *chain;
    if step_ctx.abort_signal.aborted() {
        bail!("map sub-branch [{idx}] aborted");
    }
    let node = step_ctx.graph.get_node(current).ok_or_else(|| {
        anyhow!("map node '{map_id}': sub-branch [{idx}] routed to unknown node '{current}'")
    })?;
    if !matches!(
        node.node_type,
        NodeType::Llm(_) | NodeType::Agent(_) | NodeType::Rag(_) | NodeType::Script(_)
    ) {
        bail!(
            "map branch '{current}' has type that cannot run inside a map \
             (validator should have caught this; internal error)"
        );
    }

    state.state_mut().visit_node(current);
    let visits = state.state().loop_count(current);
    let max_loops = step_ctx.graph.settings.max_loop_iterations;
    if visits > max_loops {
        bail!(
            "node '{current}' visited {visits} times in map branch [{idx}] \
             (max_loop_iterations={max_loops})"
        );
    }

    if let (Some((registry, assignment)), NodeType::Agent(a)) = (chain.peers, &node.node_type)
        && a.teammates
    {
        ctx.peer_registry = Some(Arc::clone(registry));
        ctx.peer_assignment = Some(assignment.clone());
    }

    match step(node, state, ctx, step_ctx, current).await? {
        StepResult::Continue(targets) => match targets.as_slice() {
            [] => {
                debug!(
                    "[graph:{}] map '{map_id}' [{idx}] {current} → END",
                    step_ctx.graph_name()
                );
                Ok(None)
            }
            [target] => {
                if !chain.subgraph.contains(target) {
                    let mut branch_nodes: Vec<&str> =
                        chain.subgraph.iter().map(String::as_str).collect();
                    branch_nodes.sort_unstable();
                    bail!(
                        "map node '{map_id}': sub-branch [{idx}] routed to '{target}' which is \
                         outside the branch subgraph rooted at '{}' (branch nodes: {}). Script \
                         `_next` targets inside a map branch must stay within the branch.",
                        chain.entry,
                        branch_nodes.join(", ")
                    );
                }
                debug!(
                    "[graph:{}] map '{map_id}' [{idx}] {current} → {target}",
                    step_ctx.graph_name()
                );
                Ok(Some(target.clone()))
            }
            many => bail!(
                "map node '{map_id}': sub-branch [{idx}] node '{current}' fanned out to {many:?}; \
                 a map branch must route to a single node"
            ),
        },
        StepResult::End(_) => bail!(
            "map node '{map_id}': sub-branch [{idx}] reached end node '{current}' inside a map branch"
        ),
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

/// Pre-provision one teammate identity per item before any chain starts, so a
/// fast sibling can message one still waiting on the semaphore (the message
/// queues in the pre-created inbox). The identity lives for the item's whole
/// chain; every `teammates: true` agent step in it speaks as that item. A
/// single-item fan-out has no peers, so it gets no registry.
fn provision_map_peers(
    graph: &Graph,
    subgraph: &HashSet<String>,
    entry: &str,
    item_count: usize,
) -> Option<(Arc<PeerRegistry>, Vec<PeerAssignment>)> {
    if item_count < 2 {
        return None;
    }
    let flagged_agent = |node_id: &str| match graph.get_node(node_id).map(|n| &n.node_type) {
        Some(NodeType::Agent(a)) if a.teammates => Some(a.agent.as_str()),
        _ => None,
    };
    let agent_name = flagged_agent(entry).or_else(|| {
        graph
            .nodes
            .keys()
            .filter(|id| subgraph.contains(*id))
            .find_map(|id| flagged_agent(id))
    })?;

    let registry = Arc::new(PeerRegistry::new());
    let assignments = (0..item_count)
        .map(|i| {
            let id = graph_agent_id(agent_name);
            let inbox = Arc::new(Inbox::new());
            registry.insert(id.clone(), format!("{entry}[{i}]"), inbox.clone());
            (id, inbox)
        })
        .collect();
    Some((registry, assignments))
}

#[cfg(test)]
mod tests {
    use super::super::types::{AgentNode, EndNode, GraphSettings, Node};
    use super::*;
    use indexmap::IndexMap;

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

    fn graph_with_branch(node_type: NodeType) -> Graph {
        let mut nodes: IndexMap<String, Node> = IndexMap::new();
        nodes.insert(
            "shards".into(),
            Node {
                id: "shards".into(),
                description: String::new(),
                node_type,
                next: None,
            },
        );

        Graph {
            name: "t".into(),
            description: String::new(),
            version: "1.0".into(),
            model: None,
            temperature: None,
            top_p: None,
            reasoning_effort: None,
            max_concurrent_jobs: None,
            can_spawn_agents: None,
            max_concurrent_agents: None,
            max_agent_depth: None,
            global_tools: Vec::new(),
            mcp_servers: Vec::new(),
            mcp_tools: None,
            skills_enabled: None,
            enabled_skills: None,
            inject_skill_instructions: None,
            skill_instructions: None,
            conversation_starters: Vec::new(),
            variables: Vec::new(),
            settings: GraphSettings::default(),
            initial_state: HashMap::new(),
            reducers: HashMap::new(),
            start: "shards".into(),
            nodes,
        }
    }

    fn provision(
        node_type: NodeType,
        n: usize,
    ) -> Option<(Arc<PeerRegistry>, Vec<PeerAssignment>)> {
        let graph = graph_with_branch(node_type);
        let subgraph = HashSet::from(["shards".to_string()]);
        provision_map_peers(&graph, &subgraph, "shards", n)
    }

    #[test]
    fn provision_map_peers_registers_every_branch() {
        let (registry, assignments) = provision(agent_branch(true), 3).expect("should provision");

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
        assert!(provision(agent_branch(true), 1).is_none());
    }

    #[test]
    fn provision_map_peers_unflagged_gets_no_registry() {
        assert!(provision(agent_branch(false), 3).is_none());
    }

    #[test]
    fn provision_map_peers_non_agent_branch_gets_no_registry() {
        let branch = NodeType::End(EndNode {
            output: String::new(),
            state_updates: None,
        });
        assert!(provision(branch, 3).is_none());
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
