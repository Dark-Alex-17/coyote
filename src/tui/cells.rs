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
use unicode_width::UnicodeWidthStr;

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

/// One entry of the transcript, kept as source so any width can render it.
pub(super) enum Cell {
    User {
        text: String,
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
    /// A tool call: its narration line, then the tail of its output while
    /// it runs or when opened. A bookkeeping tool (memory, skills, agent
    /// listing) is only ever its narration line.
    Tool {
        call_id: String,
        name: String,
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

/// The tool name a narration line names, after `Call `.
pub(super) fn narrated_tool_name(line: &str) -> String {
    let plain = render::plain_text(line);
    plain
        .strip_prefix("Call ")
        .unwrap_or(&plain)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

impl Cell {
    pub(super) fn user(text: &str) -> Self {
        Self::User {
            text: text.to_string(),
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
            cache: LineCache::default(),
        }
    }

    pub(super) fn tool(call_id: String, summary: String, since: Instant) -> Self {
        let name = narrated_tool_name(&summary);
        let bookkeeping = is_bookkeeping(&name);
        Self::Tool {
            call_id,
            name,
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
            Cell::Tool { expanded, .. }
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
        let no_color = *crate::utils::NO_COLOR;
        match self {
            Cell::User { text, cache } => cache.get(width, || {
                render::plain_lines(
                    &format!("> {text}"),
                    width,
                    Style::new().add_modifier(Modifier::BOLD),
                )
            }),
            Cell::Assistant {
                source,
                done,
                cache,
            } => cache.lines(source, *done, width, renderer),
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
                let mut lines = vec![render::styled_line(&format!("{summary}  {state}"))];
                let open = matches!(status, ToolStatus::Running { .. }) || *expanded;
                if open && !*bookkeeping {
                    if let ToolStatus::Failed { error, .. } = status {
                        lines.extend(render::preformatted_lines(error));
                    }
                    lines.extend(output_lines(output));
                }
                dim_wrapped(lines, width)
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
                let header = format!("job {tool} {job_id}  {state}");
                dim_wrapped(row_lines(header, calls, *expanded), width)
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
                let header = format!("agent {name} {agent_id}  {state}");
                dim_wrapped(row_lines(header, calls, *expanded), width)
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

/// A job's or agent's row: the header with its folded calls summarised,
/// then each folded call's output when open. The header is the row's
/// own text, so it is scrubbed rather than parsed for colour.
fn row_lines(header: String, calls: &[FoldedCall], expanded: bool) -> Vec<Line<'static>> {
    let summary = folded_summary(calls);
    let header = if summary.is_empty() {
        header
    } else {
        format!("{header}  {summary}")
    };
    let mut lines = vec![Line::from(render::plain_text(&header))];
    if expanded {
        for call in calls {
            lines.extend(output_lines(&call.output));
        }
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
    let help = prefix.render_hints(r#"Type ".help" for additional help."#);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell as Counter;

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
}
