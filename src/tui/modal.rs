use super::cells::LineCache;
use super::composer::{RULE_ROWS, wrapped_rows};
use super::context::context_title;
use super::keys::ModalKind;
use super::render::Renderer;
use super::sink::unattended_answer;
use super::theme::{ComposerState, Theme};
use super::{
    App, DIM, FIXED_ROWS, Overlay, TRANSCRIPT_MIN_ROWS, escalation_kind, new_field, render, roster,
};
use crate::events::{NoticeKind, Origin};
use crate::prompt::{PromptAction, PromptAnswer, PromptRequest, input_default_text};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
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
    /// The question rendered as markdown, laid out by the frame before
    /// it is measured or drawn.
    question_markdown: LineCache,
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
            question_markdown: LineCache::default(),
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

    /// The question's text with its escape sequences and blank lines
    /// gone: a tool controls it, and none may move the cursor, hide a
    /// word or push the rest of the question past the pane.
    fn question_source(&self) -> String {
        render::plain_text(&self.req.question)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Renders the question as markdown at `width` for the frames that
    /// measure and draw it, every row cut to the width. A tool's question
    /// stays plain text: markdown would conceal its syntax characters and
    /// re-wrap it, and every character of what is approved has to show.
    pub(super) fn layout_question(&mut self, width: u16, renderer: &mut Renderer) {
        if self.req.tool.is_some() {
            return;
        }
        let source = self.question_source();
        self.question_markdown.get(width, || {
            render::hard_wrap(renderer.markdown(&source, width), width)
                .into_iter()
                .filter(|line| !line.to_string().trim().is_empty())
                .collect()
        });
    }

    /// Drops the question's layout so a new renderer lays it out again.
    pub(super) fn forget_question_layout(&mut self) {
        self.question_markdown = LineCache::default();
    }

    /// The question at `width` and the help line dimmed under it, apart,
    /// so the question alone takes the question colour. Until the frame
    /// has laid the markdown out the question reads as plain text.
    fn question_parts(&self, width: u16) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
        let question = match self.question_markdown.peek(width) {
            Some(lines) => lines.to_vec(),
            None => render::plain_lines(&self.question_source(), width, Style::new()),
        };
        // The help is one paragraph however its lines were broken.
        let help = self.req.help.as_deref().map(|help| {
            render::plain_text(help)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        });
        let help = help
            .filter(|help| !help.is_empty())
            .map(|help| render::plain_lines(&help, width, DIM))
            .unwrap_or_default();
        (question, help)
    }

    /// The question wrapped to `width`, every line of it, with the help
    /// line under it: what the user approves is read from here, so none
    /// of it may be clipped away.
    pub(super) fn question_lines(&self, width: u16) -> Vec<Line<'static>> {
        let (mut lines, help) = self.question_parts(width);
        lines.extend(help);
        lines
    }

    /// Rows the question may take on a frame `height` tall: the frame's
    /// fixed rows, the box's rules and the origin, middle and hint rows
    /// are taken, and `TRANSCRIPT_MIN_ROWS` stay with the transcript.
    fn question_cap(&self, width: u16, height: u16) -> u16 {
        let fixed = FIXED_ROWS
            + self.rule_rows(width, height)
            + TRANSCRIPT_MIN_ROWS
            + self.hint_rows(width)
            + self.origin_rows()
            + self.middle_rows(width, height);
        height.saturating_sub(fixed).max(1)
    }

    /// Rows of the box's two rules: none on a frame where keeping them
    /// would cut the question, which needs the rows more.
    fn rule_rows(&self, width: u16, height: u16) -> u16 {
        let taken = FIXED_ROWS
            + RULE_ROWS
            + TRANSCRIPT_MIN_ROWS
            + self.origin_rows()
            + self.middle_rows(width, height)
            + self.hint_rows(width);
        if self.question_fits_rows(width, height.saturating_sub(taken)) {
            RULE_ROWS
        } else {
            0
        }
    }

    /// Rows of the field or the list as they wrap, none for a confirm; a
    /// list or field taller than the frame allows (one question row, the
    /// fixed rows, the rules and the transcript's minimum kept) scrolls
    /// inside what it gets.
    fn middle_rows(&self, width: u16, height: u16) -> u16 {
        if matches!(self.kind(), ModalKind::Confirm { .. }) {
            return 0;
        }
        let fixed = 1
            + FIXED_ROWS
            + RULE_ROWS
            + TRANSCRIPT_MIN_ROWS
            + self.hint_rows(width)
            + self.origin_rows()
            // A field or a wrapped list grows, but never into the rows the
            // question needs: a cut question cannot be answered, and the
            // field and the list both scroll inside what they get.
            + row_count(self.question_lines(width).len()).saturating_sub(1);
        let wanted = match &self.field {
            Some(field) => wrapped_rows(field.lines(), width, field.mask_char()),
            None => self.option_rows(width).iter().map(Vec::len).sum(),
        };
        row_count(wanted).clamp(1, height.saturating_sub(fixed).max(1))
    }

    fn question_rows(&self, width: u16, height: u16) -> u16 {
        row_count(self.question_lines(width).len()).clamp(1, self.question_cap(width, height))
    }

    /// Whether the cap a frame `height` tall allows leaves the whole
    /// question on screen.
    pub(super) fn question_fits_cap(&self, width: u16, height: u16) -> bool {
        self.question_fits_rows(width, self.question_cap(width, height))
    }

    pub(super) fn question_fits_rows(&self, width: u16, rows: u16) -> bool {
        row_count(self.question_lines(width).len()) <= rows
    }

    /// Rows of the whole box: the rules around the origin, question and
    /// field or list, then the hint under it.
    pub(super) fn rows(&self, width: u16, height: u16) -> u16 {
        self.rule_rows(width, height)
            + self.origin_rows()
            + self.question_rows(width, height)
            + self.middle_rows(width, height)
            + self.hint_rows(width)
    }

    /// The top rule, origin, question, middle, bottom rule and legend
    /// rows inside the box's `area` on a frame `frame` tall. A frame too
    /// short for the layout squeezes the question first; what it
    /// actually received is what answerability is judged on.
    pub(super) fn areas(&self, area: Rect, frame: Rect) -> [Rect; 6] {
        let rule = self.rule_rows(area.width, frame.height) / RULE_ROWS;
        Layout::vertical([
            Constraint::Length(rule),
            Constraint::Length(self.origin_rows()),
            Constraint::Min(1),
            Constraint::Length(self.middle_rows(area.width, frame.height)),
            Constraint::Length(rule),
            Constraint::Length(self.hint_rows(area.width)),
        ])
        .areas(area)
    }

    /// Each row of the list as it wraps to `width`: the marker and, on a
    /// checkbox, the tick on the first line, the rest of a long option
    /// indented under its text.
    fn option_rows(&self, width: u16) -> Vec<Vec<String>> {
        let checkbox = self.kind() == ModalKind::Checkbox;
        self.req
            .options
            .iter()
            .map(|option| render::plain_text(option).replace('\n', " "))
            .chain(self.has_custom_row().then(|| CUSTOM_CHOICE_ROW.to_string()))
            .enumerate()
            .map(|(index, text)| {
                let marker = if index == self.cursor { ">" } else { " " };
                let lead = if checkbox {
                    let tick = if self.checked[index] { "x" } else { " " };
                    format!("{marker} [{tick}] ")
                } else {
                    format!("{marker} ")
                };
                let indent = " ".repeat(lead.len());
                let room = usize::from(width).saturating_sub(lead.len()).max(1);
                let mut lines: Vec<String> = textwrap::wrap(&text, room)
                    .iter()
                    .enumerate()
                    .map(|(i, part)| format!("{}{part}", if i == 0 { &lead } else { &indent }))
                    .collect();
                if lines.is_empty() {
                    lines.push(lead);
                }
                lines
            })
            .collect()
    }

    /// The list's rows as drawn at `width`, windowed so the option under
    /// the cursor is whole when the list is taller than `visible`; that
    /// option is in the highlight style, the others plain.
    fn list_lines(&self, width: u16, visible: usize, theme: &Theme) -> Vec<Line<'static>> {
        let options = self.option_rows(width);
        let through_cursor: usize = options.iter().take(self.cursor + 1).map(Vec::len).sum();
        let mut skip = through_cursor.saturating_sub(visible);
        let mut first = 0;
        while skip > 0 && first < self.cursor {
            skip = skip.saturating_sub(options[first].len());
            first += 1;
        }
        options
            .into_iter()
            .enumerate()
            .skip(first)
            .flat_map(|(index, lines)| {
                let style = if index == self.cursor {
                    theme.option_highlight
                } else {
                    Style::new()
                };
                lines
                    .into_iter()
                    .map(move |line| Line::from(Span::styled(line, style)))
            })
            .take(visible)
            .collect()
    }
}

fn row_count(lines: usize) -> u16 {
    u16::try_from(lines).unwrap_or(u16::MAX)
}

/// A prompt drawn in the composer's box, between its rules: origin,
/// question, the field or list, then the hint under the box. A question
/// taller than its rows is cut with a marker counting the hidden lines
/// and a legend offering only Esc; the cap grows with the terminal, so
/// enlarging it shows them.
pub(super) struct PromptModal<'a> {
    pub(super) prompt: &'a PendingPrompt,
    pub(super) theme: &'a Theme,
    /// The context label the composer's top rule would carry.
    pub(super) context: Option<&'a str>,
}

impl Widget for PromptModal<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let prompt = self.prompt;
        let [top, origin, question, middle, bottom, hint] = prompt.areas(area, *buf.area());
        // A prompt on screen is always waiting on the user.
        let (rule, label) = self.theme.composer_rule(ComposerState::YourTurn);
        for (row, label) in [(top, label), (bottom, None)] {
            let mut block = Block::new().borders(Borders::TOP).border_style(rule);
            if let Some(label) = label {
                if let Some(title) = self
                    .context
                    .and_then(|context| context_title(context, row.width, &[label]))
                {
                    block = block.title(Line::styled(title, self.theme.your_turn));
                }
                block = block.title(Line::styled(label, rule).right_aligned());
            }
            block.render(row, buf);
        }
        let (mut lines, help) = prompt.question_parts(area.width);
        for line in &mut lines {
            for span in &mut line.spans {
                span.style = self.theme.question.patch(span.style);
            }
        }
        lines.extend(help);
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
        if let Some(line) = prompt.origin_line() {
            Paragraph::new(Line::from(Span::styled(line, DIM))).render(origin, buf);
        }
        match (&prompt.field, prompt.kind()) {
            (Some(field), _) => field.render(middle, buf),
            (None, ModalKind::Confirm { .. }) => {}
            (None, _) => {
                Paragraph::new(prompt.list_lines(
                    middle.width,
                    usize::from(middle.height),
                    self.theme,
                ))
                .render(middle, buf);
            }
        }
        Paragraph::new(prompt.hint_lines(area.width, clipped)).render(hint, buf);
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
        self.sync_awaiting_rows();
    }

    /// Holds open each tool row with a confirm of its own call queued,
    /// whether or not that confirm is the one up.
    fn sync_awaiting_rows(&mut self) {
        let prompts = &self.prompts;
        self.transcript.set_awaiting(&|call_id| {
            prompts.iter().any(|prompt| {
                prompt.req.action == PromptAction::Confirm && prompt.asked_by_call(call_id)
            })
        });
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
            node: None,
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
        // A question is never left waiting behind the pager, the help or
        // a pop-up, nor behind what closing one of them gives back.
        // A pager under a pop-up counts too, as clearing the stack drops it.
        if self
            .overlay
            .iter()
            .chain(&self.overlay_stack)
            .any(|overlay| matches!(overlay, Overlay::Pager(_)))
        {
            self.release_pager_lines();
        }
        self.overlay = None;
        self.overlay_stack.clear();
        let becomes_front = self.prompts.is_empty();
        self.prompts.push_back(PendingPrompt::new(req, reply));
        if becomes_front {
            self.arm_front_gates();
        }
        self.sync_awaiting_rows();
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
        self.sync_awaiting_rows();
    }

    /// Gives every queued prompt its unattended answer; used when the UI
    /// is about to go away without a human to ask.
    pub(super) fn abandon_prompts(&mut self) {
        while let Some(prompt) = self.prompts.pop_front() {
            let _ = prompt.reply.send(unattended_answer(&prompt.req));
        }
        self.sync_awaiting_rows();
    }
}
