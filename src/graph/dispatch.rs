use super::llm::CompletionRunner;
use super::route::{self, Branch};
use super::{
    Graph, GraphExecutor, GraphParser, GraphRunResult, LastRun, agent_has_graph, session_turn,
};
use crate::config::paths;
use crate::config::{Input, RequestContext};
use crate::utils::AbortSignal;
use anyhow::{Context, Result, anyhow, bail};
use log::info;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

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

/// What one REPL turn against a graph agent turned into: a finished graph
/// run (with the record to store once the exchange is saved, when the
/// agent's session block applies) or the attached input for a chat turn the
/// caller still has to send.
#[allow(dead_code)] // TASK-193: REPL wiring
pub enum GraphTurn {
    Run {
        output: String,
        last_run: Option<LastRun>,
    },
    Chat(Box<Input>),
}

/// Routes one turn: without a session block or an engaged session the graph
/// simply runs; otherwise the classifier (or `forced`) picks the branch.
/// Forcing `Chat` where no chat branch exists (no session block, or no
/// session engaged) is an error raised before the graph runs; forcing `Run`
/// there is the plain run. The graph is seeded with `input.text()`, the
/// RAG-patched text, while the recorded prompt is the text as typed.
/// Storing the record into the session and sending the chat input are left
/// to the caller.
#[allow(dead_code)] // TASK-193: REPL wiring
pub async fn run_graph_turn(
    ctx: &mut RequestContext,
    input: &Input,
    abort: AbortSignal,
    forced: Option<Branch>,
    runner: &mut CompletionRunner<'_>,
) -> Result<GraphTurn> {
    let agent_name =
        active_agent_graph_name(ctx).ok_or_else(|| anyhow!("Active agent has no graph.yaml"))?;

    info!("Agent '{agent_name}' has graph.yaml; routing the turn");

    let agent_dir = paths::agent_data_dir(&agent_name);
    let graph_path = paths::agent_graph_file(&agent_name);

    let parser = GraphParser::new(&agent_dir);
    let mut graph = parser
        .load_from_file(&graph_path)
        .with_context(|| format!("Failed to load graph.yaml for agent '{agent_name}'"))?;

    let block = match (&graph.session, ctx.session.is_some()) {
        (Some(block), true) => block.clone(),
        (block, _) => {
            if forced == Some(Branch::Chat) {
                let why = match block {
                    None => "it has no session block",
                    Some(_) => "no session is engaged",
                };
                bail!("Cannot force the chat branch for agent '{agent_name}': {why}");
            }
            seed_initial_state(&mut graph.initial_state, &input.text(), None);
            let output = execute_graph(ctx, &agent_name, &agent_dir, graph, abort)
                .await?
                .output;
            return Ok(GraphTurn::Run {
                output,
                last_run: None,
            });
        }
    };
    let branch = match forced {
        Some(branch) => branch,
        None => route::classify(ctx, &graph, &block, &input.text(), &abort, runner).await?,
    };
    ctx.set_last_graph_branch(branch, forced.is_some());
    println!("{}", route::routing_line(branch));
    match branch {
        Branch::Run => {
            seed_initial_state(&mut graph.initial_state, &input.text(), None);
            let result = execute_graph(ctx, &agent_name, &agent_dir, graph, abort).await?;
            let last_run = LastRun::capture(&result, input.bare_text(), &block.expose);
            Ok(GraphTurn::Run {
                output: result.output,
                last_run: Some(last_run),
            })
        }
        Branch::Chat => {
            let mut attached = session_turn::build_chat_input(ctx, &block, input.bare_text())?;
            attached.carry_patch_from(input);
            if input.regenerate() {
                let role = attached.role().clone();
                attached.set_regenerate(role);
            }
            Ok(GraphTurn::Chat(Box::new(attached)))
        }
    }
}

/// Runs a seeded graph and cancels whatever jobs it left in the supervisor,
/// the same way `run_active_agent_graph_with_inputs` finishes. Boxed for
/// the same stack reason as `GraphExecutor::execute`.
async fn execute_graph(
    ctx: &mut RequestContext,
    agent_name: &str,
    agent_dir: &Path,
    graph: Graph,
    abort: AbortSignal,
) -> Result<GraphRunResult> {
    let result = Box::pin(GraphExecutor::new(graph, agent_dir).execute_with_trace(ctx, abort))
        .await
        .with_context(|| format!("Graph execution failed for agent '{agent_name}'"))?;
    if let Some(supervisor) = ctx.supervisor.clone() {
        supervisor.read().cancel_all();
    }
    Ok(result)
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
    use crate::client::{Model, ModelData};
    use crate::config::{AppState, LastMessage, RoleLike, Session, WorkingMode};
    use crate::function::ToolResult;
    use crate::graph::llm::CompletionFuture;
    use crate::graph::route::ROUTE_ROLE_NAME;
    use crate::supervisor::mailbox::Inbox;
    use crate::testing::TestConfigDirGuard;
    use crate::utils::create_abort_signal;
    use serde_json::json;
    use serial_test::serial;
    use std::fs::{create_dir_all, write};
    use std::future::Future;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    const SESSION_BLOCK: &str = "session:\n  chat:\n    instructions: hi\n";
    const ECHO_GRAPH: &str = "initial_state:\n  initial_prompt: \"\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: \"{{initial_prompt}}\"\n";

    fn yaml_defaults() -> HashMap<String, Value> {
        HashMap::from([
            ("width".to_string(), json!(4)),
            ("mode".to_string(), json!("fast")),
        ])
    }

    fn unique(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}_{nanos}")
    }

    /// A graph agent whose end node echoes `initial_prompt`, with or
    /// without a session block.
    fn write_graph_agent(name: &str, session_block: &str) {
        let agent_dir = paths::agent_data_dir(name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!("name: {name}\n{session_block}{ECHO_GRAPH}"),
        )
        .unwrap();
    }

    async fn ctx_with_agent(name: &str) -> RequestContext {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        ctx.use_agent(&app, name, None, create_abort_signal())
            .await
            .unwrap();
        ctx
    }

    fn chat_session() -> Session {
        let mut session = Session::default();
        session.set_model(Model::from_config("openai", &[ModelData::new("fc-model")]).remove(0));
        session
    }

    /// An input whose request-only patch differs from the typed text, so
    /// the two can be told apart downstream.
    fn patched_input(ctx: &RequestContext) -> Input {
        let mut input = Input::from_str(ctx, "typed", None).unwrap();
        input.set_patched_text("typed (with retrieved context)");
        input
    }

    fn completion_runner<F>(f: F) -> F
    where
        F: for<'a> FnMut(&'a Input, &'a mut RequestContext, AbortSignal) -> CompletionFuture<'a>
            + Send,
    {
        f
    }

    fn boxed_completion<'a>(
        fut: impl Future<Output = Result<(String, Vec<ToolResult>)>> + Send + 'a,
    ) -> CompletionFuture<'a> {
        Box::pin(fut)
    }

    /// Drives `run_graph_turn` with a runner that answers `reply`; returns
    /// the turn and the role names of the runner's calls.
    async fn turn_with_reply(
        ctx: &mut RequestContext,
        input: &Input,
        forced: Option<Branch>,
        reply: &str,
    ) -> (Result<GraphTurn>, Vec<String>) {
        let mut roles: Vec<String> = Vec::new();
        let mut runner = completion_runner(|input, _ctx, _abort| {
            roles.push(input.role().name().to_string());
            let reply = reply.to_string();
            boxed_completion(async move { Ok((reply, vec![])) })
        });
        let turn = run_graph_turn(ctx, input, create_abort_signal(), forced, &mut runner).await;
        (turn, roles)
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_without_a_session_block_runs_the_graph_unclassified() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("no_block");
        write_graph_agent(&name, "");
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, None, "unused").await;

        let GraphTurn::Run { output, last_run } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert_eq!(output, "typed (with retrieved context)");
        assert!(last_run.is_none());
        assert!(roles.is_empty());
        assert_eq!(ctx.last_graph_branch, None);
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_without_a_session_runs_the_graph_unclassified() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("no_session");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, None, "unused").await;

        let GraphTurn::Run { output, last_run } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert_eq!(output, "typed (with retrieved context)");
        assert!(last_run.is_none());
        assert!(roles.is_empty());
        assert_eq!(ctx.last_graph_branch, None);
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_run_branch_records_the_typed_prompt_and_seeds_the_patched_text() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("run_branch");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, None, r#"{"decision":"run"}"#).await;

        let GraphTurn::Run { output, last_run } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert_eq!(roles, [ROUTE_ROLE_NAME]);
        assert_eq!(output, "typed (with retrieved context)");
        let last_run = last_run.unwrap();
        assert_eq!(last_run.prompt, "typed");
        assert_ne!(last_run.prompt, input.text());
        assert_eq!(last_run.output, output);
        assert_eq!(last_run.trace.len(), 1);
        assert_eq!(ctx.last_graph_branch, Some((Branch::Run, false)));
        assert!(
            ctx.session.as_ref().unwrap().last_run().is_none(),
            "storing the record is the caller's job"
        );
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_chat_branch_rebuilds_the_input_attached_to_the_session() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("chat_branch");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = patched_input(&ctx);
        assert!(!input.regenerate());

        let (turn, roles) = turn_with_reply(&mut ctx, &input, None, r#"{"decision":"chat"}"#).await;

        let GraphTurn::Chat(attached) = turn.unwrap() else {
            panic!("expected a chat");
        };
        assert_eq!(roles, [ROUTE_ROLE_NAME]);
        assert!(attached.with_session());
        assert_eq!(attached.text(), input.text());
        assert_eq!(attached.bare_text(), input.bare_text());
        assert!(!attached.regenerate());
        assert_eq!(ctx.last_graph_branch, Some((Branch::Chat, false)));

        let mut regenerating = patched_input(&ctx);
        let role = regenerating.role().clone();
        regenerating.set_regenerate(role);
        let (turn, _) =
            turn_with_reply(&mut ctx, &regenerating, None, r#"{"decision":"chat"}"#).await;
        let GraphTurn::Chat(attached) = turn.unwrap() else {
            panic!("expected a chat");
        };
        assert!(attached.regenerate());
        assert_eq!(attached.text(), regenerating.text());
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_chat_branch_keeps_the_files_medias_and_last_reply() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("chat_media");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let earlier = Input::from_str(&ctx, "earlier", None).unwrap();
        ctx.last_message = Some(LastMessage::new(earlier, "earlier reply".to_string()));
        let image = paths::agent_data_dir(&name).join("shot.png");
        write(&image, b"not really a png").unwrap();
        let input = Input::from_files(
            &ctx,
            "typed",
            vec![image.to_string_lossy().to_string(), "%%".to_string()],
            None,
        )
        .await
        .unwrap();
        assert_eq!(input.medias().len(), 1);
        assert_eq!(input.last_reply(), Some("earlier reply"));

        let (turn, _) = turn_with_reply(&mut ctx, &input, None, r#"{"decision":"chat"}"#).await;

        let GraphTurn::Chat(attached) = turn.unwrap() else {
            panic!("expected a chat");
        };
        assert_eq!(attached.raw(), input.raw());
        assert!(attached.raw().contains("shot.png"), "{}", attached.raw());
        assert_eq!(attached.medias(), input.medias());
        assert_eq!(attached.data_urls(), input.data_urls());
        assert_eq!(attached.render(), input.render());
        assert_eq!(attached.last_reply(), Some("earlier reply"));
        assert_eq!(attached.bare_text(), input.bare_text());
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_forced_branch_skips_the_classifier() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("forced");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, Some(Branch::Run), "unused").await;

        let GraphTurn::Run { last_run, .. } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert!(roles.is_empty());
        assert_eq!(last_run.unwrap().prompt, "typed");
        assert_eq!(ctx.last_graph_branch, Some((Branch::Run, true)));
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_forced_chat_without_a_session_block_is_an_error() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("forced_chat_no_block");
        write_graph_agent(&name, "");
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, Some(Branch::Chat), "unused").await;

        let err = turn.err().expect("forced chat must fail");
        let chain = format!("{err:#}");
        assert!(chain.contains("Cannot force the chat branch"), "{chain}");
        assert!(chain.contains("no session block"), "{chain}");
        assert!(!chain.contains("Graph execution"), "{chain}");
        assert!(roles.is_empty());
        assert_eq!(ctx.last_graph_branch, None);
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_forced_chat_without_a_session_is_an_error() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("forced_chat_no_session");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, Some(Branch::Chat), "unused").await;

        let err = turn.err().expect("forced chat must fail");
        let chain = format!("{err:#}");
        assert!(chain.contains("no session is engaged"), "{chain}");
        assert!(!chain.contains("Graph execution"), "{chain}");
        assert!(roles.is_empty());
        assert_eq!(ctx.last_graph_branch, None);
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_classifier_error_propagates_and_leaves_the_session_alone() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("classifier_error");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        let mut session = chat_session();
        let earlier = GraphRunResult {
            output: "earlier output".to_string(),
            final_state: HashMap::new(),
            trace: vec![],
        };
        session.set_last_run(Some(LastRun::capture(&earlier, "earlier prompt", &[])));
        ctx.session = Some(session);
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, None, "maybe").await;

        let err = turn.err().expect("a prose reply must fail the turn");
        let chain = format!("{err:#}");
        assert!(chain.contains("Graph route classifier"), "{chain}");
        assert_eq!(roles, [ROUTE_ROLE_NAME]);
        assert_eq!(ctx.last_graph_branch, None);
        let last_run = ctx.session.as_ref().unwrap().last_run().unwrap();
        assert_eq!(last_run.prompt, "earlier prompt");
        assert_eq!(last_run.output, "earlier output");
    }

    /// A child agent context (as `use_agent` from a graph node builds it)
    /// runs a block graph straight through the legacy entry point, with no
    /// classification and no session bookkeeping, even with a session
    /// engaged.
    #[tokio::test]
    #[serial]
    async fn run_active_agent_graph_with_inputs_bypasses_routing_for_a_child_context() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("child");
        write_graph_agent(&name, SESSION_BLOCK);
        let parent = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let mut ctx = RequestContext::new_for_child(
            Arc::clone(&parent.app),
            &parent,
            1,
            Arc::new(Inbox::new()),
            format!("{name}_1"),
        );
        let app = ctx.app.config.clone();
        ctx.use_agent(&app, &name, None, create_abort_signal())
            .await
            .unwrap();
        ctx.current_depth = 1;
        ctx.session = Some(chat_session());

        let output = run_active_agent_graph_with_inputs(
            &mut ctx,
            "from a node",
            create_abort_signal(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(output, "from a node");
        assert_eq!(ctx.current_depth, 1);
        assert!(ctx.session.as_ref().unwrap().last_run().is_none());
        assert_eq!(ctx.last_graph_branch, None);
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
