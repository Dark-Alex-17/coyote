use super::state::template_root_keys;
use super::types::{Graph, Node, NodeType};
use crate::client::{Model, ModelType};
use crate::config::{Agent, AppConfig, paths};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ValidationError {
    pub node_id: Option<String>,
    pub message: String,
}

impl ValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            node_id: None,
            message: message.into(),
        }
    }

    fn with_node(node_id: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            node_id: Some(node_id.into()),
            message: message.into(),
        }
    }
}

#[derive(Debug, Default)]
pub struct ValidationResult {
    pub errors: Vec<ValidationError>,
    pub warnings: Vec<ValidationError>,
}

impl ValidationResult {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    fn error(&mut self, e: ValidationError) {
        self.errors.push(e);
    }

    fn warning(&mut self, w: ValidationError) {
        self.warnings.push(w);
    }

    pub fn into_result(self) -> Result<()> {
        if self.is_valid() {
            return Ok(());
        }
        let lines: Vec<String> = self
            .errors
            .iter()
            .map(|e| match &e.node_id {
                Some(id) => format!("  [{id}] {}", e.message),
                None => format!("  {}", e.message),
            })
            .collect();

        bail!(
            "Graph validation failed with {} error(s):\n{}",
            self.errors.len(),
            lines.join("\n")
        );
    }
}

pub struct AgentValidationContext {
    pub tool_names: HashSet<String>,
    pub mcp_servers: HashSet<String>,
    pub app_config: Arc<AppConfig>,
}

impl AgentValidationContext {
    pub fn from_agent(agent: &Agent, app_config: Arc<AppConfig>) -> Self {
        Self {
            tool_names: agent
                .functions()
                .declarations()
                .iter()
                .map(|d| d.name.clone())
                .collect(),
            mcp_servers: agent.mcp_server_names().iter().cloned().collect(),
            app_config,
        }
    }
}

pub struct GraphValidator {
    base_dir: PathBuf,
    agent_ctx: Option<AgentValidationContext>,
    skill_exists: fn(&str) -> bool,
}

impl GraphValidator {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            agent_ctx: None,
            skill_exists: paths::has_skill,
        }
    }

    pub fn with_agent_context(mut self, ctx: AgentValidationContext) -> Self {
        self.agent_ctx = Some(ctx);
        self
    }

    #[cfg(test)]
    pub fn with_skill_exists(mut self, f: fn(&str) -> bool) -> Self {
        self.skill_exists = f;
        self
    }

    pub fn validate(&self, graph: &Graph) -> ValidationResult {
        let mut result = ValidationResult::default();
        self.validate_node_references(graph, &mut result);
        self.validate_cycles(graph, &mut result);
        self.validate_reachability(graph, &mut result);
        self.validate_terminal_nodes(graph, &mut result);
        self.validate_scripts(graph, &mut result);
        self.validate_agents(graph, &mut result);
        self.validate_approval_routes(graph, &mut result);
        self.validate_rag_nodes(graph, &mut result);
        self.validate_llm_nodes(graph, &mut result);
        self.validate_llm_skills(graph, &mut result);
        self.validate_max_concurrency(graph, &mut result);
        self.validate_map_branches(graph, &mut result);
        self.validate_parallel_user_interaction(graph, &mut result);
        self.validate_parallel_writes(graph, &mut result);
        self.validate_parallel_reads(graph, &mut result);

        result
    }

    fn validate_rag_nodes(&self, graph: &Graph, result: &mut ValidationResult) {
        for (node_id, node) in &graph.nodes {
            if let NodeType::Rag(r) = &node.node_type {
                if r.documents.is_empty() {
                    result.error(ValidationError::with_node(
                        node_id,
                        "RAG node has no 'documents'; at least one knowledge source \
                         is required",
                    ));
                }
                if r.state_updates.is_none() {
                    result.warning(ValidationError::with_node(
                        node_id,
                        "RAG node has no 'state_updates'; its retrieval result will \
                         not be written to state",
                    ));
                }
            }
        }
    }

    fn validate_llm_nodes(&self, graph: &Graph, result: &mut ValidationResult) {
        let Some(ctx) = &self.agent_ctx else {
            return;
        };

        for (node_id, node) in &graph.nodes {
            let NodeType::Llm(llm) = &node.node_type else {
                continue;
            };

            if let Some(tools) = &llm.tools {
                for entry in tools {
                    if let Some(server) = entry.strip_prefix("mcp:") {
                        if !ctx.mcp_servers.contains(server) {
                            result.error(ValidationError::with_node(
                                node_id,
                                format!("llm node references unknown MCP server 'mcp:{server}'"),
                            ));
                        }
                    } else if !ctx.tool_names.contains(entry) {
                        result.error(ValidationError::with_node(
                            node_id,
                            format!("llm node references unknown tool '{entry}'"),
                        ));
                    }
                }
            }

            if let Some(model_id) = &llm.model
                && Model::retrieve_model(ctx.app_config.as_ref(), model_id, ModelType::Chat)
                    .is_err()
            {
                result.error(ValidationError::with_node(
                    node_id,
                    format!("llm node references unknown model '{model_id}'"),
                ));
            }
        }
    }

    fn validate_llm_skills(&self, graph: &Graph, result: &mut ValidationResult) {
        let visible_skills = self
            .agent_ctx
            .as_ref()
            .and_then(|c| c.app_config.visible_skills.as_deref());

        let skill_exists = self.skill_exists;
        let has_agent_ctx = self.agent_ctx.is_some();
        let check_visibility = |name: &str| -> Option<String> {
            if !has_agent_ctx {
                return None;
            }

            match visible_skills {
                Some(list) if !list.iter().any(|s| s == name) => Some(format!(
                    "'{name}' is not in the global 'visible_skills' allow-list"
                )),
                None if !skill_exists(name) => Some(format!("'{name}' is not installed")),
                _ => None,
            }
        };

        if let Some(graph_skills) = &graph.enabled_skills {
            for name in graph_skills {
                if name.trim().is_empty() {
                    result.error(ValidationError::new(
                        "graph 'enabled_skills' contains an empty skill name",
                    ));
                    continue;
                }
                if let Some(reason) = check_visibility(name) {
                    result.error(ValidationError::new(format!(
                        "graph 'enabled_skills': {reason}"
                    )));
                }
            }
        }

        for (node_id, node) in &graph.nodes {
            let NodeType::Llm(llm) = &node.node_type else {
                continue;
            };
            let Some(node_skills) = &llm.enabled_skills else {
                continue;
            };

            for name in node_skills {
                if name.trim().is_empty() {
                    result.error(ValidationError::with_node(
                        node_id,
                        "llm node 'enabled_skills' contains an empty skill name",
                    ));
                    continue;
                }
                if let Some(reason) = check_visibility(name) {
                    result.error(ValidationError::with_node(
                        node_id,
                        format!("llm node 'enabled_skills': {reason}"),
                    ));
                    continue;
                }

                if let Some(graph_skills) = &graph.enabled_skills
                    && !graph_skills.iter().any(|g| g == name)
                {
                    result.error(ValidationError::with_node(
                        node_id,
                        format!(
                            "llm node 'enabled_skills' references '{name}' which is not in \
                             graph-level 'enabled_skills' ({})",
                            graph_skills.join(", ")
                        ),
                    ));
                }
            }
        }
    }

    fn validate_node_references(&self, graph: &Graph, result: &mut ValidationResult) {
        for (node_id, node) in &graph.nodes {
            for (target, label) in declared_targets(node) {
                if !graph.has_node(&target) {
                    result.error(ValidationError::with_node(
                        node_id,
                        format!("References non-existent node '{target}' in {label}"),
                    ));
                }
            }
        }
    }

    fn validate_cycles(&self, graph: &Graph, result: &mut ValidationResult) {
        let mut visited: HashSet<String> = HashSet::new();
        let mut rec_stack: HashSet<String> = HashSet::new();
        let mut path: Vec<String> = Vec::new();

        for node_id in graph.node_ids() {
            if !visited.contains(node_id)
                && let Some(cycle) =
                    detect_cycle_dfs(graph, node_id, &mut visited, &mut rec_stack, &mut path)
            {
                result.error(ValidationError::new(format!(
                    "Cycle detected: {}",
                    cycle.join(" -> ")
                )));
                return;
            }
        }
    }

    fn validate_reachability(&self, graph: &Graph, result: &mut ValidationResult) {
        let reachable = find_reachable_nodes(graph);
        for node_id in graph.node_ids() {
            if !reachable.contains(node_id) {
                result.warning(ValidationError::with_node(
                    node_id,
                    "Node is unreachable from the start node via declared edges \
                     (script `_next` routing is not analyzed)",
                ));
            }
        }
    }

    fn validate_terminal_nodes(&self, graph: &Graph, result: &mut ValidationResult) {
        let has_any_end = graph
            .nodes
            .values()
            .any(|n| matches!(n.node_type, NodeType::End(_)));

        if !has_any_end {
            result.error(ValidationError::new(
                "Graph has no end nodes; execution would never terminate",
            ));
            return;
        }

        let reachable = find_reachable_nodes(graph);
        let reachable_end = graph
            .nodes
            .iter()
            .any(|(id, n)| matches!(n.node_type, NodeType::End(_)) && reachable.contains(id));
        if !reachable_end {
            result.warning(ValidationError::new(
                "No end node is reachable from the start node via declared edges \
                 (a script's `_next` may still route to one)",
            ));
        }
    }

    fn validate_scripts(&self, graph: &Graph, result: &mut ValidationResult) {
        for (node_id, node) in &graph.nodes {
            if let NodeType::Script(s) = &node.node_type {
                let script_path = self.base_dir.join(&s.script);
                if !script_path.exists() {
                    result.error(ValidationError::with_node(
                        node_id,
                        format!("Script file not found: '{}'", script_path.display()),
                    ));
                }
            }
        }
    }

    fn validate_agents(&self, graph: &Graph, result: &mut ValidationResult) {
        for (node_id, node) in &graph.nodes {
            if let NodeType::Agent(a) = &node.node_type {
                let agent_dir = paths::agent_data_dir(&a.agent);
                let has_config = paths::agent_config_file(&a.agent).exists();
                let has_graph = paths::agent_graph_file(&a.agent).exists();
                if !agent_dir.exists() {
                    result.error(ValidationError::with_node(
                        node_id,
                        format!("Agent '{}' not found (directory missing)", a.agent),
                    ));
                } else if !has_config && !has_graph {
                    result.error(ValidationError::with_node(
                        node_id,
                        format!(
                            "Agent '{}' has neither a config.yaml nor a graph.yaml",
                            a.agent
                        ),
                    ));
                }
            }
        }
    }

    fn validate_approval_routes(&self, graph: &Graph, result: &mut ValidationResult) {
        for (node_id, node) in &graph.nodes {
            if let NodeType::Approval(a) = &node.node_type {
                for option in &a.options {
                    if !a.routes.contains_key(option) {
                        result.error(ValidationError::with_node(
                            node_id,
                            format!("Approval option '{option}' has no route defined"),
                        ));
                    }
                }
                for key in a.routes.keys() {
                    if !a.options.contains(key) {
                        result.warning(ValidationError::with_node(
                            node_id,
                            format!("Route '{key}' has no corresponding option"),
                        ));
                    }
                }
            }
        }
    }

    // Parallel-execution validation.
    //
    // The v1 algorithm uses immediate-successor analysis only: a parallel group is the set of `next:` targets of a
    // single fan-out node. Map nodes are checked separately by `validate_map_branches` (the branch is self-parallel,
    // but enforcement comes from strict-mode rules on the branch node, not from group membership). Transitive parallel
    // groups (deeper fan-out chains) are a v2 enhancement; v1 over-reports rather than under-reports. A false positive
    // forces an unneeded reducer (mild annoyance); a false negative allows silent data races (catastrophic).
    fn validate_max_concurrency(&self, graph: &Graph, result: &mut ValidationResult) {
        if graph.settings.max_concurrency == 0 {
            result.error(ValidationError::new(
                "settings.max_concurrency must be >= 1 (got 0); a zero cap would \
                 deadlock the executor",
            ));
        }

        for (node_id, node) in &graph.nodes {
            if let NodeType::Map(m) = &node.node_type
                && let Some(0) = m.max_concurrency
            {
                result.error(ValidationError::with_node(
                    node_id,
                    "map node's `max_concurrency` must be >= 1 (got 0); a zero cap \
                     would deadlock the executor",
                ));
            }
        }
    }

    fn validate_map_branches(&self, graph: &Graph, result: &mut ValidationResult) {
        for (map_id, node) in &graph.nodes {
            let NodeType::Map(m) = &node.node_type else {
                continue;
            };
            let Some(branch) = graph.get_node(&m.branch) else {
                continue;
            };

            match &branch.node_type {
                NodeType::Approval(_) => {
                    result.error(ValidationError::with_node(
                        map_id,
                        format!(
                            "map node points to branch '{}' which is an approval node; \
                             approval/input nodes cannot run inside a parallel map branch \
                             (the CLI would prompt the user N times concurrently)",
                            m.branch
                        ),
                    ));
                    continue;
                }
                NodeType::Input(_) => {
                    result.error(ValidationError::with_node(
                        map_id,
                        format!(
                            "map node points to branch '{}' which is an input node; \
                             input nodes cannot run inside a parallel map branch",
                            m.branch
                        ),
                    ));
                    continue;
                }
                NodeType::End(_) => {
                    result.error(ValidationError::with_node(
                        map_id,
                        format!(
                            "map node points to branch '{}' which is an end node; \
                             map branches terminate via the map's collect mechanism, \
                             not via end nodes",
                            m.branch
                        ),
                    ));
                    continue;
                }
                NodeType::Map(_) => {
                    result.error(ValidationError::with_node(
                        map_id,
                        format!(
                            "map node points to branch '{}' which is itself a map node; \
                             nested map fan-outs are not supported in v1",
                            m.branch
                        ),
                    ));
                    continue;
                }
                _ => {}
            }

            if branch.next.is_some() {
                result.error(ValidationError::with_node(
                    m.branch.clone(),
                    format!(
                        "branch node '{}' has a `next` declared, but map branches must be \
                         atomic (one node, one execution per item). Remove `next` or \
                         restructure the workflow so any chaining happens after the map.",
                        m.branch
                    ),
                ));
            }

            if let Some(updates) = node_state_updates_keys(branch) {
                for k in &updates {
                    if k != &m.output_key {
                        result.error(ValidationError::with_node(
                            m.branch.clone(),
                            format!(
                                "branch node '{}' writes state key '{}' via state_updates, \
                                 but map branches may only write through their `output_key` \
                                 ('{}'). Rename the write, or move the side effect outside \
                                 the map.",
                                m.branch, k, m.output_key
                            ),
                        ));
                    }
                }
            }

            let schema_keys = output_schema_top_level_keys(branch);
            if !schema_keys.is_empty() {
                let mut keys_sorted: Vec<String> = schema_keys.into_iter().collect();
                keys_sorted.sort();
                result.error(ValidationError::with_node(
                    m.branch.clone(),
                    format!(
                        "branch node '{}' has an `output_schema` with top-level \
                         properties ({}); map branches must write only through their \
                         `output_key` ('{}'). Remove `output_schema`, or use state_updates \
                         to map the output explicitly.",
                        m.branch,
                        keys_sorted.join(", "),
                        m.output_key
                    ),
                ));
            }
        }
    }

    fn validate_parallel_user_interaction(&self, graph: &Graph, result: &mut ValidationResult) {
        for group in compute_parallel_groups(graph) {
            for node_id in &group {
                let Some(node) = graph.get_node(node_id) else {
                    continue;
                };
                match &node.node_type {
                    NodeType::Approval(_) => {
                        result.error(ValidationError::with_node(
                            node_id,
                            "approval node is an immediate target of a fan-out \
                             (`next: [...]`); approvals must run after the join, \
                             not inside a parallel branch",
                        ));
                    }
                    NodeType::Input(_) => {
                        result.error(ValidationError::with_node(
                            node_id,
                            "input node is an immediate target of a fan-out \
                             (`next: [...]`); input nodes must run after the join, \
                             not inside a parallel branch",
                        ));
                    }
                    _ => {}
                }
            }
        }
    }

    fn validate_parallel_writes(&self, graph: &Graph, result: &mut ValidationResult) {
        for group in compute_parallel_groups(graph) {
            let mut node_writes: Vec<(String, HashSet<String>)> = Vec::new();
            for node_id in &group {
                let Some(node) = graph.get_node(node_id) else {
                    continue;
                };
                match write_set_of(node) {
                    Some(set) => node_writes.push((node_id.clone(), set)),
                    None => {
                        result.error(ValidationError::with_node(
                            node_id,
                            "script node is in a parallel branch but declares no \
                             `state_updates`; parallel scripts must declare their writes \
                             explicitly to avoid silent state collisions",
                        ));
                    }
                }
            }

            let mut writers_by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (nid, ws) in &node_writes {
                for k in ws {
                    writers_by_key
                        .entry(k.clone())
                        .or_default()
                        .push(nid.clone());
                }
            }

            for (key, mut writers) in writers_by_key {
                if writers.len() < 2 {
                    continue;
                }
                if graph.reducers.contains_key(&key) {
                    continue;
                }
                writers.sort();
                result.error(ValidationError::new(format!(
                    "nodes [{}] all write key '{}' in the same parallel super-step but \
                     no reducer is declared for '{}'. Add `reducers: {{ {}: <reducer> }}` \
                     at the graph root (built-ins: append, extend, concat, sum, max, min, \
                     merge, overwrite), or rename one node's output.",
                    writers.join(", "),
                    key,
                    key,
                    key,
                )));
            }
        }
    }

    fn validate_parallel_reads(&self, graph: &Graph, result: &mut ValidationResult) {
        for group in compute_parallel_groups(graph) {
            let nodes: Vec<(&String, &Node)> = group
                .iter()
                .filter_map(|id| graph.nodes.get(id).map(|n| (id, n)))
                .collect();

            for (id_a, node_a) in &nodes {
                let read_set_a = read_set_of(node_a);
                if read_set_a.is_empty() {
                    continue;
                }
                for (id_b, node_b) in &nodes {
                    if id_b == id_a {
                        continue;
                    }
                    let Some(write_set_b) = write_set_of(node_b) else {
                        continue;
                    };
                    let mut collisions: Vec<String> =
                        read_set_a.intersection(&write_set_b).cloned().collect();
                    if collisions.is_empty() {
                        continue;
                    }
                    collisions.sort();
                    let keys = collisions
                        .iter()
                        .map(|k| format!("`{k}`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    result.error(ValidationError::with_node(
                        id_a.as_str(),
                        format!(
                            "node '{id_a}' reads state key(s) {keys} which sibling parallel \
                             branch '{id_b}' writes in the same super-step; parallel branches \
                             see a state snapshot taken BEFORE the super-step and cannot observe \
                             each other's writes. Move the dependent read to a later super-step \
                             (or remove the cross-branch reference)."
                        ),
                    ));
                }
            }
        }
    }
}

fn declared_targets(node: &Node) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    if let Some(targets) = &node.next {
        for target in targets.as_slice() {
            out.push((target.clone(), "'next'"));
        }
    }

    match &node.node_type {
        NodeType::Approval(a) => {
            for v in a.routes.values() {
                out.push((v.clone(), "approval 'routes'"));
            }
            out.push((a.on_other.clone(), "approval 'on_other'"));
        }
        NodeType::Script(s) => {
            if let Some(t) = &s.fallback {
                out.push((t.clone(), "script 'fallback'"));
            }
        }
        NodeType::Llm(l) => {
            if let Some(t) = &l.fallback {
                out.push((t.clone(), "llm 'fallback'"));
            }
        }
        NodeType::Map(m) => {
            out.push((m.branch.clone(), "map 'branch'"));
        }
        // `agent`/`input`/`rag` route only via `next` (already collected
        // above); `end` is terminal. No type-specific routing edges to add.
        NodeType::Agent(_) | NodeType::Input(_) | NodeType::Rag(_) | NodeType::End(_) => {}
    }
    out
}

fn outgoing_node_ids(node: &Node) -> Vec<String> {
    declared_targets(node).into_iter().map(|(t, _)| t).collect()
}

fn find_reachable_nodes(graph: &Graph) -> HashSet<String> {
    let mut reachable: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    if !graph.has_node(&graph.start) {
        return reachable;
    }

    reachable.insert(graph.start.clone());
    queue.push_back(graph.start.clone());

    while let Some(id) = queue.pop_front() {
        if let Some(node) = graph.get_node(&id) {
            for next in outgoing_node_ids(node) {
                if graph.has_node(&next) && reachable.insert(next.clone()) {
                    queue.push_back(next);
                }
            }
        }
    }
    reachable
}

// v1 parallel-group detection: only the immediate `next` targets of a fan-out node count as a parallel group. Map
// branches are handled separately by `validate_map_branches` (the branch's self-parallelism is checked via strict-mode
// rules on the branch node itself, not via group membership).
//
// Returns one HashSet per fan-out source; deeper transitive parallelism is intentionally out of scope for v1.
fn compute_parallel_groups(graph: &Graph) -> Vec<HashSet<String>> {
    let mut groups = Vec::new();
    for node in graph.nodes.values() {
        if let Some(targets) = &node.next
            && targets.is_fan_out()
        {
            groups.push(targets.as_slice().iter().cloned().collect());
        }
    }
    groups
}

// Computes the set of state keys this node can write to.
//
// Sources considered:
//   - `state_updates` keys (every node type that has them)
//   - `output_schema` top-level `properties` for `llm` and `agent` (auto-merge)
fn write_set_of(node: &Node) -> Option<HashSet<String>> {
    if matches!(node.node_type, NodeType::Script(_)) && node_state_updates_keys(node).is_none() {
        return None;
    }

    let mut writes = HashSet::new();
    if let Some(keys) = node_state_updates_keys(node) {
        writes.extend(keys);
    }

    writes.extend(output_schema_top_level_keys(node));

    Some(writes)
}

// Computes the set of root state keys this node's templated fields read from.
//
// "Root key" follows the same definition as `template_root_keys`: for a
// reference like `{{user.name}}` or `{{items[0]}}`, the root is the bare
// identifier before the first `.` or `[`.
fn read_set_of(node: &Node) -> HashSet<String> {
    let mut reads: HashSet<String> = HashSet::new();
    let scoped: &[&str] = match &node.node_type {
        NodeType::Llm(_) | NodeType::Agent(_) | NodeType::Rag(_) => &["output"],
        NodeType::Approval(_) => &["choice"],
        NodeType::Input(_) => &["input"],
        NodeType::Script(_) | NodeType::End(_) | NodeType::Map(_) => &[],
    };

    for s in primary_templated_fields(node) {
        for k in template_root_keys(&s) {
            reads.insert(k);
        }
    }

    if let Some(updates) = node_state_updates_map(node) {
        for v in updates.values() {
            for k in template_root_keys(v) {
                if !scoped.contains(&k.as_str()) {
                    reads.insert(k);
                }
            }
        }
    }

    reads
}

fn primary_templated_fields(node: &Node) -> Vec<String> {
    match &node.node_type {
        NodeType::Llm(n) => {
            let mut v = vec![n.prompt.clone()];
            if let Some(i) = &n.instructions {
                v.push(i.clone());
            }
            v
        }
        NodeType::Agent(n) => vec![n.prompt.clone()],
        NodeType::Rag(n) => {
            vec![
                n.query
                    .clone()
                    .unwrap_or_else(|| "{{initial_prompt}}".to_string()),
            ]
        }
        NodeType::Approval(n) => vec![n.question.clone()],
        NodeType::Input(n) => {
            let mut v = vec![n.question.clone()];
            if let Some(d) = &n.default {
                v.push(d.clone());
            }
            v
        }
        NodeType::End(n) => vec![n.output.clone()],
        NodeType::Map(n) => vec![n.over.clone()],
        NodeType::Script(_) => Vec::new(),
    }
}

fn node_state_updates_map(node: &Node) -> Option<&std::collections::HashMap<String, String>> {
    match &node.node_type {
        NodeType::Llm(n) => n.state_updates.as_ref(),
        NodeType::Agent(n) => n.state_updates.as_ref(),
        NodeType::Rag(n) => n.state_updates.as_ref(),
        NodeType::Approval(n) => n.state_updates.as_ref(),
        NodeType::Input(n) => n.state_updates.as_ref(),
        NodeType::Script(n) => n.state_updates.as_ref(),
        NodeType::End(n) => n.state_updates.as_ref(),
        NodeType::Map(_) => None,
    }
}

fn node_state_updates_keys(node: &Node) -> Option<HashSet<String>> {
    let updates = match &node.node_type {
        NodeType::Agent(n) => n.state_updates.as_ref(),
        NodeType::Script(n) => n.state_updates.as_ref(),
        NodeType::Approval(n) => n.state_updates.as_ref(),
        NodeType::Input(n) => n.state_updates.as_ref(),
        NodeType::Llm(n) => n.state_updates.as_ref(),
        NodeType::Rag(n) => n.state_updates.as_ref(),
        NodeType::End(n) => n.state_updates.as_ref(),
        NodeType::Map(_) => return None,
    };
    updates.map(|m| m.keys().cloned().collect())
}

fn output_schema_top_level_keys(node: &Node) -> HashSet<String> {
    let schema = match &node.node_type {
        NodeType::Agent(n) => n.output_schema.as_ref(),
        NodeType::Llm(n) => n.output_schema.as_ref(),
        _ => return HashSet::new(),
    };
    let Some(schema) = schema else {
        return HashSet::new();
    };
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return HashSet::new();
    };
    properties.keys().cloned().collect()
}

fn detect_cycle_dfs(
    graph: &Graph,
    node_id: &str,
    visited: &mut HashSet<String>,
    rec_stack: &mut HashSet<String>,
    path: &mut Vec<String>,
) -> Option<Vec<String>> {
    visited.insert(node_id.to_string());
    rec_stack.insert(node_id.to_string());
    path.push(node_id.to_string());

    if let Some(node) = graph.get_node(node_id) {
        for next in outgoing_node_ids(node) {
            if !graph.has_node(&next) {
                continue;
            }

            if !visited.contains(&next) {
                if let Some(cycle) = detect_cycle_dfs(graph, &next, visited, rec_stack, path) {
                    return Some(cycle);
                }
            } else if rec_stack.contains(&next) {
                let start = path.iter().position(|n| n == &next).unwrap_or(0);
                let mut cycle: Vec<String> = path[start..].to_vec();
                cycle.push(next.clone());
                return Some(cycle);
            }
        }
    }

    path.pop();
    rec_stack.remove(node_id);
    None
}

#[cfg(test)]
mod tests {
    use super::super::types::*;
    use super::*;
    use indexmap::IndexMap;
    use std::collections::HashMap;
    use std::env;

    fn graph_with(nodes: Vec<(&str, Node)>, start: &str) -> Graph {
        let mut map: IndexMap<String, Node> = IndexMap::new();
        for (id, node) in nodes {
            map.insert(id.to_string(), node);
        }

        Graph {
            name: "t".into(),
            description: String::new(),
            version: "1.0".into(),
            model: None,
            temperature: None,
            top_p: None,
            global_tools: Vec::new(),
            mcp_servers: Vec::new(),
            skills_enabled: None,
            enabled_skills: None,
            conversation_starters: Vec::new(),
            variables: Vec::new(),
            settings: GraphSettings::default(),
            initial_state: HashMap::new(),
            reducers: HashMap::new(),
            start: start.into(),
            nodes: map,
        }
    }

    fn end_node(id: &str) -> Node {
        Node {
            id: id.into(),
            description: String::new(),
            node_type: NodeType::End(EndNode {
                output: String::new(),
                state_updates: None,
            }),
            next: None,
        }
    }

    fn approval_node(id: &str, options: &[&str], routes: &[(&str, &str)], on_other: &str) -> Node {
        let mut r: HashMap<String, String> = HashMap::new();
        for (k, v) in routes {
            r.insert((*k).into(), (*v).into());
        }

        Node {
            id: id.into(),
            description: String::new(),
            node_type: NodeType::Approval(ApprovalNode {
                question: "?".into(),
                options: options.iter().map(|s| (*s).into()).collect(),
                routes: r,
                on_other: on_other.into(),
                state_updates: None,
            }),
            next: None,
        }
    }

    fn script_node(id: &str, script: &str, fallback: Option<&str>) -> Node {
        Node {
            id: id.into(),
            description: String::new(),
            node_type: NodeType::Script(ScriptNode {
                script: script.into(),
                state_updates: None,
                fallback: fallback.map(String::from),
                timeout: 30,
            }),
            next: None,
        }
    }

    fn rag_node(id: &str, documents: &[&str], with_state_updates: bool) -> Node {
        let state_updates = with_state_updates.then(|| {
            let mut m: HashMap<String, String> = HashMap::new();
            m.insert("ctx".into(), "{{output.context}}".into());
            m
        });

        Node {
            id: id.into(),
            description: String::new(),
            node_type: NodeType::Rag(RagNode {
                documents: documents.iter().map(|s| (*s).into()).collect(),
                query: None,
                top_k: None,
                embedding_model: None,
                chunk_size: None,
                chunk_overlap: None,
                reranker_model: None,
                batch_size: None,
                state_updates,
                timeout: None,
            }),
            next: Some("end".into()),
        }
    }

    fn llm_node(id: &str, fallback: Option<&str>, next: Option<&str>) -> Node {
        Node {
            id: id.into(),
            description: String::new(),
            node_type: NodeType::Llm(LlmNode {
                instructions: None,
                prompt: "p".into(),
                tools: None,
                model: None,
                temperature: None,
                top_p: None,
                fallback: fallback.map(String::from),
                max_attempts: 1,
                max_iterations: 10,
                state_updates: None,
                output_schema: None,
                timeout: None,
                skills_enabled: None,
                enabled_skills: None,
            }),
            next: next.map(NextTargets::from),
        }
    }

    #[test]
    fn flags_missing_llm_fallback_target() {
        let graph = graph_with(
            vec![
                ("l", llm_node("l", Some("ghost"), Some("end"))),
                ("end", end_node("end")),
            ],
            "l",
        );

        let result = validator().validate(&graph);

        assert!(!result.is_valid());
        assert!(result.errors.iter().any(|e| e.message.contains("ghost")));
    }

    #[test]
    fn llm_node_skill_in_graph_set_passes() {
        let mut graph = graph_with(
            vec![
                ("l", llm_node("l", None, Some("end"))),
                ("end", end_node("end")),
            ],
            "l",
        );
        graph.enabled_skills = Some(vec!["code-review".into(), "git-master".into()]);
        if let NodeType::Llm(ref mut n) = graph.nodes.get_mut("l").unwrap().node_type {
            n.enabled_skills = Some(vec!["code-review".into()]);
        }

        let result = validator().validate(&graph);

        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("enabled_skills")),
            "unexpected enabled_skills error: {:?}",
            result.errors
        );
    }

    #[test]
    fn llm_node_skill_not_in_graph_set_errors() {
        let mut graph = graph_with(
            vec![
                ("l", llm_node("l", None, Some("end"))),
                ("end", end_node("end")),
            ],
            "l",
        );
        graph.enabled_skills = Some(vec!["code-review".into()]);
        if let NodeType::Llm(ref mut n) = graph.nodes.get_mut("l").unwrap().node_type {
            n.enabled_skills = Some(vec!["git-master".into()]);
        }

        let result = validator().validate(&graph);

        assert!(!result.is_valid());
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("'git-master'") && e.message.contains("graph-level")),
            "expected git-master subset error, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn llm_node_empty_skill_name_errors() {
        let mut graph = graph_with(
            vec![
                ("l", llm_node("l", None, Some("end"))),
                ("end", end_node("end")),
            ],
            "l",
        );
        graph.enabled_skills = Some(vec!["code-review".into()]);
        if let NodeType::Llm(ref mut n) = graph.nodes.get_mut("l").unwrap().node_type {
            n.enabled_skills = Some(vec!["".into()]);
        }

        let result = validator().validate(&graph);

        assert!(!result.is_valid());
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("empty skill name")),
            "expected empty-skill-name error, got: {:?}",
            result.errors
        );
    }

    #[test]
    fn llm_node_skill_when_no_graph_set_is_permitted_by_validator() {
        let mut graph = graph_with(
            vec![
                ("l", llm_node("l", None, Some("end"))),
                ("end", end_node("end")),
            ],
            "l",
        );
        if let NodeType::Llm(ref mut n) = graph.nodes.get_mut("l").unwrap().node_type {
            n.enabled_skills = Some(vec!["anything".into()]);
        }

        let result = validator().validate(&graph);

        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("enabled_skills")),
            "validator should not block when graph.enabled_skills is None: {:?}",
            result.errors
        );
    }

    fn agent_ctx(tools: &[&str], mcp: &[&str]) -> AgentValidationContext {
        AgentValidationContext {
            tool_names: tools.iter().map(|s| s.to_string()).collect(),
            mcp_servers: mcp.iter().map(|s| s.to_string()).collect(),
            app_config: Arc::new(AppConfig::default()),
        }
    }

    fn llm_node_with(id: &str, tools: Option<Vec<&str>>, model: Option<&str>) -> Node {
        let mut node = llm_node(id, None, Some("end"));
        if let NodeType::Llm(ref mut n) = node.node_type {
            n.tools = tools.map(|t| t.iter().map(|s| s.to_string()).collect());
            n.model = model.map(String::from);
        }

        node
    }

    #[test]
    fn llm_node_unknown_tool_is_an_error() {
        let graph = graph_with(
            vec![
                ("l", llm_node_with("l", Some(vec!["bogus_tool"]), None)),
                ("end", end_node("end")),
            ],
            "l",
        );

        let result = validator()
            .with_agent_context(agent_ctx(&["read_query"], &[]))
            .validate(&graph);

        assert!(!result.is_valid());
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("bogus_tool"))
        );
    }

    #[test]
    fn llm_node_known_tool_passes() {
        let graph = graph_with(
            vec![
                ("l", llm_node_with("l", Some(vec!["read_query"]), None)),
                ("end", end_node("end")),
            ],
            "l",
        );

        let result = validator()
            .with_agent_context(agent_ctx(&["read_query"], &[]))
            .validate(&graph);

        assert!(result.is_valid());
    }

    #[test]
    fn llm_node_unknown_mcp_server_is_an_error() {
        let graph = graph_with(
            vec![
                ("l", llm_node_with("l", Some(vec!["mcp:bogus"]), None)),
                ("end", end_node("end")),
            ],
            "l",
        );

        let result = validator()
            .with_agent_context(agent_ctx(&[], &["pubmed-search"]))
            .validate(&graph);

        assert!(!result.is_valid());
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("mcp:bogus"))
        );
    }

    #[test]
    fn llm_node_known_mcp_server_passes() {
        let graph = graph_with(
            vec![
                (
                    "l",
                    llm_node_with("l", Some(vec!["mcp:pubmed-search"]), None),
                ),
                ("end", end_node("end")),
            ],
            "l",
        );

        let result = validator()
            .with_agent_context(agent_ctx(&[], &["pubmed-search"]))
            .validate(&graph);

        assert!(result.is_valid());
    }

    #[test]
    fn llm_node_unknown_model_is_an_error() {
        let graph = graph_with(
            vec![
                ("l", llm_node_with("l", None, Some("nonexistent:model"))),
                ("end", end_node("end")),
            ],
            "l",
        );

        let result = validator()
            .with_agent_context(agent_ctx(&[], &[]))
            .validate(&graph);

        assert!(!result.is_valid());
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("nonexistent:model"))
        );
    }

    #[test]
    fn llm_node_validation_skipped_without_agent_context() {
        let graph = graph_with(
            vec![
                ("l", llm_node_with("l", Some(vec!["bogus_tool"]), None)),
                ("end", end_node("end")),
            ],
            "l",
        );

        let result = validator().validate(&graph);

        assert!(result.is_valid());
    }

    #[test]
    fn rag_node_without_documents_errors() {
        let graph = graph_with(
            vec![("r", rag_node("r", &[], true)), ("end", end_node("end"))],
            "r",
        );

        let result = validator().validate(&graph);

        assert!(!result.is_valid());
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("no 'documents'") && e.node_id.as_deref() == Some("r"))
        );
    }

    #[test]
    fn rag_node_without_state_updates_warns() {
        let graph = graph_with(
            vec![
                ("r", rag_node("r", &["./docs"], false)),
                ("end", end_node("end")),
            ],
            "r",
        );

        let result = validator().validate(&graph);

        assert!(result.is_valid());
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.message.contains("no 'state_updates'"))
        );
    }

    #[test]
    fn valid_rag_node_produces_no_findings() {
        let graph = graph_with(
            vec![
                ("r", rag_node("r", &["./docs"], true)),
                ("end", end_node("end")),
            ],
            "r",
        );

        let result = validator().validate(&graph);

        assert!(result.is_valid());
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.message.contains("RAG node"))
        );
    }

    fn agent_node(id: &str, agent: &str, next: Option<&str>) -> Node {
        Node {
            id: id.into(),
            description: String::new(),
            node_type: NodeType::Agent(AgentNode {
                agent: agent.into(),
                prompt: "hi".into(),
                state_updates: None,
                output_schema: None,
                timeout: None,
            }),
            next: next.map(NextTargets::from),
        }
    }

    fn validator() -> GraphValidator {
        GraphValidator::new(env::current_dir().unwrap()).with_skill_exists(|_: &str| true)
    }

    #[test]
    fn valid_simple_graph_passes() {
        let mut start = end_node("start");
        start.next = Some("end".into());
        let graph = graph_with(vec![("start", start), ("end", end_node("end"))], "start");

        let result = validator().validate(&graph);

        assert!(result.is_valid(), "errors: {:?}", result.errors);
    }

    #[test]
    fn flags_missing_node_reference_in_next() {
        let mut n = end_node("n1");
        n.next = Some("nope".into());
        let graph = graph_with(vec![("n1", n), ("end", end_node("end"))], "n1");

        let result = validator().validate(&graph);

        assert!(!result.is_valid());
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("non-existent node 'nope'")
                    && e.node_id.as_deref() == Some("n1"))
        );
    }

    #[test]
    fn flags_missing_approval_route_target() {
        let approval = approval_node(
            "ap",
            &["yes", "no"],
            &[("yes", "end"), ("no", "missing")],
            "end",
        );
        let graph = graph_with(vec![("ap", approval), ("end", end_node("end"))], "ap");

        let result = validator().validate(&graph);

        assert!(!result.is_valid());
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("non-existent node 'missing'"))
        );
    }

    #[test]
    fn flags_missing_approval_on_other_target() {
        let approval = approval_node("ap", &["yes"], &[("yes", "end")], "missing");
        let graph = graph_with(vec![("ap", approval), ("end", end_node("end"))], "ap");

        let result = validator().validate(&graph);

        assert!(!result.is_valid());
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("non-existent node 'missing'")
                    && e.message.contains("on_other"))
        );
    }

    #[test]
    fn flags_missing_script_fallback_target() {
        let scr = script_node("s", "does-not-exist.py", Some("nowhere"));
        let graph = graph_with(vec![("s", scr), ("end", end_node("end"))], "s");

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("non-existent node 'nowhere'"))
        );
    }

    #[test]
    fn detects_two_node_cycle() {
        let mut a = end_node("a");
        a.next = Some("b".into());
        let mut b = end_node("b");
        b.next = Some("a".into());
        let graph = graph_with(vec![("a", a), ("b", b)], "a");

        let result = validator().validate(&graph);

        assert!(!result.is_valid());
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("Cycle detected"))
        );
    }

    #[test]
    fn detects_self_loop_as_cycle() {
        let mut a = end_node("a");
        a.next = Some("a".into());
        let graph = graph_with(vec![("a", a)], "a");

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("Cycle detected"))
        );
    }

    #[test]
    fn warns_on_unreachable_node() {
        let graph = graph_with(
            vec![("start", end_node("start")), ("orphan", end_node("orphan"))],
            "start",
        );

        let result = validator().validate(&graph);

        assert!(
            result.warnings.iter().any(
                |w| w.node_id.as_deref() == Some("orphan") && w.message.contains("unreachable")
            )
        );
    }

    #[test]
    fn errors_when_graph_has_no_end_node_at_all() {
        let mut a = agent_node("a", "__no_such_agent__", Some("b"));
        let b = agent_node("b", "__no_such_agent__", None);
        a.next = Some("b".into());
        let graph = graph_with(vec![("a", a), ("b", b)], "a");

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("no end nodes")),
            "errors: {:?}",
            result.errors
        );
    }

    #[test]
    fn warns_when_end_exists_but_not_reachable() {
        let start = Node {
            id: "start".into(),
            description: String::new(),
            node_type: NodeType::Input(InputNode {
                question: "?".into(),
                default: None,
                validation: None,
                state_updates: None,
            }),
            next: None,
        };
        let graph = graph_with(
            vec![("start", start), ("orphan_end", end_node("orphan_end"))],
            "start",
        );

        let result = validator().validate(&graph);

        assert!(result.is_valid(), "unexpected errors: {:?}", result.errors);
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.message.contains("No end node is reachable"))
        );
    }

    #[test]
    fn errors_when_script_file_missing() {
        let scr = script_node("s", "definitely-not-here.py", None);
        let mut start = end_node("start");
        start.next = Some("s".into());
        let graph = graph_with(
            vec![("start", start), ("s", scr), ("end", end_node("end"))],
            "start",
        );

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("Script file not found")
                    && e.node_id.as_deref() == Some("s"))
        );
    }

    #[test]
    fn errors_when_referenced_agent_missing() {
        let agent = agent_node("a", "__definitely_no_such_agent__", Some("end"));
        let graph = graph_with(vec![("a", agent), ("end", end_node("end"))], "a");

        let result = validator().validate(&graph);

        assert!(result.errors.iter().any(|e| {
            e.message
                .contains("Agent '__definitely_no_such_agent__' not found")
        }));
    }

    #[test]
    fn errors_when_approval_option_has_no_route() {
        let approval = approval_node("ap", &["yes", "no"], &[("yes", "end")], "end");
        let graph = graph_with(vec![("ap", approval), ("end", end_node("end"))], "ap");

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("'no' has no route defined"))
        );
    }

    #[test]
    fn warns_when_approval_has_extra_route() {
        let approval = approval_node("ap", &["yes"], &[("yes", "end"), ("maybe", "end")], "end");
        let graph = graph_with(vec![("ap", approval), ("end", end_node("end"))], "ap");

        let result = validator().validate(&graph);

        assert!(result.warnings.iter().any(|w| {
            w.message
                .contains("Route 'maybe' has no corresponding option")
        }));
    }

    #[test]
    fn into_result_aggregates_all_errors() {
        let mut a = end_node("a");
        a.next = Some("missing1".into());
        let mut b = end_node("b");
        b.next = Some("missing2".into());
        let graph = graph_with(vec![("a", a), ("b", b)], "a");

        let err = validator()
            .validate(&graph)
            .into_result()
            .unwrap_err()
            .to_string();

        assert!(err.contains("missing1"), "got: {err}");
        assert!(err.contains("missing2"), "got: {err}");
        assert!(err.contains("validation failed"), "got: {err}");
    }

    #[test]
    fn into_result_returns_ok_when_no_errors() {
        let mut start = end_node("start");
        start.next = Some("end".into());

        let graph = graph_with(vec![("start", start), ("end", end_node("end"))], "start");

        assert!(validator().validate(&graph).into_result().is_ok());
    }

    #[test]
    fn cycle_detector_treats_fan_out_diamond_as_a_valid_dag() {
        let mut start = end_node("start");
        start.next = Some(NextTargets::Many(vec!["a".into(), "b".into()]));
        let mut a = end_node("a");
        a.next = Some("join".into());
        let mut b = end_node("b");
        b.next = Some("join".into());
        let mut join = end_node("join");
        join.next = Some("end".into());

        let graph = graph_with(
            vec![
                ("start", start),
                ("a", a),
                ("b", b),
                ("join", join),
                ("end", end_node("end")),
            ],
            "start",
        );

        let result = validator().validate(&graph);
        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("Cycle detected")),
            "fan-out diamond incorrectly reported as cycle: {:?}",
            result.errors
        );
    }

    #[test]
    fn reachability_visits_every_member_of_many_next_targets() {
        let mut start = end_node("start");
        start.next = Some(NextTargets::Many(vec!["a".into(), "b".into(), "c".into()]));
        let graph = graph_with(
            vec![
                ("start", start),
                ("a", end_node("a")),
                ("b", end_node("b")),
                ("c", end_node("c")),
            ],
            "start",
        );

        let result = validator().validate(&graph);

        for orphan in ["a", "b", "c"] {
            assert!(
                !result
                    .warnings
                    .iter()
                    .any(|w| w.node_id.as_deref() == Some(orphan)
                        && w.message.contains("unreachable")),
                "fan-out target '{orphan}' incorrectly marked unreachable: {:?}",
                result.warnings
            );
        }
    }

    #[test]
    fn node_reference_check_catches_missing_member_inside_many() {
        let mut start = end_node("start");
        start.next = Some(NextTargets::Many(vec!["a".into(), "ghost".into()]));
        let graph = graph_with(vec![("start", start), ("a", end_node("a"))], "start");

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("non-existent node 'ghost'")
                    && e.node_id.as_deref() == Some("start")),
            "expected error for missing 'ghost' target in Many: {:?}",
            result.errors
        );
    }

    #[test]
    fn node_reference_check_catches_missing_map_branch_target() {
        let map = Node {
            id: "fan".into(),
            description: String::new(),
            node_type: NodeType::Map(MapNode {
                over: "{{items}}".into(),
                as_name: "item".into(),
                branch: "no_such_node".into(),
                output_key: "output".into(),
                collect_into: "results".into(),
                max_concurrency: None,
            }),
            next: Some("end".into()),
        };
        let graph = graph_with(vec![("fan", map), ("end", end_node("end"))], "fan");

        let result = validator().validate(&graph);
        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("non-existent node 'no_such_node'")
                    && e.message.contains("map 'branch'")),
            "expected error for missing map branch: {:?}",
            result.errors
        );
    }

    fn map_node_basic(id: &str, branch: &str, next: Option<&str>) -> Node {
        Node {
            id: id.into(),
            description: String::new(),
            node_type: NodeType::Map(MapNode {
                over: "{{items}}".into(),
                as_name: "item".into(),
                branch: branch.into(),
                output_key: "output".into(),
                collect_into: "results".into(),
                max_concurrency: None,
            }),
            next: next.map(NextTargets::from),
        }
    }

    fn llm_with_state_updates(id: &str, updates: &[(&str, &str)], next: Option<&str>) -> Node {
        let mut node = llm_node(id, None, next);
        if let NodeType::Llm(ref mut n) = node.node_type {
            let mut map: HashMap<String, String> = HashMap::new();
            for (k, v) in updates {
                map.insert((*k).into(), (*v).into());
            }
            n.state_updates = Some(map);
        }
        node
    }

    fn llm_with_output_schema(id: &str, properties: &[&str], next: Option<&str>) -> Node {
        let mut node = llm_node(id, None, next);
        if let NodeType::Llm(ref mut n) = node.node_type {
            let mut props = serde_json::Map::new();
            for k in properties {
                props.insert((*k).to_string(), serde_json::json!({ "type": "string" }));
            }
            n.output_schema = Some(serde_json::json!({
                "type": "object",
                "properties": props,
            }));
        }
        node
    }

    fn script_with_state_updates(id: &str, updates: &[(&str, &str)]) -> Node {
        let mut node = script_node(id, "Cargo.toml", None);
        if let NodeType::Script(ref mut n) = node.node_type {
            let mut map: HashMap<String, String> = HashMap::new();
            for (k, v) in updates {
                map.insert((*k).into(), (*v).into());
            }
            n.state_updates = Some(map);
        }
        node
    }

    fn fan_out_graph_with_two_workers(worker_a: Node, worker_b: Node) -> Graph {
        let mut start = end_node("start");
        start.next = Some(NextTargets::Many(vec![
            "worker_a".into(),
            "worker_b".into(),
        ]));
        graph_with(
            vec![
                ("start", start),
                ("worker_a", worker_a),
                ("worker_b", worker_b),
                ("end", end_node("end")),
            ],
            "start",
        )
    }

    #[test]
    fn parallel_writes_to_same_key_without_reducer_errors() {
        let a = llm_with_state_updates("worker_a", &[("summary", "{{output}}")], Some("end"));
        let b = llm_with_state_updates("worker_b", &[("summary", "{{output}}")], Some("end"));
        let graph = fan_out_graph_with_two_workers(a, b);

        let result = validator().validate(&graph);

        assert!(
            result.errors.iter().any(|e| e
                .message
                .contains("nodes [worker_a, worker_b] all write key 'summary'")),
            "expected reducer-collision error for `summary`: {:?}",
            result.errors
        );
    }

    #[test]
    fn parallel_writes_to_same_key_with_reducer_pass() {
        let a = llm_with_state_updates("worker_a", &[("summary", "{{output}}")], Some("end"));
        let b = llm_with_state_updates("worker_b", &[("summary", "{{output}}")], Some("end"));
        let mut graph = fan_out_graph_with_two_workers(a, b);
        graph.reducers.insert("summary".into(), Reducer::Concat);

        let result = validator().validate(&graph);

        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("no reducer is declared")),
            "expected no reducer-collision error when reducer is declared: {:?}",
            result.errors
        );
    }

    #[test]
    fn parallel_writes_to_disjoint_keys_pass() {
        let a = llm_with_state_updates("worker_a", &[("a_out", "{{output}}")], Some("end"));
        let b = llm_with_state_updates("worker_b", &[("b_out", "{{output}}")], Some("end"));
        let graph = fan_out_graph_with_two_workers(a, b);

        let result = validator().validate(&graph);

        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("no reducer is declared")),
            "expected no collision for disjoint keys: {:?}",
            result.errors
        );
    }

    #[test]
    fn output_schema_top_level_keys_count_as_parallel_writes() {
        let a = llm_with_output_schema("worker_a", &["summary"], Some("end"));
        let b = llm_with_output_schema("worker_b", &["summary"], Some("end"));
        let graph = fan_out_graph_with_two_workers(a, b);

        let result = validator().validate(&graph);

        assert!(
            result.errors.iter().any(|e| e
                .message
                .contains("nodes [worker_a, worker_b] all write key 'summary'")),
            "output_schema top-level keys should count as writes: {:?}",
            result.errors
        );
    }

    #[test]
    fn three_parallel_writers_collision_lists_all_writers() {
        let mut start = end_node("start");
        start.next = Some(NextTargets::Many(vec!["a".into(), "b".into(), "c".into()]));
        let graph = graph_with(
            vec![
                ("start", start),
                (
                    "a",
                    llm_with_state_updates("a", &[("k", "{{output}}")], Some("end")),
                ),
                (
                    "b",
                    llm_with_state_updates("b", &[("k", "{{output}}")], Some("end")),
                ),
                (
                    "c",
                    llm_with_state_updates("c", &[("k", "{{output}}")], Some("end")),
                ),
                ("end", end_node("end")),
            ],
            "start",
        );

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("nodes [a, b, c]") && e.message.contains("'k'")),
            "expected error listing all three writers a, b, c: {:?}",
            result.errors
        );
    }

    #[test]
    fn approval_node_as_immediate_fan_out_target_errors() {
        let approval = approval_node("ap", &["yes"], &[("yes", "end")], "end");
        let other = end_node("other");
        let mut start = end_node("start");
        start.next = Some(NextTargets::Many(vec!["ap".into(), "other".into()]));
        let graph = graph_with(
            vec![
                ("start", start),
                ("ap", approval),
                ("other", other),
                ("end", end_node("end")),
            ],
            "start",
        );

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("approval node")
                    && e.message.contains("fan-out")
                    && e.node_id.as_deref() == Some("ap")),
            "expected approval-in-fan-out error: {:?}",
            result.errors
        );
    }

    #[test]
    fn input_node_as_immediate_fan_out_target_errors() {
        let input = Node {
            id: "in".into(),
            description: String::new(),
            node_type: NodeType::Input(InputNode {
                question: "?".into(),
                default: None,
                validation: None,
                state_updates: None,
            }),
            next: Some("end".into()),
        };
        let other = end_node("other");
        let mut start = end_node("start");
        start.next = Some(NextTargets::Many(vec!["in".into(), "other".into()]));
        let graph = graph_with(
            vec![
                ("start", start),
                ("in", input),
                ("other", other),
                ("end", end_node("end")),
            ],
            "start",
        );

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("input node")
                    && e.message.contains("fan-out")
                    && e.node_id.as_deref() == Some("in")),
            "expected input-in-fan-out error: {:?}",
            result.errors
        );
    }

    #[test]
    fn approval_after_join_passes() {
        let mut start = end_node("start");
        start.next = Some(NextTargets::Many(vec!["a".into(), "b".into()]));
        let mut a = end_node("a");
        a.next = Some("ap".into());
        let mut b = end_node("b");
        b.next = Some("ap".into());
        let approval = approval_node("ap", &["yes"], &[("yes", "end")], "end");
        let graph = graph_with(
            vec![
                ("start", start),
                ("a", a),
                ("b", b),
                ("ap", approval),
                ("end", end_node("end")),
            ],
            "start",
        );

        let result = validator().validate(&graph);

        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("fan-out") && e.node_id.as_deref() == Some("ap")),
            "approval AFTER join should be fine (v1 only checks immediate successors): {:?}",
            result.errors
        );
    }

    #[test]
    fn map_branch_cannot_be_approval() {
        let map = map_node_basic("m", "br", Some("end"));
        let branch = approval_node("br", &["yes"], &[("yes", "end")], "end");
        let graph = graph_with(
            vec![("m", map), ("br", branch), ("end", end_node("end"))],
            "m",
        );

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("approval node")
                    && e.message.contains("map branch")
                    && e.node_id.as_deref() == Some("m")),
            "expected map-branch-is-approval error: {:?}",
            result.errors
        );
    }

    #[test]
    fn map_branch_cannot_be_input() {
        let map = map_node_basic("m", "br", Some("end"));
        let branch = Node {
            id: "br".into(),
            description: String::new(),
            node_type: NodeType::Input(InputNode {
                question: "?".into(),
                default: None,
                validation: None,
                state_updates: None,
            }),
            next: Some("end".into()),
        };
        let graph = graph_with(
            vec![("m", map), ("br", branch), ("end", end_node("end"))],
            "m",
        );

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("input node")
                    && e.message.contains("map branch")
                    && e.node_id.as_deref() == Some("m")),
            "expected map-branch-is-input error: {:?}",
            result.errors
        );
    }

    #[test]
    fn map_branch_cannot_be_end() {
        let map = map_node_basic("m", "br", Some("done"));
        let graph = graph_with(
            vec![
                ("m", map),
                ("br", end_node("br")),
                ("done", end_node("done")),
            ],
            "m",
        );

        let result = validator().validate(&graph);

        assert!(
            result.errors.iter().any(|e| e.message.contains("end node")
                && e.message.contains("collect mechanism")
                && e.node_id.as_deref() == Some("m")),
            "expected map-branch-is-end error: {:?}",
            result.errors
        );
    }

    #[test]
    fn map_branch_cannot_be_another_map() {
        let outer = map_node_basic("outer", "inner", Some("end"));
        let inner = map_node_basic("inner", "end", Some("end"));
        let graph = graph_with(
            vec![("outer", outer), ("inner", inner), ("end", end_node("end"))],
            "outer",
        );

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("itself a map node")
                    && e.node_id.as_deref() == Some("outer")),
            "expected nested-map error: {:?}",
            result.errors
        );
    }

    #[test]
    fn map_branch_cannot_have_next_declared() {
        let map = map_node_basic("m", "br", Some("end"));
        let branch = llm_with_state_updates("br", &[("output", "{{output}}")], Some("somewhere"));
        let graph = graph_with(
            vec![
                ("m", map),
                ("br", branch),
                ("somewhere", end_node("somewhere")),
                ("end", end_node("end")),
            ],
            "m",
        );

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("has a `next` declared")
                    && e.message.contains("atomic")
                    && e.node_id.as_deref() == Some("br")),
            "expected branch-has-next error: {:?}",
            result.errors
        );
    }

    #[test]
    fn map_branch_state_updates_matching_output_key_passes() {
        let map = map_node_basic("m", "br", Some("end"));
        let branch = llm_with_state_updates("br", &[("output", "{{output}}")], None);
        let graph = graph_with(
            vec![("m", map), ("br", branch), ("end", end_node("end"))],
            "m",
        );

        let result = validator().validate(&graph);

        assert!(
            !result.errors.iter().any(|e| e
                .message
                .contains("map branches may only write through their `output_key`")),
            "valid map branch should not error on writes: {:?}",
            result.errors
        );
    }

    #[test]
    fn map_branch_state_updates_wrong_key_errors() {
        let map = map_node_basic("m", "br", Some("end"));
        let branch = llm_with_state_updates("br", &[("not_output", "{{output}}")], None);
        let graph = graph_with(
            vec![("m", map), ("br", branch), ("end", end_node("end"))],
            "m",
        );

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("writes state key 'not_output'")
                    && e.message.contains("'output'")
                    && e.node_id.as_deref() == Some("br")),
            "expected wrong-key error: {:?}",
            result.errors
        );
    }

    #[test]
    fn map_branch_with_output_schema_errors() {
        let map = map_node_basic("m", "br", Some("end"));
        let branch = llm_with_output_schema("br", &["foo", "bar"], None);
        let graph = graph_with(
            vec![("m", map), ("br", branch), ("end", end_node("end"))],
            "m",
        );

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("output_schema")
                    && e.message.contains("top-level properties")
                    && e.node_id.as_deref() == Some("br")),
            "expected output_schema-forbidden error: {:?}",
            result.errors
        );
    }

    #[test]
    fn script_in_fan_out_without_state_updates_errors() {
        let a = script_node("worker_a", "Cargo.toml", None);
        let b = end_node("worker_b");
        let graph = fan_out_graph_with_two_workers(a, b);

        let result = validator().validate(&graph);

        assert!(
            result.errors.iter().any(
                |e| e.message.contains("script node is in a parallel branch")
                    && e.message.contains("no `state_updates`")
                    && e.node_id.as_deref() == Some("worker_a")
            ),
            "expected script-no-state-updates-in-parallel error: {:?}",
            result.errors
        );
    }

    #[test]
    fn script_in_fan_out_with_state_updates_passes() {
        let a = script_with_state_updates("worker_a", &[("result_a", "{{output.x}}")]);
        let b = end_node("worker_b");
        let graph = fan_out_graph_with_two_workers(a, b);

        let result = validator().validate(&graph);

        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("script node is in a parallel branch")),
            "script with state_updates should pass C.6: {:?}",
            result.errors
        );
    }

    #[test]
    fn script_outside_fan_out_without_state_updates_passes() {
        let mut start = end_node("start");
        start.next = Some("s".into());
        let s = script_node("s", "Cargo.toml", None);
        let graph = graph_with(
            vec![("start", start), ("s", s), ("end", end_node("end"))],
            "start",
        );

        let result = validator().validate(&graph);

        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("parallel branch")),
            "script outside fan-out should not trigger C.6: {:?}",
            result.errors
        );
    }

    #[test]
    fn settings_max_concurrency_zero_errors() {
        let mut graph = graph_with(vec![("e", end_node("e"))], "e");
        graph.settings.max_concurrency = 0;

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("settings.max_concurrency must be >= 1")),
            "expected graph-level max_concurrency=0 error: {:?}",
            result.errors
        );
    }

    #[test]
    fn settings_max_concurrency_default_is_valid() {
        let graph = graph_with(vec![("e", end_node("e"))], "e");

        let result = validator().validate(&graph);

        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("max_concurrency")),
            "default max_concurrency should not error: {:?}",
            result.errors
        );
    }

    #[test]
    fn map_max_concurrency_zero_errors() {
        let mut map = map_node_basic("m", "br", Some("end"));
        if let NodeType::Map(ref mut mm) = map.node_type {
            mm.max_concurrency = Some(0);
        }
        let branch = llm_with_state_updates("br", &[("output", "{{output}}")], None);
        let graph = graph_with(
            vec![("m", map), ("br", branch), ("end", end_node("end"))],
            "m",
        );

        let result = validator().validate(&graph);

        assert!(
            result.errors.iter().any(|e| e
                .message
                .contains("map node's `max_concurrency` must be >= 1")
                && e.node_id.as_deref() == Some("m")),
            "expected map max_concurrency=0 error: {:?}",
            result.errors
        );
    }

    #[test]
    fn map_max_concurrency_none_is_valid() {
        let map = map_node_basic("m", "br", Some("end"));
        let branch = llm_with_state_updates("br", &[("output", "{{output}}")], None);
        let graph = graph_with(
            vec![("m", map), ("br", branch), ("end", end_node("end"))],
            "m",
        );

        let result = validator().validate(&graph);

        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("max_concurrency")),
            "map without max_concurrency should not error: {:?}",
            result.errors
        );
    }

    fn llm_with_prompt(id: &str, prompt: &str, next: Option<&str>) -> Node {
        let mut node = llm_node(id, None, next);
        if let NodeType::Llm(ref mut n) = node.node_type {
            n.prompt = prompt.into();
        }
        node
    }

    #[test]
    fn parallel_read_of_sibling_write_errors() {
        let reader = llm_with_prompt("worker_a", "Hello {{summary}}!", Some("end"));
        let writer = llm_with_state_updates("worker_b", &[("summary", "static")], Some("end"));
        let graph = fan_out_graph_with_two_workers(reader, writer);

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("reads state key(s) `summary`")
                    && e.message.contains("'worker_b'")),
            "expected cross-branch read error mentioning `summary` and sibling writer: {:?}",
            result.errors
        );
    }

    #[test]
    fn parallel_read_of_upstream_key_passes() {
        let reader_a = llm_with_prompt("worker_a", "Topic is {{topic}}", Some("end"));
        let reader_b = llm_with_prompt("worker_b", "Also {{topic}}", Some("end"));
        let graph = fan_out_graph_with_two_workers(reader_a, reader_b);

        let result = validator().validate(&graph);

        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("reads state key")),
            "upstream `topic` shouldn't trigger cross-branch read error: {:?}",
            result.errors
        );
    }

    #[test]
    fn scoped_output_var_in_state_updates_not_treated_as_read() {
        let scoped_user =
            llm_with_state_updates("worker_a", &[("a_key", "{{output}}")], Some("end"));
        let writes_output =
            llm_with_state_updates("worker_b", &[("output", "{{output}}")], Some("end"));
        let graph = fan_out_graph_with_two_workers(scoped_user, writes_output);

        let result = validator().validate(&graph);

        assert!(
            !result
                .errors
                .iter()
                .any(|e| e.message.contains("reads state key(s) `output`")
                    && e.message.contains("worker_a")),
            "scoped `{{{{output}}}}` inside state_updates value should NOT be treated as a read: {:?}",
            result.errors
        );
    }

    #[test]
    fn rag_query_reading_sibling_script_write_errors() {
        let mut rag = rag_node("worker_a", &["./k"], true);
        if let NodeType::Rag(ref mut n) = rag.node_type {
            n.query = Some("codes: {{loinc_codes}}\n{{db_result}}".into());
            if let Some(m) = n.state_updates.as_mut() {
                m.insert("rag_ctx".into(), "{{output.context}}".into());
            }
        }
        rag.next = Some("end".into());
        let mut script = script_with_state_updates("worker_b", &[("db_result", "{{output}}")]);
        script.next = Some("end".into());
        let graph = fan_out_graph_with_two_workers(rag, script);

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("reads state key(s) `db_result`")
                    && e.message.contains("'worker_b'")),
            "expected cross-branch read error for rag query reading db_result: {:?}",
            result.errors
        );
    }

    #[test]
    fn map_over_reading_sibling_write_errors() {
        let map_n = Node {
            id: "fan".into(),
            description: String::new(),
            node_type: NodeType::Map(MapNode {
                over: "{{items}}".into(),
                as_name: "item".into(),
                branch: "branch_n".into(),
                output_key: "output".into(),
                collect_into: "results".into(),
                max_concurrency: None,
            }),
            next: Some("end".into()),
        };
        let branch_n = llm_with_prompt("branch_n", "Process {{item}}", None);
        let producer = llm_with_state_updates("producer", &[("items", "[1,2,3]")], Some("end"));
        let mut start = end_node("start");
        start.next = Some(NextTargets::Many(vec!["fan".into(), "producer".into()]));
        let graph = graph_with(
            vec![
                ("start", start),
                ("fan", map_n),
                ("branch_n", branch_n),
                ("producer", producer),
                ("end", end_node("end")),
            ],
            "start",
        );

        let result = validator().validate(&graph);

        assert!(
            result
                .errors
                .iter()
                .any(|e| e.message.contains("reads state key(s) `items`")
                    && e.message.contains("'producer'")),
            "expected cross-branch read error for map `over` reading sibling write: {:?}",
            result.errors
        );
    }
}
