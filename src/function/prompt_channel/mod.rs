//! Lets a running tool ask the human through Coyote's prompter. The tool
//! writes one request line to its stdout; the stdout pump spots it, asks,
//! and writes the answer line to a pipe the tool reaches through
//! `COYOTE_PROMPT_REPLY`: an inherited fd on unix, a named pipe the tool
//! opens by path on Windows. Lines that are not requests flow through as
//! ordinary output.

mod router;
mod server;
#[cfg(all(test, unix))]
mod tests;

pub(crate) use router::{Attachment, Classified, LineAssembler, Routed, Router};
pub(crate) use server::Server;

use crate::config::escape_display_text;
use crate::events::{self, Origin};
use crate::prompt::{PromptAction, PromptAnswer, PromptMemory, PromptRequest, Prompter};
use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const REPLY_ENV: &str = "COYOTE_PROMPT_REPLY";
/// Exported by the bash helper once it has saved its stdout on a spare fd.
/// A tool spawned by Coyote must never inherit it: a nested run's helper
/// would skip its own save and write requests to the outer pump.
pub(crate) const OUT_FD_ENV: &str = "_COYOTE_PROMPT_OUT";
pub(crate) const MARKER: &[u8] = b"{\"coyote_prompt\"";

/// A tail without a newline is held back only this long; past it the tool
/// is streaming something that merely contains the marker, not a request.
const CARRY_CAP: usize = 64 * 1024;

/// Answers tool-originated prompts from a pump thread that has no request
/// context, honouring "Yes for this session" the same way the context does.
#[derive(Clone, Debug)]
pub(crate) struct ToolPrompter {
    prompter: Arc<dyn Prompter>,
    memory: Arc<PromptMemory>,
    origin: Origin,
}

impl ToolPrompter {
    pub(crate) fn new(
        prompter: Arc<dyn Prompter>,
        memory: Arc<PromptMemory>,
        origin: Origin,
    ) -> Self {
        Self {
            prompter,
            memory,
            origin,
        }
    }

    /// The request on a protocol line, or `None` for a line that carries the
    /// marker but no well-formed request; the caller echoes that line.
    pub(crate) fn parse_line(
        &self,
        line: &[u8],
        tool: &str,
        call_id: Option<String>,
        job_id: Option<String>,
    ) -> Option<ParsedRequest> {
        match parse_request(line, tool, call_id, job_id, self.origin.clone()) {
            Ok(parsed) => Some(parsed),
            Err(err) => {
                warn!("Malformed prompt line from tool '{tool}': {err}");
                None
            }
        }
    }

    pub(crate) fn answer(&self, req: PromptRequest) -> Result<PromptAnswer> {
        if self.memory.auto_answer(&req) == Some(true) {
            let answer = PromptAnswer::Confirmed(true);
            events::bus().publish(req.requested_event());
            events::bus().publish(req.answered_event(&answer, true));
            return Ok(answer);
        }
        // A run with no human cannot answer a tool, so the tool fails closed;
        // the headless prompter's `needs_human` JSON is only useful to
        // built-in tools, which hand it back to the model. The cancel is
        // substituted here, before any widget is built, because a tool
        // must see a clean cancel on its protocol and never a crash; the
        // prompter is not consulted (`headless_run_answers_tool_prompts_with_cancel`).
        if !crate::prompt::human_present() {
            let answer = PromptAnswer::Cancelled;
            events::bus().publish(req.requested_event());
            events::bus().publish(req.answered_event(&answer, false));
            return Ok(answer);
        }
        crate::prompt::ask(self.prompter.as_ref(), req)
    }
}

/// Asks and writes the reply line. A prompter failure, an answer of the
/// wrong shape or a checkbox choice outside the offered options all answer
/// `null`, so the tool fails closed instead of hanging or acting on a bad
/// value. Chosen options go back as the raw text the tool sent, since that
/// is what the tool matches its answer against; a select answer that is
/// not an offered option is free text the user typed and goes back as is,
/// unless it is blank, which answers `null` like a cancel.
pub(crate) fn handle_prompt_line(prompter: &ToolPrompter, reply: &Server, parsed: ParsedRequest) {
    let ParsedRequest { req, raw_options } = parsed;
    let id = req.id.clone();
    let tool = req.tool.clone().unwrap_or_default();
    let action = req.action;
    let options = req.options.clone();
    let raw = |choice: &String| {
        options
            .iter()
            .position(|option| option == choice)
            .map(|index| raw_options[index].clone())
    };
    let answer = prompter.answer(req);
    let value = match &answer {
        Err(err) => {
            warn!("Prompt from tool '{tool}' failed, answering null: {err}");
            Value::Null
        }
        Ok(answer) if !fits(action, answer) => {
            warn!(
                "Tool '{tool}' prompt '{id}': answer does not fit a {} prompt; answering null",
                action.name()
            );
            Value::Null
        }
        Ok(PromptAnswer::Selected(choice)) if action == PromptAction::Select => match raw(choice) {
            Some(raw) => json!(raw),
            None if choice.trim().is_empty() => Value::Null,
            None => json!(choice),
        },
        Ok(PromptAnswer::Checked(choices))
            if action == PromptAction::Checkbox && !options.is_empty() =>
        {
            match choices.iter().map(raw).collect::<Option<Vec<_>>>() {
                Some(raw) => json!(raw),
                None => {
                    warn!(
                        "Tool '{tool}' prompt '{id}': answer {choices:?} includes a choice outside the offered options; answering null"
                    );
                    Value::Null
                }
            }
        }
        Ok(_) => answer_value(&answer),
    };
    reply.reply(&id, &value);
}

/// Replies `null` to a marker line that names a request id but does not
/// parse as a request, so the tool fails closed instead of blocking on the
/// reply pipe. Lines without a readable id get nothing: the tool never
/// asked under a name it could wait for. The parse failure itself is
/// already logged by `parse_line`.
pub(crate) fn reject_line(reply: &Server, tool: &str, line: &[u8]) {
    if let Some(id) = wire_id(line) {
        debug!("Tool '{tool}' prompt '{id}' answered null");
        reply.reply(&id, &Value::Null);
    }
}

pub(crate) fn wire_id(line: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(line).ok()?;
    value["coyote_prompt"]["id"].as_str().map(str::to_string)
}

fn fits(action: PromptAction, answer: &PromptAnswer) -> bool {
    matches!(
        (action, answer),
        (PromptAction::Confirm, PromptAnswer::Confirmed(_))
            | (PromptAction::Select, PromptAnswer::Selected(_))
            | (PromptAction::Checkbox, PromptAnswer::Checked(_))
            | (
                PromptAction::Input | PromptAction::Password,
                PromptAnswer::Text(_)
            )
            | (_, PromptAnswer::Cancelled | PromptAnswer::Interrupted)
    )
}

pub(crate) fn answer_value(answer: &Result<PromptAnswer>) -> Value {
    match answer {
        Ok(PromptAnswer::Confirmed(yes)) => json!(yes),
        Ok(PromptAnswer::Selected(choice)) | Ok(PromptAnswer::Text(choice)) => json!(choice),
        Ok(PromptAnswer::Checked(choices)) => json!(choices),
        Ok(PromptAnswer::Cancelled | PromptAnswer::Interrupted) | Err(_) => Value::Null,
    }
}

#[derive(Deserialize)]
struct WireLine {
    coyote_prompt: WireRequest,
}

#[derive(Deserialize)]
struct WireRequest {
    id: String,
    action: String,
    question: String,
    #[serde(default)]
    options: Option<Vec<String>>,
    #[serde(default)]
    default: Option<Value>,
}

/// A request as the prompter sees it plus the option text as the tool sent
/// it. Question and options are escaped for display before the prompter
/// paints them, but a chosen option must go back verbatim or the tool cannot
/// match it.
pub(crate) struct ParsedRequest {
    pub(crate) req: PromptRequest,
    raw_options: Vec<String>,
}

pub(crate) fn parse_request(
    line: &[u8],
    tool: &str,
    call_id: Option<String>,
    job_id: Option<String>,
    origin: Origin,
) -> Result<ParsedRequest> {
    let wire: WireLine = serde_json::from_slice(line)?;
    let wire = wire.coyote_prompt;
    let action = [
        PromptAction::Confirm,
        PromptAction::Select,
        PromptAction::Checkbox,
        PromptAction::Input,
        PromptAction::Password,
    ]
    .into_iter()
    .find(|action| action.name() == wire.action)
    .ok_or_else(|| anyhow!("unknown prompt action '{}'", wire.action))?;
    let raw_options = wire.options.unwrap_or_default();
    Ok(ParsedRequest {
        req: PromptRequest {
            id: wire.id,
            action,
            question: escape_display_text(&wire.question),
            options: raw_options
                .iter()
                // An option is one row, so its newlines are spelled out too.
                .map(|option| escape_display_text(option).replace('\n', "\\x0a"))
                .collect(),
            default: wire.default,
            tool: Some(tool.to_string()),
            call_id,
            job_id,
            origin,
            help: None,
            // A picked option is mapped back to the raw text by index; the
            // custom row's typed text goes back to the tool verbatim.
            closed: false,
            sensitive: false,
            raw_question: Some(wire.question),
        },
        raw_options,
    })
}
