use super::agents::AGENT_FUNCTION_PREFIX;
use super::memory::MEMORY_FUNCTION_PREFIX;
use super::rag_query::RAG_FUNCTION_PREFIX;
use super::skill::SKILL_FUNCTION_PREFIX;
use super::todo::TODO_FUNCTION_PREFIX;
use super::user_interaction::USER_FUNCTION_PREFIX;
use super::{FunctionDeclaration, JsonSchema, PATH_SEP, mcp_error_display, render_tool_result};
use crate::config::{
    McpRuntime, RequestContext, effective_max_concurrent_jobs, jobs_enabled, paths,
};
use crate::graph;
use crate::mcp::{
    MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX, MCP_INVOKE_META_FUNCTION_NAME_PREFIX,
    MCP_PROMPT_META_FUNCTION_NAME_PREFIX, MCP_READ_META_FUNCTION_NAME_PREFIX,
    MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
};
use crate::supervisor::notification::job_notification;
use crate::supervisor::{JobHandle, JobResult, JobState, JobStatus, Supervisor};
use crate::utils::{create_abort_signal, muted_warning_text, temp_file, wait_abort_signal};

use anyhow::{Context, Result, anyhow, bail};
use indexmap::IndexMap;
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{env, fs};
use tokio::io::AsyncReadExt;
use tokio::time;
use uuid::Uuid;

pub const JOB_FUNCTION_PREFIX: &str = "job__";

pub const DEFAULT_MAX_CONCURRENT_JOBS: usize = 5;

const JOB_RESULT_TAIL_CAP_CHARS: usize = 50_000;

const JOB_KILL_GRACE: Duration = Duration::from_secs(5);

const JOB_PUMP_DRAIN_GRACE: Duration = Duration::from_secs(2);

pub fn is_agent_task(supervisor: Option<&Arc<RwLock<Supervisor>>>, id: &str) -> bool {
    id.starts_with("agent_")
        || id.starts_with("graph_agent_")
        || supervisor.is_some_and(|sup| sup.read().has_agent(id))
}

pub struct RingBuf {
    buf: Vec<u8>,
    capacity: usize,
    write_pos: usize,
    total_written: u64,
}

impl RingBuf {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: Vec::new(),
            capacity,
            write_pos: 0,
            total_written: 0,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.total_written += bytes.len() as u64;
        if self.capacity == 0 {
            return;
        }

        let src = if bytes.len() > self.capacity {
            &bytes[bytes.len() - self.capacity..]
        } else {
            bytes
        };

        for &byte in src {
            if self.buf.len() < self.capacity {
                self.buf.push(byte);
            } else {
                self.buf[self.write_pos] = byte;
            }

            self.write_pos = (self.write_pos + 1) % self.capacity;
        }
    }

    pub fn total_written(&self) -> u64 {
        self.total_written
    }

    pub fn tail(&self) -> Vec<u8> {
        if self.buf.len() < self.capacity {
            return self.buf.clone();
        }

        let mut out = Vec::with_capacity(self.capacity);
        out.extend_from_slice(&self.buf[self.write_pos..]);
        out.extend_from_slice(&self.buf[..self.write_pos]);
        out
    }
}

impl Default for RingBuf {
    fn default() -> Self {
        Self::new(64 * 1024)
    }
}

/// Everything a detached process job needs, frozen at `job__start`: config,
/// env, and PATH changes made afterwards do not affect a running job.
pub struct JobEnvSnapshot {
    cmd_name: String,
    display_name: String,
    cmd_args: Vec<String>,
    envs: HashMap<String, String>,
    output_file: PathBuf,
    timeout_secs: u64,
}

/// The complete context an MCP job task owns. The runtime holds ONLY the
/// validated server's handle, so the detached task cannot reach any other
/// server even by bug.
pub struct JobCtx {
    mcp_runtime: McpRuntime,
    current_depth: usize,
}

pub fn job_function_declarations() -> Vec<FunctionDeclaration> {
    vec![
        FunctionDeclaration {
            name: format!("{JOB_FUNCTION_PREFIX}start"),
            description: "Run a tool call as a background job and return immediately with a job id, so you can \
                          keep working while it runs. `arguments` is the same object the tool takes when called \
                          directly. Backgroundable tools: external command tools (e.g. execute_command) and \
                          `mcp_invoke_*` calls; built-in `agent__`/`job__`/`user__`/`todo__`/`memory__`/`skill__` \
                          tools cannot be backgrounded. The job runs against a snapshot of the current config and \
                          environment; later changes do not affect it. Process jobs honor COYOTE_TOOL_TIMEOUT; MCP \
                          jobs have NO timeout — cancel a hung one with `job__cancel`. Jobs do not survive coyote \
                          exiting. In graph LLM nodes, jobs are node-local: collect or cancel every job you start \
                          before the node ends — leftovers are cancelled at node exit.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "tool".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("Name of the tool to run in the background, exactly as it appears in your tool catalog".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "arguments".to_string(),
                        JsonSchema {
                            type_value: Some("object".to_string()),
                            description: Some("The arguments object the tool takes when called directly".into()),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["tool".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{JOB_FUNCTION_PREFIX}check"),
            description: "Non-blocking status probe for a background job. Returns status, elapsed time, and a tail \
                          of the output captured so far; it NEVER consumes the result — use `job__collect` for \
                          that. Call sparingly: if repeated checks show no change, do other work instead — you \
                          will be notified when the job completes. `output_bytes_captured` reports the total \
                          output size so far — use it to decide how to collect (`tail_lines`, `full_result`, or \
                          having the command write to a file).".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The job ID returned by job__start".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{JOB_FUNCTION_PREFIX}collect"),
            description: "Block until the named background job finishes, then return its result and remove the \
                          job. The result keeps the LAST 50,000 chars by default (failures land at the tail of \
                          build logs); pass `tail_lines` to keep only the last N lines instead, or \
                          `full_result: true` to skip the cap entirely (the session-wide tool-output limit still \
                          applies). Collecting is consume-once — decide first via `job__check`'s \
                          `output_bytes_captured`. For very large outputs, prefer having the command write to a \
                          file and paging it with `fs_read`.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([
                    (
                        "id".to_string(),
                        JsonSchema {
                            type_value: Some("string".to_string()),
                            description: Some("The job ID returned by job__start".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "tail_lines".to_string(),
                        JsonSchema {
                            type_value: Some("number".to_string()),
                            description: Some("Keep only the last N lines of the result".into()),
                            ..Default::default()
                        },
                    ),
                    (
                        "full_result".to_string(),
                        JsonSchema {
                            type_value: Some("boolean".to_string()),
                            description: Some("Return the complete result, skipping the default 50,000-char tail cap (default: false)".into()),
                            ..Default::default()
                        },
                    ),
                ])),
                required: Some(vec!["id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{JOB_FUNCTION_PREFIX}cancel"),
            description: "Cancel a background job: kills its process (SIGTERM, then SIGKILL after a 5s grace) and \
                          discards the handle. Returns any partial output captured so far. The id cannot be used \
                          afterwards.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::from([(
                    "id".to_string(),
                    JsonSchema {
                        type_value: Some("string".to_string()),
                        description: Some("The job ID returned by job__start".into()),
                        ..Default::default()
                    },
                )])),
                required: Some(vec!["id".to_string()]),
                ..Default::default()
            },
            agent: false,
        },
        FunctionDeclaration {
            name: format!("{JOB_FUNCTION_PREFIX}list"),
            description: "List all background jobs you have started that are still registered, with status, \
                          elapsed time, and bytes of output captured.".to_string(),
            parameters: JsonSchema {
                type_value: Some("object".to_string()),
                properties: Some(IndexMap::new()),
                ..Default::default()
            },
            agent: false,
        },
    ]
}

pub async fn handle_job_tool(
    ctx: &mut RequestContext,
    cmd_name: &str,
    args: &Value,
) -> Result<Value> {
    let action = cmd_name
        .strip_prefix(JOB_FUNCTION_PREFIX)
        .unwrap_or(cmd_name);

    match action {
        "start" => handle_start(ctx, args).await,
        "check" => handle_check(ctx, args),
        "collect" => handle_collect(ctx, args).await,
        "cancel" => handle_cancel(ctx, args).await,
        "list" => handle_list(ctx),
        _ => bail!("Unknown job action: {action}"),
    }
}

fn job_status_str(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
    }
}

fn job_miss_error(supervisor: Option<&Arc<RwLock<Supervisor>>>, id: &str) -> Value {
    if is_agent_task(supervisor, id) {
        json!({
            "status": "error",
            "message": format!(
                "'{id}' is a spawned agent, not a background job — use agent__check / agent__collect / agent__cancel"
            ),
        })
    } else {
        json!({
            "status": "error",
            "message": format!(
                "No job '{id}' is registered — it may have already been collected or cancelled. job__list shows active jobs."
            ),
        })
    }
}

fn whitelist_rejection(tool: &str) -> Option<Value> {
    let non_invoke_mcp_prefixes = [
        MCP_SEARCH_META_FUNCTION_NAME_PREFIX,
        MCP_DESCRIBE_META_FUNCTION_NAME_PREFIX,
        MCP_READ_META_FUNCTION_NAME_PREFIX,
        MCP_PROMPT_META_FUNCTION_NAME_PREFIX,
    ];
    let reason = if tool.starts_with(AGENT_FUNCTION_PREFIX) || tool.starts_with(JOB_FUNCTION_PREFIX)
    {
        Some(format!(
            "'{tool}' is already asynchronous — call it directly. Agents may start jobs, but jobs never start agents or other jobs."
        ))
    } else if tool.starts_with(USER_FUNCTION_PREFIX) {
        Some(format!(
            "'{tool}' is interactive and must run in-turn — a background job cannot touch the terminal. Call it directly."
        ))
    } else if tool.starts_with(TODO_FUNCTION_PREFIX)
        || tool.starts_with(MEMORY_FUNCTION_PREFIX)
        || tool.starts_with(SKILL_FUNCTION_PREFIX)
        || tool.starts_with(RAG_FUNCTION_PREFIX)
    {
        Some(format!(
            "'{tool}' mutates agent/session state and must run in-turn. Call it directly."
        ))
    } else if tool.starts_with("fs_") || tool == "ast_grep" {
        Some(format!(
            "'{tool}' is fast — invoke it directly instead of backgrounding it."
        ))
    } else if non_invoke_mcp_prefixes
        .iter()
        .any(|prefix| tool.starts_with(prefix))
    {
        Some(format!(
            "'{tool}' is a sub-second call; invoke it directly."
        ))
    } else {
        None
    };

    reason.map(|why| {
        json!({
            "status": "error",
            "message": format!(
                "{why} Backgroundable tools: external command tools (e.g. execute_command) and mcp_invoke_* calls."
            ),
        })
    })
}

/// Whether a declared tool could be run as a background job. This is the
/// declare-side twin of `whitelist_rejection`: a tool is backgroundable
/// exactly when `job__start` would not reject it by name.
pub fn is_backgroundable_tool(tool: &str) -> bool {
    whitelist_rejection(tool).is_none()
}

async fn handle_start(ctx: &mut RequestContext, args: &Value) -> Result<Value> {
    if !jobs_enabled(ctx.agent.as_ref(), &ctx.app.config) {
        return Ok(json!({
            "status": "error",
            "message": "Background jobs are disabled in this context (max_concurrent_jobs is 0).",
        }));
    }

    let tool = args
        .get("tool")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("'tool' is required"))?
        .to_string();
    let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));

    if let Some(rejection) = whitelist_rejection(&tool) {
        return Ok(rejection);
    }

    if !ctx.declared_function_names.contains(&tool) {
        return Ok(json!({
            "status": "error",
            "message": format!(
                "'{tool}' is not enabled in this context — job__start can only background tools declared to you in this \
                 request. Use the exact name of a tool from your current catalog."
            ),
        }));
    }

    let supervisor = match ctx.supervisor.as_ref() {
        Some(sup) => Arc::clone(sup),
        None => {
            let max_jobs = effective_max_concurrent_jobs(ctx.agent.as_ref(), &ctx.app.config);
            let sup = Arc::new(RwLock::new(
                Supervisor::new(0, 0).with_max_concurrent_jobs(max_jobs),
            ));
            ctx.supervisor = Some(Arc::clone(&sup));
            sup
        }
    };

    {
        let sup = supervisor.read();
        if sup.active_job_count() >= sup.max_concurrent_jobs() {
            return Ok(json!({
                "status": "error",
                "message": format!(
                    "At capacity: {}/{} jobs running. Collect or cancel one first.",
                    sup.active_job_count(),
                    sup.max_concurrent_jobs()
                ),
            }));
        }
    }

    let short_uuid = &Uuid::new_v4().to_string()[..8];
    let job_id = format!("job_{short_uuid}");
    let state = Arc::new(Mutex::new(JobState {
        status: JobStatus::Running,
        pgid: None,
    }));
    let output_buf = Arc::new(Mutex::new(RingBuf::default()));

    let join_handle = if tool.starts_with(MCP_INVOKE_META_FUNCTION_NAME_PREFIX) {
        let server = tool
            .strip_prefix(&format!("{MCP_INVOKE_META_FUNCTION_NAME_PREFIX}_"))
            .ok_or_else(|| anyhow!("Malformed MCP invoke function name: {tool}"))?
            .to_string();
        let Some(server_handle) = ctx.tool_scope.mcp_runtime.get(&server) else {
            return Ok(json!({
                "status": "error",
                "message": format!("MCP server '{server}' is not connected in this context."),
            }));
        };
        let inner_tool = arguments
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Missing 'tool' in arguments"))?
            .to_string();
        let inner_args = arguments
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut mcp_runtime = McpRuntime::new();
        mcp_runtime.insert(server.clone(), Arc::clone(server_handle));
        let job_ctx = JobCtx {
            mcp_runtime,
            current_depth: ctx.current_depth,
        };
        let task_state = Arc::clone(&state);
        let task_notifications = Arc::clone(&ctx.notification_queue);
        let notify_id = job_id.clone();
        let notify_tool = tool.clone();
        tokio::spawn(async move {
            let result = run_mcp_job(job_ctx, server, inner_tool, inner_args).await;
            let success = result.is_ok();
            task_state.lock().status = if success {
                JobStatus::Completed
            } else {
                JobStatus::Failed
            };
            task_notifications.push(job_notification(&notify_id, &notify_tool, success));
            result
        })
    } else {
        let snapshot = build_env_snapshot(ctx, &tool, &arguments)?;
        let task_state = Arc::clone(&state);
        let task_buf = Arc::clone(&output_buf);
        let task_notifications = Arc::clone(&ctx.notification_queue);
        let notify_id = job_id.clone();
        let notify_tool = tool.clone();
        tokio::spawn(async move {
            let result = run_process_job(snapshot, Arc::clone(&task_state), task_buf).await;
            let success = matches!(&result, Ok(job_result) if job_result.exit_code == Some(0));
            let mut job_state = task_state.lock();
            job_state.pgid = None;
            job_state.status = if success {
                JobStatus::Completed
            } else {
                JobStatus::Failed
            };

            drop(job_state);

            task_notifications.push(job_notification(&notify_id, &notify_tool, success));
            result
        })
    };

    let handle = JobHandle {
        id: job_id.clone(),
        tool: tool.clone(),
        started_at: Instant::now(),
        join_handle,
        abort_signal: create_abort_signal(),
        state,
        output_buf,
        no_change_checks: 0,
        last_check_state: None,
    };

    // On a capacity race the handle is dropped here, which kills the process
    // group and aborts the task.
    if let Err(e) = supervisor.write().register(handle) {
        return Ok(json!({
            "status": "error",
            "message": format!("{e}"),
        }));
    }

    if let Some(scope) = ctx.node_job_scope.as_mut() {
        scope.push(job_id.clone());
    }

    Ok(json!({
        "status": "ok",
        "job_id": job_id,
        "tool": tool,
        "message": "Running in background. Check with job__check, block with job__collect, cancel with job__cancel. \
                    You will receive a system_notifications entry on completion. Jobs do not survive coyote exiting.",
    }))
}

fn handle_check(ctx: &RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let Some(supervisor) = ctx.supervisor.as_ref() else {
        return Ok(job_miss_error(None, id));
    };
    let mut sup = supervisor.write();
    let Some(job) = sup.job_mut(id) else {
        drop(sup);
        return Ok(job_miss_error(ctx.supervisor.as_ref(), id));
    };

    let status = job.state.lock().status;
    let (tail, total_written) = {
        let buf = job.output_buf.lock();
        (buf.tail(), buf.total_written())
    };
    let check_state = (status, total_written);

    if job.last_check_state == Some(check_state) {
        job.no_change_checks += 1;
    } else {
        job.no_change_checks = 0;
        job.last_check_state = Some(check_state);
    }

    let tail_truncated = (tail.len() as u64) < total_written;
    let mut result = json!({
        "status": job_status_str(status),
        "id": id,
        "tool": job.tool,
        "elapsed_secs": job.started_at.elapsed().as_secs(),
        "output_tail": String::from_utf8_lossy(&tail).to_string(),
        "output_bytes_captured": total_written,
        "tail_truncated": tail_truncated,
    });

    if matches!(status, JobStatus::Running) {
        result["message"] = json!(
            "Job is still running. Call job__collect to block for the result, or do other work — you will be notified on completion."
        );

        if job.no_change_checks >= 3 {
            result["hint"] = json!(
                "No change across repeated checks — still running; call job__collect to block, or do other work — a system notification will fire on completion."
            );
        }
    } else {
        result["message"] = json!(format!(
            "Job finished — retrieve the result with job__collect --id {id}"
        ));
    }

    Ok(result)
}

async fn handle_collect(ctx: &RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;
    let tail_lines = args
        .get("tail_lines")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    let full_result = args
        .get("full_result")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let Some(supervisor) = ctx.supervisor.as_ref().cloned() else {
        return Ok(job_miss_error(None, id));
    };

    let target_abort = {
        let sup = supervisor.read();
        let Some(job) = sup.job(id) else {
            drop(sup);
            return Ok(job_miss_error(ctx.supervisor.as_ref(), id));
        };
        job.abort_signal.clone()
    };

    loop {
        let is_finished = {
            let sup = supervisor.read();
            sup.job(id).is_none_or(|job| job.join_handle.is_finished())
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
                "message": format!("Job '{id}' is still running, but child agents have pending escalations that need your reply. Reply via agent__reply_escalation, then call job__collect again."),
                "pending_escalations": summary,
            }));
        }

        if target_abort.aborted() {
            let deadline = time::Instant::now() + Duration::from_secs(2);
            while time::Instant::now() < deadline {
                let is_finished = {
                    let sup = supervisor.read();
                    sup.job(id).is_none_or(|job| job.join_handle.is_finished())
                };

                if is_finished {
                    break;
                }

                time::sleep(Duration::from_millis(50)).await;
            }

            break;
        }

        tokio::select! {
            _ = time::sleep(Duration::from_millis(200)) => {}
            _ = wait_abort_signal(&target_abort) => {}
        }
    }

    let handle = {
        let mut sup = supervisor.write();
        sup.take_job(id)
    };

    let Some(mut handle) = handle else {
        return Ok(json!({
            "status": "error",
            "message": format!("Job '{id}' completed but could not be collected. It may have been collected by another call."),
        }));
    };

    // Ctrl-C/exit teardown SIGTERMs the group without escalating, so a
    // TERM-ignoring process would hang this join forever. Bound it and
    // escalate to a group SIGKILL, gated on the pgid still being set.
    let joined = match time::timeout(JOB_KILL_GRACE, &mut handle.join_handle).await {
        Ok(joined) => joined,
        Err(_) => {
            #[cfg(unix)]
            if let Some(pgid) = handle.state.lock().pgid {
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
            }

            match time::timeout(JOB_KILL_GRACE, &mut handle.join_handle).await {
                Ok(joined) => joined,
                Err(_) => {
                    handle.join_handle.abort();
                    (&mut handle.join_handle).await
                }
            }
        }
    };
    let tool = handle.tool.clone();
    let elapsed_secs = handle.started_at.elapsed().as_secs();
    let status = handle.state.lock().status;
    let (tail, total_written) = {
        let buf = handle.output_buf.lock();
        (buf.tail(), buf.total_written())
    };
    let output_tail = String::from_utf8_lossy(&tail).to_string();

    let job_result = match joined {
        Err(join_err) => {
            return Ok(json!({
                "status": "failed",
                "id": id,
                "tool": tool,
                "error": format!("Job task panicked: {join_err}"),
                "output_tail": output_tail,
                "output_bytes_captured": total_written,
            }));
        }
        Ok(Err(e)) => {
            return Ok(json!({
                "status": "failed",
                "id": id,
                "tool": tool,
                "error": format!("{e}"),
                "output_tail": output_tail,
                "output_bytes_captured": total_written,
            }));
        }
        Ok(Ok(job_result)) => job_result,
    };

    let (result_value, result_truncated) = cap_result(job_result.output, tail_lines, full_result);
    let mut response = json!({
        "status": job_status_str(status),
        "id": id,
        "tool": tool,
        "elapsed_secs": elapsed_secs,
        "result": result_value,
        "output_tail": output_tail,
        "output_bytes_captured": job_result.output_bytes_captured,
    });

    if let Some(exit_code) = job_result.exit_code {
        response["exit_code"] = json!(exit_code);
    }
    if result_truncated {
        response["result_truncated"] = json!(true);
    }

    Ok(response)
}

async fn handle_cancel(ctx: &RequestContext, args: &Value) -> Result<Value> {
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("'id' is required"))?;

    let Some(supervisor) = ctx.supervisor.as_ref() else {
        return Ok(job_miss_error(None, id));
    };

    let handle = {
        let mut sup = supervisor.write();
        sup.take_job(id)
    };

    let Some(mut handle) = handle else {
        return Ok(job_miss_error(ctx.supervisor.as_ref(), id));
    };

    handle.abort_signal.set_ctrlc();
    kill_job_with_grace(&mut handle).await;

    let tool = handle.tool.clone();
    let (tail, total_written) = {
        let buf = handle.output_buf.lock();
        (buf.tail(), buf.total_written())
    };

    Ok(json!({
        "status": "cancelled",
        "id": id,
        "tool": tool,
        "output_tail": String::from_utf8_lossy(&tail).to_string(),
        "output_bytes_captured": total_written,
    }))
}

/// SIGTERM the process group, give it a grace period, then SIGKILL. Every
/// group kill is gated on `state.pgid` still being set: the job task clears
/// it right after `wait()` reaps the child, and killing after the reap could
/// signal an innocent recycled pid. MCP jobs (no pgid) fall through to a
/// plain task abort. Only the SIGKILL is re-gated; the SIGTERM fires after
/// the pgid read drops the lock, so a reap in that window could still hit a
/// recycled pid — accepted residual risk.
async fn kill_job_with_grace(handle: &mut JobHandle) {
    #[cfg(unix)]
    {
        let pgid = handle.state.lock().pgid;
        if let Some(pgid) = pgid {
            unsafe { libc::killpg(pgid, libc::SIGTERM) };
            if time::timeout(JOB_KILL_GRACE, &mut handle.join_handle)
                .await
                .is_ok()
            {
                return;
            }
            if handle.state.lock().pgid.is_some() {
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
            }
        }
    }
    handle.join_handle.abort();
    let _ = time::timeout(JOB_KILL_GRACE, &mut handle.join_handle).await;
}

/// Cancel and deregister the named jobs, if still registered. Used by graph
/// LLM nodes to enforce node-local job ownership: any job the node started
/// but did not collect or cancel by the time it exits is killed here, on
/// every exit path. Returns the ids actually reaped.
pub async fn reap_jobs(
    supervisor: Option<&Arc<RwLock<Supervisor>>>,
    ids: &[String],
) -> Vec<String> {
    let Some(supervisor) = supervisor else {
        return Vec::new();
    };
    let mut reaped = Vec::new();
    for id in ids {
        let handle = {
            let mut sup = supervisor.write();
            sup.take_job(id)
        };
        if let Some(mut handle) = handle {
            handle.abort_signal.set_ctrlc();
            kill_job_with_grace(&mut handle).await;
            warn!(
                "Reaped background job '{id}' ({}): left unreclaimed at graph node exit",
                handle.tool
            );
            reaped.push(id.clone());
        }
    }
    reaped
}

fn handle_list(ctx: &RequestContext) -> Result<Value> {
    let Some(supervisor) = ctx.supervisor.as_ref() else {
        return Ok(json!({
            "active_jobs": 0,
            "max_concurrent_jobs": effective_max_concurrent_jobs(ctx.agent.as_ref(), &ctx.app.config),
            "jobs": [],
        }));
    };
    let sup = supervisor.read();

    let jobs: Vec<Value> = sup
        .jobs()
        .map(|job| {
            let status = job.state.lock().status;
            json!({
                "id": job.id,
                "tool": job.tool,
                "status": job_status_str(status),
                "elapsed_secs": job.started_at.elapsed().as_secs(),
                "output_bytes_captured": job.output_buf.lock().total_written(),
            })
        })
        .collect();

    Ok(json!({
        "active_jobs": sup.active_job_count(),
        "max_concurrent_jobs": sup.max_concurrent_jobs(),
        "jobs": jobs,
    }))
}

/// Mirrors the foreground `extract_call_config` + `run_llm_function` env
/// assembly, resolved eagerly so the detached task owns everything it needs.
fn build_env_snapshot(
    ctx: &RequestContext,
    tool: &str,
    arguments: &Value,
) -> Result<JobEnvSnapshot> {
    let agent = ctx.agent.as_ref();
    let (cmd_name, mut cmd_args, mut envs) = match agent {
        Some(agent) => match agent.functions().find(tool) {
            Some(declaration) if declaration.agent => (
                format!("{}-{tool}", agent.name()),
                vec![tool.to_string()],
                agent.variable_envs(),
            ),
            Some(_) => (tool.to_string(), vec![], agent.variable_envs()),
            None => (tool.to_string(), vec![], HashMap::new()),
        },
        None => (tool.to_string(), vec![], HashMap::new()),
    };

    let mut bin_dirs: Vec<PathBuf> = vec![];
    if let Some(agent) = agent {
        let dir = paths::agent_bin_dir(agent.name());
        if dir.exists() {
            bin_dirs.push(dir);
        }
        if graph::agent_has_graph(agent.name()) {
            envs.insert("AUTO_CONFIRM".into(), "true".into());
        }
    } else {
        bin_dirs.push(paths::functions_bin_dir());
    }
    let current_path = env::var("PATH").context("No PATH environment variable")?;
    let prepend_path = bin_dirs
        .iter()
        .map(|v| format!("{}{PATH_SEP}", v.display()))
        .collect::<Vec<_>>()
        .join("");
    envs.insert("PATH".into(), format!("{prepend_path}{current_path}"));

    let output_file = temp_file("-job-", "");
    envs.insert("LLM_OUTPUT".into(), output_file.display().to_string());
    envs.insert("CLICOLOR_FORCE".into(), "1".into());
    envs.insert("FORCE_COLOR".into(), "1".into());

    cmd_args.push(arguments.to_string());

    #[cfg(windows)]
    let cmd_name = super::polyfill_cmd_name(&cmd_name, &bin_dirs);

    #[cfg(windows)]
    let cmd_args = {
        let mut args = cmd_args;
        if let Some(json_data) = args.pop() {
            let tool_data_file = temp_file("-tool-data-", ".json");
            fs::write(&tool_data_file, &json_data)?;
            envs.insert(
                "LLM_TOOL_DATA_FILE".into(),
                tool_data_file.display().to_string(),
            );
        }
        args
    };

    let timeout_secs = env::var("COYOTE_TOOL_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1800);

    Ok(JobEnvSnapshot {
        cmd_name,
        display_name: tool.to_string(),
        cmd_args,
        envs,
        output_file,
        timeout_secs,
    })
}

async fn pump_into_ring(mut reader: impl AsyncReadExt + Unpin, output_buf: Arc<Mutex<RingBuf>>) {
    let mut chunk = [0u8; 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => output_buf.lock().push(&chunk[..n]),
        }
    }
}

/// Deletes the env snapshot's temp files however the job task exits,
/// including a cancel/abort dropping the future mid-await.
struct TempFileGuard(Vec<PathBuf>);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

/// A grandchild that inherits the pipes keeps them open past the child's
/// exit; don't let that hold the job task past its own timeout.
async fn drain_pump(mut pump: tokio::task::JoinHandle<()>) {
    if time::timeout(JOB_PUMP_DRAIN_GRACE, &mut pump)
        .await
        .is_err()
    {
        pump.abort();
    }
}

async fn run_process_job(
    snapshot: JobEnvSnapshot,
    state: Arc<Mutex<JobState>>,
    output_buf: Arc<Mutex<RingBuf>>,
) -> Result<JobResult> {
    let mut temp_files = vec![snapshot.output_file.clone()];
    if let Some(tool_data_file) = snapshot.envs.get("LLM_TOOL_DATA_FILE") {
        temp_files.push(PathBuf::from(tool_data_file));
    }
    let _temp_guard = TempFileGuard(temp_files);

    let mut command = tokio::process::Command::new(&snapshot.cmd_name);
    command
        .args(&snapshot.cmd_args)
        .envs(&snapshot.envs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command
        .spawn()
        .map_err(|err| anyhow!("Unable to run {}, {err}", snapshot.display_name))?;

    #[cfg(unix)]
    if let Some(pid) = child.id() {
        state.lock().pgid = Some(pid as i32);
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("Failed to capture stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("Failed to capture stderr"))?;
    let stdout_pump = tokio::spawn(pump_into_ring(stdout, Arc::clone(&output_buf)));
    let stderr_pump = tokio::spawn(pump_into_ring(stderr, Arc::clone(&output_buf)));

    let wait_result = if snapshot.timeout_secs > 0 {
        match time::timeout(Duration::from_secs(snapshot.timeout_secs), child.wait()).await {
            Ok(wait_result) => wait_result,
            Err(_) => {
                kill_expired_job(&mut child, &state).await;
                state.lock().pgid = None;
                drain_pump(stdout_pump).await;
                drain_pump(stderr_pump).await;
                let output_bytes_captured = output_buf.lock().total_written();
                let message = format!(
                    "Tool call '{}' timed out after {}s and was killed (set COYOTE_TOOL_TIMEOUT to adjust; 0 = unlimited)",
                    snapshot.display_name, snapshot.timeout_secs
                );

                return Ok(JobResult {
                    output: json!({"tool_call_error": message}),
                    exit_code: None,
                    output_bytes_captured,
                });
            }
        }
    } else {
        child.wait().await
    };
    let status = match wait_result {
        Ok(status) => status,
        Err(err) => {
            stdout_pump.abort();
            stderr_pump.abort();
            bail!("Unable to run {}, {err}", snapshot.display_name);
        }
    };
    // pid-reuse guard: the child is reaped, so a later group kill against
    // this pgid could hit an innocent recycled pid.
    state.lock().pgid = None;
    drain_pump(stdout_pump).await;
    drain_pump(stderr_pump).await;
    let output_bytes_captured = output_buf.lock().total_written();

    let exit_code = status.code();
    if exit_code != Some(0) {
        let message = match exit_code {
            Some(code) => format!(
                "Tool call '{}' exited with code {code}",
                snapshot.display_name
            ),
            None => format!(
                "Tool call '{}' was terminated by a signal",
                snapshot.display_name
            ),
        };
        let mut error_json = json!({"tool_call_error": message});
        if let Ok(contents) = fs::read_to_string(&snapshot.output_file)
            && !contents.trim().is_empty()
        {
            error_json["output"] = json!(contents);
        }

        return Ok(JobResult {
            output: error_json,
            exit_code,
            output_bytes_captured,
        });
    }

    let mut output = Value::Null;
    if snapshot.output_file.exists() {
        let contents = fs::read_to_string(&snapshot.output_file)
            .context("Failed to retrieve tool call output")?;
        if !contents.is_empty() {
            output = serde_json::from_str(&contents)
                .ok()
                .unwrap_or_else(|| json!({"output": contents}));
        }
    }

    Ok(JobResult {
        output,
        exit_code,
        output_bytes_captured,
    })
}

/// Same kill discipline as `kill_job_with_grace`, driven through the owned
/// `Child`. The SIGTERM here fires after the pgid read drops the lock and is
/// not re-gated — the same accepted pid-reuse window.
async fn kill_expired_job(child: &mut tokio::process::Child, state: &Arc<Mutex<JobState>>) {
    #[cfg(unix)]
    {
        let pgid = state.lock().pgid;
        if let Some(pgid) = pgid {
            unsafe { libc::killpg(pgid, libc::SIGTERM) };
            if time::timeout(JOB_KILL_GRACE, child.wait()).await.is_err() {
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
                let _ = child.wait().await;
            }
            return;
        }
    }
    #[cfg(not(unix))]
    let _ = state;
    let _ = child.start_kill();
    let _ = child.wait().await;
}

async fn run_mcp_job(
    job_ctx: JobCtx,
    server: String,
    tool: String,
    arguments: Value,
) -> Result<JobResult> {
    let raw = match job_ctx.mcp_runtime.invoke(&server, &tool, arguments).await {
        Ok(raw) => raw,
        Err(e) => {
            if job_ctx.current_depth == 0 {
                let error_msg = format!("MCP job invocation failed: {e}");
                eprintln!("{}", muted_warning_text(&mcp_error_display(&error_msg)));
            }
            return Err(e);
        }
    };
    let output = render_tool_result(serde_json::to_value(raw)?, &server)?;

    Ok(JobResult {
        output,
        exit_code: None,
        output_bytes_captured: 0,
    })
}

/// Tail-biased result capping owned by the collect handler: keeps the LAST
/// `tail_lines`/50,000 chars (build failures land at the tail), always cutting
/// on a char boundary. `full_result` lifts the 50,000-char ceiling (the
/// session-wide tool-output limit still applies downstream); `tail_lines` is
/// honored either way.
fn cap_result(output: Value, tail_lines: Option<usize>, full_result: bool) -> (Value, bool) {
    if output.is_null() {
        return (json!("DONE"), false);
    }
    let mut text = match &output {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut truncated = false;
    if let Some(n) = tail_lines {
        let lines: Vec<&str> = text.lines().collect();
        if lines.len() > n {
            text = lines[lines.len() - n..].join("\n");
            truncated = true;
        }
    }
    if !full_result && let Some(capped) = tail_chars(&text, JOB_RESULT_TAIL_CAP_CHARS) {
        text = capped;
        truncated = true;
    }

    if truncated {
        (json!(text), true)
    } else {
        (output, false)
    }
}

fn tail_chars(text: &str, max_chars: usize) -> Option<String> {
    let total = text.chars().count();
    if total <= max_chars {
        return None;
    }

    let cut = text
        .char_indices()
        .nth(total - max_chars)
        .map(|(i, _)| i)
        .unwrap_or(0);

    Some(format!(
        "[truncated: kept last {max_chars} of {total} chars — the rest was not retained; next time \
         collect with full_result: true, or have the command write its output to a file]\n{}",
        &text[cut..]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AppConfig, AppState, WorkingMode};
    use crate::function::agents::{
        GuardrailAction, check_pending_tasks_guardrail, handle_agent_tool,
    };
    use crate::supervisor::mailbox::Inbox;
    use crate::supervisor::{AgentExitStatus, AgentHandle, AgentResult};
    use std::future::Future;
    use std::mem;

    fn default_app_state() -> Arc<AppState> {
        Arc::new(AppState::test_default())
    }

    fn app_state_with_config(update: impl FnOnce(&mut AppConfig)) -> Arc<AppState> {
        let mut state = AppState::test_default();
        let mut config = (*state.config).clone();
        update(&mut config);
        state.config = Arc::new(config);
        Arc::new(state)
    }

    fn plain_ctx() -> RequestContext {
        RequestContext::new(default_app_state(), WorkingMode::Cmd)
    }

    fn ctx_with_job_supervisor(max_jobs: usize) -> RequestContext {
        let mut ctx = plain_ctx();
        ctx.supervisor = Some(Arc::new(RwLock::new(
            Supervisor::new(0, 3).with_max_concurrent_jobs(max_jobs),
        )));
        ctx
    }

    fn make_running_job(id: &str) -> JobHandle {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let join_handle = rt.spawn(async {
            Ok(JobResult {
                output: Value::Null,
                exit_code: Some(0),
                output_bytes_captured: 0,
            })
        });
        mem::forget(rt);
        JobHandle {
            id: id.to_string(),
            tool: "execute_command".to_string(),
            started_at: Instant::now(),
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

    fn run_async<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[cfg(unix)]
    fn test_snapshot(cmd: &str, args: &[&str], timeout_secs: u64) -> JobEnvSnapshot {
        let output_file = temp_file("-job-test-", "");
        let mut envs = HashMap::new();
        envs.insert("PATH".to_string(), env::var("PATH").unwrap());
        envs.insert("LLM_OUTPUT".to_string(), output_file.display().to_string());
        JobEnvSnapshot {
            cmd_name: cmd.to_string(),
            display_name: cmd.to_string(),
            cmd_args: args.iter().map(|s| s.to_string()).collect(),
            envs,
            output_file,
            timeout_secs,
        }
    }

    #[test]
    fn ring_buf_returns_contents_below_capacity() {
        let mut buf = RingBuf::new(8);

        buf.push(b"abc");
        buf.push(b"de");

        assert_eq!(buf.tail(), b"abcde");
        assert_eq!(buf.total_written(), 5);
    }

    #[test]
    fn ring_buf_exact_fit_keeps_everything() {
        let mut buf = RingBuf::new(5);

        buf.push(b"abcde");

        assert_eq!(buf.tail(), b"abcde");
        assert_eq!(buf.total_written(), 5);
    }

    #[test]
    fn ring_buf_wrap_around_keeps_newest_bytes() {
        let mut buf = RingBuf::new(5);

        buf.push(b"abcde");
        buf.push(b"fg");

        assert_eq!(buf.tail(), b"cdefg");
        assert_eq!(buf.total_written(), 7);
    }

    #[test]
    fn ring_buf_oversize_push_keeps_last_capacity_bytes() {
        let mut buf = RingBuf::new(4);

        buf.push(b"abcdefghij");

        assert_eq!(buf.tail(), b"ghij");
        assert_eq!(buf.total_written(), 10);
    }

    #[test]
    fn ring_buf_default_capacity_is_64_kib() {
        let mut buf = RingBuf::default();
        let payload = vec![b'x'; 64 * 1024 + 1];

        buf.push(&payload);

        assert_eq!(buf.tail().len(), 64 * 1024);
        assert_eq!(buf.total_written(), 64 * 1024 + 1);
    }

    #[test]
    fn is_agent_task_matches_agent_prefixes() {
        assert!(is_agent_task(None, "agent_explore_a1b2c3d4"));
        assert!(is_agent_task(None, "graph_agent_explore_a1b2c3d4"));
        assert!(!is_agent_task(None, "job_deadbeef"));
    }

    #[test]
    fn is_agent_task_matches_registered_agents() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let join_handle = rt.spawn(async {
            Ok(AgentResult {
                id: "a1".into(),
                agent_name: "explore".into(),
                output: String::new(),
                exit_status: AgentExitStatus::Completed,
            })
        });
        mem::forget(rt);
        let handle = AgentHandle {
            id: "a1".to_string(),
            agent_name: "explore".to_string(),
            depth: 1,
            inbox: Arc::new(Inbox::new()),
            abort_signal: create_abort_signal(),
            join_handle,
            child_supervisor: None,
        };
        let mut sup = Supervisor::new(4, 3);
        sup.register(handle).unwrap();
        let sup = Arc::new(RwLock::new(sup));

        assert!(is_agent_task(Some(&sup), "a1"));
        assert!(!is_agent_task(Some(&sup), "missing"));
    }

    #[test]
    fn job_function_declarations_cover_all_five_actions() {
        let names: Vec<String> = job_function_declarations()
            .into_iter()
            .map(|d| d.name)
            .collect();

        assert_eq!(
            names,
            vec![
                "job__start",
                "job__check",
                "job__collect",
                "job__cancel",
                "job__list"
            ]
        );
    }

    #[test]
    fn whitelist_rejects_state_mutating_tools() {
        for tool in ["memory__write", "todo__add", "skill__load", "rag__query"] {
            let rejection = whitelist_rejection(tool).unwrap();

            let message = rejection["message"].as_str().unwrap();
            assert!(
                message.contains("mutates agent/session state"),
                "unexpected message for {tool}: {message}"
            );
        }
    }

    #[test]
    fn whitelist_rejects_async_and_interactive_tools() {
        for tool in ["agent__spawn", "job__check"] {
            let message = whitelist_rejection(tool).unwrap()["message"]
                .as_str()
                .unwrap()
                .to_string();

            assert!(
                message.contains("already asynchronous"),
                "unexpected message for {tool}: {message}"
            );
        }
        let message = whitelist_rejection("user__select").unwrap()["message"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(message.contains("interactive"));
    }

    #[test]
    fn whitelist_rejects_fast_mcp_meta_tools() {
        for tool in [
            "mcp_search_github",
            "mcp_describe_github",
            "mcp_read_github",
            "mcp_prompt_github",
        ] {
            let message = whitelist_rejection(tool).unwrap()["message"]
                .as_str()
                .unwrap()
                .to_string();

            assert!(
                message.contains("sub-second"),
                "unexpected message for {tool}: {message}"
            );
        }
    }

    #[test]
    fn whitelist_rejects_fast_file_builtins() {
        for tool in ["fs_read", "fs_cat", "fs_grep", "ast_grep"] {
            let message = whitelist_rejection(tool).unwrap()["message"]
                .as_str()
                .unwrap()
                .to_string();

            assert!(
                message.contains("is fast"),
                "unexpected message for {tool}: {message}"
            );
        }
    }

    #[test]
    fn whitelist_allows_external_and_mcp_invoke_tools() {
        assert!(whitelist_rejection("execute_command").is_none());
        assert!(whitelist_rejection("mcp_invoke_github").is_none());
    }

    #[test]
    fn handle_start_rejects_fast_builtins_without_spawn() {
        let mut ctx = plain_ctx();
        ctx.declared_function_names.insert("fs_read".into());
        ctx.declared_function_names.insert("ast_grep".into());

        for tool in ["fs_read", "ast_grep"] {
            let result = run_async(handle_start(
                &mut ctx,
                &json!({"tool": tool, "arguments": {}}),
            ))
            .unwrap();

            assert_eq!(result["status"], "error");
            assert!(result["message"].as_str().unwrap().contains("is fast"));
        }
        assert!(ctx.supervisor.is_none(), "no job may be spawned");
    }

    #[test]
    fn handle_start_rejects_non_whitelisted_tool_without_spawn() {
        let mut ctx = plain_ctx();
        ctx.declared_function_names.insert("memory__write".into());

        let result = run_async(handle_start(
            &mut ctx,
            &json!({"tool": "memory__write", "arguments": {}}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("mutates agent/session state")
        );
        assert!(ctx.supervisor.is_none(), "no job may be spawned");
    }

    #[test]
    fn handle_start_rejects_undeclared_tool_without_spawn() {
        let mut ctx = plain_ctx();

        let result = run_async(handle_start(
            &mut ctx,
            &json!({"tool": "execute_command", "arguments": {}}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("not enabled in this context")
        );
        assert!(ctx.supervisor.is_none(), "no job may be spawned");
    }

    #[test]
    fn handle_start_rejects_when_jobs_disabled() {
        let app_state = app_state_with_config(|config| config.max_concurrent_jobs = Some(0));
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        ctx.declared_function_names.insert("execute_command".into());

        let result = run_async(handle_start(
            &mut ctx,
            &json!({"tool": "execute_command", "arguments": {}}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(result["message"].as_str().unwrap().contains("disabled"));
        assert!(ctx.supervisor.is_none());
    }

    #[test]
    fn handle_start_rejects_at_capacity_without_spawn() {
        let mut ctx = ctx_with_job_supervisor(1);
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(make_running_job("j1"))
            .unwrap();
        ctx.declared_function_names.insert("execute_command".into());

        let result = run_async(handle_start(
            &mut ctx,
            &json!({"tool": "execute_command", "arguments": {}}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("At capacity: 1/1")
        );
        assert_eq!(
            ctx.supervisor.as_ref().unwrap().read().active_job_count(),
            1
        );
    }

    #[test]
    fn is_backgroundable_tool_matches_start_whitelist() {
        assert!(is_backgroundable_tool("execute_command"));
        assert!(is_backgroundable_tool("my_custom_tool.sh"));
        assert!(is_backgroundable_tool("mcp_invoke_github"));
        assert!(!is_backgroundable_tool("job__start"));
        assert!(!is_backgroundable_tool("agent__spawn"));
        assert!(!is_backgroundable_tool("user__confirm"));
        assert!(!is_backgroundable_tool("todo__add"));
        assert!(!is_backgroundable_tool("fs_read"));
        assert!(!is_backgroundable_tool("ast_grep"));
        assert!(!is_backgroundable_tool("mcp_search_github"));
    }

    #[cfg(unix)]
    #[test]
    fn handle_start_records_job_in_node_scope() {
        run_async(async {
            let mut ctx = plain_ctx();
            ctx.node_job_scope = Some(Vec::new());
            ctx.declared_function_names.insert("echo".into());

            let started = handle_start(&mut ctx, &json!({"tool": "echo", "arguments": {}}))
                .await
                .unwrap();

            let job_id = started["job_id"].as_str().unwrap().to_string();
            assert_eq!(ctx.node_job_scope.clone().unwrap(), vec![job_id.clone()]);

            let collected = handle_collect(&ctx, &json!({"id": job_id})).await.unwrap();
            assert_eq!(collected["status"], "completed");
        });
    }

    #[test]
    fn reap_jobs_kills_registered_jobs_and_reports_ids() {
        run_async(async {
            let ctx = ctx_with_job_supervisor(4);
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(make_running_job("j1"))
                .unwrap();

            let reaped = reap_jobs(
                ctx.supervisor.as_ref(),
                &["j1".to_string(), "missing".to_string()],
            )
            .await;

            assert_eq!(reaped, vec!["j1".to_string()]);
            assert!(!ctx.supervisor.as_ref().unwrap().read().has_job("j1"));
        });
    }

    #[test]
    fn handle_start_rejects_unconnected_mcp_server() {
        let mut ctx = plain_ctx();
        ctx.declared_function_names
            .insert("mcp_invoke_github".into());

        let result = run_async(handle_start(
            &mut ctx,
            &json!({"tool": "mcp_invoke_github", "arguments": {"tool": "search"}}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("not connected")
        );
        assert_eq!(
            ctx.supervisor.as_ref().unwrap().read().active_job_count(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn handle_start_lazy_inits_supervisor_and_collect_returns_result() {
        run_async(async {
            let mut ctx = plain_ctx();
            ctx.declared_function_names.insert("echo".into());

            let started = handle_start(&mut ctx, &json!({"tool": "echo", "arguments": {}}))
                .await
                .unwrap();

            assert_eq!(started["status"], "ok");
            let job_id = started["job_id"].as_str().unwrap().to_string();
            assert!(job_id.starts_with("job_"));
            assert!(
                ctx.supervisor.is_some(),
                "plain sessions lazily init a supervisor"
            );

            let collected = handle_collect(&ctx, &json!({"id": job_id})).await.unwrap();

            assert_eq!(collected["status"], "completed");
            assert_eq!(collected["result"], "DONE");
            assert_eq!(collected["exit_code"], 0);
            assert!(collected["output_tail"].as_str().unwrap().contains("{}"));
            assert!(!ctx.supervisor.as_ref().unwrap().read().has_job(&job_id));
        });
    }

    #[test]
    fn handle_check_unknown_id_teaches_job_list() {
        let ctx = ctx_with_job_supervisor(4);

        let result = handle_check(&ctx, &json!({"id": "job_x"})).unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("No job 'job_x' is registered")
        );
    }

    #[cfg(unix)]
    #[test]
    fn job_completion_pushes_notification_for_own_context() {
        run_async(async {
            let mut ctx = plain_ctx();
            ctx.declared_function_names.insert("echo".into());

            let started = handle_start(&mut ctx, &json!({"tool": "echo", "arguments": {}}))
                .await
                .unwrap();
            let job_id = started["job_id"].as_str().unwrap().to_string();

            let supervisor = ctx.supervisor.clone().unwrap();
            let deadline = time::Instant::now() + Duration::from_secs(5);
            while time::Instant::now() < deadline {
                let finished = supervisor
                    .read()
                    .job(&job_id)
                    .is_none_or(|job| job.join_handle.is_finished());
                if finished {
                    break;
                }
                time::sleep(Duration::from_millis(10)).await;
            }

            let events = ctx.notification_queue.drain();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event, "job_completed");
            assert_eq!(events[0].id, job_id);
            assert_eq!(events[0].tool_or_agent, "echo");
            assert_eq!(events[0].status, "success");
            assert_eq!(
                events[0].next_action,
                format!("job__collect --id {job_id} for output")
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn job_failure_pushes_failed_notification() {
        run_async(async {
            let mut ctx = plain_ctx();
            ctx.declared_function_names.insert("false".into());

            let started = handle_start(&mut ctx, &json!({"tool": "false", "arguments": {}}))
                .await
                .unwrap();
            let job_id = started["job_id"].as_str().unwrap().to_string();

            let supervisor = ctx.supervisor.clone().unwrap();
            let deadline = time::Instant::now() + Duration::from_secs(5);
            while time::Instant::now() < deadline {
                let finished = supervisor
                    .read()
                    .job(&job_id)
                    .is_none_or(|job| job.join_handle.is_finished());
                if finished {
                    break;
                }
                time::sleep(Duration::from_millis(10)).await;
            }

            let events = ctx.notification_queue.drain();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event, "job_failed");
            assert_eq!(events[0].status, "failed");
        });
    }

    #[test]
    fn cancelled_job_notification_is_suppressed_at_drain() {
        run_async(async {
            let ctx = ctx_with_job_supervisor(4);
            let join_handle = tokio::spawn(async {
                time::sleep(Duration::from_secs(30)).await;
                Ok(JobResult {
                    output: Value::Null,
                    exit_code: Some(0),
                    output_bytes_captured: 0,
                })
            });
            let handle = JobHandle {
                id: "j1".to_string(),
                tool: "execute_command".to_string(),
                started_at: Instant::now(),
                join_handle,
                abort_signal: create_abort_signal(),
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
            ctx.notification_queue
                .push(job_notification("j1", "execute_command", false));

            let result = handle_cancel(&ctx, &json!({"id": "j1"})).await.unwrap();

            assert_eq!(result["status"], "cancelled");
            assert!(
                super::super::drain_live_notifications(&ctx).is_empty(),
                "events for a cancelled job must never reach the model"
            );
        });
    }

    #[test]
    fn already_collected_job_notification_is_suppressed_at_drain() {
        let ctx = ctx_with_job_supervisor(4);
        ctx.notification_queue
            .push(job_notification("j1", "execute_command", true));

        assert!(
            super::super::drain_live_notifications(&ctx).is_empty(),
            "events for an already-collected job must be dropped"
        );
    }

    #[test]
    fn job_handlers_teach_cross_kind_for_agent_ids() {
        let ctx = ctx_with_job_supervisor(4);
        for result in [
            handle_check(&ctx, &json!({"id": "agent_explore_1"})).unwrap(),
            run_async(handle_collect(&ctx, &json!({"id": "agent_explore_1"}))).unwrap(),
            run_async(handle_cancel(&ctx, &json!({"id": "agent_explore_1"}))).unwrap(),
        ] {
            assert_eq!(result["status"], "error");
            assert!(
                result["message"]
                    .as_str()
                    .unwrap()
                    .contains("is a spawned agent, not a background job")
            );
        }
    }

    #[test]
    fn job_handlers_miss_without_supervisor() {
        let ctx = plain_ctx();

        let result = handle_check(&ctx, &json!({"id": "job_x"})).unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("No job 'job_x'")
        );
    }

    #[test]
    fn handle_check_reports_running_job_tail() {
        let ctx = ctx_with_job_supervisor(4);
        let job = make_running_job("j1");
        job.output_buf.lock().push(b"hello");
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(job)
            .unwrap();

        let result = handle_check(&ctx, &json!({"id": "j1"})).unwrap();

        assert_eq!(result["status"], "running");
        assert_eq!(result["tool"], "execute_command");
        assert_eq!(result["output_tail"], "hello");
        assert_eq!(result["output_bytes_captured"], 5);
        assert_eq!(result["tail_truncated"], false);
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("still running")
        );
    }

    #[test]
    fn handle_check_finished_job_points_at_collect_without_consuming() {
        let ctx = ctx_with_job_supervisor(4);
        let job = make_running_job("j1");
        job.state.lock().status = JobStatus::Completed;
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(job)
            .unwrap();

        let result = handle_check(&ctx, &json!({"id": "j1"})).unwrap();

        assert_eq!(result["status"], "completed");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("job__collect --id j1")
        );
        assert!(ctx.supervisor.as_ref().unwrap().read().has_job("j1"));
    }

    #[test]
    fn handle_check_hints_after_repeated_unchanged_checks() {
        let ctx = ctx_with_job_supervisor(4);
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(make_running_job("j1"))
            .unwrap();

        for _ in 0..3 {
            let result = handle_check(&ctx, &json!({"id": "j1"})).unwrap();
            assert!(result.get("hint").is_none());
        }
        let result = handle_check(&ctx, &json!({"id": "j1"})).unwrap();
        assert_eq!(
            result["hint"],
            "No change across repeated checks — still running; call job__collect to block, or do other work — a system notification will fire on completion."
        );
    }

    #[test]
    fn handle_check_no_change_counter_resets_on_output_change() {
        let ctx = ctx_with_job_supervisor(4);
        let job = make_running_job("j1");
        let output_buf = Arc::clone(&job.output_buf);
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(job)
            .unwrap();

        for _ in 0..3 {
            handle_check(&ctx, &json!({"id": "j1"})).unwrap();
        }
        assert!(
            handle_check(&ctx, &json!({"id": "j1"}))
                .unwrap()
                .get("hint")
                .is_some()
        );

        output_buf.lock().push(b"more");
        for _ in 0..3 {
            let result = handle_check(&ctx, &json!({"id": "j1"})).unwrap();
            assert!(result.get("hint").is_none());
        }
        assert!(
            handle_check(&ctx, &json!({"id": "j1"}))
                .unwrap()
                .get("hint")
                .is_some()
        );
    }

    #[test]
    fn handle_check_finished_job_never_hints() {
        let ctx = ctx_with_job_supervisor(4);
        let job = make_running_job("j1");
        job.state.lock().status = JobStatus::Completed;
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(job)
            .unwrap();

        for _ in 0..5 {
            let result = handle_check(&ctx, &json!({"id": "j1"})).unwrap();
            assert!(result.get("hint").is_none());
            assert!(
                result["message"]
                    .as_str()
                    .unwrap()
                    .contains("job__collect --id j1")
            );
        }
    }

    #[test]
    fn handle_collect_applies_tail_lines() {
        run_async(async {
            let ctx = ctx_with_job_supervisor(4);
            let join_handle = tokio::spawn(async {
                Ok(JobResult {
                    output: json!("l1\nl2\nl3"),
                    exit_code: Some(0),
                    output_bytes_captured: 0,
                })
            });
            let handle = JobHandle {
                id: "j1".to_string(),
                tool: "execute_command".to_string(),
                started_at: Instant::now(),
                join_handle,
                abort_signal: create_abort_signal(),
                state: Arc::new(Mutex::new(JobState {
                    status: JobStatus::Completed,
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

            let result = handle_collect(&ctx, &json!({"id": "j1", "tail_lines": 2}))
                .await
                .unwrap();

            assert_eq!(result["status"], "completed");
            assert_eq!(result["result"], "l2\nl3");
            assert_eq!(result["result_truncated"], true);
        });
    }

    #[test]
    fn handle_collect_full_result_returns_uncapped() {
        run_async(async {
            let ctx = ctx_with_job_supervisor(4);
            let big = "x".repeat(JOB_RESULT_TAIL_CAP_CHARS + 10);
            let payload = big.clone();
            let join_handle = tokio::spawn(async move {
                Ok(JobResult {
                    output: json!(payload),
                    exit_code: Some(0),
                    output_bytes_captured: 0,
                })
            });
            let handle = JobHandle {
                id: "j1".to_string(),
                tool: "execute_command".to_string(),
                started_at: Instant::now(),
                join_handle,
                abort_signal: create_abort_signal(),
                state: Arc::new(Mutex::new(JobState {
                    status: JobStatus::Completed,
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

            let result = handle_collect(&ctx, &json!({"id": "j1", "full_result": true}))
                .await
                .unwrap();

            assert_eq!(result["status"], "completed");
            assert_eq!(result["result"], json!(big));
            assert!(result.get("result_truncated").is_none());
        });
    }

    #[test]
    fn handle_collect_maps_panic_to_failed_with_ring_content() {
        run_async(async {
            let ctx = ctx_with_job_supervisor(4);
            let join_handle: tokio::task::JoinHandle<Result<JobResult>> =
                tokio::spawn(async { panic!("boom") });
            let output_buf = Arc::new(Mutex::new(RingBuf::default()));
            output_buf.lock().push(b"partial");
            let handle = JobHandle {
                id: "j1".to_string(),
                tool: "execute_command".to_string(),
                started_at: Instant::now(),
                join_handle,
                abort_signal: create_abort_signal(),
                state: Arc::new(Mutex::new(JobState {
                    status: JobStatus::Failed,
                    pgid: None,
                })),
                output_buf,
                no_change_checks: 0,
                last_check_state: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            let result = handle_collect(&ctx, &json!({"id": "j1"})).await.unwrap();

            assert_eq!(result["status"], "failed");
            assert!(result["error"].as_str().unwrap().contains("panicked"));
            assert_eq!(result["output_tail"], "partial");
        });
    }

    /// The completion notification is pushed from inside the job task, after
    /// the run — a panic unwinds past the push, and neither the supervisor nor
    /// collect synthesizes a notification for a panicked job.
    #[test]
    fn panicked_job_task_skips_the_completion_notification() {
        run_async(async {
            let ctx = ctx_with_job_supervisor(4);
            let join_handle: tokio::task::JoinHandle<Result<JobResult>> =
                tokio::spawn(async { panic!("boom") });
            let handle = JobHandle {
                id: "j1".to_string(),
                tool: "execute_command".to_string(),
                started_at: Instant::now(),
                join_handle,
                abort_signal: create_abort_signal(),
                state: Arc::new(Mutex::new(JobState {
                    // A panic never reaches the status update — the cell
                    // stays Running, which is the real post-panic state.
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

            let result = handle_collect(&ctx, &json!({"id": "j1"})).await.unwrap();

            assert_eq!(result["status"], "failed");
            assert!(result["error"].as_str().unwrap().contains("panicked"));
            assert!(
                ctx.notification_queue.drain().is_empty(),
                "a panicked job must never produce a completion notification"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn handle_cancel_kills_running_process_job() {
        run_async(async {
            let ctx = ctx_with_job_supervisor(4);
            let state = Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            }));
            let output_buf = Arc::new(Mutex::new(RingBuf::default()));
            let snapshot = test_snapshot("sleep", &["30"], 0);
            let task_state = Arc::clone(&state);
            let task_buf = Arc::clone(&output_buf);
            let join_handle =
                tokio::spawn(async move { run_process_job(snapshot, task_state, task_buf).await });

            let deadline = time::Instant::now() + Duration::from_secs(2);
            while state.lock().pgid.is_none() && time::Instant::now() < deadline {
                time::sleep(Duration::from_millis(10)).await;
            }
            let pgid = state.lock().pgid.expect("runner must record the pgid");

            let handle = JobHandle {
                id: "j1".to_string(),
                tool: "sleep".to_string(),
                started_at: Instant::now(),
                join_handle,
                abort_signal: create_abort_signal(),
                state: Arc::clone(&state),
                output_buf,
                no_change_checks: 0,
                last_check_state: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            let result = handle_cancel(&ctx, &json!({"id": "j1"})).await.unwrap();

            assert_eq!(result["status"], "cancelled");
            assert_eq!(result["tool"], "sleep");
            assert!(!ctx.supervisor.as_ref().unwrap().read().has_job("j1"));
            assert_eq!(
                unsafe { libc::killpg(pgid, 0) },
                -1,
                "process group must be dead after cancel"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        });
    }

    #[test]
    fn handle_list_reports_jobs_and_capacity() {
        let ctx = ctx_with_job_supervisor(4);
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(make_running_job("j1"))
            .unwrap();

        let result = handle_list(&ctx).unwrap();

        assert_eq!(result["active_jobs"], 1);
        assert_eq!(result["max_concurrent_jobs"], 4);
        assert_eq!(result["jobs"][0]["id"], "j1");
        assert_eq!(result["jobs"][0]["status"], "running");
    }

    #[test]
    fn handle_list_without_supervisor_reports_empty() {
        let ctx = plain_ctx();

        let result = handle_list(&ctx).unwrap();

        assert_eq!(result["active_jobs"], 0);
        assert_eq!(result["max_concurrent_jobs"], 5);
        assert_eq!(result["jobs"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn cap_result_normalizes_null_to_done() {
        let (value, truncated) = cap_result(Value::Null, None, false);

        assert_eq!(value, json!("DONE"));
        assert!(!truncated);
    }

    #[test]
    fn cap_result_preserves_small_values() {
        let (value, truncated) = cap_result(json!({"a": 1}), None, false);

        assert_eq!(value, json!({"a": 1}));
        assert!(!truncated);
    }

    #[test]
    fn cap_result_keeps_last_chars_on_char_boundary() {
        let text = "é".repeat(JOB_RESULT_TAIL_CAP_CHARS + 10);
        let (value, truncated) = cap_result(json!(text), None, false);
        assert!(truncated);
        let capped = value.as_str().unwrap();
        assert!(capped.starts_with(&format!(
            "[truncated: kept last {} of {} chars — ",
            JOB_RESULT_TAIL_CAP_CHARS,
            JOB_RESULT_TAIL_CAP_CHARS + 10
        )));
    }

    #[test]
    fn cap_result_full_result_skips_char_cap() {
        let text = "a".repeat(JOB_RESULT_TAIL_CAP_CHARS + 10);
        let (value, truncated) = cap_result(json!(text), None, true);

        assert!(!truncated);
        assert_eq!(value, json!(text));
    }

    #[test]
    fn cap_result_full_result_still_applies_tail_lines() {
        let (value, truncated) = cap_result(json!("l1\nl2\nl3"), Some(2), true);

        assert!(truncated);
        assert_eq!(value, json!("l2\nl3"));
    }

    #[test]
    fn tail_chars_floors_to_char_boundary() {
        let capped = tail_chars("aébc", 2).unwrap();

        assert!(capped.ends_with("bc"));
        assert!(capped.starts_with("[truncated: kept last 2 of 4 chars — "));
        assert!(capped.contains("full_result: true"));
        assert!(tail_chars("abc", 3).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn run_process_job_times_out_and_clears_pgid() {
        run_async(async {
            let state = Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            }));
            let output_buf = Arc::new(Mutex::new(RingBuf::default()));
            let snapshot = test_snapshot("sleep", &["30"], 1);

            let result = run_process_job(snapshot, Arc::clone(&state), output_buf)
                .await
                .unwrap();

            assert_eq!(result.exit_code, None);
            assert!(
                result.output["tool_call_error"]
                    .as_str()
                    .unwrap()
                    .contains("timed out after 1s")
            );
            assert!(
                state.lock().pgid.is_none(),
                "pid-reuse guard must clear pgid"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn run_process_job_reads_llm_output_and_captures_ring() {
        run_async(async {
            let state = Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            }));
            let output_buf = Arc::new(Mutex::new(RingBuf::default()));
            let snapshot = test_snapshot(
                "sh",
                &["-c", "printf hi > \"$LLM_OUTPUT\"; echo captured"],
                0,
            );

            let result = run_process_job(snapshot, state, Arc::clone(&output_buf))
                .await
                .unwrap();

            assert_eq!(result.exit_code, Some(0));
            assert_eq!(result.output, json!({"output": "hi"}));
            let tail = String::from_utf8_lossy(&output_buf.lock().tail()).to_string();
            assert!(tail.contains("captured"));
            assert_eq!(result.output_bytes_captured, 9);
        });
    }

    #[cfg(unix)]
    #[test]
    fn run_process_job_reports_nonzero_exit_with_partial_output() {
        run_async(async {
            let state = Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            }));
            let output_buf = Arc::new(Mutex::new(RingBuf::default()));
            let snapshot =
                test_snapshot("sh", &["-c", "printf partial > \"$LLM_OUTPUT\"; exit 3"], 0);

            let result = run_process_job(snapshot, state, output_buf).await.unwrap();

            assert_eq!(result.exit_code, Some(3));
            assert!(
                result.output["tool_call_error"]
                    .as_str()
                    .unwrap()
                    .contains("exited with code 3")
            );
            assert_eq!(result.output["output"], "partial");
        });
    }

    #[cfg(unix)]
    #[test]
    fn run_process_job_reports_signal_death_as_error() {
        run_async(async {
            let state = Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            }));
            let output_buf = Arc::new(Mutex::new(RingBuf::default()));
            let snapshot = test_snapshot("sh", &["-c", "kill -KILL $$"], 0);

            let result = run_process_job(snapshot, state, output_buf).await.unwrap();

            assert_eq!(result.exit_code, None);
            assert!(
                result.output["tool_call_error"]
                    .as_str()
                    .unwrap()
                    .contains("terminated by a signal")
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn run_process_job_removes_output_temp_file() {
        run_async(async {
            let state = Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            }));
            let output_buf = Arc::new(Mutex::new(RingBuf::default()));
            let snapshot = test_snapshot("sh", &["-c", "printf hi > \"$LLM_OUTPUT\""], 0);
            let output_file = snapshot.output_file.clone();

            let result = run_process_job(snapshot, state, output_buf).await.unwrap();

            assert_eq!(result.exit_code, Some(0));
            assert_eq!(result.output, json!({"output": "hi"}));
            assert!(!output_file.exists(), "temp file must be removed");
        });
    }

    #[cfg(unix)]
    #[test]
    fn handle_collect_returns_after_term_ignoring_job_is_aborted() {
        run_async(async {
            let ctx = ctx_with_job_supervisor(4);
            let state = Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            }));
            let output_buf = Arc::new(Mutex::new(RingBuf::default()));
            let snapshot =
                test_snapshot("sh", &["-c", "trap '' TERM; while :; do sleep 1; done"], 0);
            let task_state = Arc::clone(&state);
            let task_buf = Arc::clone(&output_buf);
            let join_handle = tokio::spawn(async move {
                let result = run_process_job(snapshot, Arc::clone(&task_state), task_buf).await;
                let mut job_state = task_state.lock();
                job_state.pgid = None;
                job_state.status = match &result {
                    Ok(job_result) if job_result.exit_code == Some(0) => JobStatus::Completed,
                    _ => JobStatus::Failed,
                };
                drop(job_state);
                result
            });

            let deadline = time::Instant::now() + Duration::from_secs(2);
            while state.lock().pgid.is_none() && time::Instant::now() < deadline {
                time::sleep(Duration::from_millis(10)).await;
            }
            let pgid = state.lock().pgid.expect("runner must record the pgid");

            let abort_signal = create_abort_signal();
            let handle = JobHandle {
                id: "j1".to_string(),
                tool: "sh".to_string(),
                started_at: Instant::now(),
                join_handle,
                abort_signal: abort_signal.clone(),
                state: Arc::clone(&state),
                output_buf,
                no_change_checks: 0,
                last_check_state: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            // Mimic Ctrl-C teardown: SIGTERM the group and flag the abort
            // signal, leaving the handle registered.
            abort_signal.set_ctrlc();
            unsafe { libc::killpg(pgid, libc::SIGTERM) };

            let result = time::timeout(
                Duration::from_secs(20),
                handle_collect(&ctx, &json!({"id": "j1"})),
            )
            .await
            .expect("collect must not hang on a TERM-ignoring job")
            .unwrap();

            assert_eq!(result["status"], "failed");
            // killpg(pgid, 0) is unreliable here: the orphaned sleep
            // grandchild lingers as an unreaped zombie and keeps the group
            // id alive. Signal death proves the escalated SIGKILL landed.
            assert!(
                result["result"]["tool_call_error"]
                    .as_str()
                    .unwrap()
                    .contains("terminated by a signal")
            );
        });
    }

    #[test]
    fn jobs_only_supervisor_rejects_agent_spawn_at_capacity_zero() {
        let mut ctx = ctx_with_job_supervisor(5);

        let result = run_async(handle_agent_tool(
            &mut ctx,
            "agent__spawn",
            &json!({"agent": "explore", "prompt": "x"}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("At capacity: 0/0")
        );
    }

    #[test]
    fn jobs_only_supervisor_guardrail_surfaces_running_job() {
        let mut ctx = ctx_with_job_supervisor(5);
        ctx.supervisor
            .as_ref()
            .unwrap()
            .write()
            .register(make_running_job("j1"))
            .unwrap();

        match check_pending_tasks_guardrail(&mut ctx) {
            GuardrailAction::Inject(prompt) => {
                assert!(prompt.contains("j1"));
                assert!(prompt.contains("job__collect"));
            }
            _ => panic!("expected Inject for a running job"),
        }
        assert_eq!(ctx.pending_tasks_guardrail_count, 1);

        let empty_ctx = &mut ctx_with_job_supervisor(5);
        assert!(matches!(
            check_pending_tasks_guardrail(empty_ctx),
            GuardrailAction::NoAction
        ));
    }

    #[test]
    fn jobs_only_supervisor_agent_surfaces_stay_functional() {
        let mut ctx = ctx_with_job_supervisor(5);

        let listed = run_async(handle_agent_tool(
            &mut ctx,
            "agent__list_running",
            &json!({}),
        ))
        .unwrap();
        assert_eq!(listed["active_count"], 0);
        assert_eq!(listed["max_concurrent"], 0);

        let created = run_async(handle_agent_tool(
            &mut ctx,
            "agent__task_create",
            &json!({"subject": "research"}),
        ))
        .unwrap();
        assert_eq!(created["status"], "ok");

        let tasks = run_async(handle_agent_tool(&mut ctx, "agent__task_list", &json!({}))).unwrap();
        assert_eq!(tasks["tasks"].as_array().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn handle_cancel_kills_grandchild_process() {
        run_async(async {
            let ctx = ctx_with_job_supervisor(4);
            let state = Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            }));
            let output_buf = Arc::new(Mutex::new(RingBuf::default()));
            let snapshot = test_snapshot("sh", &["-c", "sleep 30 & echo CHILD:$!; wait"], 0);
            let task_state = Arc::clone(&state);
            let task_buf = Arc::clone(&output_buf);
            let join_handle =
                tokio::spawn(async move { run_process_job(snapshot, task_state, task_buf).await });

            let deadline = time::Instant::now() + Duration::from_secs(5);
            let grandchild_pid = loop {
                let tail = String::from_utf8_lossy(&output_buf.lock().tail()).to_string();
                if let Some(rest) = tail.split("CHILD:").nth(1)
                    && let Some(line_end) = rest.find('\n')
                {
                    break rest[..line_end].trim().parse::<i32>().unwrap();
                }
                assert!(
                    time::Instant::now() < deadline,
                    "grandchild pid never appeared in the ring buffer"
                );
                time::sleep(Duration::from_millis(10)).await;
            };

            let handle = JobHandle {
                id: "j1".to_string(),
                tool: "sh".to_string(),
                started_at: Instant::now(),
                join_handle,
                abort_signal: create_abort_signal(),
                state: Arc::clone(&state),
                output_buf,
                no_change_checks: 0,
                last_check_state: None,
            };
            ctx.supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(handle)
                .unwrap();

            let result = handle_cancel(&ctx, &json!({"id": "j1"})).await.unwrap();

            assert_eq!(result["status"], "cancelled");
            // kill(pid, 0) alone can't observe the death: the orphaned
            // grandchild lingers as an unreaped zombie under init/launchd,
            // so a Z state also proves the group kill landed.
            fn grandchild_is_dead(pid: i32) -> bool {
                let esrch = unsafe { libc::kill(pid, 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                if esrch {
                    return true;
                }
                let stat = std::process::Command::new("ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .output()
                    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                    .unwrap_or_default();
                stat.is_empty() || stat.starts_with('Z')
            }
            let deadline = time::Instant::now() + Duration::from_secs(5);
            while !grandchild_is_dead(grandchild_pid) {
                assert!(
                    time::Instant::now() < deadline,
                    "grandchild must die with the process group"
                );
                time::sleep(Duration::from_millis(10)).await;
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn run_process_job_clears_pgid_after_normal_completion() {
        run_async(async {
            let state = Arc::new(Mutex::new(JobState {
                status: JobStatus::Running,
                pgid: None,
            }));
            let output_buf = Arc::new(Mutex::new(RingBuf::default()));
            let snapshot = test_snapshot("echo", &["done"], 0);

            let result = run_process_job(snapshot, Arc::clone(&state), output_buf)
                .await
                .unwrap();

            assert_eq!(result.exit_code, Some(0));
            assert!(
                state.lock().pgid.is_none(),
                "pid-reuse guard must clear pgid"
            );
        });
    }

    #[test]
    fn handle_start_rejects_shell_and_path_shaped_names_without_spawn() {
        let mut ctx = plain_ctx();

        for tool in ["bash", "./script.sh", "/usr/bin/env", "ls"] {
            let result = run_async(handle_start(
                &mut ctx,
                &json!({"tool": tool, "arguments": {}}),
            ))
            .unwrap();

            assert_eq!(result["status"], "error");
            assert!(
                result["message"]
                    .as_str()
                    .unwrap()
                    .contains("not enabled in this context")
            );
        }
        assert!(ctx.supervisor.is_none(), "no job may be spawned");
    }

    #[cfg(unix)]
    #[test]
    fn handle_start_rejects_context_filtered_tool_and_accepts_in_filter() {
        run_async(async {
            let mut ctx = plain_ctx();
            ctx.declared_function_names.insert("echo".into());

            let rejected = handle_start(&mut ctx, &json!({"tool": "git_command", "arguments": {}}))
                .await
                .unwrap();

            assert_eq!(rejected["status"], "error");
            assert!(
                rejected["message"]
                    .as_str()
                    .unwrap()
                    .contains("not enabled in this context")
            );
            assert!(ctx.supervisor.is_none(), "no job may be spawned");

            let started = handle_start(&mut ctx, &json!({"tool": "echo", "arguments": {}}))
                .await
                .unwrap();

            assert_eq!(started["status"], "ok");
            let job_id = started["job_id"].as_str().unwrap().to_string();
            let collected = handle_collect(&ctx, &json!({"id": job_id})).await.unwrap();
            assert_eq!(collected["status"], "completed");
        });
    }

    #[test]
    fn handle_start_rejects_undeclared_mcp_invoke_without_spawn() {
        let mut ctx = plain_ctx();

        let result = run_async(handle_start(
            &mut ctx,
            &json!({"tool": "mcp_invoke_someserver", "arguments": {"tool": "search"}}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("not enabled in this context")
        );
        assert!(ctx.supervisor.is_none(), "no job may be spawned");
    }

    #[test]
    fn handle_start_rejects_declared_but_non_whitelisted_tools_without_spawn() {
        let mut ctx = plain_ctx();
        for tool in ["memory__write", "fs_read", "agent__spawn", "user__select"] {
            ctx.declared_function_names.insert(tool.into());
        }

        for (tool, category) in [
            ("memory__write", "mutates agent/session state"),
            ("fs_read", "is fast"),
            ("agent__spawn", "already asynchronous"),
            ("user__select", "interactive"),
        ] {
            let result = run_async(handle_start(
                &mut ctx,
                &json!({"tool": tool, "arguments": {}}),
            ))
            .unwrap();

            assert_eq!(result["status"], "error");
            assert!(result["message"].as_str().unwrap().contains(category));
        }
        assert!(ctx.supervisor.is_none(), "no job may be spawned");
    }

    #[test]
    fn handle_start_rejects_mapping_tools_alias() {
        let app_state = app_state_with_config(|config| {
            config
                .mapping_tools
                .insert("shell".into(), "execute_command".into());
        });
        let mut ctx = RequestContext::new(app_state, WorkingMode::Cmd);
        ctx.declared_function_names.insert("execute_command".into());

        let result = run_async(handle_start(
            &mut ctx,
            &json!({"tool": "shell", "arguments": {}}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("not enabled in this context")
        );
        assert!(ctx.supervisor.is_none(), "no job may be spawned");
    }

    #[test]
    fn child_context_cannot_reach_parent_job_ids() {
        run_async(async {
            let parent = ctx_with_job_supervisor(4);
            parent
                .supervisor
                .as_ref()
                .unwrap()
                .write()
                .register(make_running_job("job_p1"))
                .unwrap();
            let child = RequestContext::new_for_child(
                default_app_state(),
                &parent,
                1,
                Arc::new(Inbox::new()),
                "c1".into(),
            );
            assert!(child.supervisor.is_none());

            let checked = handle_check(&child, &json!({"id": "job_p1"})).unwrap();
            let collected = handle_collect(&child, &json!({"id": "job_p1"}))
                .await
                .unwrap();
            let cancelled = handle_cancel(&child, &json!({"id": "job_p1"}))
                .await
                .unwrap();

            for result in [checked, collected, cancelled] {
                assert_eq!(result["status"], "error");
                assert!(
                    result["message"]
                        .as_str()
                        .unwrap()
                        .contains("No job 'job_p1' is registered")
                );
            }
            assert!(parent.supervisor.as_ref().unwrap().read().has_job("job_p1"));
        });
    }

    #[test]
    fn handle_start_ignores_mid_batch_tool_scope_additions() {
        let mut ctx = plain_ctx();
        ctx.declared_function_names.insert("job__start".into());
        ctx.tool_scope
            .functions
            .declarations
            .push(FunctionDeclaration {
                name: "late_external_tool".into(),
                description: String::new(),
                parameters: JsonSchema::default(),
                agent: false,
            });

        let result = run_async(handle_start(
            &mut ctx,
            &json!({"tool": "late_external_tool", "arguments": {}}),
        ))
        .unwrap();

        assert_eq!(result["status"], "error");
        assert!(
            result["message"]
                .as_str()
                .unwrap()
                .contains("not enabled in this context")
        );
        assert!(ctx.supervisor.is_none(), "no job may be spawned");
    }
}
