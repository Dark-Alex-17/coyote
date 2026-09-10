use super::executor::{StepContext, StepResult, step};
use super::state::StateManager;
use super::types::{ConcurrencyCap, Graph, MapNode, Node, NodeType};
use super::validator::branch_subgraph;
use crate::config::{RenderMode, RequestContext};
use crate::graph::type_name;
use crate::supervisor::mailbox::{Inbox, PeerAssignment, PeerRegistry, graph_agent_id};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::{BoxFuture, join_all};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
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
/// `None` once the chain has ended. Errors carry no map/item locator; the
/// caller's context supplies it.
async fn run_chain_step(
    chain: &ItemChain<'_>,
    state: &mut StateManager,
    ctx: &mut RequestContext,
    step_ctx: &StepContext<'_>,
    current: &str,
) -> Result<Option<String>> {
    let ItemChain { map_id, idx, .. } = *chain;
    if step_ctx.abort_signal.aborted() {
        bail!("aborted");
    }
    let node = step_ctx
        .graph
        .get_node(current)
        .ok_or_else(|| anyhow!("routed to unknown node '{current}'"))?;
    let disallowed = match &node.node_type {
        NodeType::Approval(_) => Some("an approval node"),
        NodeType::Input(_) => Some("an input node"),
        NodeType::End(_) => Some("an end node"),
        NodeType::Map(_) => Some("a map node"),
        NodeType::Agent(_) | NodeType::Llm(_) | NodeType::Rag(_) | NodeType::Script(_) => None,
    };
    if let Some(type_phrase) = disallowed {
        bail!(
            "'{current}' is {type_phrase}; approval/input/end/map nodes cannot run inside a \
             map branch (enable settings.validate_before_run to catch this at load time)"
        );
    }

    state.state_mut().visit_node(current);
    let visits = state.state().loop_count(current);
    let max_loops = step_ctx.graph.settings.max_loop_iterations;
    if visits > max_loops {
        bail!("node '{current}' visited {visits} times (max_loop_iterations={max_loops})");
    }

    if let Some((registry, assignment)) = chain.peers
        && wants_peer_identity(node)
    {
        ctx.peer_registry = Some(Arc::clone(registry));
        ctx.peer_assignment = Some(assignment.clone());
    }

    let result = step(node, state, ctx, step_ctx, current).await;
    ctx.peer_registry = None;
    ctx.peer_assignment = None;

    match result? {
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
                        "routed to '{target}' which is outside the branch subgraph rooted at \
                         '{}' (branch nodes: {}). Script `_next` targets inside a map branch \
                         must stay within the branch.",
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
                "node '{current}' fanned out to {many:?}; a map branch must route to a single node"
            ),
        },
        // The node-type pre-check above rejects End before step() runs.
        StepResult::End(_) => bail!(
            "internal error: step() returned End for '{current}' after the node-type pre-check"
        ),
    }
}

fn wants_peer_identity(node: &Node) -> bool {
    matches!(&node.node_type, NodeType::Agent(a) if a.teammates)
}

/// A templated cap is resolved against the parent state right before the
/// fan-out. Scripts often emit numbers as strings, so a numeric string is
/// accepted; anything else, including 0, is the author's bug and surfaces
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
/// single-item fan-out has no peers, so it gets no registry. The identity is
/// named after the first `teammates: true` agent found by BFS from the entry,
/// so the pick follows the chain as a reader traces it, not YAML order.
fn provision_map_peers(
    graph: &Graph,
    subgraph: &HashSet<String>,
    entry: &str,
    item_count: usize,
) -> Option<(Arc<PeerRegistry>, Vec<PeerAssignment>)> {
    if item_count < 2 {
        return None;
    }
    let agent_name = first_flagged_agent_by_bfs(graph, subgraph, entry)?;

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

fn first_flagged_agent_by_bfs<'g>(
    graph: &'g Graph,
    subgraph: &HashSet<String>,
    entry: &str,
) -> Option<&'g str> {
    let mut visited: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    if subgraph.contains(entry) {
        visited.insert(entry);
        queue.push_back(entry);
    }
    while let Some(id) = queue.pop_front() {
        let Some(node) = graph.get_node(id) else {
            continue;
        };
        if let NodeType::Agent(a) = &node.node_type
            && a.teammates
        {
            return Some(a.agent.as_str());
        }
        let mut edges: Vec<&String> = node
            .next
            .as_ref()
            .map(|t| t.as_slice().iter().collect())
            .unwrap_or_default();
        match &node.node_type {
            NodeType::Script(s) => edges.extend(s.fallback.as_ref()),
            NodeType::Llm(l) => edges.extend(l.fallback.as_ref()),
            _ => {}
        }
        for next in edges {
            if subgraph.contains(next) && visited.insert(next) {
                queue.push_back(next);
            }
        }
    }
    None
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

#[cfg(test)]
mod chain_tests {
    use super::super::executor::GraphExecutor;
    use super::super::script::ScriptExecutor;
    use super::*;
    use crate::config::paths;
    use crate::config::{AppState, Role, WorkingMode};
    use crate::utils::{AbortSignal, create_abort_signal, get_env_name, temp_file};
    use indexmap::IndexMap;
    use serde_json::json;
    use serial_test::serial;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn cmd_available(name: &str) -> bool {
        which::which(name).is_ok()
    }

    struct TestWorkspace {
        dir: PathBuf,
    }

    impl TestWorkspace {
        fn new() -> Self {
            let dir = temp_file("-graph-map-", "");
            fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }

        fn write_script(&self, name: &str, contents: &str) {
            fs::write(self.dir.join(name), contents).unwrap();
        }

        fn write_py(&self, name: &str, body: &str) {
            self.write_script(
                name,
                &format!(
                    "#!/usr/bin/env python3\nimport os, json\n\
                     state = json.loads(os.environ.get(\"GRAPH_STATE\", \"{{}}\"))\n{body}\n"
                ),
            );
        }
    }

    impl Drop for TestWorkspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn make_ctx() -> RequestContext {
        RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd)
    }

    async fn run_graph(yaml: &str, ws: &TestWorkspace) -> Result<String> {
        let graph: Graph = serde_yaml::from_str(yaml).unwrap();
        let mut ctx = make_ctx();
        GraphExecutor::new(graph, &ws.dir)
            .execute(&mut ctx, create_abort_signal())
            .await
    }

    fn collected(result: &str) -> Vec<Value> {
        serde_json::from_str::<Value>(result)
            .unwrap_or_else(|_| panic!("expected JSON array, got: {result}"))
            .as_array()
            .expect("collected results should be an array")
            .clone()
    }

    fn error_chain(result: Result<String>) -> String {
        match result {
            Ok(out) => panic!("expected failure, got output: {out}"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[tokio::test]
    async fn map_chain_two_script_steps_route_via_next() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_py(
            "step_one.py",
            r#"print(json.dumps({"draft": state["item"] * 2}))"#,
        );
        ws.write_py(
            "step_two.py",
            r#"print(json.dumps({"output": state["draft"] + 1}))"#,
        );

        let yaml = r#"
name: chain
start: fan_out
settings:
  validate_before_run: false
initial_state:
  items: [1, 2, 3]
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: step_one
    collect_into: results
    next: done
  step_one:
    type: script
    script: step_one.py
    next: step_two
  step_two:
    type: script
    script: step_two.py
  done:
    type: end
    output: "{{results}}"
"#;
        let result = run_graph(yaml, &ws)
            .await
            .unwrap_or_else(|e| panic!("executor failed: {e:#}"));

        assert_eq!(collected(&result), vec![json!(3), json!(5), json!(7)]);
    }

    #[tokio::test]
    async fn map_chain_two_script_steps_pass_validation_before_run() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_py(
            "step_one.py",
            r#"print(json.dumps({"draft": state["item"] * 2}))"#,
        );
        ws.write_py(
            "step_two.py",
            r#"print(json.dumps({"output": state["draft"] + 1}))"#,
        );

        let yaml = r#"
name: chain
start: fan_out
settings:
  validate_before_run: true
initial_state:
  items: [1, 2, 3]
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: step_one
    collect_into: results
    next: done
  step_one:
    type: script
    script: step_one.py
    next: step_two
  step_two:
    type: script
    script: step_two.py
  done:
    type: end
    output: "{{results}}"
"#;
        let result = run_graph(yaml, &ws)
            .await
            .unwrap_or_else(|e| panic!("validation rejected a runnable chain: {e:#}"));

        assert_eq!(collected(&result), vec![json!(3), json!(5), json!(7)]);
    }

    const RETRY_CHAIN_GRAPH: &str = r#"
name: retry-chain
start: fan_out
settings:
  validate_before_run: true
initial_state:
  items: ["a", "b", "c"]
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: draft
    output_key: result
    collect_into: results
    next: done
  draft:
    type: script
    script: draft.py
    next: check
  check:
    type: script
    script: check.py
  done:
    type: end
    output: "{{results}}"
"#;

    fn write_retry_chain_scripts(ws: &TestWorkspace) {
        ws.write_py(
            "draft.py",
            r##"attempts = state.get("attempts", 0) + 1
print(json.dumps({"attempts": attempts, "draft": f"{state['item']}#{attempts}"}))"##,
        );
        ws.write_py(
            "check.py",
            r#"if state["attempts"] < 2:
    print(json.dumps({"_next": "draft"}))
else:
    print(json.dumps({"result": {"item": state["item"], "draft": state["draft"], "attempts": state["attempts"]}}))"#,
        );
    }

    fn retry_chain_results() -> Vec<Value> {
        ["a", "b", "c"]
            .iter()
            .map(|item| json!({"item": item, "draft": format!("{item}#2"), "attempts": 2}))
            .collect()
    }

    #[tokio::test]
    async fn map_chain_retries_via_script_next_until_check_accepts() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        write_retry_chain_scripts(&ws);

        let result = run_graph(RETRY_CHAIN_GRAPH, &ws)
            .await
            .unwrap_or_else(|e| panic!("validated retry chain failed: {e:#}"));

        assert_eq!(collected(&result), retry_chain_results());
    }

    #[tokio::test]
    async fn map_chain_retries_leave_parent_state_and_loop_counts_untouched() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        write_retry_chain_scripts(&ws);
        let graph: Arc<Graph> = Arc::new(serde_yaml::from_str(RETRY_CHAIN_GRAPH).unwrap());
        let NodeType::Map(map_node) = &graph.get_node("fan_out").unwrap().node_type else {
            panic!("fan_out should be a map node");
        };
        let script = ScriptExecutor::new(&ws.dir);
        let abort = create_abort_signal();
        let step_ctx = StepContext {
            graph: Arc::clone(&graph),
            script_executor: &script,
            max_concurrency: 4,
            abort_signal: &abort,
            branch_mode: false,
        };
        let items = json!(["a", "b", "c"]);
        let mut parent = StateManager::new(HashMap::from([("items".to_string(), items.clone())]));
        let mut ctx = silent_ctx();

        MapNodeExecutor::execute(map_node, &mut parent, &mut ctx, &step_ctx, "fan_out")
            .await
            .unwrap_or_else(|e| panic!("map failed: {e:#}"));

        // Only `collect_into` lands on the parent: the per-item `attempts`,
        // `draft` and `result` writes were fork-local scratch.
        assert_eq!(
            *parent.state().data(),
            HashMap::from([
                ("items".to_string(), items),
                ("results".to_string(), Value::Array(retry_chain_results())),
            ])
        );
        for node in ["fan_out", "draft", "check"] {
            assert_eq!(
                parent.state().loop_count(node),
                0,
                "parent loop count for '{node}'"
            );
        }
    }

    #[tokio::test]
    async fn map_chain_script_next_loop_is_bounded_by_max_loop_iterations() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_py("looper.py", r#"print(json.dumps({"_next": "looper"}))"#);

        let yaml = r#"
name: chain
start: fan_out
settings:
  max_loop_iterations: 3
  validate_before_run: false
initial_state:
  items: [1]
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: looper
    collect_into: results
    next: done
  looper:
    type: script
    script: looper.py
  done:
    type: end
    output: "{{results}}"
"#;
        let chain = error_chain(run_graph(yaml, &ws).await);

        assert!(
            chain.contains("map node 'fan_out': sub-branch [0] failed at node 'looper' (step 4)"),
            "{chain}"
        );
        assert!(
            chain.contains("node 'looper' visited 4 times (max_loop_iterations=3)"),
            "{chain}"
        );
        assert_eq!(chain.matches("sub-branch [").count(), 1, "{chain}");
    }

    #[tokio::test]
    async fn map_chain_next_outside_subgraph_fails_item() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_py("gate.py", r#"print(json.dumps({"_next": "done"}))"#);

        let yaml = r#"
name: chain
start: fan_out
settings:
  validate_before_run: false
initial_state:
  items: [1]
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: gate
    collect_into: results
    next: done
  gate:
    type: script
    script: gate.py
  done:
    type: end
    output: "{{results}}"
"#;
        let chain = error_chain(run_graph(yaml, &ws).await);

        assert!(
            chain.contains(
                "routed to 'done' which is outside the branch subgraph rooted at 'gate' \
                 (branch nodes: gate)"
            ),
            "{chain}"
        );
        assert!(
            chain.contains(
                "Script `_next` targets inside a map branch must stay within the branch."
            ),
            "{chain}"
        );
        assert!(
            chain.contains("map node 'fan_out': sub-branch [0] failed at node 'gate' (step 1)"),
            "{chain}"
        );
        assert_eq!(chain.matches("sub-branch [").count(), 1, "{chain}");
    }

    #[tokio::test]
    async fn map_chain_not_writing_output_key_errors_with_hint() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_py("worker.py", r#"print(json.dumps({"other": 1}))"#);

        let yaml = r#"
name: chain
start: fan_out
settings:
  validate_before_run: false
initial_state:
  items: [1]
  output: stale
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: worker
    collect_into: results
    next: done
  worker:
    type: script
    script: worker.py
  done:
    type: end
    output: "{{results}}"
"#;
        let chain = error_chain(run_graph(yaml, &ws).await);

        assert!(
            chain.contains("sub-branch [0] did not write output_key 'output'"),
            "{chain}"
        );
        assert!(
            chain.contains("the parent's value is not inherited inside a map branch"),
            "{chain}"
        );
    }

    #[tokio::test]
    async fn map_chain_output_equal_to_parent_value_is_collected() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_py("worker.py", r#"print(json.dumps({"output": 7}))"#);

        let yaml = r#"
name: chain
start: fan_out
settings:
  validate_before_run: false
initial_state:
  items: [7, 7]
  output: 7
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: worker
    collect_into: results
    next: done
  worker:
    type: script
    script: worker.py
  done:
    type: end
    output: "{{results}}"
"#;
        let result = run_graph(yaml, &ws)
            .await
            .unwrap_or_else(|e| panic!("executor failed: {e:#}"));

        assert_eq!(collected(&result), vec![json!(7), json!(7)]);
    }

    #[tokio::test]
    async fn map_chain_state_updates_reading_output_key_sees_empty_string() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_py("worker.py", r#"print(json.dumps({"draft": "d"}))"#);

        let yaml = r#"
name: chain
start: fan_out
settings:
  validate_before_run: false
initial_state:
  items: ["a"]
  summary: parent
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: worker
    output_key: summary
    collect_into: results
    next: done
  worker:
    type: script
    script: worker.py
    state_updates:
      summary: "{{summary}}|{{draft}}"
  done:
    type: end
    output: "{{results}}"
"#;
        let result = run_graph(yaml, &ws)
            .await
            .unwrap_or_else(|e| panic!("executor failed: {e:#}"));

        assert_eq!(collected(&result), vec![json!("|d")]);
    }

    #[tokio::test]
    async fn map_chain_pre_check_rejects_approval_node() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_py("gate.py", r#"print(json.dumps({}))"#);

        let yaml = r#"
name: chain
start: fan_out
settings:
  validate_before_run: false
initial_state:
  items: [1]
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: gate
    collect_into: results
    next: done
  gate:
    type: script
    script: gate.py
    next: ask
  ask:
    type: approval
    question: "ok?"
    options: ["yes", "no"]
    routes:
      "yes": gate
    on_other: gate
  done:
    type: end
    output: "{{results}}"
"#;
        let chain = error_chain(run_graph(yaml, &ws).await);

        assert!(
            chain.contains(
                "'ask' is an approval node; approval/input/end/map nodes cannot run inside a \
                 map branch (enable settings.validate_before_run to catch this at load time)"
            ),
            "{chain}"
        );
        assert!(chain.contains("failed at node 'ask' (step 2)"), "{chain}");
    }

    #[tokio::test]
    async fn map_chain_pre_check_rejects_end_node() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_py("gate.py", r#"print(json.dumps({}))"#);

        let yaml = r#"
name: chain
start: fan_out
settings:
  validate_before_run: false
initial_state:
  items: [1]
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: gate
    collect_into: results
    next: done
  gate:
    type: script
    script: gate.py
    next: finish
  finish:
    type: end
    output: "done early"
  done:
    type: end
    output: "{{results}}"
"#;
        let chain = error_chain(run_graph(yaml, &ws).await);

        assert!(
            chain.contains(
                "'finish' is an end node; approval/input/end/map nodes cannot run inside a \
                 map branch (enable settings.validate_before_run to catch this at load time)"
            ),
            "{chain}"
        );
        assert!(
            chain.contains("map node 'fan_out': sub-branch [0] failed at node 'finish' (step 2)"),
            "{chain}"
        );
        assert_eq!(chain.matches("sub-branch [").count(), 1, "{chain}");
    }

    #[tokio::test]
    async fn map_chain_script_error_routes_to_fallback() {
        if !cmd_available("python3") || !cmd_available("bash") {
            eprintln!("skipping: python3 or bash not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_script("worker.sh", "#!/bin/bash\necho boom >&2\nexit 1\n");
        ws.write_py(
            "recover.py",
            r#"print(json.dumps({"output": "recovered"}))"#,
        );

        let yaml = r#"
name: chain
start: fan_out
settings:
  validate_before_run: false
initial_state:
  items: [1, 2]
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: worker
    collect_into: results
    next: done
  worker:
    type: script
    script: worker.sh
    fallback: recover
  recover:
    type: script
    script: recover.py
  done:
    type: end
    output: "{{results}}"
"#;
        let result = run_graph(yaml, &ws)
            .await
            .unwrap_or_else(|e| panic!("executor failed: {e:#}"));

        assert_eq!(
            collected(&result),
            vec![json!("recovered"), json!("recovered")]
        );
    }

    #[tokio::test]
    async fn map_chain_llm_failure_routes_to_fallback() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let ws = TestWorkspace::new();
        ws.write_py("recover.py", r#"print(json.dumps({"output": "fb"}))"#);

        let yaml = r#"
name: chain
start: fan_out
settings:
  validate_before_run: false
initial_state:
  items: [1]
nodes:
  fan_out:
    type: map
    over: "{{items}}"
    as: item
    branch: check
    collect_into: results
    next: done
  check:
    type: llm
    prompt: "hi"
    fallback: recover
  recover:
    type: script
    script: recover.py
  done:
    type: end
    output: "{{results}}"
"#;
        let result = run_graph(yaml, &ws)
            .await
            .unwrap_or_else(|e| panic!("executor failed: {e:#}"));

        assert_eq!(collected(&result), vec![json!("fb")]);
    }

    struct Harness {
        ws: TestWorkspace,
        graph: Arc<Graph>,
        abort: AbortSignal,
    }

    impl Harness {
        fn new(yaml: &str) -> Self {
            Self {
                ws: TestWorkspace::new(),
                graph: Arc::new(serde_yaml::from_str(yaml).unwrap()),
                abort: create_abort_signal(),
            }
        }

        fn subgraph(&self, entry: &str) -> HashSet<String> {
            branch_subgraph(&self.graph, entry)
        }

        fn provision(&self, entry: &str, n: usize) -> (Arc<PeerRegistry>, Vec<PeerAssignment>) {
            provision_map_peers(&self.graph, &self.subgraph(entry), entry, n)
                .expect("subgraph with a flagged agent should provision peers")
        }

        async fn run(
            &self,
            entry: &str,
            peers: Option<&(Arc<PeerRegistry>, PeerAssignment)>,
            state: &mut StateManager,
            ctx: &mut RequestContext,
        ) -> Result<()> {
            let script = ScriptExecutor::new(&self.ws.dir);
            let step_ctx = StepContext {
                graph: Arc::clone(&self.graph),
                script_executor: &script,
                max_concurrency: 4,
                abort_signal: &self.abort,
                branch_mode: true,
            };
            let subgraph = self.subgraph(entry);
            let chain = ItemChain {
                map_id: "fan_out",
                entry,
                subgraph: &subgraph,
                idx: 0,
                peers,
            };
            run_item_chain(&chain, state, ctx, &step_ctx).await
        }
    }

    fn item_state(item: Value) -> StateManager {
        StateManager::new(HashMap::from([("item".to_string(), item)]))
    }

    fn silent_ctx() -> RequestContext {
        let mut ctx = make_ctx();
        ctx.render_mode = RenderMode::Silent;
        ctx
    }

    fn manual_peer(label: &str) -> (Arc<PeerRegistry>, PeerAssignment) {
        let registry = Arc::new(PeerRegistry::new());
        let id = graph_agent_id(label);
        let inbox = Arc::new(Inbox::new());
        registry.insert(id.clone(), format!("{label}[0]"), Arc::clone(&inbox));
        (registry, (id, inbox))
    }

    fn unwrap_err_chain(result: Result<()>) -> String {
        match result {
            Ok(()) => panic!("expected the chain to fail"),
            Err(e) => format!("{e:#}"),
        }
    }

    struct TestConfigDirGuard {
        key: String,
        previous: Option<std::ffi::OsString>,
        path: PathBuf,
    }

    impl TestConfigDirGuard {
        fn new() -> Self {
            let key = get_env_name("config_dir");
            let previous = env::var_os(&key);
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = env::temp_dir().join(format!("coyote-graph-map-tests-{unique}"));
            fs::create_dir_all(&path).unwrap();
            unsafe {
                env::set_var(&key, &path);
            }
            Self {
                key,
                previous,
                path,
            }
        }
    }

    impl Drop for TestConfigDirGuard {
        fn drop(&mut self) {
            if let Some(previous) = &self.previous {
                unsafe {
                    env::set_var(&self.key, previous);
                }
            } else {
                unsafe {
                    env::remove_var(&self.key);
                }
            }
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    const PROBE_AGENT: &str = "timeout-probe";

    /// Materializes a graph agent under the guarded config dir that
    /// `Agent::init` can load offline: no global tools, variables, MCP servers,
    /// or rag nodes, so nothing in `run_agent_for_graph` bails before the
    /// agent's own graph starts executing. Its single script node sleeps for
    /// `hold_secs` and then writes `output`, so the whole run needs no LLM and
    /// the agent future can be held open for as long as a test needs.
    fn materialize_probe_agent(hold_secs: f64) {
        let graph_path = paths::agent_graph_file(PROBE_AGENT);
        let agent_dir = graph_path.parent().unwrap();
        fs::create_dir_all(agent_dir).unwrap();
        fs::write(
            agent_dir.join("hold.py"),
            format!(
                "#!/usr/bin/env python3\nimport json, time\ntime.sleep({hold_secs})\n\
                 print(json.dumps({{\"output\": \"held\"}}))\n"
            ),
        )
        .unwrap();
        fs::write(
            &graph_path,
            format!(
                r#"
name: {PROBE_AGENT}
start: hold
nodes:
  hold:
    type: script
    script: hold.py
    timeout: 30
    next: done
  done:
    type: end
    output: "{{{{output}}}}"
"#
            ),
        )
        .unwrap();
    }

    const DOUBLER_GRAPH: &str = r#"
name: t
start: doubler
nodes:
  doubler:
    type: script
    script: doubler.py
"#;

    const GATE_WORKER_FINISH_GRAPH: &str = r#"
name: t
start: gate
nodes:
  gate:
    type: script
    script: gate.py
    next: worker
  worker:
    type: agent
    agent: no-such-agent
    prompt: "p"
    teammates: true
    next: finish
  finish:
    type: script
    script: finish.py
"#;

    const FLAGGED_WORKER_GRAPH: &str = r#"
name: t
start: worker
nodes:
  worker:
    type: agent
    agent: no-such-agent
    prompt: "p"
    teammates: true
"#;

    #[tokio::test]
    async fn run_item_chain_single_script_branch_final_fork_state() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let h = Harness::new(DOUBLER_GRAPH);
        h.ws.write_py(
            "doubler.py",
            r#"print(json.dumps({"output": state["item"] * 2}))"#,
        );
        let mut state = item_state(json!(3));
        let mut ctx = silent_ctx();

        h.run("doubler", None, &mut state, &mut ctx)
            .await
            .unwrap_or_else(|e| panic!("chain failed: {e:#}"));

        assert_eq!(
            *state.state().data(),
            HashMap::from([
                ("item".to_string(), json!(3)),
                ("output".to_string(), json!(6)),
            ])
        );
        assert_eq!(state.state().loop_count("doubler"), 1);
        assert_eq!(state.state().current_node(), Some("doubler"));
        assert!(ctx.peer_registry.is_none() && ctx.peer_assignment.is_none());
    }

    #[tokio::test]
    async fn run_item_chain_aborted_before_first_step_touches_nothing() {
        let h = Harness::new(DOUBLER_GRAPH);
        h.ws.write_py(
            "doubler.py",
            r#"print(json.dumps({"output": state["item"] * 2}))"#,
        );
        let peers = manual_peer("doubler");
        let mut state = item_state(json!(3));
        let mut ctx = silent_ctx();
        h.abort.set_ctrlc();

        let chain = unwrap_err_chain(h.run("doubler", Some(&peers), &mut state, &mut ctx).await);

        assert!(
            chain.contains("failed at node 'doubler' (step 1): aborted"),
            "{chain}"
        );
        assert_eq!(chain.matches("sub-branch [").count(), 1, "{chain}");
        assert_eq!(state.state().loop_count("doubler"), 0);
        assert!(state.state().get("output").is_none());
        assert!(peers.0.is_finished(&peers.1.0));
    }

    #[tokio::test]
    async fn run_item_chain_marks_identity_finished_on_success_without_arming_script_steps() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let h = Harness::new(GATE_WORKER_FINISH_GRAPH);
        h.ws.write_py("gate.py", r#"print(json.dumps({"_next": "finish"}))"#);
        h.ws.write_py("finish.py", r#"print(json.dumps({"output": "ok"}))"#);
        let (registry, assignments) = h.provision("gate", 2);
        let peers = (Arc::clone(&registry), assignments[0].clone());
        let mut state = item_state(json!(0));
        let mut ctx = silent_ctx();

        h.run("gate", Some(&peers), &mut state, &mut ctx)
            .await
            .unwrap_or_else(|e| panic!("chain failed: {e:#}"));

        assert_eq!(state.state().get("output"), Some(&json!("ok")));
        assert!(registry.is_finished(&assignments[0].0));
        assert!(!registry.is_finished(&assignments[1].0));
        // Fork-time arming would have left these Some: script steps never hold the identity.
        assert!(ctx.peer_registry.is_none() && ctx.peer_assignment.is_none());
    }

    #[tokio::test]
    async fn run_item_chain_marks_identity_finished_when_agent_step_errors() {
        let h = Harness::new(FLAGGED_WORKER_GRAPH);
        let (registry, assignments) = h.provision("worker", 2);
        let peers = (Arc::clone(&registry), assignments[0].clone());
        let mut state = item_state(json!(0));
        let mut ctx = silent_ctx();

        let chain = unwrap_err_chain(h.run("worker", Some(&peers), &mut state, &mut ctx).await);

        assert!(
            chain.contains("failed at node 'worker' (step 1)"),
            "{chain}"
        );
        assert!(chain.contains("Agent 'no-such-agent' failed"), "{chain}");
        assert!(registry.is_finished(&assignments[0].0));
        assert!(ctx.peer_registry.is_none() && ctx.peer_assignment.is_none());
    }

    fn flagged_probe_graph(timeout: Option<u64>) -> String {
        let timeout_line = timeout
            .map(|t| format!("    timeout: {t}\n"))
            .unwrap_or_default();
        format!(
            r#"
name: t
start: worker
nodes:
  worker:
    type: agent
    agent: {PROBE_AGENT}
    prompt: "p"
    teammates: true
    state_updates:
      output: "{{{{output}}}}"
{timeout_line}"#
        )
    }

    /// Control for the timeout test below: the same materialized agent, with
    /// the default timeout and no hold, runs its own graph to completion
    /// offline and hands its output back through the chain. That pins the
    /// recipe, so a `timed out` error in the sibling test cannot be an init
    /// failure in disguise.
    #[tokio::test]
    #[serial]
    async fn run_item_chain_probe_agent_runs_its_graph_offline() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let _guard = TestConfigDirGuard::new();
        materialize_probe_agent(0.0);
        let h = Harness::new(&flagged_probe_graph(None));
        let peers = manual_peer("worker");
        let mut state = item_state(json!(0));
        let mut ctx = silent_ctx();

        h.run("worker", Some(&peers), &mut state, &mut ctx)
            .await
            .unwrap_or_else(|e| panic!("chain failed: {e:#}"));

        assert_eq!(state.state().get("output"), Some(&json!("held")));
        assert!(peers.0.is_finished(&peers.1.0));
        assert!(ctx.peer_registry.is_none() && ctx.peer_assignment.is_none());
    }

    /// `tokio::time::timeout` polls the agent future first and a zero-duration
    /// sleep still goes through the time driver, so `Elapsed` only fires if the
    /// agent future is still pending when the timer is next polled. The
    /// materialized agent's graph holds its script step open for seconds, so
    /// the future is dropped mid-flight and the chain runner alone retires
    /// the identity.
    #[tokio::test]
    #[serial]
    async fn run_item_chain_marks_identity_finished_when_agent_step_times_out() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let _guard = TestConfigDirGuard::new();
        materialize_probe_agent(5.0);
        let h = Harness::new(&flagged_probe_graph(Some(0)));
        let (registry, assignments) = h.provision("worker", 2);
        let peers = (Arc::clone(&registry), assignments[0].clone());
        let mut state = item_state(json!(0));
        let mut ctx = silent_ctx();

        let chain = unwrap_err_chain(h.run("worker", Some(&peers), &mut state, &mut ctx).await);

        assert!(
            chain.contains("failed at node 'worker' (step 1)"),
            "{chain}"
        );
        assert!(
            chain.contains(&format!("Agent '{PROBE_AGENT}' timed out after 0s")),
            "{chain}"
        );
        assert!(
            !chain.contains(&format!("Agent '{PROBE_AGENT}' failed")),
            "{chain}"
        );
        assert!(registry.is_finished(&assignments[0].0));
        assert!(!registry.is_finished(&assignments[1].0));
        assert!(ctx.peer_registry.is_none() && ctx.peer_assignment.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn run_item_chain_clears_peer_fields_after_flagged_agent_then_script() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let _guard = TestConfigDirGuard::new();
        materialize_probe_agent(0.0);
        let h = Harness::new(&format!(
            r#"
name: t
start: worker
nodes:
  worker:
    type: agent
    agent: {PROBE_AGENT}
    prompt: "p"
    teammates: true
    state_updates:
      output: "{{{{output}}}}"
    next: finish
  finish:
    type: script
    script: finish.py
"#
        ));
        h.ws.write_py(
            "finish.py",
            r#"print(json.dumps({"output": state["output"] + "+finished"}))"#,
        );
        let peers = manual_peer("worker");
        let mut state = item_state(json!(0));
        let mut ctx = silent_ctx();

        h.run("worker", Some(&peers), &mut state, &mut ctx)
            .await
            .unwrap_or_else(|e| panic!("chain failed: {e:#}"));

        assert_eq!(state.state().get("output"), Some(&json!("held+finished")));
        assert!(peers.0.is_finished(&peers.1.0));
        assert!(ctx.peer_registry.is_none() && ctx.peer_assignment.is_none());
    }

    /// The prompt fails to interpolate before `run_agent_for_graph` can take
    /// the peer fields, so only the chain runner's reset can clear them.
    #[tokio::test]
    async fn run_item_chain_clears_peer_fields_when_agent_step_fails_before_takeover() {
        let h = Harness::new(
            r#"
name: t
start: worker
nodes:
  worker:
    type: agent
    agent: no-such-agent
    prompt: "{{nope}}"
    teammates: true
"#,
        );
        let peers = manual_peer("worker");
        let mut state = item_state(json!(0));
        let mut ctx = silent_ctx();

        let chain = unwrap_err_chain(h.run("worker", Some(&peers), &mut state, &mut ctx).await);

        assert!(
            chain.contains("Failed to interpolate prompt for agent 'no-such-agent'"),
            "{chain}"
        );
        assert!(chain.contains("'nope' not found in state"), "{chain}");
        assert!(ctx.peer_registry.is_none() && ctx.peer_assignment.is_none());
        assert!(peers.0.is_finished(&peers.1.0));
    }

    #[tokio::test]
    async fn run_item_chain_reaches_flagged_agent_after_script_step() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let h = Harness::new(
            r#"
name: t
start: prep
nodes:
  prep:
    type: script
    script: prep.py
    next: worker
  worker:
    type: agent
    agent: no-such-agent
    prompt: "p"
    teammates: true
"#,
        );
        h.ws.write_py("prep.py", r#"print(json.dumps({"draft": "x"}))"#);
        let (registry, assignments) = h.provision("prep", 2);
        let peers = (Arc::clone(&registry), assignments[0].clone());
        let mut state = item_state(json!(0));
        let mut ctx = silent_ctx();

        let chain = unwrap_err_chain(h.run("prep", Some(&peers), &mut state, &mut ctx).await);

        assert!(
            chain.contains("failed at node 'worker' (step 2)"),
            "{chain}"
        );
        assert_eq!(state.state().get("draft"), Some(&json!("x")));
        assert!(registry.is_finished(&assignments[0].0));
    }

    #[tokio::test]
    async fn run_item_chain_never_arms_unflagged_agent() {
        let h = Harness::new(
            r#"
name: t
start: worker
nodes:
  worker:
    type: agent
    agent: no-such-agent
    prompt: "p"
    teammates: false
"#,
        );
        let peers = manual_peer("worker");
        let mut state = item_state(json!(0));
        let mut ctx = silent_ctx();

        let chain = unwrap_err_chain(h.run("worker", Some(&peers), &mut state, &mut ctx).await);

        assert!(chain.contains("Agent 'no-such-agent' failed"), "{chain}");
        assert!(peers.0.is_finished(&peers.1.0));
    }

    #[test]
    fn wants_peer_identity_only_for_flagged_agent_nodes() {
        let graph: Graph = serde_yaml::from_str(
            r#"
name: t
start: s
nodes:
  s:
    type: script
    script: s.py
    next: l
  l:
    type: llm
    prompt: "p"
    next: plain
  plain:
    type: agent
    agent: a
    prompt: "p"
    next: flagged
  flagged:
    type: agent
    agent: a
    prompt: "p"
    teammates: true
    next: e
  e:
    type: end
    output: ""
"#,
        )
        .unwrap();
        let node = |id: &str| graph.get_node(id).unwrap();

        assert!(!wants_peer_identity(node("s")));
        assert!(!wants_peer_identity(node("l")));
        assert!(!wants_peer_identity(node("plain")));
        assert!(!wants_peer_identity(node("e")));
        assert!(wants_peer_identity(node("flagged")));
    }

    #[tokio::test]
    async fn run_item_chain_llm_step_leaves_ctx_role_and_scopes_unchanged() {
        if !cmd_available("python3") {
            eprintln!("skipping: python3 not available");
            return;
        }
        let h = Harness::new(
            r#"
name: t
start: check
nodes:
  check:
    type: llm
    prompt: "hi"
    fallback: finish
  finish:
    type: script
    script: finish.py
"#,
        );
        h.ws.write_py("finish.py", r#"print(json.dumps({"output": "ok"}))"#);
        let mut state = item_state(json!(0));
        let mut ctx = silent_ctx();
        ctx.role = Some(Role::new("marker", "x"));
        ctx.node_job_scope = Some(vec!["j".to_string()]);
        let mcp_tools = Some(("n".to_string(), IndexMap::new()));
        ctx.active_node_mcp_tools = mcp_tools.clone();

        h.run("check", None, &mut state, &mut ctx)
            .await
            .unwrap_or_else(|e| panic!("chain failed: {e:#}"));

        assert_eq!(state.state().get("output"), Some(&json!("ok")));
        assert_eq!(ctx.role.as_ref().map(Role::name), Some("marker"));
        assert_eq!(ctx.node_job_scope, Some(vec!["j".to_string()]));
        assert_eq!(ctx.active_node_mcp_tools, mcp_tools);
    }

    #[test]
    fn provision_map_peers_flagged_agent_anywhere_in_subgraph() {
        let h = Harness::new(GATE_WORKER_FINISH_GRAPH);

        let (registry, assignments) = h.provision("gate", 2);

        assert_eq!(assignments.len(), 2);
        let roster = registry.roster();
        assert_eq!(roster[0].1, "gate[0]");
        assert_eq!(roster[1].1, "gate[1]");
        for (id, _) in &assignments {
            assert!(id.starts_with("graph_agent_no-such-agent_"), "{id}");
        }
    }

    #[test]
    fn provision_map_peers_picks_flagged_agent_nearest_to_entry_not_yaml_order() {
        let h = Harness::new(
            r#"
name: t
start: gate
nodes:
  far:
    type: agent
    agent: far-agent
    prompt: "p"
    teammates: true
  gate:
    type: script
    script: gate.py
    next: near
  near:
    type: agent
    agent: near-agent
    prompt: "p"
    teammates: true
    next: far
"#,
        );

        let (_, assignments) = h.provision("gate", 2);

        for (id, _) in &assignments {
            assert!(id.starts_with("graph_agent_near-agent_"), "{id}");
        }
    }

    #[test]
    fn provision_map_peers_ignores_flagged_agent_outside_subgraph() {
        let h = Harness::new(GATE_WORKER_FINISH_GRAPH);
        let subgraph = HashSet::from(["gate".to_string()]);

        assert!(provision_map_peers(&h.graph, &subgraph, "gate", 2).is_none());
    }
}
