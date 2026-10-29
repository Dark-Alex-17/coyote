use super::llm::CompletionRunner;
use super::route::{self, Branch};
use super::transcript::{PREVIOUS_TURNS_BYTES, render_turns};
use super::{
    Graph, GraphExecutor, GraphParser, GraphRunResult, LastRun, agent_has_graph, session_turn,
};
use crate::config::paths;
use crate::config::{Input, RequestContext, Session, TEMP_SESSION_NAME};
use crate::utils::AbortSignal;
use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use log::info;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub fn active_agent_graph_name(ctx: &RequestContext) -> Option<String> {
    let name = ctx.agent.as_ref()?.name().to_string();
    agent_has_graph(&name).then_some(name)
}

/// What a child graph run hands back: the run itself and the record it
/// would leave on a session. The caller decides whether to store the
/// record; the dispatcher never touches the session.
pub struct ChildGraphRun {
    pub result: GraphRunResult,
    pub last_run: Option<LastRun>,
}

/// The one-shot run of the active agent's graph: `input` is seeded the way
/// a REPL `.run` turn is (its patched text under `initial_prompt`) and the
/// record, when the input is attached to the engaged session, names the
/// bare typed text.
pub async fn run_active_agent_graph(
    ctx: &mut RequestContext,
    input: &Input,
    abort_signal: AbortSignal,
) -> Result<ChildGraphRun> {
    run_seeded_graph(
        ctx,
        &input.text(),
        input.bare_text(),
        false,
        input.session(&ctx.session).is_some(),
        abort_signal,
        None,
    )
    .await
}

/// Runs the active agent's graph for a child context, with `inputs`
/// overlaid on its `initial_state` before the executor starts.
/// `seed_prompt` lands under `initial_prompt` and the session transcript
/// under `previous_turns`; the record always names `record_prompt`, which
/// differs from the seed when the caller appends run-only context (a
/// teammate roster) that must not be stored as the child's prompt.
pub async fn run_active_agent_graph_with_inputs(
    ctx: &mut RequestContext,
    seed_prompt: &str,
    record_prompt: &str,
    abort_signal: AbortSignal,
    inputs: Option<HashMap<String, Value>>,
) -> Result<ChildGraphRun> {
    let has_session = ctx.session.is_some();
    run_seeded_graph(
        ctx,
        seed_prompt,
        record_prompt,
        true,
        has_session,
        abort_signal,
        inputs,
    )
    .await
}

/// Loads, seeds and runs the active agent's graph without routing.
/// `seed_prompt` lands under `initial_prompt`; `record_prompt` is what the
/// record names, which differs from the seed when the one-shot input
/// carries a request-only patch.
async fn run_seeded_graph(
    ctx: &mut RequestContext,
    seed_prompt: &str,
    record_prompt: &str,
    child: bool,
    has_session: bool,
    abort_signal: AbortSignal,
    inputs: Option<HashMap<String, Value>>,
) -> Result<ChildGraphRun> {
    let agent_name =
        active_agent_graph_name(ctx).ok_or_else(|| anyhow!("Active agent has no graph.yaml"))?;

    info!("Agent '{agent_name}' has graph.yaml; routing to graph executor");

    let agent_dir = paths::agent_data_dir(&agent_name);
    let graph_path = paths::agent_graph_file(&agent_name);

    let parser = GraphParser::new(&agent_dir);
    let mut graph = parser
        .load_from_file(&graph_path)
        .with_context(|| format!("Failed to load graph.yaml for agent '{agent_name}'"))?;

    let previous_turns = previous_turns_for(ctx);
    let carried = carried_for(ctx, &graph.carry);
    seed_initial_state(
        &mut graph.initial_state,
        seed_prompt,
        inputs,
        &previous_turns,
        &carried,
    );

    let expose = graph.session.as_ref().map(|block| block.expose.clone());
    let carry = graph.carry.clone();
    let result = execute_graph(ctx, &agent_name, &agent_dir, graph, abort_signal).await?;
    let last_run = last_run_for(
        expose.as_deref(),
        &carry,
        &result,
        record_prompt,
        child,
        has_session,
    );
    Ok(ChildGraphRun { result, last_run })
}

/// The record a finished run leaves behind, or `None` when there is
/// nothing to store it on. A child always records: its caller seats it on
/// its own session. A REPL turn or a one-shot run records only when its
/// input is attached to the engaged session and the graph gives the record
/// a home: a `session:` block (`expose` is `Some`, possibly empty) or a
/// non-empty `carry:` list whose values the next run reads back.
pub fn last_run_for(
    expose: Option<&[String]>,
    carry: &[String],
    result: &GraphRunResult,
    prompt: &str,
    child: bool,
    has_session: bool,
) -> Option<LastRun> {
    (child || (has_session && (expose.is_some() || !carry.is_empty())))
        .then(|| LastRun::capture(result, prompt, expose.unwrap_or(&[]), carry))
}

pub enum GraphTurn {
    /// The record is boxed so the variant stays close in size to `Chat`.
    Run {
        output: String,
        last_run: Option<Box<LastRun>>,
    },
    /// The input for the chat branch. When the block names `chat.agent`,
    /// the delegate is swapped into `ctx.agent` (and the tool scope and RAG)
    /// when this is returned, and stays there until the REPL calls
    /// `leave_delegate_turn` at the end of the turn; the input was built
    /// under it, so its tool rounds and re-entries speak as the delegate.
    Chat(Box<Input>),
}

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
            let previous_turns = previous_turns_for(ctx);
            let carried = carried_for(ctx, &graph.carry);
            seed_initial_state(
                &mut graph.initial_state,
                &input.text(),
                None,
                &previous_turns,
                &carried,
            );
            let expose = graph.session.as_ref().map(|block| block.expose.clone());
            let carry = graph.carry.clone();
            let result = execute_graph(ctx, &agent_name, &agent_dir, graph, abort).await?;
            let last_run = last_run_for(
                expose.as_deref(),
                &carry,
                &result,
                input.bare_text(),
                false,
                input.session(&ctx.session).is_some(),
            );
            return Ok(GraphTurn::Run {
                output: result.output,
                last_run: last_run.map(Box::new),
            });
        }
    };
    let branch = match forced {
        Some(branch) => branch,
        None => route::classify(ctx, &graph, &block, input.bare_text(), &abort, runner).await?,
    };
    ctx.set_last_graph_branch(branch, forced.is_some());
    println!("{}", route::routing_line(branch));
    match branch {
        Branch::Run => {
            let previous_turns = previous_turns_for(ctx);
            let carried = carried_for(ctx, &graph.carry);
            seed_initial_state(
                &mut graph.initial_state,
                &input.text(),
                None,
                &previous_turns,
                &carried,
            );
            let carry = graph.carry.clone();
            let result = execute_graph(ctx, &agent_name, &agent_dir, graph, abort).await?;
            let last_run = last_run_for(
                Some(&block.expose),
                &carry,
                &result,
                input.bare_text(),
                false,
                input.session(&ctx.session).is_some(),
            );
            Ok(GraphTurn::Run {
                output: result.output,
                last_run: last_run.map(Box::new),
            })
        }
        Branch::Chat => {
            let mut attached = if let Some(chat_agent) = block.chat.agent.as_deref() {
                delegated_chat_input(ctx, &agent_name, chat_agent, input.bare_text())?
            } else {
                session_turn::build_chat_input(ctx, &block, input.bare_text())?
            };
            attached.carry_patch_from(input);
            if input.regenerate() {
                let role = attached.role().clone();
                attached.set_regenerate(role);
            }
            Ok(GraphTurn::Chat(Box::new(attached)))
        }
    }
}

/// Swaps the delegate in and builds the ordinary REPL input under it: the
/// delegate's instructions, model and whole tool pool, with the last run
/// inlined for this request only. On error the graph agent is back in the
/// slot; on success the delegate stays in until the turn ends. The delegate
/// was built from the block as it read at engage, so a `chat.agent` that
/// has since appeared or been renamed has no matching delegate to swap in.
fn delegated_chat_input(
    ctx: &mut RequestContext,
    agent_name: &str,
    chat_agent: &str,
    text: &str,
) -> Result<Input> {
    let engaged = ctx.delegate.as_ref().is_some_and(|d| d.name == chat_agent);
    if !engaged {
        // The hint is typed back verbatim, so a session name with blanks in
        // it rides quoted, the way the `.agent` command splits its words.
        let agent = ctx
            .session
            .as_ref()
            .map(Session::name)
            .filter(|s| !s.is_empty() && *s != TEMP_SESSION_NAME)
            .map_or_else(
                || format!(".agent {agent_name}"),
                |session| {
                    if session.contains(char::is_whitespace) {
                        format!(".agent {agent_name} \"{session}\"")
                    } else {
                        format!(".agent {agent_name} {session}")
                    }
                },
            );
        bail!(
            "session.chat.agent changed since the agent was engaged; run '{}' and then '{}' again",
            ctx.render_hints(".exit agent"),
            ctx.render_hints(&agent)
        );
    }
    if !ctx.enter_delegate_turn() {
        bail!(
            "chat agent '{chat_agent}' is already swapped in; the previous turn did not restore the graph agent"
        );
    }
    let mut attached = match Input::from_str(ctx, text, None) {
        Ok(input) => input,
        Err(err) => {
            // The turn's single exit leaves as well; this keeps the promise
            // above (graph agent back on error) for any caller of this
            // function on its own.
            ctx.leave_delegate_turn();
            return Err(err);
        }
    };
    if let Some(run) = ctx.session.as_ref().and_then(|s| s.last_run()) {
        attached.set_request_preamble(run.render());
    }
    Ok(attached)
}

/// Runs a seeded graph and cancels whatever jobs it left in the supervisor;
/// every run arm (`run_seeded_graph` and both `run_graph_turn` arms) finishes
/// through here. The executor future is boxed because it is large and, in
/// debug builds, every async layer reserves room for the child future it
/// constructs; left inline it would be reserved on this frame too, and one
/// such frame per nested graph overflows the 2 MiB stack of a test thread.
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
/// own YAML defaults, then the values `carried` over from the session's last
/// run that the graph still lists in `carry:`, then the caller's `inputs`,
/// then `initial_prompt` and `previous_turns`, which the dispatcher always
/// owns. Declared `variables` come after all of these and only fill keys
/// still absent (the executor's `seed_variables` runs `or_insert` on the
/// seeded map).
pub fn seed_initial_state(
    initial_state: &mut HashMap<String, Value>,
    prompt: &str,
    inputs: Option<HashMap<String, Value>>,
    previous_turns: &str,
    carried: &IndexMap<String, Value>,
) {
    for (key, value) in carried {
        initial_state.insert(key.clone(), value.clone());
    }
    if let Some(inputs) = inputs {
        initial_state.extend(inputs);
    }
    initial_state.insert("initial_prompt".into(), Value::String(prompt.to_string()));
    initial_state.insert(
        "previous_turns".into(),
        Value::String(previous_turns.to_string()),
    );
}

/// The engaged session's transcript for `previous_turns`. Renders whatever
/// session the context holds, a child context carrying its own agent session
/// included; empty when there is none.
pub fn previous_turns_for(ctx: &RequestContext) -> String {
    ctx.session
        .as_ref()
        .map(|session| render_turns(session.messages(), PREVIOUS_TURNS_BYTES))
        .unwrap_or_default()
}

/// The values the engaged session's last run carried over that the graph
/// still lists in `carry:`, in the stored order; a key the graph has since
/// dropped from the list (or moved into `variables[]`) is left out. Empty
/// when there is no session or it has no run yet.
pub fn carried_for(ctx: &RequestContext, carry: &[String]) -> IndexMap<String, Value> {
    ctx.session
        .as_ref()
        .and_then(|session| session.last_run())
        .map(|run| {
            run.carried
                .iter()
                .filter(|(key, _)| carry.contains(key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Model, ModelData};
    use crate::config::{AppState, LastMessage, Role, RoleLike, WorkingMode};
    use crate::graph::llm::test_support::{boxed_completion, completion_runner};
    use crate::graph::route::ROUTE_ROLE_NAME;
    use crate::supervisor::mailbox::Inbox;
    use crate::testing::TestConfigDirGuard;
    use crate::utils::create_abort_signal;
    use serde_json::json;
    use serial_test::serial;
    use std::fs::{create_dir_all, write};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    const SESSION_BLOCK: &str = "session:\n  chat:\n    instructions: hi\n";
    const ECHO_GRAPH: &str = "initial_state:\n  initial_prompt: \"\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: \"{{initial_prompt}}\"\n";
    const TURNS_ECHO_GRAPH: &str = "initial_state:\n  initial_prompt: \"\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: \"{{previous_turns}}\"\n";
    /// A graph that carries `said` and echoes it.
    const CARRY_ECHO_GRAPH: &str = "carry: [said]\ninitial_state:\n  said: \"\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: \"{{said}}\"\n";

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
        write_agent(name, session_block, ECHO_GRAPH);
    }

    /// A graph agent whose end node echoes `previous_turns`, which the
    /// graph itself may not declare.
    fn write_turns_echo_agent(name: &str, session_block: &str) {
        write_agent(name, session_block, TURNS_ECHO_GRAPH);
    }

    fn write_agent(name: &str, session_block: &str, graph: &str) {
        let agent_dir = paths::agent_data_dir(name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!("name: {name}\n{session_block}{graph}"),
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

    fn add_exchange(ctx: &mut RequestContext) -> Input {
        let input = Input::from_str(ctx, "q1", None).unwrap();
        ctx.session
            .as_mut()
            .unwrap()
            .add_message(&input, "r1")
            .unwrap();
        input
    }

    fn assert_output_is_the_session_transcript(ctx: &RequestContext, output: &str) {
        let session = ctx.session.as_ref().unwrap();
        assert_eq!(
            output,
            render_turns(session.messages(), PREVIOUS_TURNS_BYTES)
        );
        assert!(!output.is_empty());
        assert!(output.contains("user: q1\nassistant: r1"), "{output}");
    }

    /// An input whose request-only patch differs from the typed text, so
    /// the two can be told apart downstream.
    fn patched_input(ctx: &RequestContext) -> Input {
        let mut input = Input::from_str(ctx, "typed", None).unwrap();
        input.set_patched_text("typed (with retrieved context)");
        input
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
    async fn run_graph_turn_without_a_session_block_seeds_previous_turns() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("no_block_turns");
        write_turns_echo_agent(&name, "");
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = add_exchange(&mut ctx);

        let (turn, _) = turn_with_reply(&mut ctx, &input, None, "unused").await;

        let GraphTurn::Run { output, .. } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert_output_is_the_session_transcript(&ctx, &output);
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_run_branch_seeds_previous_turns() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("run_branch_turns");
        write_turns_echo_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = add_exchange(&mut ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, Some(Branch::Run), "unused").await;

        let GraphTurn::Run { output, .. } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert!(roles.is_empty());
        assert_output_is_the_session_transcript(&ctx, &output);
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_without_a_session_seeds_empty_previous_turns() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("no_session_turns");
        write_turns_echo_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        let input = patched_input(&ctx);

        let (turn, _) = turn_with_reply(&mut ctx, &input, None, "unused").await;

        let GraphTurn::Run { output, .. } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert_eq!(output, "");
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
    async fn run_graph_turn_classifier_reads_the_typed_text_while_the_graph_gets_the_patch() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("classifier_text");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = patched_input(&ctx);

        let mut route_prompts: Vec<String> = Vec::new();
        let mut runner = completion_runner(|input, _ctx, _abort| {
            if input.role().name() == ROUTE_ROLE_NAME {
                route_prompts.push(input.text());
            }
            boxed_completion(async { Ok((r#"{"decision":"run"}"#.to_string(), vec![])) })
        });
        let turn = run_graph_turn(&mut ctx, &input, create_abort_signal(), None, &mut runner)
            .await
            .unwrap();

        assert_eq!(route_prompts.len(), 1);
        assert!(
            route_prompts[0].ends_with("New message: typed"),
            "{}",
            route_prompts[0]
        );
        assert!(!route_prompts[0].contains("retrieved context"));
        let GraphTurn::Run { output, .. } = turn else {
            panic!("expected a run");
        };
        assert_eq!(output, "typed (with retrieved context)");
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

    /// A block naming `chat.agent` hands the chat branch to that agent: the
    /// input comes back built with it in the agent slot, declaring its own
    /// tools, and it stays there until the caller leaves the turn. A key
    /// renamed after engage, or one that appeared after engage, has no
    /// matching delegate to swap in: the turn is refused with the graph
    /// agent still in the slot and the delegate as it was. The refusal
    /// spells out the re-engage, naming the session when the context has a
    /// named one.
    #[tokio::test]
    #[serial]
    async fn run_graph_turn_delegated_chat_is_returned_with_the_chat_agent_swapped_in() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("delegating");
        let chat_agent = unique("chat_agent");
        let chat_agent_dir = paths::agent_data_dir(&chat_agent);
        create_dir_all(&chat_agent_dir).unwrap();
        write(
            chat_agent_dir.join("config.yaml"),
            format!("name: {chat_agent}\ninstructions: hi\n"),
        )
        .unwrap();
        write(
            chat_agent_dir.join("tools.sh"),
            "#!/usr/bin/env bash\n# @describe Agent-local tools\n\n# @cmd Probe the agent\nlocal_hello() { echo hi; }\n",
        )
        .unwrap();
        let delegating_block = format!("session:\n  chat:\n    agent: {chat_agent}\n");
        write_graph_agent(&name, &delegating_block);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = patched_input(&ctx);
        assert!(!input.declared_function_names().contains("local_hello"));

        let (turn, roles) = turn_with_reply(&mut ctx, &input, Some(Branch::Chat), "unused").await;

        let GraphTurn::Chat(attached) = turn.unwrap() else {
            panic!("expected a chat");
        };
        assert!(roles.is_empty());
        assert_eq!(ctx.agent.as_ref().unwrap().name(), chat_agent);
        assert!(attached.with_session());
        assert!(attached.with_agent());
        assert_eq!(attached.text(), "typed (with retrieved context)");
        assert!(attached.declared_function_names().contains("local_hello"));
        assert_eq!(ctx.last_graph_branch, Some((Branch::Chat, true)));

        ctx.leave_delegate_turn();
        assert_eq!(ctx.agent.as_ref().unwrap().name(), name);
        assert!(ctx.delegate.as_ref().unwrap().parked.is_none());

        let renamed = unique("renamed_chat_agent");
        let renamed_dir = paths::agent_data_dir(&renamed);
        create_dir_all(&renamed_dir).unwrap();
        write(
            renamed_dir.join("config.yaml"),
            format!("name: {renamed}\ninstructions: hi\n"),
        )
        .unwrap();
        write_graph_agent(&name, &format!("session:\n  chat:\n    agent: {renamed}\n"));
        let input = patched_input(&ctx);

        let (turn, _) = turn_with_reply(&mut ctx, &input, Some(Branch::Chat), "unused").await;

        let err = turn
            .err()
            .expect("the engaged delegate is not the named one");
        assert!(ctx.session.as_ref().unwrap().name().is_empty());
        let chain = format!("{err:#}");
        assert!(
            chain.contains("session.chat.agent changed since the agent was engaged"),
            "{chain}"
        );
        assert!(chain.contains("run '.exit agent' and then"), "{chain}");
        assert!(chain.contains(&format!("'.agent {name}' again")), "{chain}");
        assert_eq!(ctx.agent.as_ref().unwrap().name(), name);
        let delegate = ctx.delegate.as_ref().unwrap();
        assert_eq!(delegate.name, chat_agent);
        assert_eq!(delegate.agent.name(), chat_agent);
        assert!(delegate.parked.is_none());

        let plain = unique("plain");
        write_graph_agent(&plain, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&plain).await;
        let mut session = chat_session();
        session.set_name("s".to_string());
        ctx.session = Some(session);
        let input = patched_input(&ctx);
        write_graph_agent(&plain, &delegating_block);

        let (turn, _) = turn_with_reply(&mut ctx, &input, Some(Branch::Chat), "unused").await;

        let err = turn.err().expect("no delegate to swap in");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("session.chat.agent changed since the agent was engaged"),
            "{chain}"
        );
        assert!(chain.contains("run '.exit agent' and then"), "{chain}");
        assert!(
            chain.contains(&format!("'.agent {plain} s' again")),
            "{chain}"
        );
        assert_eq!(ctx.agent.as_ref().unwrap().name(), plain);
        assert!(ctx.delegate.is_none());
    }

    /// The delegated chat input carries the session's last run as a
    /// request-only preamble, the same way the inline chat branch does: it
    /// is in the message content sent, not in the input's own text.
    #[tokio::test]
    #[serial]
    async fn run_graph_turn_delegated_chat_inlines_the_last_run_for_the_request_only() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("delegating");
        let chat_agent = unique("chat_agent");
        let chat_agent_dir = paths::agent_data_dir(&chat_agent);
        create_dir_all(&chat_agent_dir).unwrap();
        write(
            chat_agent_dir.join("config.yaml"),
            format!("name: {chat_agent}\ninstructions: hi\n"),
        )
        .unwrap();
        write_graph_agent(
            &name,
            &format!("session:\n  chat:\n    agent: {chat_agent}\n"),
        );
        let mut ctx = ctx_with_agent(&name).await;
        let mut session = chat_session();
        let earlier = GraphRunResult {
            output: "earlier output".to_string(),
            final_state: HashMap::new(),
            trace: vec![],
        };
        session.set_last_run(Some(LastRun::capture(&earlier, "earlier prompt", &[], &[])));
        ctx.session = Some(session);
        let input = patched_input(&ctx);

        let (turn, _) = turn_with_reply(&mut ctx, &input, Some(Branch::Chat), "unused").await;

        let GraphTurn::Chat(attached) = turn.unwrap() else {
            panic!("expected a chat");
        };
        let content = attached.message_content().to_text();
        assert!(
            content.starts_with("Last graph run (completed "),
            "{content}"
        );
        assert!(content.contains("earlier output"), "{content}");
        assert!(
            content.ends_with("\n\ntyped (with retrieved context)"),
            "{content}"
        );
        ctx.leave_delegate_turn();
    }

    /// The refusal for a `chat.agent` that appeared after engage is one
    /// sentence whose two command tokens follow the configured prefix: dot
    /// and `both` render the dot form, slash the slash form, and the session
    /// name rides on the re-engage token only when the session has a name
    /// worth re-engaging, quoted when it holds a blank so the hint can be
    /// typed back as one word; the temp session is left off like an unnamed
    /// one.
    /// Nothing is asked of the model.
    #[tokio::test]
    #[serial]
    async fn usage_probe_the_changed_chat_agent_refusal_is_one_exact_sentence_in_every_prefix() {
        use crate::config::CommandPrefix;

        let _guard = TestConfigDirGuard::new("dispatch");
        let chat_agent = unique("chat_agent");
        let chat_agent_dir = paths::agent_data_dir(&chat_agent);
        create_dir_all(&chat_agent_dir).unwrap();
        write(
            chat_agent_dir.join("config.yaml"),
            format!("name: {chat_agent}\ninstructions: hi\n"),
        )
        .unwrap();
        let delegating_block = format!("session:\n  chat:\n    agent: {chat_agent}\n");
        for (prefix, shown, session_name, token) in [
            (CommandPrefix::Dot, '.', "s", Some("s")),
            (CommandPrefix::Both, '.', "s", Some("s")),
            (CommandPrefix::Slash, '/', "s", Some("s")),
            (
                CommandPrefix::Dot,
                '.',
                "my session",
                Some("\"my session\""),
            ),
            (CommandPrefix::Slash, '/', "", None),
            (CommandPrefix::Dot, '.', TEMP_SESSION_NAME, None),
        ] {
            let plain = unique("plain");
            write_graph_agent(&plain, SESSION_BLOCK);
            let mut ctx = ctx_with_agent(&plain).await;
            let mut config = (*ctx.app.config).clone();
            config.command_prefix = prefix;
            let mut app = (*ctx.app).clone();
            app.config = Arc::new(config);
            ctx.app = Arc::new(app);
            let mut session = chat_session();
            session.set_name(session_name.to_string());
            ctx.session = Some(session);
            let input = patched_input(&ctx);
            write_graph_agent(&plain, &delegating_block);

            let (turn, roles) =
                turn_with_reply(&mut ctx, &input, Some(Branch::Chat), "unused").await;

            let err = turn.err().expect("no delegate to swap in");
            let re_engage = match token {
                Some(token) => format!("{shown}agent {plain} {token}"),
                None => format!("{shown}agent {plain}"),
            };
            let expected = format!(
                "session.chat.agent changed since the agent was engaged; run '{shown}exit agent' and then '{re_engage}' again"
            );
            assert!(
                err.chain().any(|link| link.to_string() == expected),
                "{prefix:?} {session_name:?}: {err:#}"
            );
            assert!(
                roles.is_empty(),
                "{prefix:?}: the model was asked {roles:?}"
            );
            assert_eq!(ctx.agent.as_ref().unwrap().name(), plain);
            assert!(ctx.delegate.is_none());
        }
    }

    /// A delegated chat input that finds the delegate already swapped in is
    /// refused with its own message, not the changed-block one with its
    /// re-engage hints: the block still names the engaged delegate, so
    /// re-engaging would not help. The delegate is left as found. Called
    /// directly: `run_graph_turn` looks the graph up under whoever is in
    /// the agent slot, and a swapped-in chat agent has none, so it bails
    /// before reaching this.
    #[tokio::test]
    #[serial]
    async fn delegated_chat_input_refuses_a_turn_while_the_chat_agent_is_still_swapped_in() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("delegating");
        let chat_agent = unique("chat_agent");
        let chat_agent_dir = paths::agent_data_dir(&chat_agent);
        create_dir_all(&chat_agent_dir).unwrap();
        write(
            chat_agent_dir.join("config.yaml"),
            format!("name: {chat_agent}\ninstructions: hi\n"),
        )
        .unwrap();
        write_graph_agent(
            &name,
            &format!("session:\n  chat:\n    agent: {chat_agent}\n"),
        );
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        assert!(ctx.enter_delegate_turn());
        assert_eq!(ctx.agent.as_ref().unwrap().name(), chat_agent);

        let err = delegated_chat_input(&mut ctx, &name, &chat_agent, "typed")
            .expect_err("the delegate is already swapped in");
        let expected = format!(
            "chat agent '{chat_agent}' is already swapped in; the previous turn did not restore the graph agent"
        );
        assert!(
            err.chain().any(|link| link.to_string() == expected),
            "{err:#}"
        );
        assert_eq!(ctx.agent.as_ref().unwrap().name(), chat_agent);
        ctx.leave_delegate_turn();
        assert_eq!(ctx.agent.as_ref().unwrap().name(), name);
        assert!(ctx.delegate.as_ref().unwrap().parked.is_none());
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
        session.set_last_run(Some(LastRun::capture(&earlier, "earlier prompt", &[], &[])));
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
    /// runs a block graph straight through the child entry point, with no
    /// classification, even with a session engaged. The run's record comes
    /// back to the caller naming the record prompt, not the seed the graph
    /// read; nothing is stored on the session here.
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

        let run = run_active_agent_graph_with_inputs(
            &mut ctx,
            "from a node\n\nrun-only context",
            "from a node",
            create_abort_signal(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(run.result.output, "from a node\n\nrun-only context");
        assert!(!run.result.trace.is_empty());
        let last_run = run.last_run.as_ref().expect("a child run is recorded");
        assert_eq!(last_run.prompt, "from a node");
        assert_eq!(last_run.output, "from a node\n\nrun-only context");
        assert!(last_run.exposed.is_empty());
        assert_eq!(last_run.trace, run.result.trace);
        assert_eq!(ctx.current_depth, 1);
        assert!(ctx.session.as_ref().unwrap().last_run().is_none());
        assert_eq!(ctx.last_graph_branch, None);
    }

    /// A one-shot run seeds the request-only patch the way a REPL `.run`
    /// does, but its record names the typed text: the patch is for the
    /// model, not for the session.
    #[tokio::test]
    #[serial]
    async fn run_active_agent_graph_seeds_the_patched_text_and_records_the_bare_prompt() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("one_shot");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let mut input = Input::from_str(&ctx, "hello", None).unwrap();
        input.set_patched_text("hello (retrieved)");

        let run = run_active_agent_graph(&mut ctx, &input, create_abort_signal())
            .await
            .unwrap();

        assert_eq!(run.result.output, "hello (retrieved)");
        assert_eq!(run.last_run.unwrap().prompt, "hello");
        assert!(ctx.session.as_ref().unwrap().last_run().is_none());
    }

    /// Without an engaged session a one-shot run has nowhere to be
    /// recorded, block graph or not.
    #[tokio::test]
    #[serial]
    async fn run_active_agent_graph_records_nothing_without_a_session() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("one_shot_no_session");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        let input = Input::from_str(&ctx, "hello", None).unwrap();

        let run = run_active_agent_graph(&mut ctx, &input, create_abort_signal())
            .await
            .unwrap();

        assert_eq!(run.result.output, "hello");
        assert!(run.last_run.is_none());
    }

    /// `execute_graph` boxes the executor future and says why where the
    /// box is; the explanation is not deferred to another comment.
    #[test]
    fn execute_graph_carries_its_own_stack_rationale() {
        let source = include_str!("dispatch.rs").replace("\r\n", "\n");
        let production = &source[..source.find("\n#[cfg(test)]").unwrap()];
        assert_eq!(production.matches("overflows the 2 MiB stack").count(), 1);
        assert_eq!(production.matches("GraphExecutor::execute`").count(), 0);
        assert_eq!(production.matches("see the comment on").count(), 0);
    }

    /// A child run on a `carry:` graph starts from the values the session's
    /// last run left behind and carries the run's own values forward in the
    /// record it hands back.
    #[tokio::test]
    #[serial]
    async fn run_active_agent_graph_with_inputs_seeds_and_records_the_carried_values() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("child_carried");
        write_agent(&name, "", CARRY_ECHO_GRAPH);
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
        let mut session = chat_session();
        session.set_last_run(Some(first_run_that_said("from the first run")));
        ctx.session = Some(session);

        let run = run_active_agent_graph_with_inputs(
            &mut ctx,
            "from a node",
            "from a node",
            create_abort_signal(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(run.result.output, "from the first run");
        let last_run = run.last_run.expect("a child run is recorded");
        assert!(last_run.exposed.is_empty());
        assert_eq!(
            last_run.carried.get("said"),
            Some(&json!("from the first run"))
        );
        assert_eq!(
            ctx.session.as_ref().unwrap().last_run().unwrap().prompt,
            "earlier",
            "the dispatcher leaves the session's record alone"
        );
    }

    #[tokio::test]
    #[serial]
    async fn run_active_agent_graph_with_inputs_seeds_empty_previous_turns_without_a_session() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("child_turns");
        write_turns_echo_agent(&name, SESSION_BLOCK);
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
        assert!(ctx.session.is_none());

        let run = run_active_agent_graph_with_inputs(
            &mut ctx,
            "from a node",
            "from a node",
            create_abort_signal(),
            None,
        )
        .await
        .unwrap();

        assert_eq!(run.result.output, "");
        assert!(
            run.last_run.is_some(),
            "a child run is recorded even without a session"
        );
    }

    /// Forcing `Chat` where the chat branch exists skips the classifier
    /// like forcing `Run` does; the attached input keeps the last run's
    /// preamble after the REPL-line carry-over, and a later forced turn
    /// overwrites the recorded branch.
    #[tokio::test]
    #[serial]
    async fn usage_probe_forced_chat_skips_the_classifier_and_keeps_the_preamble() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("forced_chat");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        let mut session = chat_session();
        let earlier = GraphRunResult {
            output: "earlier output".to_string(),
            final_state: HashMap::new(),
            trace: vec![],
        };
        session.set_last_run(Some(LastRun::capture(&earlier, "earlier prompt", &[], &[])));
        ctx.session = Some(session);
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, Some(Branch::Chat), "unused").await;

        let GraphTurn::Chat(attached) = turn.unwrap() else {
            panic!("expected a chat");
        };
        assert!(roles.is_empty());
        assert_eq!(ctx.last_graph_branch, Some((Branch::Chat, true)));
        assert!(attached.with_session());
        assert_eq!(attached.text(), "typed (with retrieved context)");
        assert_eq!(attached.bare_text(), "typed");
        let content = attached.message_content().to_text();
        assert!(
            content.starts_with("Last graph run (completed "),
            "the carry-over must not drop the builder's preamble: {content}"
        );
        assert!(
            content.contains("### Output\n```\nearlier output\n```"),
            "{content}"
        );
        assert!(
            content.ends_with("\n\ntyped (with retrieved context)"),
            "{content}"
        );
        assert_eq!(
            ctx.session.as_ref().unwrap().last_run().unwrap().prompt,
            "earlier prompt",
            "a chat turn leaves the stored record alone"
        );

        let (turn, roles) = turn_with_reply(&mut ctx, &input, Some(Branch::Run), "unused").await;
        assert!(matches!(turn.unwrap(), GraphTurn::Run { .. }));
        assert!(roles.is_empty());
        assert_eq!(ctx.last_graph_branch, Some((Branch::Run, true)));
    }

    /// Whether the graph ran is observed from the outside: a script node
    /// leaves a marker file. A classifier failure and a forced chat leave
    /// none; a classified run leaves exactly one.
    #[tokio::test]
    #[serial]
    async fn usage_probe_only_the_run_branch_executes_the_graph() {
        if which::which("bash").is_err() {
            eprintln!("skipping: bash not available");
            return;
        }
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("marker");
        let agent_dir = paths::agent_data_dir(&name);
        create_dir_all(&agent_dir).unwrap();
        let marker = agent_dir.join("ran.marker");
        write(
            agent_dir.join("mark.sh"),
            format!(
                "#!/bin/bash\nprintf 'x' >> '{}'\necho '{{}}'\n",
                marker.display()
            ),
        )
        .unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {name}\n{SESSION_BLOCK}initial_state:\n  initial_prompt: \"\"\nstart: mark\nnodes:\n  mark:\n    type: script\n    script: mark.sh\n    state_updates: {{}}\n    next: done\n  done:\n    type: end\n    output: \"{{{{initial_prompt}}}}\"\n"
            ),
        )
        .unwrap();
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, None, "no idea").await;
        assert!(turn.is_err(), "a prose reply fails the turn");
        assert_eq!(roles, [ROUTE_ROLE_NAME]);
        assert!(
            !marker.exists(),
            "a classifier failure must not have run the graph"
        );

        let (turn, roles) = turn_with_reply(&mut ctx, &input, Some(Branch::Chat), "unused").await;
        assert!(matches!(turn.unwrap(), GraphTurn::Chat(_)));
        assert!(roles.is_empty());
        assert!(!marker.exists(), "a chat turn must not have run the graph");

        let (turn, _) = turn_with_reply(&mut ctx, &input, None, r#"{"decision":"chat"}"#).await;
        assert!(matches!(turn.unwrap(), GraphTurn::Chat(_)));
        assert!(
            !marker.exists(),
            "a classified chat must not have run the graph"
        );

        let (turn, roles) = turn_with_reply(&mut ctx, &input, None, r#"{"decision":"run"}"#).await;
        let GraphTurn::Run { output, last_run } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert_eq!(roles, [ROUTE_ROLE_NAME]);
        assert_eq!(output, "typed (with retrieved context)");
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "x",
            "the classified run executes the graph exactly once"
        );
        let last_run = last_run.unwrap();
        assert_eq!(last_run.prompt, "typed");
        assert_eq!(
            last_run.trace.len(),
            2,
            "both nodes are in the record: {:?}",
            last_run.trace
        );
    }

    /// The branch is recorded when it is decided, so a run whose graph
    /// then fails still leaves `Run` behind for the turn's bookkeeping.
    #[tokio::test]
    #[serial]
    async fn run_graph_turn_records_the_run_branch_even_when_the_graph_fails() {
        if which::which("bash").is_err() {
            eprintln!("skipping: bash not available");
            return;
        }
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("failing_run");
        let agent_dir = paths::agent_data_dir(&name);
        create_dir_all(&agent_dir).unwrap();
        write(agent_dir.join("fail.sh"), "#!/bin/bash\nexit 3\n").unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!(
                "name: {name}\n{SESSION_BLOCK}initial_state:\n  initial_prompt: \"\"\nstart: fail\nnodes:\n  fail:\n    type: script\n    script: fail.sh\n    state_updates: {{}}\n    next: done\n  done:\n    type: end\n    output: \"{{{{initial_prompt}}}}\"\n"
            ),
        )
        .unwrap();
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, None, r#"{"decision":"run"}"#).await;

        let err = turn.err().expect("a failing script fails the run");
        let chain = format!("{err:#}");
        assert!(chain.contains("Graph execution failed"), "{chain}");
        assert_eq!(roles, [ROUTE_ROLE_NAME]);
        assert_eq!(ctx.last_graph_branch, Some((Branch::Run, false)));
        assert!(ctx.session.as_ref().unwrap().last_run().is_none());
    }

    #[test]
    fn seed_initial_state_without_inputs_only_adds_the_reserved_keys() {
        let mut state = yaml_defaults();

        seed_initial_state(&mut state, "hello", None, "", &IndexMap::new());

        let mut expected = yaml_defaults();
        expected.insert("initial_prompt".into(), json!("hello"));
        expected.insert("previous_turns".into(), json!(""));
        assert_eq!(state, expected);
    }

    #[test]
    fn seed_initial_state_without_inputs_on_empty_state_inserts_only_the_reserved_keys() {
        let mut state = HashMap::new();

        seed_initial_state(
            &mut state,
            "hello",
            None,
            "user: q1\nassistant: r1",
            &IndexMap::new(),
        );

        assert_eq!(
            state,
            HashMap::from([
                ("initial_prompt".to_string(), json!("hello")),
                (
                    "previous_turns".to_string(),
                    json!("user: q1\nassistant: r1")
                ),
            ])
        );
    }

    #[test]
    fn seed_initial_state_without_inputs_seeds_every_reserved_key() {
        let mut state = HashMap::new();

        seed_initial_state(&mut state, "p", None, "t", &IndexMap::new());

        for key in super::super::RESERVED_STATE_KEYS {
            assert!(state.contains_key(key), "reserved key '{key}' not seeded");
        }
        assert_eq!(state.len(), super::super::RESERVED_STATE_KEYS.len());
    }

    #[test]
    fn seed_initial_state_inputs_override_yaml_defaults_and_keep_the_rest() {
        let mut state = yaml_defaults();
        let inputs = HashMap::from([
            ("width".to_string(), json!(2)),
            ("extra".to_string(), json!(["a", "b"])),
        ]);

        seed_initial_state(&mut state, "hello", Some(inputs), "", &IndexMap::new());

        assert_eq!(state.get("width"), Some(&json!(2)));
        assert_eq!(state.get("mode"), Some(&json!("fast")));
        assert_eq!(state.get("extra"), Some(&json!(["a", "b"])));
        assert_eq!(state.get("initial_prompt"), Some(&json!("hello")));
        assert_eq!(state.get("previous_turns"), Some(&json!("")));
        assert_eq!(state.len(), 5);
    }

    #[test]
    fn seed_initial_state_dispatcher_prompt_beats_an_initial_prompt_input() {
        let mut state = HashMap::from([("initial_prompt".to_string(), json!("from yaml"))]);
        let inputs = HashMap::from([("initial_prompt".to_string(), json!("from inputs"))]);

        seed_initial_state(
            &mut state,
            "from dispatcher",
            Some(inputs),
            "",
            &IndexMap::new(),
        );

        assert_eq!(state.get("initial_prompt"), Some(&json!("from dispatcher")));
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn seed_initial_state_dispatcher_turns_beat_a_previous_turns_input() {
        let mut state = HashMap::from([("previous_turns".to_string(), json!("from yaml"))]);
        let inputs = HashMap::from([("previous_turns".to_string(), json!("from inputs"))]);

        seed_initial_state(
            &mut state,
            "prompt",
            Some(inputs),
            "from dispatcher",
            &IndexMap::new(),
        );

        assert_eq!(state.get("previous_turns"), Some(&json!("from dispatcher")));
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn seed_initial_state_null_input_overlays_a_default() {
        let mut state = yaml_defaults();
        let inputs = HashMap::from([("width".to_string(), Value::Null)]);

        seed_initial_state(&mut state, "hello", Some(inputs), "", &IndexMap::new());

        assert_eq!(state.get("width"), Some(&Value::Null));
    }

    fn carried_said(value: &str) -> IndexMap<String, Value> {
        IndexMap::from([("said".to_string(), json!(value))])
    }

    #[test]
    fn seed_initial_state_inputs_beat_carried_values() {
        let mut state = HashMap::from([("said".to_string(), json!("a"))]);
        let inputs = HashMap::from([("said".to_string(), json!("c"))]);

        seed_initial_state(&mut state, "p", Some(inputs), "", &carried_said("b"));

        assert_eq!(state.get("said"), Some(&json!("c")));
    }

    #[test]
    fn seed_initial_state_carried_values_beat_yaml_defaults() {
        let mut state = HashMap::from([("said".to_string(), json!("a"))]);

        seed_initial_state(&mut state, "p", None, "", &carried_said("b"));

        assert_eq!(state.get("said"), Some(&json!("b")));
        assert_eq!(state.len(), 3);
    }

    #[test]
    fn seed_initial_state_carried_values_never_override_the_reserved_keys() {
        let mut state = HashMap::new();
        let carried = IndexMap::from([
            ("initial_prompt".to_string(), json!("stale prompt")),
            ("previous_turns".to_string(), json!("stale turns")),
        ]);

        seed_initial_state(&mut state, "fresh prompt", None, "fresh turns", &carried);

        assert_eq!(state.get("initial_prompt"), Some(&json!("fresh prompt")));
        assert_eq!(state.get("previous_turns"), Some(&json!("fresh turns")));
    }

    #[test]
    fn carried_for_is_empty_without_a_session_or_a_last_run() {
        let carry = ["said".to_string()];
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        assert!(carried_for(&ctx, &carry).is_empty());

        ctx.session = Some(chat_session());
        assert!(carried_for(&ctx, &carry).is_empty());

        let mut session = chat_session();
        session.set_last_run(Some(first_run_that_said("b")));
        ctx.session = Some(session);
        assert!(carried_for(&ctx, &[]).is_empty());
    }

    #[test]
    fn carried_for_keeps_only_the_keys_the_graph_still_carries() {
        let earlier = GraphRunResult {
            output: String::new(),
            final_state: HashMap::from([
                ("said".to_string(), json!("b")),
                ("gone".to_string(), json!("z")),
            ]),
            trace: vec![],
        };
        let mut session = chat_session();
        session.set_last_run(Some(LastRun::capture(
            &earlier,
            "earlier",
            &[],
            &["said".to_string(), "gone".to_string()],
        )));
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.session = Some(session);

        let carried = carried_for(&ctx, &["said".to_string()]);

        assert_eq!(carried, IndexMap::from([("said".to_string(), json!("b"))]));
    }

    fn first_run_that_said(value: &str) -> LastRun {
        let earlier = GraphRunResult {
            output: "earlier output".to_string(),
            final_state: HashMap::from([("said".to_string(), json!(value))]),
            trace: vec![],
        };
        LastRun::capture(&earlier, "earlier", &[], &["said".to_string()])
    }

    /// A graph that carries `said` and echoes it: the second run under a
    /// session starts from the value the first run left behind, and the
    /// run's own record carries the value forward again.
    #[tokio::test]
    #[serial]
    async fn run_graph_turn_run_branch_seeds_the_carried_values_of_the_last_run() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("carried");
        write_agent(
            &name,
            SESSION_BLOCK,
            "carry: [said]\ninitial_state:\n  said: \"\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: \"{{said}}\"\n",
        );
        let mut ctx = ctx_with_agent(&name).await;
        let mut session = chat_session();
        session.set_last_run(Some(first_run_that_said("from the first run")));
        ctx.session = Some(session);
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, Some(Branch::Run), "unused").await;

        let GraphTurn::Run { output, last_run } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert!(roles.is_empty());
        assert_eq!(output, "from the first run");
        assert_eq!(
            last_run.unwrap().carried.get("said"),
            Some(&json!("from the first run"))
        );
    }

    #[tokio::test]
    #[serial]
    async fn run_graph_turn_run_branch_without_a_last_run_keeps_the_yaml_default() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("carried_default");
        write_agent(
            &name,
            SESSION_BLOCK,
            "carry: [said]\ninitial_state:\n  said: \"yaml default\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: \"{{said}}\"\n",
        );
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = patched_input(&ctx);

        let (turn, _) = turn_with_reply(&mut ctx, &input, Some(Branch::Run), "unused").await;

        let GraphTurn::Run { output, last_run } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert_eq!(output, "yaml default");
        assert_eq!(
            last_run.unwrap().carried.get("said"),
            Some(&json!("yaml default"))
        );
    }

    /// A key the last run carried but the graph no longer lists must not
    /// override that key's YAML default.
    #[tokio::test]
    #[serial]
    async fn run_graph_turn_run_branch_ignores_carried_keys_the_graph_dropped() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("carried_stale");
        write_agent(
            &name,
            SESSION_BLOCK,
            "carry: [said]\ninitial_state:\n  said: \"\"\n  gone: \"default\"\nstart: done\nnodes:\n  done:\n    type: end\n    output: \"{{said}}|{{gone}}\"\n",
        );
        let earlier = GraphRunResult {
            output: "earlier output".to_string(),
            final_state: HashMap::from([
                ("said".to_string(), json!("kept")),
                ("gone".to_string(), json!("stale")),
            ]),
            trace: vec![],
        };
        let mut ctx = ctx_with_agent(&name).await;
        let mut session = chat_session();
        session.set_last_run(Some(LastRun::capture(
            &earlier,
            "earlier",
            &[],
            &["said".to_string(), "gone".to_string()],
        )));
        ctx.session = Some(session);
        let input = patched_input(&ctx);

        let (turn, _) = turn_with_reply(&mut ctx, &input, Some(Branch::Run), "unused").await;

        let GraphTurn::Run { output, last_run } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert_eq!(output, "kept|default");
        let carried = last_run.unwrap().carried;
        assert_eq!(carried.get("said"), Some(&json!("kept")));
        assert!(!carried.contains_key("gone"));
    }

    fn run_that_said(value: &str) -> GraphRunResult {
        GraphRunResult {
            output: "out".to_string(),
            final_state: HashMap::from([("said".to_string(), json!(value))]),
            trace: vec![],
        }
    }

    #[test]
    fn last_run_for_a_child_records_with_or_without_a_session() {
        let result = run_that_said("x");
        for has_session in [false, true] {
            let run = last_run_for(None, &[], &result, "p", true, has_session)
                .unwrap_or_else(|| panic!("has_session {has_session}"));
            assert_eq!(run.prompt, "p");
            assert_eq!(run.output, "out");
            assert!(run.exposed.is_empty());
            assert!(run.carried.is_empty());
        }
    }

    #[test]
    fn last_run_for_a_repl_turn_records_nothing_without_a_block_or_a_carry() {
        let result = run_that_said("x");
        assert!(last_run_for(None, &[], &result, "p", false, true).is_none());
        assert!(last_run_for(None, &[], &result, "p", false, false).is_none());
    }

    #[test]
    fn last_run_for_a_repl_turn_records_a_block_graph_only_under_a_session() {
        let result = run_that_said("x");
        let run = last_run_for(Some(&[]), &[], &result, "p", false, true).unwrap();
        assert!(run.exposed.is_empty());
        assert!(last_run_for(Some(&[]), &[], &result, "p", false, false).is_none());

        let expose = ["said".to_string()];
        let run = last_run_for(Some(&expose), &[], &result, "p", false, true).unwrap();
        assert_eq!(run.exposed.get("said"), Some(&json!("x")));
        assert!(last_run_for(Some(&expose), &[], &result, "p", false, false).is_none());
    }

    #[test]
    fn last_run_for_a_repl_turn_records_a_blockless_carry_graph_only_under_a_session() {
        let result = run_that_said("x");
        let carry = ["said".to_string()];
        let run = last_run_for(None, &carry, &result, "p", false, true).unwrap();
        assert!(run.exposed.is_empty());
        assert_eq!(run.carried.get("said"), Some(&json!("x")));
        assert!(last_run_for(None, &carry, &result, "p", false, false).is_none());
    }

    /// A graph with no session block but a `carry:` list still records
    /// under a session: the carried value needs the record to reach the
    /// next run. No classifier runs and no branch is recorded.
    #[tokio::test]
    #[serial]
    async fn run_graph_turn_without_a_session_block_records_a_carry_graph_under_a_session() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let name = unique("no_block_carried");
        write_agent(&name, "", CARRY_ECHO_GRAPH);
        let mut ctx = ctx_with_agent(&name).await;
        let mut session = chat_session();
        session.set_last_run(Some(first_run_that_said("from the first run")));
        ctx.session = Some(session);
        let input = patched_input(&ctx);

        let (turn, roles) = turn_with_reply(&mut ctx, &input, None, "unused").await;

        let GraphTurn::Run { output, last_run } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert!(roles.is_empty());
        assert_eq!(ctx.last_graph_branch, None);
        assert_eq!(output, "from the first run");
        let last_run = last_run.expect("a carry graph records under a session");
        assert_eq!(last_run.prompt, "typed");
        assert!(last_run.exposed.is_empty());
        assert_eq!(
            last_run.carried.get("said"),
            Some(&json!("from the first run"))
        );
    }

    /// A detached input is not attached to the session even when one is
    /// engaged, so neither run arm computes a record for it.
    #[tokio::test]
    #[serial]
    async fn run_graph_turn_records_nothing_for_an_input_detached_from_the_session() {
        let _guard = TestConfigDirGuard::new("dispatch");
        let role = Role::new(ROUTE_ROLE_NAME, "p");

        let name = unique("detached_block");
        write_graph_agent(&name, SESSION_BLOCK);
        let mut ctx = ctx_with_agent(&name).await;
        ctx.session = Some(chat_session());
        let input = Input::detached_with_role(&ctx, "typed", role.clone());
        let (turn, roles) = turn_with_reply(&mut ctx, &input, Some(Branch::Run), "unused").await;
        let GraphTurn::Run { output, last_run } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert!(roles.is_empty());
        assert_eq!(output, "typed");
        assert!(last_run.is_none(), "{:?}", last_run.map(|run| run.prompt));

        let name = unique("detached_carry");
        write_agent(&name, "", CARRY_ECHO_GRAPH);
        let mut ctx = ctx_with_agent(&name).await;
        let mut session = chat_session();
        session.set_last_run(Some(first_run_that_said("from the first run")));
        ctx.session = Some(session);
        let input = Input::detached_with_role(&ctx, "typed", role);
        let (turn, _) = turn_with_reply(&mut ctx, &input, None, "unused").await;
        let GraphTurn::Run { output, last_run } = turn.unwrap() else {
            panic!("expected a run");
        };
        assert_eq!(output, "from the first run");
        assert!(last_run.is_none(), "{:?}", last_run.map(|run| run.prompt));
    }

    /// The bundled coder carries no state between runs: its orchestrator
    /// sends a fresh request each time, and both model nodes read the
    /// earlier turns in their prompts, not their standing instructions.
    #[test]
    fn bundled_coder_carries_nothing_and_reads_the_earlier_turns_in_its_prompts() {
        let graph: Graph =
            serde_yaml::from_str(include_str!("../../assets/agents/coder/graph.yaml")).unwrap();

        assert!(graph.carry.is_empty(), "{:?}", graph.carry);

        let turns_heading = "## Earlier turns with this agent (empty on a first run)";
        for node_id in ["analyze_request", "implement"] {
            let node = graph.get_node(node_id).unwrap();
            let super::super::types::NodeType::Llm(llm) = &node.node_type else {
                panic!("{node_id} is not an llm node");
            };
            assert!(
                llm.prompt.contains(turns_heading),
                "{node_id}: {}",
                llm.prompt
            );
            assert!(
                llm.prompt.contains("{{previous_turns}}"),
                "{node_id}: {}",
                llm.prompt
            );
            let instructions = llm.instructions.as_deref().unwrap_or("");
            assert!(!instructions.contains(turns_heading), "{node_id}");
            assert!(!instructions.contains("{{previous_turns}}"), "{node_id}");
            assert!(
                !instructions.contains("Plan carried from the earlier run"),
                "{node_id}"
            );
            assert!(!instructions.contains("{{plan_summary}}"), "{node_id}");
        }

        let super::super::types::NodeType::Llm(analyze) =
            &graph.get_node("analyze_request").unwrap().node_type
        else {
            panic!("analyze_request is not an llm node");
        };
        assert!(
            !analyze.prompt.contains("Plan carried from the earlier run"),
            "{}",
            analyze.prompt
        );
        assert!(
            !analyze.prompt.contains("may not have been applied"),
            "{}",
            analyze.prompt
        );
        for key in ["plan_summary", "files_to_modify", "files_to_create"] {
            let placeholder = format!("{{{{{key}}}}}");
            assert!(!analyze.prompt.contains(&placeholder), "{}", analyze.prompt);
        }
    }
}
