//! The pager: Ctrl+T's full-screen read of one thread. The root thread is
//! the transcript with every tool, job and agent row open on everything
//! its calls printed; a child agent's thread is its saved session, shown
//! read-only and read again each time the child writes it. A find field
//! and a thread picker sit on top of the text.

use super::bottom_pane::{render_header, render_rows, step_highlight};
use super::cells::{Cell, LineCache, short_args, without_think_spans};
use super::keys::PagerMode;
use super::render::{self, Renderer};
use super::{ChromeKind, DIM, chrome_style};
use crate::client::{Message, MessageContent, MessageContentPart, MessageRole};
use crate::config::child_sessions::{ChildMeta, EffectiveStatus};
use crate::config::{RequestContext, Session};
use anyhow::{Context, Result, bail};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const HINT: &str = "j/k scroll  g/G top/end  / find  n/N next/prev  Tab threads  q close";
const PICKER_HEADER: &str = "Threads  Enter = open  Esc = back";
const CHECKPOINT_NOTE: &str =
    "Read-only. Rounds inside a tool loop show up at the child's next checkpoint.";
const ROOT_THREAD: &str = "root thread";
const COMPRESSED_DIVIDER: &str = "(earlier rounds, compressed)";
/// The largest child session file the pager reads; a bigger one is
/// reported rather than parsed.
const CHILD_FILE_LIMIT: u64 = 16 * 1024 * 1024;

const BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
const REVERSED: Style = Style::new().add_modifier(Modifier::REVERSED);

/// One message of a child's session as the pager shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChildEntry {
    /// Heads the rounds the child's session compressed away.
    Compressed,
    User(String),
    Assistant(String),
    Tool {
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
                entries.extend(calls.tool_results.iter().map(|result| {
                    ChildEntry::Tool {
                        name: result.call.name.clone(),
                        args: result.call.arguments.to_string(),
                        output: match &result.output {
                            Value::String(text) => text.clone(),
                            other => serde_json::to_string_pretty(other)
                                .unwrap_or_else(|_| other.to_string()),
                        },
                    }
                }));
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
            "{} holds {size} bytes, more than the {CHILD_FILE_LIMIT} the pager reads",
            file.display()
        );
    }
    let session = Session::load_from_path(file, ctx.model.clone(), ctx, &ctx.app.config)?;
    let mut shown = entries(session.compressed_messages());
    if !shown.is_empty() {
        shown.insert(0, ChildEntry::Compressed);
    }
    shown.extend(entries(session.messages()));
    Ok(shown)
}

fn entry_lines(entry: &ChildEntry, width: u16, renderer: &mut Renderer) -> Vec<Line<'static>> {
    match entry {
        ChildEntry::Compressed => render::plain_lines(COMPRESSED_DIVIDER, width, DIM),
        ChildEntry::User(text) => render::plain_lines(&format!("> {text}"), width, BOLD),
        ChildEntry::Assistant(text) => renderer.markdown(without_think_spans(text, true), width),
        ChildEntry::Tool { name, args, output } => {
            let mut lines =
                render::plain_lines(&format!("{name}({})", short_args(args)), width, DIM);
            lines.extend(
                render::hard_wrap(render::preformatted_lines(output), width)
                    .into_iter()
                    .map(|line| line.patch_style(DIM)),
            );
            lines
        }
    }
}

fn status_word(status: EffectiveStatus) -> &'static str {
    match status {
        EffectiveStatus::Running => "running",
        EffectiveStatus::Completed => "completed",
        EffectiveStatus::Failed => "failed",
        EffectiveStatus::Cancelled => "cancelled",
        EffectiveStatus::Crashed => "crashed",
    }
}

/// A row of the thread picker: the root thread, or one child session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ThreadRow {
    pub(super) session_id: Option<String>,
    pub(super) agent: String,
    status: String,
}

impl ThreadRow {
    pub(super) fn root() -> Self {
        Self {
            session_id: None,
            agent: ROOT_THREAD.to_string(),
            status: String::new(),
        }
    }

    pub(super) fn child(meta: &ChildMeta, status: EffectiveStatus) -> Self {
        Self {
            session_id: Some(meta.session_id.clone()),
            agent: meta.agent.clone(),
            status: status_word(status).to_string(),
        }
    }

    fn label(&self) -> String {
        let label = match &self.session_id {
            Some(session_id) => format!("{}  {session_id}", self.agent),
            None => self.agent.clone(),
        };
        render::plain_text(&label)
    }
}

struct Picker {
    rows: Vec<ThreadRow>,
    highlight: usize,
    /// Said above the rows: why there are no children, or why they
    /// could not be listed.
    note: Option<String>,
}

pub(super) struct ChildView {
    session_id: String,
    agent: String,
    file: PathBuf,
    /// None until the first read comes back. A failed read keeps what the
    /// last good one gave and says why next to it.
    entries: Option<Vec<ChildEntry>>,
    error: Option<String>,
    cache: LineCache,
    /// The read under way, by generation. At most one runs at a time.
    loading: Option<u64>,
    /// The child wrote its file again while a read was under way: one
    /// more read follows it.
    stale: bool,
}

enum Thread {
    Root,
    Child(ChildView),
}

/// Where the view lands at the next frame, once the lines it is made of
/// are known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Jump {
    Top,
    Bottom,
    Cell(usize),
    Match { forward: bool },
}

/// The text of a thread laid out at one width, and the line each root
/// cell starts on.
pub(super) struct PagerText {
    lines: Vec<Line<'static>>,
    cell_starts: Vec<(usize, usize)>,
}

pub(super) struct Pager {
    thread: Thread,
    top: usize,
    jump: Option<Jump>,
    /// Set when the last frame ended on the last line: a thread that
    /// grows keeps its newest line in view until the user scrolls up.
    follow: bool,
    /// The find field while it is open.
    field: Option<String>,
    query: String,
    matched: Option<usize>,
    /// The last find found nothing.
    missed: bool,
    picker: Option<Picker>,
    /// Text rows in the last frame; what a page is.
    page: usize,
    /// The text as last laid out, with the width and the transcript
    /// generation it was laid out for; reused until either moves.
    laid_out: Option<(u16, u64, Arc<PagerText>)>,
}

impl Pager {
    fn new(thread: Thread, jump: Jump) -> Self {
        Self {
            thread,
            top: 0,
            jump: Some(jump),
            follow: false,
            field: None,
            query: String::new(),
            matched: None,
            missed: false,
            picker: None,
            page: 1,
            laid_out: None,
        }
    }

    /// The root thread from its newest line.
    pub(super) fn root() -> Self {
        Self::new(Thread::Root, Jump::Bottom)
    }

    /// The root thread from the row of transcript cell `cell`.
    pub(super) fn root_at(cell: usize) -> Self {
        Self::new(Thread::Root, Jump::Cell(cell))
    }

    /// A child's thread, empty until its first read comes back.
    pub(super) fn child(session_id: String, agent: String, file: PathBuf) -> Self {
        Self::new(
            Thread::Child(ChildView {
                session_id,
                agent,
                file,
                entries: None,
                error: None,
                cache: LineCache::default(),
                loading: None,
                stale: false,
            }),
            Jump::Bottom,
        )
    }

    pub(super) fn mode(&self) -> PagerMode {
        if self.picker.is_some() {
            PagerMode::Picker
        } else if self.field.is_some() {
            PagerMode::Find
        } else {
            PagerMode::Browse
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
                view.entries = Some(entries);
                view.error = None;
            }
            Err(error) => view.error = Some(error),
        }
        view.cache = LineCache::default();
        let again = std::mem::take(&mut view.stale).then(|| view.file.clone());
        self.laid_out = None;
        again
    }

    pub(super) fn scroll(&mut self, delta: i32) {
        self.top = self
            .top
            .saturating_add_signed(isize::try_from(delta).unwrap_or(0));
        self.follow = false;
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
            field.push_str(&text.replace('\n', " "));
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
    /// searched for.
    pub(super) fn find_next(&mut self, forward: bool) {
        if self.query.is_empty() {
            self.missed = false;
            return;
        }
        self.jump = Some(Jump::Match { forward });
        self.follow = false;
    }

    pub(super) fn open_picker(&mut self, rows: Vec<ThreadRow>, note: Option<String>) {
        let open = match &self.thread {
            Thread::Root => None,
            Thread::Child(view) => Some(view.session_id.as_str()),
        };
        let highlight = rows
            .iter()
            .position(|row| row.session_id.as_deref() == open)
            .unwrap_or(0);
        self.picker = Some(Picker {
            rows,
            highlight,
            note,
        });
    }

    pub(super) fn move_picker(&mut self, delta: i32) {
        if let Some(picker) = self.picker.as_mut() {
            picker.highlight = step_highlight(picker.highlight, delta, picker.rows.len());
        }
    }

    pub(super) fn close_picker(&mut self) {
        self.picker = None;
    }

    /// Closes the picker on its highlighted row.
    pub(super) fn take_picked(&mut self) -> Option<ThreadRow> {
        let picker = self.picker.take()?;
        picker.rows.into_iter().nth(picker.highlight)
    }

    /// Rows of text a frame `height` rows tall leaves between the title
    /// and the hint; a frame too short for them is all text.
    pub(super) fn text_rows(height: u16) -> usize {
        usize::from(if height >= 3 { height - 2 } else { height })
    }

    /// The thread laid out at `width`. The root thread is laid out again
    /// only when the transcript's `generation` moved; a child's when a
    /// read of it came back.
    pub(super) fn text(
        &mut self,
        cells: &mut [Cell],
        generation: u64,
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
        let text = Arc::new(self.lay_out(cells, width, renderer));
        self.laid_out = Some((width, generation, Arc::clone(&text)));
        text
    }

    fn lay_out(&mut self, cells: &mut [Cell], width: u16, renderer: &mut Renderer) -> PagerText {
        let mut lines = Vec::new();
        let mut cell_starts = Vec::new();
        match &mut self.thread {
            Thread::Root => {
                for (index, cell) in cells.iter_mut().enumerate() {
                    if matches!(cell, Cell::Welcome { .. }) {
                        continue;
                    }
                    if !lines.is_empty() {
                        lines.push(Line::default());
                    }
                    cell_starts.push((index, lines.len()));
                    lines.extend(cell.full_lines(width, renderer));
                }
                if lines.is_empty() {
                    lines.push(Line::from(Span::styled("(nothing yet)", DIM)));
                }
            }
            Thread::Child(view) => {
                let no_color = renderer.no_color;
                lines.extend(render::plain_lines(CHECKPOINT_NOTE, width, DIM));
                if let Some(error) = &view.error {
                    lines.extend(render::plain_lines(
                        &format!("Cannot read this transcript: {error}"),
                        width,
                        chrome_style(ChromeKind::Error, no_color),
                    ));
                }
                let ChildView { entries, cache, .. } = view;
                match entries {
                    Some(entries) => {
                        lines.extend_from_slice(cache.get(width, || {
                            let mut lines = Vec::new();
                            for entry in entries.iter() {
                                lines.push(Line::default());
                                lines.extend(entry_lines(entry, width, renderer));
                            }
                            lines
                        }));
                    }
                    None if view.error.is_none() => {
                        lines.push(Line::from(Span::styled("Loading...", DIM)));
                    }
                    None => {}
                }
            }
        }
        PagerText { lines, cell_starts }
    }

    /// Resolves the pending jump against the laid-out text and keeps the
    /// view inside it.
    pub(super) fn settle(&mut self, text: &PagerText, rows: usize) {
        self.page = rows.max(1);
        let total = text.lines.len();
        let last_top = total.saturating_sub(rows);
        match self.jump.take() {
            Some(Jump::Top) => self.top = 0,
            Some(Jump::Bottom) => self.top = last_top,
            Some(Jump::Cell(cell)) => {
                self.top = text
                    .cell_starts
                    .iter()
                    .find(|(index, _)| *index == cell)
                    .map_or(last_top, |(_, line)| *line);
            }
            Some(Jump::Match { forward }) => {
                self.matched = self.find(&text.lines, forward);
                self.missed = self.matched.is_none();
                if let Some(line) = self.matched {
                    self.top = line;
                }
            }
            None if self.follow => self.top = last_top,
            None => {}
        }
        self.top = self.top.min(last_top);
        self.follow = self.top == last_top;
    }

    /// The line of the next match after the current one (or before it),
    /// wrapping round; the first search starts at the top of the view.
    fn find(&self, lines: &[Line<'static>], forward: bool) -> Option<usize> {
        let total = lines.len();
        if total == 0 || self.query.is_empty() {
            return None;
        }
        let needle = self.query.to_lowercase();
        let origin = match (self.matched, forward) {
            (Some(line), true) => line + 1,
            (Some(line), false) => line + total - 1,
            (None, true) => self.top,
            (None, false) => self.top + total - 1,
        };
        (0..total)
            .map(|step| {
                if forward {
                    (origin + step) % total
                } else {
                    (origin + total - step) % total
                }
            })
            .find(|&index| lines[index].to_string().to_lowercase().contains(&needle))
    }

    fn title(&self) -> String {
        let title = match &self.thread {
            Thread::Root => format!("Thread: {ROOT_THREAD}"),
            Thread::Child(view) => {
                format!("Thread: {} (read-only) {}", view.agent, view.session_id)
            }
        };
        render::plain_text(&title)
    }

    fn hint(&self) -> Line<'static> {
        if let Some(field) = &self.field {
            return Line::from(format!("/{}", render::plain_text(field)));
        }
        if self.missed {
            return Line::from(Span::styled(
                format!("No match for \"{}\"", render::plain_text(&self.query)),
                DIM,
            ));
        }
        Line::from(Span::styled(HINT, DIM))
    }
}

/// The pager over the whole frame: the thread's name and the position in
/// it, the text, and the key legend or the find field.
pub(super) struct PagerView<'a> {
    pub(super) pager: &'a Pager,
    pub(super) text: &'a PagerText,
}

impl Widget for PagerView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(*buf.area());
        if area.is_empty() {
            return;
        }
        let pager = self.pager;
        let lines = &self.text.lines;
        let rows = Pager::text_rows(area.height);
        let chrome = area.height >= 3;
        let body = if chrome {
            Rect::new(area.x, area.y + 1, area.width, area.height - 2)
        } else {
            area
        };
        let start = pager.top.min(lines.len());
        let end = (start + rows).min(lines.len());
        if chrome {
            let title = Rect::new(area.x, area.y, area.width, 1);
            let position = format!("{}-{end}/{}", (start + 1).min(end), lines.len());
            // The name gives way to the position, which is what moves.
            let name_width = area
                .width
                .saturating_sub(u16::try_from(position.len() + 1).unwrap_or(u16::MAX));
            let name = Rect::new(area.x, area.y, name_width, 1);
            Paragraph::new(Line::from(Span::styled(pager.title(), BOLD))).render(name, buf);
            Paragraph::new(Line::from(Span::styled(position, DIM)))
                .right_aligned()
                .render(title, buf);
            let hint = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
            Paragraph::new(pager.hint()).render(hint, buf);
        }
        Paragraph::new(lines[start..end].to_vec()).render(body, buf);
        if let Some(line) = pager.matched
            && (start..end).contains(&line)
        {
            let y = body.y + u16::try_from(line - start).unwrap_or(u16::MAX);
            buf.set_style(Rect::new(body.x, y, body.width, 1), REVERSED);
        }
        if let Some(picker) = &pager.picker {
            Clear.render(body, buf);
            let mut rows_area =
                render_header(Line::from(Span::styled(PICKER_HEADER, DIM)), body, buf);
            if let Some(note) = &picker.note {
                rows_area = render_header(
                    Line::from(Span::styled(render::plain_text(note), DIM)),
                    rows_area,
                    buf,
                );
            }
            let labels: Vec<String> = picker.rows.iter().map(ThreadRow::label).collect();
            let rows: Vec<(&str, &str)> = labels
                .iter()
                .zip(&picker.rows)
                .map(|(label, row)| (label.as_str(), row.status.as_str()))
                .collect();
            render_rows(&rows, Some(picker.highlight), rows_area, buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::config::child_sessions::ChildStatus;
    use crate::tui::cells::{ToolStatus, append_output};
    use chrono::{TimeZone, Utc};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// A transcript generation no earlier call used: the tests change
    /// their cells freely between frames.
    fn fresh_generation() -> u64 {
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        GENERATION.fetch_add(1, Ordering::Relaxed)
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
            "Call execute_command {\"command\":\"seq\"}".to_string(),
            Instant::now(),
        );
        let output: String = (1..=lines).map(|n| format!("line {n:03}\n")).collect();
        let (target, full) = cell.output_for("c1").unwrap();
        full.push(&append_output(target, output.as_bytes()));
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
        let mut renderer = renderer();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                let text = pager.text(cells, fresh_generation(), area.width, &mut renderer);
                pager.settle(&text, Pager::text_rows(area.height));
                frame.render_widget(PagerView { pager, text: &text }, area);
            })
            .unwrap();
        terminal.backend().to_string()
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
        pager
            .text(cells, fresh_generation(), 80, &mut renderer())
            .lines[line]
            .to_string()
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
        pager.open_picker(
            vec![
                ThreadRow::root(),
                ThreadRow::child(&meta("reviewer-a1", "reviewer"), EffectiveStatus::Running),
                ThreadRow::child(&meta("planner-b2", "planner"), EffectiveStatus::Completed),
                ThreadRow::child(&meta("tester-c3", "tester"), EffectiveStatus::Failed),
                ThreadRow::child(&meta("fixer-d4", "fixer"), EffectiveStatus::Crashed),
                ThreadRow::child(&meta("scout-e5", "scout"), EffectiveStatus::Cancelled),
            ],
            None,
        );
        assert_eq!(pager.mode(), PagerMode::Picker);
        pager.move_picker(2);
        let frame = draw(&mut pager, &mut cells, 80, 24);
        insta::assert_snapshot!("thread_picker_80x24", &frame);
        let frame = draw(&mut pager, &mut cells, 40, 12);
        insta::assert_snapshot!("thread_picker_40x12", &frame);
        assert_eq!(
            pager.take_picked().and_then(|row| row.session_id),
            Some("planner-b2".to_string())
        );
        assert_eq!(pager.mode(), PagerMode::Browse);
    }

    fn child_entries() -> Vec<ChildEntry> {
        vec![
            ChildEntry::User("review src/lib.rs".to_string()),
            ChildEntry::Assistant("Reading the file first.".to_string()),
            ChildEntry::Tool {
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
        let rows = vec![
            ThreadRow::root(),
            ThreadRow::child(&meta("reviewer-a1", "reviewer"), EffectiveStatus::Running),
            ThreadRow::child(&meta("planner-b2", "planner"), EffectiveStatus::Running),
        ];
        pager.open_picker(rows, None);
        let picked = pager.take_picked().unwrap();
        assert_eq!(picked.session_id.as_deref(), Some("planner-b2"));
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
        let mut pager = Pager::root();
        let mut renderer = renderer();
        let first = pager.text(&mut cells, 5, 80, &mut renderer);
        let again = pager.text(&mut cells, 5, 80, &mut renderer);
        assert!(Arc::ptr_eq(&first, &again));
        let changed = pager.text(&mut cells, 6, 80, &mut renderer);
        assert!(!Arc::ptr_eq(&first, &changed));
        let narrower = pager.text(&mut cells, 6, 40, &mut renderer);
        assert!(!Arc::ptr_eq(&changed, &narrower));
    }

    #[test]
    fn the_pager_clamps_to_tiny_and_empty_areas() {
        let mut cells = root_cells();
        let mut pager = Pager::root();
        pager.open_picker(vec![ThreadRow::root()], Some("no children".to_string()));
        let mut renderer = renderer();
        for area in [
            Rect::new(0, 0, 1, 1),
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 5, 2),
        ] {
            let mut buf = Buffer::empty(Rect::new(0, 0, 1, 1));
            let text = pager.text(&mut cells, fresh_generation(), area.width, &mut renderer);
            pager.settle(&text, Pager::text_rows(area.height));
            PagerView {
                pager: &pager,
                text: &text,
            }
            .render(area, &mut buf);
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
                    name: "fs_read".to_string(),
                    args: "{\"path\":\"a\"}".to_string(),
                    output: "body".to_string(),
                },
                ChildEntry::Assistant("done".to_string()),
            ]
        );
    }
}
