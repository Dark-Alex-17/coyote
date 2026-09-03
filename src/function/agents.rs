use super::todo::TODO_FUNCTION_PREFIX;
use super::{FunctionDeclaration, JsonSchema};
use crate::client::{Model, ModelType, call_chat_completions};
use crate::config::{
    Agent, AppState, Input, RequestContext, Role, RoleLike, effective_max_concurrent_jobs,
    jobs_enabled, list_agents_with_descriptions,
};
use crate::supervisor::mailbox::{Envelope, EnvelopePayload, Inbox};
use crate::supervisor::notification::agent_notification;
use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult, Supervisor, TaskKind};
use crate::utils::{AbortSignal, create_abort_signal, wait_abort_signal, wait_user_interrupt};

use crate::graph;
use crate::repl::DEFAULT_CONTINUATION_PROMPT;
use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use indexmap::IndexMap;
use log::{debug, warn};
use parking_lot::RwLock;
use serde_json::{Value, json};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;
use tokio::time::Instant;
use uuid::Uuid;

pub const AGENT_FUNCTION_PREFIX: &str = "agent__";

pub const PENDING_TASKS_GUARDRAIL_MAX: u32 = 3;

pub const PARENT_ALIAS: &str = "parent";

fn agent_permitted(whitelist: Option<&[String]>, target: &str) -> bool {
    match whitelist {
        None => true,
        Some(w) => w.iter().any(|a| a == target),
    }
}

fn is_job_task(supervisor: Option<&Arc<RwLock<Supervisor>>>, id: &str) -> bool {
    id.starts_with("job_") || supervisor.is_some_and(|sup| sup.read().has_job(id))
}

fn job_id_teaching_error(id: &str) -> Value {
    json!({
        "status": "error",
        "message": format!(
            "'{id}' is a background job, not an agent — use job__check / job__collect / job__cancel"
        ),
    })
}

pub enum GuardrailAction {
    NoAction,
    Inject(String),
    ForceTerminate(Vec<String>),
}

pub struct PendingTask {
    pub id: String,
    pub kind: TaskKind,
    pub finished: bool,
}

pub fn pending_tasks(ctx: &RequestContext) -> Vec<PendingTask> {
    let Some(sup) = ctx.supervisor.as_ref() else {
        return Vec::new();
    };
    let mut tasks: Vec<PendingTask> = sup
        .read()
        .list_tasks()
        .into_iter()
        .map(|(id, kind, finished)| PendingTask {
            id: id.to_string(),
            kind,
            finished,
        })
        .collect();

    // Inside a graph LLM node, jobs are node-owned: the guardrail must only
    // nag about jobs this node started. Jobs belonging to a parallel branch
    // live in the same shared registry but are that branch's to reclaim.
    if let Some(scope) = ctx.node_job_scope.as_ref() {
        tasks.retain(|t| t.kind != TaskKind::Job || scope.contains(&t.id));
    }

    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    tasks
}

pub fn build_pending_tasks_guardrail_prompt(tasks: &[PendingTask]) -> String {
    let running: Vec<&PendingTask> = tasks.iter().filter(|t| !t.finished).collect();
    let finished: Vec<&PendingTask> = tasks.iter().filter(|t| t.finished).collect();

    let mut sections = Vec::new();
    if !running.is_empty() {
        let id_list = running
            .iter()
            .map(|t| {
                let (kind, collect, cancel) = match t.kind {
                    TaskKind::Agent => ("agent", "agent__collect", "agent__cancel"),
                    TaskKind::Job => ("job", "job__collect", "job__cancel"),
                };
                format!(
                    "- {id} ({kind}): call `{collect}` (blocks until done, returns output) or \
                     `{cancel}` (discards)",
                    id = t.id
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!(
            "Still running ({count}):\n{id_list}\n\nThese will be abandoned if your turn ends \
             now. You MUST reclaim each one before ending your turn. Do NOT emit a text-only \
             response expecting them to 'report back' — they will not.",
            count = running.len()
        ));
    }

    if !finished.is_empty() {
        let cmd_list = finished
            .iter()
            .map(|t| {
                let collect = match t.kind {
                    TaskKind::Agent => "agent__collect",
                    TaskKind::Job => "job__collect",
                };
                format!("- `{collect} --id {id}`", id = t.id)
            })
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!(
            "Completed but UNCOLLECTED — collect NOW ({count}):\n{cmd_list}\n\nCollect returns \
             instantly on a finished task. Their results are LOST if your turn ends without \
             collecting.",
            count = finished.len()
        ));
    }

    format!(
        "[SYSTEM GUARDRAIL] You attempted to end your turn with {count} unreclaimed background \
         task(s).\n\n{body}",
        count = tasks.len(),
        body = sections.join("\n\n")
    )
}

pub fn check_pending_tasks_guardrail(ctx: &mut RequestContext) -> GuardrailAction {
    let pending = pending_tasks(ctx);
    if pending.is_empty() {
        ctx.pending_tasks_guardrail_count = 0;
        return GuardrailAction::NoAction;
    }

    if ctx.pending_tasks_guardrail_count >= PENDING_TASKS_GUARDRAIL_MAX {
        if let Some(sup) = ctx.supervisor.as_ref().cloned() {
            sup.read().cancel_recursive();
            let finished: Vec<&PendingTask> = pending.iter().filter(|t| t.finished).collect();
            if !finished.is_empty() {
                let ids: Vec<&str> = finished.iter().map(|t| t.id.as_str()).collect();
                warn!(
                    "Turn-end guardrail: discarding uncollected result(s) for finished task(s) \
                     after max reminders: {ids:?}"
                );
                let mut sup = sup.write();
                for task in &finished {
                    match task.kind {
                        TaskKind::Agent => {
                            let _ = sup.take(&task.id);
                        }
                        TaskKind::Job => {
                            let _ = sup.take_job(&task.id);
                        }
                    }
                }
            }
        }
        ctx.pending_tasks_guardrail_count = 0;

        return GuardrailAction::ForceTerminate(pending.into_iter().map(|t| t.id).collect());
    }

    ctx.pending_tasks_guardrail_count += 1;
    let mut prompt = build_pending_tasks_guardrail_prompt(&pending);
    if let Some(queue) = ctx.root_escalation_queue()
        && queue.has_pending()
    {
        let summary = serde_json::to_string(&queue.pending_summary()).unwrap_or_default();
        prompt.push_str(&format!(
            "\n\nAdditionally, child agents have pending escalations blocking them. Reply to each \
             via `agent__reply_escalation` first:\n{summary}"
        ));
    }
    GuardrailAction::Inject(prompt)
}

pub fn escalation_function_declarations() -> Vec<FunctionDeclaration> {
    vec![FunctionDeclaration {
        name: format!("{AGENT_FUNCTION_PREFIX}reply_escalation"),
        description: "Reply to a pending escalation from a child agent. The child is blocked waiting for this reply. \
                      Use this after seeing pending_escalations notifications.".to_string(),
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
                        description: Some("Your answer to the child agent's question. For ask/confirm questions, use \
                                           the exact option text. For input questions, provide the text response.".into()),
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

pub fn agent_function_declarations() -> Vec<FunctionDeclaration> {
    vec![
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}spawn"),
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
            name: format!("{AGENT_FUNCTION_PREFIX}check"),
            description: "Non-blocking status probe: reports whether a spawned agent is still running or finished. \
                          NEVER returns or consumes the result — when finished, call agent__collect to retrieve it.".to_string(),
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
            name: format!("{AGENT_FUNCTION_PREFIX}collect"),
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
            name: format!("{AGENT_FUNCTION_PREFIX}list_running"),
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
            name: format!("{AGENT_FUNCTION_PREFIX}list_available"),
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
            name: format!("{AGENT_FUNCTION_PREFIX}cancel"),
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
            name: format!("{AGENT_FUNCTION_PREFIX}task_create"),
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
            name: format!("{AGENT_FUNCTION_PREFIX}task_list"),
            description: "List all tasks in the task queue with their status and dependencies.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{AGENT_FUNCTION_PREFIX}task_complete"),
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
            name: format!("{AGENT_FUNCTION_PREFIX}task_fail"),
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
            name: format!("{AGENT_FUNCTION_PREFIX}send_message"),
            description: "Send a text message to another agent's inbox: a child or sibling by its agent id, or your parent via the reserved id 'parent'. Use to share cross-cutting findings or coordinate with teammates.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The target agent ID, or the reserved id 'parent' to message the agent that spawned you".into()),
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
            name: format!("{AGENT_FUNCTION_PREFIX}check_inbox"),
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

pub async fn handle_agent_tool(
    ctx: &mut RequestContext,
    cmd_name: &str,
    args: &Value,
) -> Result<Value> {
    let action = cmd_name
        .strip_prefix(AGENT_FUNCTION_PREFIX)
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
        _ => bail!("Unknown agent action: {action}"),
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
                match check_pending_tasks_guardrail(&mut child_ctx) {
                    GuardrailAction::NoAction => {
                        if let Some(prompt) = todo_continuation_prompt(&mut child_ctx) {
                            input = Input::from_str(&child_ctx, &prompt, None)?;
                            continue;
                        }
                        break;
                    }
                    GuardrailAction::ForceTerminate(ids) => {
                        warn!(
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

fn todo_continuation_prompt(ctx: &mut RequestContext) -> Option<String> {
    let config = ctx.auto_continue_config();
    if !ctx.app.config.function_calling_support
        || !config.enabled
        || ctx.auto_continue_count >= config.max_continues
        || !ctx.todo_list.has_incomplete()
        || ctx.auto_continue_paused.is_some()
    {
        return None;
    }

    ctx.increment_auto_continue_count();
    debug!(
        "Auto-continuing child agent ({}/{}): {} incomplete todo(s) remain",
        ctx.auto_continue_count,
        config.max_continues,
        ctx.todo_list.incomplete_count()
    );

    let prompt = config
        .continuation_prompt
        .as_deref()
        .unwrap_or(DEFAULT_CONTINUATION_PROMPT);

    Some(format!("{prompt}\n\n{}", ctx.todo_list.render_for_model()))
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
    parent_ctx.ensure_inbox();
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
    let child_jobs_enabled = jobs_enabled(Some(&agent), app_config.as_ref());
    let should_init_supervisor = agent.can_spawn_agents() || child_jobs_enabled;
    let agent_max_concurrent_subagents = if agent.can_spawn_agents() {
        agent.max_concurrent_agents()
    } else {
        0
    };
    let agent_max_depth = agent.max_agent_depth();
    let agent_max_jobs = effective_max_concurrent_jobs(Some(&agent), app_config.as_ref());

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
        child_ctx.supervisor = Some(Arc::new(RwLock::new(
            Supervisor::new(agent_max_concurrent_subagents, agent_max_depth)
                .with_max_concurrent_jobs(agent_max_jobs),
        )));
    }

    if let Some(session) = session {
        child_ctx
            .use_session(app_config.as_ref(), Some(&session), child_abort.clone())
            .await?;
        sync_agent_functions_to_ctx(&mut child_ctx)?;
    } else {
        populate_agent_mcp_runtime(&mut child_ctx, &agent_mcp_servers).await?;
        child_ctx.refresh_mcp_tool_filters();
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
    let server_features = ctx.tool_scope.mcp_runtime.server_features();
    let functions = {
        let agent = ctx
            .agent
            .as_mut()
            .with_context(|| "Agent should be initialized")?;
        if !server_features.is_empty() {
            agent.append_mcp_meta_functions(server_features);
        }
        agent.functions().clone()
    };

    ctx.tool_scope.functions = functions;
    // Agent::init builds one function set regardless of how the agent runs;
    // this sync is only ever called for spawned children.
    if ctx.self_agent_id.is_some() {
        ctx.tool_scope
            .functions
            .remove_function(&format!("{TODO_FUNCTION_PREFIX}pause"));
    }

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
    ctx.ensure_inbox();

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
    let child_jobs_enabled = jobs_enabled(Some(&agent), app_config.as_ref());
    let should_init_supervisor = agent.can_spawn_agents() || child_jobs_enabled;
    let max_concurrent_agents = if agent.can_spawn_agents() {
        agent.max_concurrent_agents()
    } else {
        0
    };
    let max_depth = agent.max_agent_depth();
    let max_jobs = effective_max_concurrent_jobs(Some(&agent), app_config.as_ref());
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
        child_ctx.supervisor = Some(Arc::new(RwLock::new(
            Supervisor::new(max_concurrent_agents, max_depth).with_max_concurrent_jobs(max_jobs),
        )));
    }

    if let Some(session) = session {
        child_ctx
            .use_session(app_config.as_ref(), Some(&session), child_abort.clone())
            .await?;
        sync_agent_functions_to_ctx(&mut child_ctx)?;
    } else {
        populate_agent_mcp_runtime(&mut child_ctx, &agent_mcp_servers).await?;
        child_ctx.refresh_mcp_tool_filters();
        sync_agent_functions_to_ctx(&mut child_ctx)?;
        child_ctx.init_agent_shared_variables()?;
    }

    let input = Input::from_str(&child_ctx, &prompt, None)?;

    debug!("Spawning child agent '{agent_name}' as '{agent_id}'");

    let spawn_agent_id = agent_id.clone();
    let spawn_agent_name = agent_name.clone();
    let spawn_abort = child_abort.clone();
    let spawn_notifications = Arc::clone(&ctx.notification_queue);
    let child_supervisor = child_ctx.supervisor.clone();

    let join_handle = tokio::spawn(async move {
        let result = run_child_agent(child_ctx, input, spawn_abort).await;

        let agent_result = match result {
            Ok(output) => AgentResult {
                id: spawn_agent_id,
                agent_name: spawn_agent_name,
                output,
                exit_status: AgentExitStatus::Completed,
            },
            Err(e) => AgentResult {
                id: spawn_agent_id,
                agent_name: spawn_agent_name,
                output: String::new(),
                exit_status: AgentExitStatus::Failed(e.to_string()),
            },
        };
        let success = agent_result.exit_status == AgentExitStatus::Completed;
        spawn_notifications.push(agent_notification(
            &agent_result.id,
            &agent_result.agent_name,
            success,
        ));

        Ok(agent_result)
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
        Some(true) => Ok(json!({
            "status": "finished",
            "id": id,
            "message": format!(
                "Agent '{id}' has finished; its result is ready and has NOT been consumed. \
                 Call `agent__collect --id {id}` to retrieve it (returns instantly on a \
                 finished agent). The handle stays registered until collected."
            ),
        })),
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
        None => {
            if is_job_task(ctx.supervisor.as_ref(), id) {
                return Ok(job_id_teaching_error(id));
            }

            Ok(json!({
                "status": "error",
                "message": format!("No agent found with id '{id}'")
            }))
        }
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
            if id.starts_with("job_") || sup.has_job(id) {
                return Ok(job_id_teaching_error(id));
            }

            return Ok(json!({
                "status": "error",
                "message": format!("Agent '{id}' not found. Use agent__check to verify it exists and is finished.")
            }));
        }
        sup.abort_signal_for(id)
    };

    let interrupted = || {
        json!({
            "status": "interrupted",
            "id": id,
            "message": format!("agent__collect was interrupted by the user; agent '{id}' is still running in the background. Collect it again later, cancel it with agent__cancel, or continue with other work."),
        })
    };

    loop {
        let is_finished = {
            let sup = supervisor.read();
            sup.is_finished(id).unwrap_or(false)
        };

        if is_finished {
            break;
        }

        if ctx.session_abort.as_ref().is_some_and(|s| s.aborted()) {
            return Ok(interrupted());
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
                    _ = wait_user_interrupt(ctx.session_abort.as_ref()) => {
                        return Ok(interrupted());
                    }
                }
            }
            None => {
                tokio::select! {
                    _ = time::sleep(Duration::from_millis(200)) => {}
                    _ = wait_user_interrupt(ctx.session_abort.as_ref()) => {
                        return Ok(interrupted());
                    }
                }
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
            ctx.pending_tasks_guardrail_count = 0;

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

            ctx.pending_tasks_guardrail_count = 0;

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
        None => {
            if is_job_task(ctx.supervisor.as_ref(), id) {
                return Ok(job_id_teaching_error(id));
            }

            Ok(json!({
                "status": "error",
                "message": format!("No agent found with id '{id}'"),
            }))
        }
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

    // The routable identity of this sender from the recipient's point of view.
    // Only spawned agents have a `self_agent_id`; the name fallback covers
    // contexts that cannot be replied to anyway (e.g. macro contexts).
    let self_label = ctx
        .self_agent_id
        .clone()
        .or_else(|| ctx.agent.as_ref().map(|a| a.name().to_string()))
        .unwrap_or_else(|| PARENT_ALIAS.to_string());

    let deliver = |inbox: &Arc<Inbox>, from: String| {
        inbox.deliver(Envelope {
            from,
            to: id.to_string(),
            payload: EnvelopePayload::Text {
                content: message.to_string(),
            },
            timestamp: Utc::now(),
        });
    };

    if id == PARENT_ALIAS {
        return match ctx.parent_inbox.as_ref() {
            Some(inbox) => {
                deliver(inbox, self_label);
                Ok(json!({
                    "status": "ok",
                    "message": "Message delivered to your parent agent's inbox; it will be read on the parent's next check_inbox call.",
                }))
            }
            None => Ok(json!({
                "status": "error",
                "message": "You have no parent agent — the reserved id 'parent' is only routable from a spawned subagent.",
            })),
        };
    }

    let own_child_inbox = ctx
        .supervisor
        .as_ref()
        .and_then(|sup| sup.read().inbox(id).cloned());
    if let Some(inbox) = own_child_inbox {
        deliver(&inbox, PARENT_ALIAS.to_string());
        return Ok(json!({
            "status": "ok",
            "message": format!("Message delivered to agent '{id}'"),
        }));
    }

    let sibling_inbox = ctx
        .parent_supervisor
        .as_ref()
        .and_then(|sup| sup.read().inbox(id).cloned());
    if let Some(inbox) = sibling_inbox {
        deliver(&inbox, self_label);
        return Ok(json!({
            "status": "ok",
            "message": format!("Message delivered to agent '{id}'"),
        }));
    }

    if is_job_task(ctx.supervisor.as_ref(), id) || is_job_task(ctx.parent_supervisor.as_ref(), id) {
        return Ok(job_id_teaching_error(id));
    }

    Ok(json!({
        "status": "error",
        "message": format!("No agent found with id '{id}'. Agent may not exist or may have already completed."),
    }))
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
    use crate::config::test_fixtures::{FixtureServer, fixture_runtime};
    use crate::config::{AgentConfig, AppConfig, AppState, WorkingMode};
    use crate::function::jobs::RingBuf;
    use crate::mcp::{McpServer, McpServersConfig, McpTransportType};
    use crate::supervisor::escalation::{EscalationQueue, EscalationRequest};
    use crate::supervisor::{JobHandle, JobResult, JobState, JobStatus};
    use parking_lot::Mutex;
    use serde_json::json;
    use serial_test::serial;
    use std::mem;

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

    fn auto_continue_ctx() -> RequestContext {
        let app = AppState {
            config: Arc::new(AppConfig {
                auto_continue: true,
                ..Default::default()
            }),
            ..AppState::test_default()
        };
        RequestContext::new(Arc::new(app), WorkingMode::Cmd)
    }

    #[test]
    fn todo_continuation_fires_for_child_with_incomplete_todos() {
        let mut ctx = auto_continue_ctx();
        ctx.init_todo_list("ship feature");
        ctx.add_todo("build the thing");

        let prompt = todo_continuation_prompt(&mut ctx).expect("child should auto-continue");

        assert!(prompt.contains("TODO CONTINUATION"), "{prompt}");
        assert!(prompt.contains("build the thing"), "{prompt}");
        assert_eq!(ctx.auto_continue_count, 1);
    }

    #[test]
    fn todo_continuation_stops_when_done_paused_capped_or_disabled() {
        // All todos complete → run may end.
        let mut ctx = auto_continue_ctx();
        ctx.init_todo_list("ship feature");
        let id = ctx.add_todo("build the thing");
        ctx.mark_todo_done(id);
        assert!(todo_continuation_prompt(&mut ctx).is_none());

        // Defense-in-depth: children cannot call todo__pause, but a set flag
        // must still stop the continuation loop.
        ctx.add_todo("deploy the thing");
        ctx.pause_auto_continue("blocked on credentials");
        assert!(todo_continuation_prompt(&mut ctx).is_none());
        ctx.resume_auto_continue();
        assert!(todo_continuation_prompt(&mut ctx).is_some());

        // Continuation cap reached → run may end.
        ctx.auto_continue_count = ctx.auto_continue_config().max_continues;
        assert!(todo_continuation_prompt(&mut ctx).is_none());

        // Auto-continue disabled → run may end even with incomplete todos.
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.init_todo_list("ship feature");
        ctx.add_todo("build the thing");
        assert!(todo_continuation_prompt(&mut ctx).is_none());
    }

    fn ctx_with_job_capable_supervisor() -> RequestContext {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.supervisor = Some(Arc::new(RwLock::new(
            Supervisor::new(4, 3).with_max_concurrent_jobs(4),
        )));
        ctx
    }

    fn make_fake_job(id: &str) -> JobHandle {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let join_handle = rt.spawn(async {
            Ok(JobResult {
                output: json!(null),
                exit_code: Some(0),
                output_bytes_captured: 0,
            })
        });
        mem::forget(rt);
        JobHandle {
            id: id.to_string(),
            tool: "execute_command".to_string(),
            started_at: std::time::Instant::now(),
            join_handle,
            abort_signal: create_abort_signal(),
            state: Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            })),
            output_buf: Arc::new(Mutex::new(RingBuf::default())),
            no_change_checks: 0,
            last_check_state: None,
        }
    }

    fn register_fake_job(ctx: &mut RequestContext, id: &str) {
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(make_fake_job(id))
            .unwrap();
    }

    fn assert_job_teaching_error(result: &Value, id: &str) {
        assert_eq!(result["status"], "error");
        let message = result["message"].as_str().unwrap();
        assert_eq!(
            message,
            format!(
                "'{id}' is a background job, not an agent — use job__check / job__collect / job__cancel"
            )
        );
    }

    fn register_fake_agent(ctx: &mut RequestContext, id: &str, name: &str) {
        register_fake_agent_with_output(ctx, id, name, "fake output");
    }

    fn register_fake_agent_with_output(
        ctx: &mut RequestContext,
        id: &str,
        name: &str,
        output: &str,
    ) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let id_owned = id.to_string();
        let name_owned = name.to_string();
        let output_owned = output.to_string();
        let join_handle = rt.spawn(async move {
            Ok(AgentResult {
                id: id_owned,
                agent_name: name_owned,
                output: output_owned,
                exit_status: AgentExitStatus::Completed,
            })
        });
        mem::forget(rt);

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

    fn register_running_agent(ctx: &mut RequestContext, id: &str, name: &str) -> AbortSignal {
        let abort = create_abort_signal();
        let id_owned = id.to_string();
        let name_owned = name.to_string();
        let join_handle = tokio::spawn(async move {
            time::sleep(Duration::from_secs(60)).await;
            Ok(AgentResult {
                id: id_owned,
                agent_name: name_owned,
                output: String::new(),
                exit_status: AgentExitStatus::Completed,
            })
        });
        let handle = AgentHandle {
            id: id.to_string(),
            agent_name: name.to_string(),
            depth: 1,
            inbox: Arc::new(Inbox::new()),
            abort_signal: abort.clone(),
            join_handle,
            child_supervisor: None,
        };
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(handle)
            .unwrap();
        abort
    }

    fn wait_until_finished(ctx: &RequestContext, id: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while ctx.supervisor.as_ref().unwrap().read().is_finished(id) != Some(true) {
            assert!(
                std::time::Instant::now() < deadline,
                "agent '{id}' never finished"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn sync_agent_functions_gates_meta_functions_on_live_capabilities() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(AgentConfig::default()));
        let (runtime, _server) = fixture_runtime(FixtureServer {
            tools_capability: false,
            resources_capability: true,
            ..FixtureServer::default()
        })
        .await;
        ctx.tool_scope.mcp_runtime = runtime;

        sync_agent_functions_to_ctx(&mut ctx).unwrap();

        let functions = &ctx.tool_scope.functions;
        assert_eq!(functions.declarations().len(), 3);
        assert!(functions.contains("mcp_search_fixture"));
        assert!(functions.contains("mcp_describe_fixture"));
        assert!(functions.contains("mcp_read_fixture"));
        assert!(!functions.contains("mcp_invoke_fixture"));
    }

    /// `Agent::init` registers the full todo tool set (incl. `todo__pause`)
    /// whenever the agent config enables auto_continue, because the same
    /// function set serves top-level `--agent` sessions. Spawned children
    /// must never advertise `todo__pause` (they are driven to completion
    /// autonomously), so the child-only sync strips that one declaration
    /// while leaving the rest of the todo tools intact.
    #[tokio::test]
    async fn sync_strips_todo_pause_declaration_for_child_contexts() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        let mut agent = Agent::test_new(AgentConfig::default());
        agent.functions_mut().append_todo_functions();
        ctx.agent = Some(agent);
        ctx.self_agent_id = Some("agent_sisyphus_abc123".to_string());

        sync_agent_functions_to_ctx(&mut ctx).unwrap();

        let functions = &ctx.tool_scope.functions;
        assert!(!functions.contains("todo__pause"));
        assert!(functions.contains("todo__init"));
        assert!(functions.contains("todo__add"));
        assert!(functions.contains("todo__done"));
        assert!(functions.contains("todo__list"));
        assert!(functions.contains("todo__clear"));
    }

    fn app_state_with_fixture_mcp_config() -> Arc<AppState> {
        let mut state = AppState::test_default();
        state.mcp_config = Some(McpServersConfig {
            mcp_servers: [(
                "fixture".to_string(),
                McpServer {
                    transport_type: McpTransportType::Stdio,
                    command: Some("echo".to_string()),
                    args: None,
                    env: None,
                    cwd: None,
                    url: None,
                    headers: None,
                    oauth: None,
                    allowed_tools: None,
                },
            )]
            .into_iter()
            .collect(),
        });
        Arc::new(state)
    }

    #[tokio::test]
    async fn spawned_child_runtime_enforces_child_agent_filters() {
        use std::sync::atomic::Ordering;

        let config = AgentConfig {
            mcp_tools: Some(IndexMap::from([(
                "fixture".to_string(),
                vec!["get_*".to_string()],
            )])),
            ..Default::default()
        };
        let mut ctx = RequestContext::new(app_state_with_fixture_mcp_config(), WorkingMode::Cmd);
        ctx.agent = Some(Agent::test_new(config));
        let fixture = FixtureServer::default();
        let call_tool_calls = Arc::clone(&fixture.call_tool_calls);
        let (runtime, _server) = fixture_runtime(fixture).await;
        ctx.tool_scope.mcp_runtime = runtime;

        populate_agent_mcp_runtime(&mut ctx, &[]).await.unwrap();
        ctx.refresh_mcp_tool_filters();

        let err = ctx
            .tool_scope
            .mcp_runtime
            .invoke("fixture", "dup", json!({}))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "dup not found in fixture MCP server catalog");
        assert_eq!(call_tool_calls.load(Ordering::SeqCst), 0);
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
        // A child can only route replies through the reserved alias, so
        // messages from the spawning agent must be labeled with it.
        assert_eq!(msgs[0].from, PARENT_ALIAS);
    }

    #[test]
    fn handle_send_message_to_unknown_agent() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result =
            handle_send_message(&mut ctx, &json!({"id": "missing", "message": "hi"})).unwrap();
        assert_eq!(result["status"], "error");
    }

    #[test]
    fn handle_send_message_parent_alias_delivers_to_parent_inbox() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let parent_inbox = Arc::new(Inbox::new());
        ctx.parent_inbox = Some(Arc::clone(&parent_inbox));
        ctx.self_agent_id = Some("agent_explore_abc123".to_string());

        let result = handle_send_message(
            &mut ctx,
            &json!({"id": "parent", "message": "hello from child"}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");

        let msgs = parent_inbox.drain();
        assert_eq!(msgs.len(), 1);
        // The parent can reply through its own supervisor, so the sender must
        // be the child's routable agent id.
        assert_eq!(msgs[0].from, "agent_explore_abc123");
        match &msgs[0].payload {
            EnvelopePayload::Text { content } => assert_eq!(content, "hello from child"),
            _ => panic!("expected text payload"),
        }
    }

    #[test]
    fn handle_send_message_parent_alias_without_parent_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result =
            handle_send_message(&mut ctx, &json!({"id": "parent", "message": "hi"})).unwrap();
        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("no parent agent")
        );
    }

    #[test]
    fn handle_send_message_to_sibling_labels_sender_with_own_id() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.self_agent_id = Some("agent_explore_sender".to_string());

        let mut sibling_ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut sibling_ctx, "agent_explore_receiver", "explore");
        ctx.parent_supervisor = sibling_ctx.supervisor.clone();

        let result = handle_send_message(
            &mut ctx,
            &json!({"id": "agent_explore_receiver", "message": "hi sibling"}),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");

        let inbox = ctx
            .parent_supervisor
            .as_ref()
            .unwrap()
            .read()
            .inbox("agent_explore_receiver")
            .unwrap()
            .clone();
        let msgs = inbox.drain();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, "agent_explore_sender");
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
        let result = run_async(handle_agent_tool(&mut ctx, "agent__bogus", &json!({})));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Unknown agent action")
        );
    }

    #[test]
    fn dispatch_routes_list_running() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let result = run_async(handle_agent_tool(
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
        let result = run_async(handle_agent_tool(
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
        let result =
            run_async(handle_agent_tool(&mut ctx, "agent__task_list", &json!({}))).unwrap();
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

    #[test]
    fn guardrail_prompt_mentions_pending_escalations() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
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

            let queue = ctx.ensure_root_escalation_queue();
            let (tx, _rx) = tokio::sync::oneshot::channel();
            queue.submit(EscalationRequest {
                id: "esc_9".into(),
                from_agent_id: "a1".into(),
                from_agent_name: "explore".into(),
                question: "Which option?".into(),
                options: None,
                reply_tx: tx,
            });

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::Inject(prompt) => {
                    assert!(prompt.contains("agent__reply_escalation"));
                    assert!(prompt.contains("esc_9"));
                }
                _ => panic!("expected Inject action"),
            }
        });
    }

    #[test]
    fn guardrail_prompt_omits_escalations_when_none_pending() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
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

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::Inject(prompt) => {
                    assert!(!prompt.contains("agent__reply_escalation"));
                }
                _ => panic!("expected Inject action"),
            }
        });
    }

    #[test]
    fn handle_collect_finished_agent_returns_output_and_consumes_handle() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        ctx.pending_tasks_guardrail_count = 2;

        let result = run_async(handle_collect(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(result["status"], "completed");
        assert_eq!(result["id"], "a1");
        assert_eq!(result["agent"], "explore");
        assert_eq!(result["exit_status"], "Completed");
        assert_eq!(result["output"], "fake output");
        assert_eq!(ctx.pending_tasks_guardrail_count, 0);
        assert_eq!(
            ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
            None
        );
    }

    #[test]
    fn handle_collect_session_interrupt_returns_and_keeps_agent() {
        run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let join_handle = tokio::spawn(async move {
                let _ = rx.await;
                Ok(AgentResult {
                    id: "a1".to_string(),
                    agent_name: "explore".to_string(),
                    output: "late output".to_string(),
                    exit_status: AgentExitStatus::Completed,
                })
            });
            let handle = AgentHandle {
                id: "a1".to_string(),
                agent_name: "explore".to_string(),
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

            let session = create_abort_signal();
            session.set_ctrlc();
            ctx.session_abort = Some(session.clone());

            let result = time::timeout(
                Duration::from_secs(2),
                handle_collect(&mut ctx, &json!({"id": "a1"})),
            )
            .await
            .expect("interrupted collect must return promptly")
            .unwrap();

            assert_eq!(result["status"], "interrupted");
            assert_eq!(result["id"], "a1");
            assert!(
                result["message"]
                    .as_str()
                    .unwrap()
                    .contains("still running")
            );
            assert_eq!(
                ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
                Some(false)
            );

            session.reset();
            tx.send(()).unwrap();

            let collected = handle_collect(&mut ctx, &json!({"id": "a1"}))
                .await
                .unwrap();
            assert_eq!(collected["status"], "completed");
            assert_eq!(collected["output"], "late output");
        });
    }

    #[test]
    fn handle_collect_pending_escalations_early_out_keeps_handle() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let _abort = register_running_agent(&mut ctx, "slow", "test");
            let queue = ctx.ensure_root_escalation_queue();
            let (tx, _rx) = tokio::sync::oneshot::channel();
            queue.submit(EscalationRequest {
                id: "esc_1".into(),
                from_agent_id: "a1".into(),
                from_agent_name: "explore".into(),
                question: "What do?".into(),
                options: None,
                reply_tx: tx,
            });

            let result = handle_collect(&mut ctx, &json!({"id": "slow"}))
                .await
                .unwrap();

            assert_eq!(result["status"], "pending");
            assert!(result["pending_escalations"].is_array());
            assert_eq!(
                ctx.supervisor.as_ref().unwrap().read().is_finished("slow"),
                Some(false)
            );
        });
    }

    #[test]
    fn handle_collect_unknown_agent_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);

        let result = run_async(handle_collect(&mut ctx, &json!({"id": "missing"}))).unwrap();

        assert_eq!(result["status"], "error");
        assert!(result["message"].as_str().unwrap().contains("not found"));
    }

    #[test]
    fn handle_collect_without_agent_passes_long_output_through_verbatim() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let long_output = "x".repeat(10_000);
        register_fake_agent_with_output(&mut ctx, "a1", "explore", &long_output);

        let result = run_async(handle_collect(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(result["output"], long_output);
    }

    #[test]
    fn handle_collect_output_below_agent_threshold_passes_through() {
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.agent = Some(Agent::test_new(AgentConfig {
            summarization_threshold: 1_000_000,
            ..Default::default()
        }));
        register_fake_agent(&mut ctx, "a1", "explore");

        let result = run_async(handle_collect(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(result["status"], "completed");
        assert_eq!(result["output"], "fake output");
    }

    #[test]
    fn handle_collect_over_threshold_with_unknown_summarization_model_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.agent = Some(Agent::test_new(AgentConfig {
            summarization_threshold: 1,
            summarization_model: Some("nonexistent_client:model".into()),
            ..Default::default()
        }));
        register_fake_agent(&mut ctx, "a1", "explore");

        let err = run_async(handle_collect(&mut ctx, &json!({"id": "a1"}))).unwrap_err();

        assert!(err.to_string().contains("nonexistent_client"));
    }

    #[test]
    fn guardrail_no_supervisor_is_no_action_and_resets_counter() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        ctx.pending_tasks_guardrail_count = 2;

        assert!(matches!(
            check_pending_tasks_guardrail(&mut ctx),
            GuardrailAction::NoAction
        ));
        assert_eq!(ctx.pending_tasks_guardrail_count, 0);
    }

    /// A finished-but-uncollected agent counts as pending: the turn-end
    /// guardrail tells the model to collect it instead of letting the result
    /// be silently dropped, and the handle stays registered.
    #[test]
    fn guardrail_surfaces_finished_but_uncollected_agents() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        wait_until_finished(&ctx, "a1");
        ctx.pending_tasks_guardrail_count = 2;

        match check_pending_tasks_guardrail(&mut ctx) {
            GuardrailAction::Inject(prompt) => {
                assert!(prompt.contains("a1"));
                assert!(prompt.contains("agent__collect --id a1"));
                assert!(prompt.contains("Completed but UNCOLLECTED"));
            }
            _ => panic!("expected Inject action"),
        }
        assert_eq!(ctx.pending_tasks_guardrail_count, 3);
        assert_eq!(
            ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
            Some(true)
        );
    }

    #[test]
    fn guardrail_force_terminate_discards_finished_uncollected_handles() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        wait_until_finished(&ctx, "a1");
        ctx.pending_tasks_guardrail_count = PENDING_TASKS_GUARDRAIL_MAX;

        match check_pending_tasks_guardrail(&mut ctx) {
            GuardrailAction::ForceTerminate(ids) => {
                assert_eq!(ids, vec!["a1".to_string()]);
            }
            _ => panic!("expected ForceTerminate action"),
        }
        assert_eq!(ctx.pending_tasks_guardrail_count, 0);
        assert_eq!(
            ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
            None
        );
    }

    #[test]
    fn guardrail_prompt_renders_running_and_finished_sections() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let _abort = register_running_agent(&mut ctx, "slow", "test");
            register_fake_agent(&mut ctx, "a1", "explore");
            wait_until_finished(&ctx, "a1");

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::Inject(prompt) => {
                    assert!(prompt.contains("Still running"));
                    assert!(prompt.contains("slow (agent)"));
                    assert!(prompt.contains("Completed but UNCOLLECTED"));
                    assert!(prompt.contains("agent__collect --id a1"));
                }
                _ => panic!("expected Inject action"),
            }
        });
    }

    #[test]
    fn guardrail_prompt_is_kind_aware_for_jobs() {
        let tasks = vec![
            PendingTask {
                id: "job_1".into(),
                kind: TaskKind::Job,
                finished: false,
            },
            PendingTask {
                id: "job_2".into(),
                kind: TaskKind::Job,
                finished: true,
            },
        ];

        let prompt = build_pending_tasks_guardrail_prompt(&tasks);

        assert!(prompt.contains("job_1 (job)"));
        assert!(prompt.contains("job__cancel"));
        assert!(prompt.contains("job__collect --id job_2"));
    }

    #[test]
    fn pending_tasks_includes_registered_jobs() {
        let mut ctx = ctx_with_job_capable_supervisor();
        register_fake_job(&mut ctx, "job_1");

        let tasks = pending_tasks(&ctx);

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "job_1");
        assert_eq!(tasks[0].kind, TaskKind::Job);
    }

    #[test]
    fn pending_tasks_scopes_jobs_to_node_scope() {
        let mut ctx = ctx_with_job_capable_supervisor();
        register_fake_job(&mut ctx, "job_mine");
        register_fake_job(&mut ctx, "job_other");

        ctx.node_job_scope = Some(vec!["job_mine".to_string()]);
        let tasks = pending_tasks(&ctx);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "job_mine");
        assert_eq!(tasks[0].kind, TaskKind::Job);

        ctx.node_job_scope = None;
        assert_eq!(pending_tasks(&ctx).len(), 2);
    }

    #[test]
    fn guardrail_force_terminates_at_max_and_cancels_agents() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let abort = register_running_agent(&mut ctx, "slow", "test");
            ctx.pending_tasks_guardrail_count = PENDING_TASKS_GUARDRAIL_MAX;

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::ForceTerminate(ids) => {
                    assert_eq!(ids, vec!["slow".to_string()]);
                }
                _ => panic!("expected ForceTerminate action"),
            }
            assert_eq!(ctx.pending_tasks_guardrail_count, 0);
            assert!(abort.aborted());
        });
    }

    #[test]
    fn guardrail_injects_prompt_and_increments_counter_below_max() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let _abort = register_running_agent(&mut ctx, "slow", "test");
            ctx.pending_tasks_guardrail_count = 1;

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::Inject(prompt) => {
                    assert!(prompt.contains("slow"));
                    assert!(prompt.contains("agent__collect"));
                }
                _ => panic!("expected Inject action"),
            }
            assert_eq!(ctx.pending_tasks_guardrail_count, 2);
        });
    }

    #[test]
    fn handle_cancel_resets_guardrail_counter() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        ctx.pending_tasks_guardrail_count = 2;

        let result = run_async(handle_cancel(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(result["status"], "ok");
        assert_eq!(ctx.pending_tasks_guardrail_count, 0);
    }

    #[test]
    fn handle_spawn_missing_agent_arg_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let err = run_async(handle_spawn(&mut ctx, &json!({}))).unwrap_err();
        assert!(err.to_string().contains("'agent' is required"));
    }

    #[test]
    fn handle_spawn_missing_prompt_arg_errors() {
        let mut ctx = ctx_with_supervisor(4, 3);
        let err = run_async(handle_spawn(&mut ctx, &json!({"agent": "explore"}))).unwrap_err();
        assert!(err.to_string().contains("'prompt' is required"));
    }

    #[test]
    fn handle_spawn_rejects_agent_outside_whitelist() {
        let mut ctx = ctx_with_supervisor(4, 3);
        ctx.agent = Some(Agent::test_new(AgentConfig {
            spawnable_agents: Some(vec!["allowed".into()]),
            ..Default::default()
        }));

        let result = run_async(handle_spawn(
            &mut ctx,
            &json!({"agent": "notallowed", "prompt": "p"}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("spawnable_agents")
        );
    }

    #[test]
    fn handle_spawn_at_capacity_errors() {
        let mut ctx = ctx_with_supervisor(1, 3);
        register_fake_agent(&mut ctx, "a1", "explore");

        let result = run_async(handle_spawn(
            &mut ctx,
            &json!({"agent": "x", "prompt": "p"}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert_eq!(
            result["message"],
            "At capacity: 1/1 agents running. Wait for one to finish or cancel one."
        );
    }

    #[test]
    fn handle_spawn_exceeding_depth_errors() {
        let mut ctx = ctx_with_supervisor(4, 0);

        let result = run_async(handle_spawn(
            &mut ctx,
            &json!({"agent": "x", "prompt": "p"}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("Max agent depth exceeded")
        );
    }

    #[test]
    fn handle_spawn_no_supervisor_errors() {
        let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
        let err = run_async(handle_spawn(
            &mut ctx,
            &json!({"agent": "x", "prompt": "p"}),
        ))
        .unwrap_err();
        assert!(err.to_string().contains("No supervisor active"));
    }

    /// Checking a finished agent is a pure status probe: it reports the
    /// agent as finished, points at agent__collect, and leaves the handle
    /// registered so a subsequent collect still returns the result.
    #[test]
    fn handle_check_finished_agent_reports_status_and_keeps_handle() {
        let mut ctx = ctx_with_supervisor(4, 3);
        register_fake_agent(&mut ctx, "a1", "explore");
        wait_until_finished(&ctx, "a1");

        let result = run_async(handle_check(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(result["status"], "finished");
        assert_eq!(result["id"], "a1");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("agent__collect")
        );
        assert_eq!(
            ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
            Some(true)
        );

        let collected = run_async(handle_collect(&mut ctx, &json!({"id": "a1"}))).unwrap();

        assert_eq!(collected["status"], "completed");
        assert_eq!(collected["output"], "fake output");
    }

    #[test]
    fn handle_cancel_running_agent_aborts_and_waits_for_cleanup() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_supervisor(4, 3);
            let sig = create_abort_signal();
            let sig2 = sig.clone();
            let join_handle = tokio::spawn(async move {
                loop {
                    if sig2.aborted() {
                        return Ok(AgentResult {
                            id: "a1".into(),
                            agent_name: "explore".into(),
                            output: String::new(),
                            exit_status: AgentExitStatus::Completed,
                        });
                    }
                    time::sleep(Duration::from_millis(10)).await;
                }
            });
            let handle = AgentHandle {
                id: "a1".into(),
                agent_name: "explore".into(),
                depth: 1,
                inbox: Arc::new(Inbox::new()),
                abort_signal: sig.clone(),
                join_handle,
                child_supervisor: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();
            ctx.pending_tasks_guardrail_count = 2;

            let result = handle_cancel(&mut ctx, &json!({"id": "a1"})).await.unwrap();

            assert_eq!(result["status"], "ok");
            let message = result["message"].as_str().unwrap();
            assert!(message.contains("Cancelled agent 'explore'"));
            assert!(message.contains("waited for cleanup"));
            assert!(sig.aborted());
            assert_eq!(
                ctx.supervisor.as_ref().unwrap().read().is_finished("a1"),
                None
            );
            assert_eq!(ctx.pending_tasks_guardrail_count, 0);
        });
    }

    #[test]
    fn handle_check_registered_job_id_teaches_job_tools() {
        run_async(async {
            let mut ctx = ctx_with_job_capable_supervisor();
            register_fake_job(&mut ctx, "bg_1");

            let result = handle_check(&mut ctx, &json!({"id": "bg_1"}))
                .await
                .unwrap();

            assert_job_teaching_error(&result, "bg_1");
        });
    }

    #[test]
    fn handle_check_job_prefixed_id_teaches_job_tools() {
        run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);

            let result = handle_check(&mut ctx, &json!({"id": "job_deadbeef"}))
                .await
                .unwrap();

            assert_job_teaching_error(&result, "job_deadbeef");
        });
    }

    #[test]
    fn handle_collect_registered_job_id_teaches_job_tools() {
        run_async(async {
            let mut ctx = ctx_with_job_capable_supervisor();
            register_fake_job(&mut ctx, "bg_1");

            let result = handle_collect(&mut ctx, &json!({"id": "bg_1"}))
                .await
                .unwrap();

            assert_job_teaching_error(&result, "bg_1");
        });
    }

    #[test]
    fn handle_collect_job_prefixed_id_teaches_job_tools() {
        run_async(async {
            let mut ctx = ctx_with_supervisor(4, 3);

            let result = handle_collect(&mut ctx, &json!({"id": "job_deadbeef"}))
                .await
                .unwrap();

            assert_job_teaching_error(&result, "job_deadbeef");
        });
    }

    #[test]
    fn handle_cancel_registered_job_id_teaches_job_tools_and_keeps_job() {
        run_async(async {
            let mut ctx = ctx_with_job_capable_supervisor();
            register_fake_job(&mut ctx, "bg_1");

            let result = handle_cancel(&mut ctx, &json!({"id": "bg_1"}))
                .await
                .unwrap();

            assert_job_teaching_error(&result, "bg_1");
            assert!(ctx.supervisor.as_ref().unwrap().read().has_job("bg_1"));
        });
    }

    #[test]
    fn handle_send_message_registered_job_id_teaches_job_tools() {
        run_async(async {
            let mut ctx = ctx_with_job_capable_supervisor();
            register_fake_job(&mut ctx, "bg_1");

            let result =
                handle_send_message(&mut ctx, &json!({"id": "bg_1", "message": "hi"})).unwrap();

            assert_job_teaching_error(&result, "bg_1");
        });
    }

    #[test]
    fn handle_send_message_job_in_parent_supervisor_teaches_job_tools() {
        run_async(async {
            let mut ctx = RequestContext::new(default_app_state(), WorkingMode::Cmd);
            let mut parent_sup = Supervisor::new(4, 3).with_max_concurrent_jobs(4);
            parent_sup.register(make_fake_job("bg_p")).unwrap();
            ctx.parent_supervisor = Some(Arc::new(RwLock::new(parent_sup)));

            let result =
                handle_send_message(&mut ctx, &json!({"id": "bg_p", "message": "hi"})).unwrap();

            assert_job_teaching_error(&result, "bg_p");
        });
    }

    #[test]
    fn guardrail_burns_bounded_injects_then_force_terminates_running_job() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut ctx = ctx_with_job_capable_supervisor();
            let abort = create_abort_signal();
            let join_handle = tokio::spawn(async {
                time::sleep(Duration::from_secs(60)).await;
                Ok(JobResult {
                    output: json!(null),
                    exit_code: Some(0),
                    output_bytes_captured: 0,
                })
            });
            let handle = JobHandle {
                id: "job_1".to_string(),
                tool: "execute_command".to_string(),
                started_at: std::time::Instant::now(),
                join_handle,
                abort_signal: abort.clone(),
                state: Arc::new(Mutex::new(JobState {
                    status: JobStatus::Running,
                    pgid: None,
                })),
                output_buf: Arc::new(Mutex::new(RingBuf::default())),
                no_change_checks: 0,
                last_check_state: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            for expected_count in 1..=PENDING_TASKS_GUARDRAIL_MAX {
                match check_pending_tasks_guardrail(&mut ctx) {
                    GuardrailAction::Inject(prompt) => assert!(prompt.contains("job_1")),
                    _ => panic!("expected Inject below max"),
                }
                assert_eq!(ctx.pending_tasks_guardrail_count, expected_count);
            }

            match check_pending_tasks_guardrail(&mut ctx) {
                GuardrailAction::ForceTerminate(ids) => {
                    assert_eq!(ids, vec!["job_1".to_string()]);
                }
                _ => panic!("expected ForceTerminate at max"),
            }
            assert_eq!(ctx.pending_tasks_guardrail_count, 0);
            assert!(abort.aborted());
        });
    }

    #[test]
    fn guardrail_force_terminate_discards_finished_uncollected_job() {
        let mut ctx = ctx_with_job_capable_supervisor();
        register_fake_job(&mut ctx, "job_1");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !pending_tasks(&ctx).iter().any(|t| t.finished) {
            assert!(
                std::time::Instant::now() < deadline,
                "job 'job_1' never finished"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        ctx.pending_tasks_guardrail_count = PENDING_TASKS_GUARDRAIL_MAX;

        match check_pending_tasks_guardrail(&mut ctx) {
            GuardrailAction::ForceTerminate(ids) => {
                assert_eq!(ids, vec!["job_1".to_string()]);
            }
            _ => panic!("expected ForceTerminate action"),
        }
        assert_eq!(ctx.pending_tasks_guardrail_count, 0);
        assert!(!ctx.supervisor.as_ref().unwrap().read().has_job("job_1"));
    }
}
