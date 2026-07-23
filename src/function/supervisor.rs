use super::{FunctionDeclaration, JsonSchema};
use crate::client::{Model, ModelType, call_chat_completions};
use crate::config::{
    Agent, AppState, Input, RequestContext, Role, RoleLike, list_agents_with_descriptions,
};
use crate::supervisor::mailbox::{Envelope, EnvelopePayload, Inbox};
use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult, Supervisor};
use crate::utils::{AbortSignal, create_abort_signal, wait_abort_signal};

use crate::graph;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use indexmap::IndexMap;
use log::debug;
use parking_lot::RwLock;
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tokio::time::Instant;
use uuid::Uuid;

pub const SUPERVISOR_FUNCTION_PREFIX: &str = "agent__";

pub const PENDING_AGENTS_GUARDRAIL_MAX: u32 = 3;

fn agent_permitted(whitelist: Option<&[String]>, target: &str) -> bool {
    match whitelist {
        None => true,
        Some(w) => w.iter().any(|a| a == target),
    }
}

pub enum GuardrailAction {
    NoAction,
    Inject(String),
    ForceTerminate(Vec<String>),
}

pub fn pending_agent_ids(ctx: &RequestContext) -> Vec<String> {
    let Some(sup) = ctx.supervisor.as_ref() else {
        return Vec::new();
    };
    let sup = sup.read();
    sup.list_agents()
        .into_iter()
        .filter_map(|(id, _)| match sup.is_finished(id) {
            Some(false) => Some(id.to_string()),
            _ => None,
        })
        .collect()
}

pub fn build_pending_agents_guardrail_prompt(ids: &[String]) -> String {
    let count = ids.len();
    let id_list = ids
        .iter()
        .map(|id| format!("- {id}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "[SYSTEM GUARDRAIL] You attempted to end your turn while {count} spawned background agent(s) \
         are still running:\n{id_list}\n\nThese agents will be abandoned if your turn ends now. You MUST \
         reclaim each one before ending your turn. For each agent: call `agent__collect` (blocks until \
         done, returns output) or `agent__cancel` (discards). Do NOT emit a text-only response \
         expecting them to 'report back' — they will not."
    )
}

pub fn check_pending_agents_guardrail(ctx: &mut RequestContext) -> GuardrailAction {
    let pending = pending_agent_ids(ctx);
    if pending.is_empty() {
        ctx.pending_agents_guardrail_count = 0;
        return GuardrailAction::NoAction;
    }

    if ctx.pending_agents_guardrail_count >= PENDING_AGENTS_GUARDRAIL_MAX {
        if let Some(sup) = ctx.supervisor.as_ref().cloned() {
            sup.read().cancel_recursive();
        }
        ctx.pending_agents_guardrail_count = 0;

        return GuardrailAction::ForceTerminate(pending);
    }

    ctx.pending_agents_guardrail_count += 1;
    GuardrailAction::Inject(build_pending_agents_guardrail_prompt(&pending))
}

pub fn escalation_function_declarations() -> Vec<FunctionDeclaration> {
    vec![FunctionDeclaration {
        name: format!("{SUPERVISOR_FUNCTION_PREFIX}reply_escalation"),
        description: "Reply to a pending escalation from a child agent. The child is blocked waiting for this reply. Use this after seeing pending_escalations notifications.".to_string(),
        parameters: JsonSchema {
            type_value: Some("object".to_string()),
            properties: Some(IndexMap::from([
                (
                    "escalation_id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The escalation ID from the pending_escalations notification".into()),
                        ..Default::default()
                    },
                ),
                (
                    "reply".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("Your answer to the child agent's question. For ask/confirm questions, use the exact option text. For input questions, provide the text response.".into()),
                        ..Default::default()
                    },
                ),
            ])),
            required: Some(vec!["escalation_id".to_string(), "reply".to_string()]),
            ..Default::default()
        },
        agent: false,
    }]
}

pub fn supervisor_function_declarations() -> Vec<FunctionDeclaration> {
    vec![
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}spawn"),
            description: "Spawn a subagent to run in the background. Returns an `id` immediately so you can continue \
                          working in parallel. CRITICAL: every spawned agent MUST be reclaimed before you end your \
                          turn — call `agent__collect` to retrieve its output, or `agent__cancel` if you no longer \
                          need it. Ending your turn with pending agents will abandon their work and the system will \
                          reject the turn-end.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "agent".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Name of the agent to spawn (e.g. 'explore', 'coder', 'oracle')".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "prompt".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The task prompt to send to the agent".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "task_id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Optional task queue ID to associate with this agent".into()),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["agent".to_string(), "prompt".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}check"),
            description: "Check if a spawned agent has finished. Non-blocking; returns PENDING if still running, or the result if complete.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The agent ID returned by agent__spawn".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}collect"),
            description: "Block until the named spawned agent finishes and return its result. This is your primary \
                          wait primitive — it pauses your execution until the agent completes (or you are interrupted). \
                          Call this for every agent you spawned before ending your turn. Do NOT end your turn assuming \
                          agents will 'report back later' — they will not; they will be abandoned. If you no longer \
                          need an agent's result, call `agent__cancel` instead.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The agent ID returned by agent__spawn".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}list_running"),
            description: "List all subagents YOU have spawned that are still tracked by the supervisor, with their \
                          status. Use this to see which of your background agents are still active. To discover which \
                          agent types you can spawn in the first place, use `agent__list_available` instead.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}list_available"),
            description: "List all agent types installed and available to spawn (name + description). Use this to \
                          discover what specialists exist before calling `agent__spawn` — especially when you're unsure \
                          which agent to delegate to. This is the discovery counterpart to `agent__list_running` \
                          (which reports agents you have already spawned).".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}cancel"),
            description: "Cancel a running subagent by its ID. Use this when an agent's output is no longer needed \
                          (e.g. you changed direction, or you're about to end your turn and don't want to wait). \
                          Cancellation cascades: all of the cancelled agent's own descendants are also cancelled. This \
                          call waits briefly for the agent to actually finish cleanup before returning.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The agent ID to cancel".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}task_create"),
            description: "Create a task in the task queue. Returns the task ID.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "subject".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Short title for the task".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "description".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Detailed description of the task".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "blocked_by".to_string(),
                        JsonSchema {
                            type_value: Some("array".to_string()),
                            description: Some("Task IDs that must complete before this task can run".into()),
                            items: Some(Box::new(JsonSchema {
                                type_value: Some("string".to_string()),
                                ..Default::default()
                            })),
                            ..Default::default()
                        },
                    ),
                    (
                        "agent".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Agent to auto-spawn when this task becomes runnable (e.g. 'explore', 'coder'). If set, an agent will be spawned automatically when all dependencies complete.".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "prompt".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Prompt to send to the auto-spawned agent. Required if agent is set.".into()),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["subject".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}task_list"),
            description: "List all tasks in the task queue with their status and dependencies.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}task_complete"),
            description: "Mark a task as completed. Returns any newly unblocked task IDs.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "task_id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The task ID to mark complete".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["task_id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}task_fail"),
            description: "Mark a task as failed. Dependents will remain blocked.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "task_id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The task ID to mark as failed".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["task_id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
    ]
}

pub fn teammate_function_declarations() -> Vec<FunctionDeclaration> {
    vec![
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}send_message"),
            description: "Send a text message to a sibling or child agent's inbox. Use to share cross-cutting findings or coordinate with teammates.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The target agent ID".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "message".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The message text to send".into()),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["id".to_string(), "message".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}check_inbox"),
            description: "Check for and drain all pending messages in your inbox from sibling agents or your parent.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
    ]
}

pub async fn handle_supervisor_tool(
    ctx: &mut RequestContext,
    cmd_name: &str,
    args: &Value,
) -> Result<Value> {
    let action = cmd_name
        .strip_prefix(SUPERVISOR_FUNCTION_PREFIX)
        .unwrap_or(cmd_name);

    match action {
        "spawn" => handle_spawn(ctx, args).await,
        "check" => handle_check(ctx, args).await,
        "collect" => handle_collect(ctx, args).await,
        "list_running" => handle_list_running(ctx),
        "list_available" => handle_list_available(ctx),
        "cancel" => handle_cancel(ctx, args).await,
        "send_message" => handle_send_message(ctx, args),
        "check_inbox" => handle_check_inbox(ctx),
        "task_create" => handle_task_create(ctx, args),
        "task_list" => handle_task_list(ctx),
        "task_complete" => handle_task_complete(ctx, args).await,
        "task_fail" => handle_task_fail(ctx, args),
        "reply_escalation" => handle_reply_escalation(ctx, args),
        _ => bail!("Unknown supervisor action: {action}"),
    }
}

pub fn run_child_agent(
    mut child_ctx: RequestContext,
    initial_input: Input,
    abort_signal: AbortSignal,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> {
    Box::pin(async move {
        if graph::active_agent_graph_name(&child_ctx).is_some() {
            return graph::run_active_agent_graph(
                &mut child_ctx,
                &initial_input.text(),
                abort_signal,
            )
            .await;
        }

        let mut accumulated_output = String::new();
        let mut input = initial_input;
        let app = Arc::clone(&child_ctx.app.config);

        loop {
            let client = input.create_client()?;
            child_ctx.before_chat_completion(&input)?;

            let (output, tool_results) = call_chat_completions(
                &input,
                false,
                false,
                client.as_ref(),
                &mut child_ctx,
                abort_signal.clone(),
            )
            .await?;

            child_ctx.after_chat_completion(app.as_ref(), &input, &output, &tool_results)?;

            if !output.is_empty() {
                if !accumulated_output.is_empty() {
                    accumulated_output.push('\n');
                }
                accumulated_output.push_str(&output);
            }

            if tool_results.is_empty() {
                match check_pending_agents_guardrail(&mut child_ctx) {
                    GuardrailAction::NoAction => break,
                    GuardrailAction::ForceTerminate(ids) => {
                        log::warn!(
                            "Pending-agent guardrail force-cancelled {} agent(s) after max reminders: {:?}",
                            ids.len(),
                            ids
                        );
                        break;
                    }
                    GuardrailAction::Inject(prompt) => {
                        input = Input::from_str(&child_ctx, &prompt, None)?;
                        continue;
                    }
                }
            }

            input = input.merge_tool_results(output, tool_results);
        }

        if let Some(supervisor) = child_ctx.supervisor.clone() {
            supervisor.read().cancel_recursive();
        }

        Ok(accumulated_output)
    })
}

/// Spawn an agent synchronously from a graph node and return its accumulated
/// output. This is similar to `handle_spawn` but runs the child agent in the
/// current task (no tokio::spawn, no supervisor handle registration) so the
/// graph executor can sequence agent nodes directly.
pub async fn run_agent_for_graph(
    parent_ctx: &mut RequestContext,
    agent_name: &str,
    prompt: &str,
) -> Result<String> {
    let short_uuid = &Uuid::new_v4().to_string()[..8];
    let agent_id = format!("graph_agent_{agent_name}_{short_uuid}");
    let current_depth = parent_ctx.current_depth + 1;

    if let Some(supervisor) = parent_ctx.supervisor.as_ref().cloned() {
        let max_depth = supervisor.read().max_depth();
        if current_depth > max_depth {
            bail!("Max agent depth exceeded ({current_depth}/{max_depth})");
        }
    }

    if !parent_ctx.app.config.function_calling_support {
        bail!("Function calling support must be enabled to spawn agents.");
    }

    let child_inbox = Arc::new(Inbox::new());
    parent_ctx.ensure_root_escalation_queue();
    let child_abort = create_abort_signal();

    let app_config = Arc::clone(&parent_ctx.app.config);
    let current_model = parent_ctx.current_model().clone();
    let info_flag = parent_ctx.info_flag;
    let child_app_state = Arc::new(AppState {
        config: Arc::new(app_config.as_ref().clone()),
        vault: parent_ctx.app.vault.clone(),
        mcp_factory: parent_ctx.app.mcp_factory.clone(),
        rag_cache: parent_ctx.app.rag_cache.clone(),
        mcp_config: parent_ctx.app.mcp_config.clone(),
        mcp_log_path: parent_ctx.app.mcp_log_path.clone(),
        mcp_registry: parent_ctx.app.mcp_registry.clone(),
        functions: parent_ctx.app.functions.clone(),
    });

    let agent = Agent::init(
        app_config.as_ref(),
        child_app_state.as_ref(),
        &current_model,
        info_flag,
        agent_name,
        child_abort.clone(),
    )
    .await?;

    let agent_mcp_servers = agent.mcp_server_names().to_vec();
    let session = agent.agent_session().map(|v| v.to_string());
    let should_init_supervisor = agent.can_spawn_agents();
    let agent_max_concurrent = agent.max_concurrent_agents();
    let agent_max_depth = agent.max_agent_depth();

    let mut child_ctx = RequestContext::new_for_child(
        Arc::clone(&child_app_state),
        parent_ctx,
        current_depth,
        Arc::clone(&child_inbox),
        agent_id.clone(),
    );
    child_ctx.rag = agent.rag();
    child_ctx.agent = Some(agent);
    if should_init_supervisor {
        child_ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(
            agent_max_concurrent,
            agent_max_depth,
        ))));
    }

    if let Some(session) = session {
        child_ctx
            .use_session(app_config.as_ref(), Some(&session), child_abort.clone())
            .await?;
        sync_agent_functions_to_ctx(&mut child_ctx)?;
    } else {
        populate_agent_mcp_runtime(&mut child_ctx, &agent_mcp_servers).await?;
        sync_agent_functions_to_ctx(&mut child_ctx)?;
        child_ctx.init_agent_shared_variables()?;
    }

    let input = Input::from_str(&child_ctx, prompt, None)?;

    debug!("Spawning agent '{agent_name}' for graph node as '{agent_id}'");

    run_child_agent(child_ctx, input, child_abort).await
}

async fn populate_agent_mcp_runtime(ctx: &mut RequestContext, server_ids: &[String]) -> Result<()> {
    if !ctx.app.config.mcp_server_support {
        return Ok(());
    }

    let app = Arc::clone(&ctx.app);
    let server_specs = app
        .mcp_config
        .as_ref()
        .map(|mcp_config| {
            server_ids
                .iter()
                .filter_map(|id| {
                    mcp_config
                        .mcp_servers
                        .get(id)
                        .cloned()
                        .map(|spec| (id.clone(), spec))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    for (id, spec) in server_specs {
        let handle = app
            .mcp_factory
            .acquire(&id, &spec, app.mcp_log_path.as_deref())
            .await?;
        ctx.tool_scope.mcp_runtime.insert(id, handle);
    }

    Ok(())
}

fn sync_agent_functions_to_ctx(ctx: &mut RequestContext) -> Result<()> {
    let server_names = ctx.tool_scope.mcp_runtime.server_names();
    let functions = {
        let agent = ctx
            .agent
            .as_mut()
            .with_context(|| "Agent should be initialized")?;
        if !server_names.is_empty() {
            agent.append_mcp_meta_functions(server_names);
        }
        agent.functions().clone()
    };

    ctx.tool_scope.functions = functions;
    Ok(())
}

async fn handle_spawn(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let agent_name = args
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'agent' is required"))?
        .to_string();
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'prompt' is required"))?
        .to_string();
    let _task_id = args.get("task_id").and_then(Value::as_str);

    if let Some(parent) = ctx.agent.as_ref()
        && !agent_permitted(parent.spawnable_agents(), &agent_name)
    {
        let whitelist = parent.spawnable_agents().unwrap_or_default();
        return Ok(json!({
            "status": "error",
            "message": format!(
                "Agent '{agent_name}' is not in this agent's `spawnable_agents` whitelist. Allowed: {whitelist:?}. Call `agent__list_available` to see what you can spawn."
            ),
        }));
    }

    let short_uuid = &Uuid::new_v4().to_string()[..8];
    let agent_id = format!("agent_{agent_name}_{short_uuid}");

    let (max_depth, current_depth) = {
        let supervisor = ctx
            .supervisor
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("No supervisor active; Agent spawning not enabled"))?;
        let sup = supervisor.read();
        if sup.active_count() >= sup.max_concurrent() {
            return Ok(json!({
                "status": "error",
                "message": format!(
                    "At capacity: {}/{} agents running. Wait for one to finish or cancel one.",
                    sup.active_count(),
                    sup.max_concurrent()
                ),
            }));
        }
        (sup.max_depth(), ctx.current_depth + 1)
    };

    if current_depth > max_depth {
        return Ok(json!({
            "status": "error",
            "message": format!("Max agent depth exceeded ({current_depth}/{max_depth})"),
        }));
    }

    let child_inbox = Arc::new(Inbox::new());

    ctx.ensure_root_escalation_queue();

    let child_abort = create_abort_signal();

    if !ctx.app.config.function_calling_support {
        bail!("Please enable function calling support before using the agent.");
    }

    let app_config = Arc::clone(&ctx.app.config);
    let current_model = ctx.current_model().clone();
    let info_flag = ctx.info_flag;
    let child_app_state = Arc::new(AppState {
        config: Arc::new(app_config.as_ref().clone()),
        vault: ctx.app.vault.clone(),
        mcp_factory: ctx.app.mcp_factory.clone(),
        rag_cache: ctx.app.rag_cache.clone(),
        mcp_config: ctx.app.mcp_config.clone(),
        mcp_log_path: ctx.app.mcp_log_path.clone(),
        mcp_registry: ctx.app.mcp_registry.clone(),
        functions: ctx.app.functions.clone(),
    });
    let agent = Agent::init(
        app_config.as_ref(),
        child_app_state.as_ref(),
        &current_model,
        info_flag,
        &agent_name,
        child_abort.clone(),
    )
    .await?;

    let agent_mcp_servers = agent.mcp_server_names().to_vec();
    let session = agent.agent_session().map(|v| v.to_string());
    let should_init_supervisor = agent.can_spawn_agents();
    let max_concurrent = agent.max_concurrent_agents();
    let max_depth = agent.max_agent_depth();
    let mut child_ctx = RequestContext::new_for_child(
        Arc::clone(&child_app_state),
        ctx,
        current_depth,
        Arc::clone(&child_inbox),
        agent_id.clone(),
    );
    child_ctx.rag = agent.rag();
    child_ctx.agent = Some(agent);
    if should_init_supervisor {
        child_ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(
            max_concurrent,
            max_depth,
        ))));
    }

    if let Some(session) = session {
        child_ctx
            .use_session(app_config.as_ref(), Some(&session), child_abort.clone())
            .await?;
        sync_agent_functions_to_ctx(&mut child_ctx)?;
    } else {
        populate_agent_mcp_runtime(&mut child_ctx, &agent_mcp_servers).await?;
        sync_agent_functions_to_ctx(&mut child_ctx)?;
        child_ctx.init_agent_shared_variables()?;
    }

    let input = Input::from_str(&child_ctx, &prompt, None)?;

    debug!("Spawning child agent '{agent_name}' as '{agent_id}'");

    let spawn_agent_id = agent_id.clone();
    let spawn_agent_name = agent_name.clone();
    let spawn_abort = child_abort.clone();
    let child_supervisor = child_ctx.supervisor.clone();

    let join_handle = tokio::spawn(async move {
        let result = run_child_agent(child_ctx, input, spawn_abort).await;

        match result {
            Ok(output) => Ok(AgentResult {
                id: spawn_agent_id,
                agent_name: spawn_agent_name,
                output,
                exit_status: AgentExitStatus::Completed,
            }),
            Err(e) => Ok(AgentResult {
                id: spawn_agent_id,
                agent_name: spawn_agent_name,
                output: String::new(),
                exit_status: AgentExitStatus::Failed(e.to_string()),
            }),
        }
    });

    let handle = AgentHandle {
        id: agent_id.clone(),
        agent_name: agent_name.clone(),
        depth: current_depth,
        inbox: child_inbox,
        abort_signal: child_abort,
        join_handle,
        child_supervisor,
    };

    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let mut sup = supervisor.write();
    sup.register(handle)?;

    Ok(json!({
        "status": "ok",
        "id": agent_id,
        "agent": agent_name,
        "message": format!("Agent '{agent_name}' spawned as '{agent_id}' and is running in the background. CRITICAL: \
                           you MUST reclaim this agent before ending your turn — call `agent__collect` (blocks until \
                           done, returns output) or `agent__cancel` (if you no longer need it). Ending your turn with \
                           unreclaimed agents will be rejected and forces you to handle them. Do NOT assume the agent \
                           will 'report back' on its own."),
    }))
}

async fn handle_check(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let is_finished = {
        let supervisor = ctx
            .supervisor
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("No supervisor active"))?;
        let sup = supervisor.read();
        sup.is_finished(id)
    };

    match is_finished {
        Some(true) => handle_collect(ctx, args).await,
        Some(false) => {
            let mut result = json!({
                "status": "pending",
                "id": id,
                "message": "Agent is still running"
            });

            if let Some(queue) = ctx.root_escalation_queue()
                && queue.has_pending()
            {
                let summary = queue.pending_summary();
                result["pending_escalations"] = json!(summary);
                result["message"] = json!(
                    "Agent is still running. Child agents have pending escalations that need your reply via agent__reply_escalation."
                );
            }

            Ok(result)
        }
        None => Ok(json!({
            "status": "error",
            "message": format!("No agent found with id '{id}'")
        })),
    }
}

async fn handle_collect(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;

    let target_abort = {
        let sup = supervisor.read();
        if sup.is_finished(id).is_none() {
            return Ok(json!({
                "status": "error",
                "message": format!("Agent '{id}' not found. Use agent__check to verify it exists and is finished.")
            }));
        }
        sup.abort_signal_for(id)
    };

    loop {
        let is_finished = {
            let sup = supervisor.read();
            sup.is_finished(id).unwrap_or(false)
        };

        if is_finished {
            break;
        }

        if let Some(queue) = ctx.root_escalation_queue()
            && queue.has_pending()
        {
            let summary = queue.pending_summary();
            return Ok(json!({
                "status": "pending",
                "id": id,
                "message": format!("Agent '{id}' is still running, but child agents have pending escalations that need your reply. Reply via agent__reply_escalation, then call agent__collect again."),
                "pending_escalations": summary,
            }));
        }

        match target_abort.as_ref() {
            Some(abort) if abort.aborted() => {
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    if supervisor.read().is_finished(id).unwrap_or(false) {
                        break;
                    }
                    time::sleep(Duration::from_millis(50)).await;
                }
                break;
            }
            Some(abort) => {
                tokio::select! {
                    _ = time::sleep(Duration::from_millis(200)) => {}
                    _ = wait_abort_signal(abort) => {}
                }
            }
            None => {
                time::sleep(Duration::from_millis(200)).await;
            }
        }
    }

    let handle = {
        let mut sup = supervisor.write();
        sup.take(id)
    };

    match handle {
        Some(handle) => {
            let result = handle
                .join_handle
                .await
                .map_err(|e| anyhow!("Agent task panicked: {e}"))?
                .map_err(|e| anyhow!("Agent failed: {e}"))?;

            let output = summarize_output(ctx, &result.agent_name, &result.output).await?;
            ctx.pending_agents_guardrail_count = 0;

            Ok(json!({
                "status": "completed",
                "id": result.id,
                "agent": result.agent_name,
                "exit_status": format!("{:?}", result.exit_status),
                "output": output,
            }))
        }
        None => Ok(json!({
            "status": "error",
            "message": format!("Agent '{id}' completed but could not be collected. It may have been collected by another call.")
        })),
    }
}

fn handle_list_running(ctx: &mut RequestContext) -> Result<Value> {
    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let sup = supervisor.read();

    let agents: Vec<Value> = sup
        .list_agents()
        .into_iter()
        .map(|(id, name)| {
            let finished = sup.is_finished(id).unwrap_or(false);
            json!({
                "id": id,
                "agent": name,
                "status": if finished { "finished" } else { "running" },
            })
        })
        .collect();

    Ok(json!({
        "active_count": sup.active_count(),
        "max_concurrent": sup.max_concurrent(),
        "agents": agents,
    }))
}

fn handle_list_available(ctx: &RequestContext) -> Result<Value> {
    let whitelist: Option<Vec<String>> = ctx
        .agent
        .as_ref()
        .and_then(|a| a.spawnable_agents())
        .map(<[String]>::to_vec);

    let entries: Vec<(String, String)> = list_agents_with_descriptions()
        .into_iter()
        .filter(|(name, _)| agent_permitted(whitelist.as_deref(), name))
        .collect();
    let count = entries.len();
    let agents: Vec<Value> = entries
        .into_iter()
        .map(|(name, description)| {
            if description.is_empty() {
                json!({ "name": name })
            } else {
                json!({ "name": name, "description": description })
            }
        })
        .collect();

    Ok(json!({
        "count": count,
        "agents": agents,
    }))
}

async fn handle_cancel(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;

    let handle = {
        let mut sup = supervisor.write();
        sup.take(id)
    };

    match handle {
        Some(handle) => {
            let agent_name = handle.agent_name.clone();
            if let Some(child_sup) = handle.child_supervisor.as_ref() {
                child_sup.read().cancel_recursive();
            }
            handle.abort_signal.set_ctrlc();

            let cleanup = tokio::time::timeout(Duration::from_secs(5), handle.join_handle).await;

            ctx.pending_agents_guardrail_count = 0;

            let message = match cleanup {
                Ok(_) => format!("Cancelled agent '{agent_name}' and waited for cleanup."),
                Err(_) => format!(
                    "Cancelled agent '{agent_name}'; cleanup did not complete within 5s. Its descendants have been signalled and will tear down asynchronously."
                ),
            };

            Ok(json!({
                "status": "ok",
                "message": message,
            }))
        }
        None => Ok(json!({
            "status": "error",
            "message": format!("No agent found with id '{id}'"),
        })),
    }
}

fn handle_send_message(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'message' is required"))?;

    let sender = ctx
        .self_agent_id
        .clone()
        .or_else(|| ctx.agent.as_ref().map(|a| a.name().to_string()))
        .unwrap_or_else(|| "parent".to_string());

    let inbox = ctx
        .supervisor
        .as_ref()
        .and_then(|sup| sup.read().inbox(id).cloned());

    let inbox = inbox.or_else(|| {
        ctx.parent_supervisor
            .as_ref()
            .and_then(|sup| sup.read().inbox(id).cloned())
    });

    match inbox {
        Some(inbox) => {
            inbox.deliver(Envelope {
                from: sender,
                to: id.to_string(),
                payload: EnvelopePayload::Text {
                    content: message.to_string(),
                },
                timestamp: Utc::now(),
            });

            Ok(json!({
                "status": "ok",
                "message": format!("Message delivered to agent '{id}'"),
            }))
        }
        None => Ok(json!({
            "status": "error",
            "message": format!("No agent found with id '{id}'. Agent may not exist or may have already completed."),
        })),
    }
}

fn handle_check_inbox(ctx: &mut RequestContext) -> Result<Value> {
    match ctx.inbox.as_ref() {
        Some(inbox) => {
            let messages: Vec<Value> = inbox
                .drain()
                .into_iter()
                .map(|e| {
                    json!({
                        "from": e.from,
                        "payload": e.payload,
                        "timestamp": e.timestamp.to_rfc3339(),
                    })
                })
                .collect();
            let count = messages.len();
            Ok(json!({
                "messages": messages,
                "count": count,
            }))
        }
        None => Ok(json!({
            "messages": [],
            "count": 0,
        })),
    }
}

fn handle_reply_escalation(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let escalation_id = args
        .get("escalation_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'escalation_id' is required"))?;
    let reply = args
        .get("reply")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'reply' is required"))?;

    let queue = ctx
        .escalation_queue
        .clone()
        .ok_or_else(|| anyhow!("No escalation queue available"))?;

    match queue.take(escalation_id) {
        Some(request) => {
            let from_agent = request.from_agent_name.clone();
            let question = request.question.clone();
            let _ = request.reply_tx.send(reply.to_string());
            Ok(json!({
                "status": "ok",
                "message": format!("Reply sent to agent '{from_agent}' for escalation '{escalation_id}'"),
                "original_question": question,
            }))
        }
        None => Ok(json!({
            "status": "error",
            "message": format!("No pending escalation found with id '{escalation_id}'. It may have already been replied to or timed out."),
        })),
    }
}

fn handle_task_create(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let subject = args
        .get("subject")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'subject' is required"))?;
    let description = args
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let blocked_by: Vec<String> = args
        .get("blocked_by")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let dispatch_agent = args.get("agent").and_then(Value::as_str).map(String::from);
    let task_prompt = args.get("prompt").and_then(Value::as_str).map(String::from);

    if dispatch_agent.is_some() && task_prompt.is_none() {
        bail!("'prompt' is required when 'agent' is set");
    }

    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let mut sup = supervisor.write();

    let task_id = sup.task_queue_mut().create(
        subject.to_string(),
        description.to_string(),
        dispatch_agent.clone(),
        task_prompt,
    );

    let mut dep_errors = vec![];
    for dep_id in &blocked_by {
        if let Err(e) = sup.task_queue_mut().add_dependency(&task_id, dep_id) {
            dep_errors.push(e);
        }
    }

    let mut result = json!({
        "status": "ok",
        "task_id": task_id,
    });

    if dispatch_agent.is_some() {
        result["auto_dispatch"] = json!(true);
    }

    if !dep_errors.is_empty() {
        result["warnings"] = json!(dep_errors);
    }

    Ok(result)
}

fn handle_task_list(ctx: &mut RequestContext) -> Result<Value> {
    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let sup = supervisor.read();

    let tasks: Vec<Value> = sup
        .task_queue()
        .list()
        .into_iter()
        .map(|t| {
            json!({
                "id": t.id,
                "subject": t.subject,
                "status": t.status,
                "owner": t.owner,
                "blocked_by": t.blocked_by.iter().collect::<Vec<_>>(),
                "blocks": t.blocks.iter().collect::<Vec<_>>(),
                "agent": t.dispatch_agent,
                "prompt": t.prompt,
            })
        })
        .collect();

    Ok(json!({ "tasks": tasks }))
}

async fn handle_task_complete(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let task_id = args
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'task_id' is required"))?;

    let (newly_runnable, dispatchable) = {
        let supervisor = ctx
            .supervisor
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("No supervisor active"))?;
        let mut sup = supervisor.write();

        let newly_runnable_ids = sup.task_queue_mut().complete(task_id);

        let mut newly_runnable = Vec::new();
        let mut to_dispatch: Vec<(String, String, String)> = Vec::new();

        for id in &newly_runnable_ids {
            if let Some(t) = sup.task_queue().get(id) {
                newly_runnable.push(json!({
                    "id": t.id,
                    "subject": t.subject,
                    "description": t.description,
                    "agent": t.dispatch_agent,
                }));

                if let (Some(agent), Some(prompt)) = (&t.dispatch_agent, &t.prompt) {
                    to_dispatch.push((id.clone(), agent.clone(), prompt.clone()));
                }
            }
        }

        let mut dispatchable = Vec::new();
        for (tid, agent, prompt) in to_dispatch {
            if sup.task_queue_mut().claim(&tid, &format!("auto:{agent}")) {
                dispatchable.push((agent, prompt));
            }
        }

        (newly_runnable, dispatchable)
    };

    let mut spawned = Vec::new();
    for (agent, prompt) in &dispatchable {
        let spawn_args = json!({
            "agent": agent,
            "prompt": prompt,
        });
        match handle_spawn(ctx, &spawn_args).await {
            Ok(result) => {
                let agent_id = result
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                debug!("Auto-dispatched agent '{}' for task queue", agent_id);
                spawned.push(result);
            }
            Err(e) => {
                spawned.push(json!({
                    "status": "error",
                    "agent": agent,
                    "message": format!("Auto-dispatch failed: {e}"),
                }));
            }
        }
    }

    let mut result = json!({
        "status": "ok",
        "task_id": task_id,
        "newly_runnable": newly_runnable,
    });

    if !spawned.is_empty() {
        result["auto_dispatched"] = json!(spawned);
    }

    Ok(result)
}

fn handle_task_fail(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let task_id = args
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'task_id' is required"))?;

    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let mut sup = supervisor.write();

    let task = sup.task_queue().get(task_id);
    if task.is_none() {
        return Ok(json!({
            "status": "error",
            "message": format!("Task '{task_id}' not found"),
        }));
    }

    let blocked_dependents: Vec<String> = task.unwrap().blocks.iter().cloned().collect();

    sup.task_queue_mut().fail(task_id);

    Ok(json!({
        "status": "ok",
        "task_id": task_id,
        "blocked_dependents": blocked_dependents,
        "message": format!("Task '{task_id}' marked as failed. {} dependent task(s) will remain blocked.", blocked_dependents.len()),
    }))
}

const SUMMARIZATION_PROMPT: &str = r#"You are a precise summarization assistant. Your job is to condense a sub-agent's output into a compact summary that preserves all actionable information.

Rules:
- Preserve ALL code snippets, file paths, error messages, and concrete recommendations
- Remove conversational filler, thinking-out-loud, and redundant explanations
- Keep the summary under 30% of the original length
- Use bullet points for multiple findings
- If the output contains a final answer or conclusion, lead with it"#;

async fn summarize_output(ctx: &RequestContext, agent_name: &str, output: &str) -> Result<String> {
    let Some(agent) = ctx.agent.as_ref() else {
        return Ok(output.to_string());
    };
    let threshold = agent.summarization_threshold();
    let summarization_model_id = agent.summarization_model().map(|s| s.to_string());

    if output.len() < threshold {
        debug!(
            "Output from '{}' is {} chars (threshold {}), skipping summarization",
            agent_name,
            output.len(),
            threshold
        );
        return Ok(output.to_string());
    }

    debug!(
        "Output from '{}' is {} chars (threshold {}), summarizing...",
        agent_name,
        output.len(),
        threshold
    );

    let model = match summarization_model_id {
        Some(ref model_id) => {
            Model::retrieve_model(ctx.app.config.as_ref(), model_id, ModelType::Chat)?
        }
        None => ctx.current_model().clone(),
    };

    let mut role = Role::new("summarizer", SUMMARIZATION_PROMPT);
    role.set_model(model);

    let user_message = format!(
        "Summarize the following sub-agent output from '{}':\n\n{}",
        agent_name, output
    );
    let input = Input::from_str(ctx, &user_message, Some(role))?;

    let summary = input.fetch_chat_text().await?;

    debug!(
        "Summarized output from '{}': {} chars -> {} chars",
        agent_name,
        output.len(),
        summary.len()
    );

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppState, WorkingMode};
    use crate::supervisor::escalation::{EscalationQueue, EscalationRequest};
    use serde_json::json;
    use serial_test::serial;

    fn default_app_state() -> Arc<AppState> {
        Arc::new(AppState::test_default())
    }

    fn ctx_with_supervisor(max_concurrent: usize, max_depth: usize) -> RequestContext {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.supervisor = Some(Arc::new(RwLock::new(Supervisor::new(
            max_concurrent,
            max_depth,
        ))));
        ctx
    }

    fn register_fake_agent(ctx: &mut RequestContext, id: &str, name: &str) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let id_owned = id.to_string();
        let name_owned = name.to_string();
        let join_handle = rt.spawn(async move {
            Ok(AgentResult {
                id: id_owned,
                agent_name: name_owned,
                output: "fake output".into(),
                exit_status: AgentExitStatus::Completed,
            })
        });
        std::mem::forget(rt);

        let handle = AgentHandle {
            id: id.to_string(),
            agent_name: name.to_string(),
            depth: 1,
            inbox: Arc::new(Inbox::new()),
            abort_signal: create_abort_signal(),
            join_handle,
            child_supervisor: None,
        };
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(handle)
            .unwrap();
    }

    fn run_async<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn handle_list_running_empty_supervisor() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_list_running(&mut ctx).unwrap();
        assert_eq!(result["active_count"], 0);
        assert_eq!(result["max_concurrent"], 4);
        assert!(result["agents"].as_array().unwrap().is_empty());
    }

    #[test]
    fn handle_list_running_with_agents() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        register_fake_agent(&mut ctx, "a2", "coder");
        let result = handle_list_running(&mut ctx).unwrap();
        assert_eq!(result["active_count"], 2);
        let agents = result["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 2);
    }

    #[test]
    fn handle_list_running_no_supervisor_errors() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        let result = handle_list_running(&mut ctx);
        assert!(result.is_err());
    }

    #[test]
    fn handle_list_available_returns_shape() {
        let ctx = ctx_with_supervisor(4, 3);

        let result = handle_list_available(&ctx).unwrap();

        assert!(result["count"].is_number());
        assert!(result["agents"].is_array());
    }

    #[test]
    #[serial]
    fn handle_list_available_unrestricted_when_no_whitelist() {
        let ctx = ctx_with_supervisor(4, 3);
        let result = handle_list_available(&ctx).unwrap();

        let full_count = result["count"].as_u64().unwrap();

        assert_eq!(full_count as usize, list_agents_with_descriptions().len());
    }

    #[test]
    fn agent_permitted_none_whitelist_allows_all() {
        assert!(agent_permitted(None, "explore"));
        assert!(agent_permitted(None, "anything"));
    }

    #[test]
    fn agent_permitted_empty_whitelist_denies_all() {
        let empty: Vec<String> = vec![];

        assert!(!agent_permitted(Some(&empty), "explore"));
    }

    #[test]
    fn agent_permitted_named_whitelist_matches_exact() {
        let allowed = vec!["explore".to_string(), "coder".to_string()];

        assert!(agent_permitted(Some(&allowed), "explore"));
        assert!(agent_permitted(Some(&allowed), "coder"));
        assert!(!agent_permitted(Some(&allowed), "oracle"));
        assert!(!agent_permitted(Some(&allowed), "Explore"));
    }

    #[test]
    fn handle_check_unknown_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_check(&mut ctx, &json!({"id": "nonexistent"})));
        let val = result.unwrap();
        assert_eq!(val["status"], "error");
    }

    #[test]
    fn handle_check_pending_agent() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let inbox = Arc::new(Inbox::new());
            let abort = create_abort_signal();
            let join_handle = tokio::spawn(async {
                time::sleep(Duration::from_secs(60)).await;
                Ok(AgentResult {
                    id: "slow".into(),
                    agent_name: "test".into(),
                    output: String::new(),
                    exit_status: AgentExitStatus::Completed,
                })
            });
            let handle = AgentHandle {
                id: "slow".into(),
                agent_name: "test".into(),
                depth: 1,
                inbox,
                abort_signal: abort,
                join_handle,
                child_supervisor: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            let result = handle_check(&mut ctx, &json!({"id": "slow"}))
                .await
                .unwrap();
            assert_eq!(result["status"], "pending");
        });
    }

    #[test]
    fn handle_cancel_registered_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        let result = run_async(handle_cancel(&mut ctx, &json!({"id": "a1"}))).unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(ctx.supervisor.as_ref().unwrap().read().active_count(), 0);
    }

    #[test]
    fn handle_cancel_unknown_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_cancel(&mut ctx, &json!({"id": "missing"}))).unwrap();
        assert_eq!(result["status"], "error");
    }

    #[test]
    fn handle_cancel_no_supervisor_errors() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        let result = run_async(handle_cancel(&mut ctx, &json!({"id": "x"})));
        assert!(result.is_err());
    }

    #[test]
    fn handle_send_message_to_registered_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        let result = handle_send_message(
            &mut ctx,
            &json!({"id": "a1", "message": "hello from parent"}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");

        let inbox = ctx
            .supervisor
            .as_ref()
            .unwrap()
            .read()
            .inbox("a1")
            .unwrap()
            .clone();
        let msgs = inbox.drain();
        assert_eq!(msgs.len(), 1);
        match &msgs[0].payload {
            EnvelopePayload::Text { content } => assert_eq!(content, "hello from parent"),
            _ => panic!("expected text payload"),
        }
    }

    #[test]
    fn handle_send_message_to_unknown_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result =
            handle_send_message(&mut ctx, &json!({"id": "missing", "message": "hi"})).unwrap();
        assert_eq!(result["status"], "error");
    }

    #[test]
    fn handle_check_inbox_with_messages() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let inbox = Arc::new(Inbox::new());
        inbox.deliver(Envelope {
            from: "sibling".into(),
            to: "me".into(),
            payload: EnvelopePayload::Text {
                content: "hey".into(),
            },
            timestamp: Utc::now(),
        });
        ctx.inbox = Some(inbox);

        let result = handle_check_inbox(&mut ctx).unwrap();
        assert_eq!(result["count"], 1);
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages[0]["from"], "sibling");
    }

    #[test]
    fn handle_check_inbox_no_inbox() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_check_inbox(&mut ctx).unwrap();
        assert_eq!(result["count"], 0);
    }

    #[test]
    fn handle_check_inbox_empty_inbox() {
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.inbox = Some(Arc::new(Inbox::new()));
        let result = handle_check_inbox(&mut ctx).unwrap();
        assert_eq!(result["count"], 0);
    }

    #[test]
    fn handle_reply_escalation_success() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let queue = Arc::new(EscalationQueue::new());
        let (tx, rx) = tokio::sync::oneshot::channel();
        queue.submit(EscalationRequest {
            id: "esc_1".into(),
            from_agent_id: "a1".into(),
            from_agent_name: "explore".into(),
            question: "What do?".into(),
            options: None,
            reply_tx: tx,
        });
        ctx.escalation_queue = Some(queue);

        let result = handle_reply_escalation(
            &mut ctx,
            &json!({"escalation_id": "esc_1", "reply": "do X"}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(rx.blocking_recv().unwrap(), "do X");
    }

    #[test]
    fn handle_reply_escalation_missing_id() {
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.escalation_queue = Some(Arc::new(EscalationQueue::new()));
        let result = handle_reply_escalation(
            &mut ctx,
            &json!({"escalation_id": "missing", "reply": "whatever"}),
        )
        .unwrap();
        assert_eq!(result["status"], "error");
    }

    #[test]
    fn handle_reply_escalation_no_queue_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result =
            handle_reply_escalation(&mut ctx, &json!({"escalation_id": "x", "reply": "y"}));
        assert!(result.is_err());
    }

    #[test]
    fn handle_task_create_simple() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_task_create(&mut ctx, &json!({"subject": "Do research"})).unwrap();
        assert_eq!(result["status"], "ok");
        assert!(result["task_id"].as_str().is_some());
    }

    #[test]
    fn handle_task_create_with_dependencies() {
        let mut ctx = ctx_with_supervisor(4, 3);
        handle_task_create(&mut ctx, &json!({"subject": "Step 1"})).unwrap();
        let result =
            handle_task_create(&mut ctx, &json!({"subject": "Step 2", "blocked_by": ["1"]}))
                .unwrap();
        assert_eq!(result["status"], "ok");
    }

    #[test]
    fn handle_task_create_with_dispatch_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_task_create(
            &mut ctx,
            &json!({"subject": "Auto task", "agent": "coder", "prompt": "do it"}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["auto_dispatch"], true);
    }

    #[test]
    fn handle_task_create_agent_without_prompt_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_task_create(&mut ctx, &json!({"subject": "Bad", "agent": "coder"}));
        assert!(result.is_err());
    }

    #[test]
    fn handle_task_list_empty() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_task_list(&mut ctx).unwrap();
        assert!(result["tasks"].as_array().unwrap().is_empty());
    }

    #[test]
    fn handle_task_list_with_tasks() {
        let mut ctx = ctx_with_supervisor(4, 3);
        handle_task_create(&mut ctx, &json!({"subject": "A"})).unwrap();
        handle_task_create(&mut ctx, &json!({"subject": "B"})).unwrap();
        let result = handle_task_list(&mut ctx).unwrap();
        assert_eq!(result["tasks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn handle_task_complete_unblocks_dependents() {
        let mut ctx = ctx_with_supervisor(4, 3);
        handle_task_create(&mut ctx, &json!({"subject": "Step 1"})).unwrap();
        handle_task_create(&mut ctx, &json!({"subject": "Step 2", "blocked_by": ["1"]})).unwrap();

        let result = run_async(handle_task_complete(&mut ctx, &json!({"task_id": "1"}))).unwrap();
        assert_eq!(result["status"], "ok");
        let newly_runnable = result["newly_runnable"].as_array().unwrap();
        assert_eq!(newly_runnable.len(), 1);
        assert_eq!(newly_runnable[0]["id"], "2");
    }

    #[test]
    fn handle_task_fail_marks_failed() {
        let mut ctx = ctx_with_supervisor(4, 3);
        handle_task_create(&mut ctx, &json!({"subject": "Doomed"})).unwrap();
        let result = handle_task_fail(&mut ctx, &json!({"task_id": "1"})).unwrap();
        assert_eq!(result["status"], "ok");
    }

    #[test]
    fn handle_task_fail_reports_blocked_dependents() {
        let mut ctx = ctx_with_supervisor(4, 3);
        handle_task_create(&mut ctx, &json!({"subject": "A"})).unwrap();
        handle_task_create(&mut ctx, &json!({"subject": "B", "blocked_by": ["1"]})).unwrap();
        let result = handle_task_fail(&mut ctx, &json!({"task_id": "1"})).unwrap();
        let deps = result["blocked_dependents"].as_array().unwrap();
        assert_eq!(deps.len(), 1);
    }

    #[test]
    fn handle_task_fail_missing_task() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = handle_task_fail(&mut ctx, &json!({"task_id": "nonexistent"})).unwrap();
        assert_eq!(result["status"], "error");
    }

    #[test]
    fn dispatch_unknown_action_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_supervisor_tool(&mut ctx, "agent__bogus", &json!({})));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unknown supervisor action")
        );
    }

    #[test]
    fn dispatch_routes_list_running() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_supervisor_tool(
            &mut ctx,
            "agent__list_running",
            &json!({}),
        ))
        .unwrap();
        assert!(result["active_count"].is_number());
    }

    #[test]
    fn dispatch_routes_list_available() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_supervisor_tool(
            &mut ctx,
            "agent__list_available",
            &json!({}),
        ))
        .unwrap();
        assert!(result["count"].is_number());
        assert!(result["agents"].is_array());
    }

    #[test]
    fn dispatch_routes_task_list() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_supervisor_tool(
            &mut ctx,
            "agent__task_list",
            &json!({}),
        ))
        .unwrap();
        assert!(result["tasks"].is_array());
    }

    #[test]
    fn new_for_child_inherits_escalation_queue() {
        let mut parent = ctx_with_supervisor(4, 3);
        let queue = parent.ensure_root_escalation_queue();

        let child = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            2,
            Arc::new(Inbox::new()),
            "child_1".into(),
        );

        assert!(child.escalation_queue.is_some());
        assert!(Arc::ptr_eq(
            child.escalation_queue.as_ref().unwrap(),
            &queue
        ));
    }

    #[test]
    fn new_for_child_sets_depth_and_id() {
        let parent = ctx_with_supervisor(4, 3);
        let child = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            3,
            Arc::new(Inbox::new()),
            "child_xyz".into(),
        );
        assert_eq!(child.current_depth, 3);
        assert_eq!(child.self_agent_id, Some("child_xyz".to_string()));
    }

    #[test]
    fn new_for_child_has_inbox() {
        let parent = ctx_with_supervisor(4, 3);
        let inbox = Arc::new(Inbox::new());
        let child = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            1,
            Arc::clone(&inbox),
            "c1".into(),
        );
        assert!(child.inbox.is_some());
        assert!(Arc::ptr_eq(child.inbox.as_ref().unwrap(), &inbox));
    }

    #[test]
    fn new_for_child_inherits_parent_supervisor() {
        let parent = ctx_with_supervisor(4, 3);
        let child = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            1,
            Arc::new(Inbox::new()),
            "c1".into(),
        );
        assert!(child.parent_supervisor.is_some());
        assert!(child.supervisor.is_none());
    }

    #[test]
    fn new_for_child_starts_with_empty_scope() {
        let parent = ctx_with_supervisor(4, 3);
        let child = RequestContext::new_for_child(
            default_app_state(),
            &parent,
            1,
            Arc::new(Inbox::new()),
            "c1".into(),
        );
        assert!(child.tool_scope.functions.is_empty());
        assert!(child.tool_scope.mcp_runtime.is_empty());
        assert!(child.role.is_none());
        assert!(child.session.is_none());
        assert!(child.agent.is_none());
    }

    #[test]
    fn ensure_root_escalation_queue_creates_on_first_call() {
        let mut ctx = ctx_with_supervisor(4, 3);
        assert!(ctx.escalation_queue.is_none());
        let q = ctx.ensure_root_escalation_queue();
        assert!(!q.has_pending());
        assert!(ctx.escalation_queue.is_some());
    }

    #[test]
    fn ensure_root_escalation_queue_returns_same_on_second_call() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let q1 = ctx.ensure_root_escalation_queue();
        let q2 = ctx.ensure_root_escalation_queue();
        assert!(Arc::ptr_eq(&q1, &q2));
    }
}
