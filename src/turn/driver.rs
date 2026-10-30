use super::{emit_assistant_text, emit_notice};
use crate::config::{AppConfig, Input, RequestContext, Session, TURN_INTERRUPTED_TEXT};
use crate::events::NoticeKind;
use crate::function::agents::{GuardrailAction, check_pending_tasks_guardrail, pending_tasks};
use crate::graph;
use crate::graph::dispatch::{GraphTurn, run_graph_turn};
use crate::graph::llm::CompletionRunner;
use crate::graph::route::Branch;
use crate::hooks::{self, HookEvent};
use crate::repl::DEFAULT_CONTINUATION_PROMPT;
use crate::supervisor::TaskKind;
use crate::utils::{AbortSignal, TaskCancelGuard, wait_user_interrupt};
use anyhow::{Error, Result};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Which turn loop `TurnDriver::run_turn` runs: the interactive REPL's
/// (compression, auto-continue, interrupt bookkeeping, a divider at the
/// end) or the headless directive's (one request, its tool rounds and the
/// pending-tasks guardrail, nothing else).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnMode {
    Repl,
    /// Embeddings are resolved by the caller and a forced branch is
    /// REPL-only, so `run_turn` ignores both here.
    Headless,
}

/// How a turn ended. A headless turn is never reported interrupted: a
/// Ctrl-C there surfaces as an error from the completion instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Interrupted,
}

/// Drives one user-visible turn to completion: the completion `runner`
/// makes every model call and everything the turn shows goes to `ctx.ui`.
pub struct TurnDriver<'a> {
    pub runner: &'a mut CompletionRunner<'a>,
    pub mode: TurnMode,
}

/// Marks an error raised before any request went out, so the turn loop
/// returns it bare instead of treating it as a failed request.
#[derive(Debug)]
pub(crate) struct PreflightError(pub(crate) anyhow::Error);

impl std::fmt::Display for PreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for PreflightError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl TurnDriver<'_> {
    pub async fn run_turn(
        &mut self,
        ctx: &mut RequestContext,
        abort_signal: AbortSignal,
        input: Input,
        with_embeddings: bool,
        forced: Option<Branch>,
    ) -> Result<TurnOutcome> {
        // Boxed so a caller's `Send` proof stops here instead of descending
        // through the whole turn loop, which is already at the solver's limit.
        let turn: Pin<Box<dyn Future<Output = Result<TurnOutcome>> + Send + '_>> = match self.mode {
            TurnMode::Repl => {
                Box::pin(self.run_repl(ctx, abort_signal, input, with_embeddings, forced))
            }
            TurnMode::Headless => Box::pin(self.run_headless(ctx, abort_signal, input)),
        };
        turn.await
    }

    async fn run_repl(
        &mut self,
        ctx: &mut RequestContext,
        abort_signal: AbortSignal,
        input: Input,
        with_embeddings: bool,
        forced: Option<Branch>,
    ) -> Result<TurnOutcome> {
        let mut input = input;
        let mut with_embeddings = with_embeddings;
        // One compress-and-retry per fresh input; tool-loop iterations share it.
        let mut retried_after_compress = false;
        // The graph gate runs once per user turn: tool rounds, the compress
        // retry, the guardrail inject and auto-continue all re-enter the loop
        // with `routed` set, so a chat turn's tool rounds are never
        // re-classified and the graph never runs mid tool-loop.
        let mut routed = false;
        ctx.last_graph_branch = None;
        let ui = Arc::clone(&ctx.ui);
        let _sigint_bridge = spawn_turn_interrupt_bridge(&abort_signal);
        loop {
            if input.is_empty() {
                return Ok(repl_outcome(&abort_signal));
            }
            ctx.session_abort = Some(abort_signal.clone());
            if with_embeddings {
                input.use_embeddings(abort_signal.clone()).await?;
                with_embeddings = false;
            }
            while ctx.is_compressing_session() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            let app = Arc::clone(&ctx.app.config);

            if maybe_auto_compress(ctx, app.as_ref()).await.succeeded {
                input.refresh_session(ctx);
            }

            if !routed
                && input.tool_calls().is_none()
                && graph::active_agent_graph_name(ctx).is_some()
            {
                routed = true;
                ctx.before_chat_completion(&input)?;
                match run_graph_turn(ctx, &input, abort_signal.clone(), forced, self.runner).await?
                {
                    GraphTurn::Run { output, last_run } => {
                        emit_assistant_text(ui.as_ref(), app.as_ref(), &output)?;
                        input.clear_continue_output();
                        // `None` (a run the graph does not record: no session
                        // block and nothing to carry) clears any stale record
                        // from an earlier run.
                        ctx.record_graph_run(
                            app.as_ref(),
                            &input,
                            &output,
                            last_run.map(|run| *run),
                        )?;
                        settle_turn(ctx, app.as_ref()).await;
                        ui.turn_divider();
                        return Ok(repl_outcome(&abort_signal));
                    }
                    GraphTurn::Chat(attached) => {
                        input = *attached;
                    }
                }
            }

            let (output, tool_results) = {
                let result = (self.runner)(&input, ctx, abort_signal.clone()).await;
                match result {
                    Ok(v) => v,
                    Err(err) => {
                        let err = match err.downcast::<PreflightError>() {
                            Ok(preflight) => return Err(preflight.0),
                            Err(err) => err,
                        };
                        if compress_for_error_retry(
                            ctx,
                            app.as_ref(),
                            retried_after_compress,
                            &abort_signal,
                            &err,
                        )
                        .await
                        {
                            input.refresh_session(ctx);
                            retried_after_compress = true;
                            continue;
                        }
                        ctx.on_chat_completion_error(app.as_ref(), &input);
                        return Err(err);
                    }
                }
            };
            ctx.after_chat_completion(app.as_ref(), &input, &output, &tool_results)?;
            let interrupted = abort_signal.aborted_ctrlc();
            if !tool_results.is_empty() {
                if interrupted {
                    debug!(
                        "turn interrupted: {} tool results committed, unfinished tasks {:?}",
                        tool_results.len(),
                        pending_tasks(ctx)
                            .into_iter()
                            .filter(|task| !task.finished)
                            .map(|task| task.id)
                            .collect::<Vec<_>>()
                    );
                    let merged = input.merge_tool_results(output.clone(), tool_results);
                    ctx.after_chat_completion(app.as_ref(), &merged, TURN_INTERRUPTED_TEXT, &[])?;
                    // The marker is bookkeeping, not model text: `.continue` and
                    // `%%` must not replay it. `continuous` stays set so
                    // `.recover` and `.regenerate` work as after any tool turn.
                    if let Some(last) = ctx.last_message.as_mut() {
                        last.output.clear();
                    }
                } else {
                    input = input.merge_tool_results(output, tool_results);
                    let (next, checkpoint) = ctx.maybe_checkpoint_tool_loop(input)?;
                    input = next;
                    if let Some(checkpoint) = checkpoint {
                        emit_notice(
                            ui.as_ref(),
                            NoticeKind::Checkpoint,
                            format!(
                                "\n📎 {}",
                                dim_notice_color(app.as_ref()).italic().paint(format!(
                                    "Checkpointed {} tool results (~{} tokens) to the session; the tool loop continues.",
                                    checkpoint.committed_results, checkpoint.committed_tokens
                                ))
                            ),
                        );
                        retried_after_compress = false;
                    }
                    continue;
                }
            }

            // The guardrail must not run on an interrupted turn: its reminders
            // would be saved as phantom exchanges and its force-terminate would
            // cancel the background tasks the user chose to leave running.
            if interrupted {
                emit_interrupted_turn_notice(ctx);
            } else {
                match check_pending_tasks_guardrail(ctx) {
                    GuardrailAction::Inject(prompt) => {
                        input = Input::from_str(ctx, &prompt, None)?;
                        retried_after_compress = false;
                        continue;
                    }
                    GuardrailAction::ForceTerminate(ids) => {
                        warn!(
                            "Pending-agent guardrail force-cancelled {} agent(s) after max reminders: {:?}",
                            ids.len(),
                            ids
                        );
                    }
                    GuardrailAction::NoAction => {}
                }
            }
            let do_continue = should_continue(ctx) && !abort_signal.aborted_ctrlc();

            if do_continue {
                let full_prompt = {
                    let config = ctx.auto_continue_config();
                    let todo_state = ctx.todo_list.render_for_model();
                    let remaining = ctx.todo_list.incomplete_count();
                    ctx.set_last_continuation_response(output.clone());
                    ctx.increment_auto_continue_count();
                    let count = ctx.auto_continue_count;
                    let max = config.max_continues;

                    let prompt = config
                        .continuation_prompt
                        .as_deref()
                        .unwrap_or(DEFAULT_CONTINUATION_PROMPT);

                    emit_notice(
                        ui.as_ref(),
                        NoticeKind::AutoContinue,
                        format!(
                            "\n📋 {}",
                            dim_notice_color(app.as_ref()).italic().paint(format!(
                                "Auto-continuing ({count}/{max}): {remaining} incomplete todo(s) remain"
                            ))
                        ),
                    );

                    format!("{prompt}\n\n{todo_state}")
                };
                input = Input::from_str(ctx, &full_prompt, None)?;
                retried_after_compress = false;
                continue;
            }

            settle_turn(ctx, app.as_ref()).await;

            let agent_can_continue_after_compress =
                should_continue(ctx) && !abort_signal.aborted_ctrlc();

            // Auto-continue keys off the compression CONDITION, not the outcome:
            // a transiently failed compression retries at the next turn end, and
            // gating on success would stall an unattended agent at the pause
            // banner with incomplete todos.
            let compression = maybe_auto_compress(ctx, app.as_ref()).await;
            if compression.fired && agent_can_continue_after_compress {
                let full_prompt = {
                    let config = ctx.auto_continue_config();
                    let todo_state = ctx.todo_list.render_for_model();
                    let remaining = ctx.todo_list.incomplete_count();
                    ctx.increment_auto_continue_count();
                    let count = ctx.auto_continue_count;
                    let max = config.max_continues;

                    let prompt = config
                        .continuation_prompt
                        .as_deref()
                        .unwrap_or(DEFAULT_CONTINUATION_PROMPT);

                    emit_notice(
                        ui.as_ref(),
                        NoticeKind::AutoContinue,
                        format!(
                            "\n📋 {}",
                            dim_notice_color(app.as_ref()).italic().paint(format!(
                                "Auto-continuing after compression ({count}/{max}): {remaining} incomplete todo(s) remain"
                            ))
                        ),
                    );

                    format!("{prompt}\n\n{todo_state}")
                };
                input = Input::from_str(ctx, &full_prompt, None)?;
                retried_after_compress = false;
                continue;
            }

            ui.turn_divider();
            return Ok(repl_outcome(&abort_signal));
        }
    }

    async fn run_headless(
        &mut self,
        ctx: &mut RequestContext,
        abort_signal: AbortSignal,
        input: Input,
    ) -> Result<TurnOutcome> {
        let app = Arc::clone(&ctx.app.config);
        let ui = Arc::clone(&ctx.ui);

        if graph::active_agent_graph_name(ctx).is_some() {
            ctx.before_chat_completion(&input)?;
            let output = graph::run_active_agent_graph(ctx, &input.text(), abort_signal.clone())
                .await?
                .result
                .output;
            emit_assistant_text(ui.as_ref(), app.as_ref(), &output)?;
            ctx.after_chat_completion(app.as_ref(), &input, &output, &[])?;
            return Ok(TurnOutcome::Completed);
        }

        let mut input = input;
        loop {
            let (output, tool_results) = (self.runner)(&input, ctx, abort_signal.clone()).await?;
            ctx.after_chat_completion(app.as_ref(), &input, &output, &tool_results)?;

            if !tool_results.is_empty() {
                input = input.merge_tool_results(output, tool_results);
                let (next, _) = ctx.maybe_checkpoint_tool_loop(input)?;
                input = next;
                continue;
            }

            match check_pending_tasks_guardrail(ctx) {
                GuardrailAction::Inject(prompt) => {
                    input = Input::from_str(ctx, &prompt, None)?;
                    continue;
                }
                GuardrailAction::ForceTerminate(ids) => {
                    warn!(
                        "Pending-agent guardrail force-cancelled {} agent(s) after max reminders: {:?}",
                        ids.len(),
                        ids
                    );
                }
                GuardrailAction::NoAction => {}
            }

            break;
        }

        Ok(TurnOutcome::Completed)
    }
}

fn repl_outcome(abort_signal: &AbortSignal) -> TurnOutcome {
    if abort_signal.aborted_ctrlc() {
        TurnOutcome::Interrupted
    } else {
        TurnOutcome::Completed
    }
}

/// The end-of-turn bookkeeping every completed turn shares, graph run or
/// chat: the continuation counter resets, the pause banner is emitted and a
/// temp session gets its autoname.
async fn settle_turn(ctx: &mut RequestContext, app: &AppConfig) {
    reset_continuation(ctx);
    emit_pause_banner(ctx);
    autoname_with_notice(ctx, app).await;
}

/// Names a temp session that is due for it, announcing the request on
/// `ctx.ui` first. A failed autoname is logged and otherwise swallowed; the
/// session keeps its temp name.
pub(crate) async fn autoname_with_notice(ctx: &mut RequestContext, app: &AppConfig) {
    if !ctx.maybe_autoname_session() {
        return;
    }
    emit_notice(
        ctx.ui.as_ref(),
        NoticeKind::Autoname,
        format!(
            "\n📢 {}",
            dim_notice_color(app)
                .italic()
                .paint("Autonaming the session."),
        ),
    );
    if let Err(err) = ctx.autoname_session(app).await {
        warn!("Failed to autoname the session: {err:#}");
    }
    if let Some(session) = ctx.session.as_mut() {
        session.set_autonaming(false);
    }
}

pub(crate) struct AutoCompressOutcome {
    pub(crate) fired: bool,
    pub(crate) succeeded: bool,
}

pub(crate) async fn maybe_auto_compress(
    ctx: &mut RequestContext,
    app: &AppConfig,
) -> AutoCompressOutcome {
    let keep = ctx.compression_keep_policy(None);
    let should_compress = ctx.session.as_ref().is_some_and(|session| {
        session.needs_compression(app.compression_threshold, keep)
            || session.safety_valve_triggered(
                app.compression_safety_valve,
                keep,
                ctx.last_prompt_token_usage,
            )
    });
    if !should_compress {
        return AutoCompressOutcome {
            fired: false,
            succeeded: false,
        };
    }
    AutoCompressOutcome {
        fired: true,
        succeeded: run_auto_compress(ctx, app).await,
    }
}

pub(crate) async fn run_auto_compress(ctx: &mut RequestContext, app: &AppConfig) -> bool {
    if let Some(session) = ctx.session.as_mut() {
        session.set_compressing(true);
    }

    emit_notice(
        ctx.ui.as_ref(),
        NoticeKind::Compress,
        format!(
            "\n📢 {}",
            dim_notice_color(app)
                .italic()
                .paint("Compressing the session."),
        ),
    );

    let compressed = auto_compress_session(ctx, app).await;
    if let Some(session) = ctx.session.as_mut() {
        session.set_compressing(false);
    }
    compressed
}

/// A context-overflow rejection is proof the prompt is too big even when the
/// model declares no window for the valve to measure against, so it earns
/// the same compress-and-retry as a tripped valve, provided there is a
/// session with something to fold.
pub(crate) async fn compress_for_error_retry(
    ctx: &mut RequestContext,
    app: &AppConfig,
    already_retried: bool,
    abort_signal: &AbortSignal,
    err: &Error,
) -> bool {
    if already_retried || abort_signal.aborted() {
        return false;
    }
    let keep = ctx.compression_keep_policy(None);
    let valve_triggered = ctx.session.as_ref().is_some_and(|session| {
        session.safety_valve_triggered(
            app.compression_safety_valve,
            keep,
            ctx.last_prompt_token_usage,
        )
    });
    let compressible = ctx.session.as_ref().is_some_and(Session::has_user_messages);
    (valve_triggered || (compressible && graph::is_context_overflow_error(err)))
        && run_auto_compress(ctx, app).await
}

pub(crate) async fn auto_compress_session(ctx: &mut RequestContext, app: &AppConfig) -> bool {
    match ctx.compress_session().await {
        Ok(()) => {
            hooks::fire(
                HookEvent::SessionCompressed,
                ctx,
                &hooks::role_extras(ctx),
                None,
            );
            true
        }
        Err(err) => {
            warn!("Failed to compress the session: {err:#}");
            emit_notice(
                ctx.ui.as_ref(),
                NoticeKind::Compress,
                format!("\n{}", compression_failure_notice(app, &err)),
            );
            false
        }
    }
}

pub(crate) fn compression_failure_notice(app: &AppConfig, err: &Error) -> String {
    dim_notice_color(app)
        .italic()
        .paint(format!("⚠️ Compression failed: {err:#}"))
        .to_string()
}

pub(crate) fn dim_notice_color(app: &AppConfig) -> nu_ansi_term::Color {
    if app.light_theme() {
        nu_ansi_term::Color::LightGray
    } else {
        nu_ansi_term::Color::DarkGray
    }
}

fn should_continue(ctx: &RequestContext) -> bool {
    let config = ctx.auto_continue_config();
    ctx.app.config.function_calling_support
        && config.enabled
        && ctx.auto_continue_count < config.max_continues
        && ctx.todo_list.has_incomplete()
        && ctx.auto_continue_paused.is_none()
}

pub(crate) fn reset_continuation(ctx: &mut RequestContext) {
    ctx.reset_continuation_count();
}

pub(crate) fn spawn_turn_interrupt_bridge(abort_signal: &AbortSignal) -> TaskCancelGuard {
    let signal = abort_signal.clone();
    TaskCancelGuard::spawn_one(async move { wait_user_interrupt(Some(&signal)).await })
}

pub(crate) fn pause_banner_text(ctx: &RequestContext) -> Option<String> {
    let reason = ctx.auto_continue_paused.as_deref()?;
    if !ctx.todo_list.has_incomplete() {
        return None;
    }
    let remaining = ctx.todo_list.incomplete_count();
    Some(format!(
        "Auto-continue paused, awaiting user ({remaining} incomplete todo(s) preserved): {reason}"
    ))
}

pub(crate) fn emit_pause_banner(ctx: &RequestContext) {
    if let Some(text) = pause_banner_text(ctx) {
        emit_notice(
            ctx.ui.as_ref(),
            NoticeKind::AutoContinue,
            format!(
                "\n⏸ {}",
                dim_notice_color(&ctx.app.config).italic().paint(text)
            ),
        );
    }
}

pub(crate) fn interrupted_turn_notice(ctx: &RequestContext) -> Option<String> {
    let running: Vec<String> = pending_tasks(ctx)
        .into_iter()
        .filter(|task| !task.finished)
        .map(|task| {
            let kind = match task.kind {
                TaskKind::Agent => "agent",
                TaskKind::Job => "job",
            };
            format!("{} ({kind})", task.id)
        })
        .collect();
    if running.is_empty() {
        // Grandchild-only activity has no top-level id to list, but it is
        // what the second Ctrl-C would cancel, so the notice must still
        // agree with `latch_prompt_interrupt`.
        let has_active_tasks = ctx
            .supervisor
            .as_ref()
            .is_some_and(|supervisor| supervisor.read().has_active_tasks());
        if has_active_tasks {
            return Some("Turn interrupted; background work is still running. Press Ctrl-C again to cancel it, or type to continue.".into());
        }
        return None;
    }
    Some(format!(
        "Turn interrupted; {} background task(s) still running: {}. Press Ctrl-C again to cancel them, or type to continue.",
        running.len(),
        running.join(", ")
    ))
}

fn emit_interrupted_turn_notice(ctx: &RequestContext) {
    if let Some(text) = interrupted_turn_notice(ctx) {
        emit_notice(
            ctx.ui.as_ref(),
            NoticeKind::Interrupted,
            format!(
                "\n⏸ {}",
                dim_notice_color(&ctx.app.config).italic().paint(text)
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::test_fakes::FixedReplyClient;
    use crate::client::{call_chat_completions, call_chat_completions_streaming};
    use crate::config::{AppState, WorkingMode};
    use crate::events::{Event, RecordingSink};
    use crate::graph::llm::test_support::{boxed_completion, completion_runner};
    use crate::utils::create_abort_signal;
    use serial_test::serial;

    fn silent_ctx() -> RequestContext {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.ui = Arc::new(crate::turn::SilentSink);
        ctx
    }

    async fn drive(
        ctx: &mut RequestContext,
        mode: TurnMode,
        text: &str,
        runner: &mut CompletionRunner<'_>,
    ) -> TurnOutcome {
        let input = Input::from_str(ctx, text, None).unwrap();
        let mut driver = TurnDriver { runner, mode };
        driver
            .run_turn(ctx, create_abort_signal(), input, false, None)
            .await
            .unwrap()
    }

    #[test]
    #[serial]
    fn pause_banner_is_published_as_an_auto_continue_notice() {
        let mut ctx = silent_ctx();
        ctx.add_todo("pending work");
        ctx.pause_auto_continue("probe-pause-x7x");
        let sink = RecordingSink::install();

        emit_pause_banner(&ctx);

        let recorded: Vec<Event> = sink
            .drain()
            .into_iter()
            .filter(|event| {
                matches!(event, Event::Notice { text, .. } if text.contains("probe-pause-x7x"))
            })
            .collect();
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert!(matches!(
            &recorded[0],
            Event::Notice {
                kind: NoticeKind::AutoContinue,
                text
            } if text.starts_with("\n⏸ ")
        ));
    }

    #[tokio::test]
    #[serial]
    async fn repl_turn_publishes_the_pause_banner_when_auto_continue_is_paused() {
        let mut ctx = silent_ctx();
        ctx.add_todo("pending work");
        ctx.pause_auto_continue("probe-driver-pause-x7x");
        let mut runner = completion_runner(|_input, _ctx, _abort| {
            boxed_completion(async { Ok(("done".to_string(), vec![])) })
        });
        let sink = RecordingSink::install();

        let outcome = drive(&mut ctx, TurnMode::Repl, "hi", &mut runner).await;

        assert_eq!(outcome, TurnOutcome::Completed);
        let recorded: Vec<Event> = sink
            .drain()
            .into_iter()
            .filter(|event| {
                matches!(event, Event::Notice { text, .. } if text.contains("probe-driver-pause-x7x"))
            })
            .collect();
        assert_eq!(recorded.len(), 1, "{recorded:?}");
        assert!(matches!(
            &recorded[0],
            Event::Notice {
                kind: NoticeKind::AutoContinue,
                ..
            }
        ));
    }

    #[tokio::test]
    #[serial]
    async fn headless_turn_publishes_the_printed_reply_as_assistant_text() {
        let mut ctx = silent_ctx();
        let reply = "probe-driver-plain-reply-x7x";
        let client = Arc::new(FixedReplyClient::new(reply));
        let mut runner = completion_runner(move |input, ctx, abort| {
            let client = Arc::clone(&client);
            boxed_completion(async move {
                ctx.before_chat_completion(input)?;
                call_chat_completions(input, true, false, client.as_ref(), ctx, abort).await
            })
        });
        let sink = RecordingSink::install();

        let outcome = drive(&mut ctx, TurnMode::Headless, "hi", &mut runner).await;

        assert_eq!(outcome, TurnOutcome::Completed);
        let recorded: Vec<Event> = sink
            .drain()
            .into_iter()
            .filter(|event| matches!(event, Event::AssistantText { text, .. } if text == reply))
            .collect();
        assert_eq!(
            recorded,
            vec![Event::AssistantText {
                text: reply.to_string(),
                markdown: true,
            }]
        );
    }

    #[tokio::test]
    #[serial]
    async fn repl_turn_publishes_each_streamed_delta() {
        let mut ctx = silent_ctx();
        let reply = "probe-driver-stream-delta-x7x";
        let client = Arc::new(FixedReplyClient::new(reply));
        let mut runner = completion_runner(move |input, ctx, abort| {
            let client = Arc::clone(&client);
            boxed_completion(async move {
                ctx.before_chat_completion(input)?;
                call_chat_completions_streaming(input, client.as_ref(), ctx, abort).await
            })
        });
        let sink = RecordingSink::install();

        let outcome = drive(&mut ctx, TurnMode::Repl, "hi", &mut runner).await;

        assert_eq!(outcome, TurnOutcome::Completed);
        let deltas: Vec<Event> = sink
            .drain()
            .into_iter()
            .filter(|event| matches!(event, Event::TextDelta(text) if text == reply))
            .collect();
        assert_eq!(deltas, vec![Event::TextDelta(reply.to_string())]);
    }
}
