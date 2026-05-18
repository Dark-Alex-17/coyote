use super::types::{Graph, Node, NodeType};
use crate::client::{Model, ModelType};
use crate::config::{Agent, AppConfig, paths};
use anyhow::{Result, bail};
use std::collections::{HashSet, VecDeque};
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
}

impl GraphValidator {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            agent_ctx: None,
        }
    }

    pub fn with_agent_context(mut self, ctx: AgentValidationContext) -> Self {
        self.agent_ctx = Some(ctx);
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
}

fn declared_targets(node: &Node) -> Vec<(String, &'static str)> {
    let mut out = Vec::new();
    if let Some(n) = &node.next {
        out.push((n.clone(), "'next'"));
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
            conversation_starters: Vec::new(),
            settings: GraphSettings::default(),
            initial_state: HashMap::new(),
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
            }),
            next: next.map(String::from),
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
            next: next.map(String::from),
        }
    }

    fn validator() -> GraphValidator {
        GraphValidator::new(env::current_dir().unwrap())
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
}
