//! The transcript's entries, kept as source so any width can render them,
//! and the rules for how a tool, job or agent row collapses to one line.

use super::render::{self, RenderCache, Renderer};
use super::{ChromeKind, DIM, LATE_OUTPUT_LIMIT, TOOL_OUTPUT_TAIL, chrome_style};
use crate::config::CommandPrefix;
use crate::events::NoticeKind;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Plain lines cached per width; the two most recently used widths are kept.
#[derive(Default)]
pub(super) struct LineCache {
    entries: Vec<(u16, Vec<Line<'static>>)>,
}

impl LineCache {
    pub(super) fn get(
        &mut self,
        width: u16,
        render: impl FnOnce() -> Vec<Line<'static>>,
    ) -> Vec<Line<'static>> {
        let entry = match self.entries.iter().position(|(w, _)| *w == width) {
            Some(index) => self.entries.remove(index),
            None => {
                if self.entries.len() >= 2 {
                    self.entries.remove(0);
                }
                (width, render())
            }
        };
        let lines = entry.1.clone();
        self.entries.push(entry);
        lines
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ToolStatus {
    Running { since: Instant },
    Ok { elapsed: Duration },
    Failed { elapsed: Duration, error: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum JobStatus {
    Running,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AgentStatus {
    Running,
    Completed,
    Failed(String),
    Interrupted,
}

/// A `job__*` or `agent__*` call folded into the row of the job or agent
/// it addresses, so checking on a job three times is three words on one
/// line rather than three cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FoldedCall {
    pub(super) call_id: String,
    pub(super) name: String,
    pub(super) output: Vec<String>,
    pub(super) open_line: bool,
    pub(super) pending: Vec<u8>,
}

impl FoldedCall {
    pub(super) fn new(call_id: String, name: String) -> Self {
        Self {
            call_id,
            name,
            output: Vec::new(),
            open_line: false,
            pending: Vec::new(),
        }
    }
}

/// Why the rewind picker cannot count exchanges past a command cell.
/// Decided when the command's turn is committed, from the command word
/// and the macros enabled at that moment, so a macro disabled later
/// still stops the picker at the turn it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RewindBarrier {
    /// A macro: its one cell stands for as many exchanges as it ran.
    Macro,
    /// A verb that swapped or rewrote the session's history behind the
    /// transcript (`.session`, `.agent`, `.exit session`, ...).
    Session,
}

impl RewindBarrier {
    /// What the picker's notice says it stopped at.
    pub(crate) fn kind_name(self) -> &'static str {
        match self {
            RewindBarrier::Macro => "macro turn",
            RewindBarrier::Session => "session command",
        }
    }
}

/// One entry of the transcript, kept as source so any width can render it.
pub(super) enum Cell {
    User {
        text: String,
        /// Whether the line's turn got a request off to the model, so
        /// the session holds an exchange under it even if no reply came
        /// back; `.undo` takes that exchange and this cell with it. A
        /// turn that failed before the request recorded nothing.
        opened: bool,
        /// A line queued while a turn ran and handed to the model on a
        /// tool result of that turn. It opens no exchange of its own (the
        /// running turn's holds it), so it is never a rewind row.
        /// The cell is committed when the line joins the next request's
        /// history, not when that request succeeds: the history keeps the
        /// line through a failed or interrupted request, so the cell is
        /// still the record of what the model receives.
        mid_turn: bool,
        cache: LineCache,
    },
    Assistant {
        source: String,
        done: bool,
        cache: RenderCache,
    },
    Notice {
        kind: NoticeKind,
        text: String,
        cache: LineCache,
    },
    Error {
        text: String,
        cache: LineCache,
    },
    /// A REPL command and everything it printed, already painted.
    Command {
        name: String,
        output: String,
        /// As `User::opened`.
        opened: bool,
        rewind_barrier: Option<RewindBarrier>,
        cache: LineCache,
    },
    /// Dim lines: tool output that belongs to no tool row, such as a
    /// background job still writing after its turn ended.
    Text {
        call_id: Option<String>,
        lines: Vec<String>,
        open_line: bool,
        pending: Vec<u8>,
    },
    /// A tool call: a one-line `name(args)` header, then the narration
    /// line and the tail of its output while it runs or when opened. A
    /// bookkeeping tool (memory, skills, agent listing) is only ever its
    /// header.
    Tool {
        call_id: String,
        name: String,
        /// The arguments cut to `SHORT_ARGS_BUDGET` for the header: the
        /// start hook's compact JSON once it has arrived, the narration
        /// line's text after the name until then.
        args: String,
        summary: String,
        status: ToolStatus,
        output: Vec<String>,
        open_line: bool,
        pending: Vec<u8>,
        expanded: bool,
        bookkeeping: bool,
    },
    Job {
        job_id: String,
        tool: String,
        status: JobStatus,
        calls: Vec<FoldedCall>,
        expanded: bool,
    },
    Agent {
        agent_id: String,
        name: String,
        status: AgentStatus,
        calls: Vec<FoldedCall>,
        expanded: bool,
    },
    /// The boxed greeting an empty transcript shows until a line is
    /// submitted.
    Welcome {
        model: String,
        prefix: CommandPrefix,
        cache: LineCache,
    },
}

/// Whether `name` is a tool whose call is housekeeping rather than work
/// (memory and skill lookups, listings): its row never opens and never
/// shows output. Reading the inbox is not: what a sibling agent wrote is
/// a prompt-injection carrier the human has to be able to see.
pub(super) fn is_bookkeeping(name: &str) -> bool {
    name.starts_with("memory__")
        || name.starts_with("skill__")
        || name.starts_with("agent__list_")
        || matches!(name, "job__list" | "agent__task_list")
}

/// The tool name and the arguments a narration line names, after `Call `.
fn narrated_call(line: &str) -> (String, String) {
    let plain = render::plain_text(line);
    let call = plain.strip_prefix("Call ").unwrap_or(&plain).trim_start();
    let (name, args) = call.split_once(char::is_whitespace).unwrap_or((call, ""));
    (name.to_string(), args.trim().to_string())
}

/// The tool name a narration line names, after `Call `.
pub(super) fn narrated_tool_name(line: &str) -> String {
    narrated_call(line).0
}

/// Width in characters of the arguments in a tool row's header, the
/// closing `...` of a cut argument string included.
const SHORT_ARGS_BUDGET: usize = 48;

/// The fewest argument characters a header keeps when the row is too
/// narrow for `SHORT_ARGS_BUDGET`, the `...` included.
const MIN_ARGS_BUDGET: usize = 8;

/// The first line of `args` cut to the header's budget, so a header
/// with a file's contents in it stays one row.
pub(super) fn short_args(args: &str) -> String {
    let line = render::plain_text(args);
    let line = line.lines().next().unwrap_or_default();
    cut_to(line, SHORT_ARGS_BUDGET)
}

/// `text` cut so a header with `reserved` other columns (the name, the
/// state token, their punctuation) fits a row `width` wide, since the
/// view clips the header from the right and the state token at its end
/// is the one thing a folded row has to say. The cut never goes below
/// `MIN_ARGS_BUDGET`, so a very narrow row loses the state rather than
/// every argument.
fn fit_to_row(text: &str, reserved: usize, width: u16) -> String {
    let budget = usize::from(width)
        .saturating_sub(reserved)
        .clamp(MIN_ARGS_BUDGET, SHORT_ARGS_BUDGET);
    cut_to(text, budget)
}

/// `text` cut to `budget` columns, `...` included, so a wide character
/// counts for the two cells it takes on the row.
fn cut_to(text: &str, budget: usize) -> String {
    if text.width() <= budget {
        return text.to_string();
    }
    let mut short = String::new();
    let mut used = 0;
    for c in text.chars() {
        let columns = c.width().unwrap_or(0);
        if used + columns > budget - 3 {
            break;
        }
        short.push(c);
        used += columns;
    }
    short.push_str("...");
    short
}

impl Cell {
    pub(super) fn user(text: &str) -> Self {
        Self::User {
            text: text.to_string(),
            opened: false,
            mid_turn: false,
            cache: LineCache::default(),
        }
    }

    pub(super) fn user_mid_turn(text: String) -> Self {
        Self::User {
            text,
            opened: false,
            mid_turn: true,
            cache: LineCache::default(),
        }
    }

    pub(super) fn assistant(source: String, done: bool) -> Self {
        Self::Assistant {
            source,
            done,
            cache: RenderCache::default(),
        }
    }

    pub(super) fn notice(kind: NoticeKind, text: String) -> Self {
        Self::Notice {
            kind,
            text,
            cache: LineCache::default(),
        }
    }

    pub(super) fn command(name: &str) -> Self {
        Self::Command {
            name: name.to_string(),
            output: String::new(),
            opened: false,
            rewind_barrier: None,
            cache: LineCache::default(),
        }
    }

    pub(super) fn tool(call_id: String, summary: String, since: Instant) -> Self {
        let (name, args) = narrated_call(&summary);
        let bookkeeping = is_bookkeeping(&name);
        Self::Tool {
            call_id,
            name,
            args: short_args(&args),
            summary,
            status: ToolStatus::Running { since },
            output: Vec::new(),
            open_line: false,
            pending: Vec::new(),
            expanded: false,
            bookkeeping,
        }
    }

    pub(super) fn welcome(model: String, prefix: CommandPrefix) -> Self {
        Self::Welcome {
            model,
            prefix,
            cache: LineCache::default(),
        }
    }

    /// Whether the row can be opened and closed, and so take the focus.
    pub(super) fn collapsible(&self) -> bool {
        match self {
            Cell::Tool { bookkeeping, .. } => !bookkeeping,
            Cell::Job { .. } | Cell::Agent { .. } => true,
            _ => false,
        }
    }

    pub(super) fn toggle(&mut self) {
        match self {
            Cell::Tool {
                expanded,
                bookkeeping: false,
                ..
            }
            | Cell::Job { expanded, .. }
            | Cell::Agent { expanded, .. } => {
                *expanded = !*expanded;
            }
            _ => {}
        }
    }

    /// Where the output of call `call_id` goes: this tool row's own
    /// lines, or those of the call folded into this job or agent row.
    pub(super) fn output_for(&mut self, call_id: &str) -> Option<OutputTarget<'_>> {
        match self {
            Cell::Tool {
                call_id: id,
                output,
                open_line,
                pending,
                ..
            } if id == call_id => Some((output, open_line, pending)),
            Cell::Job { calls, .. } | Cell::Agent { calls, .. } => {
                let call = calls
                    .iter_mut()
                    .rev()
                    .find(|call| call.call_id == call_id)?;
                Some((&mut call.output, &mut call.open_line, &mut call.pending))
            }
            _ => None,
        }
    }

    pub(super) fn lines(&mut self, width: u16, renderer: &mut Renderer) -> Vec<Line<'static>> {
        let no_color = renderer.no_color;
        match self {
            Cell::User {
                text,
                mid_turn: false,
                cache,
                ..
            } => cache.get(width, || {
                render::plain_lines(
                    &format!("> {text}"),
                    width,
                    Style::new().add_modifier(Modifier::BOLD),
                )
            }),
            Cell::User { text, cache, .. } => cache.get(width, || {
                render::plain_lines(&format!("> {text} (sent mid-turn)"), width, DIM)
            }),
            Cell::Assistant {
                source,
                done,
                cache,
            } => cache.lines(without_think_spans(source, *done), *done, width, renderer),
            Cell::Notice { kind, text, cache } => {
                let style = chrome_style(ChromeKind::Notice(*kind), no_color);
                cache.get(width, || render::plain_lines(text, width, style))
            }
            Cell::Error { text, cache } => cache.get(width, || {
                render::plain_lines(text, width, chrome_style(ChromeKind::Error, no_color))
            }),
            Cell::Command {
                name,
                output,
                cache,
                ..
            } => cache.get(width, || {
                // The header is the typed line; scrubbed like a User
                // cell's, so a pasted escape cannot paint through it.
                let header = render::plain_text(&format!("> {name}"));
                let mut lines = vec![Line::from(Span::styled(header, DIM))];
                if !output.is_empty() {
                    lines.extend(render::preformatted_lines(output));
                }
                lines
            }),
            Cell::Text { lines, .. } => dim_wrapped(output_lines(lines), width),
            Cell::Tool {
                name,
                args,
                summary,
                status,
                output,
                expanded,
                bookkeeping,
                ..
            } => {
                let state = match status {
                    ToolStatus::Running { .. } => "running".to_string(),
                    ToolStatus::Ok { elapsed } => format!("{}  ok", elapsed_text(*elapsed)),
                    ToolStatus::Failed { elapsed, error } => {
                        format!("{}  failed: {}", elapsed_text(*elapsed), first_line(error))
                    }
                };
                // The header is one row; the view clips what does not
                // fit, so the arguments give way to the state. The
                // narration below it is what the user approves, so that
                // wraps and is shown whole. The error in the header is
                // the tool's own text, so it is scrubbed.
                let reserved = name.width() + "()  ".len() + state.width();
                let args = fit_to_row(args, reserved, width);
                let header = render::plain_text(&format!("{name}({args})  {state}"));
                let mut lines = vec![Line::from(Span::styled(header, DIM))];
                let open = matches!(status, ToolStatus::Running { .. }) || *expanded;
                if open && !*bookkeeping {
                    let mut body = vec![render::styled_line(summary)];
                    if let ToolStatus::Failed { error, .. } = status {
                        body.extend(render::preformatted_lines(error));
                    }
                    body.extend(output_lines(output));
                    lines.extend(dim_wrapped(body, width));
                }
                lines
            }
            Cell::Job {
                job_id,
                tool,
                status,
                calls,
                expanded,
            } => {
                let state = match status {
                    JobStatus::Running => "running".to_string(),
                    JobStatus::Done => "done".to_string(),
                    JobStatus::Failed(error) => format!("failed: {}", first_line(error)),
                    JobStatus::Cancelled => "cancelled".to_string(),
                };
                let error = match status {
                    JobStatus::Failed(error) => Some(error.as_str()),
                    _ => None,
                };
                let reserved = "job   ".len() + state.width();
                let subject = fit_to_row(&format!("{tool} {job_id}"), reserved, width);
                let header = format!("job {subject}  {state}");
                row_lines(header, error, calls, *expanded, width)
            }
            Cell::Agent {
                agent_id,
                name,
                status,
                calls,
                expanded,
            } => {
                let state = match status {
                    AgentStatus::Running => "running".to_string(),
                    AgentStatus::Completed => "completed".to_string(),
                    AgentStatus::Failed(error) => format!("failed: {}", first_line(error)),
                    AgentStatus::Interrupted => "interrupted".to_string(),
                };
                let error = match status {
                    AgentStatus::Failed(error) => Some(error.as_str()),
                    _ => None,
                };
                let reserved = "agent   ".len() + state.width();
                let subject = fit_to_row(&format!("{name} {agent_id}"), reserved, width);
                let header = format!("agent {subject}  {state}");
                row_lines(header, error, calls, *expanded, width)
            }
            Cell::Welcome {
                model,
                prefix,
                cache,
            } => cache.get(width, || welcome_lines(model, *prefix, width)),
        }
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn elapsed_text(elapsed: Duration) -> String {
    format!("{:.1}s", elapsed.as_secs_f64())
}

fn output_lines(lines: &[String]) -> Vec<Line<'static>> {
    if lines.is_empty() {
        return Vec::new();
    }
    render::preformatted_lines(&lines.join("\n"))
}

fn dim_wrapped(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    render::hard_wrap(lines, width)
        .into_iter()
        .map(|line| line.patch_style(DIM))
        .collect()
}

/// A job's or agent's row: one header line with its folded calls
/// summarised, clipped by the view rather than wrapped; when open, the
/// whole error and then each folded call's output. The header is the
/// row's own text, so it is scrubbed rather than parsed for colour.
fn row_lines(
    header: String,
    error: Option<&str>,
    calls: &[FoldedCall],
    expanded: bool,
    width: u16,
) -> Vec<Line<'static>> {
    let summary = folded_summary(calls);
    let header = if summary.is_empty() {
        header
    } else {
        format!("{header}  {summary}")
    };
    let mut lines = vec![Line::from(Span::styled(render::plain_text(&header), DIM))];
    if expanded {
        let mut body = Vec::new();
        if let Some(error) = error {
            body.extend(render::preformatted_lines(&format!("error: {error}")));
        }
        for call in calls {
            body.extend(output_lines(&call.output));
        }
        lines.extend(dim_wrapped(body, width));
    }
    lines
}

/// `checked 2, collected`: the folded calls by verb, in the order first seen.
fn folded_summary(calls: &[FoldedCall]) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for call in calls {
        let verb = folded_verb(&call.name);
        match counts.iter_mut().find(|(seen, _)| *seen == verb) {
            Some((_, count)) => *count += 1,
            None => counts.push((verb, 1)),
        }
    }
    counts
        .into_iter()
        .map(|(verb, count)| {
            if count > 1 {
                format!("{verb} {count}")
            } else {
                verb.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn folded_verb(name: &str) -> &str {
    let action = name.split_once("__").map_or(name, |(_, action)| action);
    match action {
        "start" => "started",
        "spawn" => "spawned",
        "check" => "checked",
        "collect" => "collected",
        "cancel" => "cancelled",
        "send_message" => "messaged",
        other => other,
    }
}

/// The greeting, boxed and centred in `width`: the wordmark, the model,
/// the interrupt hint and the help hint spelled with the active command
/// prefix. Only BOLD and DIM mark it up, so it reads the same under
/// `NO_COLOR`.
pub(super) fn welcome_lines(model: &str, prefix: CommandPrefix, width: u16) -> Vec<Line<'static>> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let plain = Style::new();
    let help = prefix.render_hints(crate::repl::WELCOME_HELP_HINT);
    let mut text = vec![
        (format!("coyote {}", env!("CARGO_PKG_VERSION")), bold),
        (format!("model: {model}"), plain),
        (String::new(), plain),
    ];
    text.extend(
        crate::repl::WELCOME_INTERRUPT_HINT
            .lines()
            .map(|line| (line.to_string(), DIM)),
    );
    text.push((String::new(), plain));
    text.push((help.into_owned(), plain));

    // Two border columns and one space of padding on each side.
    const FRAME: usize = 4;
    let longest = text.iter().map(|(line, _)| line.width()).max().unwrap_or(0);
    let columns = usize::from(width);
    if columns <= FRAME {
        return text
            .into_iter()
            .flat_map(|(line, style)| render::plain_lines(&line, width, style))
            .collect();
    }
    let box_width = columns.min(longest + FRAME);
    let inner = box_width - FRAME;
    let inner_width = u16::try_from(inner).unwrap_or(u16::MAX);
    let margin = Span::raw(" ".repeat((columns - box_width) / 2));
    let edge = |left: &str, fill: &str, right: &str| {
        Line::from(vec![
            margin.clone(),
            Span::raw(format!("{left}{}{right}", fill.repeat(box_width - 2))),
        ])
    };
    let set = border::PLAIN;
    let mut lines = vec![edge(set.top_left, set.horizontal_top, set.top_right)];
    for (line, style) in text {
        for wrapped in render::plain_lines(&line, inner_width, style) {
            let pad = inner.saturating_sub(wrapped.width()) + 1;
            let mut spans = vec![margin.clone(), Span::raw(format!("{} ", set.vertical_left))];
            spans.extend(wrapped.spans);
            spans.push(Span::raw(format!(
                "{}{}",
                " ".repeat(pad),
                set.vertical_right
            )));
            lines.push(Line::from(spans));
        }
    }
    lines.push(edge(
        set.bottom_left,
        set.horizontal_bottom,
        set.bottom_right,
    ));
    lines
}

/// The lines a tool's output lands in, whether the last one is still
/// open, and the bytes of a character the last read cut in two.
pub(super) type OutputTarget<'a> = (&'a mut Vec<String>, &'a mut bool, &'a mut Vec<u8>);

/// Decodes one read of a byte stream, carrying an incomplete trailing
/// character over in `pending` so a sequence split across two reads
/// still decodes. Only a sequence that can never complete becomes U+FFFD.
pub(super) fn push_utf8_chunk(pending: &mut Vec<u8>, bytes: &[u8]) -> String {
    pending.extend_from_slice(bytes);
    let mut text = String::new();
    let mut rest: &[u8] = pending;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                text.push_str(valid);
                rest = &[];
                break;
            }
            Err(error) => {
                let (valid, tail) = rest.split_at(error.valid_up_to());
                text.push_str(std::str::from_utf8(valid).expect("prefix reported valid"));
                match error.error_len() {
                    Some(invalid) => {
                        text.push('\u{FFFD}');
                        rest = &tail[invalid..];
                    }
                    None => {
                        rest = tail;
                        break;
                    }
                }
            }
        }
    }
    *pending = rest.to_vec();
    text
}

/// Appends one read of a tool's output to its lines. The read may end
/// mid-line (`open_line`), in which case the next read continues it, or
/// mid-character (`pending`). The lines kept are bounded in count and in
/// bytes, oldest dropped first.
pub(super) fn append_output((lines, open_line, pending): OutputTarget<'_>, bytes: &[u8]) {
    let text = push_utf8_chunk(pending, bytes);
    if text.is_empty() {
        return;
    }
    let pieces: Vec<&str> = text.split('\n').collect();
    for (i, piece) in pieces.iter().enumerate() {
        let last_is_open = i == pieces.len() - 1;
        if i == 0 && *open_line {
            if let Some(last) = lines.last_mut() {
                last.push_str(piece);
                // A stream that never breaks its line would grow one
                // line without bound; it keeps its newest text, like
                // late output does.
                if last.len() > LATE_OUTPUT_LIMIT {
                    let cut = last.len() - LATE_OUTPUT_LIMIT;
                    let cut = (cut..=last.len())
                        .find(|at| last.is_char_boundary(*at))
                        .unwrap_or(last.len());
                    last.drain(..cut);
                }
            }
        } else if !(last_is_open && piece.is_empty()) {
            lines.push(piece.to_string());
        }
        *open_line = last_is_open && !piece.is_empty();
    }
    if lines.len() > TOOL_OUTPUT_TAIL {
        lines.drain(..lines.len() - TOOL_OUTPUT_TAIL);
    }
    // The lines that stay are bounded in bytes too, oldest first.
    let mut total: usize = lines.iter().map(|line| line.len() + 1).sum();
    while total > LATE_OUTPUT_LIMIT && lines.len() > 1 {
        total -= lines[0].len() + 1;
        lines.remove(0);
    }
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

/// The reply as the view shows it: the REPL's `strip_think_tag` applied
/// to text that may still be arriving. Only a span at the start of the
/// reply is reasoning; a tag quoted later in prose or code is shown as
/// written. An opening tag with no close yet hides the rest, and a close
/// with nothing visible ahead of it is the end of reasoning whose open
/// never reached this cell. While the reply is streaming, a tag cut by
/// a delta boundary at the tail is held back until the rest arrives.
pub(super) fn without_think_spans(source: &str, done: bool) -> &str {
    if !source.contains('<') {
        return source;
    }
    let body = source.trim_start();
    let after_reasoning = if let Some(span) = body.strip_prefix(THINK_OPEN) {
        Some(
            span.find(THINK_CLOSE)
                .map_or("", |close| &span[close + THINK_CLOSE.len()..]),
        )
    } else if let Some(after) = body.strip_prefix(THINK_CLOSE) {
        Some(after)
    } else if !done && partial_tag_at(body) == Some(0) {
        Some("")
    } else {
        None
    };
    let shown = after_reasoning.map_or(source, str::trim_start);
    let kept = if done {
        shown.len()
    } else {
        partial_tag_at(shown).unwrap_or(shown.len())
    };
    &shown[..kept]
}

/// Byte offset of a trailing fragment that could still grow into a
/// think tag, such as `<thi` or `</think`.
fn partial_tag_at(text: &str) -> Option<usize> {
    let at = text.rfind('<')?;
    let fragment = &text[at..];
    (fragment.len() < THINK_CLOSE.len()
        && (THINK_OPEN.starts_with(fragment) || THINK_CLOSE.starts_with(fragment)))
    .then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use std::cell::Cell as Counter;

    fn renderer() -> Renderer {
        let config = AppConfig {
            highlight: false,
            ..Default::default()
        };
        Renderer::new(config.render_options().unwrap()).unwrap()
    }

    /// Same width: the cached lines come back without a render. A new
    /// width renders once and is kept beside the previous one; only a
    /// third width evicts the oldest.
    #[test]
    fn the_line_cache_rerenders_on_a_width_change_only() {
        let renders = Counter::new(0);
        let mut cache = LineCache::default();
        let mut get = |width: u16| {
            cache.get(width, || {
                renders.set(renders.get() + 1);
                vec![Line::from(format!("w{width}"))]
            })
        };
        assert_eq!(get(80)[0].to_string(), "w80");
        get(80);
        get(80);
        assert_eq!(renders.get(), 1, "a repeated width rendered again");
        assert_eq!(get(40)[0].to_string(), "w40");
        assert_eq!(renders.get(), 2);
        get(80);
        get(40);
        assert_eq!(renders.get(), 2, "one of the two kept widths was lost");
        get(100);
        assert_eq!(renders.get(), 3);
        get(40);
        assert_eq!(renders.get(), 3, "the width used most recently was evicted");
        get(80);
        assert_eq!(renders.get(), 4, "the oldest width was kept");
    }

    #[test]
    fn welcome_lines_use_only_bold_and_dim() {
        for width in [80, 40, 20, 3] {
            let lines = welcome_lines("openai:test-model", CommandPrefix::Dot, width);
            for line in &lines {
                assert!(
                    line.width() <= usize::from(width),
                    "{width}: {line:?} is wider than the view"
                );
                for span in &line.spans {
                    assert_eq!(span.style.fg, None, "{span:?}");
                    assert_eq!(span.style.bg, None, "{span:?}");
                    let allowed = Modifier::BOLD | Modifier::DIM;
                    assert!(
                        allowed.contains(span.style.add_modifier),
                        "{span:?} carries more than BOLD or DIM"
                    );
                }
            }
            let text: Vec<String> = lines.iter().map(Line::to_string).collect();
            let text = text.join("\n");
            if width >= 40 {
                // Narrower boxes wrap these phrases across rows.
                assert!(text.contains("openai:test-model"), "{text}");
                assert!(text.contains("Tip: Press Ctrl-C"), "{text}");
                assert!(text.contains(".help"), "{text}");
            }
        }
    }

    #[test]
    fn welcome_lines_spell_the_help_hint_with_the_active_prefix() {
        let lines = welcome_lines("m", CommandPrefix::Slash, 80);
        let text: Vec<String> = lines.iter().map(Line::to_string).collect();
        assert!(
            text.iter().any(|line| line.contains("\"/help\"")),
            "{text:?}"
        );
    }

    #[test]
    fn folded_calls_summarise_by_verb() {
        let call = |name: &str| FoldedCall::new(String::new(), name.to_string());
        let calls = [call("job__check"), call("job__check"), call("job__collect")];
        assert_eq!(folded_summary(&calls), "checked 2, collected");
        assert_eq!(folded_summary(&[]), "");
        assert_eq!(
            folded_summary(&[call("agent__send_message"), call("agent__cancel")]),
            "messaged, cancelled"
        );
    }

    #[test]
    fn the_narrated_tool_name_is_the_word_after_call() {
        assert_eq!(
            narrated_tool_name("Call \x1b[1mfs_write\x1b[0m {\"path\":\"x\"}"),
            "fs_write"
        );
        assert_eq!(narrated_tool_name("Call todo__add {}"), "todo__add");
        assert_eq!(narrated_tool_name(""), "");
    }

    #[test]
    fn short_args_cut_at_the_budget_and_keep_a_short_line_whole() {
        assert_eq!(short_args("{\"path\":\"x\"}"), "{\"path\":\"x\"}");
        assert_eq!(short_args(""), "");
        let content = "x".repeat(2048);
        let args = serde_json::json!({ "path": "src/x.rs", "content": content }).to_string();
        let short = short_args(&args);
        assert_eq!(
            short,
            "{\"path\":\"src/x.rs\",\"content\":\"xxxxxxxxxxxxxxx..."
        );
        assert_eq!(short.chars().count(), SHORT_ARGS_BUDGET);
        assert_eq!(short_args("a\x1b[31mb\x1b[0m\nc"), "ab");
    }

    /// The row width sets the budget: a wide row keeps the stored
    /// arguments whole, a narrow one cuts them to what leaves the
    /// state in view, and no row cuts below the floor.
    #[test]
    fn fit_to_row_cuts_the_arguments_to_what_the_width_leaves() {
        let args = "x".repeat(SHORT_ARGS_BUDGET);
        assert_eq!(fit_to_row(&args, 20, 80), args);
        let narrow = fit_to_row(&args, 31, 40);
        assert_eq!(narrow, format!("{}...", "x".repeat(6)));
        let floor = fit_to_row(&args, 31, 10);
        assert_eq!(floor.chars().count(), MIN_ARGS_BUDGET);
        assert!(floor.ends_with("..."));
        assert_eq!(fit_to_row("short", 31, 40), "short");
        let wide = fit_to_row(&"\u{4e2d}".repeat(SHORT_ARGS_BUDGET), 31, 40);
        assert_eq!(wide, format!("{}...", "\u{4e2d}".repeat(3)));
        assert_eq!(wide.width(), 9);
    }

    /// A `content` argument of kilobytes leaves the collapsed row one
    /// line; the whole narration waits in the body for the row to open.
    #[test]
    fn a_large_argument_keeps_the_header_to_one_line_and_the_narration_in_the_body() {
        let mut renderer = renderer();
        let content = "x".repeat(2048);
        let narration =
            format!("Call fs_write {{\"path\":\"src/x.rs\",\"content\":\"{content}\"}}");
        let mut cell = Cell::tool("c1".to_string(), narration.clone(), Instant::now());
        let header = "fs_write({\"path\":\"src/x.rs\",\"content\":\"xxxxxxxxxxxxxxx...)";

        let lines = cell.lines(80, &mut renderer);
        assert_eq!(lines[0].to_string(), format!("{header}  running"));
        let body: String = lines[1..].iter().map(Line::to_string).collect();
        assert_eq!(
            body, narration,
            "the running row does not show the narration whole"
        );

        let Cell::Tool { status, .. } = &mut cell else {
            unreachable!();
        };
        *status = ToolStatus::Ok {
            elapsed: Duration::from_millis(1200),
        };
        let lines = cell.lines(80, &mut renderer);
        assert_eq!(lines.len(), 1, "a collapsed row took more than one line");
        assert_eq!(lines[0].to_string(), format!("{header}  1.2s  ok"));

        cell.toggle();
        let lines = cell.lines(80, &mut renderer);
        assert_eq!(lines[0].to_string(), format!("{header}  1.2s  ok"));
        let body: String = lines[1..].iter().map(Line::to_string).collect();
        assert_eq!(
            body, narration,
            "the opened row does not show the narration whole"
        );
        for line in &lines[1..] {
            assert!(line.width() <= 80, "{line:?} is wider than the view");
        }
    }

    /// The error a tool reports is its own text; a bidi override or an
    /// escape sequence in it reaches neither the header nor the body.
    #[test]
    fn a_failed_tool_header_drops_format_characters_and_escapes_from_the_error() {
        let mut renderer = renderer();
        let mut cell = Cell::tool(
            "c1".to_string(),
            "Call fs_read {\"path\":\"x\"}".to_string(),
            Instant::now(),
        );
        let Cell::Tool { status, .. } = &mut cell else {
            unreachable!();
        };
        *status = ToolStatus::Failed {
            elapsed: Duration::from_millis(100),
            error: "no such \u{202E}elif\x1b[31m red\x1b[0m".to_string(),
        };
        let lines = cell.lines(80, &mut renderer);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].to_string(),
            "fs_read({\"path\":\"x\"})  0.1s  failed: no such elif red"
        );
        cell.toggle();
        let text: Vec<String> = cell
            .lines(80, &mut renderer)
            .iter()
            .map(Line::to_string)
            .collect();
        let text = text.join("\n");
        assert!(!text.contains('\u{202E}'), "{text:?}");
        assert!(!text.contains('\x1b'), "{text:?}");
    }

    /// A job's or agent's failure keeps the collapsed row to one line
    /// however long the message; opening the row shows it whole.
    #[test]
    fn a_long_failure_stays_on_one_collapsed_line_and_opens_whole() {
        let mut renderer = renderer();
        let error = "e".repeat(300);
        let mut rows = [
            Cell::Job {
                job_id: "job-1".to_string(),
                tool: "execute_command".to_string(),
                status: JobStatus::Failed(error.clone()),
                calls: vec![FoldedCall::new("c1".to_string(), "job__check".to_string())],
                expanded: false,
            },
            Cell::Agent {
                agent_id: "agent-1".to_string(),
                name: "reviewer".to_string(),
                status: AgentStatus::Failed(error.clone()),
                calls: Vec::new(),
                expanded: false,
            },
        ];
        for cell in &mut rows {
            let lines = cell.lines(40, &mut renderer);
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].to_string().contains("failed: eee"));
            cell.toggle();
            let lines = cell.lines(40, &mut renderer);
            let body: String = lines[1..].iter().map(Line::to_string).collect();
            assert_eq!(body, format!("error: {error}"));
            for line in &lines[1..] {
                assert!(line.width() <= 40, "{line:?} is wider than the view");
            }
        }
    }

    /// A bookkeeping row has no body, so toggling it leaves it closed
    /// and one line tall.
    #[test]
    fn toggling_a_bookkeeping_row_does_nothing() {
        let mut renderer = renderer();
        let mut cell = Cell::tool(
            "c1".to_string(),
            "Call memory__search {\"query\":\"x\"}".to_string(),
            Instant::now(),
        );
        assert!(!cell.collapsible());
        cell.toggle();
        assert!(matches!(
            cell,
            Cell::Tool {
                expanded: false,
                ..
            }
        ));
        assert_eq!(cell.lines(80, &mut renderer).len(), 1);
    }

    #[test]
    fn bookkeeping_covers_lookups_and_listings_but_not_the_inbox() {
        for name in [
            "memory__search",
            "skill__load",
            "agent__list_skills",
            "job__list",
            "agent__task_list",
        ] {
            assert!(is_bookkeeping(name), "{name} should be bookkeeping");
        }
        for name in [
            "fs_read",
            "job__start",
            "job__check",
            "agent__spawn",
            "agent__collect",
            "agent__check_inbox",
        ] {
            assert!(!is_bookkeeping(name), "{name} is work, not bookkeeping");
        }
    }

    /// A character cut in two by the pipe read boundary decodes once its
    /// second half arrives; a byte that can never start a character is
    /// replaced on the spot.
    #[test]
    fn output_reads_decode_across_a_split_character() {
        let mut lines = Vec::new();
        let mut open_line = false;
        let mut pending = Vec::new();
        append_output((&mut lines, &mut open_line, &mut pending), b"caf\xC3");
        assert_eq!(lines, ["caf"]);
        assert_eq!(pending, [0xC3]);
        append_output((&mut lines, &mut open_line, &mut pending), b"\xA9\n");
        assert_eq!(lines, ["caf\u{e9}"]);
        assert!(pending.is_empty());
        assert!(!open_line);

        let mut pending = Vec::new();
        assert_eq!(push_utf8_chunk(&mut pending, b"a\xFFb"), "a\u{FFFD}b");
        assert!(pending.is_empty());
        assert_eq!(push_utf8_chunk(&mut pending, b"\xE2\x9C"), "");
        assert_eq!(pending, [0xE2, 0x9C]);
        assert_eq!(push_utf8_chunk(&mut pending, b"\x94 done"), "\u{2714} done");
        assert!(pending.is_empty());
    }

    /// A read that holds only the start of a character leaves the open
    /// line as it was; nothing has arrived to extend or close it.
    #[test]
    fn a_pending_only_read_keeps_the_line_open() {
        let mut lines = Vec::new();
        let mut open_line = false;
        let mut pending = Vec::new();
        append_output((&mut lines, &mut open_line, &mut pending), b"half");
        assert!(open_line);
        append_output((&mut lines, &mut open_line, &mut pending), b"\xF0\x9F");
        assert!(open_line);
        assert_eq!(lines, ["half"]);
        append_output((&mut lines, &mut open_line, &mut pending), b"\x98\x80\n");
        assert_eq!(lines, ["half\u{1F600}"]);
        assert!(!open_line);
    }

    #[test]
    fn output_keeps_only_the_newest_tail_of_lines_in_order() {
        let mut lines = Vec::new();
        let mut open_line = false;
        let mut pending = Vec::new();
        for n in 0..25u8 {
            let line = format!("{}\n", char::from(b'a' + n));
            append_output((&mut lines, &mut open_line, &mut pending), line.as_bytes());
        }
        assert_eq!(lines.len(), TOOL_OUTPUT_TAIL);
        let kept: String = lines.concat();
        assert_eq!(
            kept, "fghijklmnopqrstuvwxy",
            "the oldest lines were not the ones dropped"
        );
    }

    /// A streamed reply arrives as the deltas the chat stream emits:
    /// the open tag, the reasoning, the close tag, then the answer. No
    /// frame along the way shows a tag or a word of the reasoning, and
    /// the finished cell shows the answer alone.
    #[test]
    fn a_streaming_reply_never_shows_its_think_span() {
        let mut renderer = renderer();
        let mut cell = Cell::assistant(String::new(), false);
        let deltas = [
            "<thi",
            "nk>\n",
            "weighing the ",
            "options\n",
            "</thi",
            "nk>\n\n",
            "the ",
            "answer",
        ];
        for delta in deltas {
            let Cell::Assistant { source, .. } = &mut cell else {
                unreachable!()
            };
            source.push_str(delta);
            let frame: Vec<String> = cell
                .lines(80, &mut renderer)
                .iter()
                .map(Line::to_string)
                .collect();
            let frame = frame.join("\n");
            assert!(!frame.contains('<'), "{delta:?}: a tag showed:\n{frame}");
            assert!(
                !frame.contains("weighing") && !frame.contains("options"),
                "{delta:?}: the reasoning showed:\n{frame}"
            );
        }
        let Cell::Assistant { done, .. } = &mut cell else {
            unreachable!()
        };
        *done = true;
        let lines = cell.lines(80, &mut renderer);
        assert_eq!(lines[0].to_string().trim_end(), "the answer");
    }

    /// A close tag with nothing visible ahead of it ends reasoning
    /// whose open was never seen, a tag only partly arrived is held
    /// back while the reply is streaming, and a finished reply keeps a
    /// stray `<` as written.
    #[test]
    fn think_spans_are_cut_however_the_deltas_split_them() {
        assert_eq!(
            without_think_spans("<think>\nhmm\n</think>\n\nanswer", true),
            "answer"
        );
        assert_eq!(without_think_spans("\n</think>\n\nanswer", false), "answer");
        assert_eq!(without_think_spans("<think>\nhmm", false), "");
        assert_eq!(without_think_spans("<thi", false), "");
        assert_eq!(without_think_spans("answer</thi", false), "answer");
        assert_eq!(without_think_spans("answer <", false), "answer ");
        assert_eq!(without_think_spans("a < b", false), "a < b");
        assert_eq!(without_think_spans("a <", true), "a <");
    }

    /// Only the span at the start of a reply is reasoning: a tag the
    /// model quotes in prose or code stays on screen, as it does in the
    /// REPL.
    #[test]
    fn a_think_tag_quoted_after_visible_text_is_shown_as_written() {
        assert_eq!(
            without_think_spans("see </think> in code", true),
            "see </think> in code"
        );
        assert_eq!(
            without_think_spans("x <think>y</think> z", true),
            "x <think>y</think> z"
        );
        assert_eq!(
            without_think_spans("<think>x</think>two<think>y</think>three", true),
            "two<think>y</think>three"
        );
    }
}
