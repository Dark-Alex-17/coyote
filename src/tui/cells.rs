//! The transcript's entries, kept as source so any width can render them,
//! and the rules for how a tool, job or agent row collapses to one line.

use super::render::{self, RenderCache, Renderer};
use super::theme::{SWEEP_HIGHLIGHT, ToolHeaderStyles, logo_style, notice_style};
use super::throbber::{self, Tick};
use super::{
    ChromeKind, DIM, FULL_OUTPUT_LIMIT, LATE_OUTPUT_LIMIT, TOOL_OUTPUT_TAIL, chrome_style,
};
use crate::config::CommandPrefix;
use crate::events::NoticeKind;
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::borrow::Cow;
use std::fmt;
use std::time::{Duration, Instant};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Plain lines cached per width, or per whatever else `K` names that the
/// lines depend on; the two most recently used keys are kept.
#[derive(Default)]
pub(super) struct LineCache<K = u16> {
    entries: Vec<(K, Vec<Line<'static>>)>,
}

impl<K: Copy + PartialEq> LineCache<K> {
    pub(super) fn get(
        &mut self,
        key: K,
        render: impl FnOnce() -> Vec<Line<'static>>,
    ) -> &[Line<'static>] {
        match self.entries.iter().position(|(k, _)| *k == key) {
            Some(index) => {
                let entry = self.entries.remove(index);
                self.entries.push(entry);
            }
            None => {
                if self.entries.len() >= 2 {
                    self.entries.remove(0);
                }
                self.entries.push((key, render()));
            }
        }
        self.entries
            .last()
            .map_or(&[], |(_, lines)| lines.as_slice())
    }

    /// The lines laid out for `key`, if they have been.
    pub(super) fn peek(&self, key: K) -> Option<&[Line<'static>]> {
        self.entries
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, lines)| lines.as_slice())
    }
}

/// Every byte a call printed, up to `FULL_OUTPUT_LIMIT`, for the pager;
/// the row itself keeps only a tail. Past the cap the oldest text goes
/// and the count of what went is kept to be said; the budget over all
/// calls can take the whole text the same way.
#[derive(Default)]
pub(super) struct FullOutput {
    text: String,
    dropped: usize,
    /// The run of `\r` that ended the last push, held until the next push
    /// shows whether a `\n` follows it.
    pending_crs: usize,
    wrapped: Option<WrappedOutput>,
}

/// The lines of a `FullOutput` at one width. The text up to `committed`
/// ends on a whole line with no colour left open, so its lines cannot
/// change as more text arrives; only what follows is laid out again.
struct WrappedOutput {
    width: u16,
    /// The `dropped` count the lines were laid out with; once it moves
    /// the note and the text's offsets have changed, so all of it is.
    dropped: usize,
    committed: usize,
    committed_lines: usize,
    /// An escape came before `committed`, so the text after it starts in
    /// the style a reset leaves.
    reset: bool,
    lines: Vec<Line<'static>>,
}

impl FullOutput {
    /// Appends `text` with each run of `\r` before a `\n` dropped, even a
    /// run split across pushes, returning how many more bytes are kept now
    /// (fewer when the cap cut the oldest away).
    pub(super) fn push(&mut self, text: &str) -> isize {
        if text.is_empty() {
            return 0;
        }
        let mut crs = std::mem::take(&mut self.pending_crs);
        let mut chunk = String::with_capacity(crs + text.len());
        for c in text.chars() {
            match c {
                '\r' => crs += 1,
                '\n' => {
                    crs = 0;
                    chunk.push('\n');
                }
                _ => {
                    chunk.extend(std::iter::repeat_n('\r', crs));
                    crs = 0;
                    chunk.push(c);
                }
            }
        }
        self.pending_crs = crs;
        self.append(&chunk)
    }

    /// Keeps the `\r`s still held back by `push` as they are, for output
    /// that has ended.
    pub(super) fn settle(&mut self) -> isize {
        let crs = std::mem::take(&mut self.pending_crs);
        self.append(&"\r".repeat(crs))
    }

    fn append(&mut self, text: &str) -> isize {
        if text.is_empty() {
            return 0;
        }
        let before = self.text.len();
        self.text.push_str(text);
        if self.text.len() > FULL_OUTPUT_LIMIT {
            // Cut well under the cap, so a stream that keeps going past it
            // does not move the whole buffer on every read.
            let keep = FULL_OUTPUT_LIMIT - FULL_OUTPUT_LIMIT / 4;
            let mut cut = self.text.len() - keep;
            while !self.text.is_char_boundary(cut) {
                cut += 1;
            }
            self.text.drain(..cut);
            self.dropped += cut;
        }
        byte_delta(before, self.text.len())
    }

    pub(super) fn text(&self) -> &str {
        &self.text
    }

    pub(super) fn dropped(&self) -> usize {
        self.dropped
    }

    /// Gives up the whole kept text, counting it as not kept, and returns
    /// the bytes given up.
    fn evict(&mut self) -> usize {
        let freed = self.text.len();
        if freed == 0 {
            return 0;
        }
        self.dropped += freed;
        self.text = String::new();
        self.wrapped = None;
        freed
    }

    /// Frees the laid-out lines; the next read lays them out again.
    pub(super) fn release_cache(&mut self) {
        self.wrapped = None;
    }

    /// The kept text as dim lines broken at `width`, led by a line saying
    /// how much was not kept when the cap dropped any.
    pub(super) fn lines(&mut self, width: u16) -> &[Line<'static>] {
        let dropped = self.dropped;
        if self
            .wrapped
            .as_ref()
            .is_some_and(|wrapped| wrapped.width != width || wrapped.dropped != dropped)
        {
            self.wrapped = None;
        }
        let wrapped = self.wrapped.get_or_insert_with(|| {
            let lines = if dropped > 0 {
                dim_wrapped(
                    vec![Line::from(format!(
                        "[... {dropped} earlier bytes not kept]"
                    ))],
                    width,
                )
            } else {
                Vec::new()
            };
            WrappedOutput {
                width,
                dropped,
                committed: 0,
                committed_lines: lines.len(),
                reset: false,
                lines,
            }
        });
        wrapped.lines.truncate(wrapped.committed_lines);
        let fresh = &self.text[wrapped.committed..];
        let (settled, styled) = settled_prefix(fresh);
        if settled > 0 {
            wrapped.lines.extend(dim_wrapped(
                output_segment_lines(&fresh[..settled], wrapped.reset),
                width,
            ));
            wrapped.committed += settled;
            wrapped.committed_lines = wrapped.lines.len();
            wrapped.reset |= styled;
        }
        wrapped.lines.extend(dim_wrapped(
            output_segment_lines(&self.text[wrapped.committed..], wrapped.reset),
            width,
        ));
        &wrapped.lines
    }
}

/// The length of the longest prefix of `text` that ends a line with text
/// on it while no colour is left open, and whether an escape (a reset,
/// then) came before that end. Laid out apart from the rest, that prefix
/// gives the same lines as it does inside the whole: it ends on a line
/// with text, so no blank line is trimmed off its end, and the rest
/// starts in the style the whole would carry over to it.
fn settled_prefix(text: &str) -> (usize, bool) {
    let bytes = text.as_bytes();
    let mut plain = true;
    let mut styled = false;
    let mut settled = (0, false);
    for (at, &byte) in bytes.iter().enumerate() {
        match byte {
            0x1b => {
                let rest = &bytes[at..];
                plain = rest.starts_with(b"\x1b[0m") || rest.starts_with(b"\x1b[m");
                styled = true;
            }
            b'\n' if plain && at > 0 && !matches!(bytes[at - 1], b'\n' | b'\r') => {
                settled = (at + 1, styled);
            }
            _ => {}
        }
    }
    settled
}

/// The lines of a piece of output that starts in the style a reset left
/// when `reset`, plain style otherwise: once an escape has been read the
/// parser carries the reset style rather than none, and a piece laid out
/// on its own has to start from the same.
fn output_segment_lines(segment: &str, reset: bool) -> Vec<Line<'static>> {
    let trimmed = segment.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        Vec::new()
    } else if reset {
        render::preformatted_lines(&format!("\x1b[0m{trimmed}"))
    } else {
        render::preformatted_lines(trimmed)
    }
}

fn byte_delta(before: usize, after: usize) -> isize {
    let signed = |bytes: usize| isize::try_from(bytes).unwrap_or(isize::MAX);
    signed(after) - signed(before)
}

impl Clone for FullOutput {
    fn clone(&self) -> Self {
        Self {
            text: self.text.clone(),
            dropped: self.dropped,
            pending_crs: self.pending_crs,
            wrapped: None,
        }
    }
}

impl PartialEq for FullOutput {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.dropped == other.dropped
            && self.pending_crs == other.pending_crs
    }
}

impl Eq for FullOutput {}

impl fmt::Debug for FullOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FullOutput")
            .field("bytes", &self.text.len())
            .field("dropped", &self.dropped)
            .finish()
    }
}

/// Frees the pager's lines of every call's whole output.
pub(super) fn release_full_output_caches(cells: &mut [Cell]) {
    for full in cells.iter_mut().flat_map(Cell::full_outputs) {
        full.release_cache();
    }
}

#[cfg(test)]
pub(super) fn any_full_output_laid_out(cells: &mut [Cell]) -> bool {
    cells
        .iter_mut()
        .flat_map(Cell::full_outputs)
        .any(|full| full.wrapped.is_some())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ToolStatus {
    Running {
        since: Instant,
    },
    Ok {
        elapsed: Duration,
    },
    Failed {
        elapsed: Duration,
        error: String,
    },
    /// A call a saved session holds, which says neither how long it took
    /// nor whether the tool reported a failure.
    Recorded,
}

impl ToolStatus {
    /// Settles a running call as its finish hook reports it; a call
    /// already settled keeps its state.
    pub(super) fn finish(
        &mut self,
        now: Instant,
        ok: bool,
        error: Option<String>,
        duration_ms: Option<u64>,
    ) {
        let ToolStatus::Running { since } = *self else {
            return;
        };
        // The hook's own measurement when it has one; the UI's clock
        // otherwise, so a failed call still shows how long it took.
        let elapsed = duration_ms.map_or_else(|| now.duration_since(since), Duration::from_millis);
        *self = if ok {
            ToolStatus::Ok { elapsed }
        } else {
            ToolStatus::Failed {
                elapsed,
                error: error.unwrap_or_default(),
            }
        };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum JobStatus {
    Running { since: Instant },
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AgentStatus {
    Running { since: Instant },
    Completed,
    Failed(String),
    Interrupted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NodeStatus {
    Running {
        since: Instant,
    },
    Completed {
        elapsed: Option<Duration>,
        route: Option<String>,
    },
    Failed(String),
}

/// A `job__*` or `agent__*` call folded into the row of the job or agent
/// it addresses, so checking on a job three times is three words on one
/// line rather than three cells; or a child's tool call folded into the
/// row of the graph node it ran inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FoldedCall {
    pub(super) call_id: String,
    pub(super) name: String,
    /// A node's call keeps its arguments, which pair it with its finish
    /// and its saved entry when the provider gave it no id.
    pub(super) args: Option<Value>,
    pub(super) tail: OutputTail,
    /// Only a call folded into a node row has one; a job or agent row
    /// tracks no state per call.
    pub(super) status: Option<ToolStatus>,
}

impl FoldedCall {
    pub(super) fn new(call_id: String, name: String) -> Self {
        Self {
            call_id,
            name,
            args: None,
            tail: OutputTail::default(),
            status: None,
        }
    }

    pub(super) fn running(
        call_id: String,
        name: String,
        args: Option<Value>,
        since: Instant,
    ) -> Self {
        Self {
            args,
            status: Some(ToolStatus::Running { since }),
            ..Self::new(call_id, name)
        }
    }
}

/// What a call printed: the tail of lines its row shows, whether the
/// last of them is still open, the bytes of a character the last read
/// cut in two, and everything, for the pager.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct OutputTail {
    pub(super) lines: Vec<String>,
    pub(super) open_line: bool,
    pub(super) pending: Vec<u8>,
    pub(super) full: FullOutput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TaskRow<S> {
    pub(super) status: S,
    pub(super) calls: Vec<FoldedCall>,
    pub(super) expanded: bool,
}

impl<S> TaskRow<S> {
    pub(super) fn new(status: S) -> Self {
        Self {
            status,
            calls: Vec::new(),
            expanded: false,
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
    /// background job still writing after its turn ended. The cell keeps
    /// a tail like a tool row does.
    Text {
        call_id: Option<String>,
        tail: OutputTail,
        cache: LineCache,
    },
    /// A tool call: a one-line `name(args)` header behind a fold arrow,
    /// then, indented under it, the narration line and the tail of its
    /// output once opened or while its own confirm question waits. A
    /// bookkeeping tool (memory, skills, agent listing) is only ever its
    /// header inline; the pager still shows its output.
    Tool {
        call_id: String,
        name: String,
        /// The arguments cut to `SHORT_ARGS_BUDGET` for the header: the
        /// start hook's compact JSON once it has arrived, the narration
        /// line's text after the name until then.
        args: String,
        /// The arguments the summary line names after the tool name,
        /// whole: the open row repeats the summary only when the header
        /// shows something else.
        narrated_args: String,
        summary: String,
        status: ToolStatus,
        tail: OutputTail,
        expanded: bool,
        /// A confirm this call asked is pending: what is being approved
        /// stays on screen until it is answered.
        awaiting: bool,
        bookkeeping: bool,
        cache: LineCache,
    },
    Job {
        job_id: String,
        tool: String,
        row: TaskRow<JobStatus>,
        cache: LineCache,
    },
    Agent {
        agent_id: String,
        name: String,
        row: TaskRow<AgentStatus>,
        cache: LineCache,
    },
    /// A graph node a child agent ran, with the tool calls it made folded
    /// under it.
    Node {
        node_id: String,
        node_type: String,
        row: TaskRow<NodeStatus>,
        cache: LineCache,
    },
    /// The boxed greeting an empty transcript shows until a line is
    /// submitted, laid out per width and the height it has to fit.
    Welcome {
        model: String,
        prefix: CommandPrefix,
        /// Where the startup sweep is across the wordmark; None once it
        /// has settled, or never started.
        sweep: Option<usize>,
        cache: LineCache<(u16, Option<u16>)>,
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

/// Width in characters of the arguments in a tool row's header, the
/// closing `...` of a cut argument string included.
const SHORT_ARGS_BUDGET: usize = 48;

/// The fold arrows in front of a tool row's header.
pub(super) const COLLAPSED_ARROW: &str = "\u{25B8} ";
pub(super) const EXPANDED_ARROW: &str = "\u{25BE} ";
/// How far an open tool row's narration and output sit in from its
/// header.
pub(super) const BODY_INDENT: &str = "  ";

/// The fewest argument characters a header keeps when the row is too
/// narrow for `SHORT_ARGS_BUDGET`, the `...` included.
const MIN_ARGS_BUDGET: usize = 6;

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

    /// A running call's row. `summary` is the line drawn when the row is
    /// open; `name` and `args` are what it says, `args` as the words after
    /// the name.
    pub(super) fn tool(
        call_id: String,
        name: &str,
        args: &str,
        summary: String,
        since: Instant,
    ) -> Self {
        let name = render::plain_text(name);
        let narrated_args = render::plain_text(args).trim().to_string();
        let bookkeeping = is_bookkeeping(&name);
        Self::Tool {
            call_id,
            name,
            args: short_args(&narrated_args),
            narrated_args,
            summary,
            status: ToolStatus::Running { since },
            tail: OutputTail::default(),
            expanded: false,
            awaiting: false,
            bookkeeping,
            cache: LineCache::default(),
        }
    }

    /// A call read back from a saved session: a finished tool row, folded,
    /// with what the call returned as its output.
    pub(super) fn recorded_tool(
        call_id: String,
        name: &str,
        args: &str,
        summary: String,
        output: &str,
    ) -> Self {
        let mut cell = Self::tool(call_id, name, args, summary, Instant::now());
        if let Cell::Tool { status, tail, .. } = &mut cell {
            *status = ToolStatus::Recorded;
            let text = append_output(tail, output.as_bytes());
            tail.full.push(&text);
            tail.full.settle();
        }
        cell
    }

    pub(super) fn welcome(model: String, prefix: CommandPrefix) -> Self {
        Self::Welcome {
            model,
            prefix,
            sweep: None,
            cache: LineCache::default(),
        }
    }

    pub(super) fn node(node_id: String, node_type: String, since: Instant) -> Self {
        Self::Node {
            node_id,
            node_type,
            row: TaskRow::new(NodeStatus::Running { since }),
            cache: LineCache::default(),
        }
    }

    /// Whether the row's work has yet to end, so its throbber turns.
    pub(super) fn running(&self) -> bool {
        match self {
            Cell::Tool { status, .. } => matches!(status, ToolStatus::Running { .. }),
            Cell::Job { row, .. } => matches!(row.status, JobStatus::Running { .. }),
            Cell::Agent { row, .. } => matches!(row.status, AgentStatus::Running { .. }),
            // A call can still run under a node that has ended, as one
            // started after the node's end is filed under it.
            Cell::Node { row, .. } => {
                matches!(row.status, NodeStatus::Running { .. })
                    || row
                        .calls
                        .iter()
                        .any(|call| matches!(call.status, Some(ToolStatus::Running { .. })))
            }
            _ => false,
        }
    }

    /// Whether the row can be opened and closed, and so take the focus.
    pub(super) fn collapsible(&self) -> bool {
        match self {
            Cell::Tool { bookkeeping, .. } => !bookkeeping,
            Cell::Job { .. } | Cell::Agent { .. } | Cell::Node { .. } => true,
            _ => false,
        }
    }

    pub(super) fn toggle(&mut self) {
        match self {
            Cell::Tool {
                expanded,
                bookkeeping: false,
                cache,
                ..
            }
            | Cell::Job {
                row: TaskRow { expanded, .. },
                cache,
                ..
            }
            | Cell::Agent {
                row: TaskRow { expanded, .. },
                cache,
                ..
            }
            | Cell::Node {
                row: TaskRow { expanded, .. },
                cache,
                ..
            } => {
                *expanded = !*expanded;
                *cache = LineCache::default();
            }
            _ => {}
        }
    }

    /// Marks whether a confirm asked by this tool row's call is pending,
    /// returning whether that changed what the row shows.
    pub(super) fn set_awaiting(&mut self, pending: &dyn Fn(&str) -> bool) -> bool {
        if let Cell::Tool {
            call_id, awaiting, ..
        } = self
        {
            let now = pending(call_id);
            if *awaiting != now {
                *awaiting = now;
                return true;
            }
        }
        false
    }

    /// Where the output of call `call_id` goes: this tool row's own tail,
    /// or that of the call folded into this job, agent or node row.
    pub(super) fn output_for(&mut self, call_id: &str) -> Option<&mut OutputTail> {
        match self {
            Cell::Tool {
                call_id: id, tail, ..
            } if id == call_id => Some(tail),
            Cell::Job {
                row: TaskRow { calls, .. },
                ..
            }
            | Cell::Agent {
                row: TaskRow { calls, .. },
                ..
            }
            | Cell::Node {
                row: TaskRow { calls, .. },
                ..
            } => calls
                .iter_mut()
                .rev()
                .find(|call| call.call_id == call_id)
                .map(|call| &mut call.tail),
            _ => None,
        }
    }

    /// Whether `output_for(call_id)` finds somewhere in this cell.
    pub(super) fn takes_output_of(&self, call_id: &str) -> bool {
        self.calls_with_id(call_id) > 0
    }

    /// How many calls in this cell carry `call_id`: a provider may repeat
    /// an id, so a job, agent or node row can hold more than one.
    pub(super) fn calls_with_id(&self, call_id: &str) -> usize {
        match self {
            Cell::Tool { call_id: id, .. } => usize::from(id == call_id),
            Cell::Job {
                row: TaskRow { calls, .. },
                ..
            }
            | Cell::Agent {
                row: TaskRow { calls, .. },
                ..
            }
            | Cell::Node {
                row: TaskRow { calls, .. },
                ..
            } => calls.iter().filter(|call| call.call_id == call_id).count(),
            _ => 0,
        }
    }

    /// The output tail of the `nth` call in this cell carrying `call_id`,
    /// in call order.
    pub(super) fn nth_output_for(&mut self, call_id: &str, nth: usize) -> Option<&mut OutputTail> {
        match self {
            Cell::Tool {
                call_id: id, tail, ..
            } if id == call_id && nth == 0 => Some(tail),
            Cell::Job {
                row: TaskRow { calls, .. },
                ..
            }
            | Cell::Agent {
                row: TaskRow { calls, .. },
                ..
            }
            | Cell::Node {
                row: TaskRow { calls, .. },
                ..
            } => calls
                .iter_mut()
                .filter(|call| call.call_id == call_id)
                .nth(nth)
                .map(|call| &mut call.tail),
            _ => None,
        }
    }

    pub(super) fn expanded(&self) -> bool {
        match self {
            Cell::Tool { expanded, .. }
            | Cell::Job {
                row: TaskRow { expanded, .. },
                ..
            }
            | Cell::Agent {
                row: TaskRow { expanded, .. },
                ..
            }
            | Cell::Node {
                row: TaskRow { expanded, .. },
                ..
            } => *expanded,
            _ => false,
        }
    }

    /// The whole output of a tool, job, agent or node row as Alt+C copies
    /// it; None for every other cell.
    pub(super) fn copied_output(&self) -> Option<CopiedOutput> {
        match self {
            Cell::Tool { name, tail, .. } => Some(CopiedOutput {
                from: name.clone(),
                text: render::plain_text(tail.full.text()),
                dropped: tail.full.dropped(),
            }),
            Cell::Job { tool, row, .. } => Some(row_output(format!("job {tool}"), &row.calls)),
            Cell::Agent { name, row, .. } => Some(row_output(format!("agent {name}"), &row.calls)),
            Cell::Node { node_id, row, .. } => {
                Some(row_output(format!("node {node_id}"), &row.calls))
            }
            _ => None,
        }
    }

    /// Every whole-output buffer the cell holds, oldest call first.
    fn full_outputs(&mut self) -> Vec<&mut FullOutput> {
        match self {
            Cell::Tool { tail, .. } | Cell::Text { tail, .. } => vec![&mut tail.full],
            Cell::Job {
                row: TaskRow { calls, .. },
                ..
            }
            | Cell::Agent {
                row: TaskRow { calls, .. },
                ..
            }
            | Cell::Node {
                row: TaskRow { calls, .. },
                ..
            } => calls.iter_mut().map(|call| &mut call.tail.full).collect(),
            _ => Vec::new(),
        }
    }

    /// The bytes every whole-output buffer of the cell keeps.
    pub(super) fn full_output_bytes(&self) -> usize {
        match self {
            Cell::Tool { tail, .. } | Cell::Text { tail, .. } => tail.full.text.len(),
            Cell::Job {
                row: TaskRow { calls, .. },
                ..
            }
            | Cell::Agent {
                row: TaskRow { calls, .. },
                ..
            }
            | Cell::Node {
                row: TaskRow { calls, .. },
                ..
            } => calls.iter().map(|call| call.tail.full.text.len()).sum(),
            _ => 0,
        }
    }

    /// Gives up whole outputs, oldest call first, until at least `excess`
    /// bytes are freed or none is left; returns the bytes freed.
    pub(super) fn evict_full_outputs(&mut self, excess: usize) -> usize {
        let mut freed = 0;
        for full in self.full_outputs() {
            if freed >= excess {
                break;
            }
            freed += full.evict();
        }
        freed
    }

    /// Drops the cell's cached lines so the next layout styles it anew.
    pub(super) fn forget_layout(&mut self) {
        if let Cell::Assistant { cache, .. } = self {
            *cache = RenderCache::default();
        }
        self.forget_wrapped_lines();
    }

    /// Drops the wrapped lines of every cell but a reply, whose cache
    /// already follows its source; the transcript calls this on every
    /// change to a cell.
    pub(super) fn forget_wrapped_lines(&mut self) {
        if let Cell::User { cache, .. }
        | Cell::Notice { cache, .. }
        | Cell::Error { cache, .. }
        | Cell::Command { cache, .. }
        | Cell::Text { cache, .. }
        | Cell::Tool { cache, .. }
        | Cell::Job { cache, .. }
        | Cell::Agent { cache, .. }
        | Cell::Node { cache, .. } = self
        {
            *cache = LineCache::default();
        }
        if let Cell::Welcome { cache, .. } = self {
            *cache = LineCache::default();
        }
    }

    pub(super) fn lines(&mut self, width: u16, renderer: &mut Renderer) -> Vec<Line<'static>> {
        self.cached_lines(width, None, renderer).into_owned()
    }

    /// The cell's lines at `width`, borrowed from its cache; only a reply
    /// still streaming comes back owned, as its tail is rendered anew.
    /// `height` is the room the cell is shown in, when it is the only one;
    /// only the greeting fits itself to it.
    pub(super) fn cached_lines(
        &mut self,
        width: u16,
        height: Option<u16>,
        renderer: &mut Renderer,
    ) -> Cow<'_, [Line<'static>]> {
        let no_color = renderer.no_color;
        let faded = renderer.faded;
        let tick = Some(renderer.tick);
        Cow::Borrowed(match self {
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
                render::plain_lines(&format!("> {text} (sent mid-turn)"), width, faded)
            }),
            Cell::Assistant {
                source,
                done: true,
                cache,
            } => cache.finished_lines(without_think_spans(source, true), width, renderer),
            Cell::Assistant {
                source,
                done,
                cache,
            } => {
                return Cow::Owned(cache.lines(
                    without_think_spans(source, *done),
                    *done,
                    width,
                    renderer,
                ));
            }
            Cell::Notice { kind, text, cache } => {
                let style = notice_style(*kind, faded, no_color);
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
                let mut lines = vec![Line::from(Span::styled(header, faded))];
                if !output.is_empty() {
                    lines.extend(render::preformatted_lines(output));
                }
                lines
            }),
            Cell::Text { tail, cache, .. } => {
                cache.get(width, || dim_wrapped(output_lines(&tail.lines), width))
            }
            Cell::Tool {
                name,
                args,
                summary,
                status,
                tail,
                expanded,
                awaiting,
                bookkeeping,
                cache,
                ..
            } => cache.get(width, || {
                let styles = ToolHeaderStyles::new(renderer.markdown_render(), no_color);
                let open = (*expanded || *awaiting) && !*bookkeeping;
                let arrow = match (*bookkeeping, open) {
                    (true, _) => "  ",
                    (false, true) => EXPANDED_ARROW,
                    (false, false) => COLLAPSED_ARROW,
                };
                let state = tool_state(status, tick);
                // The header is one row; the view clips what does not
                // fit, so the arguments give way to the state. The
                // narration below it is what the user approves, so that
                // wraps and is shown whole. The error in the header is
                // the tool's own text, so it is scrubbed.
                let reserved = arrow.width() + name.width() + "()  ".len() + state.width();
                let args = fit_to_row(args, reserved, width);
                let state_style = if matches!(status, ToolStatus::Failed { .. }) {
                    styles.failed
                } else {
                    styles.muted
                };
                let mut header = vec![
                    Span::styled(arrow, styles.muted),
                    Span::styled(render::plain_text(name), styles.name),
                    Span::styled(render::plain_text(&format!("({args})  ")), styles.muted),
                ];
                header.extend(state_spans(&state, state_style, styles.throbber));
                let mut lines = vec![Line::from(header)];
                if open {
                    let body_width = width.saturating_sub(BODY_INDENT.len() as u16).max(1);
                    let mut body =
                        render::hard_wrap(vec![render::styled_line(summary)], body_width);
                    let mut rest = Vec::new();
                    if let ToolStatus::Failed { error, .. } = status {
                        rest.extend(render::preformatted_lines(error));
                    }
                    rest.extend(output_lines(&tail.lines));
                    body.extend(dim_wrapped(rest, body_width));
                    lines.extend(body.into_iter().map(|mut line| {
                        line.spans.insert(0, Span::raw(BODY_INDENT));
                        line
                    }));
                }
                lines
            }),
            Cell::Job {
                job_id,
                tool,
                row,
                cache,
            } => cache.get(width, || {
                let styles = ToolHeaderStyles::new(renderer.markdown_render(), no_color);
                let header = RowHeader {
                    kind: "job",
                    name: tool,
                    id: job_id,
                    state: job_state(&row.status, tick),
                };
                row_lines(header, row, width, &styles)
            }),
            Cell::Agent {
                agent_id,
                name,
                row,
                cache,
            } => cache.get(width, || {
                let styles = ToolHeaderStyles::new(renderer.markdown_render(), no_color);
                let header = RowHeader {
                    kind: "agent",
                    name,
                    id: agent_id,
                    state: agent_state(&row.status, tick),
                };
                row_lines(header, row, width, &styles)
            }),
            Cell::Node {
                node_id,
                node_type,
                row,
                cache,
            } => cache.get(width, || {
                let styles = ToolHeaderStyles::new(renderer.markdown_render(), no_color);
                let header = RowHeader {
                    kind: "node",
                    name: node_id,
                    id: node_type,
                    state: node_state(&row.status, tick),
                };
                row_lines(header, row, width, &styles)
            }),
            Cell::Welcome {
                model,
                prefix,
                sweep,
                cache,
            } => cache.get((width, height), || {
                let logo = logo_style(renderer.markdown_render(), no_color);
                welcome_lines(model, *prefix, width, height, logo, *sweep)
            }),
        })
    }

    /// The cell as the pager shows it: a tool, job or agent row open,
    /// with everything its calls printed rather than the inline tail, and
    /// all of a stray call's output. Every other cell reads as it does in
    /// the transcript.
    pub(super) fn full_lines(&mut self, width: u16, renderer: &mut Renderer) -> Vec<Line<'static>> {
        if let Cell::Text { tail, .. } = self {
            return tail.full.lines(width).to_vec();
        }
        let tick = Some(renderer.tick);
        let Some(header) = self.summary_line(tick) else {
            return self.lines(width, renderer);
        };
        match self {
            Cell::Tool {
                summary,
                status,
                tail,
                ..
            } => {
                let mut lines = render::plain_lines(&header, width, DIM);
                let mut body = vec![render::styled_line(summary)];
                if let ToolStatus::Failed { error, .. } = status {
                    body.extend(render::preformatted_lines(error));
                }
                lines.extend(dim_wrapped(body, width));
                lines.extend_from_slice(tail.full.lines(width));
                lines
            }
            Cell::Job { row, .. } => full_row_lines(
                &header,
                job_state(&row.status, tick).1,
                &mut row.calls,
                width,
            ),
            Cell::Agent { row, .. } => full_row_lines(
                &header,
                agent_state(&row.status, tick).1,
                &mut row.calls,
                width,
            ),
            Cell::Node { row, .. } => full_row_lines(
                &header,
                node_state(&row.status, tick).1,
                &mut row.calls,
                width,
            ),
            _ => render::plain_lines(&header, width, DIM),
        }
    }

    /// The row as a child's thread shows it: folded as in the transcript,
    /// or open on everything its calls printed, each call under a node
    /// with its own state. The call line under a tool row's header is left
    /// out when the header already shows the arguments whole, since a
    /// child's call has no narration of its own.
    pub(super) fn thread_lines(
        &mut self,
        width: u16,
        renderer: &mut Renderer,
    ) -> Vec<Line<'static>> {
        let mut lines = self.lines(width, renderer);
        if !self.expanded() {
            return lines;
        }
        let tick = Some(renderer.tick);
        let styles = ToolHeaderStyles::new(renderer.markdown_render(), renderer.no_color);
        let body_width = width.saturating_sub(BODY_INDENT.len() as u16).max(1);
        let body = match self {
            Cell::Tool {
                args,
                narrated_args,
                summary,
                status,
                tail,
                ..
            } => {
                let mut body = Vec::new();
                if narrated_args != args {
                    body = render::hard_wrap(vec![render::styled_line(summary)], body_width);
                }
                if let ToolStatus::Failed { error, .. } = status {
                    body.extend(dim_wrapped(render::preformatted_lines(error), body_width));
                }
                body.extend_from_slice(tail.full.lines(body_width));
                body
            }
            Cell::Node { row, .. } => {
                let mut body = Vec::new();
                if let NodeStatus::Failed(error) = &row.status {
                    body.extend(dim_wrapped(
                        render::preformatted_lines(&format!("error: {error}")),
                        body_width,
                    ));
                }
                for call in &mut row.calls {
                    body.extend(folded_call_line(call, tick, &styles));
                    body.extend_from_slice(call.tail.full.lines(body_width));
                }
                body
            }
            _ => return lines,
        };
        lines.truncate(1);
        lines.extend(body.into_iter().map(|mut line| {
            line.spans.insert(0, Span::raw(BODY_INDENT));
            line
        }));
        lines
    }

    /// The one-line, unclipped header of a tool, job or agent row, with
    /// no escape left in it, a running row's throbber and elapsed time
    /// read at `tick`; None for every other cell.
    pub(super) fn summary_line(&self, tick: Option<Tick>) -> Option<String> {
        let line = match self {
            Cell::Tool {
                name, args, status, ..
            } => format!("{name}({args})  {}", tool_state(status, tick)),
            Cell::Job {
                job_id, tool, row, ..
            } => with_folded(
                format!("job {tool} {job_id}  {}", job_state(&row.status, tick).0),
                &row.calls,
            ),
            Cell::Agent {
                agent_id,
                name,
                row,
                ..
            } => with_folded(
                format!(
                    "agent {name} {agent_id}  {}",
                    agent_state(&row.status, tick).0
                ),
                &row.calls,
            ),
            Cell::Node {
                node_id,
                node_type,
                row,
                ..
            } => with_folded(
                format!(
                    "node {node_id} {node_type}  {}",
                    node_state(&row.status, tick).0
                ),
                &row.calls,
            ),
            _ => return None,
        };
        Some(render::plain_text(&line))
    }
}

/// A running row's state: the throbber's frame, `running` and how long
/// the row has run by `tick`; the word alone without a tick, as in the
/// transcript printed on exit.
fn running_state(since: Instant, tick: Option<Tick>) -> String {
    match tick {
        Some(tick) => format!(
            "{} running {}",
            tick.frame(),
            elapsed_text(tick.elapsed(since))
        ),
        None => "running".to_string(),
    }
}

/// A row's state, scrubbed, with a leading throbber frame in a style of
/// its own.
fn state_spans(state: &str, style: Style, throbber_style: Style) -> Vec<Span<'static>> {
    let state = render::plain_text(state);
    match throbber::split_frame(&state) {
        Some((frame, rest)) => vec![
            Span::styled(frame, throbber_style),
            Span::styled(rest.to_string(), style),
        ],
        None => vec![Span::styled(state, style)],
    }
}

fn tool_state(status: &ToolStatus, tick: Option<Tick>) -> String {
    match status {
        ToolStatus::Running { since } => running_state(*since, tick),
        ToolStatus::Ok { elapsed } => format!("{}  ok", elapsed_text(*elapsed)),
        ToolStatus::Failed { elapsed, error } => {
            format!("{}  failed: {}", elapsed_text(*elapsed), first_line(error))
        }
        ToolStatus::Recorded => "done".to_string(),
    }
}

/// A call folded under a node, as a line of the node's open row: its name
/// and its state in the colours of a tool row's header. None for a call
/// that tracks no state, as under a job or agent row.
fn folded_call_line(
    call: &FoldedCall,
    tick: Option<Tick>,
    styles: &ToolHeaderStyles,
) -> Option<Line<'static>> {
    let status = call.status.as_ref()?;
    let state_style = if matches!(status, ToolStatus::Failed { .. }) {
        styles.failed
    } else {
        styles.muted
    };
    let mut spans = vec![
        Span::styled(render::plain_text(&call.name), styles.name),
        Span::styled("  ", styles.muted),
    ];
    spans.extend(state_spans(
        &tool_state(status, tick),
        state_style,
        styles.throbber,
    ));
    Some(Line::from(spans))
}

/// The state word of a job row and the whole error of a failed one.
fn job_state(status: &JobStatus, tick: Option<Tick>) -> (String, Option<&str>) {
    match status {
        JobStatus::Running { since } => (running_state(*since, tick), None),
        JobStatus::Done => ("done".to_string(), None),
        JobStatus::Failed(error) => (format!("failed: {}", first_line(error)), Some(error)),
        JobStatus::Cancelled => ("cancelled".to_string(), None),
    }
}

/// The state word of an agent row and the whole error of a failed one.
fn agent_state(status: &AgentStatus, tick: Option<Tick>) -> (String, Option<&str>) {
    match status {
        AgentStatus::Running { since } => (running_state(*since, tick), None),
        AgentStatus::Completed => ("completed".to_string(), None),
        AgentStatus::Failed(error) => (format!("failed: {}", first_line(error)), Some(error)),
        AgentStatus::Interrupted => ("interrupted".to_string(), None),
    }
}

/// The state word of a node row and the whole error of a failed one.
fn node_state(status: &NodeStatus, tick: Option<Tick>) -> (String, Option<&str>) {
    match status {
        NodeStatus::Running { since } => (running_state(*since, tick), None),
        NodeStatus::Completed { elapsed, route } => {
            let mut state = "completed".to_string();
            if let Some(elapsed) = elapsed {
                state.push_str(&format!(" in {}", elapsed_text(*elapsed)));
            }
            if let Some(route) = route {
                state.push_str(&format!(", route {route}"));
            }
            (state, None)
        }
        NodeStatus::Failed(error) => (format!("failed: {}", first_line(error)), Some(error)),
    }
}

fn with_folded(header: String, calls: &[FoldedCall]) -> String {
    let summary = folded_summary(calls);
    if summary.is_empty() {
        header
    } else {
        format!("{header}  {summary}")
    }
}

/// A job's or agent's row in the pager: the header wrapped, the whole
/// error, then each folded call that printed anything under its name.
fn full_row_lines(
    header: &str,
    error: Option<&str>,
    calls: &mut [FoldedCall],
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = render::plain_lines(header, width, DIM);
    if let Some(error) = error {
        lines.extend(dim_wrapped(
            render::preformatted_lines(&format!("error: {error}")),
            width,
        ));
    }
    for call in calls {
        if !printed(&call.tail.full) {
            continue;
        }
        lines.extend(render::plain_lines(&format!("{}:", call.name), width, DIM));
        lines.extend_from_slice(call.tail.full.lines(width));
    }
    lines
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

/// An elapsed time in whole seconds: `42s`, `1m 3s`, `2h 36m 5s`. Hours
/// are the largest unit, and every smaller unit shows once a larger does.
pub(super) fn elapsed_text(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
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

/// A row's whole output with every escape stripped, what it is called in
/// the notice, and the bytes of it already dropped.
pub(super) struct CopiedOutput {
    pub(super) from: String,
    pub(super) text: String,
    pub(super) dropped: usize,
}

/// The row Alt+C copies from: the focused one, or with no focus the
/// newest open one.
pub(super) fn copy_target(cells: &[Cell], focus: Option<usize>) -> Option<&Cell> {
    match focus {
        Some(index) => cells.get(index),
        None => cells
            .iter()
            .rev()
            .find(|cell| cell.collapsible() && cell.expanded()),
    }
}

/// A job's or agent's calls joined as `full_row_lines` shows them.
fn row_output(from: String, calls: &[FoldedCall]) -> CopiedOutput {
    let mut text = String::new();
    let mut dropped = 0;
    for call in calls.iter().filter(|call| printed(&call.tail.full)) {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&call.name);
        text.push_str(":\n");
        text.push_str(&render::plain_text(call.tail.full.text()));
        dropped += call.tail.full.dropped();
    }
    CopiedOutput {
        from,
        text,
        dropped,
    }
}

/// Whether a call printed anything, kept or since dropped.
fn printed(full: &FullOutput) -> bool {
    !full.text().is_empty() || full.dropped() > 0
}

/// What a job's or agent's header names: `<kind> <name> <id>  <state>`,
/// and the whole error of a failed one.
struct RowHeader<'a> {
    kind: &'static str,
    name: &'a str,
    id: &'a str,
    state: (String, Option<&'a str>),
}

/// A job's or agent's row: one header line behind the fold arrow, in the
/// colours of a tool row's, with its folded calls summarised and clipped
/// by the view rather than wrapped; when open, the whole error and then
/// each folded call's output, indented under the header. The header is
/// the row's own text, so it is scrubbed rather than parsed for colour.
fn row_lines<S>(
    header: RowHeader<'_>,
    row: &TaskRow<S>,
    width: u16,
    styles: &ToolHeaderStyles,
) -> Vec<Line<'static>> {
    let RowHeader {
        kind,
        name,
        id,
        state: (state, error),
    } = header;
    let arrow = if row.expanded {
        EXPANDED_ARROW
    } else {
        COLLAPSED_ARROW
    };
    let reserved = arrow.width() + kind.len() + "   ".len() + state.width();
    let subject = render::plain_text(&fit_to_row(&format!("{name} {id}"), reserved, width));
    let (name, rest) = subject.split_at(subject.find(' ').unwrap_or(subject.len()));
    let state_style = if error.is_some() {
        styles.failed
    } else {
        styles.muted
    };
    let mut spans = vec![
        Span::styled(arrow, styles.muted),
        Span::styled(format!("{kind} "), styles.muted),
        Span::styled(name.to_string(), styles.name),
        Span::styled(format!("{rest}  "), styles.muted),
    ];
    spans.extend(state_spans(&state, state_style, styles.throbber));
    let summary = folded_summary(&row.calls);
    if !summary.is_empty() {
        spans.push(Span::styled(
            format!("  {}", render::plain_text(&summary)),
            styles.muted,
        ));
    }
    let mut lines = vec![Line::from(spans)];
    if row.expanded {
        let mut body = Vec::new();
        if let Some(error) = error {
            body.extend(render::preformatted_lines(&format!("error: {error}")));
        }
        for call in &row.calls {
            body.extend(output_lines(&call.tail.lines));
        }
        let body_width = width.saturating_sub(BODY_INDENT.len() as u16).max(1);
        lines.extend(dim_wrapped(body, body_width).into_iter().map(|mut line| {
            line.spans.insert(0, Span::raw(BODY_INDENT));
            line
        }));
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

/// The greeting's wordmark, "coyote" in figlet's small font. Plain ASCII,
/// so any terminal and any font draws it the same.
const WORDMARK: [&str; 5] = [
    r"                 _       ",
    r" __ ___ _  _ ___| |_ ___ ",
    r"/ _/ _ \ || / _ \  _/ -_)",
    r"\__\___/\_, \___/\__\___|",
    r"        |__/             ",
];

pub(super) const WORDMARK_WIDTH: usize = 25;

/// Columns the bright band of the startup sweep spans.
const SWEEP_BAND: usize = 4;

/// The steps the startup sweep takes: its band enters at the wordmark's
/// left edge and has left it at the last step, where the sweep settles.
pub(super) const SWEEP_STEPS: usize = WORDMARK_WIDTH + SWEEP_BAND;

/// Two border columns and one space of padding on each side.
const WELCOME_FRAME: usize = 4;

/// The widest the box's text runs before it wraps.
const WELCOME_TEXT_WIDTH: usize = 52;

/// The greeting, centred in `width`: the wordmark, then a box with the
/// version and the model, the interrupt hint, and the help and key help
/// hints spelled with the active command prefix. Within `height` the hints
/// give way, the interrupt tip first; the wordmark goes only when the box's
/// first line and borders would not fit under it, or on a frame narrower
/// than it. `sweep` is the step of the startup sweep across the wordmark,
/// None once settled. Only BOLD and DIM mark the box up, so it reads the
/// same under `NO_COLOR`.
pub(super) fn welcome_lines(
    model: &str,
    prefix: CommandPrefix,
    width: u16,
    height: Option<u16>,
    logo: Style,
    sweep: Option<usize>,
) -> Vec<Line<'static>> {
    let plain = Style::new();
    let help = prefix.render_hints(crate::repl::WELCOME_HELP_HINT);
    // The version and the model share a line where it fits the box, and
    // take one each where it would wrap.
    let version = format!("coyote {}", env!("CARGO_PKG_VERSION"));
    let model_line = format!("model: {model}");
    let joined = format!("{version}  {model_line}");
    let room = usize::from(width)
        .saturating_sub(WELCOME_FRAME)
        .min(WELCOME_TEXT_WIDTH);
    let head: Vec<(String, Style)> = if joined.width() <= room {
        vec![(joined, plain)]
    } else {
        vec![(version, plain), (model_line, plain)]
    };
    // In the order they give way, with no blank rows between: the dim tip
    // reads apart from the plain hints under it. The tip goes as a whole,
    // its later lines reading on from its first.
    let mut hints: Vec<(String, Style)> = crate::repl::WELCOME_INTERRUPT_HINT
        .lines()
        .map(|line| (line.to_string(), DIM))
        .collect();
    let interrupt = hints.len();
    hints.push((help.into_owned(), plain));
    hints.push((
        format!(
            "Empty prompt: \"?\" or \"{}{}\" = keys",
            prefix.display(),
            super::KEYS_COMMAND
        ),
        plain,
    ));
    let text_without = |dropped: usize| {
        let mut text = head.to_vec();
        text.extend(hints[dropped..].iter().cloned());
        text
    };

    let columns = usize::from(width);
    if columns <= WELCOME_FRAME {
        return text_without(0)
            .into_iter()
            .flat_map(|(line, style)| render::plain_lines(&line, width, style))
            .collect();
    }
    let longest = text_without(0)
        .iter()
        .map(|(line, _)| line.width())
        .max()
        .unwrap_or(0);
    let natural = longest.min(WELCOME_TEXT_WIDTH) + WELCOME_FRAME;
    let box_width = columns.min(natural);
    let fits = |rows: usize| height.is_none_or(|height| rows <= usize::from(height));
    let drops = [0, interrupt, interrupt + 1, hints.len()];
    // The wordmark's last row holds only its descender, which spaces it
    // from the box below.
    let mark_rows = WORDMARK.len();
    let marked = (columns >= WORDMARK_WIDTH)
        .then(|| {
            drops
                .iter()
                .map(|&dropped| boxed(text_without(dropped), box_width))
                .find(|rows| fits(rows.len() + mark_rows))
        })
        .flatten();
    let with_mark = marked.is_some();
    let rows = marked.unwrap_or_else(|| {
        drops[..3]
            .iter()
            .map(|&dropped| boxed(text_without(dropped), box_width))
            .find(|rows| fits(rows.len()))
            .unwrap_or_else(|| boxed(text_without(hints.len()), box_width))
    });

    let mut lines = Vec::with_capacity(rows.len() + mark_rows);
    if with_mark {
        let margin = " ".repeat((columns - WORDMARK_WIDTH) / 2);
        lines.extend(
            WORDMARK
                .iter()
                .map(|row| wordmark_line(&margin, row, logo, sweep)),
        );
    }
    let margin = " ".repeat((columns - box_width) / 2);
    lines.extend(rows.into_iter().map(|row| {
        let mut spans = vec![Span::raw(margin.clone())];
        spans.extend(row.spans);
        Line::from(spans)
    }));
    lines
}

/// One row of the wordmark after `margin`. Settled, it is drawn bold in
/// the logo's style. Mid-sweep, the columns the band has passed are
/// settled, the band itself is bright, and those it has yet to reach are
/// dim, so the word is revealed left to right.
fn wordmark_line(margin: &str, row: &str, logo: Style, sweep: Option<usize>) -> Line<'static> {
    let settled = logo.add_modifier(Modifier::BOLD);
    let Some(step) = sweep else {
        return Line::from(vec![
            Span::raw(margin.to_string()),
            Span::styled(row.to_string(), settled),
        ]);
    };
    let band = Style::new()
        .fg(SWEEP_HIGHLIGHT)
        .add_modifier(Modifier::BOLD);
    let ahead = logo.add_modifier(Modifier::DIM);
    let style_at = |column: usize| {
        if column + SWEEP_BAND < step {
            settled
        } else if column < step {
            band
        } else {
            ahead
        }
    };
    let mut spans = vec![Span::raw(margin.to_string())];
    let mut start = 0;
    for column in 1..=row.len() {
        if column == row.len() || style_at(column) != style_at(start) {
            spans.push(Span::styled(
                row[start..column].to_string(),
                style_at(start),
            ));
            start = column;
        }
    }
    Line::from(spans)
}

/// `text` wrapped inside a box `box_width` columns wide, border included;
/// `box_width` is more than `WELCOME_FRAME`.
fn boxed(text: Vec<(String, Style)>, box_width: usize) -> Vec<Line<'static>> {
    let inner = box_width - WELCOME_FRAME;
    let inner_width = u16::try_from(inner).unwrap_or(u16::MAX);
    let edge = |left: &str, fill: &str, right: &str| {
        Line::from(format!("{left}{}{right}", fill.repeat(box_width - 2)))
    };
    let set = border::PLAIN;
    let mut lines = vec![edge(set.top_left, set.horizontal_top, set.top_right)];
    for (line, style) in text {
        for wrapped in render::plain_lines(&line, inner_width, style) {
            let pad = inner.saturating_sub(wrapped.width()) + 1;
            let mut spans = vec![Span::raw(format!("{} ", set.vertical_left))];
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

/// Appends one read of a tool's output to its tail's lines; the whole
/// output is the caller's to push. The read may end mid-line
/// (`open_line`), in which case the next read continues it, or
/// mid-character (`pending`). The lines kept are bounded in count and in
/// bytes, oldest dropped first. Returns the text the read decoded to.
pub(super) fn append_output(tail: &mut OutputTail, bytes: &[u8]) -> String {
    let OutputTail {
        lines,
        open_line,
        pending,
        ..
    } = tail;
    let text = push_utf8_chunk(pending, bytes);
    if text.is_empty() {
        return text;
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
    text
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

/// What the next part of a reply cut off after `source` starts with so
/// it reads on as the same reply: the reasoning tag left open, else the
/// line of the code fence left open, else nothing.
pub(super) fn reply_continuation(source: &str) -> String {
    let body = source.trim_start();
    if body.starts_with(THINK_OPEN) && !body.contains(THINK_CLOSE) {
        return THINK_OPEN.to_string();
    }
    render::open_fence_line(source)
        .unwrap_or_default()
        .to_string()
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

    /// A running row as a `Call name args` narration opens it.
    fn narrated(call_id: &str, name: &str, args: &str, since: Instant) -> Cell {
        let summary = format!("Call {name} {args}");
        Cell::tool(call_id.to_string(), name, args, summary, since)
    }

    /// A row read back from a session, as the pager names it.
    fn recorded(call_id: &str, name: &str, args: &str, output: &str) -> Cell {
        let summary = format!("{name} {args}");
        Cell::recorded_tool(call_id.to_string(), name, args, summary, output)
    }

    /// Past the cap the oldest bytes go, on a char boundary, and the
    /// first line says how many.
    #[test]
    fn the_full_output_drops_its_oldest_bytes_past_the_cap_and_says_so() {
        let mut full = FullOutput::default();
        full.push("\u{e9}");
        full.push(&"x".repeat(FULL_OUTPUT_LIMIT));
        full.push("last line\n");
        assert!(full.text().len() <= FULL_OUTPUT_LIMIT);
        assert!(full.text().ends_with("last line\n"));
        assert_eq!(
            full.dropped() + full.text().len(),
            "\u{e9}".len() + FULL_OUTPUT_LIMIT + "last line\n".len()
        );
        let first = full.lines(80)[0].to_string();
        assert_eq!(
            first,
            format!("[... {} earlier bytes not kept]", full.dropped())
        );

        let mut small = FullOutput::default();
        small.push("a\nb\n");
        assert_eq!(small.dropped(), 0);
        assert_eq!(small.lines(80)[0].to_string(), "a");
    }

    /// A row gives up its calls' whole output oldest first, only as much
    /// as was asked for, and each call given up says how much went.
    #[test]
    fn a_row_gives_up_its_oldest_calls_whole_output_first() {
        let mut calls: Vec<FoldedCall> = (1..=3)
            .map(|n| FoldedCall::new(format!("c{n}"), "job__check".to_string()))
            .collect();
        for (n, call) in calls.iter_mut().enumerate() {
            call.tail.full.push(&format!("call {} out\n", n + 1));
        }
        let mut cell = Cell::Job {
            job_id: "job_0a1b2c3d".to_string(),
            tool: "execute_command".to_string(),
            row: TaskRow {
                calls,
                ..TaskRow::new(JobStatus::Running {
                    since: Instant::now(),
                })
            },
            cache: LineCache::default(),
        };
        let each = "call 1 out\n".len();
        assert_eq!(cell.full_output_bytes(), 3 * each);
        assert_eq!(cell.evict_full_outputs(each + 1), 2 * each);
        assert_eq!(cell.full_output_bytes(), each);
        let mut fulls = cell.full_outputs();
        assert_eq!(fulls[2].text(), "call 3 out\n");
        let note = fulls[0].lines(80)[0].to_string();
        assert_eq!(note, format!("[... {each} earlier bytes not kept]"));
    }

    /// Text pushed in pieces lays out to the same lines as the whole text
    /// laid out at once, however the pieces cut its lines, blank lines
    /// and colours; the lines before the last whole one are not redone.
    #[test]
    fn the_full_output_appends_new_lines_to_what_it_laid_out() {
        let pieces = [
            "one\n",
            "\n",
            "\n",
            "two",
            " still two\n",
            "\x1b[31mred\n",
            "still red\x1b[0m\n",
            "crlf\r\n",
            "after\n\n",
            "x".repeat(200).as_str(),
            "\n",
            "last",
        ]
        .map(str::to_string);
        let mut full = FullOutput::default();
        for piece in &pieces {
            full.push(piece);
            let incremental = full.lines(40).to_vec();
            let whole = dim_wrapped(render::preformatted_lines(full.text()), 40);
            assert_eq!(incremental, whole, "after {piece:?}");
        }
        let committed = full.wrapped.as_ref().unwrap().committed;
        assert!(
            full.text()[..committed].ends_with('\n') && committed > "one\n".len(),
            "nothing was kept laid out: {committed}"
        );
        let freed = full.evict();
        assert_eq!(freed, pieces.concat().replace("\r\n", "\n").len());
        assert_eq!(full.lines(40).len(), 1, "the note alone");
    }

    /// A `\r\n` is kept as `\n` whether it sits inside one push or is cut
    /// between two; a `\r` not followed by `\n` is kept as it is.
    #[test]
    fn the_full_output_keeps_crlf_as_lf_across_pushes() {
        let mut full = FullOutput::default();
        full.push("one\r\ntwo\r\n");
        assert_eq!(full.text(), "one\ntwo\n");

        let mut split = FullOutput::default();
        assert_eq!(split.push("one\r"), 3);
        assert_eq!(split.text(), "one");
        split.push("\ntwo\n");
        assert_eq!(split.text(), "one\ntwo\n");
        assert_eq!(split.settle(), 0);

        let mut settled = FullOutput::default();
        settled.push("50%\r");
        assert_eq!(settled.settle(), 1);
        assert_eq!(settled.text(), "50%\r");
        assert_eq!(settled.settle(), 0);

        let mut overwritten = FullOutput::default();
        overwritten.push("50%\r");
        overwritten.push("99%\n");
        assert_eq!(overwritten.text(), "50%\r99%\n");
    }

    /// Any run of `\r` before a `\n` folds to the `\n`, also when the run
    /// is spread over several pushes; a run before other text is kept.
    #[test]
    fn the_full_output_folds_a_run_of_cr_before_lf_across_pushes() {
        let mut full = FullOutput::default();
        full.push("one\r\r\ntwo\r\r\r\n");
        assert_eq!(full.text(), "one\ntwo\n");

        let mut split = FullOutput::default();
        assert_eq!(split.push("one\r"), 3);
        assert_eq!(split.push("\r"), 0);
        split.push("\r\ntwo\n");
        assert_eq!(split.text(), "one\ntwo\n");
        assert_eq!(split.settle(), 0);

        let mut kept = FullOutput::default();
        kept.push("a\r");
        kept.push("\rb\n");
        assert_eq!(kept.text(), "a\r\rb\n");

        let mut settled = FullOutput::default();
        settled.push("50%\r\r");
        assert_eq!(settled.settle(), 2);
        assert_eq!(settled.text(), "50%\r\r");
    }

    /// The whole output's lines are laid out once per width and change;
    /// a push lays them out again at the next read, not at the push.
    #[test]
    fn the_full_output_lays_out_again_only_after_it_changed() {
        let mut full = FullOutput::default();
        full.push("a\n");
        let first = full.lines(80).as_ptr();
        assert_eq!(full.lines(80).as_ptr(), first);
        full.push("b\n");
        assert_eq!(full.lines(80).len(), 2);
        full.release_cache();
        assert_eq!(full.lines(80).len(), 2);
    }

    /// Same width: the cached lines come back without a render. A new
    /// width renders once and is kept beside the previous one; only a
    /// third width evicts the oldest.
    #[test]
    fn the_line_cache_rerenders_on_a_width_change_only() {
        let renders = Counter::new(0);
        let mut cache = LineCache::default();
        let mut get = |width: u16| {
            cache
                .get(width, || {
                    renders.set(renders.get() + 1);
                    vec![Line::from(format!("w{width}"))]
                })
                .to_vec()
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
            let lines = welcome_lines(
                "openai:test-model",
                CommandPrefix::Dot,
                width,
                None,
                DIM,
                None,
            );
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
                assert!(
                    text.contains("Empty prompt: \"?\" or \".keys\" = keys"),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn welcome_lines_spell_the_help_hint_with_the_active_prefix() {
        let lines = welcome_lines("m", CommandPrefix::Slash, 80, None, DIM, None);
        let text: Vec<String> = lines.iter().map(Line::to_string).collect();
        assert!(
            text.iter().any(|line| line.contains("\"/help\"")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|line| line.contains("\"/keys\"")),
            "{text:?}"
        );
    }

    #[test]
    fn welcome_lines_give_up_the_hints_before_the_wordmark() {
        let text = |height: u16| -> Vec<String> {
            welcome_lines("m", CommandPrefix::Dot, 40, Some(height), DIM, None)
                .iter()
                .map(Line::to_string)
                .collect()
        };
        let full = text(u16::MAX);
        assert!(full.iter().any(|line| line.contains("Tip:")), "{full:?}");
        assert!(full[1].contains(WORDMARK[1]), "{full:?}");
        for height in [9, 8, 7, 5, 3] {
            let lines = text(height);
            assert!(lines.len() <= usize::from(height), "{height}: {lines:?}");
            let top = lines
                .iter()
                .position(|line| line.contains(border::PLAIN.top_left))
                .unwrap_or_else(|| panic!("{height}: no box in {lines:?}"));
            assert!(lines[top + 1].contains("coyote "), "{height}: {lines:?}");
            assert!(lines[top + 1].contains("model: m"), "{height}: {lines:?}");
            let bottom = lines.last().unwrap();
            assert!(bottom.contains(border::PLAIN.bottom_left), "{lines:?}");
            assert!(!lines.iter().any(|line| line.contains("Tip:")), "{lines:?}");
        }
        // The hints go before the wordmark: it stays while the box keeps
        // its first line, and goes only when even that would not fit.
        assert!(text(8).iter().any(|line| line.contains(WORDMARK[1])));
        assert!(!text(7).iter().any(|line| line.contains(WORDMARK[1])));
        assert!(text(7).iter().any(|line| line.contains("Empty prompt")));
    }

    /// The box under the wordmark is slim: at 80 columns, with every hint,
    /// it is one row for the version and the model, four for the tip, two
    /// for the hints and its borders, with no blank rows; the whole card
    /// is the wordmark and that box.
    #[test]
    fn the_welcome_box_holds_no_blank_rows() {
        let lines = welcome_lines("m", CommandPrefix::Dot, 80, None, DIM, None);
        assert_eq!(lines.len(), WORDMARK.len() + 9, "{lines:#?}");
        let inside: Vec<String> = lines[WORDMARK.len() + 1..lines.len() - 1]
            .iter()
            .map(|line| {
                line.to_string()
                    .trim()
                    .trim_matches('\u{2502}')
                    .trim()
                    .to_string()
            })
            .collect();
        assert!(inside.iter().all(|row| !row.is_empty()), "{inside:#?}");
        assert!(inside[0].contains("coyote ") && inside[0].contains("model: m"));
    }

    #[test]
    fn the_wordmark_sits_above_the_box_where_it_fits() {
        let has_mark = |width: u16, height: Option<u16>| {
            welcome_lines("m", CommandPrefix::Dot, width, height, DIM, None)
                .iter()
                .any(|line| line.to_string().contains(WORDMARK[2]))
        };
        assert!(has_mark(80, None));
        assert!(has_mark(40, Some(16)));
        assert!(has_mark(25, None));
        assert!(!has_mark(24, None));
        assert!(!has_mark(80, Some(4)));
        assert!(
            WORDMARK
                .iter()
                .all(|row| row.is_ascii() && row.len() == WORDMARK_WIDTH)
        );
        for width in [80, 40, 25] {
            let lines = welcome_lines("m", CommandPrefix::Dot, width, None, DIM, None);
            assert!(
                lines.iter().all(|line| line.width() <= usize::from(width)),
                "{lines:?}"
            );
            let mark = lines
                .iter()
                .find(|line| line.to_string().contains(WORDMARK[2]))
                .unwrap()
                .to_string();
            let left = mark.len() - mark.trim_start().len();
            assert_eq!(
                left,
                (usize::from(width) - WORDMARK_WIDTH) / 2,
                "{width}: not centred"
            );
        }
    }

    /// Mid-sweep the wordmark is revealed left to right: settled behind
    /// the band, the band bright, the rest dim; at no step does the text
    /// itself change, and settled it is one bold span per row.
    #[test]
    fn the_sweep_reveals_the_wordmark_left_to_right() {
        let logo = Style::new().fg(ratatui::style::Color::Yellow);
        let row = WORDMARK[2];
        let style_at = |line: &Line<'static>, column: usize| {
            let mut at = 0;
            for span in &line.spans[1..] {
                at += span.content.len();
                if column < at {
                    return span.style;
                }
            }
            panic!("column {column} is past {line:?}");
        };
        let step = 12;
        let line = wordmark_line("", row, logo, Some(step));
        assert_eq!(line.to_string(), row);
        assert_eq!(style_at(&line, 0), logo.add_modifier(Modifier::BOLD));
        assert_eq!(style_at(&line, step - 1).fg, Some(SWEEP_HIGHLIGHT));
        assert_eq!(style_at(&line, step), logo.add_modifier(Modifier::DIM));
        let first = wordmark_line("", row, logo, Some(0));
        assert_eq!(style_at(&first, 0), logo.add_modifier(Modifier::DIM));
        let last = wordmark_line("", row, logo, Some(SWEEP_STEPS - 1));
        assert_eq!(style_at(&last, 0), logo.add_modifier(Modifier::BOLD));
        assert_eq!(style_at(&last, row.len() - 1).fg, Some(SWEEP_HIGHLIGHT));
        let settled = wordmark_line("  ", row, logo, None);
        assert_eq!(settled.spans.len(), 2);
        assert_eq!(settled.spans[1].style, logo.add_modifier(Modifier::BOLD));
    }

    #[test]
    fn elapsed_text_counts_whole_seconds_up_to_hours() {
        let cases = [
            (Duration::ZERO, "0s"),
            (Duration::from_millis(999), "0s"),
            (Duration::from_secs(42), "42s"),
            (Duration::from_millis(59_900), "59s"),
            (Duration::from_secs(60), "1m 0s"),
            (Duration::from_secs(63), "1m 3s"),
            (Duration::from_secs(3599), "59m 59s"),
            (Duration::from_secs(3600), "1h 0m 0s"),
            (Duration::from_secs(3607), "1h 0m 7s"),
            (Duration::from_secs(9365), "2h 36m 5s"),
            (Duration::from_secs(26 * 3600), "26h 0m 0s"),
        ];
        for (elapsed, text) in cases {
            assert_eq!(elapsed_text(elapsed), text, "{elapsed:?}");
        }
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

    /// The header takes the name and arguments it is given, drawn plain,
    /// whatever styling the summary line carries.
    #[test]
    fn a_tool_row_names_the_call_it_is_given() {
        let mut renderer = renderer();
        let mut cell = Cell::tool(
            "c1".to_string(),
            "\x1b[1mfs_write\x1b[0m",
            " {\"path\":\"x\"} ",
            "Call \x1b[1mfs_write\x1b[0m {\"path\":\"x\"}".to_string(),
            Instant::now(),
        );
        let header = cell.lines(80, &mut renderer)[0].to_string();
        assert!(header.contains("fs_write({\"path\":\"x\"})"), "{header:?}");
        let Cell::Tool {
            name,
            args,
            narrated_args,
            ..
        } = &cell
        else {
            unreachable!();
        };
        assert_eq!(name, "fs_write");
        assert_eq!(args, "{\"path\":\"x\"}");
        assert_eq!(narrated_args, "{\"path\":\"x\"}");
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
    /// line, running or done; the whole narration waits in the body,
    /// indented under the header, for the row to open.
    #[test]
    fn a_large_argument_keeps_the_header_to_one_line_and_the_narration_in_the_body() {
        let mut renderer = renderer();
        let content = "x".repeat(2048);
        let narration =
            format!("Call fs_write {{\"path\":\"src/x.rs\",\"content\":\"{content}\"}}");
        let args = format!("{{\"path\":\"src/x.rs\",\"content\":\"{content}\"}}");
        let mut cell = narrated("c1", "fs_write", &args, Instant::now());
        let running = format!("{} running 0s", crate::utils::SPINNER_FRAMES[0]);
        let header = "fs_write({\"path\":\"src/x.rs\",\"content\":\"xxxxxxxxxxxxxxx...)";
        let body = |lines: &[Line<'_>]| -> String {
            lines[1..]
                .iter()
                .map(|line| {
                    let text = line.to_string();
                    assert!(text.starts_with(BODY_INDENT), "{text:?} is not indented");
                    assert!(line.width() <= 80, "{line:?} is wider than the view");
                    text[BODY_INDENT.len()..].to_string()
                })
                .collect()
        };

        let lines = cell.lines(80, &mut renderer);
        assert_eq!(lines.len(), 1, "a running row did not start collapsed");
        assert_eq!(
            lines[0].to_string(),
            format!("{COLLAPSED_ARROW}{header}  {running}")
        );

        cell.toggle();
        let lines = cell.lines(80, &mut renderer);
        assert_eq!(
            lines[0].to_string(),
            format!("{EXPANDED_ARROW}{header}  {running}")
        );
        assert_eq!(body(&lines), narration, "the narration is not whole");

        let Cell::Tool { status, .. } = &mut cell else {
            unreachable!();
        };
        *status = ToolStatus::Ok {
            elapsed: Duration::from_millis(1200),
        };
        cell.forget_wrapped_lines();
        let lines = cell.lines(80, &mut renderer);
        assert_eq!(
            lines[0].to_string(),
            format!("{EXPANDED_ARROW}{header}  1s  ok")
        );
        assert_eq!(body(&lines), narration, "the narration is not whole");

        cell.toggle();
        let lines = cell.lines(80, &mut renderer);
        assert_eq!(lines.len(), 1, "a collapsed row took more than one line");
        assert_eq!(
            lines[0].to_string(),
            format!("{COLLAPSED_ARROW}{header}  1s  ok")
        );
    }

    /// The error a tool reports is its own text; a bidi override or an
    /// escape sequence in it reaches neither the header nor the body.
    #[test]
    fn a_failed_tool_header_drops_format_characters_and_escapes_from_the_error() {
        let mut renderer = renderer();
        let mut cell = narrated("c1", "fs_read", "{\"path\":\"x\"}", Instant::now());
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
            "\u{25B8} fs_read({\"path\":\"x\"})  0s  failed: no such elif red"
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
    /// however long the message, behind the fold arrow with the name in
    /// the call line's style; opening the row shows it whole.
    #[test]
    fn a_long_failure_stays_on_one_collapsed_line_and_opens_whole() {
        let mut renderer = renderer();
        let error = "e".repeat(300);
        let mut rows = [
            Cell::Job {
                job_id: "job-1".to_string(),
                tool: "execute_command".to_string(),
                row: TaskRow {
                    calls: vec![FoldedCall::new("c1".to_string(), "job__check".to_string())],
                    ..TaskRow::new(JobStatus::Failed(error.clone()))
                },
                cache: LineCache::default(),
            },
            Cell::Agent {
                agent_id: "agent-1".to_string(),
                name: "reviewer".to_string(),
                row: TaskRow::new(AgentStatus::Failed(error.clone())),
                cache: LineCache::default(),
            },
        ];
        for cell in &mut rows {
            let lines = cell.lines(40, &mut renderer);
            assert_eq!(lines.len(), 1, "{lines:?}");
            let header = lines[0].to_string();
            assert!(header.contains("failed: eee"), "{header}");
            assert!(header.starts_with(COLLAPSED_ARROW), "{header}");
            let bold: Vec<&str> = lines[0]
                .spans
                .iter()
                .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(bold.len(), 1, "only the name is bold: {bold:?}");
            assert!(
                ["exe", "rev"]
                    .iter()
                    .any(|start| bold[0].starts_with(start)),
                "{bold:?}"
            );
            let failed = lines[0]
                .spans
                .iter()
                .find(|span| span.content.starts_with("failed: "))
                .expect("the state is a span of its own");
            assert!(!failed.style.add_modifier.contains(Modifier::BOLD));
            cell.toggle();
            let lines = cell.lines(40, &mut renderer);
            assert!(lines[0].to_string().starts_with(EXPANDED_ARROW));
            let body: String = lines[1..]
                .iter()
                .map(|line| {
                    let text = line.to_string();
                    assert!(text.starts_with(BODY_INDENT), "{text:?} is not indented");
                    text[BODY_INDENT.len()..].to_string()
                })
                .collect();
            assert_eq!(body, format!("error: {error}"));
            for line in &lines[1..] {
                assert!(line.width() <= 40, "{line:?} is wider than the view");
            }
        }
    }

    /// Under `NO_COLOR` a tool, job or agent header keeps its bold name
    /// and dim rest, running or failed, and loses every colour.
    #[test]
    fn no_color_headers_keep_their_attributes_and_lose_their_colours() {
        let mut renderer = renderer();
        renderer.no_color = true;
        let args = "{\"path\":\"x\"}";
        let running = narrated("c1", "fs_read", args, Instant::now());
        let mut failed = narrated("c2", "fs_read", args, Instant::now());
        if let Cell::Tool { status, .. } = &mut failed {
            *status = ToolStatus::Failed {
                elapsed: Duration::from_millis(100),
                error: "no such file".to_string(),
            };
        }
        let mut cells = [
            running,
            failed,
            Cell::Job {
                job_id: "job-1".to_string(),
                tool: "execute_command".to_string(),
                row: TaskRow::new(JobStatus::Running {
                    since: Instant::now(),
                }),
                cache: LineCache::default(),
            },
            Cell::Agent {
                agent_id: "agent-1".to_string(),
                name: "reviewer".to_string(),
                row: TaskRow::new(AgentStatus::Failed("boom".to_string())),
                cache: LineCache::default(),
            },
        ];
        for cell in &mut cells {
            let lines = cell.lines(80, &mut renderer);
            let header = &lines[0];
            for span in &header.spans {
                assert_eq!(span.style.fg, None, "{header:?}");
            }
            let has = |modifier: Modifier| {
                header
                    .spans
                    .iter()
                    .any(|span| span.style.add_modifier.contains(modifier))
            };
            assert!(has(Modifier::BOLD), "{header:?}");
            assert!(has(Modifier::DIM), "{header:?}");
        }
    }

    /// A running tool, job or agent row shows the throbber's frame at the
    /// renderer's tick, coloured unless `NO_COLOR`, and how long the row
    /// has run; the exit dump's header, with no tick, says `running`.
    #[test]
    fn running_rows_show_the_throbber_and_how_long_they_have_run() {
        let mut renderer = renderer();
        let since = Instant::now();
        renderer.tick = Tick {
            origin: since,
            now: since + Duration::from_millis(12_300),
        };
        let frame = throbber::frame_at(since, renderer.tick.now);
        assert_eq!(frame, crate::utils::SPINNER_FRAMES[3]);
        let mut cells = [
            narrated("c1", "fs_read", "{\"path\":\"x\"}", since),
            Cell::Job {
                job_id: "job-1".to_string(),
                tool: "execute_command".to_string(),
                row: TaskRow::new(JobStatus::Running { since }),
                cache: LineCache::default(),
            },
            Cell::Agent {
                agent_id: "agent-1".to_string(),
                name: "reviewer".to_string(),
                row: TaskRow::new(AgentStatus::Running { since }),
                cache: LineCache::default(),
            },
        ];
        for no_color in [false, true] {
            renderer.no_color = no_color;
            let colour = ToolHeaderStyles::new(renderer.markdown_render(), no_color)
                .throbber
                .fg;
            assert_eq!(colour.is_none(), no_color);
            for cell in &mut cells {
                cell.forget_wrapped_lines();
                let lines = cell.lines(80, &mut renderer);
                let header = lines[0].to_string();
                assert!(
                    header.ends_with(&format!("  {frame} running 12s")),
                    "{header}"
                );
                let glyph = lines[0]
                    .spans
                    .iter()
                    .find(|span| span.content == frame)
                    .expect("the frame is a span of its own");
                assert_eq!(glyph.style.fg, colour, "{header}");
                let dumped = cell.summary_line(None).unwrap();
                assert!(dumped.ends_with("  running"), "{dumped}");
            }
        }
    }

    /// A saved call says neither how long it took nor how it ended, so
    /// its state is `done`, with no time.
    #[test]
    fn a_recorded_call_says_done_with_no_time() {
        assert_eq!(tool_state(&ToolStatus::Recorded, None), "done");
        let mut renderer = renderer();
        let mut cell = recorded("c1", "fs_read", "{\"path\":\"x\"}", "body\n");
        let header = cell.lines(80, &mut renderer)[0].to_string();
        assert_eq!(header, "\u{25B8} fs_read({\"path\":\"x\"})  done");
    }

    /// An open row in a child's thread shows every line its call printed,
    /// past the tail the transcript's row keeps, and no call line that
    /// says again what the header says.
    #[test]
    fn an_open_thread_row_shows_the_whole_output_and_no_repeated_call() {
        let mut renderer = renderer();
        let output: String = (1..=TOOL_OUTPUT_TAIL + 5)
            .map(|n| format!("line {n:02}\n"))
            .collect();
        let mut cell = recorded("c1", "execute_command", "{\"command\":\"seq\"}", &output);
        cell.toggle();
        let lines: Vec<String> = cell
            .thread_lines(80, &mut renderer)
            .iter()
            .map(Line::to_string)
            .collect();
        assert_eq!(
            lines[0],
            "\u{25BE} execute_command({\"command\":\"seq\"})  done"
        );
        assert_eq!(lines[1], format!("{BODY_INDENT}line 01"));
        assert_eq!(lines.len(), 1 + TOOL_OUTPUT_TAIL + 5, "{lines:?}");

        let content = "x".repeat(200);
        let args = format!("{{\"content\":\"{content}\"}}");
        let mut cut = recorded("c2", "fs_write", &args, "");
        cut.toggle();
        let text: String = cut
            .thread_lines(80, &mut renderer)
            .iter()
            .skip(1)
            .map(|line| line.to_string().trim_start().to_string())
            .collect();
        assert!(text.contains(&content), "a cut call lost its arguments");
    }

    /// Each call under an open node row shows its own state as a tool row
    /// does: the throbber and time while it runs, the error once failed.
    #[test]
    fn calls_under_a_node_show_their_state() {
        let mut renderer = renderer();
        renderer.no_color = false;
        let since = Instant::now();
        renderer.tick = Tick {
            origin: since,
            now: since + Duration::from_millis(2_300),
        };
        let frame = throbber::frame_at(since, renderer.tick.now);
        let mut failed = FoldedCall::running("c2".to_string(), "fs_grep".to_string(), None, since);
        failed.status = Some(ToolStatus::Failed {
            elapsed: Duration::from_secs(1),
            error: "denied".to_string(),
        });
        let mut cell = Cell::node("plan".to_string(), "llm".to_string(), since);
        if let Cell::Node { row, .. } = &mut cell {
            row.calls = vec![
                FoldedCall::running("c1".to_string(), "fs_read".to_string(), None, since),
                failed,
            ];
        }
        cell.toggle();
        let lines = cell.thread_lines(80, &mut renderer);
        let text: Vec<String> = lines.iter().map(Line::to_string).collect();
        assert_eq!(
            text[1..],
            [
                format!("{BODY_INDENT}fs_read  {frame} running 2s"),
                format!("{BODY_INDENT}fs_grep  1s  failed: denied"),
            ]
        );
        let styles = ToolHeaderStyles::new(renderer.markdown_render(), false);
        let glyph = lines[1]
            .spans
            .iter()
            .find(|span| span.content == frame)
            .expect("the frame is a span of its own");
        assert_eq!(glyph.style, styles.throbber);
        let failure = lines[2]
            .spans
            .iter()
            .find(|span| span.content.contains("failed"))
            .expect("the state is a span of its own");
        assert_eq!(failure.style, styles.failed);
    }

    /// A bookkeeping row has no body, so toggling it leaves it closed
    /// and one line tall.
    #[test]
    fn toggling_a_bookkeeping_row_does_nothing() {
        let mut renderer = renderer();
        let mut cell = narrated("c1", "memory__search", "{\"query\":\"x\"}", Instant::now());
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

    /// The pager is the whole log, so a bookkeeping row's output shows
    /// there even though the inline row never opens.
    #[test]
    fn a_bookkeeping_rows_output_shows_in_the_pager_only() {
        let mut renderer = renderer();
        let mut cell = narrated("c1", "memory__search", "{\"query\":\"x\"}", Instant::now());
        let tail = cell.output_for("c1").unwrap();
        let text = append_output(tail, b"remembered fact\n");
        tail.full.push(&text);
        let inline: String = cell
            .lines(80, &mut renderer)
            .iter()
            .map(Line::to_string)
            .collect();
        assert!(!inline.contains("remembered fact"), "{inline}");
        let paged: String = cell
            .full_lines(80, &mut renderer)
            .iter()
            .map(Line::to_string)
            .collect();
        assert!(paged.contains("memory__search"), "{paged}");
        assert!(paged.contains("remembered fact"), "{paged}");
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
        let mut tail = OutputTail::default();
        append_output(&mut tail, b"caf\xC3");
        assert_eq!(tail.lines, ["caf"]);
        assert_eq!(tail.pending, [0xC3]);
        append_output(&mut tail, b"\xA9\n");
        assert_eq!(tail.lines, ["caf\u{e9}"]);
        assert!(tail.pending.is_empty());
        assert!(!tail.open_line);

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
        let mut tail = OutputTail::default();
        append_output(&mut tail, b"half");
        assert!(tail.open_line);
        append_output(&mut tail, b"\xF0\x9F");
        assert!(tail.open_line);
        assert_eq!(tail.lines, ["half"]);
        append_output(&mut tail, b"\x98\x80\n");
        assert_eq!(tail.lines, ["half\u{1F600}"]);
        assert!(!tail.open_line);
    }

    #[test]
    fn output_keeps_only_the_newest_tail_of_lines_in_order() {
        let mut tail = OutputTail::default();
        for n in 0..25u8 {
            let line = format!("{}\n", char::from(b'a' + n));
            append_output(&mut tail, line.as_bytes());
        }
        assert_eq!(tail.lines.len(), TOOL_OUTPUT_TAIL);
        let kept: String = tail.lines.concat();
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
