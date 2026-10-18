use super::llm::CompletionRunner;
use super::structured;
use super::types::{Graph, SessionBlock};
use crate::client::{Message, MessageContent, Model, ModelType};
use crate::config::{Input, RequestContext, Role, RoleLike};
use crate::utils::AbortSignal;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::fmt::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    Run,
    Chat,
}

pub const ROUTE_CONTEXT_MESSAGES: usize = 6;
pub const ROUTE_CONTEXT_BYTES: usize = 4096;

pub const ROUTE_SCHEMA: &str = r#"{"type":"object","properties":{"decision":{"type":"string","enum":["run","chat"]}},"required":["decision"]}"#;

pub const ROUTE_ROLE_NAME: &str = "__graph_route__";

const ROUTE_QUESTION: &str = "Is the user instructing the agent to perform its task (run) or asking about / discussing previous work (chat)? Answer with JSON matching the schema.";

const TRUNCATION_MARK: &str = "...";

/// Decides whether the new message starts a graph run or continues the chat
/// about the previous one. One detached model call through `runner`, then
/// the structured-output extractor if the reply is not already JSON. The
/// classifier declares no functions at all, not even the agent's built-ins,
/// so a tool call in its reply has nothing to execute. Its own exchange
/// never becomes `ctx.last_message` and never
/// replaces `ctx.declared_function_names`, and any failure is the turn's
/// failure: there is no silent default branch.
pub async fn classify(
    ctx: &mut RequestContext,
    graph: &Graph,
    block: &SessionBlock,
    text: &str,
    abort: &AbortSignal,
    runner: &mut CompletionRunner<'_>,
) -> Result<Branch> {
    let role = build_route_role(ctx, block)?;
    let prompt = build_route_prompt(ctx, graph, block, text);
    let input = Input::detached_with_role(ctx, &prompt, role);

    // Callers await this to completion; a dropped future would leave ctx
    // without the taken state.
    let saved_last_message = ctx.last_message.take();
    let saved_declared = std::mem::take(&mut ctx.declared_function_names);
    let outcome = decide(&input, ctx, abort, runner).await;
    ctx.last_message = saved_last_message;
    ctx.declared_function_names = saved_declared;
    outcome
}

async fn decide(
    input: &Input,
    ctx: &mut RequestContext,
    abort: &AbortSignal,
    runner: &mut CompletionRunner<'_>,
) -> Result<Branch> {
    let (raw, _) = runner(input, ctx, abort.clone())
        .await
        .context("Graph route classifier call failed")?;
    let schema: Value = serde_json::from_str(ROUTE_SCHEMA)?;
    let value = structured::extract(&raw, &schema, ctx, abort)
        .await
        .context("Graph route classifier could not read the decision")?;
    match value.get("decision").and_then(Value::as_str) {
        Some("run") => Ok(Branch::Run),
        Some("chat") => Ok(Branch::Chat),
        _ => bail!("Graph route classifier returned an unrecognised decision: {value}"),
    }
}

fn build_route_role(ctx: &RequestContext, block: &SessionBlock) -> Result<Role> {
    let model = match block.route.as_ref().and_then(|r| r.model.as_deref()) {
        Some(id) => Model::retrieve_model(ctx.app.config.as_ref(), id, ModelType::Chat)
            .with_context(|| format!("Unknown model '{id}' on session.route"))?,
        None => ctx.current_model().clone(),
    };
    let mut role = Role::new(
        ROUTE_ROLE_NAME,
        &format!("{ROUTE_QUESTION}\n\nSchema:\n{ROUTE_SCHEMA}"),
    );
    role.set_model(model);
    role.set_enabled_tools(Some(Vec::new()));
    role.set_enabled_mcp_servers(Some(Vec::new()));
    Ok(role)
}

fn build_route_prompt(
    ctx: &RequestContext,
    graph: &Graph,
    block: &SessionBlock,
    text: &str,
) -> String {
    let mut prompt = String::new();
    if !graph.description.is_empty() {
        let _ = writeln!(prompt, "Agent: {}", graph.description);
    }
    if let Some(instructions) = block.route.as_ref().and_then(|r| r.instructions.as_deref()) {
        let _ = writeln!(prompt, "Instructions: {instructions}");
    }
    let session = ctx.session.as_ref();
    let has_last_run = session.and_then(|s| s.last_run()).is_some();
    let _ = writeln!(prompt, "has_last_run: {has_last_run}");
    let recent = recent_messages(session.map(|s| s.messages()).unwrap_or(&[]));
    if !recent.is_empty() {
        let _ = writeln!(prompt, "Recent messages:\n{recent}");
    }
    let _ = write!(prompt, "New message: {text}");
    prompt
}

/// The last `ROUTE_CONTEXT_MESSAGES` user/assistant text messages, one per
/// line, cut from the front to `ROUTE_CONTEXT_BYTES` so the newest context
/// survives.
fn recent_messages(messages: &[Message]) -> String {
    let lines: Vec<String> = messages
        .iter()
        .filter_map(|m| {
            let speaker = if m.role.is_user() {
                "user"
            } else if m.role.is_assistant() {
                "assistant"
            } else {
                return None;
            };
            match &m.content {
                MessageContent::ToolCalls(_) => None,
                content => Some(format!("{speaker}: {}", content.to_text())),
            }
        })
        .collect();
    let start = lines.len().saturating_sub(ROUTE_CONTEXT_MESSAGES);
    truncate_from_front(&lines[start..].join("\n"), ROUTE_CONTEXT_BYTES)
}

fn truncate_from_front(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let keep = max_bytes.saturating_sub(TRUNCATION_MARK.len());
    let mut cut = text.len() - keep;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    format!("{TRUNCATION_MARK}{}", &text[cut..])
}

pub fn routing_line(branch: Branch) -> String {
    let text = match branch {
        Branch::Run => "routing: run",
        Branch::Chat => "routing: chat",
    };
    crate::utils::dimmed_text(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ModelData;
    use crate::config::{AppState, LastMessage, Session, WorkingMode, paths};
    use crate::function::ToolResult;
    use crate::graph::llm::CompletionFuture;
    use crate::graph::types::{ChatBlock, RouteBlock};
    use crate::graph::{GraphRunResult, LastRun};
    use crate::testing::TestConfigDirGuard;
    use crate::utils::create_abort_signal;
    use serial_test::serial;
    use std::collections::{HashMap, HashSet};
    use std::fs::{create_dir_all, write};
    use std::future::Future;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    const SESSION_BLOCK: &str = "session:\n  chat:\n    instructions: hi\n";

    /// Identity funnel that pins a closure to the runner's higher-ranked
    /// signature, so inline closures at call sites infer the right lifetimes.
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

    fn ctx_with_session() -> RequestContext {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.session = Some(chat_session());
        ctx
    }

    fn chat_session() -> Session {
        let mut session = Session::default();
        session.set_model(Model::from_config("openai", &[ModelData::new("fc-model")]).remove(0));
        session
    }

    fn unique(prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}_{nanos}")
    }

    fn write_graph_agent(name: &str) {
        let agent_dir = paths::agent_data_dir(name);
        create_dir_all(&agent_dir).unwrap();
        write(
            agent_dir.join("graph.yaml"),
            format!("name: {name}\n{SESSION_BLOCK}start: done\nnodes:\n  done:\n    type: end\n    output: done\n"),
        )
        .unwrap();
    }

    /// A ctx as `run_graph_turn` sees it: a loaded graph agent, whose pool
    /// carries the built-in teammate and user-interaction functions, plus
    /// an engaged session.
    async fn ctx_with_agent_and_session(name: &str) -> RequestContext {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        let app = ctx.app.config.clone();
        ctx.use_agent(&app, name, None, create_abort_signal())
            .await
            .unwrap();
        ctx.session = Some(chat_session());
        ctx
    }

    fn graph(description: &str) -> Graph {
        let yaml = format!(
            "name: probe\ndescription: {description:?}\nstart: done\nnodes:\n  done:\n    type: end\n    output: done\n"
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    fn block(route: Option<RouteBlock>) -> SessionBlock {
        SessionBlock {
            expose: vec![],
            route,
            chat: ChatBlock {
                model: None,
                instructions: "hi".to_string(),
                memory: false,
                tools: vec![],
            },
        }
    }

    fn add_exchange(ctx: &mut RequestContext, question: &str, answer: &str) {
        let input = Input::from_str(ctx, question, None).unwrap();
        ctx.session
            .as_mut()
            .unwrap()
            .add_message(&input, answer)
            .unwrap();
    }

    fn seed_last_message(ctx: &mut RequestContext) {
        let input = Input::from_str(ctx, "earlier", None).unwrap();
        ctx.last_message = Some(LastMessage::new(input, "earlier reply".to_string()));
        ctx.declared_function_names = HashSet::from(["echo".to_string()]);
    }

    fn assert_last_message_is_the_seeded_one(ctx: &RequestContext) {
        let last = ctx.last_message.as_ref().unwrap();
        assert_eq!(last.input.text(), "earlier");
        assert_eq!(last.output, "earlier reply");
        assert!(last.continuous);
        assert_eq!(
            ctx.declared_function_names,
            HashSet::from(["echo".to_string()])
        );
    }

    /// Runs `classify` with a runner that answers `reply` and, like the
    /// production runner's before-hook, overwrites `ctx.last_message` and
    /// `ctx.declared_function_names`; returns the outcome and the inputs
    /// the runner saw.
    async fn classify_with_reply(
        ctx: &mut RequestContext,
        graph: &Graph,
        block: &SessionBlock,
        text: &str,
        reply: &str,
    ) -> (Result<Branch>, Vec<Input>) {
        let mut seen: Vec<Input> = Vec::new();
        let mut runner = completion_runner(|input, ctx, _abort| {
            seen.push(input.clone());
            ctx.last_message = Some(LastMessage::new(input.clone(), String::new()));
            ctx.declared_function_names = input.declared_function_names();
            let reply = reply.to_string();
            boxed_completion(async move { Ok((reply, vec![])) })
        });
        let outcome = classify(ctx, graph, block, text, &create_abort_signal(), &mut runner).await;
        (outcome, seen)
    }

    fn recent_section(text: &str) -> &str {
        let (_, rest) = text.split_once("Recent messages:\n").unwrap();
        rest.rsplit_once("\nNew message: ").unwrap().0
    }

    #[tokio::test]
    async fn classify_maps_the_decision_to_a_branch() {
        let mut ctx = ctx_with_session();
        let graph = graph("");
        let block = block(None);

        let (chat, _) =
            classify_with_reply(&mut ctx, &graph, &block, "x", r#"{"decision":"chat"}"#).await;
        assert_eq!(chat.unwrap(), Branch::Chat);

        let (run, _) =
            classify_with_reply(&mut ctx, &graph, &block, "x", r#"{"decision":"run"}"#).await;
        assert_eq!(run.unwrap(), Branch::Run);
    }

    #[tokio::test]
    async fn classify_accepts_fenced_and_think_wrapped_json() {
        let mut ctx = ctx_with_session();
        let graph = graph("");
        let block = block(None);

        let fenced = "```json\n{\"decision\":\"run\"}\n```";
        let (run, _) = classify_with_reply(&mut ctx, &graph, &block, "x", fenced).await;
        assert_eq!(run.unwrap(), Branch::Run);

        let thought = "<think>hmm</think>\n{\"decision\":\"chat\"}";
        let (chat, _) = classify_with_reply(&mut ctx, &graph, &block, "x", thought).await;
        assert_eq!(chat.unwrap(), Branch::Chat);
    }

    #[tokio::test]
    async fn classify_fails_through_the_extractor_on_prose_and_restores_last_message() {
        let mut ctx = ctx_with_session();
        seed_last_message(&mut ctx);
        let graph = graph("");
        let block = block(None);

        let (outcome, seen) =
            classify_with_reply(&mut ctx, &graph, &block, "x", "I think you should run it").await;

        let err = outcome.unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("Graph route classifier could not read the decision"),
            "{chain}"
        );
        assert!(chain.contains("Structured-output extractor"), "{chain}");
        assert_eq!(
            seen.len(),
            1,
            "the repair path does not go through the runner"
        );
        assert_last_message_is_the_seeded_one(&ctx);
    }

    #[tokio::test]
    async fn classify_wraps_a_failed_model_call_and_restores_last_message() {
        let mut ctx = ctx_with_session();
        seed_last_message(&mut ctx);
        let mut runner = completion_runner(|input, ctx, _abort| {
            ctx.last_message = Some(LastMessage::new(input.clone(), String::new()));
            ctx.declared_function_names = input.declared_function_names();
            boxed_completion(async { Err(anyhow::anyhow!("boom")) })
        });

        let err = classify(
            &mut ctx,
            &graph(""),
            &block(None),
            "x",
            &create_abort_signal(),
            &mut runner,
        )
        .await
        .unwrap_err();

        let chain = format!("{err:#}");
        assert!(
            chain.contains("Graph route classifier call failed"),
            "{chain}"
        );
        assert!(chain.contains("boom"), "{chain}");
        assert_last_message_is_the_seeded_one(&ctx);
    }

    #[tokio::test]
    async fn classify_rejects_an_unknown_decision() {
        let mut ctx = ctx_with_session();
        let graph = graph("");
        let block = block(None);

        let (outcome, _) =
            classify_with_reply(&mut ctx, &graph, &block, "x", r#"{"decision":"maybe"}"#).await;

        assert!(
            outcome
                .unwrap_err()
                .to_string()
                .contains("unrecognised decision")
        );
    }

    #[tokio::test]
    async fn classify_restores_last_message_after_a_decision() {
        let mut ctx = ctx_with_session();
        seed_last_message(&mut ctx);

        let (outcome, _) = classify_with_reply(
            &mut ctx,
            &graph(""),
            &block(None),
            "x",
            r#"{"decision":"run"}"#,
        )
        .await;

        assert_eq!(outcome.unwrap(), Branch::Run);
        assert_last_message_is_the_seeded_one(&ctx);
    }

    #[tokio::test]
    async fn classify_sends_a_detached_one_shot_under_the_route_role() {
        let mut ctx = ctx_with_session();
        add_exchange(&mut ctx, "q1", "r1");

        let (_, seen) = classify_with_reply(
            &mut ctx,
            &graph(""),
            &block(None),
            "x",
            r#"{"decision":"run"}"#,
        )
        .await;

        assert_eq!(seen.len(), 1);
        let input = &seen[0];
        assert!(!input.with_session());
        assert_eq!(input.role().name(), ROUTE_ROLE_NAME);
        assert_eq!(input.role().model().id(), "openai:fc-model");
        assert!(input.declared_function_names().is_empty());
        let system = input.role().prompt();
        assert!(system.contains("perform its task (run)"), "{system}");
        assert!(system.contains(ROUTE_SCHEMA), "{system}");
        assert!(system.is_ascii());
    }

    #[tokio::test]
    #[serial]
    async fn classify_declares_no_functions_even_when_the_agent_has_built_ins() {
        let _guard = TestConfigDirGuard::new("route");
        let name = unique("route_agent");
        write_graph_agent(&name);
        let mut ctx = ctx_with_agent_and_session(&name).await;
        add_exchange(&mut ctx, "q1", "r1");
        assert!(
            !ctx.agent
                .as_ref()
                .unwrap()
                .functions()
                .declarations()
                .is_empty(),
            "the agent pool must offer functions for the classifier to refuse"
        );

        let (outcome, seen) = classify_with_reply(
            &mut ctx,
            &graph(""),
            &block(None),
            "x",
            r#"{"decision":"run"}"#,
        )
        .await;

        assert_eq!(outcome.unwrap(), Branch::Run);
        assert_eq!(seen.len(), 1);
        let input = &seen[0];
        assert_eq!(input.role().name(), ROUTE_ROLE_NAME);
        assert!(!input.with_session());
        assert!(!input.with_agent());
        assert!(
            input.declared_function_names().is_empty(),
            "{:?}",
            input.declared_function_names()
        );
    }

    #[tokio::test]
    async fn classify_prompt_carries_description_instructions_and_has_last_run() {
        let mut ctx = ctx_with_session();
        let described = graph("Writes release notes");
        let with_instructions = block(Some(RouteBlock {
            model: None,
            instructions: Some("Prefer run when a version is named".to_string()),
        }));

        let (_, seen) = classify_with_reply(
            &mut ctx,
            &described,
            &with_instructions,
            "ship 1.2",
            r#"{"decision":"run"}"#,
        )
        .await;
        let text = seen[0].text();
        assert!(text.contains("Agent: Writes release notes\n"), "{text}");
        assert!(
            text.contains("Instructions: Prefer run when a version is named\n"),
            "{text}"
        );
        assert!(text.contains("has_last_run: false\n"), "{text}");
        assert!(!text.contains("Recent messages:"), "{text}");
        assert!(text.ends_with("New message: ship 1.2"), "{text}");

        let result = GraphRunResult {
            output: "done".to_string(),
            final_state: HashMap::new(),
            trace: vec![],
        };
        ctx.session
            .as_mut()
            .unwrap()
            .set_last_run(Some(LastRun::capture(&result, "ship 1.1", &[])));

        let (_, seen) = classify_with_reply(
            &mut ctx,
            &graph(""),
            &block(None),
            "what changed?",
            r#"{"decision":"chat"}"#,
        )
        .await;
        let text = seen[0].text();
        assert!(text.contains("has_last_run: true\n"), "{text}");
        assert!(!text.contains("Agent:"), "{text}");
        assert!(!text.contains("Instructions:"), "{text}");
    }

    #[tokio::test]
    async fn classify_prompt_lists_only_the_most_recent_messages() {
        let mut ctx = ctx_with_session();
        for i in 1..=10 {
            add_exchange(&mut ctx, &format!("q{i}"), &format!("r{i}"));
        }

        let (_, seen) = classify_with_reply(
            &mut ctx,
            &graph(""),
            &block(None),
            "next",
            r#"{"decision":"run"}"#,
        )
        .await;

        let text = seen[0].text();
        let recent = recent_section(&text);
        assert_eq!(
            recent,
            "user: q8\nassistant: r8\nuser: q9\nassistant: r9\nuser: q10\nassistant: r10"
        );
        assert_eq!(recent.lines().count(), ROUTE_CONTEXT_MESSAGES);
    }

    #[tokio::test]
    async fn classify_prompt_keeps_the_newest_bytes_at_a_char_boundary() {
        let mut ctx = ctx_with_session();
        let huge = "\u{4e2d}".repeat(3000);
        add_exchange(&mut ctx, &huge, "tail");

        let (_, seen) = classify_with_reply(
            &mut ctx,
            &graph(""),
            &block(None),
            "next",
            r#"{"decision":"run"}"#,
        )
        .await;

        let text = seen[0].text();
        let recent = recent_section(&text);
        assert!(recent.len() <= ROUTE_CONTEXT_BYTES, "{}", recent.len());
        assert!(recent.len() > ROUTE_CONTEXT_BYTES - 4, "{}", recent.len());
        assert!(recent.starts_with("..."), "{recent}");
        assert!(recent.ends_with("\nassistant: tail"), "{recent}");
        assert!(recent[3..].chars().all(|c| c == '\u{4e2d}' || c.is_ascii()));
    }

    #[test]
    fn truncate_from_front_never_splits_a_char() {
        let text = "e\u{301}".repeat(10);
        for max in 0..=text.len() {
            let cut = truncate_from_front(&text, max);
            assert!(
                cut.len() <= max.max(TRUNCATION_MARK.len()),
                "{max}: {cut:?}"
            );
            assert!(
                text.ends_with(cut.trim_start_matches('.')),
                "{max}: {cut:?}"
            );
        }
        assert_eq!(truncate_from_front("short", 100), "short");
    }

    #[tokio::test]
    #[serial]
    async fn classify_uses_the_route_model_over_the_current_one() {
        let _guard = TestConfigDirGuard::new("route");
        let mut ctx = ctx_with_session();
        let block = block(Some(RouteBlock {
            model: Some("test-seeded:router".to_string()),
            instructions: None,
        }));

        let (_, seen) =
            classify_with_reply(&mut ctx, &graph(""), &block, "x", r#"{"decision":"run"}"#).await;

        assert_eq!(seen[0].role().model().id(), "test-seeded:router");
        assert_eq!(ctx.current_model().id(), "openai:fc-model");
    }

    #[tokio::test]
    async fn classify_rejects_an_unknown_route_model() {
        let mut ctx = ctx_with_session();
        let block = block(Some(RouteBlock {
            model: Some("nope:nope".to_string()),
            instructions: None,
        }));

        let (outcome, seen) =
            classify_with_reply(&mut ctx, &graph(""), &block, "x", r#"{"decision":"run"}"#).await;

        let err = outcome.unwrap_err();
        assert!(
            err.to_string()
                .contains("Unknown model 'nope:nope' on session.route"),
            "{err:#}"
        );
        assert!(seen.is_empty());
    }

    #[test]
    fn routing_line_names_the_branch() {
        assert!(routing_line(Branch::Run).contains("routing: run"));
        assert!(routing_line(Branch::Chat).contains("routing: chat"));
    }
}
