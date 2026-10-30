//! A question for the human: the engine asks through a `Prompter`, so one
//! call site can draw an inquire widget, answer for an absent human, or
//! replay scripted answers in a test. Every prompt is published on the
//! event bus as it is asked and as it is answered.

use crate::events::{Event, Origin, bus};
use crate::utils::{ACP_SERVER, HEADLESS};
use anyhow::{Result, anyhow};
use inquire::{Confirm, InquireError, MultiSelect, Password, PasswordDisplayMode, Select, Text};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

const CUSTOM_MULTI_CHOICE_ANSWER_OPTION: &str = "Other (custom)";
const YES_FOR_SESSION: &str = "Yes for this session";

/// Whether a human can answer a prompt in this run. A headless run and the
/// ACP server have none; every other run has a terminal.
pub(crate) fn human_present() -> bool {
    !(HEADLESS.load(Ordering::SeqCst) || ACP_SERVER.load(Ordering::SeqCst))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAction {
    Confirm,
    /// `InquirePrompter` appends a free-text "Other (custom)" entry after
    /// the offered options, so the answer need not be one of them.
    Select,
    Checkbox,
    Input,
    Password,
}

impl PromptAction {
    pub(crate) fn name(self) -> &'static str {
        match self {
            PromptAction::Confirm => "confirm",
            PromptAction::Select => "select",
            PromptAction::Checkbox => "checkbox",
            PromptAction::Input => "input",
            PromptAction::Password => "password",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRequest {
    pub id: String,
    pub action: PromptAction,
    pub question: String,
    pub options: Vec<String>,
    pub default: Option<Value>,
    /// The tool that needs the answer; a confirm with a tool offers
    /// "Yes for this session".
    pub tool: Option<String>,
    pub call_id: Option<String>,
    pub job_id: Option<String>,
    pub origin: Origin,
}

impl PromptRequest {
    /// A bare request: fresh id, no options, default, tool or job.
    pub fn new(action: PromptAction, question: impl Into<String>, origin: Origin) -> Self {
        Self {
            id: new_prompt_id(),
            action,
            question: question.into(),
            options: Vec::new(),
            default: None,
            tool: None,
            call_id: None,
            job_id: None,
            origin,
        }
    }

    pub(crate) fn requested_event(&self) -> Event {
        Event::PromptRequested {
            id: self.id.clone(),
            action: self.action,
            question: self.question.clone(),
            options: self.options.clone(),
            default: self.default.clone(),
            tool: self.tool.clone(),
            call_id: self.call_id.clone(),
            job_id: self.job_id.clone(),
            origin: self.origin.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptAnswer {
    Confirmed(bool),
    Selected(String),
    Checked(Vec<String>),
    Text(String),
    Cancelled,
}

impl PromptAnswer {
    /// The answer as published text; a password never leaves the prompt.
    pub fn summary(&self, action: PromptAction) -> String {
        match self {
            PromptAnswer::Confirmed(true) => "yes".to_string(),
            PromptAnswer::Confirmed(false) => "no".to_string(),
            PromptAnswer::Selected(choice) => choice.clone(),
            PromptAnswer::Checked(choices) => json!(choices).to_string(),
            PromptAnswer::Text(_) if action == PromptAction::Password => "<redacted>".to_string(),
            PromptAnswer::Text(text) => text.clone(),
            PromptAnswer::Cancelled => "<cancelled>".to_string(),
        }
    }

    pub fn into_confirmed(self) -> Result<bool> {
        match self {
            PromptAnswer::Confirmed(value) => Ok(value),
            other => Err(other.mismatch(PromptAction::Confirm)),
        }
    }

    pub fn into_selected(self) -> Result<String> {
        match self {
            PromptAnswer::Selected(choice) => Ok(choice),
            other => Err(other.mismatch(PromptAction::Select)),
        }
    }

    pub fn into_checked(self) -> Result<Vec<String>> {
        match self {
            PromptAnswer::Checked(choices) => Ok(choices),
            other => Err(other.mismatch(PromptAction::Checkbox)),
        }
    }

    pub fn into_text(self) -> Result<String> {
        match self {
            PromptAnswer::Text(text) => Ok(text),
            other => Err(other.mismatch(PromptAction::Input)),
        }
    }

    fn mismatch(self, expected: PromptAction) -> anyhow::Error {
        match self {
            PromptAnswer::Cancelled => anyhow!("Operation was canceled by the user"),
            _ => anyhow!("Unexpected answer to the {} prompt", expected.name()),
        }
    }
}

/// Answers a [`PromptRequest`] on behalf of the human. Synchronous: callers
/// sit on the REPL thread or inside a blocking tool, and a prompter that
/// has no human to ask answers at once.
pub trait Prompter: Send + Sync + fmt::Debug {
    fn prompt(&self, req: PromptRequest) -> Result<PromptAnswer>;
}

/// Confirms a tool already waved through with "Yes for this session", keyed
/// by tool and question. Shared by the context that consults it and the
/// prompter that records into it; cleared when the session exits.
#[derive(Debug, Default)]
pub struct PromptMemory {
    remembered: Mutex<HashMap<String, bool>>,
}

impl PromptMemory {
    fn key(tool: &str, question: &str) -> String {
        format!("{tool}\u{1f}{question}")
    }

    pub fn remember(&self, tool: &str, question: &str) {
        self.remembered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(Self::key(tool, question), true);
    }

    /// The remembered answer for a tool confirm; `None` for any other prompt
    /// or when nothing was remembered.
    pub fn auto_answer(&self, req: &PromptRequest) -> Option<bool> {
        if req.action != PromptAction::Confirm {
            return None;
        }
        let tool = req.tool.as_deref()?;
        self.remembered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&Self::key(tool, &req.question))
            .copied()
    }

    pub fn clear(&self) {
        self.remembered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

pub fn new_prompt_id() -> String {
    let short = &Uuid::new_v4().to_string()[..8];
    format!("prompt_{short}")
}

/// Asks through `prompter` and publishes the prompt as requested and as
/// answered, so a prompter stays a renderer and the bus stays the observer.
/// A prompter failure still closes the pair, with `<error>` as the answer.
pub fn ask(prompter: &dyn Prompter, req: PromptRequest) -> Result<PromptAnswer> {
    let id = req.id.clone();
    let action = req.action;
    bus().publish(req.requested_event());
    let answer = match prompter.prompt(req) {
        Ok(answer) => answer,
        Err(err) => {
            bus().publish(Event::PromptAnswered {
                id,
                answer: "<error>".to_string(),
                auto: false,
            });
            return Err(err);
        }
    };
    bus().publish(Event::PromptAnswered {
        id,
        answer: answer.summary(action),
        auto: false,
    });
    Ok(answer)
}

/// Draws the prompt as an inquire widget on the terminal.
#[derive(Debug)]
pub struct InquirePrompter {
    memory: Arc<PromptMemory>,
}

impl InquirePrompter {
    pub fn new(memory: Arc<PromptMemory>) -> Self {
        Self { memory }
    }

    fn select(req: &PromptRequest) -> Result<PromptAnswer, InquireError> {
        let mut options = req.options.clone();
        options.push(CUSTOM_MULTI_CHOICE_ANSWER_OPTION.to_string());
        let mut answer = Select::new(&req.question, options)
            .without_filtering()
            .with_help_message("↑↓ to move, enter to select") // debris-exempt: inquire help text reused verbatim
            .prompt()?;
        if answer == CUSTOM_MULTI_CHOICE_ANSWER_OPTION {
            answer = Text::new("Custom response:").prompt()?;
        }
        Ok(PromptAnswer::Selected(answer))
    }

    fn confirm(&self, req: &PromptRequest) -> Result<PromptAnswer, InquireError> {
        let Some(tool) = req.tool.as_deref() else {
            let default = req
                .default
                .as_ref()
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let answer = Confirm::new(&req.question).with_default(default).prompt()?;
            return Ok(PromptAnswer::Confirmed(answer));
        };
        let choice = Select::new(&req.question, vec!["Yes", "No", YES_FOR_SESSION])
            .without_filtering()
            .prompt()?;
        if choice == YES_FOR_SESSION {
            self.memory.remember(tool, &req.question);
        }
        Ok(PromptAnswer::Confirmed(choice != "No"))
    }
}

impl Default for InquirePrompter {
    fn default() -> Self {
        Self::new(Arc::new(PromptMemory::default()))
    }
}

impl Prompter for InquirePrompter {
    fn prompt(&self, req: PromptRequest) -> Result<PromptAnswer> {
        let answer = match req.action {
            PromptAction::Select => Self::select(&req),
            PromptAction::Confirm => self.confirm(&req),
            PromptAction::Input => Text::new(&req.question).prompt().map(PromptAnswer::Text),
            PromptAction::Checkbox => MultiSelect::new(&req.question, req.options.clone())
                .prompt()
                .map(PromptAnswer::Checked),
            PromptAction::Password => Password::new(&req.question)
                .without_confirmation()
                .with_display_mode(PasswordDisplayMode::Masked)
                .prompt()
                .map(PromptAnswer::Text),
        };
        match answer {
            Ok(answer) => Ok(answer),
            Err(InquireError::OperationCanceled | InquireError::OperationInterrupted) => {
                Ok(PromptAnswer::Cancelled)
            }
            Err(err) => Err(err.into()),
        }
    }
}

/// Answers for a run with no human: the reply is the structured
/// `needs_human` JSON the model is expected to act on, carried as text.
/// Only `handle_user_tool` knows how to read that text; every other Input
/// consumer must gate on [`human_present`] or it will take the JSON as the
/// human's answer.
#[derive(Debug, Default)]
pub struct HeadlessPrompter;

/// The JSON a headless run hands back in place of a human answer.
pub(crate) fn headless_reply(req: &PromptRequest) -> Value {
    json!({
        "needs_human": true,
        "action": req.action.name(),
        "question": req.question,
        "options": req.options,
        "guidance": "No human is present. Apply a sensible default or abort the task.",
    })
}

impl Prompter for HeadlessPrompter {
    fn prompt(&self, req: PromptRequest) -> Result<PromptAnswer> {
        Ok(PromptAnswer::Text(headless_reply(&req).to_string()))
    }
}

/// Replays scripted answers in order and records every request it saw.
/// Panics on a prompt it has no answer for, which pins "never prompts";
/// a failing one errors on every prompt instead.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct ScriptedPrompter {
    answers: Mutex<std::collections::VecDeque<PromptAnswer>>,
    failure: Option<String>,
    requests: Mutex<Vec<PromptRequest>>,
}

#[cfg(test)]
impl ScriptedPrompter {
    pub(crate) fn new(answers: Vec<PromptAnswer>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            failure: None,
            requests: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn refusing() -> Self {
        Self::new(Vec::new())
    }

    pub(crate) fn failing(message: &str) -> Self {
        Self {
            failure: Some(message.to_string()),
            ..Self::refusing()
        }
    }

    pub(crate) fn requests(&self) -> Vec<PromptRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(test)]
impl Prompter for ScriptedPrompter {
    fn prompt(&self, req: PromptRequest) -> Result<PromptAnswer> {
        if let Some(message) = &self.failure {
            return Err(anyhow!("{message}"));
        }
        let answer = self
            .answers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .unwrap_or_else(|| panic!("unexpected prompt: {req:?}"));
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(req);
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::RecordingSink;
    use crate::testing::AtomicFlagGuard;
    use serial_test::serial;

    fn request(action: PromptAction, tool: Option<&str>) -> PromptRequest {
        PromptRequest {
            options: vec!["a".to_string(), "b".to_string()],
            tool: tool.map(str::to_string),
            ..PromptRequest::new(action, "proceed? pm-4t8", Origin::detached())
        }
    }

    /// The requested events carrying `question` and the answered events that
    /// share their id, out of a drain other tests may have written to.
    fn prompt_events(events: Vec<Event>, question: &str) -> (Vec<Event>, Vec<Event>) {
        let requested: Vec<Event> = events
            .iter()
            .filter(|event| {
                matches!(event, Event::PromptRequested { question: q, .. } if q == question)
            })
            .cloned()
            .collect();
        let ids: Vec<&String> = requested
            .iter()
            .filter_map(|event| match event {
                Event::PromptRequested { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        let answered: Vec<Event> = events
            .iter()
            .filter(|event| matches!(event, Event::PromptAnswered { id, .. } if ids.contains(&id)))
            .cloned()
            .collect();
        (requested, answered)
    }

    fn requested_id(requested: &[Event]) -> String {
        let [Event::PromptRequested { id, .. }] = requested else {
            panic!("{requested:?}");
        };
        id.clone()
    }

    #[test]
    fn new_prompt_id_has_prefix_and_is_unique() {
        let first = new_prompt_id();
        assert!(first.starts_with("prompt_"));
        assert_ne!(first, new_prompt_id());
    }

    #[test]
    #[serial]
    fn human_present_is_false_only_for_headless_and_acp_runs() {
        let _headless_off = AtomicFlagGuard::set(&HEADLESS, false);
        let _acp_off = AtomicFlagGuard::set(&ACP_SERVER, false);
        assert!(human_present());

        let headless_on = AtomicFlagGuard::set(&HEADLESS, true);
        assert!(!human_present());
        drop(headless_on);
        assert!(human_present());

        let _acp_on = AtomicFlagGuard::set(&ACP_SERVER, true);
        assert!(!human_present());
    }

    #[test]
    #[serial]
    fn ask_publishes_requested_then_answered() {
        let sink = RecordingSink::install();
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Confirmed(false)]);
        let question = "ask-ok-6b3";
        let req = PromptRequest {
            question: question.to_string(),
            default: Some(json!(true)),
            ..request(PromptAction::Confirm, None)
        };

        let answer = ask(&prompter, req.clone()).unwrap();

        assert_eq!(answer, PromptAnswer::Confirmed(false));
        let (requested, answered) = prompt_events(sink.drain(), question);
        assert_eq!(requested, vec![req.requested_event()]);
        let [Event::PromptRequested { default, .. }] = requested.as_slice() else {
            panic!("{requested:?}");
        };
        assert_eq!(*default, Some(json!(true)));
        assert_eq!(
            answered,
            vec![Event::PromptAnswered {
                id: req.id,
                answer: "no".to_string(),
                auto: false,
            }]
        );
    }

    #[test]
    #[serial]
    fn ask_closes_the_pair_with_an_error_marker_when_the_prompter_fails() {
        let sink = RecordingSink::install();
        let prompter = ScriptedPrompter::failing("no tty here");
        let req = PromptRequest::new(PromptAction::Input, "ask-err-2m9", Origin::detached());

        let err = ask(&prompter, req).unwrap_err();

        assert_eq!(err.to_string(), "no tty here");
        let (requested, answered) = prompt_events(sink.drain(), "ask-err-2m9");
        let id = requested_id(&requested);
        assert_eq!(
            answered,
            vec![Event::PromptAnswered {
                id,
                answer: "<error>".to_string(),
                auto: false,
            }]
        );
    }

    #[test]
    #[serial]
    fn ask_returns_a_password_but_publishes_it_redacted() {
        let sink = RecordingSink::install();
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Text("hunter2".to_string())]);
        let req = PromptRequest::new(PromptAction::Password, "secret? pw-4j", Origin::detached());

        let answer = ask(&prompter, req).unwrap();

        assert_eq!(answer, PromptAnswer::Text("hunter2".to_string()));
        let recorded = prompter.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].action, PromptAction::Password);
        let (requested, answered) = prompt_events(sink.drain(), "secret? pw-4j");
        let id = requested_id(&requested);
        assert_eq!(
            answered,
            vec![Event::PromptAnswered {
                id,
                answer: "<redacted>".to_string(),
                auto: false,
            }]
        );
        assert!(!format!("{requested:?}{answered:?}").contains("hunter2"));
    }

    #[test]
    fn memory_answers_only_a_remembered_tool_confirm() {
        let memory = PromptMemory::default();
        let req = request(PromptAction::Confirm, Some("fake_tool"));
        assert_eq!(memory.auto_answer(&req), None);

        memory.remember("fake_tool", "proceed? pm-4t8");
        assert_eq!(memory.auto_answer(&req), Some(true));
        assert_eq!(
            memory.auto_answer(&request(PromptAction::Confirm, None)),
            None
        );
        assert_eq!(
            memory.auto_answer(&request(PromptAction::Select, Some("fake_tool"))),
            None
        );
        assert_eq!(
            memory.auto_answer(&request(PromptAction::Confirm, Some("other_tool"))),
            None
        );

        memory.clear();
        assert_eq!(memory.auto_answer(&req), None);
    }

    #[test]
    fn headless_reply_is_the_structured_needs_human_json() {
        for (action, name) in [
            (PromptAction::Confirm, "confirm"),
            (PromptAction::Select, "select"),
            (PromptAction::Checkbox, "checkbox"),
            (PromptAction::Input, "input"),
            (PromptAction::Password, "password"),
        ] {
            let reply = headless_reply(&request(action, None));
            let expected = json!({
                "needs_human": true,
                "action": name,
                "question": "proceed? pm-4t8",
                "options": ["a", "b"],
                "guidance": "No human is present. Apply a sensible default or abort the task.",
            });
            assert_eq!(reply, expected);
            assert_eq!(reply.to_string(), expected.to_string());
        }
    }

    #[test]
    fn headless_prompter_carries_the_reply_as_text() {
        let req = request(PromptAction::Select, None);
        let expected = headless_reply(&req).to_string();
        let answer = HeadlessPrompter.prompt(req).unwrap();
        assert_eq!(answer, PromptAnswer::Text(expected));
    }

    #[test]
    fn summary_redacts_a_password_and_names_the_rest() {
        let text = PromptAnswer::Text("hunter2".to_string());
        assert_eq!(text.summary(PromptAction::Password), "<redacted>");
        assert_eq!(text.summary(PromptAction::Input), "hunter2");
        assert_eq!(
            PromptAnswer::Confirmed(true).summary(PromptAction::Confirm),
            "yes"
        );
        assert_eq!(
            PromptAnswer::Confirmed(false).summary(PromptAction::Confirm),
            "no"
        );
        assert_eq!(
            PromptAnswer::Selected("b".to_string()).summary(PromptAction::Select),
            "b"
        );
        assert_eq!(
            PromptAnswer::Checked(vec!["a".to_string(), "b".to_string()])
                .summary(PromptAction::Checkbox),
            r#"["a","b"]"#
        );
        assert_eq!(
            PromptAnswer::Cancelled.summary(PromptAction::Confirm),
            "<cancelled>"
        );
    }

    #[test]
    fn accessors_return_the_matching_shape() {
        assert!(PromptAnswer::Confirmed(true).into_confirmed().unwrap());
        assert_eq!(
            PromptAnswer::Selected("a".to_string())
                .into_selected()
                .unwrap(),
            "a"
        );
        assert_eq!(
            PromptAnswer::Checked(vec!["a".to_string()])
                .into_checked()
                .unwrap(),
            vec!["a".to_string()]
        );
        assert_eq!(
            PromptAnswer::Text("t".to_string()).into_text().unwrap(),
            "t"
        );
    }

    #[test]
    fn accessors_reject_a_mismatch_and_a_cancel() {
        let err = PromptAnswer::Text("t".to_string())
            .into_confirmed()
            .unwrap_err();
        assert_eq!(err.to_string(), "Unexpected answer to the confirm prompt");
        let err = PromptAnswer::Confirmed(true).into_selected().unwrap_err();
        assert_eq!(err.to_string(), "Unexpected answer to the select prompt");
        let err = PromptAnswer::Confirmed(true).into_checked().unwrap_err();
        assert_eq!(err.to_string(), "Unexpected answer to the checkbox prompt");
        let err = PromptAnswer::Confirmed(true).into_text().unwrap_err();
        assert_eq!(err.to_string(), "Unexpected answer to the input prompt");
        for err in [
            PromptAnswer::Cancelled.into_confirmed().unwrap_err(),
            PromptAnswer::Cancelled.into_selected().unwrap_err(),
            PromptAnswer::Cancelled.into_checked().unwrap_err(),
            PromptAnswer::Cancelled.into_text().unwrap_err(),
        ] {
            assert_eq!(err.to_string(), "Operation was canceled by the user");
        }
    }

    #[test]
    fn scripted_prompter_replays_in_order_and_records_requests() {
        let prompter = ScriptedPrompter::new(vec![
            PromptAnswer::Confirmed(false),
            PromptAnswer::Text("second".to_string()),
        ]);
        let first = request(PromptAction::Confirm, None);
        let second = request(PromptAction::Input, None);
        assert_eq!(
            prompter.prompt(first.clone()).unwrap(),
            PromptAnswer::Confirmed(false)
        );
        assert_eq!(
            prompter.prompt(second.clone()).unwrap(),
            PromptAnswer::Text("second".to_string())
        );
        assert_eq!(prompter.requests(), vec![first, second]);
    }

    #[test]
    fn failing_prompter_errors_without_recording_the_request() {
        let prompter = ScriptedPrompter::failing("boom");
        let err = prompter
            .prompt(request(PromptAction::Input, None))
            .unwrap_err();
        assert_eq!(err.to_string(), "boom");
        assert!(prompter.requests().is_empty());
    }
}
