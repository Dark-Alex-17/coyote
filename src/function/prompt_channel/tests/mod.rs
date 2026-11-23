use super::*;
use crate::config::{AppState, RequestContext, WorkingMode};
use crate::events::{Event, OutputStream, RecordingSink};
use crate::function::{FunctionDeclaration, JsonSchema, ToolCall, ToolIo, jobs, run_llm_function};
use crate::prompt::{ScriptedPrompter, prompt_events};
use crate::testing::{AtomicFlagGuard, EnvVarGuard, TestConfigDirGuard, bash_program};
use crate::turn::SilentSink;
use crate::utils::{ACP_SERVER, HEADLESS};
use serial_test::serial;
use std::collections::HashMap;
use std::fs;
use std::io::BufRead;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

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

/// A reply path no child can open: `/dev/fd/<n>` where `n` is the
/// process's descriptor limit, one past the highest number any
/// descriptor can have. A fixed small number would not do: a test
/// process with many files open may hold it, or the test's own channel
/// may land on it, and either turns the "unopenable" path into a
/// working one and the case into a false pass.
fn dead_reply_path() -> String {
    format!("/dev/fd/{}", dead_fd())
}

/// A descriptor number no child can hold: the process limit itself.
fn dead_fd() -> u64 {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit fills the struct behind a valid pointer.
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    assert_eq!(rc, 0, "getrlimit failed");
    // An unlimited process still cannot hold a descriptor this high.
    limit.rlim_cur.min(1 << 20)
}

/// Installs a bash tool that sources the shipped helper on the
/// functions bin dir the active config guard points at.
fn install_tool(name: &str, body: &str) {
    install_tool_in(&crate::config::paths::functions_bin_dir(), name, body);
}

fn install_tool_in(bin: &std::path::Path, name: &str, body: &str) {
    fs::create_dir_all(bin).unwrap();
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
        .output
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
            ..
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

const PASSWORD_TOOL: &str = r#"if v="$(password "Secret? pw-x7x")"; then echo "got=$v" >> "$LLM_OUTPUT"; else echo refused >> "$LLM_OUTPUT"; fi"#;

#[test]
#[serial]
fn any_tool_may_ask_for_a_password_under_its_own_name() {
    require_tools();
    let _guard = TestConfigDirGuard::new("prompt-pw-user-tool");
    install_tool("ask-x7x", PASSWORD_TOOL);
    let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Text(
        "s3cret".to_string(),
    )]));

    let output = run_tool(
        "ask-x7x",
        tool_envs(),
        io_with(&prompter, Arc::new(PromptMemory::default())),
    );

    assert_eq!(output["output"], "got=s3cret\n");
    let requests = prompter.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].action, PromptAction::Password);
    assert_eq!(requests[0].tool.as_deref(), Some("ask-x7x"));
}

#[test]
fn a_tool_named_on_the_line_does_not_replace_the_spawning_tool() {
    let prompter = ToolPrompter::new(
        Arc::new(ScriptedPrompter::refusing()),
        Arc::new(PromptMemory::default()),
        Origin::detached(),
    );
    let line = json!({
        "tool": "fs_cat",
        "coyote_prompt": {
            "id": "pw1", "action": "password", "question": "Secret? forged-x7x",
            "tool": "fs_cat"
        }
    })
    .to_string();

    let parsed = prompter
        .parse_line(line.as_bytes(), "ask-x7x", None, None)
        .expect("a line with extra fields still parses");

    assert_eq!(parsed.req.tool.as_deref(), Some("ask-x7x"));
}

#[test]
#[serial]
fn a_global_tool_run_under_an_agent_asks_under_its_own_name() {
    require_tools();
    let _guard = TestConfigDirGuard::new("prompt-global-under-agent");
    let agent = "agent-x7x";
    install_tool_in(
        &crate::config::paths::agent_bin_dir(agent),
        "ask-x7x",
        PASSWORD_TOOL,
    );
    let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Text(
        "s3cret".to_string(),
    )]));

    let output = run_llm_function(
        "ask-x7x".to_string(),
        vec![r#"{"path":"args-x7x"}"#.to_string()],
        tool_envs(),
        Some(agent.to_string()),
        None,
        io_with(&prompter, Arc::new(PromptMemory::default())),
        None,
    )
    .unwrap()
    .output
    .unwrap_or_default();

    assert_eq!(output, "got=s3cret\n");
    let requests = prompter.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].tool.as_deref(), Some("ask-x7x"));
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
    assert!(!req.closed);

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
    let line =
        r#"{"coyote_prompt": {"id": "p1", "action": "select", "question": "q", "options": null}}"#;

    let req = parse_request(line.as_bytes(), "t", None, None, Origin::detached())
        .unwrap()
        .req;

    assert!(req.options.is_empty());
    assert!(!req.closed);
}

/// A tool's select with options stays open, so the human can type an
/// answer the tool did not offer.
#[test]
fn parse_request_leaves_a_select_with_options_open() {
    let line = r#"{"coyote_prompt": {"id": "p1", "action": "select", "question": "q", "options": ["a", "b"]}}"#;

    let req = parse_request(line.as_bytes(), "t", None, None, Origin::detached())
        .unwrap()
        .req;

    assert_eq!(req.action, PromptAction::Select);
    assert_eq!(req.options, vec!["a", "b"]);
    assert!(!req.closed);
}

#[test]
fn parse_request_leaves_a_checkbox_with_options_open() {
    let line = r#"{"coyote_prompt": {"id": "p1", "action": "checkbox", "question": "q", "options": ["a", "b"]}}"#;

    let req = parse_request(line.as_bytes(), "t", None, None, Origin::detached())
        .unwrap()
        .req;

    assert_eq!(req.action, PromptAction::Checkbox);
    assert_eq!(req.options, vec!["a", "b"]);
    assert!(!req.closed);
}

#[test]
fn parse_request_shows_terminal_escapes_in_tool_text_visibly() {
    let line = json!({"coyote_prompt": {
        "id": "p1", "action": "select", "question": "\u{1b}[2Jwhich?",
        "options": ["\u{1b}[31ma", "b", "keep\n  delete all"]
    }})
    .to_string();

    let parsed = parse_request(line.as_bytes(), "t", None, None, Origin::detached()).unwrap();

    assert_eq!(parsed.req.question, "\\x1b[2Jwhich?");
    assert_eq!(parsed.req.raw_question.as_deref(), Some("\u{1b}[2Jwhich?"));
    assert_eq!(
        parsed.req.options,
        vec!["\\x1b[31ma", "b", "keep\\x0a  delete all"]
    );
    assert_eq!(
        parsed.raw_options,
        vec!["\u{1b}[31ma", "b", "keep\n  delete all"]
    );
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
    assert_eq!(answer_value(&Ok(PromptAnswer::Interrupted)), Value::Null);
    assert_eq!(answer_value(&Ok(PromptAnswer::NeedsHuman)), Value::Null);
    assert_eq!(answer_value(&Err(anyhow!("boom"))), Value::Null);
}

/// A tool sees a Ctrl-C exactly as an Esc, and a run with no human as
/// either: each fits every action and is answered null.
#[test]
fn a_cancel_or_an_interrupt_fits_every_action() {
    for action in [
        PromptAction::Confirm,
        PromptAction::Select,
        PromptAction::Checkbox,
        PromptAction::Input,
        PromptAction::Password,
    ] {
        assert!(fits(action, &PromptAnswer::Cancelled), "{action:?}");
        assert!(fits(action, &PromptAnswer::Interrupted), "{action:?}");
        assert!(fits(action, &PromptAnswer::NeedsHuman), "{action:?}");
    }
}

#[test]
#[serial]
fn headless_run_answers_tool_prompts_with_cancel() {
    let _headless = AtomicFlagGuard::set(&HEADLESS, true);
    let sink = RecordingSink::install();
    let scripted = Arc::new(ScriptedPrompter::refusing());
    let prompter = ToolPrompter::new(
        scripted.clone(),
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
    assert!(scripted.requests().is_empty(), "{:?}", scripted.requests());
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
fn select_answer_outside_the_options_is_replied_as_typed() {
    let mut req = PromptRequest::new(PromptAction::Select, "Pick? opt-x7x", Origin::detached());
    req.options = vec!["a".to_string(), "b".to_string()];
    let id = req.id.clone();

    let reply = reply_for(
        ScriptedPrompter::new(vec![PromptAnswer::Selected("typed by hand".to_string())]),
        req,
    );

    assert_eq!(reply, json!({"id": id, "answer": "typed by hand"}));
}

#[test]
#[serial]
fn blank_select_answer_outside_the_options_is_replied_as_null() {
    for blank in ["", "   "] {
        let mut req = PromptRequest::new(PromptAction::Select, "Pick? opt-b1k", Origin::detached());
        req.options = vec!["a".to_string(), "b".to_string()];
        let id = req.id.clone();

        let reply = reply_for(
            ScriptedPrompter::new(vec![PromptAnswer::Selected(blank.to_string())]),
            req,
        );

        assert_eq!(reply, json!({"id": id, "answer": null}), "{blank:?}");
    }
}

#[test]
#[serial]
fn a_blank_answer_to_a_select_with_no_options_is_replied_as_null() {
    for (answer, expected) in [("", Value::Null), ("  ", Value::Null), ("x", json!("x"))] {
        let req = PromptRequest::new(PromptAction::Select, "Pick? none-q8v", Origin::detached());
        let id = req.id.clone();

        let reply = reply_for(
            ScriptedPrompter::new(vec![PromptAnswer::Selected(answer.to_string())]),
            req,
        );

        assert_eq!(reply, json!({"id": id, "answer": expected}), "{answer:?}");
    }
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
    let shown = "\\x1b[31mred\\x1b[0m";
    assert_eq!(parsed.req.options, vec![shown, "b"]);

    let reply = reply_for_parsed(
        ScriptedPrompter::new(vec![PromptAnswer::Selected(shown.to_string())]),
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
    let io = ToolIo::new(Arc::new(SilentSink), Some("call-hl-x7x".to_string()), 0).with_prompter(
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

/// A scripted prompter with no answers panics when asked; the tool still
/// gets its reply, and the bus sees the request answered as errored.
#[test]
#[serial]
fn panicking_prompter_answers_the_tool_null() {
    require_tools();
    let _guard = TestConfigDirGuard::new("prompt-panic");
    let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
    let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
    install_tool(
        "panic-x7x",
        "echo '{\"coyote_prompt\":{\"id\":\"p-panic-x7x\",\"action\":\"confirm\",\"question\":\"q-panic-x7x\"}}' >&9\nIFS= read -r r <\"$COYOTE_PROMPT_REPLY\"; echo \"$r\" >> \"$LLM_OUTPUT\"",
    );
    let prompter = Arc::new(ScriptedPrompter::refusing());
    let sink = RecordingSink::install();

    let output = run_tool(
        "panic-x7x",
        tool_envs(),
        io_with(&prompter, Arc::new(PromptMemory::default())),
    );

    assert_eq!(
        output["output"],
        format!("{}\n", json!({"id": "p-panic-x7x", "answer": null}))
    );
    let (requested, answered) = prompt_events(sink.drain(), "q-panic-x7x");
    let [Event::PromptRequested { id, .. }] = requested.as_slice() else {
        panic!("{requested:?}");
    };
    assert_eq!(
        answered,
        vec![Event::PromptAnswered {
            id: id.clone(),
            answer: crate::prompt::ERROR_MARKER.to_string(),
            auto: false,
        }]
    );
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
    memory.remember(&PromptRequest {
        tool: Some("ask-x7x".to_string()),
        ..PromptRequest::new(PromptAction::Confirm, "Delete? mem-x7x", Origin::detached())
    });
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

/// Runs a guarded `tool` on `first`, remembers its question the way
/// "Yes for this session" does, and checks that `first` then runs
/// unasked while `second` asks again. Paths reach the question
/// resolved, so only their last component is looked for.
fn check_session_yes_covers_one_operation(
    tool: &str,
    run: impl Fn(&str, ToolIo) -> Value,
    first: &str,
    second: &str,
) {
    let leaf = |target: &str| target.rsplit('/').next().unwrap().to_string();
    let memory = Arc::new(PromptMemory::default());
    let prompter = Arc::new(ScriptedPrompter::new(vec![
        PromptAnswer::Confirmed(true),
        PromptAnswer::Confirmed(true),
    ]));

    let output = run(first, io_with(&prompter, Arc::clone(&memory)));
    assert_eq!(output.get("tool_call_error"), None, "{tool}: {output}");
    let asked = prompter.requests();
    assert_eq!(asked.len(), 1, "{tool}: {asked:?}");
    let question = asked[0].question.clone();
    assert!(question.contains(&leaf(first)), "{tool}: {question:?}");
    memory.remember(&asked[0]);

    let sink = RecordingSink::install();
    let output = run(first, io_with(&prompter, Arc::clone(&memory)));
    assert_eq!(output.get("tool_call_error"), None, "{tool}: {output}");
    assert_eq!(
        prompter.requests().len(),
        1,
        "{tool}: the remembered operation asked again"
    );
    let (_, answered) = prompt_events(sink.drain(), &question);
    assert!(
        matches!(
            answered.as_slice(),
            [Event::PromptAnswered { auto: true, .. }]
        ),
        "{tool}: {answered:?}"
    );

    let output = run(second, io_with(&prompter, memory));
    assert_eq!(output.get("tool_call_error"), None, "{tool}: {output}");
    let asked = prompter.requests();
    assert_eq!(
        asked.len(),
        2,
        "{tool}: a different operation must ask again: {asked:?}"
    );
    assert!(
        asked[1].question.contains(&leaf(second)),
        "{tool}: {:?}",
        asked[1].question
    );
}

#[test]
#[serial]
fn session_yes_covers_only_that_operation_or_path() {
    require_tools();
    let _guard = TestConfigDirGuard::new("prompt-session-yes");
    install_tool(
        "op-x7x",
        "unset AUTO_CONFIRM LLM_AGENT_VAR_AUTO_CONFIRM\nguard_operation \"Run?\" \"$GUARD_X7X\"\necho ok >> \"$LLM_OUTPUT\"",
    );
    install_tool(
        "path-x7x",
        "unset AUTO_CONFIRM LLM_AGENT_VAR_AUTO_CONFIRM\nguard_path \"$GUARD_X7X\" \"Touch?\"\necho ok >> \"$LLM_OUTPUT\"",
    );
    let runner = |tool: &'static str| {
        move |target: &str, io: ToolIo| {
            let mut envs = tool_envs();
            envs.insert("GUARD_X7X".to_string(), target.to_string());
            run_tool(tool, envs, io)
        }
    };
    let outside = std::env::temp_dir();

    check_session_yes_covers_one_operation(
        "op-x7x",
        runner("op-x7x"),
        "echo one-x7x",
        "echo two-x7x",
    );
    check_session_yes_covers_one_operation(
        "path-x7x",
        runner("path-x7x"),
        &outside.join("one-x7x.txt").to_string_lossy(),
        &outside.join("two-x7x.txt").to_string_lossy(),
    );
}

/// A remembered command covers its exact bytes: one that differs only
/// by an escape sequence or by a newline for a space asks again, and
/// the escape is spelled out where the user can see it.
#[test]
#[serial]
fn session_yes_tells_apart_commands_that_differ_by_control_bytes() {
    require_tools();
    let _guard = TestConfigDirGuard::new("prompt-session-yes-raw");
    install_tool(
        "op-x7x",
        "unset AUTO_CONFIRM LLM_AGENT_VAR_AUTO_CONFIRM\nguard_operation \"Run?\" \"$GUARD_X7X\"\necho ok >> \"$LLM_OUTPUT\"",
    );
    let run = |command: &str, io: ToolIo| {
        let mut envs = tool_envs();
        envs.insert("GUARD_X7X".to_string(), command.to_string());
        run_tool("op-x7x", envs, io)
    };
    let memory = Arc::new(PromptMemory::default());
    let prompter = Arc::new(ScriptedPrompter::new(vec![
        PromptAnswer::Confirmed(true),
        PromptAnswer::Confirmed(true),
        PromptAnswer::Confirmed(true),
    ]));

    run("echo one", io_with(&prompter, Arc::clone(&memory)));
    memory.remember(&prompter.requests()[0]);
    run("echo one", io_with(&prompter, Arc::clone(&memory)));
    assert_eq!(
        prompter.requests().len(),
        1,
        "the approved command asked again"
    );

    run(
        "echo one\u{1b}]0;x\u{7}",
        io_with(&prompter, Arc::clone(&memory)),
    );
    run("echo\none", io_with(&prompter, memory));
    let asked = prompter.requests();
    assert_eq!(asked.len(), 3, "{asked:?}");
    assert_eq!(asked[1].question, "Run?\necho one\\x1b]0;x\\x07");
    assert_eq!(asked[2].question, "Run?\necho\none");
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
async fn tool_env_says_tui_while_the_full_screen_ui_owns_the_terminal() {
    require_tools();
    let _guard = TestConfigDirGuard::new("prompt-ui-tui");
    install_tool(
        "uiprobe-x7x",
        "printf '%s' \"$COYOTE_UI\" >> \"$LLM_OUTPUT\"",
    );
    let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
    let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
    let _tui_on = crate::tui::OwnerGuard::tui_for_test();
    assert_eq!(probe_ui_env().await, json!({"output": "tui"}));
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
    ctx.prompt_memory.remember(&requests[0]);
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

/// Starts `tool` as a job with `prompter` seated and collects it.
async fn run_job(tool: &str, prompter: &Arc<ScriptedPrompter>) -> Value {
    let mut ctx = RequestContext::new(Arc::new(AppState::test_default()), WorkingMode::Cmd);
    ctx.ui = Arc::new(SilentSink);
    ctx.prompter = Arc::clone(prompter) as Arc<dyn Prompter>;
    ctx.declared_function_names.insert(tool.to_string());
    let started = jobs::handle_job_tool(
        &mut ctx,
        "job__start",
        &json!({"tool": tool, "arguments": {}}),
    )
    .await
    .unwrap();
    assert_eq!(started["status"], "ok", "{started}");
    let job_id = started["job_id"].as_str().unwrap().to_string();
    jobs::handle_job_tool(&mut ctx, "job__collect", &json!({"id": job_id}))
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn job_unparseable_request_with_an_id_is_answered_null_and_echoed() {
    require_tools();
    let _guard = TestConfigDirGuard::new("prompt-job-bad-action");
    let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
    let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
    install_tool(
        "jobdance-x7x",
        r#"echo '{"coyote_prompt":{"id":"p-jbad-x7x","action":"dance","question":"q"}}' >&9
IFS= read -r r <"$COYOTE_PROMPT_REPLY"; echo "$r" >> "$LLM_OUTPUT""#,
    );
    let prompter = Arc::new(ScriptedPrompter::refusing());

    let collected = run_job("jobdance-x7x", &prompter).await;

    assert_eq!(collected["status"], "completed", "{collected}");
    assert_eq!(
        collected["result"],
        json!({"id": "p-jbad-x7x", "answer": null}),
        "{collected}"
    );
    let tail = collected["output_tail"].as_str().unwrap();
    assert!(tail.contains("dance"), "{collected}");
    assert!(prompter.requests().is_empty(), "{:?}", prompter.requests());
}

/// A scripted prompter with no answers panics when asked; the job still
/// gets its reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
async fn job_panicking_prompter_answers_null() {
    require_tools();
    let _guard = TestConfigDirGuard::new("prompt-job-panic");
    let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
    let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
    install_tool(
        "jobpanic-x7x",
        r#"echo '{"coyote_prompt":{"id":"p-jpanic-x7x","action":"confirm","question":"q"}}' >&9
IFS= read -r r <"$COYOTE_PROMPT_REPLY"; echo "$r" >> "$LLM_OUTPUT""#,
    );
    let prompter = Arc::new(ScriptedPrompter::refusing());

    let collected = run_job("jobpanic-x7x", &prompter).await;

    assert_eq!(collected["status"], "completed", "{collected}");
    assert_eq!(
        collected["result"],
        json!({"id": "p-jpanic-x7x", "answer": null}),
        "{collected}"
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
mod bash_contract;

/// Python and TypeScript tools reach the channel through their own
/// helpers, loaded by path from the env vars the shims export. The
/// fixture and cancel tests go through the real shim builder; the
/// contract modules drive each helper directly, the way `bash_contract`
/// does for the shell one.
mod script_clients;

/// The reply channel exercised the way a tool process uses it, through the
/// public `Server` surface only (`open`, `env_value`, `configure`,
/// `spawned`, `reply`, drop) with a real bash child on the reading side:
/// the host writes a reply and the child reads it through the path in
/// `COYOTE_PROMPT_REPLY`, which the helper reopens per prompt, and a closed
/// channel ends with the helper's own `prompt reply channel closed`.
mod child_contract;

mod router;
