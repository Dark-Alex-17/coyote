use super::{FunctionDeclaration, JsonSchema};
use crate::client::call_chat_completions;
use crate::config::{Config, GlobalConfig, Input};
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
            name: format!("{SUPERVISOR_FUNCTION_PREFIX}send_message"),
            description: "Send a text message to a running subagent's inbox.".to_string(),
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
            description: "Check for and drain all pending messages in your inbox.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
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

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

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
        "task_complete" => handle_task_complete(config, args),
        _ => bail!("Unknown supervisor action: {action}"),
    }
}

// ---------------------------------------------------------------------------
// Child agent execution loop
// ---------------------------------------------------------------------------

/// Run a child agent to completion, returning its accumulated text output.
///
/// This mirrors `start_directive` in main.rs but:
///   - Uses `call_chat_completions(print=false)` so nothing goes to stdout
///   - Returns the output text instead of printing it
///   - Loops on tool calls just like `start_directive`'s recursion
///
/// Returns a boxed future to break the recursive type cycle:
///   handle_spawn → tokio::spawn(run_child_agent) → call_chat_completions
///   → eval_tool_calls → ToolCall::eval → handle_supervisor_tool → handle_spawn
/// Without boxing, the compiler cannot prove Send for the recursive async type.
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

            let (output, tool_results) = call_chat_completions(
                &input,
                false, // print=false — silent, no stdout
                false, // extract_code=false
                client.as_ref(),
                abort_signal.clone(),
            )
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

            // Feed tool results back for the next round (mirrors start_directive recursion)
            input = input.merge_tool_results(output, tool_results);
        }

        Ok(accumulated_output)
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

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

    // Generate a unique agent ID
    let short_uuid = &Uuid::new_v4().to_string()[..8];
    let agent_id = format!("agent_{agent_name}_{short_uuid}");

    // --- Validate capacity ---
    // Read the supervisor to check capacity, then drop locks before doing async work
    let (max_depth, current_depth) = {
        let cfg = config.read();
        let supervisor = cfg
            .supervisor
            .as_ref()
            .ok_or_else(|| anyhow!("No supervisor active — agent spawning not enabled"))?;
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

    // --- Build an isolated child Config ---
    let child_inbox = Arc::new(Inbox::new());

    let child_config: GlobalConfig = {
        let mut child_cfg = config.read().clone();

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

        Arc::new(RwLock::new(child_cfg))
    };

    // Load the target agent into the child config
    let child_abort = create_abort_signal();
    Config::use_agent(&child_config, &agent_name, None, child_abort.clone()).await?;

    // Create the initial input from the prompt
    let input = Input::from_str(&child_config, &prompt, None);

    debug!("Spawning child agent '{agent_name}' as '{agent_id}'");

    // --- Spawn the agent task ---
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

    // Register the handle with the supervisor
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
        Some(true) => {
            // Finished — collect the result
            handle_collect(config, args).await
        }
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

    // Extract the join handle while holding the lock, then drop the lock before awaiting
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
            // Await the join handle OUTSIDE of any lock
            let result = handle
                .join_handle
                .await
                .map_err(|e| anyhow!("Agent task panicked: {e}"))?
                .map_err(|e| anyhow!("Agent failed: {e}"))?;

            Ok(json!({
                "status": "completed",
                "id": result.id,
                "agent": result.agent_name,
                "exit_status": format!("{:?}", result.exit_status),
                "output": result.output,
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
    let supervisor = cfg
        .supervisor
        .as_ref()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let sup = supervisor.read();

    match sup.inbox(id) {
        Some(inbox) => {
            let parent_name = cfg
                .agent
                .as_ref()
                .map(|a| a.name().to_string())
                .unwrap_or_else(|| "parent".to_string());

            inbox.deliver(Envelope {
                from: parent_name,
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
            "message": format!("No agent found with id '{id}'"),
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

    let cfg = config.read();
    let supervisor = cfg
        .supervisor
        .as_ref()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let mut sup = supervisor.write();

    let task_id = sup
        .task_queue_mut()
        .create(subject.to_string(), description.to_string());

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
            })
        })
        .collect();

    Ok(json!({ "tasks": tasks }))
}

fn handle_task_complete(config: &GlobalConfig, args: &Value) -> Result<Value> {
    let task_id = args
        .get("task_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'task_id' is required"))?;

    let cfg = config.read();
    let supervisor = cfg
        .supervisor
        .as_ref()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let mut sup = supervisor.write();

    let newly_runnable_ids = sup.task_queue_mut().complete(task_id);

    let newly_runnable: Vec<Value> = newly_runnable_ids
        .iter()
        .filter_map(|id| {
            sup.task_queue().get(id).map(|t| {
                json!({
                    "id": t.id,
                    "subject": t.subject,
                    "description": t.description,
                })
            })
        })
        .collect();

    Ok(json!({
        "status": "ok",
        "task_id": task_id,
        "newly_runnable": newly_runnable,
    }))
}
