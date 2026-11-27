use super::*;
use std::fs::File;
use std::io::BufReader;
use std::sync::atomic::AtomicBool;

const TOOL: &str = "route-x7x";

/// A router for `TOOL` and a reader on the reply pipe it answers through.
fn router(prompter: &Arc<ScriptedPrompter>, exited: bool) -> (Router, BufReader<File>) {
    let mut envs = HashMap::new();
    let attachment = Attachment::open(
        Some(ToolPrompter::new(
            Arc::clone(prompter) as Arc<dyn Prompter>,
            Arc::new(PromptMemory::default()),
            Origin::detached(),
        )),
        TOOL,
        &mut envs,
    );
    let replies = BufReader::new(File::open(&envs[REPLY_ENV]).unwrap());
    let router = attachment
        .router(
            TOOL,
            Some("call-route-x7x".to_string()),
            None,
            Arc::new(AtomicBool::new(exited)),
        )
        .unwrap();
    (router, replies)
}

fn next_reply(replies: &mut BufReader<File>) -> Value {
    let mut line = String::new();
    replies.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn output_is_passed_through() {
    let prompter = Arc::new(ScriptedPrompter::refusing());
    let (router, _replies) = router(&prompter, false);

    let routed = router.route(Classified::Output(b"progress 42%\n".to_vec()));

    assert!(matches!(routed, Routed::Output(bytes) if bytes == b"progress 42%\n"));
}

#[test]
#[serial]
fn a_request_is_served_and_answered() {
    let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
    let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
    let prompter = Arc::new(ScriptedPrompter::new(vec![PromptAnswer::Confirmed(true)]));
    let (router, mut replies) = router(&prompter, false);

    let routed = router.route(Classified::Prompt(request_line("p1", "confirm", "Go?")));

    let Routed::Served(request) = routed else {
        panic!("a well-formed request must be served");
    };
    assert_eq!(request.id(), "p1");
    request.serve();
    assert_eq!(
        next_reply(&mut replies),
        json!({"id": "p1", "answer": true})
    );
    let requests = prompter.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].tool.as_deref(), Some(TOOL));
    assert_eq!(requests[0].call_id.as_deref(), Some("call-route-x7x"));
}

#[test]
fn a_request_read_after_exit_is_rejected_unparsed() {
    let prompter = Arc::new(ScriptedPrompter::refusing());
    let (router, mut replies) = router(&prompter, true);
    let line = request_line("p-late", "confirm", "Late?");

    let routed = router.route(Classified::Prompt(line.clone()));

    assert!(matches!(routed, Routed::Rejected(bytes) if bytes == line));
    assert_eq!(
        next_reply(&mut replies),
        json!({"id": "p-late", "answer": null})
    );
    assert!(prompter.requests().is_empty());
}

#[test]
fn a_malformed_request_is_rejected_with_a_null_reply() {
    let prompter = Arc::new(ScriptedPrompter::refusing());
    let (router, mut replies) = router(&prompter, false);
    let line = request_line("p-bad", "dance", "Shall we?");

    let routed = router.route(Classified::Prompt(line.clone()));

    assert!(matches!(routed, Routed::Rejected(bytes) if bytes == line));
    assert_eq!(
        next_reply(&mut replies),
        json!({"id": "p-bad", "answer": null})
    );
}

/// A scripted prompter with no answers left panics when asked.
#[test]
#[serial]
fn a_panicking_prompter_answers_null() {
    let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
    let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
    let prompter = Arc::new(ScriptedPrompter::refusing());
    let (router, mut replies) = router(&prompter, false);

    let Routed::Served(request) =
        router.route(Classified::Prompt(request_line("p-boom", "confirm", "Go?")))
    else {
        panic!("a well-formed request must be served");
    };
    request.serve();

    assert_eq!(
        next_reply(&mut replies),
        json!({"id": "p-boom", "answer": null})
    );
}
