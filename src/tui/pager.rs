//! The pager: Ctrl+T's full-screen read of one thread. The root thread is
//! the transcript with every tool, job and agent row open on everything
//! its calls printed; a child agent's thread is its saved session, read
//! again each time the child writes it, merged with the tool and node
//! rows the child's hooks report as they happen, shown read-only. A find
//! field takes the key legend's row, and the threads pop-up can be opened
//! over the text.

use super::cells::{Cell, without_think_spans};
use super::child_live::{ChildLive, ChildThread, call_summary};
use super::keys::PagerMode;
use super::render::{self, Renderer};
use super::selection::{self, Pos};
use super::theme::Theme;
use super::{ChromeKind, DIM, chrome_style};
use crate::client::{Message, MessageContent, MessageContentPart, MessageRole};
use crate::config::{RequestContext, Session};
use anyhow::{Context, Result, bail};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

const HINT: &str = "j/k scroll  g/G top/end  / find  n/N next/prev  Tab threads  q close";
/// The legend under `NARROW` columns, where `HINT` would lose its end:
/// the way out and the way to the other threads come first.
const SHORT_HINT: &str = "q close  Tab threads  / find  n/N  j/k  g/G";
const NARROW: u16 = 70;
const CHECKPOINT_NOTE: &str =
    "Read-only. Tool calls show live; replies show up at the child's next checkpoint.";
const ROOT_THREAD: &str = "root thread";
pub(super) const COMPRESSED_DIVIDER: &str = "(earlier rounds, compressed)";
/// The largest child session file the pager reads; a bigger one is
/// reported rather than parsed.
const CHILD_FILE_LIMIT: u64 = 16 * 1024 * 1024;

const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const REVERSED: Style = Style::new().add_modifier(Modifier::REVERSED);

/// One message of a child's session as the pager shows it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ChildEntry {
    /// Heads the rounds the child's session compressed away.
    Compressed,
    User(String),
    Assistant(String),
    /// A call and what it returned. `call_id` is empty for a result the
    /// session keeps without its call.
    Tool {
        call_id: String,
        name: String,
        args: String,
        output: String,
    },
}

/// The entries of a child's session, system prompt left out: each tool
/// round's narration, then every call of the round with its result.
fn entries(messages: &[Message]) -> Vec<ChildEntry> {
    let mut entries = Vec::new();
    for message in messages {
        let text = match &message.content {
            MessageContent::ToolCalls(calls) => {
                if !calls.text.trim().is_empty() {
                    entries.push(ChildEntry::Assistant(calls.text.clone()));
                }
                for result in &calls.tool_results {
                    // A later round's narration rides on its first result.
                    if let Some(text) = result.text.as_deref().filter(|t| !t.trim().is_empty()) {
                        entries.push(ChildEntry::Assistant(text.to_string()));
                    }
                    entries.push(ChildEntry::Tool {
                        call_id: result.call.id.clone().unwrap_or_default(),
                        name: result.call.name.clone(),
                        args: result.call.arguments.to_string(),
                        output: match &result.output {
                            Value::String(text) => text.clone(),
                            other => serde_json::to_string_pretty(other)
                                .unwrap_or_else(|_| other.to_string()),
                        },
                    });
                }
                continue;
            }
            MessageContent::Text(text) => text.clone(),
            MessageContent::Array(parts) => parts
                .iter()
                .map(|part| match part {
                    MessageContentPart::Text { text } => text.as_str(),
                    MessageContentPart::ImageUrl { .. } => "[image]",
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        match message.role {
            MessageRole::System => {}
            MessageRole::User => entries.push(ChildEntry::User(text)),
            MessageRole::Assistant => entries.push(ChildEntry::Assistant(text)),
            MessageRole::Tool => entries.push(ChildEntry::Tool {
                call_id: String::new(),
                name: "tool".to_string(),
                args: String::new(),
                output: text,
            }),
        }
    }
    entries
}

/// Reads a child's session file. Runs off the UI loop: the file is
/// parsed whole, and a running child's grows with every round.
pub(super) fn load_child(file: &Path, ctx: &RequestContext) -> Result<Vec<ChildEntry>> {
    let size = std::fs::metadata(file)
        .with_context(|| format!("Failed to read {}", file.display()))?
        .len();
    if size > CHILD_FILE_LIMIT {
        bail!(
            "{} holds {size} bytes, more than the {CHILD_FILE_LIMIT} the transcript view reads",
            file.display()
        );
    }
    let session = Session::load_from_path(file, ctx.model.clone(), ctx, &ctx.app.config)?;
    Ok(session_entries(
        session.compressed_messages(),
        session.messages(),
    ))
}

/// The entries of a session's compressed rounds, headed by their divider,
/// then those of its live messages.
pub(super) fn session_entries(compressed: &[Message], active: &[Message]) -> Vec<ChildEntry> {
    let mut shown = entries(compressed);
    if !shown.is_empty() {
        shown.insert(0, ChildEntry::Compressed);
    }
    shown.extend(entries(active));
    shown
}

/// The lines of a text entry. A tool call is drawn from its row instead,
/// so it has none here.
fn entry_lines(entry: &ChildEntry, width: u16, renderer: &mut Renderer) -> Vec<Line<'static>> {
    match entry {
        ChildEntry::Compressed => render::plain_lines(COMPRESSED_DIVIDER, width, DIM),
        ChildEntry::User(text) => render::plain_lines(&format!("> {text}"), width, BOLD),
        ChildEntry::Assistant(text) => renderer.markdown(without_think_spans(text, true), width),
        ChildEntry::Tool { .. } => Vec::new(),
    }
}

/// A saved call's row text, as a live row of the same call has it.
pub(super) fn entry_summary(name: &str, args: &str) -> String {
    format!("{name} {args}")
}

/// A saved call as a finished tool row of the main view's, for a call
/// no live row reports; None for a text entry.
fn recorded_row(entry: &ChildEntry) -> Option<Cell> {
    let ChildEntry::Tool {
        call_id,
        name,
        args,
        output,
    } = entry
    else {
        return None;
    };
    Some(Cell::recorded_tool(
        call_id.clone(),
        name,
        args,
        entry_summary(name, args),
        output,
    ))
}

/// Where a row of a child's thread is drawn from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildRow {
    /// Entry `n` of the saved session; a call there is its recorded row.
    Disk(usize),
    /// Row `n` of the child's live thread.
    Live(usize),
}

/// The rows of a child's thread: the saved session in order, each call
/// in it drawn as its live row when it has one and left out when a node
/// row holds it, then every live row the session does not name yet. A
/// call with no id (some providers give none) is its live call when one
/// has its name and arguments, top-level or under a node: the first such
/// saved call pairs with the first such live call, and so on.
fn merged_rows(entries: &[ChildEntry], live: &[Cell]) -> Vec<ChildRow> {
    let mut live_tools: HashMap<&str, usize> = HashMap::new();
    // Each id-less live call by its row text: the row drawing it, or None
    // for a call folded under a node.
    let mut unnamed: HashMap<String, VecDeque<Option<usize>>> = HashMap::new();
    for (index, cell) in live.iter().enumerate() {
        match cell {
            Cell::Tool { call_id, .. } if !call_id.is_empty() => {
                live_tools.insert(call_id, index);
            }
            Cell::Tool { summary, .. } => unnamed
                .entry(summary.clone())
                .or_default()
                .push_back(Some(index)),
            Cell::Node { row, .. } => {
                for call in row.calls.iter().filter(|call| call.call_id.is_empty()) {
                    unnamed
                        .entry(call_summary(&call.name, call.args.as_ref()))
                        .or_default()
                        .push_back(None);
                }
            }
            _ => {}
        }
    }
    let in_node = |call_id: &str| {
        live.iter()
            .any(|cell| matches!(cell, Cell::Node { .. }) && cell.takes_output_of(call_id))
    };
    let mut shown = vec![false; live.len()];
    let mut rows = Vec::with_capacity(entries.len() + live.len());
    for (index, entry) in entries.iter().enumerate() {
        match entry {
            ChildEntry::Tool { call_id, .. } if !call_id.is_empty() => {
                match live_tools.get(call_id.as_str()) {
                    Some(&at) if !shown[at] => {
                        shown[at] = true;
                        rows.push(ChildRow::Live(at));
                    }
                    Some(_) => {}
                    None if in_node(call_id) => {}
                    None => rows.push(ChildRow::Disk(index)),
                }
            }
            ChildEntry::Tool { name, args, .. } => {
                let paired = unnamed
                    .get_mut(entry_summary(name, args).as_str())
                    .and_then(VecDeque::pop_front);
                match paired {
                    Some(Some(at)) => {
                        shown[at] = true;
                        rows.push(ChildRow::Live(at));
                    }
                    Some(None) => {}
                    None => rows.push(ChildRow::Disk(index)),
                }
            }
            _ => rows.push(ChildRow::Disk(index)),
        }
    }
    rows.extend((0..live.len()).filter(|at| !shown[*at]).map(ChildRow::Live));
    rows
}

/// What the focus in a child's thread holds on to, so it stays on its row
/// when rows above it come or go.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChildRowKey {
    Call(String),
    /// A call with no id, by its row text (its name and arguments) and
    /// how many calls with the same text come before it.
    CallWithoutId {
        summary: String,
        nth: usize,
    },
    /// A node's row, by node id and how many earlier rows the node has: a
    /// graph that loops back to a node gives it a row per visit.
    Node {
        node_id: String,
        visit: usize,
    },
}

/// The area the pager's text is drawn in: all of `area` but the title
/// and hint rows, when it has room for them.
pub(super) fn text_area(area: Rect) -> Rect {
    if area.height >= 3 {
        Rect::new(area.x, area.y + 1, area.width, area.height - 2)
    } else {
        area
    }
}

/// One child entry's lines, with what they were laid out from.
struct EntryLayout {
    hash: u64,
    width: u16,
    lines: Vec<Line<'static>>,
}

fn entry_hash(entry: &ChildEntry) -> u64 {
    let mut hasher = DefaultHasher::new();
    entry.hash(&mut hasher);
    hasher.finish()
}

pub(super) struct ChildView {
    session_id: String,
    agent: String,
    /// The names of the threads above this one, the main thread first,
    /// which the title shows as a path.
    crumbs: Vec<String>,
    file: PathBuf,
    /// None until the first read comes back. A failed read keeps what the
    /// last good one gave and says why next to it.
    entries: Option<Vec<ChildEntry>>,
    /// The recorded row of each saved call, by entry; None for a text
    /// entry. A re-read keeps the row of an unchanged entry, folded or
    /// open as it was.
    recorded: Vec<Option<Cell>>,
    error: Option<String>,
    /// Each text entry's lines by position. A re-read of a growing
    /// thread lays out only the entries that are new or changed.
    layouts: Vec<Option<EntryLayout>>,
    /// The rows as last laid out, which a click is matched against.
    rows: Vec<ChildRow>,
    /// The foldable row Ctrl+Up and Ctrl+Down last moved to.
    focus: Option<ChildRowKey>,
    /// The read under way, by generation. At most one runs at a time.
    loading: Option<u64>,
    /// The child wrote its file again while a read was under way: one
    /// more read follows it.
    stale: bool,
}

impl ChildView {
    fn cell<'a>(&'a self, row: ChildRow, live: &'a [Cell]) -> Option<&'a Cell> {
        match row {
            ChildRow::Disk(index) => self.recorded.get(index)?.as_ref(),
            ChildRow::Live(index) => live.get(index),
        }
    }

    fn cell_mut<'a>(&'a mut self, row: ChildRow, live: &'a mut [Cell]) -> Option<&'a mut Cell> {
        match row {
            ChildRow::Disk(index) => self.recorded.get_mut(index)?.as_mut(),
            ChildRow::Live(index) => live.get_mut(index),
        }
    }

    fn merge(&mut self, live: &[Cell]) {
        self.rows = merged_rows(self.entries.as_deref().unwrap_or_default(), live);
    }

    /// The focus key of each row; None for text.
    fn keys(&self, live: &[Cell]) -> Vec<Option<ChildRowKey>> {
        fn nth<'a>(seen: &mut HashMap<&'a str, usize>, key: &'a str) -> usize {
            let count = seen.entry(key).or_default();
            *count += 1;
            *count - 1
        }
        let mut unnamed = HashMap::new();
        let mut visits = HashMap::new();
        self.rows
            .iter()
            .map(|&row| match self.cell(row, live)? {
                Cell::Tool { call_id, .. } if !call_id.is_empty() => {
                    Some(ChildRowKey::Call(call_id.clone()))
                }
                Cell::Tool { summary, .. } => Some(ChildRowKey::CallWithoutId {
                    summary: summary.clone(),
                    nth: nth(&mut unnamed, summary),
                }),
                Cell::Node { node_id, .. } => Some(ChildRowKey::Node {
                    node_id: node_id.clone(),
                    visit: nth(&mut visits, node_id),
                }),
                _ => None,
            })
            .collect()
    }

    /// Where the focused row is among the rows, if it is still there.
    fn focused_row(&self, live: &[Cell]) -> Option<usize> {
        let focus = self.focus.as_ref()?;
        self.keys(live)
            .iter()
            .position(|key| key.as_ref() == Some(focus))
    }

    /// Takes a good read, keeping the recorded row of each call that is
    /// where it was with what it had.
    fn set_entries(&mut self, entries: Vec<ChildEntry>) {
        let previous = self.entries.take().unwrap_or_default();
        let mut kept = std::mem::take(&mut self.recorded);
        self.recorded = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let unchanged = previous.get(index) == Some(entry);
                match kept.get_mut(index).and_then(Option::take) {
                    Some(cell) if unchanged => Some(cell),
                    _ => recorded_row(entry),
                }
            })
            .collect();
        self.entries = Some(entries);
    }
}

fn live_rows<'a>(live: &'a ChildLive, session_id: &str) -> &'a [Cell] {
    live.by_session(session_id).map_or(&[], ChildThread::rows)
}

enum Thread {
    Root,
    Child(Box<ChildView>),
}

/// Where the view lands at the next frame, once the lines it is made of
/// are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Jump {
    Top,
    Bottom,
    Cell(usize),
    Match {
        forward: bool,
    },
    /// Scroll just far enough to show the first line of row `n`.
    Reveal(usize),
}

/// A run of the text and where it starts: the lines of one root cell
/// (shared with the layout cache) or of one row of a child's thread, or
/// the note heading a child's thread.
struct Piece {
    cell: Option<usize>,
    start: usize,
    lines: Arc<[Line<'static>]>,
}

/// The text of a thread laid out at one width. Root cells are kept as
/// their own runs with a blank line between, so a frame after one cell
/// changed shares every other cell's lines rather than copying them.
pub(super) struct PagerText {
    pieces: Vec<Piece>,
    total: usize,
}

impl PagerText {
    fn single(lines: Vec<Line<'static>>) -> Self {
        Self {
            total: lines.len(),
            pieces: vec![Piece {
                cell: None,
                start: 0,
                lines: lines.into(),
            }],
        }
    }

    fn len(&self) -> usize {
        self.total
    }

    /// Line `index`; None past the end and for the blank line between
    /// two cells.
    fn line(&self, index: usize) -> Option<&Line<'static>> {
        let at = self
            .pieces
            .partition_point(|piece| piece.start <= index)
            .checked_sub(1)?;
        let piece = &self.pieces[at];
        piece.lines.get(index - piece.start)
    }

    fn cell_start(&self, cell: usize) -> Option<usize> {
        self.pieces
            .iter()
            .find(|piece| piece.cell == Some(cell))
            .map(|piece| piece.start)
    }

    /// The cell or row whose lines hold line `index`.
    fn cell_at(&self, index: usize) -> Option<usize> {
        let at = self
            .pieces
            .partition_point(|piece| piece.start <= index)
            .checked_sub(1)?;
        let piece = &self.pieces[at];
        if index - piece.start < piece.lines.len() {
            piece.cell
        } else {
            None
        }
    }
}

fn holds(line: Option<&Line<'static>>, needle: &str) -> bool {
    line.is_some_and(|line| line.to_string().to_lowercase().contains(needle))
}

/// Each root cell's pager lines at one width, by the cell's revision.
#[derive(Default)]
struct RootLines {
    width: u16,
    by_revision: HashMap<u64, Arc<[Line<'static>]>>,
}

pub(super) struct Pager {
    thread: Thread,
    top: usize,
    jump: Option<Jump>,
    /// Set by a jump to the end, or a scroll that reached it: a thread
    /// that grows keeps its newest line in view until the user moves.
    follow: bool,
    /// The user scrolled since the last frame; whether that reached the
    /// end decides `follow`.
    scrolled: bool,
    /// The find field while it is open.
    field: Option<String>,
    query: String,
    matched: Option<usize>,
    /// The last find found nothing.
    missed: bool,
    /// Text rows in the last frame; what a page is.
    page: usize,
    /// The text as last laid out, with the width and the transcript
    /// generation it was laid out for; reused until either moves.
    laid_out: Option<(u16, u64, Arc<PagerText>)>,
    /// The text the last frame drew, which a click or a selection is
    /// matched against. It outlives `laid_out`: a running row of a child's
    /// thread drops that after every frame so its throbber is drawn anew,
    /// and a press landing before the next frame still has to find its row.
    drawn: Option<Arc<PagerText>>,
    root_lines: RootLines,
    /// Root cells laid out since the pager opened.
    #[cfg(test)]
    cell_layouts: usize,
    /// Child entries laid out since the pager opened.
    #[cfg(test)]
    entry_layouts: usize,
}

impl Pager {
    fn new(thread: Thread, jump: Jump) -> Self {
        Self {
            thread,
            top: 0,
            jump: Some(jump),
            follow: false,
            scrolled: false,
            field: None,
            query: String::new(),
            matched: None,
            missed: false,
            page: 1,
            laid_out: None,
            drawn: None,
            root_lines: RootLines::default(),
            #[cfg(test)]
            cell_layouts: 0,
            #[cfg(test)]
            entry_layouts: 0,
        }
    }

    /// The root thread from its newest line.
    pub(super) fn root() -> Self {
        Self::new(Thread::Root, Jump::Bottom)
    }

    /// Drops every laid-out line so the next frame styles the text anew;
    /// for when the renderer itself changed.
    pub(super) fn forget_layout(&mut self) {
        self.laid_out = None;
        self.drawn = None;
        self.root_lines = RootLines::default();
        if let Thread::Child(view) = &mut self.thread {
            view.layouts.clear();
            view.recorded
                .iter_mut()
                .flatten()
                .for_each(Cell::forget_layout);
        }
    }

    /// The root thread from the row of transcript cell `cell`.
    pub(super) fn root_at(cell: usize) -> Self {
        Self::new(Thread::Root, Jump::Cell(cell))
    }

    /// A child's thread, empty until its first read comes back.
    pub(super) fn child(session_id: String, agent: String, file: PathBuf) -> Self {
        Self::new(
            Thread::Child(Box::new(ChildView {
                session_id,
                agent,
                crumbs: Vec::new(),
                file,
                entries: None,
                recorded: Vec::new(),
                error: None,
                layouts: Vec::new(),
                rows: Vec::new(),
                focus: None,
                loading: None,
                stale: false,
            })),
            Jump::Bottom,
        )
    }

    /// The child's thread with the names of the threads above it, the
    /// main thread first.
    pub(super) fn with_crumbs(mut self, crumbs: Vec<String>) -> Self {
        if let Thread::Child(view) = &mut self.thread {
            view.crumbs = crumbs;
        }
        self
    }

    pub(super) fn mode(&self) -> PagerMode {
        if self.field.is_some() {
            PagerMode::Find
        } else {
            PagerMode::Browse
        }
    }

    pub(super) fn shows_root(&self) -> bool {
        matches!(self.thread, Thread::Root)
    }

    pub(super) fn child_session(&self) -> Option<&str> {
        match &self.thread {
            Thread::Root => None,
            Thread::Child(view) => Some(&view.session_id),
        }
    }

    /// The live rows of the child on show changed: its text is laid out
    /// again at the next frame.
    pub(super) fn child_rows_changed(&mut self) {
        if let Thread::Child(_) = self.thread {
            self.laid_out = None;
        }
    }

    /// Moves the focus to the next foldable row of the child on show, or
    /// the previous one; from no focus, to the first or the last.
    pub(super) fn step_focus(&mut self, live: &ChildLive, forward: bool) {
        let Thread::Child(view) = &mut self.thread else {
            return;
        };
        let cells = live_rows(live, &view.session_id);
        view.merge(cells);
        let foldable: Vec<usize> = (0..view.rows.len())
            .filter(|&index| {
                view.cell(view.rows[index], cells)
                    .is_some_and(Cell::collapsible)
            })
            .collect();
        let at = view.focused_row(cells);
        let target = match (at, forward) {
            (None, true) => foldable.first().copied(),
            (None, false) => foldable.last().copied(),
            (Some(at), true) => foldable
                .iter()
                .copied()
                .find(|&index| index > at)
                .or(Some(at)),
            (Some(at), false) => foldable
                .iter()
                .copied()
                .rev()
                .find(|&index| index < at)
                .or(Some(at)),
        };
        let Some(target) = target else {
            return;
        };
        view.focus = view.keys(cells).swap_remove(target);
        self.jump = Some(Jump::Reveal(target));
        self.laid_out = None;
    }

    /// Opens or closes the focused row of the child on show.
    pub(super) fn toggle_focused(&mut self, live: &mut ChildLive) {
        let Thread::Child(view) = &mut self.thread else {
            return;
        };
        view.merge(live_rows(live, &view.session_id));
        let Some(focus) = view.focused_row(live_rows(live, &view.session_id)) else {
            return;
        };
        let focus = view.rows[focus];
        let cells = live
            .by_session_mut(&view.session_id)
            .map_or(&mut [][..], ChildThread::rows_mut);
        if let Some(cell) = view.cell_mut(focus, cells) {
            cell.toggle();
            self.laid_out = None;
        }
    }

    /// The focused row of the child on show, which Alt+C copies from.
    pub(super) fn focused_child_cell<'a>(&'a self, live: &'a ChildLive) -> Option<&'a Cell> {
        let Thread::Child(view) = &self.thread else {
            return None;
        };
        let cells = live_rows(live, &view.session_id);
        let focus = view.focused_row(cells)?;
        view.cell(view.rows[focus], cells)
    }

    /// A left click at `column`, `row` of a pager drawn over `area`: on a
    /// foldable row of the child on show, focuses it and opens or closes
    /// it.
    pub(super) fn click(&mut self, live: &mut ChildLive, column: u16, row: u16, area: Rect) {
        let body = text_area(area);
        if !body.contains(Position::new(column, row)) {
            return;
        }
        let line = self.top + usize::from(row - body.y);
        let Some(index) = self.drawn.as_ref().and_then(|text| text.cell_at(line)) else {
            return;
        };
        let Thread::Child(view) = &mut self.thread else {
            return;
        };
        let Some(&clicked) = view.rows.get(index) else {
            return;
        };
        let key = view
            .keys(live_rows(live, &view.session_id))
            .swap_remove(index);
        let cells = live
            .by_session_mut(&view.session_id)
            .map_or(&mut [][..], ChildThread::rows_mut);
        if let Some(cell) = view.cell_mut(clicked, cells)
            && cell.collapsible()
        {
            cell.toggle();
            view.focus = key;
            self.laid_out = None;
        }
    }

    fn open_child_view(&mut self, session_id: &str) -> Option<&mut ChildView> {
        match &mut self.thread {
            Thread::Child(view) if view.session_id == session_id => Some(view),
            _ => None,
        }
    }

    /// The child on show wrote its file: the file to read now, or None
    /// when that child is not on show or a read is already under way (it
    /// is then read once more when that read comes back).
    pub(super) fn child_flushed(&mut self, session_id: &str) -> Option<PathBuf> {
        let view = self.open_child_view(session_id)?;
        if view.loading.is_some() {
            view.stale = true;
            return None;
        }
        Some(view.file.clone())
    }

    /// Records that read `generation` of the child on show is under way.
    pub(super) fn child_load_started(&mut self, generation: u64) {
        if let Thread::Child(view) = &mut self.thread {
            view.loading = Some(generation);
            view.stale = false;
        }
    }

    /// Takes a read of the child on show. A read for another child (one
    /// left before its read came back) or an older read is dropped.
    /// Returns the file to read again when the child wrote it while this
    /// read ran.
    pub(super) fn set_child_entries(
        &mut self,
        session_id: &str,
        generation: u64,
        result: Result<Vec<ChildEntry>, String>,
    ) -> Option<PathBuf> {
        let view = self.open_child_view(session_id)?;
        if view.loading != Some(generation) {
            return None;
        }
        view.loading = None;
        match result {
            Ok(entries) => {
                view.set_entries(entries);
                view.error = None;
            }
            Err(error) => view.error = Some(error),
        }
        let again = std::mem::take(&mut view.stale).then(|| view.file.clone());
        self.laid_out = None;
        again
    }

    pub(super) fn scroll(&mut self, delta: i32) {
        self.top = self
            .top
            .saturating_add_signed(isize::try_from(delta).unwrap_or(0));
        self.follow = false;
        self.scrolled = true;
    }

    /// The rows of text in a pager drawn over `area`.
    pub(super) fn body(area: Rect) -> Rect {
        text_area(area)
    }

    /// The line and column of the last frame's text at screen cell
    /// `column`, `row` of a pager drawn over `area`; the space under the
    /// last line counts as the last line's end.
    pub(super) fn pos_at(&self, column: u16, row: u16, area: Rect) -> Option<Pos> {
        let body = text_area(area);
        let text = self.drawn.as_ref()?;
        if text.len() == 0 || !body.contains(Position::new(column, row)) {
            return None;
        }
        let line = self.top + usize::from(row - body.y);
        let column = usize::from(column - body.x);
        Some(if line < text.len() {
            Pos::new(0, line, column)
        } else {
            Pos::new(0, text.len() - 1, usize::MAX)
        })
    }

    /// Lines `first..=last` of the last frame's text as the selection
    /// copies them.
    pub(super) fn selection_rows(&self, first: usize, last: usize) -> Vec<selection::Row> {
        let Some(text) = self.drawn.as_ref() else {
            return Vec::new();
        };
        let empty = Line::default();
        selection::rows(
            (first..=last.min(text.len().saturating_sub(1)))
                .map(|index| (Pos::new(0, index, 0), text.line(index).unwrap_or(&empty))),
        )
    }

    /// The rows of the wrapped line that row `at` of the last frame's
    /// text is part of, so a triple click can take the line whole.
    pub(super) fn line_rows(&self, at: usize) -> Vec<selection::Row> {
        let Some(text) = self.drawn.as_ref() else {
            return Vec::new();
        };
        let wraps = |index: usize| {
            text.line(index)
                .is_some_and(|line| render::wrap_join(line).is_some())
        };
        let mut first = at;
        while first > 0 && wraps(first) {
            first -= 1;
        }
        let mut last = at;
        while wraps(last + 1) {
            last += 1;
        }
        self.selection_rows(first, last)
    }

    /// Paints the lines on show in a pager drawn over `area` that fall
    /// inside `range`.
    pub(super) fn paint_selection(
        &self,
        buf: &mut Buffer,
        area: Rect,
        range: (Pos, Pos),
        style: Style,
    ) {
        let body = text_area(area).intersection(*buf.area());
        for y in body.top()..body.bottom() {
            let key = (0, self.top + usize::from(y - body.y));
            selection::paint_row(buf, body, y, key, range, style);
        }
    }

    pub(super) fn page(&mut self, direction: i32) {
        let rows = i32::try_from(self.page.saturating_sub(1).max(1)).unwrap_or(i32::MAX);
        self.scroll(direction.saturating_mul(rows));
    }

    pub(super) fn jump_to_top(&mut self) {
        self.jump = Some(Jump::Top);
        self.follow = false;
    }

    pub(super) fn jump_to_bottom(&mut self) {
        self.jump = Some(Jump::Bottom);
    }

    pub(super) fn open_find(&mut self) {
        self.field = Some(String::new());
        self.missed = false;
    }

    pub(super) fn find_char(&mut self, c: char) {
        if let Some(field) = self.field.as_mut() {
            field.push(c);
        }
    }

    pub(super) fn find_backspace(&mut self) {
        if let Some(field) = self.field.as_mut() {
            field.pop();
        }
    }

    /// A paste goes into the find field while it is open, on one line,
    /// and nowhere otherwise.
    pub(super) fn paste(&mut self, text: &str) {
        if let Some(field) = self.field.as_mut() {
            field.push_str(&text.replace(['\r', '\n'], " "));
        }
    }

    pub(super) fn cancel_find(&mut self) {
        self.field = None;
        self.query.clear();
        self.matched = None;
        self.missed = false;
    }

    /// Closes the find field and searches for its text, which `n` and `N`
    /// then step through.
    pub(super) fn submit_find(&mut self) {
        let Some(field) = self.field.take() else {
            return;
        };
        if field != self.query {
            self.matched = None;
        }
        self.query = field;
        self.find_next(true);
    }

    /// Jumps to the next match (or the previous one) of the last text
    /// searched for. Over a child's thread the text is laid out again,
    /// since the match can sit in a folded row that the find opens.
    pub(super) fn find_next(&mut self, forward: bool) {
        if self.query.is_empty() {
            self.missed = false;
            return;
        }
        self.jump = Some(Jump::Match { forward });
        self.follow = false;
        if let Thread::Child(_) = self.thread {
            self.laid_out = None;
        }
    }

    /// Rows of text a frame `height` rows tall leaves between the title
    /// and the hint; a frame too short for them is all text.
    pub(super) fn text_rows(height: u16) -> usize {
        usize::from(if height >= 3 { height - 2 } else { height })
    }

    /// The thread laid out at `width`. The root thread is looked at
    /// again only when the transcript's `generation` moved, and then only
    /// the cells whose revision moved are laid out again; a child's
    /// thread is laid out again when a read of it came back, its live
    /// rows in `live` changed or one of its rows was opened or closed.
    pub(super) fn text(
        &mut self,
        cells: &mut [Cell],
        revisions: &[u64],
        generation: u64,
        live: &mut ChildLive,
        width: u16,
        renderer: &mut Renderer,
    ) -> Arc<PagerText> {
        let generation = match self.thread {
            Thread::Root => generation,
            Thread::Child(_) => 0,
        };
        if let Some((laid_width, laid_generation, text)) = &self.laid_out
            && *laid_width == width
            && *laid_generation == generation
        {
            return Arc::clone(text);
        }
        let mut text = self.lay_out(cells, revisions, live, width, renderer);
        if self.open_folded_match(&text, live) {
            text = self.lay_out(cells, revisions, live, width, renderer);
        }
        let text = Arc::new(text);
        self.reanchor(&text);
        self.laid_out = Some((width, generation, Arc::clone(&text)));
        self.drawn = Some(Arc::clone(&text));
        text
    }

    /// With a find under way over a child's thread, opens the folded row
    /// whose output holds the next match when that comes before the next
    /// match already on show, so the find lands inside it. Returns whether
    /// a row was opened.
    fn open_folded_match(&mut self, text: &PagerText, live: &mut ChildLive) -> bool {
        let Some(Jump::Match { forward }) = self.jump else {
            return false;
        };
        let total = text.len();
        if total == 0 || self.query.is_empty() {
            return false;
        }
        let origin = self.find_origin(total, forward) % total;
        let distance = |line: usize| {
            if forward {
                (line + total - origin) % total
            } else {
                (origin + total - line) % total
            }
        };
        let shown = self.find(text, forward).map(distance);
        let needle = self.query.to_lowercase();
        let Thread::Child(view) = &mut self.thread else {
            return false;
        };
        let cells = live_rows(live, &view.session_id);
        let folded = view
            .rows
            .iter()
            .enumerate()
            .filter(|&(_, &row)| {
                view.cell(row, cells).is_some_and(|cell| {
                    cell.collapsible()
                        && !cell.expanded()
                        && cell
                            .copied_output()
                            .is_some_and(|output| output.text.to_lowercase().contains(&needle))
                })
            })
            .filter_map(|(index, &row)| Some((distance(text.cell_start(index)?), row)))
            .filter(|&(away, _)| shown.is_none_or(|shown| away < shown))
            .min_by_key(|&(away, _)| away);
        let Some((_, row)) = folded else {
            return false;
        };
        let cells = live
            .by_session_mut(&view.session_id)
            .map_or(&mut [][..], ChildThread::rows_mut);
        match view.cell_mut(row, cells) {
            Some(cell) => {
                cell.toggle();
                true
            }
            None => false,
        }
    }

    /// A highlighted match the new text no longer has on its line is
    /// looked for again from the top of the view.
    fn reanchor(&mut self, text: &PagerText) {
        let Some(line) = self.matched else {
            return;
        };
        if holds(text.line(line), &self.query.to_lowercase()) {
            return;
        }
        self.matched = None;
        if self.jump.is_none() {
            self.jump = Some(Jump::Match { forward: true });
        }
    }

    fn lay_out(
        &mut self,
        cells: &mut [Cell],
        revisions: &[u64],
        live: &mut ChildLive,
        width: u16,
        renderer: &mut Renderer,
    ) -> PagerText {
        match &mut self.thread {
            Thread::Root => self.lay_out_root(cells, revisions, width, renderer),
            Thread::Child(view) => {
                let mut head = Vec::new();
                let no_color = renderer.no_color;
                head.extend(render::plain_lines(CHECKPOINT_NOTE, width, DIM));
                if let Some(error) = &view.error {
                    head.extend(render::plain_lines(
                        &format!("Cannot read this transcript: {error}"),
                        width,
                        chrome_style(ChromeKind::Error, no_color),
                    ));
                }
                if view.entries.is_none() && view.error.is_none() {
                    head.push(Line::from(Span::styled("Loading...", DIM)));
                }
                let live = live
                    .by_session_mut(&view.session_id)
                    .map_or(&mut [][..], ChildThread::rows_mut);
                view.merge(live);
                let focused = view.focused_row(live);
                let entries = view.entries.as_deref().unwrap_or_default();
                view.layouts.resize_with(entries.len(), || None);
                let mut total = head.len();
                let mut pieces = vec![Piece {
                    cell: None,
                    start: 0,
                    lines: head.into(),
                }];
                for (index, &row) in view.rows.iter().enumerate() {
                    let mut lines = match row {
                        ChildRow::Live(at) => live[at].thread_lines(width, renderer),
                        ChildRow::Disk(at) => match view.recorded[at].as_mut() {
                            Some(cell) => cell.thread_lines(width, renderer),
                            None => {
                                let entry = &entries[at];
                                let hash = entry_hash(entry);
                                let slot = &mut view.layouts[at];
                                let fresh = slot.as_ref().is_some_and(|layout| {
                                    layout.hash == hash && layout.width == width
                                });
                                if !fresh {
                                    *slot = Some(EntryLayout {
                                        hash,
                                        width,
                                        lines: entry_lines(entry, width, renderer),
                                    });
                                    #[cfg(test)]
                                    {
                                        self.entry_layouts += 1;
                                    }
                                }
                                slot.as_ref()
                                    .map_or_else(Vec::new, |layout| layout.lines.clone())
                            }
                        },
                    };
                    if focused == Some(index)
                        && let Some(header) = lines.first_mut()
                    {
                        *header = header.clone().patch_style(REVERSED);
                    }
                    total += 1;
                    let rows = lines.len();
                    pieces.push(Piece {
                        cell: Some(index),
                        start: total,
                        lines: lines.into(),
                    });
                    total += rows;
                }
                PagerText { pieces, total }
            }
        }
    }

    /// The root thread: each cell's lines are taken from the last layout
    /// when its revision has not moved, and laid out again otherwise.
    fn lay_out_root(
        &mut self,
        cells: &mut [Cell],
        revisions: &[u64],
        width: u16,
        renderer: &mut Renderer,
    ) -> PagerText {
        if self.root_lines.width != width {
            self.root_lines = RootLines {
                width,
                by_revision: HashMap::new(),
            };
        }
        let mut kept = HashMap::with_capacity(cells.len());
        let mut pieces = Vec::new();
        let mut total = 0;
        for (index, (cell, &revision)) in cells.iter_mut().zip(revisions).enumerate() {
            if matches!(cell, Cell::Welcome { .. }) {
                continue;
            }
            let lines = match self.root_lines.by_revision.remove(&revision) {
                Some(lines) => lines,
                None => {
                    #[cfg(test)]
                    {
                        self.cell_layouts += 1;
                    }
                    Arc::from(cell.full_lines(width, renderer))
                }
            };
            if total > 0 {
                total += 1;
            }
            pieces.push(Piece {
                cell: Some(index),
                start: total,
                lines: Arc::clone(&lines),
            });
            total += lines.len();
            kept.insert(revision, lines);
        }
        self.root_lines.by_revision = kept;
        if total == 0 {
            return PagerText::single(vec![Line::from(Span::styled("(nothing yet)", DIM))]);
        }
        PagerText { pieces, total }
    }

    /// Resolves the pending jump against the laid-out text and keeps the
    /// view inside it.
    pub(super) fn settle(&mut self, text: &PagerText, rows: usize) {
        self.page = rows.max(1);
        let total = text.len();
        let last_top = total.saturating_sub(rows);
        match self.jump.take() {
            Some(Jump::Top) => {
                self.top = 0;
                self.follow = false;
            }
            Some(Jump::Bottom) => {
                self.top = last_top;
                self.follow = true;
            }
            Some(Jump::Cell(cell)) => {
                self.top = text.cell_start(cell).unwrap_or(last_top);
                self.follow = false;
            }
            Some(Jump::Reveal(cell)) => {
                if let Some(line) = text.cell_start(cell) {
                    if line < self.top {
                        self.top = line;
                    } else if line >= self.top + self.page {
                        self.top = line + 1 - self.page;
                    }
                }
                self.follow = self.top >= last_top;
            }
            // A match on the last page is a place the user went to read,
            // not a request to follow new lines past it.
            Some(Jump::Match { forward }) => {
                self.matched = self.find(text, forward);
                self.missed = self.matched.is_none();
                if let Some(line) = self.matched {
                    self.top = line;
                }
                self.follow = false;
            }
            None if self.follow => self.top = last_top,
            None => {}
        }
        self.top = self.top.min(last_top);
        if std::mem::take(&mut self.scrolled) {
            self.follow = self.top == last_top;
        }
    }

    /// The line of the next match after the current one (or before it),
    /// wrapping round; the first search starts at the top of the view.
    fn find(&self, text: &PagerText, forward: bool) -> Option<usize> {
        let total = text.len();
        if total == 0 || self.query.is_empty() {
            return None;
        }
        let needle = self.query.to_lowercase();
        let origin = self.find_origin(total, forward);
        (0..total)
            .map(|step| {
                if forward {
                    (origin + step) % total
                } else {
                    (origin + total - step) % total
                }
            })
            .find(|&index| holds(text.line(index), &needle))
    }

    /// The line a find over `total` lines starts from, past `total` when
    /// it wraps: after the current match (or before it), or else the top
    /// of the view.
    fn find_origin(&self, total: usize, forward: bool) -> usize {
        match (self.matched, forward) {
            (Some(line), true) => line + 1,
            (Some(line), false) => line + total - 1,
            (None, true) => self.top,
            (None, false) => self.top + total - 1,
        }
    }

    /// The thread's name for a title `width` columns wide; a child's is
    /// its path from the main thread. Short of room the path loses its
    /// threads from the top behind `... > `, the child's own name kept.
    /// Then a session id too long for it loses its middle: the ids end in
    /// what tells two runs of one agent apart. With too little room even
    /// for that, the "(read-only)" shrinks to "(ro)", since the body says
    /// so too, and then all but the id's end goes.
    fn title(&self, width: usize) -> String {
        match &self.thread {
            Thread::Root => format!("Thread: {ROOT_THREAD}"),
            Thread::Child(view) => {
                let id = render::plain_text(&view.session_id);
                let names: Vec<String> = view
                    .crumbs
                    .iter()
                    .chain(std::iter::once(&view.agent))
                    .map(|name| render::plain_text(name))
                    .collect();
                let path = |dropped: usize| {
                    let kept = names[dropped..].join(" > ");
                    if dropped == 0 {
                        kept
                    } else {
                        format!("... > {kept}")
                    }
                };
                for dropped in 0..names.len() {
                    let title = format!("Thread: {} (read-only) {id}", path(dropped));
                    if title.width() <= width {
                        return title;
                    }
                }
                let agent = path(names.len() - 1);
                for marker in ["(read-only)", "(ro)"] {
                    let head = format!("Thread: {agent} {marker} ");
                    let room = width.saturating_sub(head.width());
                    if room >= ELIDED_MIN || id.width() <= room {
                        return format!("{head}{}", elide_middle(&id, room));
                    }
                }
                let head = format!("Thread: {agent} (ro) ");
                let room = width.saturating_sub(head.width());
                format!("{head}{}", keep_tail(&id, room))
            }
        }
    }

    fn hint(&self, width: u16) -> Line<'static> {
        if let Some(field) = &self.field {
            return Line::from(format!("/{}", render::plain_text(field)));
        }
        if self.missed {
            return Line::from(Span::styled(
                format!("No match for \"{}\"", render::plain_text(&self.query)),
                DIM,
            ));
        }
        let legend = if width < NARROW { SHORT_HINT } else { HINT };
        Line::from(Span::styled(legend, DIM))
    }
}

/// The fewest columns `elide_middle` cuts to.
const ELIDED_MIN: usize = 7;

/// `text` cut to `width` characters by dropping its middle for `...`;
/// whole when it fits or when too little room is left to say anything.
fn elide_middle(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width || width < ELIDED_MIN {
        return text.to_string();
    }
    let kept = width - 3;
    let tail = kept.div_ceil(2);
    let head = kept - tail;
    let mut short: String = chars[..head].iter().collect();
    short.push_str("...");
    short.extend(&chars[chars.len() - tail..]);
    short
}

/// `text` cut to its last characters behind `...`, `width` in all; the
/// end alone when even the `...` does not fit.
pub(super) fn keep_tail(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    if width <= 3 {
        return chars[chars.len() - width..].iter().collect();
    }
    let mut short = "...".to_string();
    short.extend(&chars[chars.len() - (width - 3)..]);
    short
}

/// The pager over the whole frame: the thread's name and the position in
/// it, the text, and the key legend or the find field.
pub(super) struct PagerView<'a> {
    pub(super) pager: &'a Pager,
    pub(super) text: &'a PagerText,
    pub(super) theme: &'a Theme,
}

impl Widget for PagerView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let pager = self.pager;
        let text = self.text;
        let total = text.len();
        let rows = Pager::text_rows(area.height);
        let chrome = area.height >= 3;
        let body = text_area(area);
        let start = pager.top.min(total);
        let end = (start + rows).min(total);
        buf.set_style(area, self.theme.surface_base);
        if chrome {
            let title = Rect::new(area.x, area.y, area.width, 1);
            let hint = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
            buf.set_style(title, self.theme.surface_chrome);
            buf.set_style(hint, self.theme.surface_chrome);
            let position = format!("{}-{end}/{total}", (start + 1).min(end));
            // The name gives way to the position, which is what moves.
            let name_width = area
                .width
                .saturating_sub(u16::try_from(position.len() + 1).unwrap_or(u16::MAX));
            let name = Rect::new(area.x, area.y, name_width, 1);
            Paragraph::new(Line::from(Span::styled(
                pager.title(usize::from(name_width)),
                BOLD,
            )))
            .render(name, buf);
            Paragraph::new(Line::from(Span::styled(position, DIM)))
                .right_aligned()
                .render(title, buf);
            Paragraph::new(pager.hint(area.width)).render(hint, buf);
        }
        let visible: Vec<Line<'static>> = (start..end)
            .map(|index| text.line(index).cloned().unwrap_or_default())
            .collect();
        Paragraph::new(visible).render(body, buf);
        if let Some(line) = pager.matched
            && (start..end).contains(&line)
        {
            let y = body.y + u16::try_from(line - start).unwrap_or(u16::MAX);
            buf.set_style(Rect::new(body.x, y, body.width, 1), REVERSED);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::config::child_sessions::{ChildMeta, ChildStatus, EffectiveStatus};
    use crate::tui::TOOL_OUTPUT_TAIL;
    use crate::tui::cells::{
        AgentStatus, FoldedCall, JobStatus, LineCache, TaskRow, ToolStatus, append_output,
    };
    use crate::tui::child_live::ToolEnd;
    use crate::tui::sink::ChildNodeState;
    use crate::tui::thread_tree::{ThreadNode, ThreadTree};
    use crate::tui::threads::{Threads, ThreadsPopup};
    use crate::tui::transcript::Transcript;
    use chrono::{DateTime, FixedOffset, TimeZone, Utc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// A transcript generation no earlier call used: the tests change
    /// their cells freely between frames.
    fn fresh_generation() -> u64 {
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        GENERATION.fetch_add(1, Ordering::Relaxed)
    }

    /// A fresh revision for each of `cells`, so every one is laid out.
    fn fresh_revisions(cells: &[Cell]) -> Vec<u64> {
        cells.iter().map(|_| fresh_generation()).collect()
    }

    fn renderer() -> Renderer {
        let config = AppConfig {
            highlight: false,
            ..Default::default()
        };
        Renderer::new(config.render_options().unwrap()).unwrap()
    }

    /// A finished `execute_command` row that printed `lines` numbered
    /// lines, far more than the row's own tail keeps.
    fn long_tool(lines: usize) -> Cell {
        let mut cell = Cell::tool(
            "c1".to_string(),
            "execute_command",
            "{\"command\":\"seq\"}",
            "Call execute_command {\"command\":\"seq\"}".to_string(),
            Instant::now(),
        );
        let output: String = (1..=lines).map(|n| format!("line {n:03}\n")).collect();
        let tail = cell.output_for("c1").unwrap();
        let text = append_output(tail, output.as_bytes());
        tail.full.push(&text);
        let Cell::Tool { status, .. } = &mut cell else {
            unreachable!();
        };
        *status = ToolStatus::Ok {
            elapsed: Duration::from_millis(1500),
        };
        cell
    }

    fn root_cells() -> Vec<Cell> {
        vec![
            Cell::user("count to forty"),
            long_tool(40),
            Cell::assistant("Done counting.".to_string(), true),
        ]
    }

    fn draw(pager: &mut Pager, cells: &mut [Cell], width: u16, height: u16) -> String {
        draw_live(pager, cells, &mut ChildLive::default(), width, height)
    }

    fn draw_live(
        pager: &mut Pager,
        cells: &mut [Cell],
        live: &mut ChildLive,
        width: u16,
        height: u16,
    ) -> String {
        draw_terminal(pager, None, cells, live, &mut renderer(), width, height)
            .backend()
            .to_string()
    }

    /// The pager with the threads pop-up over it, as the App draws them.
    fn draw_with_threads(
        pager: &mut Pager,
        threads: &mut Threads,
        folds: &HashSet<String>,
        cells: &mut [Cell],
        width: u16,
        height: u16,
    ) -> String {
        draw_terminal(
            pager,
            Some((threads, folds)),
            cells,
            &mut ChildLive::default(),
            &mut renderer(),
            width,
            height,
        )
        .backend()
        .to_string()
    }

    fn draw_terminal(
        pager: &mut Pager,
        threads: Option<(&mut Threads, &HashSet<String>)>,
        cells: &mut [Cell],
        live: &mut ChildLive,
        renderer: &mut Renderer,
        width: u16,
        height: u16,
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let revisions = fresh_revisions(cells);
                let text = pager.text(
                    cells,
                    &revisions,
                    fresh_generation(),
                    live,
                    area.width,
                    renderer,
                );
                pager.settle(&text, Pager::text_rows(area.height));
                let theme = Theme::fallback(renderer.no_color);
                frame.render_widget(
                    PagerView {
                        pager,
                        text: &text,
                        theme: &theme,
                    },
                    area,
                );
                if let Some((threads, folds)) = threads {
                    let body = text_area(area);
                    threads.set_area(body);
                    frame.render_widget(
                        ThreadsPopup {
                            threads,
                            folds,
                            theme: &theme,
                            now: tree_now(),
                            offset: FixedOffset::east_opt(0).unwrap(),
                        },
                        body,
                    );
                }
            })
            .unwrap();
        terminal
    }

    #[test]
    fn the_root_thread_shows_a_tools_whole_output() {
        let mut cells = root_cells();
        let mut pager = Pager::root();
        let frame = draw(&mut pager, &mut cells, 80, 24);
        insta::assert_snapshot!("pager_root_bottom_80x24", &frame);
        assert!(frame.contains("line 040"), "{frame}");
        pager.jump_to_top();
        let frame = draw(&mut pager, &mut cells, 80, 24);
        insta::assert_snapshot!("pager_root_top_80x24", &frame);
        assert!(
            frame.contains("line 001"),
            "a line before the row's tail is not shown: {frame}"
        );
        let frame = draw(&mut pager, &mut cells, 40, 12);
        insta::assert_snapshot!("pager_root_top_40x12", &frame);
    }

    #[test]
    fn find_jumps_to_each_match_and_wraps() {
        let mut cells = root_cells();
        let mut pager = Pager::root();
        draw(&mut pager, &mut cells, 80, 24);
        pager.jump_to_top();
        draw(&mut pager, &mut cells, 80, 24);
        pager.open_find();
        assert_eq!(pager.mode(), PagerMode::Find);
        for c in "LINE 02".chars() {
            pager.find_char(c);
        }
        let frame = draw(&mut pager, &mut cells, 80, 24);
        insta::assert_snapshot!("pager_find_active_80x24", &frame);
        pager.submit_find();
        assert_eq!(pager.mode(), PagerMode::Browse);
        let frame = draw(&mut pager, &mut cells, 80, 24);
        assert!(frame.lines().last().unwrap().contains(HINT), "{frame}");
        let first = pager.matched.unwrap();
        let found = cells_line(&mut pager, &mut cells, first);
        assert!(found.contains("line 020"), "{found:?}");
        pager.find_next(true);
        draw(&mut pager, &mut cells, 80, 24);
        let second = pager.matched.unwrap();
        assert!(cells_line(&mut pager, &mut cells, second).contains("line 021"));
        pager.find_next(false);
        draw(&mut pager, &mut cells, 80, 24);
        assert_eq!(pager.matched, Some(first));
        pager.find_next(false);
        draw(&mut pager, &mut cells, 80, 24);
        let last = pager.matched.unwrap();
        assert!(
            cells_line(&mut pager, &mut cells, last).contains("line 029"),
            "a backward find did not wrap to the last match"
        );

        pager.open_find();
        pager.find_char('x');
        pager.find_char('q');
        pager.submit_find();
        let frame = draw(&mut pager, &mut cells, 80, 24);
        assert!(frame.contains("No match for \"xq\""), "{frame}");
        pager.open_find();
        pager.cancel_find();
        assert_eq!(pager.mode(), PagerMode::Browse);
        assert_eq!(pager.matched, None);
    }

    fn cells_line(pager: &mut Pager, cells: &mut [Cell], line: usize) -> String {
        let revisions = fresh_revisions(cells);
        pager
            .text(
                cells,
                &revisions,
                fresh_generation(),
                &mut ChildLive::default(),
                80,
                &mut renderer(),
            )
            .line(line)
            .map(Line::to_string)
            .unwrap_or_default()
    }

    #[test]
    fn scrolling_clamps_and_a_growing_thread_is_followed_from_the_bottom() {
        let mut cells = root_cells();
        let mut pager = Pager::root();
        draw(&mut pager, &mut cells, 80, 24);
        assert!(pager.follow);
        pager.scroll(-3);
        draw(&mut pager, &mut cells, 80, 24);
        assert!(!pager.follow);
        let held = pager.top;
        cells.push(Cell::user("more"));
        draw(&mut pager, &mut cells, 80, 24);
        assert_eq!(pager.top, held, "a scrolled view moved as lines arrived");
        pager.page(1);
        pager.page(1);
        draw(&mut pager, &mut cells, 80, 24);
        assert!(pager.follow);
        cells.push(Cell::user("newest line here"));
        let frame = draw(&mut pager, &mut cells, 80, 24);
        assert!(frame.contains("newest line here"), "{frame}");
        pager.scroll(-1000);
        draw(&mut pager, &mut cells, 80, 24);
        assert_eq!(pager.top, 0);
    }

    #[test]
    fn a_cell_jump_starts_the_view_at_that_row() {
        let mut cells = root_cells();
        let mut pager = Pager::root_at(1);
        let frame = draw(&mut pager, &mut cells, 80, 24);
        let first_text_row = frame.lines().nth(1).unwrap();
        assert!(first_text_row.contains("execute_command("), "{frame}");
    }

    fn meta(session_id: &str, agent: &str) -> ChildMeta {
        let at = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        let mut meta = ChildMeta::new_running_at(
            at,
            session_id,
            agent,
            "agent-1",
            crate::utils::process::ProcessStamp { pid: 1, start: 1 },
            None,
            None,
            "do it",
            None,
        );
        meta.finish_at(at, ChildStatus::Completed, None);
        meta
    }

    #[test]
    fn the_thread_picker_lists_the_root_and_every_child_with_its_status() {
        let mut cells = root_cells();
        let mut pager = Pager::root();
        let folds = HashSet::new();
        let mut threads = open_tree(&folds);
        threads.move_highlight(2, &folds);
        let frame = draw_with_threads(&mut pager, &mut threads, &folds, &mut cells, 80, 24);
        insta::assert_snapshot!("thread_tree_80x24", &frame);
        let order: Vec<usize> = [
            "reviewer ",
            "coder ",
            "fixer ",
            "tester ",
            "planner ",
            "scout ",
            "auditor ",
        ]
        .iter()
        .map(|name| {
            frame
                .find(name)
                .unwrap_or_else(|| panic!("{name}: {frame}"))
        })
        .collect();
        assert!(order.is_sorted(), "{frame}");
        assert!(
            frame.contains("    \u{25BE} coder  running  03:01:05  3m 0s  coder-b2"),
            "{frame}"
        );
        assert!(
            frame.contains("        tester  completed  03:02:05  1m 0s  tester-c3"),
            "{frame}"
        );
        assert!(frame.contains("scout  cancelled"), "{frame}");
        let frame = draw_with_threads(&mut pager, &mut threads, &folds, &mut cells, 40, 20);
        insta::assert_snapshot!("thread_tree_40x20", &frame);
        assert_eq!(
            threads.picked(&folds).session_id.as_deref(),
            Some("coder-b2")
        );
    }

    fn tree_meta(
        session_id: &str,
        parent: Option<&str>,
        minute: u32,
        ended: Option<(u32, ChildStatus)>,
    ) -> ChildMeta {
        let at = |minute: u32| Utc.with_ymd_and_hms(2026, 1, 2, 3, minute, 5).unwrap();
        let agent = session_id.split('-').next().unwrap();
        let mut meta = ChildMeta::new_running_at(
            at(minute),
            session_id,
            agent,
            "agent-1",
            crate::utils::process::ProcessStamp { pid: 1, start: 1 },
            parent.map(str::to_string),
            None,
            "do it",
            None,
        );
        if let Some((minute, status)) = ended {
            meta.finish_at(at(minute), status, None);
        }
        meta
    }

    /// Three levels of children under the main thread, running and
    /// finished ones mixed at each, listed out of order.
    fn thread_tree() -> ThreadTree {
        let running = |session_id, parent, minute| {
            ThreadNode::child(
                &tree_meta(session_id, parent, minute, None),
                EffectiveStatus::Running,
            )
        };
        let ended = |session_id, parent, minute, end, status, effective| {
            ThreadNode::child(
                &tree_meta(session_id, parent, minute, Some((end, status))),
                effective,
            )
        };
        ThreadTree::new(vec![
            ended(
                "scout-f6",
                None,
                0,
                1,
                ChildStatus::Cancelled,
                EffectiveStatus::Cancelled,
            ),
            ended(
                "planner-e5",
                None,
                0,
                2,
                ChildStatus::Completed,
                EffectiveStatus::Completed,
            ),
            ended(
                "tester-c3",
                Some("coder-b2"),
                2,
                3,
                ChildStatus::Completed,
                EffectiveStatus::Completed,
            ),
            running("fixer-d4", Some("coder-b2"), 3),
            running("coder-b2", Some("reviewer-a1"), 1),
            running("reviewer-a1", None, 0),
            ended(
                "auditor-g7",
                None,
                1,
                2,
                ChildStatus::Failed,
                EffectiveStatus::Failed,
            ),
        ])
    }

    /// What the threads pop-up's run times are counted to in these tests.
    fn tree_now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap()
    }

    fn open_tree(folds: &HashSet<String>) -> Threads {
        Threads::new(thread_tree(), None, None, folds)
    }

    /// The arrow keys' moves: right unfolds or steps into the first
    /// child, left folds or steps out to the parent, Space folds and
    /// unfolds, and the folds outlast the pop-up.
    #[test]
    fn the_thread_picker_folds_unfolds_and_steps_through_the_tree() {
        let mut folds = HashSet::new();
        let mut threads = open_tree(&folds);
        let picked = |threads: &Threads| threads.highlight().to_string();
        threads.unfold(&mut folds);
        assert_eq!(picked(&threads), "reviewer-a1");
        threads.unfold(&mut folds);
        threads.unfold(&mut folds);
        assert_eq!(picked(&threads), "fixer-d4");
        threads.unfold(&mut folds);
        assert_eq!(picked(&threads), "fixer-d4", "a leaf has nothing to unfold");
        threads.fold(&mut folds);
        assert_eq!(picked(&threads), "coder-b2");
        threads.fold(&mut folds);
        assert!(folds.contains("coder-b2"));
        assert_eq!(picked(&threads), "coder-b2");
        threads.move_highlight(1, &folds);
        assert_eq!(
            picked(&threads),
            "planner-e5",
            "a folded thread hides its children"
        );
        threads.move_highlight(-1, &folds);
        threads.fold(&mut folds);
        assert_eq!(picked(&threads), "reviewer-a1");
        threads.toggle(&mut folds);
        assert!(folds.contains("reviewer-a1"));
        let mut threads = open_tree(&folds);
        let mut pager = Pager::root();
        let mut cells = root_cells();
        let frame = draw_with_threads(&mut pager, &mut threads, &folds, &mut cells, 80, 24);
        assert!(frame.contains("\u{25B8} reviewer"), "{frame}");
        assert!(!frame.contains("coder"), "{frame}");
        threads.move_highlight(1, &folds);
        threads.toggle(&mut folds);
        let frame = draw_with_threads(&mut pager, &mut threads, &folds, &mut cells, 80, 24);
        assert!(frame.contains("\u{25B8} coder"), "{frame}");
        assert!(!frame.contains("fixer"), "{frame}");
    }

    /// A click on a thread's arrow folds it and highlights it; one
    /// elsewhere on a row only highlights it.
    #[test]
    fn a_click_on_an_arrow_folds_and_on_a_row_highlights() {
        let mut cells = root_cells();
        let mut pager = Pager::root();
        let mut folds = HashSet::new();
        let mut threads = open_tree(&folds);
        let frame = draw_with_threads(&mut pager, &mut threads, &folds, &mut cells, 80, 24);
        let at = |frame: &str, needle: &str| {
            frame
                .lines()
                .enumerate()
                .find_map(|(y, line)| {
                    let x = line.find(needle)?;
                    Some((
                        u16::try_from(line[..x].chars().count() - 1).unwrap(),
                        u16::try_from(y).unwrap(),
                    ))
                })
                .unwrap_or_else(|| panic!("no {needle}: {frame}"))
        };
        let (column, row) = at(&frame, "tester");
        threads.click(column + 3, row, &mut folds);
        assert_eq!(threads.highlight(), "tester-c3");
        assert!(folds.is_empty());
        let (column, row) = at(&frame, "\u{25BE} coder");
        threads.click(column + 1, row, &mut folds);
        assert_eq!(threads.highlight(), "coder-b2");
        assert!(folds.contains("coder-b2"));
        let frame = draw_with_threads(&mut pager, &mut threads, &folds, &mut cells, 80, 24);
        assert!(!frame.contains("tester"), "{frame}");
    }

    /// Under NO_COLOR the states keep their words and lose their colours;
    /// with colour, each state's name and word take its theme token.
    #[test]
    fn the_thread_states_are_coloured_by_the_theme_and_plain_under_no_color() {
        let mut cells = root_cells();
        let mut pager = Pager::root();
        let folds = HashSet::new();
        let mut threads = open_tree(&folds);
        let mut renderer = renderer();
        renderer.no_color = false;
        let terminal = draw_terminal(
            &mut pager,
            Some((&mut threads, &folds)),
            &mut cells,
            &mut ChildLive::default(),
            &mut renderer,
            80,
            24,
        );
        let theme = Theme::fallback(false);
        let buffer = terminal.backend().buffer();
        let frame = terminal.backend().to_string();
        let fg_of = |needle: &str| {
            let (y, line) = frame
                .lines()
                .enumerate()
                .find(|(_, line)| line.contains(needle))
                .unwrap();
            let x = line[..line.find(needle).unwrap()].chars().count() - 1;
            buffer[(u16::try_from(x).unwrap(), u16::try_from(y).unwrap())].fg
        };
        assert_eq!(Some(fg_of("running")), theme.thread_running.fg);
        assert_eq!(Some(fg_of("planner")), theme.thread_completed.fg);
        assert_eq!(Some(fg_of("cancelled")), theme.thread_failed.fg);
        assert_eq!(Some(fg_of("failed")), theme.thread_failed.fg);

        renderer.no_color = true;
        let terminal = draw_terminal(
            &mut pager,
            Some((&mut threads, &folds)),
            &mut cells,
            &mut ChildLive::default(),
            &mut renderer,
            80,
            24,
        );
        let frame = terminal.backend().to_string();
        for word in ["running", "completed", "cancelled", "failed"] {
            assert!(frame.contains(word), "{word}: {frame}");
        }
        for cell in terminal.backend().buffer().content() {
            assert_eq!(cell.fg, ratatui::style::Color::Reset, "{cell:?}");
        }
    }

    fn child_entries() -> Vec<ChildEntry> {
        vec![
            ChildEntry::User("review src/lib.rs".to_string()),
            ChildEntry::Assistant("Reading the file first.".to_string()),
            ChildEntry::Tool {
                call_id: "call-r1".to_string(),
                name: "fs_read".to_string(),
                args: "{\"path\":\"src/lib.rs\"}".to_string(),
                output: "fn main() {}\n".to_string(),
            },
            ChildEntry::Assistant("It looks fine.".to_string()),
        ]
    }

    #[test]
    fn a_child_thread_is_read_only_and_says_when_rounds_appear() {
        let mut cells = Vec::new();
        let mut pager = Pager::child(
            "reviewer-a1".to_string(),
            "reviewer".to_string(),
            PathBuf::from("/nonexistent/reviewer-a1.yaml"),
        );
        let frame = draw(&mut pager, &mut cells, 80, 24);
        assert!(frame.contains("Loading..."), "{frame}");
        deliver(&mut pager, "someone-else", Ok(Vec::new()));
        deliver(&mut pager, "reviewer-a1", Ok(child_entries()));
        let frame = draw(&mut pager, &mut cells, 80, 24);
        insta::assert_snapshot!("pager_child_80x24", &frame);
        let frame = draw(&mut pager, &mut cells, 40, 12);
        insta::assert_snapshot!("pager_child_40x12", &frame);
    }

    /// Hands the pager a read the way the loop does: started, then back.
    fn deliver(pager: &mut Pager, session_id: &str, result: Result<Vec<ChildEntry>, String>) {
        pager.child_load_started(1);
        pager.set_child_entries(session_id, 1, result);
    }

    const REVIEWER: &str = "agent_reviewer_4e5f";
    const PLANNER: &str = "agent_planner_7a8b";

    fn ok_end(duration_ms: u64) -> ToolEnd {
        ToolEnd {
            name: String::new(),
            args: None,
            ok: true,
            error: None,
            duration_ms: Some(duration_ms),
        }
    }

    /// The reviewer's live rows: the saved `call-r1` finished, and
    /// `call-r2`, which its session has yet to save, still running.
    fn reviewer_live() -> ChildLive {
        let now = Instant::now();
        let mut live = ChildLive::default();
        live.agent_started(
            REVIEWER.to_string(),
            "reviewer".to_string(),
            Some("reviewer-a1".to_string()),
            None,
            None,
            Utc::now(),
        );
        live.tool_started(
            REVIEWER,
            None,
            "call-r1".to_string(),
            "fs_read".to_string(),
            Some(serde_json::json!({"path": "src/lib.rs"})),
            now,
        );
        live.tool_output(REVIEWER, None, "call-r1", b"fn main() {}\n");
        live.tool_finished(REVIEWER, None, "call-r1", ok_end(1200), now);
        live.tool_started(
            REVIEWER,
            None,
            "call-r2".to_string(),
            "execute_command".to_string(),
            Some(serde_json::json!({"command": "cargo test"})),
            now,
        );
        live.tool_output(REVIEWER, None, "call-r2", b"running 3 tests\n");
        live
    }

    /// The reviewer's saved session: `call-r0` from before the UI
    /// started, which no live row reports, and `call-r1`.
    fn reviewer_entries() -> Vec<ChildEntry> {
        let mut entries = child_entries();
        entries.insert(
            2,
            ChildEntry::Tool {
                call_id: "call-r0".to_string(),
                name: "fs_grep".to_string(),
                args: "{\"pattern\":\"unsafe\"}".to_string(),
                output: "no matches\n".to_string(),
            },
        );
        entries
    }

    fn reviewer_pager() -> Pager {
        let mut pager = Pager::child(
            "reviewer-a1".to_string(),
            "reviewer".to_string(),
            PathBuf::from("/nonexistent/reviewer-a1.yaml"),
        );
        deliver(&mut pager, "reviewer-a1", Ok(reviewer_entries()));
        pager
    }

    /// A child's thread shows its saved text with each call as a tool
    /// row of the main view's: a saved call with a live row as that row,
    /// one without as a recorded row that claims no time, then the calls
    /// made since the last save. Every row starts folded.
    #[test]
    fn a_child_thread_merges_saved_text_with_live_tool_rows() {
        let mut cells = Vec::new();
        let mut live = reviewer_live();
        let mut pager = reviewer_pager();
        let frame = draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        insta::assert_snapshot!("pager_child_live_80x24", &frame);
        let row = |name: &str| {
            frame
                .lines()
                .find(|line| line.contains(name))
                .unwrap_or_else(|| panic!("no {name} row: {frame}"))
                .to_string()
        };
        assert!(row("fs_grep(").contains("pattern"), "{frame}");
        assert!(row("fs_grep(").contains("})  done "), "{frame}");
        assert!(row("fs_read(").contains("1s  ok"), "{frame}");
        assert!(row("execute_command(").contains("running"), "{frame}");
        assert_eq!(frame.matches("fs_read(").count(), 1, "{frame}");
        assert!(
            !frame.contains("fn main()"),
            "a folded row shows output: {frame}"
        );
        let frame = draw_live(&mut pager, &mut cells, &mut live, 40, 12);
        insta::assert_snapshot!("pager_child_live_40x12", &frame);
    }

    /// Ctrl+Up from no focus goes to the last foldable row and Enter
    /// opens it on its call and output tail; the open state is kept in
    /// the live row, so it outlasts a new layout.
    #[test]
    fn a_focused_child_row_opens_and_stays_open() {
        let mut cells = Vec::new();
        let mut live = reviewer_live();
        let mut pager = reviewer_pager();
        draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        pager.step_focus(&live, false);
        pager.toggle_focused(&mut live);
        let frame = draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        insta::assert_snapshot!("pager_child_live_expanded_80x24", &frame);
        assert!(frame.contains("running 3 tests"), "{frame}");
        assert!(
            !frame.contains("execute_command {\"command\""),
            "the open row says its call again: {frame}"
        );
        pager.child_rows_changed();
        let frame = draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        assert!(frame.contains("running 3 tests"), "{frame}");

        pager.step_focus(&live, false);
        pager.toggle_focused(&mut live);
        let frame = draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        assert!(frame.contains("fn main() {}"), "{frame}");
        pager.step_focus(&live, false);
        pager.toggle_focused(&mut live);
        let frame = draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        assert!(frame.contains("no matches"), "{frame}");
        pager.step_focus(&live, false);
        assert_eq!(pager.mode(), PagerMode::Browse);
        deliver(&mut pager, "reviewer-a1", Ok(reviewer_entries()));
        let frame = draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        assert!(
            frame.contains("no matches"),
            "a re-read folded a recorded row: {frame}"
        );
    }

    /// A left click on a row's header opens it; on text it does nothing.
    #[test]
    fn a_click_on_a_child_row_opens_it() {
        let mut cells = Vec::new();
        let mut live = reviewer_live();
        let mut pager = reviewer_pager();
        let area = Rect::new(0, 0, 80, 24);
        let frame = draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        let y = |frame: &str, needle: &str| {
            u16::try_from(
                frame
                    .lines()
                    .position(|line| line.contains(needle))
                    .unwrap(),
            )
            .unwrap()
        };
        pager.click(&mut live, 3, y(&frame, "It looks fine."), area);
        assert_eq!(draw_live(&mut pager, &mut cells, &mut live, 80, 24), frame);
        pager.click(&mut live, 3, y(&frame, "execute_command("), area);
        let frame = draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        assert!(frame.contains("running 3 tests"), "{frame}");
    }

    /// The focus stays on its call when a re-read puts entries above it.
    #[test]
    fn the_focus_stays_on_its_call_across_a_reread() {
        let mut cells = Vec::new();
        let mut live = reviewer_live();
        let mut pager = reviewer_pager();
        draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        pager.step_focus(&live, true);
        let mut entries = reviewer_entries();
        entries.insert(0, ChildEntry::User("an earlier line".to_string()));
        entries.insert(1, ChildEntry::Assistant("An earlier reply.".to_string()));
        deliver(&mut pager, "reviewer-a1", Ok(entries));
        pager.toggle_focused(&mut live);
        let frame = draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        assert!(frame.contains("no matches"), "{frame}");
    }

    /// A call with no id is shown once: a saved one pairs with the live
    /// row that has its name and arguments, in the order both came.
    #[test]
    fn calls_with_no_id_pair_with_their_live_rows_by_name_and_arguments() {
        let now = Instant::now();
        let mut live = ChildLive::default();
        live.agent_started(
            REVIEWER.to_string(),
            "reviewer".to_string(),
            Some("reviewer-a1".to_string()),
            None,
            None,
            Utc::now(),
        );
        for name in ["fs_read", "fs_grep", "fs_read"] {
            live.tool_started(
                REVIEWER,
                None,
                String::new(),
                name.to_string(),
                Some(serde_json::json!({"path": "a"})),
                now,
            );
        }
        let saved = |name: &str| ChildEntry::Tool {
            call_id: String::new(),
            name: name.to_string(),
            args: "{\"path\":\"a\"}".to_string(),
            output: String::new(),
        };
        let entries = vec![saved("fs_read"), saved("fs_read"), saved("fs_grep")];
        let rows = merged_rows(&entries, live_rows(&live, "reviewer-a1"));
        assert_eq!(
            rows,
            [ChildRow::Live(0), ChildRow::Live(2), ChildRow::Live(1)]
        );

        let mut pager = reviewer_pager();
        deliver(&mut pager, "reviewer-a1", Ok(entries));
        let frame = draw_live(&mut pager, &mut Vec::new(), &mut live, 80, 24);
        assert_eq!(frame.matches("fs_read(").count(), 2, "{frame}");
        assert!(!frame.contains("done"), "{frame}");
    }

    /// A graph child shows a row per node in start order, its calls
    /// folded under it; a saved call a node holds is not shown again.
    #[test]
    fn a_graph_child_shows_a_row_per_node_with_its_calls() {
        let now = Instant::now();
        let mut live = ChildLive::default();
        live.agent_started(
            PLANNER.to_string(),
            "planner".to_string(),
            Some("planner-b2".to_string()),
            None,
            None,
            Utc::now(),
        );
        live.node(
            PLANNER,
            "plan".to_string(),
            "llm".to_string(),
            ChildNodeState::Started,
            now,
        );
        for (call_id, name, said) in [
            ("call-p1", "fs_read", "read Cargo.toml"),
            ("call-p2", "fs_grep", "3 matches"),
        ] {
            live.tool_started(
                PLANNER,
                Some("plan".to_string()),
                call_id.to_string(),
                name.to_string(),
                None,
                now,
            );
            live.tool_output(
                PLANNER,
                Some("plan"),
                call_id,
                format!("{said}\n").as_bytes(),
            );
            live.tool_finished(PLANNER, Some("plan"), call_id, ok_end(300), now);
        }
        live.node(
            PLANNER,
            "plan".to_string(),
            "llm".to_string(),
            ChildNodeState::Completed {
                duration_ms: Some(2000),
                route: Some("build".to_string()),
            },
            now,
        );
        live.node(
            PLANNER,
            "build".to_string(),
            "tool".to_string(),
            ChildNodeState::Started,
            now,
        );
        live.node(
            PLANNER,
            "build".to_string(),
            "tool".to_string(),
            ChildNodeState::Failed {
                error: "exit 3".to_string(),
            },
            now,
        );
        let mut pager = Pager::child(
            "planner-b2".to_string(),
            "planner".to_string(),
            PathBuf::from("/nonexistent/planner-b2.yaml"),
        );
        deliver(
            &mut pager,
            "planner-b2",
            Ok(vec![
                ChildEntry::User("plan the release".to_string()),
                ChildEntry::Tool {
                    call_id: "call-p1".to_string(),
                    name: "fs_read".to_string(),
                    args: "{}".to_string(),
                    output: "read Cargo.toml\n".to_string(),
                },
            ]),
        );
        let mut cells = Vec::new();
        let frame = draw_live(&mut pager, &mut cells, &mut live, 80, 24);
        insta::assert_snapshot!("pager_child_graph_80x24", &frame);
        let plan = frame.find("node plan").expect("a plan row");
        let build = frame.find("node build").expect("a build row");
        assert!(plan < build, "{frame}");
        assert!(frame.contains("read Cargo.toml"), "{frame}");
        assert!(frame.contains("3 matches"), "{frame}");
        assert!(frame.contains("failed: exit 3"), "{frame}");
        assert!(
            !frame.contains("fs_read("),
            "a node's call shows twice: {frame}"
        );
        let frame = draw_live(&mut pager, &mut cells, &mut live, 40, 12);
        insta::assert_snapshot!("pager_child_graph_40x12", &frame);
    }

    /// Calls with no id inside a node are shown once, folded under it:
    /// each saved one pairs with the node's call of the same name and
    /// arguments. Run side by side, each finish settles the call with its
    /// own arguments, whatever order they end in.
    #[test]
    fn a_nodes_calls_with_no_id_show_once_and_finish_by_arguments() {
        let now = Instant::now();
        let mut live = ChildLive::default();
        live.agent_started(
            PLANNER.to_string(),
            "planner".to_string(),
            Some("planner-b2".to_string()),
            None,
            None,
            Utc::now(),
        );
        live.node(
            PLANNER,
            "plan".to_string(),
            "llm".to_string(),
            ChildNodeState::Started,
            now,
        );
        for path in ["a.rs", "b.rs"] {
            live.tool_started(
                PLANNER,
                Some("plan".to_string()),
                String::new(),
                "fs_read".to_string(),
                Some(serde_json::json!({ "path": path })),
                now,
            );
        }
        live.tool_finished(
            PLANNER,
            Some("plan"),
            "",
            ToolEnd {
                name: "fs_read".to_string(),
                args: Some(serde_json::json!({"path": "b.rs"})),
                ok: false,
                error: Some("denied".to_string()),
                duration_ms: Some(10),
            },
            now,
        );
        let [Cell::Node { row, .. }] = live_rows(&live, "planner-b2") else {
            panic!("not one node row");
        };
        let calls = &row.calls;
        assert!(matches!(calls[0].status, Some(ToolStatus::Running { .. })));
        assert!(matches!(calls[1].status, Some(ToolStatus::Failed { .. })));

        let saved = |path: &str| ChildEntry::Tool {
            call_id: String::new(),
            name: "fs_read".to_string(),
            args: format!("{{\"path\":\"{path}\"}}"),
            output: String::new(),
        };
        let entries = vec![
            ChildEntry::User("plan the release".to_string()),
            saved("a.rs"),
            saved("b.rs"),
        ];
        let rows = merged_rows(&entries, live_rows(&live, "planner-b2"));
        assert_eq!(rows, [ChildRow::Disk(0), ChildRow::Live(0)]);

        let mut pager = Pager::child(
            "planner-b2".to_string(),
            "planner".to_string(),
            PathBuf::from("/nonexistent/planner-b2.yaml"),
        );
        deliver(&mut pager, "planner-b2", Ok(entries));
        let frame = draw_live(&mut pager, &mut Vec::new(), &mut live, 80, 24);
        assert!(!frame.contains("fs_read("), "{frame}");
        assert!(!frame.contains("done"), "{frame}");
        let call_lines = frame
            .lines()
            .filter(|line| line.trim_matches('"').trim_start().starts_with("fs_read"))
            .count();
        assert_eq!(call_lines, 2, "{frame}");
    }

    /// Under NO_COLOR a child's tool rows keep their text and lose every
    /// colour.
    #[test]
    fn a_child_thread_under_no_color_has_no_colour() {
        let mut cells = Vec::new();
        let mut live = reviewer_live();
        let mut pager = reviewer_pager();
        let mut renderer = renderer();
        renderer.no_color = true;
        let terminal = draw_terminal(
            &mut pager,
            None,
            &mut cells,
            &mut live,
            &mut renderer,
            80,
            24,
        );
        let frame = terminal.backend().to_string();
        insta::assert_snapshot!("pager_child_live_no_color_80x24", &frame);
        for cell in terminal.backend().buffer().content() {
            assert_eq!(cell.fg, ratatui::style::Color::Reset, "{cell:?}");
            assert_eq!(cell.bg, ratatui::style::Color::Reset, "{cell:?}");
        }
    }

    /// A re-read of a growing thread lays out the new entry only; a new
    /// width or a forgotten layout lays out every entry again. A saved
    /// call is drawn from its row's own cache, so only text counts.
    #[test]
    fn a_reread_lays_out_only_the_entries_it_added() {
        let mut cells = Vec::new();
        let mut pager = Pager::child(
            "reviewer-a1".to_string(),
            "reviewer".to_string(),
            PathBuf::from("/nonexistent/reviewer-a1.yaml"),
        );
        let mut entries = child_entries();
        let texts = |entries: &[ChildEntry]| {
            entries
                .iter()
                .filter(|entry| recorded_row(entry).is_none())
                .count()
        };
        deliver(&mut pager, "reviewer-a1", Ok(entries.clone()));
        draw(&mut pager, &mut cells, 80, 24);
        assert_eq!(pager.entry_layouts, texts(&entries));

        entries.push(ChildEntry::Assistant("One more round.".to_string()));
        deliver(&mut pager, "reviewer-a1", Ok(entries.clone()));
        let frame = draw(&mut pager, &mut cells, 80, 24);
        assert!(frame.contains("One more round."), "{frame}");
        assert_eq!(pager.entry_layouts, texts(&entries));

        draw(&mut pager, &mut cells, 60, 24);
        assert_eq!(pager.entry_layouts, 2 * texts(&entries));
        pager.forget_layout();
        draw(&mut pager, &mut cells, 60, 24);
        assert_eq!(pager.entry_layouts, 3 * texts(&entries));
    }

    /// The picker opens on the thread on show, and the rounds a child's
    /// session compressed sit under a divider of their own.
    #[test]
    fn the_picker_starts_on_the_open_thread_and_compressed_rounds_are_marked() {
        let mut cells = Vec::new();
        let mut pager = Pager::child(
            "planner-b2".to_string(),
            "planner".to_string(),
            PathBuf::from("/nonexistent/planner-b2.yaml"),
        );
        deliver(
            &mut pager,
            "planner-b2",
            Ok(vec![
                ChildEntry::Compressed,
                ChildEntry::Assistant("an old round".to_string()),
                ChildEntry::Assistant("the newest round".to_string()),
            ]),
        );
        let frame = draw(&mut pager, &mut cells, 80, 24);
        assert!(frame.contains(COMPRESSED_DIVIDER), "{frame}");
        assert!(
            frame.contains("Thread: planner (read-only) planner-b2"),
            "{frame}"
        );
        let tree = ThreadTree::new(vec![
            ThreadNode::child(&meta("reviewer-a1", "reviewer"), EffectiveStatus::Running),
            ThreadNode::child(&meta("planner-b2", "planner"), EffectiveStatus::Running),
        ]);
        let threads = Threads::new(tree, None, pager.child_session(), &HashSet::new());
        assert_eq!(
            threads.picked(&HashSet::new()).session_id.as_deref(),
            Some("planner-b2")
        );
    }

    /// A thread three levels down is titled with its path from the main
    /// thread; short of room the path loses threads from the top first,
    /// and only then does the id give way.
    #[test]
    fn a_nested_thread_is_titled_with_its_path_which_narrows_from_the_top() {
        let tree = thread_tree();
        let crumbs = tree.crumbs("tester-c3");
        assert_eq!(crumbs, ["main", "reviewer", "coder"]);
        let pager = Pager::child(
            "tester-c3".to_string(),
            "tester".to_string(),
            PathBuf::from("/nonexistent/tester-c3.yaml"),
        )
        .with_crumbs(crumbs);
        let mut cells = Vec::new();
        let mut pager = pager;
        let frame = draw(&mut pager, &mut cells, 80, 24);
        let title = frame.lines().next().unwrap();
        assert!(
            title.contains("Thread: main > reviewer > coder > tester (read-only) tester-c3"),
            "{title}"
        );
        let full = "Thread: main > reviewer > coder > tester (read-only) tester-c3";
        assert_eq!(pager.title(full.len()), full);
        assert_eq!(
            pager.title(full.len() - 1),
            "Thread: ... > reviewer > coder > tester (read-only) tester-c3"
        );
        assert_eq!(
            pager.title(50),
            "Thread: ... > coder > tester (read-only) tester-c3"
        );
        assert_eq!(
            pager.title(49),
            "Thread: ... > tester (read-only) tester-c3"
        );
        assert_eq!(pager.title(41), "Thread: ... > tester (read-only) te...-c3");
    }

    /// A flush during a read asks for nothing at once and exactly one
    /// read after it; a read older than the newest one asked for is
    /// dropped.
    #[test]
    fn one_read_runs_at_a_time_and_a_stale_read_is_dropped() {
        let mut cells = Vec::new();
        let file = PathBuf::from("/nonexistent/reviewer-a1.yaml");
        let mut pager = Pager::child(
            "reviewer-a1".to_string(),
            "reviewer".to_string(),
            file.clone(),
        );
        pager.child_load_started(1);
        assert_eq!(pager.child_flushed("reviewer-a1"), None);
        assert_eq!(pager.child_flushed("reviewer-a1"), None);
        assert_eq!(pager.child_flushed("someone-else"), None);
        let older = vec![ChildEntry::Assistant("old read".to_string())];
        assert_eq!(
            pager.set_child_entries("reviewer-a1", 1, Ok(older)),
            Some(file.clone())
        );
        pager.child_load_started(2);
        assert_eq!(
            pager.set_child_entries("reviewer-a1", 1, Ok(Vec::new())),
            None
        );
        let newest = vec![ChildEntry::Assistant("new read".to_string())];
        assert_eq!(pager.set_child_entries("reviewer-a1", 2, Ok(newest)), None);
        let frame = draw(&mut pager, &mut cells, 80, 24);
        assert!(frame.contains("new read"), "{frame}");
        assert_eq!(pager.child_flushed("reviewer-a1"), Some(file));
    }

    #[test]
    fn a_child_that_cannot_be_read_degrades_to_an_error_line() {
        let mut cells = Vec::new();
        let mut pager = Pager::child(
            "reviewer-a1".to_string(),
            "reviewer".to_string(),
            PathBuf::from("/nonexistent/reviewer-a1.yaml"),
        );
        deliver(
            &mut pager,
            "reviewer-a1",
            Err("Invalid session at reviewer-a1.yaml: did not find expected key".to_string()),
        );
        let frame = draw(&mut pager, &mut cells, 80, 24);
        insta::assert_snapshot!("pager_child_malformed_80x24", &frame);
        deliver(&mut pager, "reviewer-a1", Ok(child_entries()));
        deliver(&mut pager, "reviewer-a1", Err("gone".to_string()));
        let frame = draw(&mut pager, &mut cells, 80, 24);
        assert!(
            frame.contains("Cannot read this transcript: gone"),
            "{frame}"
        );
        assert!(
            frame.contains("It looks fine."),
            "a failed re-read dropped what the last good read showed: {frame}"
        );
    }

    /// The root thread is laid out once per width and transcript
    /// generation; frames in between reuse it.
    #[test]
    fn the_root_thread_is_laid_out_again_only_when_the_transcript_or_width_moved() {
        let mut cells = root_cells();
        let revisions = vec![1, 2, 3];
        let mut pager = Pager::root();
        let mut renderer = renderer();
        let mut live = ChildLive::default();
        let first = pager.text(&mut cells, &revisions, 5, &mut live, 80, &mut renderer);
        let again = pager.text(&mut cells, &revisions, 5, &mut live, 80, &mut renderer);
        assert!(Arc::ptr_eq(&first, &again));
        let changed = pager.text(&mut cells, &revisions, 6, &mut live, 80, &mut renderer);
        assert!(!Arc::ptr_eq(&first, &changed));
        let narrower = pager.text(&mut cells, &revisions, 6, &mut live, 40, &mut renderer);
        assert!(!Arc::ptr_eq(&changed, &narrower));
    }

    /// A folded call that printed `lines` numbered lines, each starting
    /// with `label`.
    fn folded(call_id: &str, name: &str, label: &str, lines: usize) -> FoldedCall {
        let mut call = FoldedCall::new(call_id.to_string(), name.to_string());
        let output: String = (1..=lines).map(|n| format!("{label} {n:02}\n")).collect();
        let text = append_output(&mut call.tail, output.as_bytes());
        call.tail.full.push(&text);
        call
    }

    /// A job and an agent whose folded calls printed more than a row
    /// keeps, the job failed: the pager shows each call under its name
    /// from its first line, and the job's error whole where its row
    /// shows only the first line.
    #[test]
    fn the_pager_opens_job_and_agent_rows_on_every_call_and_the_whole_error() {
        let lines = TOOL_OUTPUT_TAIL + 5;
        let mut cells = vec![
            Cell::Job {
                job_id: "job_0a1b2c3d".to_string(),
                tool: "execute_command".to_string(),
                row: TaskRow {
                    calls: vec![folded("c1", "job__check", "job", lines)],
                    ..TaskRow::new(JobStatus::Failed(
                        "exit status 2\nstderr: no space left\nwhile writing out.bin".to_string(),
                    ))
                },
                cache: LineCache::default(),
            },
            Cell::Agent {
                agent_id: "agent_reviewer_4e5f6a7b".to_string(),
                name: "reviewer".to_string(),
                row: TaskRow {
                    calls: vec![
                        folded("c2", "agent__check", "progress", lines),
                        folded("c3", "agent__collect", "verdict", 2),
                    ],
                    ..TaskRow::new(AgentStatus::Completed)
                },
                cache: LineCache::default(),
            },
        ];
        let mut renderer = renderer();
        let collapsed: Vec<String> = cells
            .iter_mut()
            .flat_map(|cell| cell.lines(80, &mut renderer))
            .map(|line| line.to_string())
            .collect();
        assert!(
            !collapsed.iter().any(|line| line.contains("stderr")),
            "{collapsed:?}"
        );
        let mut pager = Pager::root();
        pager.jump_to_top();
        let frame = draw(&mut pager, &mut cells, 80, 70);
        insta::assert_snapshot!("pager_job_and_agent_rows_80x70", &frame);
        for needle in [
            "job__check:",
            "job 01",
            "agent__check:",
            "progress 01",
            "agent__collect:",
            "verdict 02",
            "error: exit status 2",
            "stderr: no space left",
            "while writing out.bin",
        ] {
            assert!(frame.contains(needle), "{needle} is missing: {frame}");
        }
    }

    /// Each chunk streamed into the newest cell lays out that cell alone;
    /// every cell before it is taken from the last layout.
    #[test]
    fn streaming_into_the_last_cell_lays_out_only_that_cell() {
        let mut transcript = Transcript::new();
        for cell in root_cells() {
            transcript.push(cell);
        }
        transcript.open_stream(Cell::assistant(String::new(), false));
        let mut pager = Pager::root();
        let mut renderer = renderer();
        let mut lay_out = |pager: &mut Pager, transcript: &mut Transcript| {
            let generation = transcript.generation();
            let (cells, revisions) = transcript.cells_for_layout();
            pager.text(
                cells,
                revisions,
                generation,
                &mut ChildLive::default(),
                80,
                &mut renderer,
            )
        };
        lay_out(&mut pager, &mut transcript);
        assert_eq!(pager.cell_layouts, 4);
        for n in 1..=5 {
            let Some(Cell::Assistant { source, .. }) = transcript.stream_cell_mut() else {
                panic!("the stream cell is gone");
            };
            source.push_str(&format!("chunk {n} "));
            let text = lay_out(&mut pager, &mut transcript);
            assert_eq!(
                pager.cell_layouts,
                4 + n,
                "chunk {n} laid out more than one cell"
            );
            let last = text.len() - 1;
            assert!(holds(text.line(last), &format!("chunk {n}")));
        }
        lay_out(&mut pager, &mut transcript);
        assert_eq!(
            pager.cell_layouts, 9,
            "a frame with no change laid out a cell"
        );
    }

    /// A highlighted match whose line the new layout changed is looked
    /// for again from the top of the view; one still on its line stays.
    /// A match on the last page does not start following.
    #[test]
    fn a_layout_change_finds_the_match_again_and_a_match_never_follows() {
        let mut cells = root_cells();
        let mut pager = Pager::root();
        draw(&mut pager, &mut cells, 80, 24);
        pager.open_find();
        for c in "done counting".chars() {
            pager.find_char(c);
        }
        pager.submit_find();
        draw(&mut pager, &mut cells, 80, 24);
        let matched = pager.matched.unwrap();
        assert!(!pager.follow, "a match on the last page started following");
        cells.push(Cell::user("below"));
        draw(&mut pager, &mut cells, 80, 24);
        assert_eq!(pager.matched, Some(matched));
        cells.insert(0, Cell::user("above"));
        cells.insert(0, Cell::user("above"));
        draw(&mut pager, &mut cells, 80, 24);
        let moved = pager.matched.expect("the match was not found again");
        assert_eq!(moved, matched + 4);
        assert!(cells_line(&mut pager, &mut cells, moved).contains("Done counting"));
        cells.pop();
        cells[4] = Cell::assistant("Gone.".to_string(), true);
        draw(&mut pager, &mut cells, 80, 24);
        assert_eq!(pager.matched, None);
        assert!(pager.missed);
    }

    /// A paste into the find field lands on one line however its lines
    /// end.
    #[test]
    fn a_paste_into_the_find_field_drops_every_line_break() {
        let mut pager = Pager::root();
        pager.paste("ignored");
        pager.open_find();
        pager.paste("one\r\ntwo\rthree\n");
        assert_eq!(pager.field.as_deref(), Some("one  two three "));
    }

    /// A narrow pager keeps the way out and the threads key first in its
    /// legend, and a long session id loses its middle, not its end.
    #[test]
    fn a_narrow_pager_keeps_close_and_threads_and_the_ids_end() {
        let pager = Pager::root();
        assert!(
            pager
                .hint(69)
                .to_string()
                .starts_with("q close  Tab threads")
        );
        assert_eq!(pager.hint(70).to_string(), HINT);

        let mut cells = Vec::new();
        let mut pager = Pager::child(
            "reviewer_20260102_0a1b2c3d4e5f".to_string(),
            "reviewer".to_string(),
            PathBuf::from("/nonexistent/reviewer.yaml"),
        );
        let frame = draw(&mut pager, &mut cells, 50, 12);
        let title = frame.lines().next().unwrap();
        assert!(title.contains("Thread: reviewer (read-only) "), "{title}");
        assert!(title.contains("review...3d4e5f "), "{title}");
        assert!(!title.contains("_2026"), "{title}");
        assert_eq!(elide_middle("abcdefghij", 7), "ab...ij");
        assert_eq!(elide_middle("abcdefghij", 10), "abcdefghij");
        assert_eq!(elide_middle("abcdefghij", 6), "abcdefghij");
    }

    /// Too narrow for the id beside "(read-only)", the title shortens
    /// that to "(ro)" first and then keeps only the id's end.
    #[test]
    fn a_cramped_title_shortens_read_only_then_keeps_the_ids_end() {
        let pager = Pager::child(
            "reviewer_20260102_0a1b2c3d4e5f".to_string(),
            "reviewer".to_string(),
            PathBuf::from("/nonexistent/reviewer.yaml"),
        );
        assert_eq!(pager.title(36), "Thread: reviewer (read-only) re...5f");
        assert_eq!(pager.title(35), "Thread: reviewer (ro) revie...d4e5f");
        assert_eq!(pager.title(27), "Thread: reviewer (ro) ...5f");
        let pager = Pager::child(
            "rev-a1".to_string(),
            "reviewer".to_string(),
            PathBuf::from("/nonexistent/reviewer.yaml"),
        );
        assert_eq!(pager.title(35), "Thread: reviewer (read-only) rev-a1");
        assert_eq!(keep_tail("abcdefghij", 10), "abcdefghij");
        assert_eq!(keep_tail("abcdefghij", 5), "...ij");
    }

    /// A session file past the cap is refused before it is read; the
    /// file is sparse, so the test writes nothing near that size.
    #[test]
    fn a_child_file_past_the_cap_is_refused_unread() {
        let dir = crate::testing::ScratchDirGuard::new("pager-child-cap");
        let file = dir.path.join("huge.yaml");
        std::fs::File::create(&file)
            .unwrap()
            .set_len(CHILD_FILE_LIMIT + 1)
            .unwrap();
        let ctx = RequestContext::new(
            Arc::new(crate::config::AppState::test_default()),
            crate::config::WorkingMode::Repl,
        );
        let error = load_child(&file, &ctx).unwrap_err().to_string();
        assert_eq!(
            error,
            format!(
                "{} holds {} bytes, more than the {CHILD_FILE_LIMIT} the transcript view reads",
                file.display(),
                CHILD_FILE_LIMIT + 1
            )
        );
    }

    #[test]
    fn the_pager_clamps_to_tiny_and_empty_areas() {
        let mut cells = root_cells();
        let mut pager = Pager::root();
        let folds = HashSet::new();
        let threads = Threads::new(thread_tree(), Some("no children".to_string()), None, &folds);
        let mut renderer = renderer();
        for area in [
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 5, 2),
        ] {
            let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
            let revisions = fresh_revisions(&cells);
            let text = pager.text(
                &mut cells,
                &revisions,
                fresh_generation(),
                &mut ChildLive::default(),
                area.width,
                &mut renderer,
            );
            pager.settle(&text, Pager::text_rows(area.height));
            let theme = Theme::fallback(renderer.no_color);
            PagerView {
                pager: &pager,
                text: &text,
                theme: &theme,
            }
            .render(area, &mut buf);
            ThreadsPopup {
                threads: &threads,
                folds: &folds,
                theme: &theme,
                now: Utc::now(),
                offset: FixedOffset::east_opt(0).unwrap(),
            }
            .render(text_area(area), &mut buf);
        }
    }

    #[test]
    fn session_messages_become_entries_without_the_system_prompt() {
        use crate::client::MessageContentToolCalls;
        use crate::function::{ToolCall, ToolResult};
        let call = ToolCall::new(
            "fs_read".to_string(),
            serde_json::json!({"path": "a"}),
            Some("c1".to_string()),
        );
        let messages = vec![
            Message::new(
                MessageRole::System,
                MessageContent::Text("be terse".to_string()),
            ),
            Message::new(MessageRole::User, MessageContent::Text("hi".to_string())),
            Message::new(
                MessageRole::Assistant,
                MessageContent::ToolCalls(MessageContentToolCalls::new(
                    vec![ToolResult::new(call, Value::String("body".to_string()))],
                    "Looking.".to_string(),
                )),
            ),
            Message::new(
                MessageRole::Assistant,
                MessageContent::Text("done".to_string()),
            ),
        ];
        assert_eq!(
            entries(&messages),
            vec![
                ChildEntry::User("hi".to_string()),
                ChildEntry::Assistant("Looking.".to_string()),
                ChildEntry::Tool {
                    call_id: "c1".to_string(),
                    name: "fs_read".to_string(),
                    args: "{\"path\":\"a\"}".to_string(),
                    output: "body".to_string(),
                },
                ChildEntry::Assistant("done".to_string()),
            ]
        );
    }

    #[test]
    fn each_rounds_narration_comes_before_its_tool() {
        use crate::client::MessageContentToolCalls;
        use crate::function::{ToolCall, ToolResult};
        let result = |name: &str| {
            let call = ToolCall::new(name.to_string(), serde_json::json!({}), None);
            ToolResult::new(call, Value::String(format!("{name} out")))
        };
        let mut calls = MessageContentToolCalls::new(vec![], String::new());
        calls.merge(vec![result("first")], "Round one.".to_string());
        calls.merge(vec![result("second")], "Round two.".to_string());
        let messages = vec![Message::new(
            MessageRole::Assistant,
            MessageContent::ToolCalls(calls),
        )];
        let tool = |name: &str| ChildEntry::Tool {
            call_id: String::new(),
            name: name.to_string(),
            args: "{}".to_string(),
            output: format!("{name} out"),
        };
        assert_eq!(
            entries(&messages),
            vec![
                ChildEntry::Assistant("Round one.".to_string()),
                tool("first"),
                ChildEntry::Assistant("Round two.".to_string()),
                tool("second"),
            ]
        );
    }
}
