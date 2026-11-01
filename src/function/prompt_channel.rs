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
        HashMap::from([
            ("TERM".to_string(), "dumb".to_string()),
            ("NODE_NO_WARNINGS".to_string(), "1".to_string()),
        ])
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

    /// Cancelling a job aborts its task mid-await; the pump it leaves behind
    /// must not raise a live prompt for a descendant that outlives the kill.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn cancelled_job_never_prompts_through_a_detached_descendant() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-job-cancel");
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
        let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
        install_tool(
            "jobcancel-x7x",
            r#"( sleep 2; confirm "late? jl-x7x" >/dev/null ) & sleep 30"#,
        );
        let prompter = Arc::new(ScriptedPrompter::refusing());
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.ui = Arc::new(SilentSink);
        ctx.prompter = Arc::clone(&prompter) as Arc<dyn Prompter>;
        ctx.declared_function_names
            .insert("jobcancel-x7x".to_string());

        let started = jobs::handle_job_tool(
            &mut ctx,
            "job__start",
            &json!({"tool": "jobcancel-x7x", "arguments": {}}),
        )
        .await
        .unwrap();
        assert_eq!(started["status"], "ok", "{started}");
        let job_id = started["job_id"].as_str().unwrap().to_string();
        let cancelled = jobs::handle_job_tool(&mut ctx, "job__cancel", &json!({"id": job_id}))
            .await
            .unwrap();
        assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
        assert_eq!(cancelled["id"], job_id, "{cancelled}");

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        assert!(prompter.requests().is_empty(), "{:?}", prompter.requests());
    }

    /// A marker line that is not valid JSON is plain output: it lands in the
    /// job's tail and nobody is asked anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial]
    async fn job_malformed_prompt_line_is_echoed_to_the_tail_and_never_prompts() {
        require_tools();
        let _guard = TestConfigDirGuard::new("prompt-job-malformed");
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
        let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
        install_tool(
            "jobbad-x7x",
            r#"echo '{"coyote_prompt": not-json}'; exit 3"#,
        );
        let prompter = Arc::new(ScriptedPrompter::refusing());
        let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
        ctx.ui = Arc::new(SilentSink);
        ctx.prompter = Arc::clone(&prompter) as Arc<dyn Prompter>;
        ctx.declared_function_names.insert("jobbad-x7x".to_string());

        let started = jobs::handle_job_tool(
            &mut ctx,
            "job__start",
            &json!({"tool": "jobbad-x7x", "arguments": {}}),
        )
        .await
        .unwrap();
        assert_eq!(started["status"], "ok", "{started}");
        let job_id = started["job_id"].as_str().unwrap().to_string();
        let collected = jobs::handle_job_tool(&mut ctx, "job__collect", &json!({"id": job_id}))
            .await
            .unwrap();

        assert_eq!(collected["exit_code"], 3, "{collected}");
        let tail = collected["output_tail"].as_str().unwrap();
        assert!(
            tail.contains(r#"{"coyote_prompt": not-json}"#),
            "{collected}"
        );
        assert!(prompter.requests().is_empty(), "{:?}", prompter.requests());
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

        /// Reply lines the harness plants that no request asked for.
        #[derive(Clone, Copy, PartialEq, Eq)]
        enum Stray {
            None,
            /// A foreign-id reply before every real one.
            Before,
            /// A foreign-id reply after every real one: the next helper has
            /// to skip it and the last one must still sit in the pipe when
            /// the tool reads the channel itself.
            After,
            /// A foreign-id reply and a line that is not JSON, then the
            /// channel closes without ever answering the request.
            ThenClose,
        }

        /// Runs `body` in a bash that sourced the helper with a live reply
        /// channel; request lines the shell writes are answered from
        /// `answers` in order (`null` once they run out; `None` closes the
        /// channel before the first read). `detach` puts the shell in its
        /// own session so `/dev/tty` cannot be opened.
        fn run_helper(dir: &Path, answers: Option<&[Value]>, body: &str, detach: bool) -> Run {
            run_helper_opts(dir, answers, body, detach, Stray::None)
        }

        /// `run_helper` with a [`Stray`] policy for unasked-for reply lines.
        fn run_helper_opts(
            dir: &Path,
            answers: Option<&[Value]>,
            body: &str,
            detach: bool,
            stray: Stray,
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
            let mut server = answers.is_some().then_some(server);
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
                let mut close_channel = false;
                if is_request_line(&line)
                    && let Some(server) = server.as_ref()
                {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let id = req["coyote_prompt"]["id"].as_str().unwrap();
                    match stray {
                        Stray::None => server.reply(id, answers.next().unwrap_or(&Value::Null)),
                        Stray::Before => {
                            server.reply("not-this-request", &json!(false));
                            server.reply(id, answers.next().unwrap_or(&Value::Null));
                        }
                        Stray::After => {
                            server.reply(id, answers.next().unwrap_or(&Value::Null));
                            server.reply("not-this-request", &json!(false));
                        }
                        Stray::ThenClose => {
                            server.reply("not-this-request", &json!(false));
                            let mut writer = &server.writer;
                            writer.write_all(b"this is not json\n").unwrap();
                            writer.flush().unwrap();
                            close_channel = true;
                        }
                    }
                }
                if close_channel {
                    server = None;
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
                Stray::Before,
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
            // `set +u` before the guard: `guard_path` reaches `_to_real_path`,
            // whose bare `$OS` read predates the protocol and is not nounset
            // clean; that path is outside the protocol contract.
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

        // The request is ONE stdout line whatever the question text, answers
        // are printed as text, secrets stay off
        // stderr, the terminal-only helpers never touch the channel, and an
        // unusable reply path fails closed instead of hanging the tool.

        #[test]
        fn question_with_a_newline_stays_one_request_line() {
            require_tools();
            let dir = scratch_dir("newline-question");
            // A tool writes ONE stdout line: a raw newline in the question
            // must be JSON-escaped, never split the request in two.
            let run = run_helper(
                &dir,
                Some(&[json!(true)]),
                &capture("confirm $'first line\\nsecond line'"),
                false,
            );
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            let requests = run.request_lines();
            assert_eq!(requests.len(), 1, "{}", run.stdout);
            assert_eq!(
                requests[0]["coyote_prompt"]["question"],
                "first line\nsecond line"
            );
            assert_eq!(
                run.other_lines(),
                vec!["v=[1] rc=0"],
                "a fragment of the request must not leak as plain output: {}",
                run.stdout
            );
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn multi_line_input_answer_is_printed_as_text() {
            require_tools();
            let dir = scratch_dir("newline-answer");
            // input/password print the text: the reply is one JSON line, so a
            // newline inside the answer arrives escaped and must be decoded.
            let run = run_helper(
                &dir,
                Some(&[json!("line one\nline two")]),
                &capture("input \"Notes?\""),
                false,
            );
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
            assert_eq!(
                run.other_lines(),
                vec!["v=[line one", "line two] rc=0"],
                "{}\n{}",
                run.stdout,
                run.stderr
            );
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn password_answer_never_reaches_stderr() {
            require_tools();
            let dir = scratch_dir("password-stderr");
            // The terminal path echoes asterisks and the prompt to stderr; on
            // the protocol path nothing about the secret may surface there,
            // and the request itself carries no default that could leak one.
            let secret = "hunter2-x7x";
            let run = run_helper(
                &dir,
                Some(&[json!(secret)]),
                &capture("password \"Token?\""),
                false,
            );
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            let requests = run.request_lines();
            assert_eq!(requests.len(), 1, "{}", run.stdout);
            assert_eq!(requests[0]["coyote_prompt"]["action"], "password");
            assert!(
                !requests[0].to_string().contains(secret),
                "the request precedes the answer and cannot contain it: {}",
                requests[0]
            );
            assert_eq!(run.other_lines(), vec![format!("v=[{secret}] rc=0")]);
            assert!(
                run.stderr.is_empty(),
                "protocol password path must write nothing to stderr: {:?}",
                run.stderr
            );
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn editor_and_range_stay_terminal_only() {
            require_tools();
            let dir = scratch_dir("terminal-only");
            // `editor` and `range` stay terminal-only. With a live
            // channel and no tty they must still not emit a protocol request;
            // `range` falls through to its current value on EOF and `editor`
            // returns an empty buffer. `timeout` is only a hang guard.
            let body = "export EDITOR=true; \
                        r=\"$(timeout 10 bash -c 'source \"$PROMPT_UTILS\"; range \"How many?\" 1 3 5' 2>/dev/null)\"; echo \"range=[$r] rc=$?\"; \
                        e=\"$(timeout 10 bash -c 'source \"$PROMPT_UTILS\"; editor \"Notes?\"' 2>/dev/null)\"; echo \"editor=[$e] rc=$?\"";
            let run = run_helper(&dir, Some(&[json!(true)]), body, true);
            assert!(
                run.request_lines().is_empty(),
                "editor/range must never use the prompt channel: {}",
                run.stdout
            );
            let printed = run.other_lines();
            assert_eq!(printed.len(), 2, "{}\n{}", run.stdout, run.stderr);
            assert!(
                printed[0].starts_with("range=[") && !printed[0].contains("rc=124"),
                "range must terminate without a tty: {}\n{}",
                run.stdout,
                run.stderr
            );
            assert!(
                printed[1].starts_with("editor=[") && !printed[1].contains("rc=124"),
                "editor must terminate without a tty: {}\n{}",
                run.stdout,
                run.stderr
            );
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn an_unopenable_reply_path_fails_closed_without_hanging() {
            require_tools();
            let dir = scratch_dir("bad-reply-path");
            // A stale or broken COYOTE_PROMPT_REPLY (an fd the child did not
            // inherit) is "fail closed for prompting": the helper returns 1
            // and a guard aborts with its usual message, and nothing blocks.
            // The outer shell keeps the real channel so a request that does
            // escape is still answered rather than deadlocking the harness.
            let body = format!(
                "{}; \
                 COYOTE_PROMPT_REPLY=/dev/fd/97 timeout 10 bash -c '{}'; echo \"outer rc=$?\"; \
                 COYOTE_PROMPT_REPLY=/dev/fd/97 timeout 10 bash -c 'source \"$PROMPT_UTILS\"; guard_operation \"Really?\"; echo continued'; echo \"guard rc=$?\"",
                capture("confirm \"Warm up?\""),
                "source \"$PROMPT_UTILS\"; v=\"$(confirm \"Go?\")\"; rc=$?; echo \"v=[$v] rc=$rc\"; exit 0",
            );
            let started = std::time::Instant::now();
            let run = run_helper(
                &dir,
                Some(&[json!(true), json!(true), json!(true)]),
                &body,
                false,
            );
            assert!(
                started.elapsed() < std::time::Duration::from_secs(9),
                "the helper must not block on an unopenable reply path: {:?}",
                started.elapsed()
            );
            let printed = run.other_lines();
            assert!(
                printed.contains(&"v=[1] rc=0"),
                "the warm-up on the real channel must work: {}\n{}",
                run.stdout,
                run.stderr
            );
            assert!(
                printed.contains(&"v=[] rc=1"),
                "confirm over a dead reply path must print nothing and return 1: {}\n{}",
                run.stdout,
                run.stderr
            );
            assert!(
                printed.contains(&"outer rc=0") && printed.contains(&"guard rc=1"),
                "{}\n{}",
                run.stdout,
                run.stderr
            );
            assert!(
                !run.stdout.contains("continued"),
                "the guarded body must not run: {}",
                run.stdout
            );
            assert!(
                run.stderr.contains("Operation aborted!"),
                "the guard must say why it stopped: {:?}",
                run.stderr
            );
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn no_channel_or_the_reserved_fd_keeps_the_helpers_on_the_terminal_path() {
            require_tools();
            let dir = scratch_dir("no-channel");
            // Without COYOTE_PROMPT_REPLY, or with it naming the fd the helper
            // reserves for its own request stream, the helpers must behave as
            // before the protocol existed: no request line, the terminal path
            // (which, with no tty, settles on the confirm default) and no hang.
            // The outer shell keeps the real channel so a request that does
            // escape is answered rather than deadlocking the harness.
            let inner =
                "source \"$PROMPT_UTILS\"; v=\"$(confirm \"Go?\")\"; echo \"{tag} v=[$v] rc=$?\"";
            let body = format!(
                "env -u {reply} -u {out} timeout 10 bash -c '{unset}' 2>/dev/null; echo \"unset outer=$?\"; \
                 {reply}=/dev/fd/9 env -u {out} timeout 10 bash -c '{fd9}' 2>/dev/null; echo \"fd9 outer=$?\"",
                reply = REPLY_ENV,
                out = OUT_FD_ENV,
                unset = inner.replace("{tag}", "unset"),
                fd9 = inner.replace("{tag}", "fd9"),
            );
            let run = run_helper(&dir, Some(&[json!(true), json!(true)]), &body, true);
            assert!(
                run.request_lines().is_empty(),
                "no channel means no protocol request: {}",
                run.stdout
            );
            let printed = run.other_lines();
            assert_eq!(
                printed,
                vec![
                    "unset v=[0] rc=0",
                    "unset outer=0",
                    "fd9 v=[0] rc=0",
                    "fd9 outer=0"
                ],
                "{}\n{}",
                run.stdout,
                run.stderr
            );
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn replies_of_the_wrong_shape_are_unusable_for_every_helper() {
            require_tools();
            let dir = scratch_dir("wrong-shape");
            // The reply must fit the action it answers: a string is not a
            // confirm, an index is not a select, a bare string is not a
            // checkbox and a number or boolean is not text. Each helper prints
            // nothing, returns 1 and says why on stderr, so callers fail closed
            // exactly as they do for null.
            let cases: Vec<(&str, Value)> = vec![
                ("confirm \"Go?\"", json!("yes")),
                ("confirm \"Go?\"", json!(1)),
                ("list \"Pick\" a b", json!(0)),
                ("list \"Pick\" a b", json!(["a"])),
                ("checkbox \"Pick\" a b", json!("a")),
                ("checkbox \"Pick\" a b", json!(true)),
                ("input \"Name?\"", json!(42)),
                ("input \"Name?\"", json!(["x"])),
                ("password \"Secret?\"", json!(true)),
                ("password \"Secret?\"", json!({"text": "x"})),
            ];
            for (call, answer) in &cases {
                let run = run_helper(
                    &dir,
                    Some(std::slice::from_ref(answer)),
                    &capture(call),
                    false,
                );
                assert_eq!(
                    run.request_lines().len(),
                    1,
                    "{call} <- {answer}: {}",
                    run.stdout
                );
                assert_eq!(
                    run.other_lines(),
                    vec!["v=[] rc=1"],
                    "{call} <- {answer}: {}\n{}",
                    run.stdout,
                    run.stderr
                );
                assert!(
                    run.stderr.contains("prompt reply unusable"),
                    "{call} <- {answer}: {}",
                    run.stderr
                );
                if call.starts_with("password") {
                    assert!(
                        !run.stderr.contains("\"x\"") && !run.stderr.contains("true"),
                        "{call}: a rejected password reply must not be echoed: {}",
                        run.stderr
                    );
                }
            }
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn each_helper_consumes_exactly_one_reply_line() {
            require_tools();
            let dir = scratch_dir("one-line");
            // A reply the helper did not ask for sits behind every real one.
            // The second helper must skip the first decoy and stop at its own
            // reply; the decoy behind that must still be in the pipe when the
            // tool reads the channel itself, and nothing may follow it.
            let body = "a=\"$(confirm \"One?\")\"; b=\"$(list \"Two?\" a b c)\"; \
                        IFS= read -r -t 5 rest <\"$COYOTE_PROMPT_REPLY\"; rc=$?; \
                        IFS= read -r -t 1 more <\"$COYOTE_PROMPT_REPLY\"; \
                        echo \"a=[$a] b=[$b] rc=$rc more=[$more]\"; echo \"rest=$rest\"";
            let run = run_helper_opts(
                &dir,
                Some(&[json!(true), json!("b")]),
                body,
                false,
                Stray::After,
            );
            assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
            assert_eq!(run.request_lines().len(), 2, "{}", run.stdout);
            let lines = run.other_lines();
            assert_eq!(lines.len(), 2, "{}\n{}", run.stdout, run.stderr);
            assert_eq!(lines[0], "a=[1] b=[1] rc=0 more=[]", "{}", run.stderr);
            let rest: Value = serde_json::from_str(lines[1].trim_start_matches("rest="))
                .unwrap_or_else(|err| panic!("leftover is not the decoy ({err}): {}", lines[1]));
            assert_eq!(rest["id"], json!("not-this-request"), "{}", run.stdout);
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn strays_and_junk_before_a_closed_channel_still_fail_closed() {
            require_tools();
            let dir = scratch_dir("stray-eof");
            // The id-matching loop must give up at EOF rather than spin or
            // take a foreign reply: a stray id, a line that is not JSON and
            // then a closed channel leave every helper on the cancel path.
            for call in [
                "confirm \"Go?\"",
                "list \"Pick\" a b",
                "checkbox \"Pick\" a b",
                "input \"Name?\"",
                "password \"Secret?\"",
            ] {
                let run = run_helper_opts(&dir, Some(&[]), &capture(call), false, Stray::ThenClose);
                assert_eq!(run.request_lines().len(), 1, "{call}: {}", run.stdout);
                assert_eq!(
                    run.other_lines(),
                    vec!["v=[] rc=1"],
                    "{call}: {}\n{}",
                    run.stdout,
                    run.stderr
                );
                assert!(
                    run.stderr.contains("prompt reply channel closed"),
                    "{call}: {}",
                    run.stderr
                );
            }
            let run = run_helper_opts(
                &dir,
                Some(&[]),
                "guard_operation \"Really?\"; echo continued",
                false,
                Stray::ThenClose,
            );
            assert_eq!(run.code, 1, "{}\n{}", run.stdout, run.stderr);
            assert!(run.stderr.contains("Operation aborted!"), "{}", run.stderr);
            assert!(!run.stdout.contains("continued"), "{}", run.stdout);
            fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn a_reply_longer_than_the_request_carry_cap_is_printed_whole() {
            require_tools();
            let dir = scratch_dir("big-reply");
            // The 64 KiB carry cap bounds request lines on the host side; the
            // reply path has no such limit and the helper prints all of it.
            let big = "x".repeat(CARRY_CAP + 6_000);
            let run = run_helper(
                &dir,
                Some(&[json!(big)]),
                "v=\"$(input \"Big?\")\"; rc=$?; echo \"len=${#v} rc=$rc\"",
                false,
            );
            assert_eq!(run.request_lines().len(), 1, "{}", run.stdout);
            assert_eq!(
                run.other_lines(),
                vec![format!("len={} rc=0", CARRY_CAP + 6_000).as_str()],
                "{}",
                run.stderr
            );
            fs::remove_dir_all(&dir).ok();
        }
    }

    /// Python and TypeScript tools reach the channel through their own
    /// helpers, loaded by path from the env vars the shims export. The
    /// fixture and cancel tests go through the real shim builder; the
    /// contract modules drive each helper directly, the way `bash_contract`
    /// does for the shell one.
    mod script_clients {
        use super::*;
        use std::path::{Path, PathBuf};
        use std::process::{Child, ExitStatus, Stdio};
        use std::time::{SystemTime, UNIX_EPOCH};

        const PY_HELPER: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/functions/utils/prompt-utils.py"
        );
        const TS_HELPER: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/functions/utils/prompt-utils.ts"
        );

        fn scratch_dir(tag: &str) -> PathBuf {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "coyote-prompt-script-{tag}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn python_available() -> bool {
            if which::which("python3").is_err() {
                eprintln!("skipping: python3 not available");
                return false;
            }
            true
        }

        /// The command that runs a `.ts` file: `node` when it strips types
        /// itself without a word on stderr (the contract cases require it
        /// empty), else `tsx`; None, with the reason printed, when neither.
        fn ts_runtime() -> Option<&'static str> {
            let dir = scratch_dir("ts-probe");
            let probe = dir.join("probe.ts");
            fs::write(&probe, "const n: number = 1;\nconsole.log(n);\n").unwrap();
            let run_probe = || {
                Command::new("node")
                    .arg(&probe)
                    .env("NODE_NO_WARNINGS", "1")
                    .stdin(Stdio::null())
                    .output()
            };
            let clean = |out: &std::io::Result<std::process::Output>| {
                out.as_ref()
                    .is_ok_and(|out| out.status.success() && out.stderr.is_empty())
            };
            // A cold node start can print a one-off warning; one retry absorbs it.
            let mut probe_result = run_probe();
            if !clean(&probe_result) {
                probe_result = run_probe();
            }
            fs::remove_dir_all(&dir).ok();
            if clean(&probe_result) {
                return Some("node");
            }
            if which::which("tsx").is_ok() {
                return Some("tsx");
            }
            let (status, stderr) = match &probe_result {
                Ok(out) => (
                    out.status.to_string(),
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                ),
                Err(err) => ("not spawned".to_string(), err.to_string()),
            };
            eprintln!(
                "skipping: node cannot run TypeScript cleanly ({status}; stderr: {stderr}) and tsx is not installed"
            );
            None
        }

        /// Copies a shipped helper into the active config's utils dir, where
        /// the shim's env var points, writes `body` as a tool source and has
        /// the real shim builder turn it into a runnable binary.
        fn install_script_tool(helper: &str, tool_file: &str, body: &str) {
            let utils = crate::config::paths::global_utils_dir();
            fs::create_dir_all(&utils).unwrap();
            fs::copy(helper, utils.join(Path::new(helper).file_name().unwrap())).unwrap();
            let tools = crate::config::paths::global_tools_dir();
            fs::create_dir_all(&tools).unwrap();
            fs::write(tools.join(tool_file), body).unwrap();
            fs::create_dir_all(crate::config::paths::functions_bin_dir()).unwrap();
            crate::function::Functions::build_global_function_binary(tool_file, None).unwrap();
        }

        /// Like `run_tool`, with the JSON argument the shims insist on.
        fn run_shim_tool(name: &str, io: ToolIo) -> Value {
            let result = run_llm_function(
                name.to_string(),
                vec!["{}".to_string()],
                tool_envs(),
                None,
                None,
                io,
                None,
            )
            .unwrap()
            .unwrap_or_default();
            match serde_json::from_str::<Value>(&result) {
                Ok(error) if error.get("tool_call_error").is_some() => error,
                _ => json!({"output": result}),
            }
        }

        fn round_trip_prompter() -> Arc<ScriptedPrompter> {
            Arc::new(ScriptedPrompter::new(vec![
                PromptAnswer::Confirmed(true),
                PromptAnswer::Selected("b".to_string()),
                PromptAnswer::Checked(vec!["a".to_string(), "c".to_string()]),
                PromptAnswer::Text("hello".to_string()),
                PromptAnswer::Text("s3cret".to_string()),
            ]))
        }

        /// Reaps `child`, killing it after 10 s so a wedged helper fails the
        /// test instead of hanging it. Drain the child's pipes first.
        fn wait_or_kill(child: &mut Child, what: &str) -> ExitStatus {
            let started = std::time::Instant::now();
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    return status;
                }
                if started.elapsed() > std::time::Duration::from_secs(10) {
                    child.kill().ok();
                    child.wait().ok();
                    panic!("{what}: child still running after 10 s");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }

        /// Hands each stdout line of `child` to `on_line` from a forwarding
        /// thread, so a helper that has not closed its stdout within 10 s of
        /// starting is killed instead of parking the test on a blocking read.
        fn pump_stdout(child: &mut Child, what: &str, mut on_line: impl FnMut(String)) {
            let stdout = child.stdout.take().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let reader = std::thread::spawn(move || {
                for line in std::io::BufReader::new(stdout).lines() {
                    if tx.send(line.unwrap()).is_err() {
                        break;
                    }
                }
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                match rx.recv_timeout(remaining) {
                    Ok(line) => on_line(line),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        child.kill().ok();
                        child.wait().ok();
                        panic!("{what}: the child did not finish within 10 s");
                    }
                }
            }
            reader.join().unwrap();
        }

        fn assert_round_trip(
            tool: &str,
            prompter: &ScriptedPrompter,
            sink: &RecordingSink,
            output: &Value,
            expected: &str,
        ) {
            assert_eq!(output["output"], expected, "{output}");
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
                assert_eq!(req.tool.as_deref(), Some(tool));
                assert_eq!(req.call_id.as_deref(), Some("call-x7x"));
                assert_eq!(req.job_id, None);
            }
            assert_eq!(requests[0].question, format!("Delete? {tool}"));
            assert!(requests[0].options.is_empty());
            assert_eq!(requests[1].options, vec!["a", "b", "c"]);
            assert_eq!(requests[2].options, vec!["a", "b", "c"]);
            let chunks = stdout_chunks(&sink.drain(), "call-x7x");
            assert!(!chunks.contains("coyote_prompt"), "{chunks}");
        }

        fn assert_cancelled(output: &Value, prompter: &ScriptedPrompter, question: &str) {
            assert!(
                output["tool_call_error"]
                    .as_str()
                    .unwrap_or_default()
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
            assert_eq!(prompter.requests()[0].question, question);
        }

        const PY_IMPORT: &str = "import importlib.util, os, sys\n\
            sys.dont_write_bytecode = True\n\
            spec = importlib.util.spec_from_file_location(\"prompt_utils\", os.environ[\"LLM_PROMPT_UTILS_PY\"])\n\
            prompt_utils = importlib.util.module_from_spec(spec); spec.loader.exec_module(prompt_utils)\n";

        fn ts_tool(runtime: &str, body: &str) -> String {
            let shebang = if runtime == "node" {
                "#!/usr/bin/env node\n"
            } else {
                ""
            };
            format!(
                "{shebang}import {{ pathToFileURL }} from \"node:url\";\n\
                 export async function run(): Promise<string> {{\n\
                 \x20 const prompt = await import(pathToFileURL(process.env.LLM_PROMPT_UTILS_TS!).href);\n\
                 {body}\n\
                 }}\n"
            )
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[serial]
        async fn python_tool_round_trips_every_action_through_the_shim() {
            if !python_available() {
                return;
            }
            let guard = TestConfigDirGuard::new("prompt-py");
            let _functions = crate::testing::EnvVarGuard::set(
                crate::utils::get_env_name("functions_dir"),
                guard.path.join("functions"),
            );
            install_script_tool(
                PY_HELPER,
                "ask-py-x7x.py",
                &format!(
                    "#!/usr/bin/env python3\n{PY_IMPORT}\n\
                     def run(**kwargs):\n\
                     \x20   answers = [\n\
                     \x20       prompt_utils.confirm(\"Delete? ask-py-x7x\"),\n\
                     \x20       prompt_utils.select(\"Pick one ask-py-x7x\", [\"a\", \"b\", \"c\"]),\n\
                     \x20       prompt_utils.checkbox(\"Pick many ask-py-x7x\", [\"a\", \"b\", \"c\"]),\n\
                     \x20       prompt_utils.input(\"Name? ask-py-x7x\"),\n\
                     \x20       prompt_utils.password(\"Secret? ask-py-x7x\"),\n\
                     \x20   ]\n\
                     \x20   return \"\".join(f\"{{answer}}\\n\" for answer in answers)\n"
                ),
            );
            let prompter = round_trip_prompter();
            let sink = RecordingSink::install();

            let output = run_shim_tool(
                "ask-py-x7x",
                io_with(&prompter, Arc::new(PromptMemory::default())),
            );

            // Python reprs: bool, index, list of indices, then the two texts.
            assert_round_trip(
                "ask-py-x7x",
                &prompter,
                &sink,
                &output,
                "True\n1\n[0, 2]\nhello\ns3cret\n",
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[serial]
        async fn typescript_tool_round_trips_every_action_through_the_shim() {
            let Some(runtime) = ts_runtime() else {
                return;
            };
            let guard = TestConfigDirGuard::new("prompt-ts");
            let _functions = crate::testing::EnvVarGuard::set(
                crate::utils::get_env_name("functions_dir"),
                guard.path.join("functions"),
            );
            install_script_tool(
                TS_HELPER,
                "ask-ts-x7x.ts",
                &ts_tool(
                    runtime,
                    "  const answers = [\n\
                     \x20   prompt.confirm(\"Delete? ask-ts-x7x\"),\n\
                     \x20   prompt.select(\"Pick one ask-ts-x7x\", [\"a\", \"b\", \"c\"]),\n\
                     \x20   prompt.checkbox(\"Pick many ask-ts-x7x\", [\"a\", \"b\", \"c\"]),\n\
                     \x20   prompt.input(\"Name? ask-ts-x7x\"),\n\
                     \x20   prompt.password(\"Secret? ask-ts-x7x\"),\n\
                     \x20 ];\n\
                     \x20 return answers.map((a) => `${JSON.stringify(a)}\\n`).join(\"\");",
                ),
            );
            let prompter = round_trip_prompter();
            let sink = RecordingSink::install();

            let output = run_shim_tool(
                "ask-ts-x7x",
                io_with(&prompter, Arc::new(PromptMemory::default())),
            );

            // JSON: bool, index, array of indices, then the two quoted texts.
            assert_round_trip(
                "ask-ts-x7x",
                &prompter,
                &sink,
                &output,
                "true\n1\n[0,2]\n\"hello\"\n\"s3cret\"\n",
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[serial]
        async fn cancel_fails_the_guarded_python_tool_closed() {
            if !python_available() {
                return;
            }
            let guard = TestConfigDirGuard::new("prompt-py-cancel");
            let _functions = crate::testing::EnvVarGuard::set(
                crate::utils::get_env_name("functions_dir"),
                guard.path.join("functions"),
            );
            install_script_tool(
                PY_HELPER,
                "cancel-py-x7x.py",
                &format!(
                    "#!/usr/bin/env python3\n{PY_IMPORT}\n\
                     def run(**kwargs):\n\
                     \x20   os.environ.pop(\"AUTO_CONFIRM\", None)\n\
                     \x20   os.environ.pop(\"LLM_AGENT_VAR_AUTO_CONFIRM\", None)\n\
                     \x20   prompt_utils.guard_operation(\"Delete? cancel-py-x7x\")\n\
                     \x20   return \"reached\\n\"\n"
                ),
            );
            let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Cancelled]));

            let output = run_shim_tool(
                "cancel-py-x7x",
                io_with(&prompter, Arc::new(PromptMemory::default())),
            );

            assert_cancelled(&output, &prompter, "Delete? cancel-py-x7x");
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        #[serial]
        async fn cancel_fails_the_guarded_typescript_tool_closed() {
            let Some(runtime) = ts_runtime() else {
                return;
            };
            let guard = TestConfigDirGuard::new("prompt-ts-cancel");
            let _functions = crate::testing::EnvVarGuard::set(
                crate::utils::get_env_name("functions_dir"),
                guard.path.join("functions"),
            );
            install_script_tool(
                TS_HELPER,
                "cancel-ts-x7x.ts",
                &ts_tool(
                    runtime,
                    "  delete process.env[\"AUTO_CONFIRM\"];\n\
                     \x20 delete process.env[\"LLM_AGENT_VAR_AUTO_CONFIRM\"];\n\
                     \x20 prompt.guardOperation(\"Delete? cancel-ts-x7x\");\n\
                     \x20 return \"reached\\n\";",
                ),
            );
            let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Cancelled]));

            let output = run_shim_tool(
                "cancel-ts-x7x",
                io_with(&prompter, Arc::new(PromptMemory::default())),
            );

            assert_cancelled(&output, &prompter, "Delete? cancel-ts-x7x");
        }

        struct Run {
            /// Stdout minus the request lines.
            printed: Vec<String>,
            requests: usize,
            /// The `coyote_prompt` object of every request line, in order.
            wire: Vec<Value>,
            stderr: String,
            code: i32,
        }

        /// Runs `cmd` with a live reply channel; each request line it writes
        /// is answered from `answers` in order (`null` once they run out,
        /// `None` closes the channel before the first read), preceded by a
        /// foreign-id reply when `stray_first` is set.
        fn drive(cmd: Command, answers: Option<&[Value]>, stray_first: bool) -> Run {
            drive_env(cmd, answers, stray_first, &[])
        }

        /// `drive` with extra environment set after the auto-confirm
        /// variables are scrubbed, so a case can opt back into one of them.
        fn drive_env(
            mut cmd: Command,
            answers: Option<&[Value]>,
            stray_first: bool,
            env: &[(&str, &str)],
        ) -> Run {
            let mut server = Server::open().unwrap();
            cmd.env_remove("AUTO_CONFIRM")
                .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
                .env_remove(OUT_FD_ENV)
                .envs(env.iter().copied())
                .env(REPLY_ENV, server.env_value())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            server.configure(&mut cmd);
            let mut child = cmd.spawn().unwrap();
            server.spawned();
            let server = answers.is_some().then_some(server);
            let mut stderr = child.stderr.take().unwrap();
            let stderr_thread = std::thread::spawn(move || {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
                text
            });
            let mut answers = answers.unwrap_or_default().iter();
            let mut printed = Vec::new();
            let mut requests = 0;
            let mut wire = Vec::new();
            pump_stdout(&mut child, "drive_env", |line| {
                if line.trim_start().as_bytes().starts_with(MARKER) {
                    requests += 1;
                    let req: Value = serde_json::from_str(&line).unwrap();
                    wire.push(req["coyote_prompt"].clone());
                    if let Some(server) = server.as_ref() {
                        let id = req["coyote_prompt"]["id"].as_str().unwrap();
                        if stray_first {
                            server.reply("not-this-request", &json!(false));
                        }
                        server.reply(id, answers.next().unwrap_or(&Value::Null));
                    }
                } else {
                    printed.push(line);
                }
            });
            let code = wait_or_kill(&mut child, "drive_env").code().unwrap_or(-1);
            Run {
                printed,
                requests,
                wire,
                stderr: stderr_thread.join().unwrap(),
                code,
            }
        }

        /// Runs `cmd` with no reply channel in its environment and no
        /// controlling terminal (its own session, stdin at /dev/null), the
        /// shape of a headless run, and counts any request line it emits.
        fn drive_without_channel(mut cmd: Command) -> Run {
            cmd.env_remove("AUTO_CONFIRM")
                .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
                .env_remove(OUT_FD_ENV)
                .env_remove(REPLY_ENV)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // SAFETY: setsid is async-signal-safe and touches no shared state.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let out = cmd.output().unwrap();
            let mut printed = Vec::new();
            let mut requests = 0;
            let mut wire = Vec::new();
            for line in out.stdout.lines() {
                let line = line.unwrap();
                if line.trim_start().as_bytes().starts_with(MARKER) {
                    requests += 1;
                    let req: Value = serde_json::from_str(&line).unwrap();
                    wire.push(req["coyote_prompt"].clone());
                } else {
                    printed.push(line);
                }
            }
            Run {
                printed,
                requests,
                wire,
                code: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8(out.stderr).unwrap(),
            }
        }

        /// Without a channel or a terminal a helper must stay silent on
        /// stdout (no request line) and give the missing answer.
        fn check_no_channel(run: Run, none: &str) {
            assert_eq!(run.requests, 0, "request sent without a channel");
            assert_eq!(run.code, 0, "{}", run.stderr);
            assert_eq!(run.printed, vec![none], "{}", run.stderr);
        }

        /// What a language prints for the two booleans, a missing answer
        /// and the checkbox indices; everything else prints the same.
        struct Contract {
            confirm_true: &'static str,
            confirm_false: &'static str,
            none: &'static str,
            checked: &'static str,
        }

        fn check_contract(eval: impl Fn(&str, Option<&[Value]>, bool) -> Run, shape: &Contract) {
            let confirm = "confirm(\"Go?\")";
            for (answer, printed) in [(true, shape.confirm_true), (false, shape.confirm_false)] {
                let run = eval(confirm, Some(&[json!(answer)]), false);
                assert_eq!(run.code, 0, "{}", run.stderr);
                assert_eq!(run.requests, 1);
                assert_eq!(run.printed, vec![printed]);
                assert!(run.stderr.is_empty(), "{}", run.stderr);
            }

            let select = "select(\"Pick\", [\"a\", \"b\", \"c\"])";
            let run = eval(select, Some(&[json!("b")]), false);
            assert_eq!(run.printed, vec!["1"], "{}", run.stderr);

            let run = eval(
                "checkbox(\"Pick\", [\"a\", \"b\", \"c\"])",
                Some(&[json!(["c", "a"])]),
                false,
            );
            assert_eq!(run.printed, vec![shape.checked], "{}", run.stderr);

            let run = eval(confirm, Some(&[Value::Null]), false);
            assert_eq!(run.printed, vec![shape.none], "{}", run.stderr);
            assert!(run.stderr.is_empty(), "cancel is silent: {}", run.stderr);

            let run = eval(confirm, Some(&[json!(true)]), true);
            assert_eq!(run.printed, vec![shape.confirm_true], "{}", run.stderr);

            let run = eval(select, Some(&[json!("zzz")]), false);
            assert_eq!(run.printed, vec![shape.none], "{}", run.stderr);
            assert!(
                run.stderr.contains("prompt reply unusable"),
                "{}",
                run.stderr
            );

            let run = eval(confirm, None, false);
            assert_eq!(run.printed, vec![shape.none], "{}", run.stderr);
            assert!(
                run.stderr.contains("prompt reply channel closed"),
                "{}",
                run.stderr
            );
        }

        /// One guard call evaluated with `env` added and `cwd` as the working
        /// directory; `continued` is what the language prints for the guard's
        /// return value when it lets the tool carry on.
        type GuardEval<'a> = &'a dyn Fn(&str, Option<&[Value]>, &[(&str, &str)], &Path) -> Run;

        /// The bash guards' contract, mirrored: the auto-confirm variables
        /// short-circuit before any request is sent, anything but an exact
        /// `true` aborts with exit 1 and `Operation aborted!` on stderr, and
        /// `guard_path` stays silent for paths inside the working directory
        /// (by components, so a sibling sharing the prefix still asks, and
        /// by where a path really lands, so a new file under a symlink that
        /// points outside asks too).
        fn check_guards(eval: GuardEval<'_>, continued: &str, cwd: &Path) {
            fs::create_dir_all(cwd.join("sub")).unwrap();
            let sibling = PathBuf::from(format!("{}-sibling", cwd.display()));
            fs::create_dir_all(&sibling).unwrap();
            std::os::unix::fs::symlink(&sibling, cwd.join("link")).unwrap();
            std::os::unix::fs::symlink(sibling.join("missing.txt"), cwd.join("dangling")).unwrap();
            let op = "guard_operation(\"Really?\")";

            let run = eval(op, Some(&[json!(true)]), &[], cwd);
            assert_eq!(run.code, 0, "{}", run.stderr);
            assert_eq!(run.requests, 1);
            assert_eq!(run.printed, vec![continued]);
            assert!(run.stderr.is_empty(), "{}", run.stderr);

            for (label, answers) in [
                ("false", Some(&[json!(false)][..])),
                ("cancelled", Some(&[Value::Null][..])),
                ("string yes", Some(&[json!("yes")][..])),
                ("channel closed", None),
            ] {
                let run = eval(op, answers, &[], cwd);
                assert_eq!(run.code, 1, "{label}: {}", run.stderr);
                assert_eq!(run.requests, 1, "{label}");
                assert!(run.printed.is_empty(), "{label}: {:?}", run.printed);
                assert!(
                    run.stderr.contains("Operation aborted!"),
                    "{label}: {}",
                    run.stderr
                );
            }

            for var in ["AUTO_CONFIRM", "LLM_AGENT_VAR_AUTO_CONFIRM"] {
                let run = eval(op, Some(&[json!(false)]), &[(var, "1")], cwd);
                assert_eq!(run.code, 0, "{var}: {}", run.stderr);
                assert_eq!(
                    run.requests, 0,
                    "{var} must short-circuit before any request"
                );
                assert_eq!(run.printed, vec![continued], "{var}");
                assert!(run.stderr.is_empty(), "{var}: {}", run.stderr);
            }

            let inside = [
                "sub/file.txt".to_string(),
                "sub/../sub/./x".to_string(),
                ".".to_string(),
                cwd.join("sub").display().to_string(),
            ];
            for path in &inside {
                let run = eval(
                    &format!("guard_path(\"{path}\", \"Touch?\")"),
                    Some(&[json!(false)]),
                    &[],
                    cwd,
                );
                assert_eq!(run.code, 0, "{path}: {}", run.stderr);
                assert_eq!(run.requests, 0, "{path} is inside cwd and must not ask");
                assert_eq!(run.printed, vec![continued], "{path}");
            }

            let outside = [
                "../outside.txt".to_string(),
                "sub/../../x".to_string(),
                format!("{}-sibling/f", cwd.display()),
                "/".to_string(),
                "link/new.txt".to_string(),
                "link/../escaped.txt".to_string(),
                "dangling".to_string(),
            ];
            for path in &outside {
                let expr = format!("guard_path(\"{path}\", \"Touch?\")");
                let run = eval(&expr, Some(&[json!(true)]), &[], cwd);
                assert_eq!(run.code, 0, "{path}: {}", run.stderr);
                assert_eq!(run.requests, 1, "{path} is outside cwd and must ask");
                assert_eq!(run.printed, vec![continued], "{path}");

                let run = eval(&expr, Some(&[json!(false)]), &[], cwd);
                assert_eq!(run.code, 1, "{path}: {}", run.stderr);
                assert!(
                    run.stderr.contains("Operation aborted!"),
                    "{path}: {}",
                    run.stderr
                );

                let run = eval(&expr, Some(&[json!(false)]), &[("AUTO_CONFIRM", "1")], cwd);
                assert_eq!(run.code, 0, "{path}: {}", run.stderr);
                assert_eq!(
                    run.requests, 0,
                    "{path}: AUTO_CONFIRM must skip the question"
                );
            }
            fs::remove_dir_all(&sibling).ok();
        }

        /// Replies that fit no action: a string for `confirm`, an unknown
        /// option inside a `checkbox` list, a number for `input`. Each prints
        /// the missing answer and names the problem on stderr.
        fn check_unusable_shapes(eval: impl Fn(&str, Option<&[Value]>, bool) -> Run, none: &str) {
            for (expr, answer) in [
                ("confirm(\"Go?\")", json!("yes")),
                (
                    "checkbox(\"Pick\", [\"a\", \"b\", \"c\"])",
                    json!(["a", "zzz"]),
                ),
                ("input(\"Name?\")", json!(42)),
                ("select(\"Pick\", [\"a\", \"b\", \"c\"])", json!(1)),
            ] {
                let run = eval(expr, Some(std::slice::from_ref(&answer)), false);
                assert_eq!(run.code, 0, "{expr}: {}", run.stderr);
                assert_eq!(run.printed, vec![none], "{expr} with {answer}");
                assert!(
                    run.stderr.contains("prompt reply unusable"),
                    "{expr}: {}",
                    run.stderr
                );
            }
        }

        /// The request line carries the tool's text verbatim however awkward
        /// it is (quotes, a newline, non-ASCII, a backslash), as one JSON
        /// line the host can parse, and every prompt in a process mints a
        /// fresh `p<pid>-<16 hex>` id. `second` is how the language names
        /// the helper module for a second call inside one expression.
        fn check_wire(
            eval: impl Fn(&str, Option<&[Value]>, bool) -> Run,
            second: &str,
            text_printed: &str,
            one: &str,
        ) {
            let question = "He said \"hi\"\nline2 \u{e9}\u{2713} \\ back";
            let run = eval(
                "input(\"He said \\\"hi\\\"\\nline2 \\u00e9\\u2713 \\\\ back\")",
                Some(&[json!("x")]),
                false,
            );
            assert_eq!(run.code, 0, "{}", run.stderr);
            assert_eq!(run.requests, 1, "{:?}", run.wire);
            assert_eq!(run.wire[0]["action"], "input");
            assert_eq!(run.wire[0]["question"], question, "{}", run.wire[0]);
            assert_eq!(run.wire[0]["options"], json!([]));
            assert_eq!(run.printed, vec![text_printed], "{}", run.stderr);

            let run = eval(
                "select(\"Pick\", [\"a\", \"b\\\"q\", \"\\u00e9\"])",
                Some(&[json!("b\"q")]),
                false,
            );
            assert_eq!(run.code, 0, "{}", run.stderr);
            assert_eq!(run.wire[0]["options"], json!(["a", "b\"q", "\u{e9}"]));
            assert_eq!(run.printed, vec![one], "{}", run.stderr);

            let run = eval(
                &format!("confirm(\"A\"), {second}confirm(\"B\")"),
                Some(&[json!(true), json!(false)]),
                false,
            );
            assert_eq!(run.code, 0, "{}", run.stderr);
            assert_eq!(run.requests, 2, "{:?}", run.wire);
            let ids: Vec<&str> = run
                .wire
                .iter()
                .map(|req| req["id"].as_str().unwrap())
                .collect();
            assert_ne!(ids[0], ids[1], "two prompts shared an id");
            for id in &ids {
                let (pid, hex) = id
                    .strip_prefix('p')
                    .and_then(|rest| rest.split_once('-'))
                    .unwrap_or_else(|| panic!("id {id} is not p<pid>-<hex>"));
                assert!(
                    !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()),
                    "{id}"
                );
                assert!(
                    hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
                    "{id}"
                );
            }
            assert_eq!(
                ids[0].split_once('-').unwrap().0,
                ids[1].split_once('-').unwrap().0
            );
            assert_eq!(run.wire[0]["question"], "A");
            assert_eq!(run.wire[1]["question"], "B");
        }

        /// `inner`, run by a bash tool inside `$(...)` after it sourced
        /// prompt-utils.sh: the request must leave through the fd the bash
        /// helper saved (`_COYOTE_PROMPT_OUT`), not the capture, and the
        /// helper's answer must be what the capture receives.
        fn nested_in_bash(inner: &Command) -> Command {
            fn quote(arg: &std::ffi::OsStr) -> String {
                format!("'{}'", arg.to_str().unwrap().replace('\'', "'\\''"))
            }
            let words: Vec<String> = std::iter::once(inner.get_program())
                .chain(inner.get_args())
                .map(quote)
                .collect();
            let mut cmd = Command::new(bash_program().unwrap());
            for (key, value) in inner.get_envs() {
                match value {
                    Some(value) => cmd.env(key, value),
                    None => cmd.env_remove(key),
                };
            }
            cmd.arg("-c").arg(format!(
                "source {}; out=\"$({})\"; echo \"captured: $out\"",
                quote(HELPER.as_ref()),
                words.join(" ")
            ));
            cmd
        }

        fn check_nested_in_bash(inner: &Command, one: &str) {
            let run = drive(nested_in_bash(inner), Some(&[json!("b")]), false);
            assert_eq!(run.code, 0, "{}", run.stderr);
            assert_eq!(run.requests, 1, "{:?}", run.wire);
            assert_eq!(run.wire[0]["action"], "select");
            assert_eq!(
                run.printed,
                vec![format!("captured: {one}")],
                "{}",
                run.stderr
            );
            assert!(run.stderr.is_empty(), "{}", run.stderr);
        }

        /// Like `drive`, but every reply is preceded, in the same write, by
        /// lines the helper must skip: text that is not JSON, a blank line,
        /// an object without an id and a reply for another id. The bash
        /// helper loops until `.id` matches; the twins must do the same.
        fn drive_through_junk(mut cmd: Command, answers: &[Value]) -> Run {
            let mut server = Server::open().unwrap();
            cmd.env_remove("AUTO_CONFIRM")
                .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
                .env_remove(OUT_FD_ENV)
                .env(REPLY_ENV, server.env_value())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            server.configure(&mut cmd);
            let mut child = cmd.spawn().unwrap();
            server.spawned();
            let mut stderr = child.stderr.take().unwrap();
            let stderr_thread = std::thread::spawn(move || {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
                text
            });
            let mut answers = answers.iter();
            let mut printed = Vec::new();
            let mut requests = 0;
            let mut wire = Vec::new();
            pump_stdout(&mut child, "drive_through_junk", |line| {
                if line.trim_start().as_bytes().starts_with(MARKER) {
                    requests += 1;
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let id = req["coyote_prompt"]["id"].as_str().unwrap().to_string();
                    wire.push(req["coyote_prompt"].clone());
                    let answer = answers.next().unwrap_or(&Value::Null);
                    let batch = format!(
                        "this is not json\n\n{{\"answer\":true}}\n\
                         {{\"id\":\"p1-0000000000000000\",\"answer\":false}}\n\
                         {}\n",
                        json!({"id": id, "answer": answer})
                    );
                    let mut writer = &server.writer;
                    writer.write_all(batch.as_bytes()).unwrap();
                    writer.flush().unwrap();
                } else {
                    printed.push(line);
                }
            });
            let code = wait_or_kill(&mut child, "drive_through_junk")
                .code()
                .unwrap_or(-1);
            Run {
                printed,
                requests,
                wire,
                stderr: stderr_thread.join().unwrap(),
                code,
            }
        }

        /// `drive_without_channel` with `COYOTE_PROMPT_REPLY` naming the fd
        /// the bash helper reserves for its saved stdout, which every helper
        /// treats as no channel at all.
        fn drive_with_reserved_fd(mut cmd: Command) -> Run {
            cmd.env_remove("AUTO_CONFIRM")
                .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
                .env_remove(OUT_FD_ENV)
                .env(REPLY_ENV, "/dev/fd/9")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // SAFETY: setsid is async-signal-safe and touches no shared state.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let out = cmd.output().unwrap();
            let printed = out
                .stdout
                .lines()
                .map(Result::unwrap)
                .filter(|line| !line.trim_start().as_bytes().starts_with(MARKER))
                .collect();
            let requests = out
                .stdout
                .lines()
                .map(Result::unwrap)
                .filter(|line| line.trim_start().as_bytes().starts_with(MARKER))
                .count();
            Run {
                printed,
                requests,
                wire: Vec::new(),
                code: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8(out.stderr).unwrap(),
            }
        }

        /// Edges of the wire contract the broad checks leave out: junk on
        /// the channel is skipped until our id; options are matched by text
        /// (a duplicate yields its first index, a dash or empty option is an
        /// option like any other); an empty checkbox list and an empty input
        /// are valid answers, not cancels; a number is unusable for confirm
        /// and a bare string for checkbox; `guard_operation` asks its default
        /// question when given none.
        fn check_edges(
            command: impl Fn(&str) -> Command,
            none: &str,
            confirm_true: &str,
            empty_list: &str,
            empty_text: &str,
        ) {
            let run = drive_through_junk(command("confirm(\"Go?\")"), &[json!(true)]);
            assert_eq!(run.code, 0, "{}", run.stderr);
            assert_eq!(run.requests, 1, "{:?}", run.wire);
            assert_eq!(run.printed, vec![confirm_true], "{}", run.stderr);
            assert!(run.stderr.is_empty(), "{}", run.stderr);

            for (expr, answer, printed) in [
                ("select(\"Pick\", [\"dup\", \"dup\"])", json!("dup"), "0"),
                (
                    "select(\"Pick\", [\"-x\", \"--y\", \"\"])",
                    json!("--y"),
                    "1",
                ),
                ("select(\"Pick\", [\"-x\", \"--y\", \"\"])", json!(""), "2"),
                ("checkbox(\"Pick\", [\"a\", \"b\"])", json!([]), empty_list),
                ("input(\"Name?\")", json!(""), empty_text),
            ] {
                let run = drive(command(expr), Some(std::slice::from_ref(&answer)), false);
                assert_eq!(run.code, 0, "{expr}: {}", run.stderr);
                assert_eq!(
                    run.printed,
                    vec![printed],
                    "{expr} with {answer}: {}",
                    run.stderr
                );
                assert!(run.stderr.is_empty(), "{expr}: {}", run.stderr);
            }

            for (expr, answer) in [
                ("confirm(\"Go?\")", json!(1)),
                ("checkbox(\"Pick\", [\"a\", \"b\"])", json!("a")),
            ] {
                let run = drive(command(expr), Some(std::slice::from_ref(&answer)), false);
                assert_eq!(run.code, 0, "{expr}: {}", run.stderr);
                assert_eq!(run.printed, vec![none], "{expr} with {answer}");
                assert!(
                    run.stderr.contains("prompt reply unusable"),
                    "{expr}: {}",
                    run.stderr
                );
            }

            let run = drive(command("guard_operation()"), Some(&[json!(true)]), false);
            assert_eq!(run.code, 0, "{}", run.stderr);
            assert_eq!(run.requests, 1, "{:?}", run.wire);
            assert_eq!(run.wire[0]["action"], "confirm");
            assert_eq!(
                run.wire[0]["question"], "Are you sure you want to continue?",
                "{}",
                run.wire[0]
            );

            check_no_channel(drive_with_reserved_fd(command("confirm(\"Go?\")")), none);
        }

        /// `drive` with a reply channel the host holds open, answering each
        /// request with the raw line `line(id)` instead of a well-formed
        /// reply, so a case can send an object the host never would.
        fn drive_raw(mut cmd: Command, line: impl Fn(&str) -> String) -> Run {
            let mut server = Server::open().unwrap();
            cmd.env_remove("AUTO_CONFIRM")
                .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
                .env_remove(OUT_FD_ENV)
                .env(REPLY_ENV, server.env_value())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            server.configure(&mut cmd);
            let mut child = cmd.spawn().unwrap();
            server.spawned();
            let mut stderr = child.stderr.take().unwrap();
            let stderr_thread = std::thread::spawn(move || {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
                text
            });
            let mut printed = Vec::new();
            let mut requests = 0;
            let mut wire = Vec::new();
            pump_stdout(&mut child, "drive_raw", |out| {
                if out.trim_start().as_bytes().starts_with(MARKER) {
                    requests += 1;
                    let req: Value = serde_json::from_str(&out).unwrap();
                    let id = req["coyote_prompt"]["id"].as_str().unwrap().to_string();
                    wire.push(req["coyote_prompt"].clone());
                    let mut writer = &server.writer;
                    writer.write_all(line(&id).as_bytes()).unwrap();
                    writer.write_all(b"\n").unwrap();
                } else {
                    printed.push(out);
                }
            });
            let code = wait_or_kill(&mut child, "drive_raw").code().unwrap_or(-1);
            Run {
                printed,
                requests,
                wire,
                stderr: stderr_thread.join().unwrap(),
                code,
            }
        }

        /// Runs `cmd` with `COYOTE_PROMPT_REPLY` naming `reply`, a path the
        /// helper cannot open, in its own session with no terminal. Nothing
        /// answers a request here, so a helper that sends one before it
        /// finds the channel dead would block; the child is killed after
        /// ten seconds and the exit code reports it.
        fn drive_with_dead_reply(mut cmd: Command, reply: &str) -> Run {
            cmd.env_remove("AUTO_CONFIRM")
                .env_remove("LLM_AGENT_VAR_AUTO_CONFIRM")
                .env_remove(OUT_FD_ENV)
                .env(REPLY_ENV, reply)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            // SAFETY: setsid is async-signal-safe and touches no shared state.
            unsafe {
                cmd.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = cmd.spawn().unwrap();
            let mut stdout = child.stdout.take().unwrap();
            let mut stderr = child.stderr.take().unwrap();
            let stdout_thread = std::thread::spawn(move || {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut stdout, &mut text).unwrap();
                text
            });
            let stderr_thread = std::thread::spawn(move || {
                let mut text = String::new();
                std::io::Read::read_to_string(&mut stderr, &mut text).unwrap();
                text
            });
            let status = wait_or_kill(&mut child, "drive_with_dead_reply");
            let stdout = stdout_thread.join().unwrap();
            let stderr = stderr_thread.join().unwrap();
            let mut printed = Vec::new();
            let mut requests = 0;
            for out in stdout.lines() {
                if out.trim_start().as_bytes().starts_with(MARKER) {
                    requests += 1;
                } else {
                    printed.push(out.to_string());
                }
            }
            Run {
                printed,
                requests,
                wire: Vec::new(),
                code: status.code().unwrap_or(-1),
                stderr,
            }
        }

        /// Edges the bash contract pins that the broad checks leave out for
        /// the other two languages: a reply path the helper cannot open (a
        /// missing file, an fd the tool never inherited) or a request fd it
        /// never inherited fails closed without a request and without
        /// blocking, and a guard over it still aborts; a reply for our id
        /// with no `answer` key is a cancel, not an error; a checkbox answer
        /// that repeats an option yields each index once.
        fn check_probe_edges(
            command: impl Fn(&str) -> Command,
            none: &str,
            checked_once: &str,
            dir: &Path,
        ) {
            let missing = dir.join("missing").join("reply");
            for reply in [missing.to_str().unwrap(), "/dev/fd/97"] {
                let run = drive_with_dead_reply(command("confirm(\"Go?\")"), reply);
                assert_eq!(run.requests, 0, "{reply}: request sent over a dead channel");
                assert_eq!(run.code, 0, "{reply}: {}", run.stderr);
                assert_eq!(run.printed, vec![none], "{reply}: {}", run.stderr);
                assert_eq!(
                    run.stderr.trim_end(),
                    "prompt request channel unavailable",
                    "{reply}: a dead channel is reported on stderr: {}",
                    run.stderr
                );

                let run = drive_with_dead_reply(command("guard_operation(\"Really?\")"), reply);
                assert_eq!(run.requests, 0, "{reply}: request sent over a dead channel");
                assert_eq!(run.code, 1, "{reply}: a guard over a dead channel aborts");
                assert!(
                    run.stderr.contains("Operation aborted!"),
                    "{reply}: {}",
                    run.stderr
                );
            }

            let dead_out = [(OUT_FD_ENV, "97")];
            let run = drive_env(command("confirm(\"Go?\")"), Some(&[]), false, &dead_out);
            assert_eq!(run.requests, 0, "request sent over a dead request fd");
            assert_eq!(run.code, 0, "{}", run.stderr);
            assert_eq!(run.printed, vec![none], "{}", run.stderr);
            assert_eq!(
                run.stderr.trim_end(),
                "prompt request channel unavailable",
                "a dead request fd is reported on stderr: {}",
                run.stderr
            );

            let run = drive_env(
                command("guard_operation(\"Go?\")"),
                Some(&[]),
                false,
                &dead_out,
            );
            assert_eq!(run.requests, 0, "request sent over a dead request fd");
            assert_eq!(run.code, 1, "a guard over a dead request fd aborts");
            assert!(run.stderr.contains("Operation aborted!"), "{}", run.stderr);

            let run = drive_raw(command("confirm(\"Go?\")"), |id| {
                format!("{{\"id\":\"{id}\"}}")
            });
            assert_eq!(run.code, 0, "{}", run.stderr);
            assert_eq!(run.requests, 1, "{:?}", run.wire);
            assert_eq!(run.printed, vec![none], "{}", run.stderr);
            assert!(
                run.stderr.is_empty(),
                "a reply without an answer is a silent cancel: {}",
                run.stderr
            );

            let run = drive(
                command("checkbox(\"Pick\", [\"a\", \"b\", \"c\"])"),
                Some(&[json!(["a", "a"])]),
                false,
            );
            assert_eq!(run.code, 0, "{}", run.stderr);
            assert_eq!(run.printed, vec![checked_once], "{}", run.stderr);
            assert!(run.stderr.is_empty(), "{}", run.stderr);
        }

        mod py_contract {
            use super::*;

            const PY_DRIVER: &str = "import importlib.util, sys\n\
                sys.dont_write_bytecode = True\n\
                spec = importlib.util.spec_from_file_location(\"prompt_utils\", sys.argv[1])\n\
                prompt_utils = importlib.util.module_from_spec(spec)\n\
                spec.loader.exec_module(prompt_utils)\n";

            fn py_command(expr: &str) -> Command {
                let mut cmd = Command::new("python3");
                cmd.arg("-c")
                    .arg(format!("{PY_DRIVER}print(prompt_utils.{expr})\n"))
                    .arg(PY_HELPER);
                cmd.env("PYTHONDONTWRITEBYTECODE", "1");
                cmd
            }

            fn run_py(expr: &str, answers: Option<&[Value]>, stray_first: bool) -> Run {
                drive(py_command(expr), answers, stray_first)
            }

            fn run_py_in(
                expr: &str,
                answers: Option<&[Value]>,
                env: &[(&str, &str)],
                cwd: &Path,
            ) -> Run {
                let mut cmd = py_command(expr);
                cmd.current_dir(cwd);
                drive_env(cmd, answers, false, env)
            }

            #[test]
            fn helper_honours_the_channel_contract() {
                if !python_available() {
                    return;
                }
                check_contract(
                    run_py,
                    &Contract {
                        confirm_true: "True",
                        confirm_false: "False",
                        none: "None",
                        checked: "[0, 2]",
                    },
                );
            }

            #[test]
            fn guards_mirror_the_bash_contract() {
                if !python_available() {
                    return;
                }
                let cwd = scratch_dir("py-guards");
                check_guards(&run_py_in, "None", &cwd);
                fs::remove_dir_all(&cwd).ok();
            }

            #[test]
            fn replies_of_the_wrong_shape_are_unusable() {
                if !python_available() {
                    return;
                }
                check_unusable_shapes(run_py, "None");
            }

            #[test]
            fn no_channel_and_no_terminal_falls_closed_without_a_request() {
                if !python_available() {
                    return;
                }
                check_no_channel(
                    drive_without_channel(py_command("confirm(\"Go?\")")),
                    "None",
                );
            }

            #[test]
            fn request_lines_carry_the_text_verbatim_with_fresh_ids() {
                if !python_available() {
                    return;
                }
                check_wire(run_py, "prompt_utils.", "x", "1");
            }

            #[test]
            fn requests_escape_a_bash_tools_capture_through_its_saved_stdout() {
                if !python_available() || bash_program().is_none() {
                    return;
                }
                check_nested_in_bash(&py_command("select(\"Pick\", [\"a\", \"b\", \"c\"])"), "1");
            }

            #[test]
            fn wire_edges_mirror_the_bash_helper() {
                if !python_available() {
                    return;
                }
                check_edges(py_command, "None", "True", "[]", "");
            }

            #[test]
            fn dead_channel_and_answerless_reply_fail_closed() {
                if !python_available() {
                    return;
                }
                let dir = scratch_dir("py-probe-edges");
                check_probe_edges(py_command, "None", "[0]", &dir);
                fs::remove_dir_all(&dir).ok();
            }
        }

        mod ts_contract {
            use super::*;

            /// A driver in `dir` that prints `expr` through `stringify`,
            /// importing the helper from a copy beside it: under the repo
            /// the helper sits below a package.json without a `type` field,
            /// and node warns about that on stderr, which the contract
            /// requires empty.
            fn ts_command(runtime: &str, dir: &Path, expr: &str, stringify: &str) -> Command {
                let helper = dir.join("prompt-utils.ts");
                fs::copy(TS_HELPER, &helper).unwrap();
                let driver = dir.join("driver.ts");
                fs::write(
                    &driver,
                    format!(
                        "import {{ pathToFileURL }} from \"node:url\";\n\
                         const prompt = await import(pathToFileURL(process.argv[2]).href);\n\
                         console.log(({stringify})(prompt.{expr}));\n"
                    ),
                )
                .unwrap();
                let mut cmd = Command::new(runtime);
                cmd.arg(&driver).arg(&helper);
                cmd.env("NODE_NO_WARNINGS", "1");
                cmd
            }

            fn run_ts(
                runtime: &str,
                dir: &Path,
                expr: &str,
                answers: Option<&[Value]>,
                stray_first: bool,
            ) -> Run {
                drive(
                    ts_command(runtime, dir, expr, "JSON.stringify"),
                    answers,
                    stray_first,
                )
            }

            fn run_ts_in(
                runtime: &str,
                dir: &Path,
                expr: &str,
                answers: Option<&[Value]>,
                env: &[(&str, &str)],
                cwd: &Path,
            ) -> Run {
                // Guards return undefined, which JSON.stringify alone would
                // print as an empty line.
                let mut cmd = ts_command(runtime, dir, expr, "(v) => String(JSON.stringify(v))");
                cmd.current_dir(cwd);
                drive_env(cmd, answers, false, env)
            }

            #[test]
            fn helper_honours_the_channel_contract() {
                let Some(runtime) = ts_runtime() else {
                    return;
                };
                let dir = scratch_dir("ts-contract");
                check_contract(
                    |expr, answers, stray_first| run_ts(runtime, &dir, expr, answers, stray_first),
                    &Contract {
                        confirm_true: "true",
                        confirm_false: "false",
                        none: "null",
                        checked: "[0,2]",
                    },
                );
                fs::remove_dir_all(&dir).ok();
            }

            #[test]
            fn guards_mirror_the_bash_contract() {
                let Some(runtime) = ts_runtime() else {
                    return;
                };
                let dir = scratch_dir("ts-guards");
                let cwd = scratch_dir("ts-guards-cwd");
                check_guards(
                    &|expr, answers, env, cwd| run_ts_in(runtime, &dir, expr, answers, env, cwd),
                    "undefined",
                    &cwd,
                );
                fs::remove_dir_all(&dir).ok();
                fs::remove_dir_all(&cwd).ok();
            }

            #[test]
            fn replies_of_the_wrong_shape_are_unusable() {
                let Some(runtime) = ts_runtime() else {
                    return;
                };
                let dir = scratch_dir("ts-shapes");
                check_unusable_shapes(
                    |expr, answers, stray_first| run_ts(runtime, &dir, expr, answers, stray_first),
                    "null",
                );
                fs::remove_dir_all(&dir).ok();
            }

            #[test]
            fn no_channel_and_no_terminal_falls_closed_without_a_request() {
                let Some(runtime) = ts_runtime() else {
                    return;
                };
                let dir = scratch_dir("ts-no-channel");
                check_no_channel(
                    drive_without_channel(ts_command(
                        runtime,
                        &dir,
                        "confirm(\"Go?\")",
                        "JSON.stringify",
                    )),
                    "null",
                );
                fs::remove_dir_all(&dir).ok();
            }

            #[test]
            fn request_lines_carry_the_text_verbatim_with_fresh_ids() {
                let Some(runtime) = ts_runtime() else {
                    return;
                };
                let dir = scratch_dir("ts-wire");
                check_wire(
                    |expr, answers, stray_first| run_ts(runtime, &dir, expr, answers, stray_first),
                    "prompt.",
                    "\"x\"",
                    "1",
                );
                fs::remove_dir_all(&dir).ok();
            }

            #[test]
            fn requests_escape_a_bash_tools_capture_through_its_saved_stdout() {
                let Some(runtime) = ts_runtime() else {
                    return;
                };
                if bash_program().is_none() {
                    return;
                }
                let dir = scratch_dir("ts-nested");
                check_nested_in_bash(
                    &ts_command(
                        runtime,
                        &dir,
                        "select(\"Pick\", [\"a\", \"b\", \"c\"])",
                        "JSON.stringify",
                    ),
                    "1",
                );
                fs::remove_dir_all(&dir).ok();
            }

            #[test]
            fn wire_edges_mirror_the_bash_helper() {
                let Some(runtime) = ts_runtime() else {
                    return;
                };
                let dir = scratch_dir("ts-edges");
                check_edges(
                    |expr| ts_command(runtime, &dir, expr, "(v) => String(JSON.stringify(v))"),
                    "null",
                    "true",
                    "[]",
                    "\"\"",
                );
                fs::remove_dir_all(&dir).ok();
            }

            #[test]
            fn dead_channel_and_answerless_reply_fail_closed() {
                let Some(runtime) = ts_runtime() else {
                    return;
                };
                let dir = scratch_dir("ts-probe-edges");
                check_probe_edges(
                    |expr| ts_command(runtime, &dir, expr, "(v) => String(JSON.stringify(v))"),
                    "null",
                    "[0]",
                    &dir,
                );
                fs::remove_dir_all(&dir).ok();
            }

            /// The host keeps the channel's write end open, so a helper that
            /// waited for EOF would hang: the reply must come through a
            /// bounded readSync loop on the opened fd.
            #[test]
            fn helper_reads_the_reply_with_a_readsync_loop() {
                let source = fs::read_to_string(TS_HELPER).unwrap();
                assert!(
                    source.contains("fs.readSync("),
                    "reply read is not a readSync loop"
                );
                assert!(
                    source.contains("fs.openSync(process.env[REPLY_ENV]"),
                    "reply path is not opened from the channel env var"
                );
                let read_file_sync_calls: Vec<&str> = source
                    .lines()
                    .filter(|line| {
                        !line.trim_start().starts_with("//") && line.contains("readFileSync(")
                    })
                    .collect();
                assert!(
                    read_file_sync_calls.is_empty(),
                    "the reply must not be read to EOF: {read_file_sync_calls:?}"
                );
            }
        }
    }
}
