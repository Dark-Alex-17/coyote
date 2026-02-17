use super::{FunctionDeclaration, JsonSchema};
use crate::client::{Model, ModelType, call_chat_completions};
use crate::config::{Config, GlobalConfig, Input, Role, RoleLike};
use crate::supervisor::mailbox::{Envelope, EnvelopePayload, Inbox};
use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult};
use crate::utils::{AbortSignal, create_abort_signal};

use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use indexmap::IndexMap;
use log::debug;
use parking_lot::RwLock;
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

pub const SUPERVISOR_FUNCTION_PREFIX: &str = "agent__";

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
                ..Default::default()
            },
            agent: false,
        },
    ]
}

pub async fn handle_supervisor_tool(
    config: &GlobalConfig,
    cmd_name: &str,
    args: &Value,
) -> Result<Value> {
    let action = cmd_name
        .strip_prefix(SUPERVISOR_FUNCTION_PREFIX)
        .unwrap_or(cmd_name);

    match action {
        "spawn" => handle_spawn(config, args).await,
        "check" => handle_check(config, args).await,
        "collect" => handle_collect(config, args).await,
        "list" => handle_list(config),
        "cancel" => handle_cancel(config, args),
        "send_message" => handle_send_message(config, args),
        "check_inbox" => handle_check_inbox(config),
        "task_create" => handle_task_create(config, args),
        "task_list" => handle_task_list(config),
        "task_complete" => handle_task_complete(config, args).await,
        _ => bail!("Unknown supervisor action: {action}"),
    }
}

fn run_child_agent(
    child_config: GlobalConfig,
    initial_input: Input,
    abort_signal: AbortSignal,
) -> Pin<Box<dyn Future<Output = Result<String>> + Send>> {
    Box::pin(async move {
        let mut accumulated_output = String::new();
        let mut input = initial_input;

        loop {
            let client = input.create_client()?;
            child_config.write().before_chat_completion(&input)?;

            let (output, tool_results) =
                call_chat_completions(&input, false, false, client.as_ref(), abort_signal.clone())
                    .await?;

            child_config
                .write()
                .after_chat_completion(&input, &output, &tool_results)?;

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

        Ok(accumulated_output)
    })
}

async fn handle_spawn(config: &GlobalConfig, args: &Value) -> Result<Value> {
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
        let cfg = config.read();
        let supervisor = cfg
            .supervisor
            .as_ref()
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
        (sup.max_depth(), cfg.current_depth + 1)
    };

    if current_depth > max_depth {
        return Ok(json!({
            "status": "error",
            "message": format!("Max agent depth exceeded ({current_depth}/{max_depth})"),
        }));
    }

    let child_inbox = Arc::new(Inbox::new());

    let child_config: GlobalConfig = {
        let mut child_cfg = config.read().clone();

        child_cfg.parent_supervisor = child_cfg.supervisor.clone();
        child_cfg.agent = None;
        child_cfg.session = None;
        child_cfg.rag = None;
        child_cfg.supervisor = None;
        child_cfg.last_message = None;
        child_cfg.tool_call_tracker = None;

        child_cfg.stream = false;
        child_cfg.save = false;
        child_cfg.current_depth = current_depth;
        child_cfg.inbox = Some(Arc::clone(&child_inbox));
        child_cfg.self_agent_id = Some(agent_id.clone());

        Arc::new(RwLock::new(child_cfg))
    };

    let child_abort = create_abort_signal();
    Config::use_agent(&child_config, &agent_name, None, child_abort.clone()).await?;

    let input = Input::from_str(&child_config, &prompt, None);

    debug!("Spawning child agent '{agent_name}' as '{agent_id}'");

    let spawn_agent_id = agent_id.clone();
    let spawn_agent_name = agent_name.clone();
    let spawn_abort = child_abort.clone();

    let join_handle = tokio::spawn(async move {
        let result = run_child_agent(child_config, input, spawn_abort).await;

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

    {
        let cfg = config.read();
        let supervisor = cfg
            .supervisor
            .as_ref()
            .ok_or_else(|| anyhow!("No supervisor active"))?;
        let mut sup = supervisor.write();
        sup.register(handle)?;
    }

    Ok(json!({
        "status": "ok",
        "id": agent_id,
        "agent": agent_name,
        "message": format!("Agent '{agent_name}' spawned as '{agent_id}'. Use agent__check or agent__collect to get results."),
    }))
}

async fn handle_check(config: &GlobalConfig, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let is_finished = {
        let cfg = config.read();
        let supervisor = cfg
            .supervisor
            .as_ref()
            .ok_or_else(|| anyhow!("No supervisor active"))?;
        let sup = supervisor.read();
        sup.is_finished(id)
    };

    match is_finished {
        Some(true) => handle_collect(config, args).await,
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

async fn handle_collect(config: &GlobalConfig, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let handle = {
        let cfg = config.read();
        let supervisor = cfg
            .supervisor
            .as_ref()
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

            let output = summarize_output(config, &result.agent_name, &result.output).await?;

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

fn handle_list(config: &GlobalConfig) -> Result<Value> {
    let cfg = config.read();
    let supervisor = cfg
        .supervisor
        .as_ref()
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

fn handle_cancel(config: &GlobalConfig, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let cfg = config.read();
    let supervisor = cfg
        .supervisor
        .as_ref()
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

fn handle_send_message(config: &GlobalConfig, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'message' is required"))?;

    let cfg = config.read();

    // Determine sender identity: self_agent_id (child), agent name (parent), or "parent"
    let sender = cfg
        .self_agent_id
        .clone()
        .or_else(|| cfg.agent.as_ref().map(|a| a.name().to_string()))
        .unwrap_or_else(|| "parent".to_string());

    // Try local supervisor first (parent → child routing)
    let inbox = cfg
        .supervisor
        .as_ref()
        .and_then(|sup| sup.read().inbox(id).cloned());

    // Fall back to parent_supervisor (sibling → sibling routing)
    let inbox = inbox.or_else(|| {
        cfg.parent_supervisor
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

fn handle_check_inbox(config: &GlobalConfig) -> Result<Value> {
    let cfg = config.read();
    match &cfg.inbox {
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

fn handle_task_create(config: &GlobalConfig, args: &Value) -> Result<Value> {
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

    let cfg = config.read();
    let supervisor = cfg
        .supervisor
        .as_ref()
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

fn handle_task_list(config: &GlobalConfig) -> Result<Value> {
    let cfg = config.read();
    let supervisor = cfg
        .supervisor
        .as_ref()
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

async fn handle_task_complete(config: &GlobalConfig, args: &Value) -> Result<Value> {
    let task_id = args
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'task_id' is required"))?;

    let (newly_runnable, dispatchable) = {
        let cfg = config.read();
        let supervisor = cfg
            .supervisor
            .as_ref()
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
        match handle_spawn(config, &spawn_args).await {
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

const SUMMARIZATION_PROMPT: &str = r#"You are a precise summarization assistant. Your job is to condense a sub-agent's output into a compact summary that preserves all actionable information.

Rules:
- Preserve ALL code snippets, file paths, error messages, and concrete recommendations
- Remove conversational filler, thinking-out-loud, and redundant explanations
- Keep the summary under 30% of the original length
- Use bullet points for multiple findings
- If the output contains a final answer or conclusion, lead with it"#;

async fn summarize_output(config: &GlobalConfig, agent_name: &str, output: &str) -> Result<String> {
    let (threshold, summarization_model_id) = {
        let cfg = config.read();
        match cfg.agent.as_ref() {
            Some(agent) => (
                agent.summarization_threshold(),
                agent.summarization_model().map(|s| s.to_string()),
            ),
            None => return Ok(output.to_string()),
        }
    };

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

    let model = {
        let cfg = config.read();
        match summarization_model_id {
            Some(ref model_id) => Model::retrieve_model(&cfg, model_id, ModelType::Chat)?,
            None => cfg.current_model().clone(),
        }
    };

    let mut role = Role::new("summarizer", SUMMARIZATION_PROMPT);
    role.set_model(model);

    let user_message = format!(
        "Summarize the following sub-agent output from '{}':\n\n{}",
        agent_name, output
    );
    let input = Input::from_str(config, &user_message, Some(role));

    let summary = input.fetch_chat_text().await?;

    debug!(
        "Summarized output from '{}': {} chars -> {} chars",
        agent_name,
        output.len(),
        summary.len()
    );

    Ok(summary)
}
