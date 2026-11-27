use super::{
    CARRY_CAP, MARKER, OUT_FD_ENV, ParsedRequest, REPLY_ENV, Server, ToolPrompter,
    handle_prompt_line, reject_line,
};
use serde_json::Value;
use std::any::Any;
use std::collections::HashMap;
use std::fmt::Display;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Classified {
    Output(Vec<u8>),
    /// From [`MARKER`] to the end of its line, newline included.
    Prompt(Vec<u8>),
}

/// Splits a tool's stdout into lines and picks out the protocol ones. A
/// request may follow unterminated output on the same line (a progress bar
/// drawn without a newline); the output before the marker is handed on as
/// output. Bytes are passed on in order and unchanged; only the chunking
/// differs, and only where a tail might still turn into a request line.
#[derive(Default)]
pub(crate) struct LineAssembler {
    carry: Vec<u8>,
}

impl LineAssembler {
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<Classified> {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(offset) = data[start..].iter().position(|&byte| byte == b'\n') {
            let end = start + offset + 1;
            classify_line(&mut out, &data[start..end]);
            start = end;
        }
        let tail = &data[start..];
        if !could_become_prompt(tail) {
            push_output(&mut out, tail);
        } else if tail.len() > CARRY_CAP {
            warn!(
                "Tool output held {} bytes without a newline that could still become a prompt line; flushing it as output",
                tail.len()
            );
            push_output(&mut out, tail);
        } else {
            self.carry = tail.to_vec();
        }
        out
    }

    /// The carried tail at EOF, which no newline will ever complete.
    pub(crate) fn finish(&mut self) -> Option<Vec<u8>> {
        let tail = std::mem::take(&mut self.carry);
        (!tail.is_empty()).then_some(tail)
    }
}

fn classify_line(out: &mut Vec<Classified>, line: &[u8]) {
    match find_marker(line) {
        None => push_output(out, line),
        Some(pos) => {
            push_output(out, &line[..pos]);
            out.push(Classified::Prompt(line[pos..].to_vec()));
        }
    }
}

fn push_output(out: &mut Vec<Classified>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    match out.last_mut() {
        Some(Classified::Output(previous)) => previous.extend_from_slice(bytes),
        _ => out.push(Classified::Output(bytes.to_vec())),
    }
}

fn find_marker(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(MARKER.len())
        .position(|window| window == MARKER)
}

/// Holding back ordinary partial output would stall progress bars, so a
/// tail is only carried while it holds the marker or ends in a prefix of it.
fn could_become_prompt(tail: &[u8]) -> bool {
    find_marker(tail).is_some()
        || (1..MARKER.len().min(tail.len() + 1))
            .any(|len| MARKER.starts_with(&tail[tail.len() - len..]))
}

/// The reply channel a tool is spawned with. Without a prompter, or when
/// the channel does not open, the tool gets no `COYOTE_PROMPT_REPLY` and
/// its request lines are plain output.
#[derive(Default)]
pub(crate) struct Attachment {
    prompter: Option<ToolPrompter>,
    server: Option<Server>,
}

impl Attachment {
    pub(crate) fn open(
        prompter: Option<ToolPrompter>,
        tool: &str,
        envs: &mut HashMap<String, String>,
    ) -> Self {
        let server = prompter.as_ref().and_then(|_| match Server::open() {
            Ok(server) => {
                envs.insert(REPLY_ENV.into(), server.env_value());
                Some(server)
            }
            Err(err) => {
                warn!("Prompt reply channel unavailable for '{tool}': {err}");
                None
            }
        });
        Self { prompter, server }
    }

    /// Strips the channel variables a tool could inherit from the
    /// environment Coyote itself runs in, so a nested run never writes
    /// to, or reads from, an outer pump.
    pub(crate) fn configure(&self, cmd: &mut Command) {
        cmd.env_remove(OUT_FD_ENV);
        match &self.server {
            Some(server) => server.configure(cmd),
            None => {
                cmd.env_remove(REPLY_ENV);
            }
        }
    }

    pub(crate) fn spawned(&mut self) {
        if let Some(server) = self.server.as_mut() {
            server.spawned();
        }
    }

    /// `None` when the tool has no channel to be answered on. `exited` is
    /// set once the tool is reaped.
    pub(crate) fn router(
        self,
        tool: &str,
        call_id: Option<String>,
        job_id: Option<String>,
        exited: Arc<AtomicBool>,
    ) -> Option<Router> {
        let (prompter, server) = self.prompter.zip(self.server)?;
        let who = match &job_id {
            Some(job_id) => format!("job '{job_id}'"),
            None => format!("tool '{tool}'"),
        };
        Some(Router {
            prompter,
            reply: Arc::new(server),
            tool: tool.to_string(),
            call_id,
            job_id,
            who,
            exited,
        })
    }
}

/// Decides what becomes of each piece of a tool's stdout. Request lines
/// read after the tool exited come from a surviving descendant; they are
/// answered null rather than raised as prompts for a tool already
/// reported finished.
pub(crate) struct Router {
    prompter: ToolPrompter,
    reply: Arc<Server>,
    tool: String,
    call_id: Option<String>,
    job_id: Option<String>,
    who: String,
    exited: Arc<AtomicBool>,
}

pub(crate) enum Routed {
    Output(Vec<u8>),
    Served(Request),
    /// A marker line that will not be answered, to be echoed as output.
    /// A request id it names has already been answered null.
    Rejected(Vec<u8>),
}

impl Router {
    pub(crate) fn route(&self, classified: Classified) -> Routed {
        let line = match classified {
            Classified::Output(bytes) => return Routed::Output(bytes),
            Classified::Prompt(line) => line,
        };
        if !self.exited.load(Ordering::Acquire)
            && let Some(parsed) = self.prompter.parse_line(
                &line,
                &self.tool,
                self.call_id.clone(),
                self.job_id.clone(),
            )
        {
            return Routed::Served(Request {
                prompter: self.prompter.clone(),
                reply: Arc::clone(&self.reply),
                who: self.who.clone(),
                parsed: Box::new(parsed),
            });
        }
        reject_line(&self.reply, &self.tool, &line);
        Routed::Rejected(line)
    }

    /// Answers null for a request whose serving never finished, so the
    /// tool is not left waiting on the reply pipe.
    pub(crate) fn not_answered(&self, id: &str, err: impl Display) {
        not_answered(&self.reply, &self.who, id, err);
    }
}

/// A parsed request, owning what it needs to be answered on another thread.
pub(crate) struct Request {
    prompter: ToolPrompter,
    reply: Arc<Server>,
    who: String,
    parsed: Box<ParsedRequest>,
}

impl Request {
    pub(crate) fn id(&self) -> &str {
        &self.parsed.req.id
    }

    /// Asks and writes the reply; blocks while the prompter waits on a
    /// human. A panicking prompter answers null, so the tool fails closed
    /// instead of waiting on a reply that never comes, and the bus sees the
    /// request closed as errored.
    pub(crate) fn serve(self) {
        let Self {
            prompter,
            reply,
            who,
            parsed,
        } = self;
        let id = parsed.req.id.clone();
        let served = catch_unwind(AssertUnwindSafe(|| {
            handle_prompt_line(&prompter, &reply, *parsed)
        }));
        if let Err(panic) = served {
            crate::events::bus().publish(crate::events::Event::PromptAnswered {
                id: id.clone(),
                answer: crate::prompt::ERROR_MARKER.to_string(),
                auto: false,
            });
            not_answered(
                &reply,
                &who,
                &id,
                format_args!("the prompter panicked: {}", panic_message(panic.as_ref())),
            );
        }
    }
}

fn not_answered(reply: &Server, who: &str, id: &str, err: impl Display) {
    warn!("Prompt from {who} was not answered: {err}");
    reply.reply(id, &Value::Null);
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}
