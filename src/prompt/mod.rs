//! A question for the human: the engine asks through a `Prompter`, so one
//! call site can draw an inquire widget, answer for an absent human, or
//! replay scripted answers in a test. Every prompt is published on the
//! event bus as it is asked and as it is answered.

use crate::events::{Event, Origin, bus};
use crate::utils::{ACP_SERVER, HEADLESS, IS_STDOUT_TERMINAL};
use anyhow::{Result, anyhow, bail};
use inquire::{Confirm, InquireError, MultiSelect, Password, PasswordDisplayMode, Select, Text};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub(crate) const CUSTOM_MULTI_CHOICE_ANSWER_OPTION: &str = "Other (custom)";
const YES_FOR_SESSION: &str = "Yes for this session";
/// The menu a tool's confirm is asked as on the terminal.
const TOOL_CONFIRM_ROWS: [&str; 3] = ["Yes", "No", YES_FOR_SESSION];
/// The published answer of a prompt a run with no human refused.
pub(crate) const NEEDS_HUMAN_MARKER: &str = "<needs_human>";
const REDACTED_MARKER: &str = "<redacted>";

/// Whether a human can answer a prompt in this run. A headless run and the
/// ACP server have none; every other run has a terminal.
pub(crate) fn human_present() -> bool {
    !(HEADLESS.load(Ordering::SeqCst) || ACP_SERVER.load(Ordering::SeqCst))
}

/// Whether a question has somewhere to be shown: the full-screen UI's
/// modal, or a terminal on stdout for an inquire widget. Stdin does not
/// matter: when it is not a terminal the widget reads its keys from
/// `/dev/tty` instead.
fn terminal_reachable() -> bool {
    #[cfg(test)]
    let on_tty =
        crate::config::conflict::prompt_script::forced_stdout_tty().unwrap_or(*IS_STDOUT_TERMINAL);
    #[cfg(not(test))]
    let on_tty = *IS_STDOUT_TERMINAL;
    crate::tui::active() || on_tty
}

/// Who a prompter's questions can reach. A run with no human is
/// `Headless` whether or not it has a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    Human,
    /// A human, but nowhere to show the question.
    NoTerminal,
    /// A headless run or the ACP server.
    Headless,
}

/// The run's reach, read from its flags and terminal rather than from the
/// prompter's type: a command may build its own inquire prompter while
/// the full-screen UI owns the screen.
fn ambient_reach() -> Reach {
    if !human_present() {
        Reach::Headless
    } else if !terminal_reachable() {
        Reach::NoTerminal
    } else {
        Reach::Human
    }
}

/// Whether a question may be put to `prompter`. A run with no human still
/// asks when there is a terminal, so the refusal it gets is the
/// prompter's typed [`NeedsHuman`].
pub(crate) fn can_ask(prompter: &dyn Prompter) -> bool {
    match prompter.reach() {
        Reach::Human => true,
        Reach::NoTerminal => false,
        Reach::Headless => terminal_reachable(),
    }
}

/// Refuses `action` when [`can_ask`] says no question can be shown; every
/// site refuses here so the message has one shape. `remedy` follows
/// "Re-run in a terminal" directly, so it begins with its own separator
/// (" or pass --force.", ", with --yes (updates).") or is just a period.
pub(crate) fn require_human(
    prompter: &dyn Prompter,
    action: impl fmt::Display,
    remedy: impl fmt::Display,
) -> Result<()> {
    if can_ask(prompter) {
        return Ok(());
    }
    Err(anyhow!(
        "Refusing to {action} non-interactively. Re-run in a terminal{remedy}"
    ))
}

pub(crate) const NO_HUMAN_FOR_OAUTH: &str =
    "OAuth sign-in needs an interactive terminal; run it without --headless or --acp-server";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAction {
    Confirm,
    /// The prompters append a free-text "Other (custom)" entry after the
    /// offered options, so the answer need not be one of them, unless the
    /// request is `closed`.
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
    /// A line shown under the question; rendering detail, so it is not
    /// published with the request. The TUI modal renders it for every
    /// kind. The inquire prompter renders it for Input and Password,
    /// whose widgets have a help line, and leaves a Confirm's help to the
    /// transcript the caller printed.
    pub help: Option<String>,
    /// A select whose answer must be one of `options`: no "Other
    /// (custom)" row is offered. Not published either.
    pub closed: bool,
    /// An answer that must not be published, though it is not masked
    /// while typed. Its default is withheld from the published request
    /// too.
    pub sensitive: bool,
    /// The question exactly as a tool sent it, before it was escaped for
    /// display; `None` for Coyote's own prompts. "Yes for this session"
    /// matches on it, so two lines that display alike are still told
    /// apart. Not published.
    pub raw_question: Option<String>,
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
            help: None,
            closed: false,
            sensitive: false,
            raw_question: None,
        }
    }

    pub(crate) fn requested_event(&self) -> Event {
        Event::PromptRequested {
            id: self.id.clone(),
            action: self.action,
            question: self.question.clone(),
            options: self.options.clone(),
            default: if self.sensitive || self.action == PromptAction::Password {
                None
            } else {
                self.default.clone()
            },
            tool: self.tool.clone(),
            call_id: self.call_id.clone(),
            job_id: self.job_id.clone(),
            origin: self.origin.clone(),
        }
    }

    /// The event that closes this request, published wherever it is
    /// answered so a sensitive answer is redacted at every site.
    pub(crate) fn answered_event(&self, answer: &PromptAnswer, auto: bool) -> Event {
        let answer = match answer {
            PromptAnswer::Text(_) if self.sensitive => REDACTED_MARKER.to_string(),
            answer => answer.summary(self.action),
        };
        Event::PromptAnswered {
            id: self.id.clone(),
            answer,
            auto,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptAnswer {
    Confirmed(bool),
    Selected(String),
    Checked(Vec<String>),
    Text(String),
    /// Closed with Esc: no answer to this question.
    Cancelled,
    /// Closed with Ctrl-C: no answer, and the human wants the whole
    /// command stopped. Reads as [`PromptAnswer::Cancelled`] except where
    /// a cancel would carry on without the answer.
    Interrupted,
    /// A run with no human declined to answer; the caller decides what
    /// the refusal becomes.
    NeedsHuman,
}

impl PromptAnswer {
    /// The answer as published text; a password never leaves the prompt.
    pub fn summary(&self, action: PromptAction) -> String {
        match self {
            PromptAnswer::Confirmed(true) => "yes".to_string(),
            PromptAnswer::Confirmed(false) => "no".to_string(),
            PromptAnswer::Selected(choice) => choice.clone(),
            PromptAnswer::Checked(choices) => json!(choices).to_string(),
            PromptAnswer::Text(_) if action == PromptAction::Password => {
                REDACTED_MARKER.to_string()
            }
            PromptAnswer::Text(text) => text.clone(),
            PromptAnswer::Cancelled | PromptAnswer::Interrupted => "<cancelled>".to_string(),
            PromptAnswer::NeedsHuman => NEEDS_HUMAN_MARKER.to_string(),
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
            PromptAnswer::Cancelled => PromptCancelled { interrupted: false }.into(),
            PromptAnswer::Interrupted => PromptCancelled { interrupted: true }.into(),
            PromptAnswer::NeedsHuman => NeedsHuman::Prompt(expected).into(),
            _ => anyhow!("Unexpected answer to the {} prompt", expected.name()),
        }
    }
}

/// The error a cancelled prompt becomes for a caller that wanted an
/// answer rather than the cancel itself.
#[derive(Debug)]
pub(crate) struct PromptCancelled {
    interrupted: bool,
}

impl PromptCancelled {
    /// Whether `err` is, or wraps through `context`, a cancelled prompt,
    /// whichever key closed it.
    pub(crate) fn caused(err: &anyhow::Error) -> bool {
        err.chain()
            .any(|cause| cause.downcast_ref::<PromptCancelled>().is_some())
    }

    /// Whether `err` is, or wraps through `context`, a prompt closed with
    /// Ctrl-C.
    pub(crate) fn interrupted(err: &anyhow::Error) -> bool {
        err.chain().any(|cause| {
            cause
                .downcast_ref::<PromptCancelled>()
                .is_some_and(|cancel| cancel.interrupted)
        })
    }
}

impl fmt::Display for PromptCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Operation was canceled by the user")
    }
}

impl std::error::Error for PromptCancelled {}

/// Answers a [`PromptRequest`] on behalf of the human. Synchronous: callers
/// sit on the REPL thread or inside a blocking tool, and a prompter that
/// has no human to ask answers at once.
pub trait Prompter: Send + Sync + fmt::Debug {
    fn prompt(&self, req: PromptRequest) -> Result<PromptAnswer>;

    fn reach(&self) -> Reach {
        ambient_reach()
    }

    /// Lets a test look at the seated prompter behind the trait object.
    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any;
}

/// Confirms a tool already waved through with "Yes for this session", keyed
/// by tool and the question as the tool sent it. Shared by the context that
/// consults it and the prompter that records into it; cleared when the
/// session exits.
#[derive(Debug, Default)]
pub struct PromptMemory {
    remembered: Mutex<HashMap<String, bool>>,
}

impl PromptMemory {
    fn key(req: &PromptRequest) -> Option<String> {
        let tool = req.tool.as_deref()?;
        let question = req.raw_question.as_deref().unwrap_or(&req.question);
        Some(format!("{tool}\u{1f}{question}"))
    }

    /// Records a yes for `req`; a request with no tool is not remembered.
    pub fn remember(&self, req: &PromptRequest) {
        let Some(key) = Self::key(req) else {
            return;
        };
        self.remembered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, true);
    }

    /// The remembered answer for a tool confirm; `None` for any other prompt
    /// or when nothing was remembered.
    pub fn auto_answer(&self, req: &PromptRequest) -> Option<bool> {
        if req.action != PromptAction::Confirm {
            return None;
        }
        let key = Self::key(req)?;
        self.remembered
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
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

/// `text` without the whitespace-only lines it starts with.
pub(crate) fn trim_leading_blank_lines(text: &str) -> &str {
    let mut rest = text;
    while let Some((line, tail)) = rest.split_once('\n') {
        if !line.trim().is_empty() {
            break;
        }
        rest = tail;
    }
    rest
}

/// Asks through `prompter` and publishes the prompt as requested and as
/// answered, so a prompter stays a renderer and the bus stays the observer.
/// A prompter failure still closes the pair, with `<error>` as the answer,
/// or `<needs_human>` when a run with no human refused the prompt.
pub fn ask(prompter: &dyn Prompter, req: PromptRequest) -> Result<PromptAnswer> {
    bus().publish(req.requested_event());
    let answer = match prompter.prompt(req.clone()) {
        Ok(answer) => answer,
        Err(err) => {
            let answer = if NeedsHuman::caused(&err) {
                NEEDS_HUMAN_MARKER
            } else {
                "<error>"
            };
            bus().publish(Event::PromptAnswered {
                id: req.id,
                answer: answer.to_string(),
                auto: false,
            });
            return Err(err);
        }
    };
    bus().publish(req.answered_event(&answer, false));
    Ok(answer)
}

pub(crate) fn confirm(
    prompter: &dyn Prompter,
    question: impl Into<String>,
    default: bool,
) -> Result<bool> {
    let req = PromptRequest {
        default: Some(json!(default)),
        ..PromptRequest::new(PromptAction::Confirm, question, Origin::detached())
    };
    ask(prompter, req).and_then(PromptAnswer::into_confirmed)
}

/// [`confirm`] for a question where closing it is itself an answer:
/// `Ok(None)` when the human cancelled instead of answering. An
/// interrupt is still an error, so the command stops.
pub(crate) fn confirm_or_cancel(
    prompter: &dyn Prompter,
    question: impl Into<String>,
    default: bool,
) -> Result<Option<bool>> {
    let req = PromptRequest {
        default: Some(json!(default)),
        ..PromptRequest::new(PromptAction::Confirm, question, Origin::detached())
    };
    match ask(prompter, req)? {
        PromptAnswer::Cancelled => Ok(None),
        answer => answer.into_confirmed().map(Some),
    }
}

/// A confirm whose context was printed to stdout before the question.
/// Under the TUI that output lands in the transcript, so the same text
/// rides along as the request's help and the modal shows it as well.
pub(crate) fn confirm_with_help(
    prompter: &dyn Prompter,
    question: impl Into<String>,
    default: bool,
    help: &str,
) -> Result<bool> {
    let req = PromptRequest {
        default: Some(json!(default)),
        help: Some(help.to_string()),
        ..PromptRequest::new(PromptAction::Confirm, question, Origin::detached())
    };
    ask(prompter, req).and_then(PromptAnswer::into_confirmed)
}

/// A select whose answer must be one of `options`; an answer outside them
/// is rejected here so no caller has to, and a select with nothing to
/// offer is refused before any prompter sees it.
pub(crate) fn select_closed(
    prompter: &dyn Prompter,
    question: &str,
    options: Vec<String>,
) -> Result<String> {
    match select_closed_or_cancel(prompter, question, options)? {
        Some(choice) => Ok(choice),
        None => Err(PromptAnswer::Cancelled.mismatch(PromptAction::Select)),
    }
}

/// [`select_closed`] for a menu where closing the question is itself an
/// answer: `Ok(None)` when the human cancelled instead of choosing. An
/// interrupt is still an error, so the command stops.
pub(crate) fn select_closed_or_cancel(
    prompter: &dyn Prompter,
    question: &str,
    options: Vec<String>,
) -> Result<Option<String>> {
    if options.is_empty() {
        bail!("'{question}' has no options to choose from");
    }
    let req = PromptRequest {
        options: options.clone(),
        closed: true,
        ..PromptRequest::new(PromptAction::Select, question, Origin::detached())
    };
    let choice = match ask(prompter, req)? {
        PromptAnswer::Cancelled => return Ok(None),
        answer => answer.into_selected()?,
    };
    if !options.contains(&choice) {
        bail!("'{choice}' is not one of the offered options");
    }
    Ok(Some(choice))
}

pub(crate) fn input(
    prompter: &dyn Prompter,
    question: &str,
    default: Option<&str>,
    help: Option<&str>,
) -> Result<String> {
    text_input(prompter, question, default, help, false)
}

fn text_input(
    prompter: &dyn Prompter,
    question: &str,
    default: Option<&str>,
    help: Option<&str>,
    sensitive: bool,
) -> Result<String> {
    let req = PromptRequest {
        default: default.map(|value| json!(value)),
        help: help.map(str::to_string),
        sensitive,
        ..PromptRequest::new(PromptAction::Input, question, Origin::detached())
    };
    ask(prompter, req).and_then(PromptAnswer::into_text)
}

/// [`input`] asked until the answer is not blank; a blank one is asked
/// again with the requirement in front of the label. The answer is
/// handed back as typed, surrounding whitespace included.
pub(crate) fn required_input(
    prompter: &dyn Prompter,
    label: &str,
    default: Option<&str>,
    help: Option<&str>,
) -> Result<String> {
    required_text_input(prompter, label, default, help, false)
}

/// [`required_input`] for a value that must not be published, such as a
/// secret the human types in the clear.
pub(crate) fn required_sensitive_input(prompter: &dyn Prompter, label: &str) -> Result<String> {
    required_text_input(prompter, label, None, None, true)
}

fn required_text_input(
    prompter: &dyn Prompter,
    label: &str,
    default: Option<&str>,
    help: Option<&str>,
    sensitive: bool,
) -> Result<String> {
    let mut question = label.to_string();
    loop {
        let answer = text_input(prompter, &question, default, help, sensitive)?;
        if answer.trim().is_empty() {
            question = reask("This field is required.", label);
            continue;
        }
        return Ok(answer);
    }
}

/// The default an input offers, as the text the widget shows: a
/// string as it is, any other value (a number, a bool) rendered. An
/// empty string offers nothing, like no default at all.
pub(crate) fn input_default_text(req: &PromptRequest) -> Option<String> {
    match req.default.as_ref()? {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// The text an input shows in place of a default whose raw form carries
/// escapes or line breaks; `None` when the default can be shown as it is.
fn input_hint(default: &str) -> Option<String> {
    let shown = crate::tui::plain_text(default).replace('\n', " ");
    (shown != default).then_some(shown)
}

/// The question asked again after a rejected answer. The label stays so
/// the field keeps its name under the TUI, where the modal is all the
/// user sees; the reason comes first so it is read before answering.
pub(crate) fn reask(reason: &str, label: &str) -> String {
    format!("{reason} {label}")
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

    #[cfg(test)]
    pub(crate) fn memory(&self) -> &Arc<PromptMemory> {
        &self.memory
    }

    /// The rows of a select: the offered options, plus the free-text row
    /// unless the request is closed.
    fn select_options(req: &PromptRequest) -> Vec<String> {
        let mut options = req.options.clone();
        if !req.closed {
            options.push(CUSTOM_MULTI_CHOICE_ANSWER_OPTION.to_string());
        }
        options
    }

    /// The question as the widget shows it: a tool's carries the tool's
    /// name, so its prompt cannot pass for one of Coyote's own. Leading
    /// blank lines are dropped so the label sits next to the text.
    fn prompt_text(req: &PromptRequest) -> String {
        match req.tool.as_deref() {
            Some(tool) => format!("{tool} asks: {}", trim_leading_blank_lines(&req.question)),
            None => req.question.clone(),
        }
    }

    /// A closed command menu is inquire's bare widget; a tool's select,
    /// closed or not, keeps the REPL styling (no type-to-filter, the
    /// movement hint as help), and only an open one offers the custom row.
    fn select(req: &PromptRequest) -> Result<PromptAnswer, InquireError> {
        let question = Self::prompt_text(req);
        let select = Select::new(&question, Self::select_options(req));
        if Self::is_command_menu(req) {
            return select.prompt().map(PromptAnswer::Selected);
        }
        let mut answer = select
            .without_filtering()
            .with_help_message("↑↓ to move, enter to select") // debris-exempt: inquire's select help minus the filter hint, as the REPL showed it
            .prompt()?;
        if !req.closed && answer == CUSTOM_MULTI_CHOICE_ANSWER_OPTION {
            answer = Text::new("Custom response:").prompt()?;
        }
        Ok(PromptAnswer::Selected(answer))
    }

    /// A closed select no tool asked for is a command menu, which takes
    /// inquire's bare select; a tool's select keeps the REPL styling its
    /// selects always had.
    fn is_command_menu(req: &PromptRequest) -> bool {
        req.closed && req.tool.is_none()
    }

    fn input(req: &PromptRequest) -> Result<PromptAnswer, InquireError> {
        let default = input_default_text(req);
        let hint = default.as_deref().and_then(input_hint);
        let question = Self::prompt_text(req);
        let mut text = Text::new(&question);
        match (hint.as_deref(), default.as_deref()) {
            (Some(hint), _) => text = text.with_placeholder(hint),
            (None, Some(default)) => text = text.with_default(default),
            (None, None) => {}
        }
        if let Some(help) = req.help.as_deref() {
            text = text.with_help_message(help);
        }
        let answer = text.prompt()?;
        // A placeholder is only drawn, so Enter alone is mapped to the raw
        // default here, as inquire does for a default it was given.
        Ok(PromptAnswer::Text(match default {
            Some(default) if hint.is_some() && answer.is_empty() => default,
            _ => answer,
        }))
    }

    fn password(req: &PromptRequest) -> Result<PromptAnswer, InquireError> {
        let question = Self::prompt_text(req);
        let mut password = Password::new(&question)
            .without_confirmation()
            .with_display_mode(PasswordDisplayMode::Masked);
        if let Some(help) = req.help.as_deref() {
            password = password.with_help_message(help);
        }
        password.prompt().map(PromptAnswer::Text)
    }

    fn confirm(&self, req: &PromptRequest) -> Result<PromptAnswer, InquireError> {
        if req.tool.is_none() {
            let default = req
                .default
                .as_ref()
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let answer = Confirm::new(&req.question).with_default(default).prompt()?;
            return Ok(PromptAnswer::Confirmed(answer));
        }
        let question = Self::prompt_text(req);
        let choice = Select::new(&question, TOOL_CONFIRM_ROWS.to_vec())
            .without_filtering()
            .with_starting_cursor(Self::tool_confirm_starting_row(req))
            .prompt()?;
        if choice == YES_FOR_SESSION {
            self.memory.remember(req);
        }
        Ok(PromptAnswer::Confirmed(choice != "No"))
    }

    /// The row of [`TOOL_CONFIRM_ROWS`] the cursor starts on, so Enter
    /// alone answers No unless the tool asked for a default of yes.
    fn tool_confirm_starting_row(req: &PromptRequest) -> usize {
        match req.default.as_ref().and_then(Value::as_bool) {
            Some(true) => 0,
            _ => 1,
        }
    }
}

impl Default for InquirePrompter {
    fn default() -> Self {
        Self::new(Arc::new(PromptMemory::default()))
    }
}

impl Prompter for InquirePrompter {
    fn prompt(&self, req: PromptRequest) -> Result<PromptAnswer> {
        crate::utils::drain_stale_tty_input();
        let answer = match req.action {
            PromptAction::Select => Self::select(&req),
            PromptAction::Confirm => self.confirm(&req),
            PromptAction::Input => Self::input(&req),
            PromptAction::Checkbox => {
                MultiSelect::new(&Self::prompt_text(&req), req.options.clone())
                    .prompt()
                    .map(PromptAnswer::Checked)
            }
            PromptAction::Password => Self::password(&req),
        };
        match answer {
            Ok(answer) => Ok(answer),
            Err(InquireError::OperationCanceled) => Ok(PromptAnswer::Cancelled),
            Err(InquireError::OperationInterrupted) => Ok(PromptAnswer::Interrupted),
            Err(err) => Err(err.into()),
        }
    }

    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Answers for a run with no human. A confirm is answered with
/// [`PromptAnswer::NeedsHuman`], which `handle_user_tool` turns into the
/// structured `needs_human` JSON the model is expected to act on, and
/// every other confirm consumer reads as the [`NeedsHuman`] refusal.
/// Every other kind is refused with [`NeedsHuman`], because a
/// text-shaped answer would be taken as the human's: a role would be
/// saved under the JSON, or an MCP server would receive it as an
/// argument.
#[derive(Debug, Default)]
pub struct HeadlessPrompter;

/// The refusal a run with no human gives a prompt whose answer would
/// otherwise be read as text, or an action that would have to ask one.
#[derive(Debug)]
pub(crate) enum NeedsHuman {
    Prompt(PromptAction),
    /// A `kind` named `name` would have been created behind a prompt;
    /// the gate refuses before any prompter is reached.
    Creating {
        kind: &'static str,
        name: String,
    },
}

const NO_HUMAN_ATTACHED: &str =
    "and none is attached to this run; run it without --headless or --acp-server";

impl NeedsHuman {
    pub(crate) fn for_creating(kind: &'static str, name: &str) -> Self {
        Self::Creating {
            kind,
            name: name.to_string(),
        }
    }

    /// Whether `err` is, or wraps through `context`, this refusal.
    pub(crate) fn caused(err: &anyhow::Error) -> bool {
        err.chain()
            .any(|cause| cause.downcast_ref::<NeedsHuman>().is_some())
    }
}

impl fmt::Display for NeedsHuman {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prompt(action) => write!(
                f,
                "The {} prompt needs a human {NO_HUMAN_ATTACHED}",
                action.name()
            ),
            Self::Creating { kind, name } => write!(
                f,
                "Creating {kind} '{name}' needs a human to answer the prompt {NO_HUMAN_ATTACHED}"
            ),
        }
    }
}

impl std::error::Error for NeedsHuman {}

/// The JSON a headless run hands back in place of a human answer.
pub(crate) fn headless_reply(req: &PromptRequest) -> Value {
    json!({
        "needs_human": true,
        "action": req.action.name(),
        "question": req.question,
        "options": req.options,
        "guidance": crate::function::user_interaction::HEADLESS_GUIDANCE,
    })
}

impl Prompter for HeadlessPrompter {
    fn prompt(&self, req: PromptRequest) -> Result<PromptAnswer> {
        match req.action {
            PromptAction::Confirm => Ok(PromptAnswer::NeedsHuman),
            action => Err(NeedsHuman::Prompt(action).into()),
        }
    }

    fn reach(&self) -> Reach {
        Reach::Headless
    }

    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any {
        self
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

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The requested events carrying `question` and the answered events that
/// share their id, out of a drain other tests may have written to.
#[cfg(test)]
pub(crate) fn prompt_events(events: Vec<Event>, question: &str) -> (Vec<Event>, Vec<Event>) {
    let requested: Vec<Event> = events
        .iter()
        .filter(
            |event| matches!(event, Event::PromptRequested { question: q, .. } if q == question),
        )
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::conflict::prompt_script;
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
    fn help_and_closed_are_not_published_with_the_request() {
        let bare = request(PromptAction::Select, None);
        let decorated = PromptRequest {
            help: Some("pick carefully".to_string()),
            closed: true,
            ..bare.clone()
        };
        assert_eq!(decorated.requested_event(), bare.requested_event());
    }

    #[test]
    fn a_closed_select_offers_no_custom_row_and_an_open_one_does() {
        let open = request(PromptAction::Select, None);
        let closed = PromptRequest {
            closed: true,
            ..open.clone()
        };
        assert_eq!(
            InquirePrompter::select_options(&open),
            vec!["a", "b", CUSTOM_MULTI_CHOICE_ANSWER_OPTION]
        );
        assert_eq!(InquirePrompter::select_options(&closed), vec!["a", "b"]);
    }

    #[test]
    fn a_tools_prompt_names_the_tool_and_others_show_the_bare_question() {
        assert_eq!(
            InquirePrompter::prompt_text(&request(PromptAction::Password, Some("fs_cat"))),
            "fs_cat asks: proceed? pm-4t8"
        );
        assert_eq!(
            InquirePrompter::prompt_text(&request(PromptAction::Confirm, None)),
            "proceed? pm-4t8"
        );
    }

    #[test]
    fn a_tools_prompt_drops_leading_blank_lines_and_keeps_the_others() {
        let req = PromptRequest {
            tool: Some("fs_rm".to_string()),
            ..PromptRequest::new(
                PromptAction::Confirm,
                "\n  \nDelete?\n\n/tmp/q4b",
                Origin::detached(),
            )
        };
        assert_eq!(
            InquirePrompter::prompt_text(&req),
            "fs_rm asks: Delete?\n\n/tmp/q4b"
        );
    }

    /// Enter alone on a tool's confirm menu answers No; only the tool's
    /// own default of yes starts the cursor on Yes.
    #[test]
    fn a_tool_confirm_starts_on_no_unless_the_tool_defaults_to_yes() {
        let starting_row = |default: Option<Value>| {
            let req = PromptRequest {
                default,
                ..request(PromptAction::Confirm, Some("tool-x"))
            };
            TOOL_CONFIRM_ROWS[InquirePrompter::tool_confirm_starting_row(&req)]
        };
        assert_eq!(starting_row(None), "No");
        assert_eq!(starting_row(Some(json!(false))), "No");
        assert_eq!(starting_row(Some(json!("yes"))), "No");
        assert_eq!(starting_row(Some(json!(true))), "Yes");
    }

    #[test]
    fn an_input_default_that_is_a_number_is_offered_as_text() {
        let input = |default: Option<Value>| PromptRequest {
            default,
            ..request(PromptAction::Input, None)
        };
        assert_eq!(
            input_default_text(&input(Some(json!(1000)))).as_deref(),
            Some("1000")
        );
        assert_eq!(
            input_default_text(&input(Some(json!(true)))).as_deref(),
            Some("true")
        );
        assert_eq!(
            input_default_text(&input(Some(json!("plain")))).as_deref(),
            Some("plain")
        );
        assert_eq!(input_default_text(&input(Some(Value::Null))), None);
        assert_eq!(input_default_text(&input(None)), None);
    }

    #[test]
    fn an_empty_string_input_default_is_no_default() {
        let req = PromptRequest {
            default: Some(json!("")),
            ..request(PromptAction::Input, Some("tool-x"))
        };
        assert_eq!(input_default_text(&req), None);
    }

    /// An ordinary default keeps inquire's own default; one with escapes
    /// or line breaks is shown scrubbed instead.
    #[test]
    fn an_input_default_with_escapes_is_hinted_scrubbed() {
        assert_eq!(input_hint("plain default"), None);
        assert_eq!(
            input_hint("\x1b]0;title\x07keep-me").as_deref(),
            Some("keep-me")
        );
        assert_eq!(input_hint("one\ntwo").as_deref(), Some("one two"));
    }

    /// Which widget a select gets depends on who asks: a closed command
    /// menu takes inquire's bare select, a tool's open select keeps the
    /// REPL styling its selects always had plus the custom row. The
    /// widget only exists on a terminal, so the two branches are pinned
    /// on the source, split at the guard.
    #[test]
    fn an_open_tool_select_keeps_the_filterless_help_styling_and_the_custom_row() {
        let closed = |tool: Option<&str>| PromptRequest {
            closed: true,
            ..request(PromptAction::Select, tool)
        };
        let tool_select = request(PromptAction::Select, Some("tool-x"));
        assert!(InquirePrompter::is_command_menu(&closed(None)));
        assert!(!InquirePrompter::is_command_menu(&closed(Some("tool-x"))));
        assert!(!InquirePrompter::is_command_menu(&tool_select));
        assert_eq!(
            InquirePrompter::select_options(&tool_select),
            vec!["a", "b", CUSTOM_MULTI_CHOICE_ANSWER_OPTION]
        );
        assert!(!InquirePrompter::is_command_menu(&request(
            PromptAction::Select,
            None
        )));

        let source = include_str!("mod.rs").replace("\r\n", "\n");
        let start = source
            .find("fn select(req: &PromptRequest)")
            .expect("no `fn select(req: &PromptRequest)` in prompt/mod.rs");
        let body = &source[start..];
        let body = &body[..body
            .find("\n    }\n")
            .expect("`fn select` has no closing brace at four spaces of indent")];
        let (_, after_guard) = body
            .split_once("if Self::is_command_menu(req) {")
            .expect("`fn select` has no `if Self::is_command_menu(req) {` guard");
        let (bare, styled) = after_guard
            .split_once("\n        }\n")
            .expect("the command-menu guard in `fn select` has no closing brace at eight spaces");
        assert!(bare.contains("select.prompt()"), "{bare}");
        assert!(styled.contains(".prompt()"), "{styled}");
        for needle in [
            ".without_filtering()",
            "\"↑↓ to move, enter to select\"", // debris-exempt: inquire's select help minus the filter hint, as the REPL showed it
            "\"Custom response:\"",
        ] {
            assert!(!bare.contains(needle), "{needle} in:\n{bare}");
            assert!(styled.contains(needle), "{needle} missing from:\n{styled}");
        }
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

    /// The flags of a run with a human, so a test controls the terminal
    /// alone.
    fn with_a_human() -> (AtomicFlagGuard, AtomicFlagGuard) {
        (
            AtomicFlagGuard::set(&HEADLESS, false),
            AtomicFlagGuard::set(&ACP_SERVER, false),
        )
    }

    #[test]
    #[serial]
    fn reach_is_human_under_the_tui_whatever_the_tty() {
        let _human = with_a_human();
        let _script = prompt_script::install_non_interactive();
        let _tui_on = crate::tui::OwnerGuard::tui_for_test();
        let prompter = ScriptedPrompter::refusing();
        assert_eq!(prompter.reach(), Reach::Human);

        let _tui_off = crate::tui::OwnerGuard::shell_for_test();
        assert_eq!(prompter.reach(), Reach::NoTerminal);
    }

    /// Outside the UI the terminal follows stdout alone; a piped stdin
    /// does not close it, since inquire then reads its keys from
    /// `/dev/tty`.
    #[test]
    #[serial]
    fn reach_needs_only_a_stdout_tty_outside_the_tui() {
        let _human = with_a_human();
        let _tui_off = crate::tui::OwnerGuard::shell_for_test();
        let prompter = ScriptedPrompter::refusing();
        assert_eq!(prompter.reach() == Reach::Human, *IS_STDOUT_TERMINAL);

        let forced_on = prompt_script::install(&[]);
        assert_eq!(prompter.reach(), Reach::Human);
        drop(forced_on);

        let _forced_off = prompt_script::install_non_interactive();
        assert_eq!(prompter.reach(), Reach::NoTerminal);

        let _tui_on = crate::tui::OwnerGuard::tui_for_test();
        assert_eq!(prompter.reach(), Reach::Human);
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn reach_stays_human_with_a_stdout_tty_and_piped_stdin() {
        let _human = with_a_human();
        let _tui_off = crate::tui::OwnerGuard::shell_for_test();
        let _stdout_tty = prompt_script::install(&[]);
        let _stdin = crate::testing::pipe_stdin();

        assert_eq!(ScriptedPrompter::refusing().reach(), Reach::Human);
    }

    /// A missing human outranks a missing terminal; the headless
    /// prompter is headless even where a human is present.
    #[test]
    #[serial]
    fn reach_is_headless_without_a_human_whatever_the_terminal() {
        let _tui_off = crate::tui::OwnerGuard::shell_for_test();
        let scripted = ScriptedPrompter::refusing();
        for forced_tty in [true, false] {
            let _script = if forced_tty {
                prompt_script::install(&[])
            } else {
                prompt_script::install_non_interactive()
            };
            for (headless, acp) in [(true, false), (false, true)] {
                let _headless = AtomicFlagGuard::set(&HEADLESS, headless);
                let _acp = AtomicFlagGuard::set(&ACP_SERVER, acp);
                assert_eq!(scripted.reach(), Reach::Headless);
                assert_eq!(HeadlessPrompter.reach(), Reach::Headless);
            }
            let _human = with_a_human();
            assert_eq!(HeadlessPrompter.reach(), Reach::Headless);
        }
    }

    #[test]
    #[serial]
    fn require_human_lets_a_human_with_a_terminal_be_asked() {
        let _human = with_a_human();
        let _script = prompt_script::install(&[]);
        let prompter = ScriptedPrompter::refusing();
        assert_eq!(prompter.reach(), Reach::Human);
        assert!(can_ask(&prompter));
        require_human(&prompter, "pick a bundle", ".").unwrap();
    }

    /// The refusal has one shape; only the action and the remedy vary.
    #[test]
    #[serial]
    fn the_refusal_names_the_action_and_where_to_re_run() {
        let _human = with_a_human();
        let _tui_off = crate::tui::OwnerGuard::shell_for_test();
        let _script = prompt_script::install_non_interactive();
        let prompter = ScriptedPrompter::refusing();
        assert_eq!(prompter.reach(), Reach::NoTerminal);
        assert!(!can_ask(&prompter));

        let err =
            require_human(&prompter, "overwrite local file", " or pass --force.").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Refusing to overwrite local file non-interactively. Re-run in a terminal or pass --force."
        );

        let err = require_human(&prompter, "pick a bundle", ".").unwrap_err();
        assert_eq!(
            err.to_string(),
            "Refusing to pick a bundle non-interactively. Re-run in a terminal."
        );
    }

    /// With no human, a terminal still lets the question through so the
    /// headless prompter gives its typed refusal; without one the run is
    /// refused like any other.
    #[test]
    #[serial]
    fn require_human_without_a_human_refuses_only_without_a_terminal() {
        let _tui_off = crate::tui::OwnerGuard::shell_for_test();
        let _headless = AtomicFlagGuard::set(&HEADLESS, true);

        let forced_on = prompt_script::install(&[]);
        assert!(can_ask(&HeadlessPrompter));
        require_human(&HeadlessPrompter, "pick a bundle", ".").unwrap();
        drop(forced_on);

        let _forced_off = prompt_script::install_non_interactive();
        assert!(!can_ask(&HeadlessPrompter));
        let err = require_human(&HeadlessPrompter, "pick a bundle", ".").unwrap_err();
        assert!(!NeedsHuman::caused(&err), "{err}");
        assert_eq!(
            err.to_string(),
            "Refusing to pick a bundle non-interactively. Re-run in a terminal."
        );
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

    /// No `user__*` tool raises a `Password` prompt; the variant reaches
    /// the prompter from the wire protocol and is pinned here as well as
    /// by the TUI's masked modal.
    #[test]
    #[serial]
    fn ask_returns_a_password_but_publishes_it_redacted() {
        let sink = RecordingSink::install();
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Text("hunter2".to_string())]);
        let req = PromptRequest {
            default: Some(json!("dflt-pw-4j")),
            ..PromptRequest::new(PromptAction::Password, "secret? pw-4j", Origin::detached())
        };

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
        assert!(
            !format!("{requested:?}").contains("dflt-pw-4j"),
            "{requested:?}"
        );
    }

    /// A sensitive input is typed in the clear but published like a
    /// password, and its default is withheld from the request.
    #[test]
    #[serial]
    fn ask_returns_a_sensitive_answer_but_publishes_neither_it_nor_the_default() {
        let sink = RecordingSink::install();
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Text("tok-9v2q".to_string())]);
        let req = PromptRequest {
            default: Some(json!("dflt-4k8w")),
            sensitive: true,
            ..PromptRequest::new(PromptAction::Input, "token? sv-3n", Origin::detached())
        };

        let answer = ask(&prompter, req).unwrap();

        assert_eq!(answer, PromptAnswer::Text("tok-9v2q".to_string()));
        let [recorded] = prompter.requests().try_into().unwrap();
        assert_eq!(recorded.default, Some(json!("dflt-4k8w")));
        let events = sink.drain();
        let (requested, answered) = prompt_events(events.clone(), "token? sv-3n");
        let [Event::PromptRequested { id, default, .. }] = requested.as_slice() else {
            panic!("{requested:?}");
        };
        assert_eq!(*default, None);
        assert_eq!(
            answered,
            vec![Event::PromptAnswered {
                id: id.clone(),
                answer: "<redacted>".to_string(),
                auto: false,
            }]
        );
        let published = format!("{events:?}");
        assert!(!published.contains("tok-9v2q"), "{published}");
        assert!(!published.contains("dflt-4k8w"), "{published}");
    }

    /// Every site that closes a request publishes through
    /// `answered_event`, so a sensitive request is redacted wherever it
    /// is answered; only text carries the value.
    #[test]
    fn answered_event_redacts_a_sensitive_text_and_leaves_the_rest() {
        let plain = request(PromptAction::Input, None);
        let sensitive = PromptRequest {
            sensitive: true,
            ..plain.clone()
        };
        let answer_of = |req: &PromptRequest, answer: PromptAnswer| {
            let Event::PromptAnswered { answer, .. } = req.answered_event(&answer, false) else {
                unreachable!("answered_event builds a PromptAnswered");
            };
            answer
        };
        let text = PromptAnswer::Text("hunter2".to_string());
        assert_eq!(answer_of(&plain, text.clone()), "hunter2");
        assert_eq!(answer_of(&sensitive, text), "<redacted>");
        assert_eq!(
            answer_of(&sensitive, PromptAnswer::Cancelled),
            "<cancelled>"
        );
        let confirm = PromptRequest {
            sensitive: true,
            ..request(PromptAction::Confirm, Some("tool-x"))
        };
        assert_eq!(
            confirm.answered_event(&PromptAnswer::Confirmed(true), true),
            Event::PromptAnswered {
                id: confirm.id.clone(),
                answer: "yes".to_string(),
                auto: true,
            }
        );
    }

    #[test]
    fn memory_answers_only_a_remembered_tool_confirm() {
        let memory = PromptMemory::default();
        let req = request(PromptAction::Confirm, Some("fake_tool"));
        assert_eq!(memory.auto_answer(&req), None);

        memory.remember(&req);
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

    /// Two tool lines that display alike are told apart by what the tool
    /// sent; a request with no raw text is keyed on its question.
    #[test]
    fn memory_keys_a_tool_confirm_on_the_raw_question() {
        let memory = PromptMemory::default();
        let shown = PromptRequest {
            raw_question: Some("Run? \u{1b}[8mrm -rf ~".to_string()),
            ..request(PromptAction::Confirm, Some("fake_tool"))
        };
        memory.remember(&shown);
        assert_eq!(memory.auto_answer(&shown), Some(true));

        let same_display = PromptRequest {
            raw_question: Some("Run? \u{1b}[8mls".to_string()),
            ..shown.clone()
        };
        assert_eq!(memory.auto_answer(&same_display), None);
        assert_eq!(
            memory.auto_answer(&request(PromptAction::Confirm, Some("fake_tool"))),
            None
        );

        let own = request(PromptAction::Confirm, Some("other_tool"));
        memory.remember(&own);
        let raw_equal = PromptRequest {
            raw_question: Some(own.question.clone()),
            ..own.clone()
        };
        assert_eq!(memory.auto_answer(&raw_equal), Some(true));
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
                "guidance": "No human is available to answer. Go with the recommended option; if none is marked, use your best judgment.",
            });
            assert_eq!(reply, expected);
            assert_eq!(reply.to_string(), expected.to_string());
        }
    }

    #[test]
    fn headless_prompter_answers_a_confirm_with_needs_human() {
        let answer = HeadlessPrompter
            .prompt(request(PromptAction::Confirm, None))
            .unwrap();
        assert_eq!(answer, PromptAnswer::NeedsHuman);
    }

    #[test]
    fn headless_prompter_refuses_every_text_shaped_kind() {
        for (action, name) in [
            (PromptAction::Input, "input"),
            (PromptAction::Password, "password"),
            (PromptAction::Select, "select"),
            (PromptAction::Checkbox, "checkbox"),
        ] {
            let err = HeadlessPrompter.prompt(request(action, None)).unwrap_err();
            let refusal = err
                .downcast_ref::<NeedsHuman>()
                .unwrap_or_else(|| panic!("{name}: {err}"));
            assert!(
                matches!(refusal, NeedsHuman::Prompt(refused) if *refused == action),
                "{name}: {refusal:?}"
            );
            assert_eq!(
                err.to_string(),
                format!(
                    "The {name} prompt needs a human and none is attached to this run; \
                     run it without --headless or --acp-server"
                )
            );
        }
    }

    /// The creation gate's refusal shares its tail with the prompt
    /// refusals, so the two never drift apart.
    #[test]
    fn a_creation_refusal_names_the_thing_and_the_missing_human() {
        let err: anyhow::Error = NeedsHuman::for_creating("role", "coder").into();
        assert!(NeedsHuman::caused(&err));
        assert_eq!(
            err.to_string(),
            "Creating role 'coder' needs a human to answer the prompt and none is attached to \
             this run; run it without --headless or --acp-server"
        );
    }

    #[test]
    fn a_refusal_is_recognised_under_added_context() {
        use anyhow::Context;
        let err = HeadlessPrompter
            .prompt(request(PromptAction::Select, None))
            .context("while asking for a role name")
            .unwrap_err();
        assert!(NeedsHuman::caused(&err));
        assert!(!NeedsHuman::caused(&anyhow!("no tty here")));
    }

    #[test]
    fn a_cancel_is_recognised_under_added_context() {
        use anyhow::Context;
        let err = PromptAnswer::Cancelled
            .into_text()
            .context("while asking for a path")
            .unwrap_err();
        assert!(PromptCancelled::caused(&err));
        assert_eq!(
            err.root_cause().to_string(),
            "Operation was canceled by the user"
        );
        assert!(!PromptCancelled::caused(&anyhow!("no tty here")));
        assert!(!PromptCancelled::caused(
            &NeedsHuman::Prompt(PromptAction::Input).into()
        ));
    }

    #[test]
    fn summary_redacts_a_password_and_names_the_rest() {
        let text = PromptAnswer::Text("hunter2".to_string());
        assert_eq!(text.summary(PromptAction::Password), "<redacted>");
        assert_eq!(text.summary(PromptAction::Input), "hunter2");
        assert_eq!(
            PromptAnswer::NeedsHuman.summary(PromptAction::Confirm),
            "<needs_human>"
        );
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
        assert_eq!(
            PromptAnswer::Interrupted.summary(PromptAction::Confirm),
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
        let err = PromptAnswer::Selected("t".to_string())
            .into_confirmed()
            .unwrap_err();
        assert_eq!(err.to_string(), "Unexpected answer to the confirm prompt");
        let err = PromptAnswer::NeedsHuman.into_confirmed().unwrap_err();
        assert!(
            matches!(
                err.downcast_ref::<NeedsHuman>(),
                Some(NeedsHuman::Prompt(PromptAction::Confirm))
            ),
            "{err}"
        );
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
            PromptAnswer::Interrupted.into_confirmed().unwrap_err(),
            PromptAnswer::Interrupted.into_text().unwrap_err(),
        ] {
            assert_eq!(err.to_string(), "Operation was canceled by the user");
            assert!(PromptCancelled::caused(&err));
        }
        assert!(!PromptCancelled::interrupted(
            &PromptAnswer::Cancelled.into_text().unwrap_err()
        ));
        assert!(PromptCancelled::interrupted(
            &PromptAnswer::Interrupted.into_text().unwrap_err()
        ));
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

    #[test]
    #[serial]
    fn select_closed_hands_back_an_offered_option() {
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Selected("b".to_string())]);
        let options = vec!["a".to_string(), "b".to_string()];
        assert_eq!(
            select_closed(&prompter, "pick one pm-sc1", options.clone()).unwrap(),
            "b"
        );
        let [req] = prompter.requests().try_into().unwrap();
        assert!(req.closed);
        assert_eq!(req.options, options);
    }

    #[test]
    #[serial]
    fn select_closed_rejects_an_answer_outside_the_options() {
        let prompter =
            ScriptedPrompter::new(vec![PromptAnswer::Selected("not-offered".to_string())]);
        let err = select_closed(
            &prompter,
            "pick one pm-sc2",
            vec!["a".to_string(), "b".to_string()],
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "'not-offered' is not one of the offered options"
        );
    }

    #[test]
    #[serial]
    fn select_closed_without_a_human_is_a_needs_human_refusal() {
        let err = select_closed(
            &HeadlessPrompter,
            "pick one pm-sc3",
            vec!["a".to_string(), "b".to_string()],
        )
        .unwrap_err();
        assert!(NeedsHuman::caused(&err), "{err}");
    }

    #[test]
    fn select_closed_with_no_options_is_refused_before_asking() {
        let prompter = ScriptedPrompter::refusing();
        let err = select_closed(&prompter, "pick one pm-sc4", Vec::new()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "'pick one pm-sc4' has no options to choose from"
        );
        assert!(prompter.requests().is_empty());
    }

    #[test]
    #[serial]
    fn a_cancelled_closed_select_is_an_error_unless_the_caller_accepts_the_cancel() {
        let options = vec!["a".to_string(), "b".to_string()];
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Cancelled]);
        let err = select_closed(&prompter, "pick one pm-sc5", options.clone()).unwrap_err();
        assert_eq!(err.to_string(), "Operation was canceled by the user");

        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Cancelled]);
        assert_eq!(
            select_closed_or_cancel(&prompter, "pick one pm-sc6", options.clone()).unwrap(),
            None
        );
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Selected("a".to_string())]);
        assert_eq!(
            select_closed_or_cancel(&prompter, "pick one pm-sc7", options).unwrap(),
            Some("a".to_string())
        );
    }

    /// Esc is the answer a cancel-accepting menu carries on with; Ctrl-C
    /// stays an error, so the command asking stops.
    #[test]
    #[serial]
    fn an_interrupted_menu_is_an_error_even_where_a_cancel_is_an_answer() {
        let options = vec!["a".to_string(), "b".to_string()];
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Interrupted]);
        let err = select_closed_or_cancel(&prompter, "pick one pm-si1", options).unwrap_err();
        assert!(PromptCancelled::interrupted(&err), "{err:#}");

        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Cancelled]);
        assert_eq!(
            confirm_or_cancel(&prompter, "go? pm-si2", false).unwrap(),
            None
        );
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Interrupted]);
        let err = confirm_or_cancel(&prompter, "go? pm-si3", false).unwrap_err();
        assert!(PromptCancelled::interrupted(&err), "{err:#}");
    }

    #[test]
    #[serial]
    fn confirm_with_help_carries_the_printed_context_as_help() {
        let prompter = ScriptedPrompter::new(vec![PromptAnswer::Confirmed(true)]);
        assert!(confirm_with_help(&prompter, "Continue? pm-ch1", false, "WARNING: body").unwrap());
        let [req] = prompter.requests().try_into().unwrap();
        assert_eq!(req.action, PromptAction::Confirm);
        assert_eq!(req.question, "Continue? pm-ch1");
        assert_eq!(req.default, Some(json!(false)));
        assert_eq!(req.help.as_deref(), Some("WARNING: body"));
    }
}
