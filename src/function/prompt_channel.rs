//! Lets a running tool ask the human through Coyote's prompter. The tool
//! writes one request line to its stdout; the stdout pump spots it, asks,
//! and writes the answer line to a pipe the tool inherits as
//! `COYOTE_PROMPT_REPLY`. Lines that are not requests flow through as
//! ordinary output.

use crate::config::sanitize_display_text;
use crate::events::{self, Event, Origin};
use crate::prompt::{PromptAction, PromptAnswer, PromptMemory, PromptRequest, Prompter};
use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(unix)]
use std::io::{self, PipeWriter, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::Command;
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

/// The parent's end of the reply pipe. The child reads replies from the fd
/// named by `env_value`, which `configure` keeps open across exec.
#[cfg(unix)]
pub(crate) struct Server {
    writer: PipeWriter,
    child_end: Option<OwnedFd>,
    child_fd: RawFd,
}

#[cfg(unix)]
impl Server {
    /// The child's read end sits at fd 10 or above: `Stdio` redirects 0-2
    /// in the child and the bash helper saves its stdout on fd 9, so a
    /// lower number would be clobbered before the tool reads from it.
    pub(crate) fn open() -> Result<Self> {
        let (reader, writer) = io::pipe()?;
        // SAFETY: fcntl with F_DUPFD_CLOEXEC only reads the descriptor and
        // returns a fresh one; `reader` stays valid for the call.
        let child_fd = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
        if child_fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: `child_fd` was just returned by fcntl, is open, and nothing
        // else owns it.
        let child_end = unsafe { OwnedFd::from_raw_fd(child_fd) };
        drop(reader);
        Ok(Self {
            writer,
            child_end: Some(child_end),
            child_fd,
        })
    }

    pub(crate) fn env_value(&self) -> String {
        format!("/dev/fd/{}", self.child_fd)
    }

    /// Both pipe ends are CLOEXEC; the child needs its read end to survive
    /// exec, and only that one.
    pub(crate) fn configure(&self, cmd: &mut Command) {
        let fd = self.child_fd;
        // SAFETY: the closure runs between fork and exec and only calls
        // fcntl, which is async-signal-safe; it touches no shared state.
        unsafe {
            cmd.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    /// Drops the parent's copy of the child's read end, so a write after the
    /// tool exits fails with a broken pipe instead of filling a dead pipe.
    pub(crate) fn spawned(&mut self) {
        self.child_end.take();
    }

    pub(crate) fn reply(&self, id: &str, answer: &Value) {
        let mut line = json!({"id": id, "answer": answer}).to_string();
        line.push('\n');
        let mut writer = &self.writer;
        match writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.flush())
        {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::BrokenPipe => {
                debug!("Prompt reply for '{id}' dropped: tool already exited")
            }
            Err(err) => warn!("Unable to write the prompt reply for '{id}': {err}"),
        }
    }
}

#[cfg(not(unix))]
pub(crate) enum Server {}

#[cfg(not(unix))]
impl Server {
    pub(crate) fn open() -> Result<Self> {
        anyhow::bail!("Prompt reply channel is not implemented on this platform") // TASK-316
    }

    pub(crate) fn env_value(&self) -> String {
        match *self {}
    }

    pub(crate) fn configure(&self, _cmd: &mut Command) {
        match *self {}
    }

    pub(crate) fn spawned(&mut self) {
        match *self {}
    }

    pub(crate) fn reply(&self, _id: &str, _answer: &Value) {
        match *self {}
    }
}

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
            events::bus().publish(Event::PromptAnswered {
                id: req.id,
                answer: answer.summary(PromptAction::Confirm),
                auto: true,
            });
            return Ok(answer);
        }
        // A run with no human cannot answer a tool, so the tool fails closed;
        // the headless prompter's `needs_human` JSON is only useful to
        // built-in tools, which hand it back to the model.
        if !crate::prompt::human_present() {
            let answer = PromptAnswer::Cancelled;
            events::bus().publish(req.requested_event());
            events::bus().publish(Event::PromptAnswered {
                id: req.id,
                answer: answer.summary(req.action),
                auto: false,
            });
            return Ok(answer);
        }
        crate::prompt::ask(self.prompter.as_ref(), req)
    }
}

/// Asks and writes the reply line. A prompter failure, an answer of the
/// wrong shape or a choice outside the offered options all answer `null`,
/// so the tool fails closed instead of hanging or acting on a bad value.
/// Chosen options go back as the raw text the tool sent, since that is what
/// the tool matches its answer against.
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
        Ok(PromptAnswer::Selected(choice))
            if action == PromptAction::Select && !options.is_empty() =>
        {
            match raw(choice) {
                Some(raw) => json!(raw),
                None => {
                    warn!(
                        "Tool '{tool}' prompt '{id}': answer '{choice}' is not one of the offered options; answering null"
                    );
                    Value::Null
                }
            }
        }
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
            | (_, PromptAnswer::Cancelled)
    )
}

pub(crate) fn answer_value(answer: &Result<PromptAnswer>) -> Value {
    match answer {
        Ok(PromptAnswer::Confirmed(yes)) => json!(yes),
        Ok(PromptAnswer::Selected(choice)) | Ok(PromptAnswer::Text(choice)) => json!(choice),
        Ok(PromptAnswer::Checked(choices)) => json!(choices),
        Ok(PromptAnswer::Cancelled) | Err(_) => Value::Null,
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
/// it. Question and options are sanitised before the prompter paints them,
/// but a chosen option must go back verbatim or the tool cannot match it.
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
            question: sanitize_display_text(&wire.question),
            options: raw_options
                .iter()
                .map(|option| sanitize_display_text(option))
                .collect(),
            default: wire.default,
            tool: Some(tool.to_string()),
            call_id,
            job_id,
            origin,
        },
        raw_options,
    })
}

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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config::{AppState, RequestContext, WorkingMode};
    use crate::events::{OutputStream, RecordingSink};
    use crate::function::{
        FunctionDeclaration, JsonSchema, ToolCall, ToolIo, jobs, run_llm_function,
    };
    use crate::prompt::{ScriptedPrompter, prompt_events};
    use crate::testing::{AtomicFlagGuard, EnvVarGuard, TestConfigDirGuard, bash_program};
    use crate::turn::SilentSink;
    use crate::utils::{ACP_SERVER, HEADLESS};
    use serial_test::serial;
    use std::collections::HashMap;
    use std::fs;
    use std::io::BufRead;
    use std::os::unix::fs::PermissionsExt;

    const HELPER: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/functions/utils/prompt-utils.sh"
    );

    fn require_tools() {
        assert!(
            bash_program().is_some(),
            "bash must resolve on unix; a silent skip would hide the harness"
        );
        assert!(
            which::which("jq").is_ok(),
            "jq must resolve on unix; a silent skip would hide the harness"
        );
    }

    /// Installs a bash tool that sources the shipped helper on the
    /// functions bin dir the active config guard points at.
    fn install_tool(name: &str, body: &str) {
        let bin = crate::config::paths::functions_bin_dir();
        fs::create_dir_all(&bin).unwrap();
        let path = bin.join(name);
        fs::write(
            &path,
            format!("#!/usr/bin/env bash\nsource \"{HELPER}\"\n{body}\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn tool_envs() -> HashMap<String, String> {
        HashMap::from([("TERM".to_string(), "dumb".to_string())])
    }

    fn io_with(prompter: &Arc<ScriptedPrompter>, memory: Arc<PromptMemory>) -> ToolIo {
        ToolIo::new(Arc::new(SilentSink), Some("call-x7x".to_string()), 0).with_prompter(
            Arc::clone(prompter) as Arc<dyn Prompter>,
            memory,
            Origin::detached(),
        )
    }

    /// A successful tool hands back its raw `$LLM_OUTPUT`; a failed one an
    /// error object. Wrap the former so every test reads `output["output"]`.
    fn run_tool(name: &str, envs: HashMap<String, String>, io: ToolIo) -> Value {
        let result = run_llm_function(name.to_string(), vec![], envs, None, None, io, None)
            .unwrap()
            .unwrap_or_default();
        match serde_json::from_str::<Value>(&result) {
            Ok(error) if error.get("tool_call_error").is_some() => error,
            _ => json!({"output": result}),
        }
    }

    fn request_line(id: &str, action: &str, question: &str) -> Vec<u8> {
        let mut line = json!({"coyote_prompt": {"id": id, "action": action, "question": question}})
            .to_string()
            .into_bytes();
        line.push(b'\n');
        line
    }

    fn stdout_chunks(events: &[Event], call_id: &str) -> String {
        let mut bytes = Vec::new();
        for event in events {
            if let Event::ToolOutputChunk {
                call_id: id,
                stream: OutputStream::Stdout,
                bytes: chunk,
            } = event
                && id == call_id
            {
                bytes.extend_from_slice(chunk);
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    /// The reply line a scripted answer produces for `req`, read back from
    /// the child's end of a fresh server.
    fn reply_for(prompter: ScriptedPrompter, req: PromptRequest) -> Value {
        let raw_options = req.options.clone();
        reply_for_parsed(prompter, ParsedRequest { req, raw_options })
    }

    fn reply_for_parsed(prompter: ScriptedPrompter, parsed: ParsedRequest) -> Value {
        let server = Server::open().unwrap();
        let child_end = server.child_end.as_ref().unwrap().try_clone().unwrap();
        let prompter = ToolPrompter::new(
            Arc::new(prompter),
            Arc::new(PromptMemory::default()),
            Origin::detached(),
        );
        handle_prompt_line(&prompter, &server, parsed);
        let mut line = String::new();
        std::io::BufReader::new(std::fs::File::from(child_end))
            .read_line(&mut line)
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn reply_fd_sits_above_the_low_range() {
        let server = Server::open().unwrap();

        assert!(server.child_fd >= 10, "{}", server.child_fd);
        assert_eq!(server.env_value(), format!("/dev/fd/{}", server.child_fd));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn fixture_tool_round_trips_every_action() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-proto");
        install_tool(
            "ask-x7x",
            r#"ans="$(confirm "Delete? rt-x7x")"; echo "$ans" >> "$LLM_OUTPUT"
ans="$(list "Pick one rt-x7x" a b c)"; echo "$ans" >> "$LLM_OUTPUT"
ans="$(checkbox "Pick many rt-x7x" a b c)"; echo "$ans" >> "$LLM_OUTPUT"
ans="$(input "Name? rt-x7x")"; echo "$ans" >> "$LLM_OUTPUT"
ans="$(password "Secret? rt-x7x")"; echo "$ans" >> "$LLM_OUTPUT""#,
        );
        let prompter = Arc::new(ScriptedPrompter::new(vec![
            PromptAnswer::Confirmed(true),
            PromptAnswer::Selected("b".to_string()),
            PromptAnswer::Checked(vec!["a".to_string(), "c".to_string()]),
            PromptAnswer::Text("hello".to_string()),
            PromptAnswer::Text("s3cret".to_string()),
        ]));
        let sink = RecordingSink::install();

        let output = run_tool(
            "ask-x7x",
            tool_envs(),
            io_with(&prompter, Arc::new(PromptMemory::default())),
        );

        assert_eq!(output["output"], "1\n1\n0 2\nhello\ns3cret\n");
        let requests = prompter.requests();
        assert_eq!(requests.len(), 5, "{requests:?}");
        let actions: Vec<PromptAction> = requests.iter().map(|req| req.action).collect();
        assert_eq!(
            actions,
            vec![
                PromptAction::Confirm,
                PromptAction::Select,
                PromptAction::Checkbox,
                PromptAction::Input,
                PromptAction::Password,
            ]
        );
        for req in &requests {
            assert_eq!(req.tool.as_deref(), Some("ask-x7x"));
            assert_eq!(req.call_id.as_deref(), Some("call-x7x"));
            assert_eq!(req.job_id, None);
        }
        assert_eq!(requests[0].question, "Delete? rt-x7x");
        assert_eq!(requests[1].options, vec!["a", "b", "c"]);
        assert_eq!(requests[2].options, vec!["a", "b", "c"]);
        assert!(requests[0].options.is_empty());
        let chunks = stdout_chunks(&sink.drain(), "call-x7x");
        assert!(!chunks.contains("coyote_prompt"), "{chunks}");
    }

    #[test]
    fn assembler_reassembles_a_request_split_across_reads() {
        let line = request_line("p1", "confirm", &"q".repeat(1500));
        let mut assembler = LineAssembler::default();

        let first = assembler.feed(&line[..1024]);
        let rest = assembler.feed(&line[1024..]);

        assert!(first.is_empty(), "{first:?}");
        assert_eq!(rest, vec![Classified::Prompt(line)]);
        assert_eq!(assembler.finish(), None);
    }

    #[test]
    fn assembler_emits_a_partial_line_unless_it_could_still_be_a_request() {
        let mut assembler = LineAssembler::default();

        assert_eq!(
            assembler.feed(b"progress 42%"),
            vec![Classified::Output(b"progress 42%".to_vec())]
        );
        assert!(assembler.feed(b"  {\"coyote").is_empty());
        assert_eq!(
            assembler.feed(b"_prompt_not_really\n"),
            vec![Classified::Output(
                b"  {\"coyote_prompt_not_really\n".to_vec()
            )]
        );
        assert_eq!(
            assembler.feed(b"   "),
            vec![Classified::Output(b"   ".to_vec())]
        );
        assert_eq!(assembler.finish(), None);
    }

    #[test]
    fn assembler_splits_a_request_glued_to_a_partial_line() {
        let request = request_line("p1", "confirm", "sure?");
        let mut assembler = LineAssembler::default();

        assert_eq!(
            assembler.feed(b"40%"),
            vec![Classified::Output(b"40%".to_vec())]
        );
        assert_eq!(
            assembler.feed(&request),
            vec![Classified::Prompt(request.clone())]
        );

        let mut chunk = b"40%".to_vec();
        chunk.extend_from_slice(&request);
        assert_eq!(
            assembler.feed(&chunk),
            vec![
                Classified::Output(b"40%".to_vec()),
                Classified::Prompt(request),
            ]
        );
        assert_eq!(assembler.finish(), None);
    }

    #[test]
    fn assembler_carries_a_tail_that_may_still_become_a_request() {
        let request = request_line("p1", "confirm", "sure?");
        let (head, rest) = request.split_at(7);
        let mut first = b"Working... ".to_vec();
        first.extend_from_slice(head);
        let mut assembler = LineAssembler::default();

        assert!(assembler.feed(&first).is_empty());
        assert_eq!(
            assembler.feed(rest),
            vec![
                Classified::Output(b"Working... ".to_vec()),
                Classified::Prompt(request),
            ]
        );
        assert_eq!(assembler.finish(), None);
    }

    #[test]
    fn assembler_flushes_an_oversized_carry() {
        let mut assembler = LineAssembler::default();
        let filler = vec![b'x'; 70_000];

        assert!(assembler.feed(MARKER).is_empty());
        let flushed = assembler.feed(&filler);

        let mut expected = MARKER.to_vec();
        expected.extend_from_slice(&filler);
        assert_eq!(flushed, vec![Classified::Output(expected)]);
        assert_eq!(assembler.finish(), None);
    }

    #[test]
    fn assembler_keeps_output_order_around_a_request() {
        let mut chunk = b"before\n".to_vec();
        chunk.extend(request_line("p1", "input", "name?"));
        chunk.extend(b"after\nmore");
        let mut assembler = LineAssembler::default();

        let classified = assembler.feed(&chunk);

        assert_eq!(
            classified,
            vec![
                Classified::Output(b"before\n".to_vec()),
                Classified::Prompt(request_line("p1", "input", "name?")),
                Classified::Output(b"after\nmore".to_vec()),
            ]
        );
    }

    #[test]
    fn parse_request_maps_the_wire_fields_and_rejects_bad_lines() {
        let line = json!({"coyote_prompt": {
            "id": "p9", "action": "select", "question": "which?",
            "options": ["a", "b"], "default": "a"
        }})
        .to_string();

        let req = parse_request(
            line.as_bytes(),
            "tool-x",
            Some("call-1".to_string()),
            Some("job_1".to_string()),
            Origin::detached(),
        )
        .unwrap()
        .req;

        assert_eq!(req.id, "p9");
        assert_eq!(req.action, PromptAction::Select);
        assert_eq!(req.question, "which?");
        assert_eq!(req.options, vec!["a", "b"]);
        assert_eq!(req.default, Some(json!("a")));
        assert_eq!(req.tool.as_deref(), Some("tool-x"));
        assert_eq!(req.call_id.as_deref(), Some("call-1"));
        assert_eq!(req.job_id.as_deref(), Some("job_1"));

        for bad in [
            r#"{"coyote_prompt": not-json}"#,
            r#"{"coyote_prompt": {"id": "p1", "action": "dance", "question": "q"}}"#,
            r#"{"coyote_prompt": {"id": "p1", "action": "confirm"}}"#,
            r#"{"coyote_prompt": {"id": 7, "action": "confirm", "question": "q"}}"#,
            r#"{"coyote_prompt": {"id": "p1", "action": "confirm", "question": ["q"]}}"#,
        ] {
            assert!(
                parse_request(bad.as_bytes(), "t", None, None, Origin::detached()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn parse_request_tolerates_null_options() {
        let line = r#"{"coyote_prompt": {"id": "p1", "action": "select", "question": "q", "options": null}}"#;

        let req = parse_request(line.as_bytes(), "t", None, None, Origin::detached())
            .unwrap()
            .req;

        assert!(req.options.is_empty());
    }

    #[test]
    fn parse_request_strips_terminal_escapes_from_tool_text() {
        let line = json!({"coyote_prompt": {
            "id": "p1", "action": "select", "question": "\u{1b}[2Jwhich?",
            "options": ["\u{1b}[31ma", "b"]
        }})
        .to_string();

        let parsed = parse_request(line.as_bytes(), "t", None, None, Origin::detached()).unwrap();

        assert_eq!(parsed.req.question, "which?");
        assert_eq!(parsed.req.options, vec!["a", "b"]);
        assert_eq!(parsed.raw_options, vec!["\u{1b}[31ma", "b"]);
    }

    #[test]
    fn wire_id_reads_the_id_off_an_unparseable_request() {
        assert_eq!(
            wire_id(br#"{"coyote_prompt": {"id": "p-bad", "action": "dance"}}"#),
            Some("p-bad".to_string())
        );
        assert_eq!(wire_id(br#"{"coyote_prompt": {"id": 7}}"#), None);
        assert_eq!(wire_id(br#"{"coyote_prompt": not-json}"#), None);
    }

    #[test]
    fn answer_value_serialises_every_answer_shape() {
        assert_eq!(
            answer_value(&Ok(PromptAnswer::Confirmed(true))),
            json!(true)
        );
        assert_eq!(
            answer_value(&Ok(PromptAnswer::Confirmed(false))),
            json!(false)
        );
        assert_eq!(
            answer_value(&Ok(PromptAnswer::Selected("b".to_string()))),
            json!("b")
        );
        assert_eq!(
            answer_value(&Ok(PromptAnswer::Text("t".to_string()))),
            json!("t")
        );
        assert_eq!(
            answer_value(&Ok(PromptAnswer::Checked(vec!["a".to_string()]))),
            json!(["a"])
        );
        assert_eq!(answer_value(&Ok(PromptAnswer::Cancelled)), Value::Null);
        assert_eq!(answer_value(&Err(anyhow!("boom"))), Value::Null);
    }

    #[test]
    #[serial]
    fn headless_run_answers_tool_prompts_with_cancel() {
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);
        let sink = RecordingSink::install();
        let prompter = ToolPrompter::new(
            Arc::new(crate::prompt::HeadlessPrompter),
            Arc::new(PromptMemory::default()),
            Origin::detached(),
        );
        let req = PromptRequest::new(
            PromptAction::Input,
            "Name? headless-x7x",
            Origin::detached(),
        );

        let answer = prompter.answer(req).unwrap();

        assert_eq!(answer, PromptAnswer::Cancelled);
        let (requested, answered) = prompt_events(sink.drain(), "Name? headless-x7x");
        let [Event::PromptRequested { id, .. }] = requested.as_slice() else {
            panic!("{requested:?}");
        };
        assert_eq!(
            answered,
            vec![Event::PromptAnswered {
                id: id.clone(),
                answer: "<cancelled>".to_string(),
                auto: false,
            }]
        );
    }

    #[test]
    #[serial]
    fn select_answer_outside_the_options_is_answered_null() {
        let mut req = PromptRequest::new(PromptAction::Select, "Pick? opt-x7x", Origin::detached());
        req.options = vec!["a".to_string(), "b".to_string()];
        let id = req.id.clone();

        let reply = reply_for(
            ScriptedPrompter::new(vec![PromptAnswer::Selected("zzz".to_string())]),
            req,
        );

        assert_eq!(reply, json!({"id": id, "answer": null}));
    }

    #[test]
    #[serial]
    fn chosen_option_is_replied_with_the_text_the_tool_sent() {
        let raw = "\u{1b}[31mred\u{1b}[0m";
        let line = json!({"coyote_prompt": {
            "id": "p-raw", "action": "select", "question": "Colour? raw-x7x",
            "options": [raw, "b"]
        }})
        .to_string();
        let parsed = parse_request(line.as_bytes(), "t", None, None, Origin::detached()).unwrap();
        assert_eq!(parsed.req.options, vec!["red", "b"]);

        let reply = reply_for_parsed(
            ScriptedPrompter::new(vec![PromptAnswer::Selected("red".to_string())]),
            parsed,
        );

        assert_eq!(reply, json!({"id": "p-raw", "answer": raw}));
    }

    #[test]
    #[serial]
    fn mismatched_answer_shape_is_answered_null() {
        let req = PromptRequest::new(PromptAction::Confirm, "Sure? shape-x7x", Origin::detached());
        let id = req.id.clone();

        let reply = reply_for(
            ScriptedPrompter::new(vec![PromptAnswer::Text("x".to_string())]),
            req,
        );

        assert_eq!(reply, json!({"id": id, "answer": null}));
    }

    #[test]
    #[serial]
    fn checkbox_answer_outside_the_options_is_answered_null() {
        let mut req = PromptRequest::new(
            PromptAction::Checkbox,
            "Pick many? cb-x7x",
            Origin::detached(),
        );
        req.options = vec!["a".to_string(), "b".to_string()];
        let id = req.id.clone();

        let reply = reply_for(
            ScriptedPrompter::new(vec![PromptAnswer::Checked(vec![
                "a".to_string(),
                "zzz".to_string(),
            ])]),
            req,
        );

        assert_eq!(reply, json!({"id": id, "answer": null}));
    }

    #[test]
    #[serial]
    fn headless_input_helper_returns_one() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-headless");
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);
        install_tool(
            "hl-x7x",
            r#"if ! v="$(input "Name? hl-x7x")"; then echo refused >> "$LLM_OUTPUT"; fi"#,
        );
        let io = ToolIo::new(Arc::new(SilentSink), Some("call-hl-x7x".to_string()), 0)
            .with_prompter(
                Arc::new(crate::prompt::HeadlessPrompter),
                Arc::new(PromptMemory::default()),
                Origin::detached(),
            );

        let output = run_tool("hl-x7x", tool_envs(), io);

        assert_eq!(output["output"], "refused\n");
    }

    #[test]
    #[serial]
    fn long_question_split_across_reads_round_trips_end_to_end() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-straddle");
        let question = format!("{} straddle-x7x", "q".repeat(1200));
        install_tool(
            "askl-x7x",
            r#"ans="$(confirm "$PROBE_QUESTION")"; echo "$ans" >> "$LLM_OUTPUT""#,
        );
        let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Confirmed(true)]));
        let mut envs = tool_envs();
        envs.insert("PROBE_QUESTION".to_string(), question.clone());

        let output = run_tool(
            "askl-x7x",
            envs,
            io_with(&prompter, Arc::new(PromptMemory::default())),
        );

        assert_eq!(output["output"], "1\n");
        let requests = prompter.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].question, question);
    }

    #[test]
    #[serial]
    fn malformed_and_plain_lines_are_echoed_and_never_prompt() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-malformed");
        install_tool(
            "mal-x7x",
            "echo '{\"coyote_prompt\": not-json}'\necho 'plain line mal-x7x'\nexit 3",
        );
        let sink = RecordingSink::install();
        let prompter = Arc::new(ScriptedPrompter::refusing());

        let output = run_tool(
            "mal-x7x",
            tool_envs(),
            io_with(&prompter, Arc::new(PromptMemory::default())),
        );

        assert!(
            output["tool_call_error"]
                .as_str()
                .unwrap()
                .contains("exited with code 3"),
            "{output}"
        );
        let stdout = output["stdout"].as_str().unwrap();
        assert!(stdout.contains("{\"coyote_prompt\": not-json}"), "{stdout}");
        assert!(stdout.contains("plain line mal-x7x"), "{stdout}");
        assert!(prompter.requests().is_empty());
        let events = sink.drain();
        let chunks = stdout_chunks(&events, "call-x7x");
        assert!(
            chunks.contains("{\"coyote_prompt\": not-json}\n"),
            "{chunks}"
        );
        assert!(chunks.contains("plain line mal-x7x\n"), "{chunks}");
        assert!(
            !events.iter().any(|event| matches!(
                event,
                Event::PromptRequested { tool: Some(tool), .. } if tool == "mal-x7x"
            )),
            "{events:?}"
        );
    }

    #[test]
    #[serial]
    fn request_line_without_a_prompter_is_echoed() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-noprompter");
        install_tool(
            "nop-x7x",
            "echo '{\"coyote_prompt\":{\"id\":\"p1\",\"action\":\"confirm\",\"question\":\"q nop-x7x\"}}'\necho done >> \"$LLM_OUTPUT\"",
        );
        let sink = RecordingSink::install();

        let output = run_tool(
            "nop-x7x",
            tool_envs(),
            ToolIo::new(Arc::new(SilentSink), Some("call-nop-x7x".to_string()), 0),
        );

        assert_eq!(output["output"], "done\n");
        let chunks = stdout_chunks(&sink.drain(), "call-nop-x7x");
        assert!(chunks.contains("coyote_prompt"), "{chunks}");
    }

    #[test]
    #[serial]
    fn unparseable_request_with_an_id_is_answered_null_and_echoed() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-bad-action");
        install_tool(
            "bad-x7x",
            "echo '{\"coyote_prompt\":{\"id\":\"p-bad-x7x\",\"action\":\"dance\",\"question\":\"q\"}}' >&9\nIFS= read -r r <\"$COYOTE_PROMPT_REPLY\"; echo \"$r\" >> \"$LLM_OUTPUT\"",
        );
        let sink = RecordingSink::install();
        let prompter = Arc::new(ScriptedPrompter::refusing());

        let output = run_tool(
            "bad-x7x",
            tool_envs(),
            io_with(&prompter, Arc::new(PromptMemory::default())),
        );

        assert_eq!(
            output["output"],
            format!("{}\n", json!({"id": "p-bad-x7x", "answer": null}))
        );
        assert!(prompter.requests().is_empty());
        let chunks = stdout_chunks(&sink.drain(), "call-x7x");
        assert!(chunks.contains("dance"), "{chunks}");
    }

    /// The helper keeps a second handle on the tool's stdout pipe which a
    /// background descendant inherits, so EOF never arrives while it runs.
    /// A request that descendant makes after the tool exited is answered
    /// null without reaching the prompter.
    #[test]
    #[serial]
    fn tool_returns_once_it_exits_even_if_a_descendant_holds_its_pipes() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-detached");
        install_tool(
            "bg-x7x",
            concat!(
                "sleep 30 >/dev/null 2>&1 &\n",
                "( sleep 1; confirm \"late? late-x7x\" >/dev/null; echo \"late=$?\" >&2 ) &\n",
                "echo done >> \"$LLM_OUTPUT\"",
            ),
        );
        let prompter = Arc::new(ScriptedPrompter::refusing());
        let started = std::time::Instant::now();

        let output = run_tool(
            "bg-x7x",
            tool_envs(),
            io_with(&prompter, Arc::new(PromptMemory::default())),
        );

        assert_eq!(output["output"], "done\n");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(prompter.requests().is_empty(), "{:?}", prompter.requests());
    }

    /// The helper exports the fd it saved stdout on; a tool must start
    /// without that marker or it would skip its own save and write requests
    /// to whatever fd 9 happens to be.
    #[test]
    #[serial]
    fn inherited_saved_fd_marker_is_scrubbed_before_the_tool_starts() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-outfd");
        let _ambient = EnvVarGuard::set(OUT_FD_ENV, "9");
        install_tool("outfd-x7x", "confirm \"Go? outfd-x7x\" >> \"$LLM_OUTPUT\"");
        let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Confirmed(true)]));

        let output = run_tool(
            "outfd-x7x",
            tool_envs(),
            io_with(&prompter, Arc::new(PromptMemory::default())),
        );

        assert_eq!(output["output"], "1");
        assert_eq!(prompter.requests().len(), 1);
    }

    #[test]
    #[serial]
    fn cancel_fails_the_guarded_tool_closed() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-cancel");
        install_tool(
            "cancel-x7x",
            "unset AUTO_CONFIRM LLM_AGENT_VAR_AUTO_CONFIRM\nguard_operation \"Delete? cancel-x7x\"\necho reached >> \"$LLM_OUTPUT\"",
        );
        let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Cancelled]));

        let output = run_tool(
            "cancel-x7x",
            tool_envs(),
            io_with(&prompter, Arc::new(PromptMemory::default())),
        );

        assert!(
            output["tool_call_error"]
                .as_str()
                .unwrap()
                .contains("exited with code 1"),
            "{output}"
        );
        assert!(
            output["stderr"]
                .as_str()
                .unwrap_or_default()
                .contains("Operation aborted!"),
            "{output}"
        );
        assert_eq!(output.get("output"), None, "{output}");
        assert_eq!(prompter.requests().len(), 1);
        assert_eq!(prompter.requests()[0].question, "Delete? cancel-x7x");
    }

    #[test]
    #[serial]
    fn no_prompter_means_no_reply_channel() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-none");
        let _ambient = EnvVarGuard::set(REPLY_ENV, "/dev/fd/99");
        install_tool(
            "nochan-x7x",
            "printf '%s' \"${COYOTE_PROMPT_REPLY:-unset}\" >> \"$LLM_OUTPUT\"",
        );

        let output = run_tool(
            "nochan-x7x",
            tool_envs(),
            ToolIo::new(Arc::new(SilentSink), None, 0),
        );

        assert_eq!(output["output"], "unset");
    }

    #[test]
    #[serial]
    fn remembered_confirm_is_replayed_without_asking() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-memory");
        install_tool(
            "ask-x7x",
            r#"ans="$(confirm "Delete? mem-x7x")"; echo "$ans" >> "$LLM_OUTPUT""#,
        );
        let memory = Arc::new(PromptMemory::default());
        memory.remember("ask-x7x", "Delete? mem-x7x");
        let prompter = Arc::new(ScriptedPrompter::refusing());
        let sink = RecordingSink::install();

        let output = run_tool("ask-x7x", tool_envs(), io_with(&prompter, memory));

        assert_eq!(output["output"], "1\n");
        assert!(prompter.requests().is_empty());
        let (requested, answered) = prompt_events(sink.drain(), "Delete? mem-x7x");
        let [
            Event::PromptRequested {
                id,
                action,
                tool,
                call_id,
                job_id,
                ..
            },
        ] = requested.as_slice()
        else {
            panic!("{requested:?}");
        };
        assert_eq!(*action, PromptAction::Confirm);
        assert_eq!(tool.as_deref(), Some("ask-x7x"));
        assert_eq!(call_id.as_deref(), Some("call-x7x"));
        assert_eq!(*job_id, None);
        assert_eq!(
            answered,
            vec![Event::PromptAnswered {
                id: id.clone(),
                answer: "yes".to_string(),
                auto: true,
            }]
        );
    }

    /// A tool that drew a progress line without a newline and then prompts
    /// must get its answer, keep the partial output, and never leak the
    /// request line to the display.
    #[test]
    #[serial]
    fn prompt_after_unterminated_output_round_trips_end_to_end() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-progress");
        install_tool(
            "bar-x7x",
            r#"printf 'Working... '
ans="$(confirm "Go? bar-x7x")"
echo "done=$ans"
echo "$ans" >> "$LLM_OUTPUT""#,
        );
        let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Confirmed(true)]));
        let sink = RecordingSink::install();

        let output = run_tool(
            "bar-x7x",
            tool_envs(),
            io_with(&prompter, Arc::new(PromptMemory::default())),
        );

        assert_eq!(output["output"], "1\n", "{output}");
        let requests = prompter.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].question, "Go? bar-x7x");
        let chunks = stdout_chunks(&sink.drain(), "call-x7x");
        assert!(!chunks.contains("coyote_prompt"), "{chunks}");
        assert!(
            chunks.starts_with("Working... ") && chunks.contains("done=1"),
            "the partial line must survive and precede the echo: {chunks:?}"
        );
    }

    async fn probe_ui_env() -> Value {
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.ui = Arc::new(SilentSink);
        ctx.prompter = Arc::new(ScriptedPrompter::refusing());
        ctx.tool_scope
            .functions
            .append_declaration(FunctionDeclaration {
                name: "uiprobe-x7x".to_string(),
                description: String::new(),
                parameters: JsonSchema::default(),
                agent: false,
            });
        ToolCall::new(
            "uiprobe-x7x".to_string(),
            json!({}),
            Some("id1".to_string()),
        )
        .eval(&mut ctx)
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn tool_env_carries_coyote_ui() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-ui");
        install_tool(
            "uiprobe-x7x",
            "printf '%s' \"$COYOTE_UI\" >> \"$LLM_OUTPUT\"",
        );
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);

        let headless_off = AtomicFlagGuard::set(&HEADLESS, false);
        assert_eq!(probe_ui_env().await, json!({"output": "repl"}));
        drop(headless_off);

        let _headless_on = AtomicFlagGuard::set(&HEADLESS, true);
        assert_eq!(probe_ui_env().await, json!({"output": "headless"}));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn background_job_prompts_through_the_ring_pump_and_completes() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-job");
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
        let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
        install_tool(
            "jobask-x7x",
            r#"ans="$(confirm "Proceed? job-x7x")"; echo "answer=$ans"; echo "ui=$COYOTE_UI"; echo DONE > "$LLM_OUTPUT""#,
        );
        let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Confirmed(true)]));
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.ui = Arc::new(SilentSink);
        ctx.prompter = Arc::clone(&prompter) as Arc<dyn Prompter>;
        ctx.declared_function_names.insert("jobask-x7x".to_string());

        let started = jobs::handle_job_tool(
            &mut ctx,
            "job__start",
            &json!({"tool": "jobask-x7x", "arguments": {}}),
        )
        .await
        .unwrap();
        assert_eq!(started["status"], "ok", "{started}");
        let job_id = started["job_id"].as_str().unwrap().to_string();
        let collected = jobs::handle_job_tool(&mut ctx, "job__collect", &json!({"id": job_id}))
            .await
            .unwrap();

        assert_eq!(collected["status"], "completed", "{collected}");
        assert_eq!(collected["exit_code"], 0);
        assert_eq!(collected["result"], json!({"output": "DONE\n"}));
        let tail = collected["output_tail"].as_str().unwrap();
        assert!(tail.contains("answer=1"), "{collected}");
        assert!(tail.contains("ui=repl"), "{collected}");
        let requests = prompter.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].question, "Proceed? job-x7x");
        assert_eq!(requests[0].tool.as_deref(), Some("jobask-x7x"));
        assert_eq!(requests[0].job_id.as_deref(), Some(job_id.as_str()));
        assert_eq!(requests[0].call_id, None);
    }

    /// The path a real tool call takes: `ToolCall::eval` against a context
    /// whose prompter is seated. The tool's `confirm` must reach THAT
    /// prompter tagged with the tool name and the call id, the request line
    /// must stay off the displayed output, and a "Yes for this session"
    /// remembered in the context's own memory must replay without asking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn eval_path_hands_the_seated_prompter_and_memory_to_the_tool() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-eval");
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
        let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
        install_tool(
            "evalask-x7x",
            r#"ans="$(confirm "Go? eval-x7x")"; printf 'ans=%s' "$ans" >> "$LLM_OUTPUT""#,
        );
        let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Confirmed(false)]));
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.ui = Arc::new(SilentSink);
        ctx.prompter = Arc::clone(&prompter) as Arc<dyn Prompter>;
        ctx.tool_scope
            .functions
            .append_declaration(FunctionDeclaration {
                name: "evalask-x7x".to_string(),
                description: String::new(),
                parameters: JsonSchema::default(),
                agent: false,
            });
        let sink = RecordingSink::install();

        let first = ToolCall::new(
            "evalask-x7x".to_string(),
            json!({}),
            Some("id-eval-x7x".to_string()),
        )
        .eval(&mut ctx)
        .await
        .unwrap();

        assert_eq!(first, json!({"output": "ans=0"}));
        let requests = prompter.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].action, PromptAction::Confirm);
        assert_eq!(requests[0].question, "Go? eval-x7x");
        assert_eq!(requests[0].tool.as_deref(), Some("evalask-x7x"));
        assert_eq!(requests[0].call_id.as_deref(), Some("id-eval-x7x"));
        assert_eq!(requests[0].job_id, None);
        let chunks = stdout_chunks(&sink.drain(), "id-eval-x7x");
        assert!(!chunks.contains("coyote_prompt"), "{chunks}");

        // Yes-for-session lives on the context; remembering it there alone
        // must be enough for the next call to go through unasked.
        ctx.prompt_memory.remember("evalask-x7x", "Go? eval-x7x");
        let second = ToolCall::new(
            "evalask-x7x".to_string(),
            json!({}),
            Some("id-eval2-x7x".to_string()),
        )
        .eval(&mut ctx)
        .await
        .unwrap();

        assert_eq!(second, json!({"output": "ans=1"}));
        assert_eq!(
            prompter.requests().len(),
            1,
            "a remembered answer must not reach the prompter again"
        );
        let (requested, answered) = prompt_events(sink.drain(), "Go? eval-x7x");
        let [Event::PromptRequested { id, call_id, .. }] = requested.as_slice() else {
            panic!("{requested:?}");
        };
        assert_eq!(call_id.as_deref(), Some("id-eval2-x7x"));
        assert_eq!(
            answered,
            vec![Event::PromptAnswered {
                id: id.clone(),
                answer: "yes".to_string(),
                auto: true,
            }]
        );
    }

    // Black-box contract of the bash helper's reply path, exercised the way
    // a tool author uses it: source the helper, call `confirm`/`list`/
    // `checkbox`/`input`/`password`/`guard_operation`/`guard_path`, read the
    // printed value. No stdout pump is involved; the harness answers the
    // request lines it sees through a live `Server`. The contract pinned:
    //
    // - every helper writes exactly ONE request line starting with the
    //   marker on the tool's REAL stdout (not into the `$(...)` capturing
    //   it), carrying `id`, `action`, `question`, `options`;
    // - answers map onto the helpers' existing return conventions: confirm
    //   prints `1`/`0`, list prints the option INDEX, checkbox prints
    //   space-separated indices, input/password print the text, `null`
    //   (cancel) makes the helper return 1;
    // - `guard_operation`/`guard_path` abort unless the answer is `true`
    //   (fail closed), and do so without a controlling tty;
    // - the AUTO_CONFIRM short-circuit emits no request at all.
    mod bash_contract {
        use super::*;
        use std::path::{Path, PathBuf};
        use std::process::Stdio;
        use std::time::{SystemTime, UNIX_EPOCH};

        fn is_request_line(line: &str) -> bool {
            line.trim_start().as_bytes().starts_with(MARKER)
        }

        /// A unique scratch directory per test, so `guard_path` has a known pwd.
        fn scratch_dir(tag: &str) -> PathBuf {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "coyote-prompt-utils-{tag}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        struct Run {
            stdout: String,
            stderr: String,
            code: i32,
        }

        impl Run {
            fn request_lines(&self) -> Vec<Value> {
                self.stdout
                    .lines()
                    .filter(|line| is_request_line(line))
                    .map(|line| {
                        serde_json::from_str::<Value>(line).unwrap_or_else(|err| {
                            panic!("request line is not JSON ({err}): {line}")
                        })
                    })
                    .collect()
            }

            fn other_lines(&self) -> Vec<&str> {
                self.stdout
                    .lines()
                    .filter(|line| !is_request_line(line))
                    .collect()
            }
        }

        /// Runs `body` in a bash that sourced the helper with a live reply
        /// channel; request lines the shell writes are answered from
        /// `answers` in order (`null` once they run out; `None` closes the
        /// channel before the first read). `detach` puts the shell in its
        /// own session so `/dev/tty` cannot be opened.
        fn run_helper(dir: &Path, answers: Option<&[Value]>, body: &str, detach: bool) -> Run {
            run_helper_opts(dir, answers, body, detach, false)
        }

        /// `run_helper` with `stray`: every real reply is preceded by a reply
        /// line carrying an id no request asked for.
        fn run_helper_opts(
            dir: &Path,
            answers: Option<&[Value]>,
            body: &str,
            detach: bool,
            stray: bool,
        ) -> Run {
            let mut server = Server::open().unwrap();
            let mut cmd = Command::new(bash_program().unwrap());
            cmd.arg("-c")
                .arg(format!("source \"$PROMPT_UTILS\"; {body}"))
                .current_dir(dir)
                .env_remove("AUTO_CONFIRM")
                .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
                .env_remove(OUT_FD_ENV)
                .env(REPLY_ENV, server.env_value())
                .env("PROMPT_UTILS", HELPER)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            server.configure(&mut cmd);
            if detach {
                // SAFETY: setsid is async-signal-safe and touches no shared state.
                unsafe {
                    cmd.pre_exec(|| {
                        if libc::setsid() < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
            let mut child = cmd.spawn().expect("spawn bash");
            server.spawned();
            let server = answers.is_some().then_some(server);
            let mut stderr = child.stderr.take().unwrap();
            let stderr_thread = std::thread::spawn(move || {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
                text
            });
            let mut answers = answers.unwrap_or_default().iter();
            let mut stdout = String::new();
            for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines() {
                let line = line.unwrap();
                if is_request_line(&line)
                    && let Some(server) = server.as_ref()
                {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let id = req["coyote_prompt"]["id"].as_str().unwrap();
                    if stray {
                        server.reply("not-this-request", &json!(false));
                    }
                    server.reply(id, answers.next().unwrap_or(&Value::Null));
                }
                stdout.push_str(&line);
                stdout.push('\n');
            }
            let status = child.wait().unwrap();
            Run {
                stdout,
                stderr: stderr_thread.join().unwrap(),
                code: status.code().unwrap_or(-1),
            }
        }

        /// `v=[<printed>] rc=<helper rc>` is how every value test reports.
        fn capture(helper_call: &str) -> String {
            format!("v=\"$({helper_call})\"; rc=$?; echo \"v=[$v] rc=$rc\"")
        }

        #[test]
        fn confirm_emits_one_request_and_maps_booleans_to_one_and_zero() {
            require_tools();
            let dir = scratch_dir("confirm");
            for (answer, printed) in [(true, "1"), (false, "0")] {
                let run = run_helper(
                    &dir,
                    Some(&[json!(answer)]),
                    &capture("confirm \"Go?\""),
                    false,
                );
                assert_eq!(run.code, 0, "stdout={}\nstderr={}", run.stdout, run.stderr);
                let requests = run.request_lines();
                assert_eq!(
                    requests.len(),
                    1,
                    "exactly one request line: {}",
                    run.stdout
                );
                let req = &requests[0]["coyote_prompt"];
                assert_eq!(req["action"], "confirm");
                assert_eq!(req["question"], "Go?");
                assert!(
                    req["id"].as_str().is_some_and(|id| !id.is_empty()),
                    "request carries a non-empty id: {req}"
                );
                assert_eq!(req["options"], json!([]));
                assert_eq!(run.other_lines(), vec![format!("v=[{printed}] rc=0")]);
                assert!(
                    run.stderr.is_empty(),
                    "protocol path writes nothing to stderr: {}",
                    run.stderr
                );
            }
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn list_prints_the_index_of_the_chosen_option_and_rejects_unknown_ones() {
            require_tools();
            let dir = scratch_dir("list");
            let run = run_helper(
                &dir,
                Some(&[json!("b")]),
                &capture("list \"Pick\" a b c"),
                false,
            );
            let requests = run.request_lines();
            assert_eq!(requests.len(), 1, "{}", run.stdout);
            assert_eq!(requests[0]["coyote_prompt"]["action"], "select");
            assert_eq!(
                requests[0]["coyote_prompt"]["options"],
                json!(["a", "b", "c"])
            );
            assert_eq!(run.other_lines(), vec!["v=[1] rc=0"]);

            // An answer that is not one of the offered options cannot be mapped to
            // an index: the helper fails closed instead of printing something.
            let run = run_helper(
                &dir,
                Some(&[json!("zzz")]),
                &capture("list \"Pick\" a b c"),
                false,
            );
            assert_eq!(run.other_lines(), vec!["v=[] rc=1"], "{}", run.stdout);
            assert!(
                run.stderr.contains("prompt reply unusable"),
                "{}",
                run.stderr
            );
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn options_starting_with_a_dash_keep_their_indices() {
            require_tools();
            let dir = scratch_dir("dash");
            let run = run_helper(
                &dir,
                Some(&[json!("-x")]),
                &capture("list \"Mode?\" --force -x plain"),
                false,
            );
            let requests = run.request_lines();
            assert_eq!(requests.len(), 1, "{}", run.stdout);
            assert_eq!(
                requests[0]["coyote_prompt"]["options"],
                json!(["--force", "-x", "plain"])
            );
            assert_eq!(run.other_lines(), vec!["v=[1] rc=0"], "{}", run.stderr);
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn checkbox_prints_space_separated_indices() {
            require_tools();
            let dir = scratch_dir("checkbox");
            let run = run_helper(
                &dir,
                Some(&[json!(["a", "c"])]),
                &capture("checkbox \"Pick many\" a b c"),
                false,
            );
            let requests = run.request_lines();
            assert_eq!(requests.len(), 1, "{}", run.stdout);
            assert_eq!(requests[0]["coyote_prompt"]["action"], "checkbox");
            assert_eq!(
                requests[0]["coyote_prompt"]["options"],
                json!(["a", "b", "c"])
            );
            assert_eq!(run.other_lines(), vec!["v=[0 2] rc=0"]);

            // Nothing ticked is a valid, non-cancelled answer: empty output, rc 0.
            let run = run_helper(
                &dir,
                Some(&[json!([])]),
                &capture("checkbox \"Pick many\" a b c"),
                false,
            );
            assert_eq!(run.other_lines(), vec!["v=[] rc=0"], "{}", run.stdout);
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn input_and_password_print_the_text_verbatim() {
            require_tools();
            let dir = scratch_dir("text");
            // Quotes, a backslash and non-ASCII letters must survive the round trip.
            let text = r#"Alice "Q" O'Brien \ ünï"#; // debris-exempt: the fixture is the non-ASCII input under test
            for (helper, action) in [("input", "input"), ("password", "password")] {
                let run = run_helper(
                    &dir,
                    Some(&[json!(text)]),
                    &capture(&format!("{helper} \"Value?\"")),
                    false,
                );
                let requests = run.request_lines();
                assert_eq!(requests.len(), 1, "{}", run.stdout);
                assert_eq!(requests[0]["coyote_prompt"]["action"], action);
                assert_eq!(requests[0]["coyote_prompt"]["question"], "Value?");
                assert_eq!(run.other_lines(), vec![format!("v=[{text}] rc=0")]);
            }
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn question_text_is_json_escaped_in_the_request() {
            require_tools();
            let dir = scratch_dir("escape");
            let question = "Delete \"it\"? back\\slash tab\there";
            let body = capture(&format!("confirm '{question}'"));
            let run = run_helper(&dir, Some(&[json!(true)]), &body, false);
            let requests = run.request_lines();
            assert_eq!(requests.len(), 1, "{}", run.stdout);
            assert_eq!(requests[0]["coyote_prompt"]["question"], question);
            assert_eq!(run.other_lines(), vec!["v=[1] rc=0"]);
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn null_answer_makes_every_helper_return_one() {
            require_tools();
            let dir = scratch_dir("null");
            for call in [
                "confirm \"Go?\"",
                "list \"Pick\" a b c",
                "checkbox \"Pick\" a b c",
                "input \"Name?\"",
                "password \"Secret?\"",
            ] {
                let run = run_helper(&dir, Some(&[Value::Null]), &capture(call), false);
                assert_eq!(run.request_lines().len(), 1, "{call}: {}", run.stdout);
                assert_eq!(
                    run.other_lines(),
                    vec!["v=[] rc=1"],
                    "{call}: {}",
                    run.stdout
                );
                assert!(run.stderr.is_empty(), "{call}: {}", run.stderr);
            }
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn guard_operation_fails_closed_unless_the_answer_is_true() {
            require_tools();
            let dir = scratch_dir("guard-op");
            let body = "guard_operation \"Really?\"; echo \"continued rc=$?\"";

            let run = run_helper(&dir, Some(&[json!(true)]), body, false);
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
            assert_eq!(run.other_lines(), vec!["continued rc=0"]);

            // false, cancel, a non-boolean "truthy" value and a closed reply
            // channel all abort: only a boolean `true` lets the tool go on.
            let refusals: Vec<Option<Vec<Value>>> = vec![
                Some(vec![json!(false)]),
                Some(vec![Value::Null]),
                Some(vec![json!("1")]),
                Some(vec![json!(1)]),
                None,
            ];
            for answers in &refusals {
                let run = run_helper(&dir, answers.as_deref(), body, false);
                assert_eq!(
                    run.code, 1,
                    "answers={answers:?}\nstdout={}\nstderr={}",
                    run.stdout, run.stderr
                );
                assert_eq!(
                    run.request_lines().len(),
                    1,
                    "answers={answers:?}: {}",
                    run.stdout
                );
                assert!(
                    run.stderr.contains("Operation aborted!"),
                    "answers={answers:?}: {}",
                    run.stderr
                );
                assert!(
                    !run.stdout.contains("continued"),
                    "answers={answers:?}: the guarded body must not run: {}",
                    run.stdout
                );
            }
            let run = run_helper(&dir, None, body, false);
            assert!(
                run.stderr.contains("prompt reply channel closed"),
                "{}",
                run.stderr
            );
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn guards_work_without_a_controlling_tty() {
            require_tools();
            let dir = scratch_dir("notty");
            // The pre-protocol guards redirected the prompt to /dev/tty; a detached
            // tool has none. On the protocol path the guard must still ask and
            // still honour the answer.
            let body = "(echo probe 2>/dev/tty) && echo has-tty || echo no-tty; \
                        guard_operation \"Really?\"; echo \"continued rc=$?\"";
            let run = run_helper(&dir, Some(&[json!(true)]), body, true);
            assert!(
                run.stdout.contains("no-tty"),
                "the detached shell must have no tty: {}\n{}",
                run.stdout,
                run.stderr
            );
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
            assert!(run.stdout.contains("continued rc=0"), "{}", run.stdout);

            let run = run_helper(&dir, Some(&[json!(false)]), body, true);
            assert_eq!(run.code, 1, "{}\n{}", run.stdout, run.stderr);
            assert!(
                run.stderr.contains("Operation aborted!"),
                "{}\n{}",
                run.stdout,
                run.stderr
            );
            assert!(!run.stdout.contains("continued"), "{}", run.stdout);
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn guard_path_prompts_only_outside_the_working_directory_and_fails_closed() {
            require_tools();
            let dir = scratch_dir("guard-path");
            let outside = std::env::temp_dir().join("coyote-prompt-utils-outside.txt");
            let outside = outside.to_string_lossy().into_owned();
            // Note: the scratch dir lives under temp_dir too, so `outside` must be a
            // sibling of it, not a child; `pwd` is the scratch dir itself.
            let body = format!(
                "guard_path ./inside.txt \"Inside?\"; echo \"inside rc=$?\"; \
                 guard_path \"{outside}\" \"Outside?\"; echo \"outside rc=$?\"; \
                 guard_path \"{outside}\" \"Again?\"; echo \"again rc=$?\""
            );
            // One `true` then nothing: the second outside prompt is answered
            // null and aborts.
            let run = run_helper(&dir, Some(&[json!(true)]), &body, true);
            let requests = run.request_lines();
            assert_eq!(
                requests.len(),
                2,
                "inside path asks nothing: {}",
                run.stdout
            );
            assert_eq!(requests[0]["coyote_prompt"]["question"], "Outside?");
            assert_eq!(requests[1]["coyote_prompt"]["question"], "Again?");
            assert_eq!(
                run.other_lines(),
                vec!["inside rc=0", "outside rc=0"],
                "{}\n{}",
                run.stdout,
                run.stderr
            );
            assert_eq!(run.code, 1, "{}\n{}", run.stdout, run.stderr);
            assert!(run.stderr.contains("Operation aborted!"), "{}", run.stderr);

            let run = run_helper(
                &dir,
                Some(&[json!(false)]),
                &format!("guard_path \"{outside}\" \"Outside?\"; echo \"outside rc=$?\""),
                false,
            );
            assert_eq!(run.code, 1, "{}\n{}", run.stdout, run.stderr);
            assert!(!run.stdout.contains("outside rc="), "{}", run.stdout);
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn auto_confirm_short_circuit_emits_no_request() {
            require_tools();
            let dir = scratch_dir("auto");
            for var in ["AUTO_CONFIRM", "LLM_AGENT_VAR_AUTO_CONFIRM"] {
                let body = format!(
                    "export {var}=1; guard_operation \"Really?\"; echo \"op rc=$?\"; \
                     guard_path /etc/hosts \"Path?\"; echo \"path rc=$?\""
                );
                // No channel: any emitted request would hit EOF and abort.
                let run = run_helper(&dir, None, &body, false);
                assert_eq!(run.code, 0, "{var}: {}\n{}", run.stdout, run.stderr);
                assert!(run.request_lines().is_empty(), "{var}: {}", run.stdout);
                assert_eq!(run.other_lines(), vec!["op rc=0", "path rc=0"], "{var}");
            }
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn request_goes_to_the_real_stdout_even_when_the_helper_is_redirected() {
            require_tools();
            let dir = scratch_dir("real-stdout");
            // The request must bypass whatever the caller does with the helper's
            // stdout; the printed value, by contrast, follows the redirect.
            let run = run_helper(
                &dir,
                Some(&[json!(true)]),
                "v=\"$(confirm \"Go?\" >/dev/null)\"; echo \"v=[$v]\"",
                false,
            );
            assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
            assert_eq!(run.other_lines(), vec!["v=[]"]);

            // And a request written inside a nested substitution still surfaces.
            let run = run_helper(
                &dir,
                Some(&[json!(true)]),
                "outer=\"$(inner=\"$(confirm \"Go?\")\"; echo \"inner=[$inner]\")\"; echo \"$outer\"",
                false,
            );
            assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
            assert_eq!(run.other_lines(), vec!["inner=[1]"]);
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn successive_prompts_consume_replies_in_order_with_distinct_ids() {
            require_tools();
            let dir = scratch_dir("order");
            let run = run_helper(
                &dir,
                Some(&[json!(true), json!("c"), json!("Bob")]),
                "a=\"$(confirm \"One?\")\"; b=\"$(list \"Two?\" a b c)\"; c=\"$(input \"Three?\")\"; \
                 echo \"a=[$a] b=[$b] c=[$c]\"",
                false,
            );
            let requests = run.request_lines();
            assert_eq!(requests.len(), 3, "{}", run.stdout);
            let questions: Vec<&str> = requests
                .iter()
                .map(|req| req["coyote_prompt"]["question"].as_str().unwrap())
                .collect();
            assert_eq!(questions, vec!["One?", "Two?", "Three?"]);
            let mut ids: Vec<&str> = requests
                .iter()
                .map(|req| req["coyote_prompt"]["id"].as_str().unwrap())
                .collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), 3, "request ids must be distinct: {}", run.stdout);
            assert_eq!(run.other_lines(), vec!["a=[1] b=[2] c=[Bob]"]);
            fs::remove_dir_all(&dir).ok();
        }

        // Replies are read until the id matches; a cancel makes the helper
        // exit 1 with its usual stderr message. The shipped tools
        // (`execute_command.sh`, `fs_rm.sh`, ...) and the `run-tool.sh` shim
        // all run under `set -e`, so the guards are exercised that way too.

        #[test]
        fn reply_lines_for_other_ids_are_skipped_until_ours_arrives() {
            require_tools();
            let dir = scratch_dir("stray-id");
            let run = run_helper_opts(
                &dir,
                Some(&[json!(true), json!("b")]),
                "a=\"$(confirm \"One?\")\"; b=\"$(list \"Two?\" a b c)\"; echo \"a=[$a] b=[$b]\"",
                false,
                true,
            );
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            assert_eq!(run.request_lines().len(), 2, "{}", run.stdout);
            // The stray `false` for a foreign id must not be taken as ours.
            assert_eq!(run.other_lines(), vec!["a=[1] b=[1]"]);
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn guards_under_set_e_abort_with_their_message_on_cancel() {
            require_tools();
            let dir = scratch_dir("set-e");
            let outside = scratch_dir("set-e-outside").join("victim.txt");
            fs::write(&outside, "x").unwrap();
            let bodies = [
                "set -e; guard_operation \"Really?\"; echo continued".to_string(),
                format!(
                    "set -e; guard_path \"{}\" \"Touch?\"; echo continued",
                    outside.display()
                ),
            ];
            for body in &bodies {
                // A plain `false` keeps its message under set -e...
                let run = run_helper(&dir, Some(&[json!(false)]), body, false);
                assert_eq!(run.code, 1, "{body}\n{}\n{}", run.stdout, run.stderr);
                assert!(
                    run.stderr.contains("Operation aborted!"),
                    "{body}: stderr={}",
                    run.stderr
                );
                assert!(!run.stdout.contains("continued"), "{body}: {}", run.stdout);

                // ...and so must a cancel (`null`): the tool exits 1 AND says why.
                let run = run_helper(&dir, Some(&[Value::Null]), body, false);
                assert_eq!(run.code, 1, "{body}\n{}\n{}", run.stdout, run.stderr);
                assert!(!run.stdout.contains("continued"), "{body}: {}", run.stdout);
                assert!(
                    run.stderr.contains("Operation aborted!"),
                    "{body}: a cancelled prompt under set -e must still report \
                     'Operation aborted!' on stderr; got stderr={:?}",
                    run.stderr
                );
            }
            fs::remove_dir_all(&dir).ok();
            fs::remove_dir_all(outside.parent().unwrap()).ok();
        }

        const TOOLS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/functions/tools");

        /// Sources a shipped tool the way the `run-tool.sh` shim does (env
        /// prepared, `main` invoked) so the probe runs the real consumers.
        fn shipped(tool: &str, pre: &str) -> String {
            format!(
                "export LLM_PROMPT_UTILS_FILE=\"$PROMPT_UTILS\" LLM_OUTPUT=/dev/stdout; \
                 {pre}; source \"{TOOLS}/{tool}\"; main"
            )
        }

        #[test]
        fn shipped_execute_command_honours_every_answer() {
            require_tools();
            let dir = scratch_dir("shipped-exec");
            let pre = "export LLM_TOOL_RAW_JSON='{\"command\":\"echo ran-x7x\"}'";
            let body = shipped("execute_command.sh", pre);

            let run = run_helper(&dir, Some(&[json!(true)]), &body, false);
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            let requests = run.request_lines();
            assert_eq!(requests.len(), 1, "{}", run.stdout);
            assert_eq!(
                requests[0]["coyote_prompt"]["question"],
                "Are you sure you want to continue?"
            );
            assert_eq!(run.other_lines(), vec!["ran-x7x"]);

            for answer in [json!(false), Value::Null] {
                let run = run_helper(&dir, Some(std::slice::from_ref(&answer)), &body, false);
                assert_eq!(run.code, 1, "{answer}: {}\n{}", run.stdout, run.stderr);
                assert!(!run.stdout.contains("ran-x7x"), "{answer}: {}", run.stdout);
                assert!(
                    run.stderr.contains("Operation aborted!"),
                    "{answer}: execute_command must say why it stopped; stderr={:?}",
                    run.stderr
                );
            }
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn shipped_fs_rm_outside_the_working_directory_asks_and_fails_closed() {
            require_tools();
            let dir = scratch_dir("shipped-rm");
            let victim_dir = scratch_dir("shipped-rm-victim");
            let victim = victim_dir.join("victim.txt");
            let body = shipped("fs_rm.sh", &format!("argc_path=\"{}\"", victim.display()));

            for answer in [json!(false), Value::Null] {
                fs::write(&victim, "x").unwrap();
                let run = run_helper(&dir, Some(std::slice::from_ref(&answer)), &body, false);
                assert_eq!(run.code, 1, "{answer}: {}\n{}", run.stdout, run.stderr);
                let requests = run.request_lines();
                assert_eq!(requests.len(), 1, "{answer}: {}", run.stdout);
                assert_eq!(
                    requests[0]["coyote_prompt"]["question"],
                    format!("Remove '{}'?", victim.display())
                );
                assert!(victim.exists(), "{answer}: the refused path must survive");
                assert!(
                    !run.stdout.contains("Path removed"),
                    "{answer}: {}",
                    run.stdout
                );
                assert!(
                    run.stderr.contains("Operation aborted!"),
                    "{answer}: fs_rm must say why it stopped; stderr={:?}",
                    run.stderr
                );
            }

            fs::write(&victim, "x").unwrap();
            let run = run_helper(&dir, Some(&[json!(true)]), &body, false);
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
            assert!(!victim.exists(), "an approved removal must remove the path");
            assert_eq!(
                run.other_lines(),
                vec![format!("Path removed: {}", victim.display())]
            );
            fs::remove_dir_all(&dir).ok();
            fs::remove_dir_all(&victim_dir).ok();
        }

        #[test]
        fn checkbox_and_input_accept_empty_answers() {
            require_tools();
            let dir = scratch_dir("empty");
            let run = run_helper(
                &dir,
                Some(&[json!([]), json!("")]),
                &format!(
                    "{}; {}",
                    capture("checkbox \"Pick\" a b c"),
                    capture("input \"Name?\"")
                ),
                false,
            );
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            assert_eq!(run.request_lines().len(), 2, "{}", run.stdout);
            // Nothing picked / nothing typed is a valid answer, not a cancel.
            assert_eq!(run.other_lines(), vec!["v=[] rc=0", "v=[] rc=0"]);
            fs::remove_dir_all(&dir).ok();
        }

        // The helpers' return conventions must hold whatever shell options
        // and option texts a tool author brings along, and the channel the
        // host hands a tool must still be there one `bash` deeper.

        #[test]
        fn helpers_keep_their_conventions_under_strict_mode_and_a_custom_ifs() {
            require_tools();
            let dir = scratch_dir("strict");
            // `set -euo pipefail` plus a whitespace-free IFS is the common
            // "strict mode" prologue; an unset variable or a word-splitting
            // assumption on the protocol path would surface here.
            // `set +u` before the guard: its AUTO_CONFIRM short-circuit is the
            // pre-protocol code (`[[ -z "$AUTO_CONFIRM" ]]`) and has never
            // tolerated nounset; that is outside the protocol contract.
            let body = format!(
                "set -euo pipefail; IFS=$'\\n\\t'; {}; {}; {}; {}; \
                 set +u; guard_operation \"Go on?\"; echo \"guard rc=$?\"",
                capture("confirm \"Go?\""),
                capture("list \"Pick\" a b c"),
                capture("checkbox \"Pick many\" a b c"),
                capture("input \"Name?\"")
            );
            let run = run_helper(
                &dir,
                Some(&[
                    json!(true),
                    json!("c"),
                    json!(["a", "b", "c"]),
                    json!("Bob Smith"),
                    json!(true),
                ]),
                &body,
                false,
            );
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            assert_eq!(run.request_lines().len(), 5, "{}", run.stdout);
            assert_eq!(
                run.other_lines(),
                vec![
                    "v=[1] rc=0",
                    "v=[2] rc=0",
                    "v=[0 1 2] rc=0",
                    "v=[Bob Smith] rc=0",
                    "guard rc=0",
                ],
                "{}\n{}",
                run.stdout,
                run.stderr
            );
            assert!(run.stderr.is_empty(), "{}", run.stderr);
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn options_are_matched_exactly_whatever_their_text() {
            require_tools();
            let dir = scratch_dir("options");
            // Spaces, quotes, regex metacharacters, non-ASCII and an option that
            // is a prefix of another: the request must carry them verbatim and
            // the printed index must come from an exact match.
            let options = [
                "foo",
                "foo.bar",
                "two words",
                "say \"hi\"",
                "a+b*",
                "ünï", // debris-exempt: the fixture is the non-ASCII option under test
                "10",
                "1",
            ];
            let quoted: Vec<String> = options
                .iter()
                .map(|opt| format!("'{}'", opt.replace('\'', "'\\''")))
                .collect();
            let quoted = quoted.join(" ");
            for (answer, index) in [
                ("foo", 0),
                ("foo.bar", 1),
                ("two words", 2),
                ("say \"hi\"", 3),
                ("a+b*", 4),
                ("ünï", 5), // debris-exempt: see the fixture above
                ("1", 7),
            ] {
                let run = run_helper(
                    &dir,
                    Some(&[json!(answer)]),
                    &capture(&format!("list \"Pick\" {quoted}")),
                    false,
                );
                let requests = run.request_lines();
                assert_eq!(requests.len(), 1, "{answer}: {}", run.stdout);
                assert_eq!(
                    requests[0]["coyote_prompt"]["options"],
                    json!(options),
                    "{answer}: options must reach the host verbatim"
                );
                assert_eq!(
                    run.other_lines(),
                    vec![format!("v=[{index}] rc=0")],
                    "{answer}: {}\n{}",
                    run.stdout,
                    run.stderr
                );
            }

            let run = run_helper(
                &dir,
                Some(&[json!(["1", "two words", "foo.bar"])]),
                &capture(&format!("checkbox \"Pick many\" {quoted}")),
                false,
            );
            assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
            let printed = run.other_lines();
            assert_eq!(printed.len(), 1, "{}", run.stdout);
            let mut indices: Vec<&str> = printed[0]
                .trim_start_matches("v=[")
                .split("] rc=")
                .next()
                .unwrap()
                .split(' ')
                .collect();
            indices.sort_unstable();
            assert_eq!(
                indices,
                vec!["1", "2", "7"],
                "{}\n{}",
                run.stdout,
                run.stderr
            );
            assert!(printed[0].ends_with("rc=0"), "{}", run.stdout);
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn an_option_with_a_control_character_is_sent_verbatim_and_index_mapped() {
            require_tools();
            let dir = scratch_dir("tab-option");

            let run = run_helper(
                &dir,
                Some(&[json!("a\tb")]),
                &capture("list \"Pick\" $'a\\tb' plain"),
                false,
            );

            let requests = run.request_lines();
            assert_eq!(requests.len(), 1, "{}", run.stdout);
            assert_eq!(
                requests[0]["coyote_prompt"]["options"],
                json!(["a\tb", "plain"])
            );
            assert_eq!(
                run.other_lines(),
                vec!["v=[0] rc=0"],
                "{}\n{}",
                run.stdout,
                run.stderr
            );
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn a_nested_bash_script_inherits_the_channel() {
            require_tools();
            let dir = scratch_dir("nested");
            // A tool that shells out to another script which sources the helper
            // itself: the reply fd and the real stdout are inherited, so the
            // inner prompt must round-trip exactly like an outer one.
            let inner = dir.join("inner.sh");
            fs::write(
                &inner,
                "source \"$PROMPT_UTILS\"\nv=\"$(confirm \"Inner?\")\"; echo \"inner v=[$v] rc=$?\"\n",
            )
            .unwrap();
            let body = format!(
                "a=\"$(confirm \"Outer?\")\"; echo \"outer a=[$a]\"; bash \"{}\"; echo \"after rc=$?\"",
                inner.display()
            );
            let run = run_helper(&dir, Some(&[json!(true), json!(false)]), &body, false);
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            let requests = run.request_lines();
            assert_eq!(requests.len(), 2, "{}", run.stdout);
            assert_eq!(requests[1]["coyote_prompt"]["question"], "Inner?");
            assert_eq!(
                run.other_lines(),
                vec!["outer a=[1]", "inner v=[0] rc=0", "after rc=0"],
                "{}\n{}",
                run.stdout,
                run.stderr
            );
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn sourcing_the_helper_twice_keeps_a_single_channel() {
            require_tools();
            let dir = scratch_dir("resource");
            // Tool libraries routinely re-source the helper. The second
            // source must neither save fd 9 over the already-saved copy nor
            // lose the channel: one request, the usual answer, no noise.
            let body = format!(
                "source \"$PROMPT_UTILS\"; {}",
                capture("confirm \"Again?\"")
            );
            let run = run_helper(&dir, Some(&[json!(true)]), &body, false);
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            let requests = run.request_lines();
            assert_eq!(requests.len(), 1, "{}", run.stdout);
            assert_eq!(requests[0]["coyote_prompt"]["question"], "Again?");
            assert_eq!(
                run.other_lines(),
                vec!["v=[1] rc=0"],
                "{}\n{}",
                run.stdout,
                run.stderr
            );
            assert!(
                run.stderr.is_empty(),
                "re-sourcing must stay silent: {}",
                run.stderr
            );
            fs::remove_dir_all(&dir).ok();
        }
    }
}
