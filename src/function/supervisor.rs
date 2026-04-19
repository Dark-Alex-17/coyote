use super::{FunctionDeclaration, JsonSchema};
use crate::client::{Model, ModelType, call_chat_completions};
use crate::config::{Agent, AppState, Input, RequestContext, Role, RoleLike};
use crate::supervisor::mailbox::{Envelope, EnvelopePayload, Inbox};
use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult, Supervisor};
use crate::utils::{AbortSignal, create_abort_signal};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use indexmap::IndexMap;
use log::debug;
use parking_lot::RwLock;
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

pub const SUPERVISOR_FUNCTION_PREFIX: &str = "agent__";

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
            description: "Spawn a subagent to run in the background. Returns a task_id for tracking. The agent runs in parallel. You can continue working while it executes.".to_string(),
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
            description: "Wait for a spawned agent to finish and return its result. Blocks until the agent completes.".to_string(),
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
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}list"),
            description: "List all currently running subagents and their status.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}cancel"),
            description: "Cancel a running subagent by its ID.".to_string(),
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
        "list" => handle_list(ctx),
        "cancel" => handle_cancel(ctx, args),
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

fn run_child_agent(
    mut child_ctx: RequestContext,
    initial_input: Input,
    abort_signal: AbortSignal,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> {
    Box::pin(async move {
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
                break;
            }

            input = input.merge_tool_results(output, tool_results);
        }

        if let Some(supervisor) = child_ctx.supervisor.clone() {
            supervisor.read().cancel_all();
        }

        Ok(accumulated_output)
    })
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

    let input = Input::from_str(&child_ctx, &prompt, None);

    debug!("Spawning child agent '{agent_name}' as '{agent_id}'");

    let spawn_agent_id = agent_id.clone();
    let spawn_agent_name = agent_name.clone();
    let spawn_abort = child_abort.clone();

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
        "message": format!("Agent '{agent_name}' spawned as '{agent_id}'. Use agent__check or agent__collect to get results."),
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
        Some(false) => Ok(json!({
            "status": "pending",
            "id": id,
            "message": "Agent is still running"
        })),
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

    let handle = {
        let supervisor = ctx
            .supervisor
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow!("No supervisor active"))?;
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
            "message": format!("Agent '{id}' not found. Use agent__check to verify it exists and is finished.")
        })),
    }
}

fn handle_list(ctx: &mut RequestContext) -> Result<Value> {
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

fn handle_cancel(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let supervisor = ctx
        .supervisor
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let mut sup = supervisor.write();

    match sup.take(id) {
        Some(handle) => {
            handle.abort_signal.set_ctrlc();
            Ok(json!({
                "status": "ok",
                "message": format!("Cancelled agent '{}'", handle.agent_name),
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
    let input = Input::from_str(ctx, &user_message, Some(role));

    let summary = input.fetch_chat_text().await?;

    debug!(
        "Summarized output from '{}': {} chars -> {} chars",
        agent_name,
        output.len(),
        summary.len()
    );

    Ok(summary)
}
