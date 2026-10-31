//! Lets a running tool ask the human through Coyote's prompter. The tool
//! writes one request line to its stdout; the stdout pump spots it, asks,
//! and writes the answer line to a pipe the tool inherits as
//! `COYOTE_PROMPT_REPLY`. Lines that are not requests flow through as
//! ordinary output.

use crate::events::{self, Event, Origin};
use crate::prompt::{PromptAction, PromptAnswer, PromptMemory, PromptRequest, Prompter};
use anyhow::{Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(unix)]
use std::io::{self, PipeReader, PipeWriter, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Arc;

pub(crate) const REPLY_ENV: &str = "COYOTE_PROMPT_REPLY";
pub(crate) const MARKER: &[u8] = b"{\"coyote_prompt\"";

/// The parent's end of the reply pipe. The child reads replies from the fd
/// named by `env_value`, which `configure` keeps open across exec.
#[cfg(unix)]
pub(crate) struct Server {
    writer: PipeWriter,
    child_end: Option<PipeReader>,
    child_fd: RawFd,
}

#[cfg(unix)]
impl Server {
    pub(crate) fn open() -> Result<Self> {
        let (reader, writer) = io::pipe()?;
        let child_fd = reader.as_raw_fd();
        Ok(Self {
            writer,
            child_end: Some(reader),
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
        anyhow::bail!("prompt reply channel is not implemented on this platform") // TASK-316
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
    ) -> Option<PromptRequest> {
        match parse_request(line, tool, call_id, job_id, self.origin.clone()) {
            Ok(req) => Some(req),
            Err(err) => {
                warn!("Ignoring malformed prompt line from tool '{tool}': {err}");
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
        crate::prompt::ask(self.prompter.as_ref(), req)
    }
}

/// Asks and writes the reply line; a prompter failure answers `null` so
/// the tool fails closed instead of hanging.
pub(crate) fn handle_prompt_line(prompter: &ToolPrompter, reply: &Server, req: PromptRequest) {
    let id = req.id.clone();
    let tool = req.tool.clone().unwrap_or_default();
    let answer = prompter.answer(req);
    if let Err(err) = &answer {
        warn!("Prompt from tool '{tool}' failed, answering null: {err}");
    }
    reply.reply(&id, &answer_value(&answer));
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
    options: Vec<String>,
    #[serde(default)]
    default: Option<Value>,
}

pub(crate) fn parse_request(
    line: &[u8],
    tool: &str,
    call_id: Option<String>,
    job_id: Option<String>,
    origin: Origin,
) -> Result<PromptRequest> {
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
    Ok(PromptRequest {
        id: wire.id,
        action,
        question: wire.question,
        options: wire.options,
        default: wire.default,
        tool: Some(tool.to_string()),
        call_id,
        job_id,
        origin,
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Classified {
    Output(Vec<u8>),
    /// A complete line, newline included, that starts with [`MARKER`].
    Prompt(Vec<u8>),
}

/// Splits a tool's stdout into lines and picks out the protocol ones.
/// Output is handed on in order and byte for byte; only the chunking
/// changes, and only where a tail might still turn into a request line.
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
            let line = &data[start..end];
            if is_prompt_line(line) {
                out.push(Classified::Prompt(line.to_vec()));
            } else {
                push_output(&mut out, line);
            }
            start = end;
        }
        let tail = &data[start..];
        if could_become_prompt(tail) {
            self.carry = tail.to_vec();
        } else {
            push_output(&mut out, tail);
        }
        out
    }

    /// The carried tail at EOF, which no newline will ever complete.
    pub(crate) fn finish(&mut self) -> Option<Vec<u8>> {
        let tail = std::mem::take(&mut self.carry);
        (!tail.is_empty()).then_some(tail)
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

fn trim_leading_whitespace(bytes: &[u8]) -> &[u8] {
    let skip = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_whitespace())
        .count();
    &bytes[skip..]
}

fn is_prompt_line(line: &[u8]) -> bool {
    trim_leading_whitespace(line).starts_with(MARKER)
}

/// Holding back ordinary partial output would stall progress bars, so a
/// tail is only carried while it is still a prefix of a request line.
fn could_become_prompt(tail: &[u8]) -> bool {
    let tail = trim_leading_whitespace(tail);
    tail.starts_with(MARKER) || MARKER.starts_with(tail)
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
    use crate::testing::{AtomicFlagGuard, TestConfigDirGuard, bash_program};
    use crate::turn::SilentSink;
    use crate::utils::{ACP_SERVER, HEADLESS};
    use serial_test::serial;
    use std::collections::HashMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    const HELPER: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/assets/functions/utils/prompt-utils.sh"
    );

    fn tools_available() -> bool {
        if bash_program().is_none() {
            eprintln!("skipping: bash not found");
            return false;
        }
        if which::which("jq").is_err() {
            eprintln!("skipping: jq not found");
            return false;
        }
        true
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

    #[test]
    #[serial]
    fn fixture_tool_round_trips_every_action() {
        if !tools_available() {
            return;
        }
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
        assert!(assembler.feed(b"   ").is_empty());
        assert_eq!(assembler.finish(), Some(b"   ".to_vec()));
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
        .unwrap();

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
    fn long_question_split_across_reads_round_trips_end_to_end() {
        if !tools_available() {
            return;
        }
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
        if !tools_available() {
            return;
        }
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
        let mut chunks = Vec::new();
        for event in &events {
            if let Event::ToolOutputChunk {
                call_id,
                stream: OutputStream::Stdout,
                bytes,
            } = event
                && call_id == "call-x7x"
            {
                chunks.extend_from_slice(bytes);
            }
        }
        let chunks = String::from_utf8(chunks).unwrap();
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
    fn cancel_fails_the_guarded_tool_closed() {
        if !tools_available() {
            return;
        }
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
        // guard_operation sends the abort message through `2>&1`, so it
        // lands on whichever captured stream the shell wired it to.
        let captured = format!(
            "{}{}",
            output["stdout"].as_str().unwrap_or_default(),
            output["stderr"].as_str().unwrap_or_default()
        );
        assert!(captured.contains("Operation aborted!"), "{output}");
        assert_eq!(output.get("output"), None, "{output}");
        assert_eq!(prompter.requests().len(), 1);
        assert_eq!(prompter.requests()[0].question, "Delete? cancel-x7x");
    }

    #[test]
    #[serial]
    fn no_prompter_means_no_reply_channel() {
        if !tools_available() {
            return;
        }
        let _guard = TestConfigDirGuard::new("prompt-none");
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
        if !tools_available() {
            return;
        }
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

    #[test]
    #[serial]
    fn ui_kind_follows_whether_a_human_is_present() {
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
        let ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);

        let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
        assert_eq!(ctx.ui_kind(), crate::config::UiKind::Repl);
        assert_eq!(ctx.ui_kind().as_str(), "repl");

        let _headless_on = AtomicFlagGuard::set(&HEADLESS, true);
        assert_eq!(ctx.ui_kind(), crate::config::UiKind::Headless);
        assert_eq!(ctx.ui_kind().as_str(), "headless");
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
        if !tools_available() {
            return;
        }
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
        if !tools_available() {
            return;
        }
        let _guard = TestConfigDirGuard::new("prompt-job");
        install_tool(
            "jobask-x7x",
            r#"ans="$(confirm "Proceed? job-x7x")"; echo "answer=$ans"; echo DONE > "$LLM_OUTPUT""#,
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
        assert!(
            collected["output_tail"]
                .as_str()
                .unwrap()
                .contains("answer=1"),
            "{collected}"
        );
        let requests = prompter.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].question, "Proceed? job-x7x");
        assert_eq!(requests[0].tool.as_deref(), Some("jobask-x7x"));
        assert_eq!(requests[0].job_id.as_deref(), Some(job_id.as_str()));
        assert_eq!(requests[0].call_id, None);
    }
}
