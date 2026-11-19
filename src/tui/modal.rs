use super::keys::ModalKind;
use super::sink::unattended_answer;
use super::{App, DIM, TRANSCRIPT_MIN_ROWS, escalation_kind, new_field, render, roster};
use crate::events::{NoticeKind, Origin};
use crate::prompt::{PromptAction, PromptAnswer, PromptRequest, input_default_text};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use ratatui_textarea::TextArea;
use serde_json::Value;
use std::sync::mpsc;

const INPUT_HINT: &str = "Enter = answer  Esc = cancel";
const SELECT_HINT: &str = "Up/Down = move  Enter = choose  Esc = cancel";
const CHECKBOX_HINT: &str = "Space = tick  Up/Down = move  Enter = answer  Esc = cancel";
/// The last row of a select, the entry the REPL's prompter appends too;
/// choosing it opens a free-text field in the list's place.
pub(super) const CUSTOM_CHOICE_ROW: &str = crate::prompt::CUSTOM_MULTI_CHOICE_ANSWER_OPTION;
/// The confirm legend while part of the question is off screen: nothing
/// clipped may be approved, so the approving keys are not offered. Esc
/// leads so a narrow wrap never splits it from its meaning.
pub(super) const CLIPPED_HINT: &str = "Esc = cancel  enlarge the terminal to answer";

/// The key legend under a yes/no question, naming what a bare Enter answers.
pub(super) fn confirm_hint(default: bool, tool: bool) -> String {
    let enter = if default { "yes" } else { "no" };
    if tool {
        format!("y = yes  n = no  a = yes for this session  Enter = {enter}  Esc = cancel")
    } else {
        format!("y = yes  n = no  Enter = {enter}  Esc = cancel")
    }
}

pub(super) struct PendingPrompt {
    pub(super) req: PromptRequest,
    reply: mpsc::Sender<PromptAnswer>,
    /// The text field of an input or password, or of a select whose
    /// custom row was chosen.
    pub(super) field: Option<TextArea<'static>>,
    /// The row under the marker of a select or checkbox.
    pub(super) cursor: usize,
    /// One flag per option of a checkbox, in the options' order.
    checked: Vec<bool>,
}

impl PendingPrompt {
    pub(super) fn new(req: PromptRequest, reply: mpsc::Sender<PromptAnswer>) -> Self {
        let field = match req.action {
            PromptAction::Input => {
                let mut field = new_field(false);
                // An input's default is shown as the placeholder, the way
                // inquire's widget does: Enter alone accepts it and typing
                // replaces it rather than appending to it.
                if let Some(default) = input_default_text(&req) {
                    field.set_placeholder_text(render::plain_text(&default).replace('\n', " "));
                    field.set_placeholder_style(DIM);
                }
                Some(field)
            }
            PromptAction::Password => Some(new_field(true)),
            _ => None,
        };
        let checked = vec![false; req.options.len()];
        Self {
            req,
            reply,
            field,
            cursor: 0,
            checked,
        }
    }

    pub(super) fn kind(&self) -> ModalKind {
        match self.req.action {
            PromptAction::Confirm => {
                let tool = self.req.tool.is_some();
                // A tool's confirm only starts on yes when the tool asked
                // for it; Enter alone must not approve a tool call.
                let default = self
                    .req
                    .default
                    .as_ref()
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(!tool);
                ModalKind::Confirm { default, tool }
            }
            PromptAction::Input => ModalKind::Input,
            PromptAction::Password => ModalKind::Password,
            PromptAction::Select => ModalKind::Select {
                custom_field_open: self.field.is_some(),
            },
            PromptAction::Checkbox => ModalKind::Checkbox,
        }
    }

    /// Whether the answer is a non-empty id of the tool call or job that
    /// asked, so a report that the call ended can take the question down.
    pub(super) fn asked_by_call(&self, call_id: &str) -> bool {
        !call_id.is_empty() && self.req.call_id.as_deref() == Some(call_id)
    }

    pub(super) fn asked_by_job(&self, job_id: &str) -> bool {
        !job_id.is_empty() && self.req.job_id.as_deref() == Some(job_id)
    }

    pub(super) fn asked_by_agent(&self, agent_id: &str) -> bool {
        !agent_id.is_empty() && self.req.origin.agent_id.as_deref() == Some(agent_id)
    }

    /// Whether the select ends in the custom row: every select that is
    /// not closed.
    fn has_custom_row(&self) -> bool {
        self.req.action == PromptAction::Select && !self.req.closed
    }

    /// The rows of a list modal: the options, plus the custom row of an
    /// open select.
    pub(super) fn list_len(&self) -> usize {
        match self.kind() {
            ModalKind::Select {
                custom_field_open: false,
            } => self.req.options.len() + usize::from(self.has_custom_row()),
            ModalKind::Checkbox => self.req.options.len(),
            _ => 0,
        }
    }

    pub(super) fn move_cursor(&mut self, delta: i32) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        let last = len - 1;
        self.cursor = if delta < 0 {
            self.cursor.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.cursor.saturating_add(delta as usize).min(last)
        };
    }

    pub(super) fn toggle_cursor(&mut self) {
        if let Some(flag) = self.checked.get_mut(self.cursor) {
            *flag = !*flag;
        }
    }

    /// What Enter on a list answers: the select's row (or `None` for the
    /// custom row, which opens the field instead) or the ticked options.
    pub(super) fn choice(&self) -> Option<PromptAnswer> {
        match self.kind() {
            ModalKind::Select {
                custom_field_open: false,
            } => self
                .req
                .options
                .get(self.cursor)
                .cloned()
                .map(PromptAnswer::Selected),
            ModalKind::Checkbox => Some(PromptAnswer::Checked(
                self.req
                    .options
                    .iter()
                    .zip(&self.checked)
                    .filter(|(_, ticked)| **ticked)
                    .map(|(option, _)| option.clone())
                    .collect(),
            )),
            _ => None,
        }
    }

    pub(super) fn open_custom_field(&mut self) {
        self.field = Some(new_field(false));
    }

    /// What the field holds as an answer, by the kind that opened it: a
    /// select's custom text is a selection, the others are text; an
    /// input left empty answers with its default.
    pub(super) fn field_answer(&self) -> PromptAnswer {
        let text = self
            .field
            .as_ref()
            .map(|field| field.lines().join(" "))
            .unwrap_or_default();
        match self.req.action {
            PromptAction::Select => PromptAnswer::Selected(text),
            PromptAction::Input if text.is_empty() => {
                PromptAnswer::Text(input_default_text(&self.req).unwrap_or_default())
            }
            _ => PromptAnswer::Text(text),
        }
    }

    /// Who is asking: the tool, else the agent, else nobody in particular.
    fn origin_line(&self) -> Option<String> {
        if let Some(tool) = &self.req.tool {
            return Some(format!("{} asks:", render::plain_text(tool)));
        }
        self.req
            .origin
            .agent_id
            .as_ref()
            .map(|agent| format!("agent: {agent}"))
    }

    /// Rows the origin takes above the question.
    pub(super) fn origin_rows(&self) -> u16 {
        u16::from(self.origin_line().is_some())
    }

    /// The legend under the modal; a question that is clipped offers
    /// only Esc.
    fn hint(&self, clipped: bool) -> String {
        if clipped {
            return CLIPPED_HINT.to_string();
        }
        match self.kind() {
            ModalKind::Confirm { default, tool } => confirm_hint(default, tool),
            ModalKind::Input
            | ModalKind::Password
            | ModalKind::Select {
                custom_field_open: true,
            } => INPUT_HINT.to_string(),
            ModalKind::Select { .. } => SELECT_HINT.to_string(),
            ModalKind::Checkbox => CHECKBOX_HINT.to_string(),
        }
    }

    fn hint_lines(&self, width: u16, clipped: bool) -> Vec<Line<'static>> {
        render::plain_lines(&self.hint(clipped), width, DIM)
    }

    /// Rows reserved for the legend: enough for either wording, so the
    /// swap to the clipped one never changes the layout.
    fn hint_rows(&self, width: u16) -> u16 {
        row_count(self.hint_lines(width, false).len())
            .max(row_count(self.hint_lines(width, true).len()))
    }

    /// The question wrapped to `width`, every line of it, with the help
    /// line dimmed under it: what the user approves is read from here,
    /// so none of it may be clipped away. Blank lines are dropped from
    /// both so a run of newlines cannot push the rest of the question
    /// past the pane or paint an empty help row.
    pub(super) fn question_lines(&self, width: u16) -> Vec<Line<'static>> {
        // A tool controls the question's text; escape sequences in it
        // are stripped so none can move the cursor or hide a word.
        let question = render::plain_text(&self.req.question)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let mut lines =
            render::plain_lines(&question, width, Style::new().add_modifier(Modifier::BOLD));
        // The help is one paragraph however its lines were broken.
        let help = self.req.help.as_deref().map(|help| {
            render::plain_text(help)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        });
        if let Some(help) = help.filter(|help| !help.is_empty()) {
            lines.extend(render::plain_lines(&help, width, DIM));
        }
        lines
    }

    /// Rows the question may take on a frame `height` tall: the status
    /// row, the footer, the modal's origin, middle and hint rows are
    /// fixed, and `TRANSCRIPT_MIN_ROWS` stay with the transcript.
    fn question_cap(&self, width: u16, height: u16) -> u16 {
        let fixed = 2
            + TRANSCRIPT_MIN_ROWS
            + self.hint_rows(width)
            + self.origin_rows()
            + self.middle_rows(width, height);
        height.saturating_sub(fixed).max(1)
    }

    /// Rows of the field or the list, none for a confirm; a list longer
    /// than the frame allows (one question row, the fixed rows and the
    /// transcript's minimum kept) scrolls inside what it gets.
    fn middle_rows(&self, width: u16, height: u16) -> u16 {
        if matches!(self.kind(), ModalKind::Confirm { .. }) {
            return 0;
        }
        let fixed = 3 + TRANSCRIPT_MIN_ROWS + self.hint_rows(width) + self.origin_rows();
        row_count(self.list_len()).clamp(1, height.saturating_sub(fixed).max(1))
    }

    fn question_rows(&self, width: u16, height: u16) -> u16 {
        row_count(self.question_lines(width).len()).clamp(1, self.question_cap(width, height))
    }

    /// Whether the cap a frame `height` tall allows leaves the whole
    /// question on screen.
    pub(super) fn question_fits_cap(&self, width: u16, height: u16) -> bool {
        self.question_fits_rows(width, self.question_cap(width, height))
    }

    /// Whether `rows` of the pane hold the whole question.
    pub(super) fn question_fits_rows(&self, width: u16, rows: u16) -> bool {
        row_count(self.question_lines(width).len()) <= rows
    }

    /// Rows of the whole modal: origin, question, field or list, hint.
    pub(super) fn rows(&self, width: u16, height: u16) -> u16 {
        self.origin_rows()
            + self.question_rows(width, height)
            + self.middle_rows(width, height)
            + self.hint_rows(width)
    }

    /// The origin, question, middle and legend rows inside the modal's
    /// `area` on a frame `frame` tall. A frame too short for the layout
    /// squeezes the question first; what it actually received is what
    /// answerability is judged on.
    pub(super) fn areas(&self, area: Rect, frame: Rect) -> [Rect; 4] {
        Layout::vertical([
            Constraint::Length(self.origin_rows()),
            Constraint::Min(1),
            Constraint::Length(self.middle_rows(area.width, frame.height)),
            Constraint::Length(self.hint_rows(area.width)),
        ])
        .areas(area)
    }

    /// The list's rows as drawn, windowed around the cursor when it is
    /// taller than `visible`.
    fn list_lines(&self, visible: usize) -> Vec<Line<'static>> {
        let checkbox = self.kind() == ModalKind::Checkbox;
        let rows: Vec<String> = self
            .req
            .options
            .iter()
            .map(|option| render::plain_text(option).replace('\n', " "))
            .chain(self.has_custom_row().then(|| CUSTOM_CHOICE_ROW.to_string()))
            .collect();
        let first = if self.cursor < visible {
            0
        } else {
            self.cursor + 1 - visible
        };
        rows.into_iter()
            .enumerate()
            .skip(first)
            .take(visible)
            .map(|(index, text)| {
                let marker = if index == self.cursor { ">" } else { " " };
                let line = if checkbox {
                    let tick = if self.checked[index] { "x" } else { " " };
                    format!("{marker} [{tick}] {text}")
                } else {
                    format!("{marker} {text}")
                };
                let style = if index == self.cursor {
                    Style::new().add_modifier(Modifier::BOLD)
                } else {
                    Style::new()
                };
                Line::from(Span::styled(line, style))
            })
            .collect()
    }
}

fn row_count(lines: usize) -> u16 {
    u16::try_from(lines).unwrap_or(u16::MAX)
}

/// A prompt drawn where the composer was: origin, question, the field or
/// list, hint. A question taller than its rows is cut with a marker
/// counting the hidden lines and a legend offering only Esc; the cap
/// grows with the terminal, so enlarging it shows them.
pub(super) struct PromptModal<'a>(pub(super) &'a PendingPrompt);

impl Widget for PromptModal<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let [origin, question, middle, hint] = self.0.areas(area, *buf.area());
        let mut lines = self.0.question_lines(area.width);
        let visible = usize::from(question.height);
        let clipped = lines.len() > visible;
        if clipped && visible > 0 {
            let kept = visible - 1;
            let hidden = lines.len() - kept;
            lines.truncate(kept);
            lines.push(Line::from(Span::styled(
                format!("(+{hidden} more lines; enlarge the terminal)"),
                DIM,
            )));
        }
        Paragraph::new(lines).render(question, buf);
        if let Some(line) = self.0.origin_line() {
            Paragraph::new(Line::from(Span::styled(line, DIM))).render(origin, buf);
        }
        match (&self.0.field, self.0.kind()) {
            (Some(field), _) => field.render(middle, buf),
            (None, ModalKind::Confirm { .. }) => {}
            (None, _) => {
                Paragraph::new(self.0.list_lines(usize::from(middle.height))).render(middle, buf);
            }
        }
        Paragraph::new(self.0.hint_lines(area.width, clipped)).render(hint, buf);
    }
}

impl App {
    pub(super) fn answer_front(&mut self, answer: PromptAnswer) {
        if let Some(prompt) = self.prompts.pop_front() {
            // The asker gave up waiting (its turn was interrupted); the
            // answer has nowhere to go.
            let _ = prompt.reply.send(answer);
            self.arm_front_gates();
        }
    }

    /// Starts the modal gates over for whatever prompt is now at the
    /// front: a held or repeated key must not answer it on the previous
    /// prompt's momentum, and a user idle for a second who taps a key as
    /// the modal comes up has not seen it yet.
    pub(super) fn arm_front_gates(&mut self) {
        let now = self.clock.now();
        self.last_keystroke = Some(now);
        self.front_since = (!self.prompts.is_empty()).then_some(now);
        self.question_area = None;
        self.origin_area = None;
    }

    /// Puts an agent's question to the user in the prompt modal of the
    /// kind the agent asked for, named by the `[kind] ` its question
    /// starts with; a list or checklist with no options, or a question
    /// with no kind, is a list when it has options and a text field
    /// otherwise. The answer is relayed once the modal gives it, as the
    /// published text of that answer; a cancelled modal leaves the
    /// question waiting.
    pub(super) fn ask_escalation(&mut self, ask: roster::Escalation) {
        let (kind, question) = escalation_kind(&ask.question);
        let action = match kind {
            Some(PromptAction::Select | PromptAction::Checkbox) | None
                if ask.options.is_empty() =>
            {
                PromptAction::Input
            }
            Some(action) => action,
            None => PromptAction::Select,
        };
        let origin = Origin {
            agent_id: Some(ask.agent_id),
            // The queue does not say how deep the asking agent is; a
            // direct child is the fallback.
            depth: 1,
        };
        let mut req = PromptRequest::new(action, question.to_string(), origin);
        req.options = ask.options;
        let (reply, answer) = mpsc::channel();
        self.escalation_answers.push((ask.id, action, answer));
        self.enqueue_prompt(req, reply);
    }

    pub(super) fn settle_escalation_answers(&mut self) {
        let mut answered = Vec::new();
        self.escalation_answers
            .retain(|(id, action, answer)| match answer.try_recv() {
                Ok(answer) => {
                    answered.push((id.clone(), *action, answer));
                    false
                }
                Err(mpsc::TryRecvError::Empty) => true,
                Err(mpsc::TryRecvError::Disconnected) => false,
            });
        for (id, action, answer) in answered {
            if !matches!(answer, PromptAnswer::Cancelled | PromptAnswer::Interrupted) {
                self.reply_escalation(&id, &answer.summary(action));
            }
        }
    }

    fn reply_escalation(&mut self, id: &str, reply: &str) {
        let Some(control) = &self.control else {
            self.transcript.push_notice(
                self.notice_merge_from(),
                NoticeKind::ToolWarning,
                "The reply was not delivered: the session is ending".to_string(),
            );
            return;
        };
        let mut ctx = control.fork_for_branch();
        match crate::function::agents::reply_escalation(&mut ctx, id, reply) {
            Ok(result) if result.get("status").and_then(Value::as_str) == Some("ok") => {}
            Ok(result) => self.transcript.push_notice(
                self.notice_merge_from(),
                NoticeKind::ToolWarning,
                result
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("The reply was not delivered")
                    .to_string(),
            ),
            Err(err) => self.transcript.push_notice(
                self.notice_merge_from(),
                NoticeKind::ToolWarning,
                format!("The reply was not delivered: {err:#}"),
            ),
        }
    }

    pub(super) fn enqueue_prompt(&mut self, req: PromptRequest, reply: mpsc::Sender<PromptAnswer>) {
        self.close_view();
        // A question is never left waiting behind the pager or the help.
        self.close_pager();
        self.help = None;
        let becomes_front = self.prompts.is_empty();
        self.prompts.push_back(PendingPrompt::new(req, reply));
        if becomes_front {
            self.arm_front_gates();
        }
    }

    /// Cancels every question, queued or up, whose asker has ended: the
    /// tool was killed or the job finished under it, so nobody is left
    /// to take the answer. The front's successor gets a fresh gate.
    pub(super) fn dismiss_prompts(&mut self, asked_by: impl Fn(&PendingPrompt) -> bool) {
        let front_dismissed = self.prompts.front().is_some_and(&asked_by);
        self.prompts.retain(|prompt| {
            if asked_by(prompt) {
                let _ = prompt.reply.send(PromptAnswer::Cancelled);
                false
            } else {
                true
            }
        });
        if front_dismissed {
            self.arm_front_gates();
        }
    }

    /// Gives every queued prompt its unattended answer; used when the UI
    /// is about to go away without a human to ask.
    pub(super) fn abandon_prompts(&mut self) {
        while let Some(prompt) = self.prompts.pop_front() {
            let _ = prompt.reply.send(unattended_answer(&prompt.req));
        }
    }
}
