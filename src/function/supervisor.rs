use super::{FunctionDeclaration, JsonSchema};
use crate::config::GlobalConfig;
use crate::supervisor::mailbox::{Envelope, EnvelopePayload};

use anyhow::{Result, bail, anyhow};
use chrono::Utc;
use indexmap::IndexMap;
use serde_json::{Value, json};

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

pub fn handle_supervisor_tool(
    config: &GlobalConfig,
    cmd_name: &str,
    args: &Value,
) -> Result<Value> {
    let action = cmd_name
        .strip_prefix(SUPERVISOR_FUNCTION_PREFIX)
        .unwrap_or(cmd_name);

    match action {
        "spawn" => handle_spawn(config, args),
        "check" => handle_check(config, args),
        "collect" => handle_collect(config, args),
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

fn handle_spawn(_config: &GlobalConfig, args: &Value) -> Result<Value> {
    let _agent_name = args
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'agent' is required"))?;
    let _prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'prompt' is required"))?;
    let _task_id = args.get("task_id").and_then(Value::as_str);

    // TODO: Step 3 — actual agent spawning via tokio::spawn
    // For now, return a placeholder so the tool declarations compile and wire up
    Ok(json!({
        "status": "error",
        "message": "agent__spawn is not yet implemented — coming in Step 3"
    }))
}

fn handle_check(config: &GlobalConfig, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let cfg = config.read();
    let supervisor = cfg
        .supervisor
        .as_ref()
        .ok_or_else(|| anyhow!("No supervisor active"))?;
    let sup = supervisor.read();

    match sup.is_finished(id) {
        Some(true) => {
            drop(sup);
            drop(cfg);
            handle_collect(config, args)
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

fn handle_collect(config: &GlobalConfig, args: &Value) -> Result<Value> {
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

    match sup.take_if_finished(id) {
        Some(handle) => {
            drop(sup);
            drop(cfg);

            let rt = tokio::runtime::Handle::current();
            let result = rt
                .block_on(handle.join_handle)
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
            "message": format!("Agent '{id}' not found or still running. Use agent__check first.")
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

fn handle_check_inbox(_config: &GlobalConfig) -> Result<Value> {
    // The parent agent's own inbox — will be wired when we plumb Inbox into Agent
    // For now, return empty
    Ok(json!({
        "messages": [],
        "count": 0,
    }))
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

    let newly_runnable = sup.task_queue_mut().complete(task_id);

    Ok(json!({
        "status": "ok",
        "task_id": task_id,
        "newly_runnable": newly_runnable,
    }))
}
