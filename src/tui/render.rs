//! Turns a cell's source into ratatui lines at a given width. Markdown
//! goes through the REPL's renderer, wrapping at that width, so both
//! surfaces paint the same way; the result is scrubbed of every escape
//! that is not a colour or attribute and parsed back into styled spans. A
//! streaming reply is split into a stable part, rendered once per width,
//! and a live tail. A reply's closed fences of a kind the TUI draws
//! itself (see [`super::rich`]) are drawn in place of their source, and
//! so is its LaTeX math.

use super::rich::math_render::{self, MathRenderer, TermMathsBackend};
use super::rich::mermaid::{self, MermaidRenderer, MermaidTextBackend};
use super::rich::{
    self, Block, BlockKind, CopyRegion, Delimiter, Rendered, RichCache, RichKey, RichStyle,
    Segment, chart, math,
};
use super::throbber::Tick;
use crate::config::is_format_char;
use crate::render::{
    MarkdownRender, RenderOptions, SOFT_BREAK_SPACE, SOFT_BREAK_WORD, broke_at_space,
};
use ansi_to_tui::IntoText;
use anyhow::Result;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use std::sync::Arc;
use std::time::Instant;
use unicode_width::UnicodeWidthChar;

const ESC: char = '\x1b';

/// Keeps SGR sequences and drops everything else a terminal would act on:
/// OSC, DCS, SOS, PM and APC strings (hyperlinks, titles, terminal
/// queries), non-SGR CSI (cursor moves, erases), other escape pairs and
/// C0 controls other than newline and tab.
pub(crate) fn sanitize_ansi(text: &str) -> String {
    scrub(text, true)
}

/// Drops every escape sequence, SGR included; for text that is drawn as
/// plain lines rather than parsed for colour.
pub(crate) fn plain_text(text: &str) -> String {
    scrub(text, false)
}

/// Introducers of the control strings that run until ST (`ESC \`) or BEL:
/// OSC, DCS, SOS, PM and APC.
const STRING_INTRODUCERS: [char; 5] = [']', 'P', 'X', '^', '_'];

fn scrub(text: &str, keep_sgr: bool) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != ESC {
            if c == '\n' || c == '\t' || !(c.is_control() || is_format_char(c)) {
                out.push(c);
            }
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('[') => {
                let start = i;
                i += 2;
                // A private-marker parameter (`<`, `=`, `>`, `?`) makes
                // the sequence vendor-defined even when it ends in `m`.
                let private = matches!(chars.get(i), Some(c) if ('\x3c'..='\x3f').contains(c));
                while matches!(chars.get(i), Some(c) if ('\x30'..='\x3f').contains(c)) {
                    i += 1;
                }
                while matches!(chars.get(i), Some(c) if ('\x20'..='\x2f').contains(c)) {
                    i += 1;
                }
                // Only a final byte (0x40..=0x7e) is consumed; a sequence
                // cut short by anything else leaves that character to the
                // next round rather than swallowing it.
                let final_byte = chars
                    .get(i)
                    .copied()
                    .filter(|c| ('\x40'..='\x7e').contains(c));
                if final_byte.is_some() {
                    i += 1;
                }
                if keep_sgr && !private && final_byte == Some('m') {
                    let params: String = chars[start + 2..i - 1].iter().collect();
                    if let Some(kept) = without_conceal(&params) {
                        out.push(ESC);
                        out.push('[');
                        out.push_str(&kept);
                        out.push('m');
                    }
                }
            }
            Some(c) if STRING_INTRODUCERS.contains(c) => {
                i += 2;
                // The string runs to ST (`ESC \`) or BEL. A bare ESC ends
                // it too, as it does on a terminal: an unterminated string
                // must not swallow the rest of the text, and the sequence
                // the ESC starts is handled by the next round.
                loop {
                    match chars.get(i) {
                        None => break,
                        Some('\x07') => {
                            i += 1;
                            break;
                        }
                        Some(&ESC) => {
                            if chars.get(i + 1) == Some(&'\\') {
                                i += 2;
                            }
                            break;
                        }
                        Some(_) => i += 1,
                    }
                }
            }
            Some(c) if ('\x20'..='\x2f').contains(c) => i += 3,
            Some(_) => i += 2,
            None => i += 1,
        }
    }
    out
}

/// The SGR parameter list without conceal (8); None when nothing but
/// conceal was asked for. The arguments of an extended colour (`38;5;n`,
/// `38;2;r;g;b` and the 48/58 twins) are carried over untouched, so a
/// colour index of 8 is not mistaken for the attribute. Parameters are
/// compared as numbers, so a leading zero (`08`) is conceal as well.
fn without_conceal(params: &str) -> Option<String> {
    let is_conceal = |p: &str| p.parse::<u32>() == Ok(8);
    if !params.split(';').any(is_conceal) {
        return Some(params.to_string());
    }
    let parts: Vec<&str> = params.split(';').collect();
    let mut kept: Vec<&str> = Vec::with_capacity(parts.len());
    let mut i = 0;
    while i < parts.len() {
        let part = parts[i];
        let span = match part.parse::<u32>() {
            Ok(38 | 48 | 58) => match parts.get(i + 1).and_then(|p| p.parse::<u32>().ok()) {
                Some(5) => 3,
                Some(2) => 5,
                _ => 1,
            },
            Ok(8) => {
                i += 1;
                continue;
            }
            _ => 1,
        };
        let end = (i + span).min(parts.len());
        kept.extend_from_slice(&parts[i..end]);
        i = end;
    }
    if kept.is_empty() {
        None
    } else {
        Some(kept.join(";"))
    }
}

/// Parses SGR-styled text into lines; falls back to the bare characters
/// when the parser rejects the input. Conceal (SGR 8) is dropped: text a
/// model or a tool painted invisible is still text the user is shown. A
/// reset background (what SGR 0 and 49 leave) is dropped too, so the
/// text shows the surface it is drawn on rather than the terminal's own
/// background; a colour a tool painted on purpose stays.
fn styled_lines(ansi: &str) -> Vec<Line<'static>> {
    if ansi.is_empty() {
        return Vec::new();
    }
    let clean = |style: Style| {
        let mut style = style.remove_modifier(Modifier::HIDDEN);
        if style.bg == Some(Color::Reset) {
            style.bg = None;
        }
        style
    };
    match ansi.into_text() {
        Ok(text) => text
            .lines
            .into_iter()
            .map(|mut line| {
                line.style = clean(line.style);
                for span in &mut line.spans {
                    span.style = clean(span.style);
                }
                line
            })
            .collect(),
        Err(err) => {
            debug!("Falling back to plain text for a cell: {err}");
            Text::raw(scrub(ansi, false)).lines
        }
    }
}

/// The underline colours of a wrap mark: the row joins the one above with
/// a space, or with nothing.
const JOIN_WITH_SPACE: Color = Color::Indexed(1);
const JOIN_WITH_NOTHING: Color = Color::Indexed(0);

/// The empty span that leads a row a wrap continued. It draws nothing (it
/// has no text, and sets no underline for its colour to show on); the
/// colour records whether the wrap took out a space, so a copy can join
/// the row back onto the one above. A row without one starts a line.
pub(super) fn wrap_mark(at_space: bool) -> Span<'static> {
    let join = if at_space {
        JOIN_WITH_SPACE
    } else {
        JOIN_WITH_NOTHING
    };
    Span::styled("", Style::new().underline_color(join))
}

pub(super) fn is_wrap_mark(span: &Span) -> bool {
    span.content.is_empty()
        && matches!(
            span.style.underline_color,
            Some(JOIN_WITH_SPACE | JOIN_WITH_NOTHING)
        )
}

/// What joins `line` to the row above it, when a wrap continued it.
pub(super) fn wrap_join(line: &Line) -> Option<&'static str> {
    line.spans
        .iter()
        .find(|span| is_wrap_mark(span))
        .map(|span| match span.style.underline_color {
            Some(JOIN_WITH_SPACE) => " ",
            _ => "",
        })
}

/// Plain text wrapped to `width`, every span in `style`, each row a wrap
/// continued led by a [`wrap_mark`]. The text is scrubbed first: whatever
/// is drawn this way (a user line, a notice, an error, a question) carries
/// no escape or format character to the cell.
pub(crate) fn plain_lines(text: &str, width: u16, style: Style) -> Vec<Line<'static>> {
    let text = plain_text(text);
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    for line in text.split('\n') {
        let mut cursor = 0;
        for (i, piece) in textwrap::wrap(line, width).iter().enumerate() {
            let content = Span::styled(piece.to_string(), style);
            let space = broke_at_space(line, piece, &mut cursor);
            lines.push(if i == 0 {
                Line::from(content)
            } else {
                Line::from(vec![wrap_mark(space), content])
            });
        }
    }
    lines
}

/// Already-painted output (a command's table) parsed as-is: no wrapping,
/// so a table keeps its columns and the view clips what does not fit.
pub(crate) fn preformatted_lines(output: &str) -> Vec<Line<'static>> {
    styled_lines(&sanitize_ansi(output.trim_end_matches(['\n', '\r'])))
}

/// Breaks every line wider than `width` at the column boundary, span
/// styles kept and each row after the first led by a [`wrap_mark`], so
/// nothing the view would clip goes unseen. A tool call's narration is
/// approved on what is shown; a long argument must not hide its tail past
/// the right edge.
pub(crate) fn hard_wrap(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if line.width() <= width {
            out.push(line);
            continue;
        }
        let Line { spans, style, .. } = line;
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        for span in spans {
            if is_wrap_mark(&span) {
                row.push(span);
                continue;
            }
            let mut piece = String::new();
            for c in span.content.chars() {
                let columns = c.width().unwrap_or(0);
                if used + columns > width && used > 0 {
                    if !piece.is_empty() {
                        row.push(Span::styled(std::mem::take(&mut piece), span.style));
                    }
                    out.push(Line::from(std::mem::take(&mut row)).style(style));
                    row.push(wrap_mark(false));
                    used = 0;
                }
                piece.push(c);
                used += columns;
            }
            if !piece.is_empty() {
                row.push(Span::styled(piece, span.style));
            }
        }
        if !row.is_empty() {
            out.push(Line::from(row).style(style));
        }
    }
    out
}

/// The first line of already-painted text; the footer's usage readout
/// comes from the REPL's right prompt with its colours still in.
pub(crate) fn styled_line(ansi: &str) -> Line<'static> {
    preformatted_lines(ansi)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// The REPL's markdown renderer kept warm between frames.
pub(crate) struct Renderer {
    markdown: MarkdownRender,
    /// The user's `wrap` cap in columns, when it is a number; `auto` and
    /// `no` leave the pane width to decide alone.
    wrap_cap: Option<u16>,
    /// `NO_COLOR` at start: the UI's own colours are dropped while the
    /// attributes stay. Tests flip it through `App::set_no_color`.
    pub(super) no_color: bool,
    /// What faded text in the cells is drawn with: the theme's, which the
    /// App sets each time it builds one.
    pub(super) faded: Style,
    /// The theme's accent, which the welcome wordmark is painted in; set
    /// with `faded`.
    pub(super) accent: super::theme::Gradient,
    /// The instant the frame being laid out is drawn for: what a running
    /// row's throbber and elapsed time read. The App sets it each frame.
    pub(super) tick: Tick,
    /// Whether a reply's rich blocks are drawn, or shown as their source;
    /// Alt+M flips it.
    pub(super) rich: bool,
    /// The rows a chart takes unless its spec says otherwise; the App sets
    /// it from the `tui_chart_height` setting.
    pub(super) chart_height: u16,
    mermaid: Arc<dyn MermaidRenderer>,
    math: Arc<dyn MathRenderer>,
    rich_cache: RichCache,
}

impl Renderer {
    /// The width is set per call; `init` would otherwise size it from fd 1.
    pub(crate) fn new(options: RenderOptions) -> Result<Self> {
        let wrap_cap = options
            .wrap
            .as_deref()
            .and_then(|value| value.parse::<u16>().ok());
        let now = Instant::now();
        let mut markdown = MarkdownRender::init(RenderOptions {
            wrap: None,
            ..options
        })?;
        markdown.mark_soft_breaks();
        Ok(Self {
            markdown,
            wrap_cap,
            no_color: *crate::utils::NO_COLOR,
            faded: super::theme::DIM,
            accent: super::theme::Gradient::PLAIN,
            tick: Tick { origin: now, now },
            rich: true,
            chart_height: chart::DEFAULT_HEIGHT,
            mermaid: Arc::new(MermaidTextBackend),
            math: Arc::new(TermMathsBackend),
            rich_cache: RichCache::default(),
        })
    }

    pub(super) fn markdown_render(&self) -> &MarkdownRender {
        &self.markdown
    }

    /// Drops every drawn block, so the next frame draws them in the
    /// colours set since.
    pub(super) fn forget_rich_blocks(&mut self) {
        self.rich_cache = RichCache::default();
    }

    /// Swaps the mermaid backend, dropping the blocks the old one drew.
    #[cfg(test)]
    pub(super) fn set_mermaid_backend(&mut self, backend: Arc<dyn MermaidRenderer>) {
        self.mermaid = backend;
        self.rich_cache = RichCache::default();
    }

    /// Swaps the math backend, dropping the blocks the old one drew.
    #[cfg(test)]
    pub(super) fn set_math_backend(&mut self, backend: Arc<dyn MathRenderer>) {
        self.math = backend;
        self.rich_cache = RichCache::default();
    }

    /// A reply laid out at `width`: with `rich`, its rich blocks drawn and
    /// its inline math drawn, and the rows and columns they take recorded
    /// for the copy; without, all of it as markdown source.
    pub(crate) fn render_reply(&mut self, source: &str, width: u16, rich: bool) -> Rendered {
        if !rich {
            return Rendered::plain(self.markdown_run(source, width));
        }
        let mut rendered = Rendered::default();
        for segment in rich::segment(source) {
            match segment {
                Segment::Markdown(run) => rendered.append(self.prose_run(run, width)),
                Segment::Block(block) => rendered.append(self.rich_block(&block, width)),
            }
        }
        rendered
    }

    /// A markdown run with each inline formula that draws on one row drawn
    /// in place of its source.
    fn prose_run(&mut self, run: &str, width: u16) -> Rendered {
        let room = self.wrap_cap.map_or(width, |cap| width.min(cap)).max(1);
        let backend = Arc::clone(&self.math);
        let (masked, formulas) = math::mask_inline(run, room, backend.as_ref());
        let (lines, regions) = math::unmask(self.markdown_run(&masked, width), &formulas);
        Rendered { lines, regions }
    }

    /// Markdown with every block shown as source: what the transcript
    /// view, a child's thread and a question draw, so what they show and
    /// copy is the text the model wrote.
    pub(crate) fn markdown_source(&mut self, source: &str, width: u16) -> Vec<Line<'static>> {
        self.render_reply(source, width, false).lines
    }

    /// A block drawn at `width`, from the cache when it was drawn before.
    /// One that cannot be drawn is its source, with a faded note under it
    /// that a copy leaves out: a fence highlighted as in a markdown run,
    /// and display math as its literal text, since markdown would read a
    /// `*` or `_` in the TeX as emphasis. Either way its rows copy as the
    /// source.
    fn rich_block(&mut self, block: &Block, width: u16) -> Rendered {
        let width = self.wrap_cap.map_or(width, |cap| width.min(cap)).max(1);
        let key = RichKey::new(block, width, self.chart_height, self.no_color);
        let drawn = match self.rich_cache.get(&key) {
            Some(drawn) => drawn,
            None => {
                let drawn = Arc::new(self.draw_block(block, width));
                self.rich_cache.insert(key, Arc::clone(&drawn));
                drawn
            }
        };
        match &*drawn {
            Ok(lines) => Rendered {
                lines: lines.clone(),
                regions: vec![CopyRegion::Block {
                    rows: 0..lines.len(),
                    source: block.copy_source(),
                }],
            },
            Err(note) => {
                let mut lines = match block.kind {
                    BlockKind::DisplayMath => plain_lines(block.fenced, width, Style::new()),
                    BlockKind::Mermaid | BlockKind::Chart => self.markdown_run(block.fenced, width),
                };
                let row = lines.len();
                lines.push(rich::note_line(note, self.faded));
                Rendered {
                    lines,
                    regions: vec![
                        CopyRegion::Block {
                            rows: 0..row,
                            source: block.copy_source(),
                        },
                        CopyRegion::Block {
                            rows: row..row + 1,
                            source: Arc::from(""),
                        },
                    ],
                }
            }
        }
    }

    fn draw_block(&self, block: &Block, width: u16) -> rich::BlockResult {
        match block.kind {
            BlockKind::Mermaid => mermaid::render_mermaid(self.mermaid.as_ref(), block.body, width)
                .map(|rows| rows.into_iter().map(Line::from).collect())
                .map_err(|err| err.note()),
            BlockKind::Chart => {
                chart::render_chart(block.body, width, self.chart_height, &self.rich_style())
                    .map_err(|err| err.note())
            }
            BlockKind::DisplayMath => {
                math_render::math_rows(self.math.as_ref(), block.body.trim(), width)
                    .map(|rows| {
                        rows.into_iter()
                            .map(|row| Line::from(format!("  {row}")))
                            .collect()
                    })
                    .map_err(|err| err.note())
            }
        }
    }

    /// The theme's colours for a drawn block: the heading colour, then the
    /// two ends of the accent, for a chart's series in turn.
    fn rich_style(&self) -> RichStyle {
        let heading = super::theme::heading_color(&self.markdown);
        let accent = |column| self.accent.style_at(column, 2, &self.markdown).fg;
        let mut palette = vec![heading];
        palette.extend([accent(0), accent(1)].into_iter().flatten());
        palette.extend([Color::Cyan, Color::Magenta]);
        RichStyle {
            palette,
            title: Style::new().fg(heading).add_modifier(Modifier::BOLD),
            axis: self.faded,
            no_color: self.no_color,
        }
    }

    /// Markdown laid out at `width` by the renderer itself: it wraps prose
    /// and lists on word boundaries and sizes tables to fit, where wrapping
    /// its painted output afterwards would break border rows mid-cell. Code
    /// lines follow the `wrap_code` option and are otherwise clipped by the
    /// view. A numeric `wrap` setting caps the width below the pane's, as
    /// it caps the REPL's below the terminal's. Each row a wrap continued
    /// is led by a [`wrap_mark`].
    fn markdown_run(&mut self, source: &str, width: u16) -> Vec<Line<'static>> {
        if source.is_empty() {
            return Vec::new();
        }
        let width = self.wrap_cap.map_or(width, |cap| width.min(cap));
        self.markdown.set_wrap_width(Some(width.max(1)));
        // The soft-break characters mean a wrap only when the renderer
        // put them there.
        let source = source.replace([SOFT_BREAK_SPACE, SOFT_BREAK_WORD], "");
        let source = source.as_str();
        let mut ansi = self.markdown.render(source);
        let pending_table = self.markdown.finalize();
        if !pending_table.is_empty() {
            if !ansi.is_empty() {
                ansi.push('\n');
            }
            ansi.push_str(&pending_table);
        }
        // The renderer remembers an open fence across calls; closing it
        // keeps the next call from highlighting prose as code.
        if fence_open(source) {
            self.markdown.render("```");
        }
        styled_lines(&sanitize_ansi(&ansi))
            .into_iter()
            .map(take_soft_break)
            .collect()
    }
}

/// Swaps the soft-break character the markdown renderer put in a row it
/// wrapped for a [`wrap_mark`] in the same place: after the hanging indent
/// or quote bar the row repeats, so the copy can tell that prefix from text.
fn take_soft_break(line: Line<'static>) -> Line<'static> {
    let breaks = [SOFT_BREAK_SPACE, SOFT_BREAK_WORD];
    if !line.spans.iter().any(|span| span.content.contains(breaks)) {
        return line;
    }
    let mut spans = Vec::with_capacity(line.spans.len() + 2);
    for span in line.spans {
        let content = span.content.as_ref();
        let Some((at, mark)) = content.char_indices().find(|(_, c)| breaks.contains(c)) else {
            spans.push(span);
            continue;
        };
        let before = &content[..at];
        let after = content[at + mark.len_utf8()..].replace(breaks, "");
        if !before.is_empty() {
            spans.push(Span::styled(before.to_string(), span.style));
        }
        spans.push(wrap_mark(mark == SOFT_BREAK_SPACE));
        if !after.is_empty() {
            spans.push(Span::styled(after, span.style));
        }
    }
    Line { spans, ..line }
}

fn is_fence(line: &str) -> bool {
    matches!(rich::block_delimiter(line), Some(Delimiter::Fence { .. }))
}

fn fence_open(source: &str) -> bool {
    source.split('\n').filter(|line| is_fence(line)).count() % 2 == 1
}

/// The whole line that opened a code fence `source` leaves open, if any.
pub(super) fn open_fence_line(source: &str) -> Option<&str> {
    let mut open = None;
    for line in source.split_inclusive('\n') {
        if line.ends_with('\n') && is_fence(line) {
            open = if open.is_some() { None } else { Some(line) };
        }
    }
    open
}

/// Byte offset where the live tail of a streaming reply begins: the last
/// blank line outside a code fence or display math. Everything before it
/// is complete paragraphs that render the same on every frame.
pub(crate) fn split_stable_tail(source: &str) -> usize {
    let mut split = 0;
    let mut offset = 0;
    let mut scan = rich::BlockScan::default();
    for line in source.split_inclusive('\n') {
        scan.line(line, offset);
        if !scan.is_open() && line.trim().is_empty() && line.ends_with('\n') {
            split = offset;
        }
        offset += line.len();
    }
    split
}

struct StableRender {
    width: u16,
    rich: bool,
    stable_len: usize,
    rendered: Rendered,
}

/// One reply rendered with its rich blocks drawn or shown as source, the
/// stable part cached per width and mode. The two most recently used
/// widths of each mode stay cached so a resize and back costs nothing,
/// and the pager's source view never evicts the transcript's drawn one.
#[derive(Default)]
pub(crate) struct RenderCache {
    entries: Vec<StableRender>,
}

impl RenderCache {
    /// A reply rendered: the cached stable part copied, with a streaming
    /// tail past it rendered anew.
    pub(crate) fn rendered(
        &mut self,
        source: &str,
        done: bool,
        width: u16,
        rich: bool,
        renderer: &mut Renderer,
    ) -> Rendered {
        let split = if done {
            source.len()
        } else {
            split_stable_tail(source)
        };
        let mut rendered = self.stable(source, split, width, rich, renderer).clone();
        if split < source.len() {
            rendered.append(renderer.render_reply(&source[split..], width, rich));
        }
        rendered
    }

    /// A finished reply rendered, borrowed from the cache: the whole
    /// source is its stable part.
    pub(crate) fn finished(
        &mut self,
        source: &str,
        width: u16,
        rich: bool,
        renderer: &mut Renderer,
    ) -> &Rendered {
        self.stable(source, source.len(), width, rich, renderer)
    }

    fn stable(
        &mut self,
        source: &str,
        split: usize,
        width: u16,
        rich: bool,
        renderer: &mut Renderer,
    ) -> &Rendered {
        let found = self
            .entries
            .iter()
            .position(|e| e.width == width && e.rich == rich);
        let entry = match found {
            Some(index) => self.entries.remove(index),
            None => {
                if self.entries.iter().filter(|e| e.rich == rich).count() >= 2 {
                    let oldest = self.entries.iter().position(|e| e.rich == rich);
                    if let Some(oldest) = oldest {
                        self.entries.remove(oldest);
                    }
                }
                StableRender {
                    width,
                    rich,
                    stable_len: usize::MAX,
                    rendered: Rendered::default(),
                }
            }
        };
        self.entries.push(entry);
        let index = self.entries.len() - 1;
        let entry = &mut self.entries[index];
        if entry.stable_len != split {
            entry.rendered = renderer.render_reply(&source[..split], width, rich);
            entry.stable_len = split;
        }
        &entry.rendered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::rich::math_render::FakeMath;
    use crate::tui::rich::mermaid::{FakeMermaid, MermaidError};
    use crate::utils::decode_bin;
    use syntect::highlighting::Theme;

    const DARK_THEME: &[u8] = include_bytes!("../../assets/monokai-extended.theme.bin");

    fn every_span<'a>(lines: &'a [Line<'a>]) -> impl Iterator<Item = &'a Span<'a>> {
        lines.iter().flat_map(|line| line.spans.iter())
    }

    /// A numeric `wrap` caps prose below the pane width the way it caps
    /// the REPL below the terminal; `auto` and `no` do not.
    #[test]
    fn a_numeric_wrap_setting_caps_the_pane_width() {
        let prose = "word ".repeat(30);
        let widest = |lines: &[Line<'_>]| lines.iter().map(Line::width).max().unwrap_or(0);
        let mut capped = Renderer::new(RenderOptions {
            wrap: Some("30".to_string()),
            ..RenderOptions::default()
        })
        .unwrap();
        assert!(widest(&capped.markdown_source(&prose, 80)) <= 30);
        assert!(widest(&capped.markdown_source(&prose, 20)) <= 20);
        for setting in ["auto", "no"] {
            let mut uncapped = Renderer::new(RenderOptions {
                wrap: Some(setting.to_string()),
                ..RenderOptions::default()
            })
            .unwrap();
            assert!(
                widest(&uncapped.markdown_source(&prose, 80)) > 30,
                "{setting}"
            );
        }
    }

    #[test]
    fn sanitize_keeps_sgr_and_drops_hyperlinks_and_cursor_moves() {
        let input =
            "\x1b]8;;https://example.com\x07link\x1b]8;;\x07 \x1b[2Aup \x1b[1mbold\x1b[0m\r\x07";
        assert_eq!(sanitize_ansi(input), "link up \x1b[1mbold\x1b[0m");
        assert_eq!(sanitize_ansi("\x1b(Bx\x1b=y\tz\n"), "xy\tz\n");
        assert_eq!(sanitize_ansi("\x1b]0;title\x1b\\after"), "after");
    }

    /// A control string ends at ST (`ESC \`), at BEL, or at a bare ESC
    /// the way a terminal ends it: the text after the terminator is kept
    /// whole, and an unterminated string swallows nothing past the next
    /// escape, whose own sequence is then handled by the normal rules.
    #[test]
    fn a_control_string_ends_at_st_bel_or_a_bare_escape() {
        assert_eq!(
            plain_text("see \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ end"),
            "see link end"
        );
        assert_eq!(plain_text("a\x1b]0;title\x07b"), "ab");
        // A lone ESC cuts the string; `ESC [31m` after it is a CSI, dropped.
        assert_eq!(plain_text("a\x1b]8;;http://x\x1b[31mb\x1b[0m"), "ab");
        // ... and the two-byte escape a stripped `ESC \` leaves behind
        // takes the character it introduces, as on a terminal.
        assert_eq!(
            plain_text("zw-\x1b]8;;http://x\x1blink\x1b]8;;\x1b-end-q"),
            "zw-inknd-q"
        );
        assert_eq!(plain_text("tail\x1b]8;;open"), "tail");
        assert_eq!(
            sanitize_ansi("\x1b]8;;http://x\x1b[1mbold\x1b[0m"),
            "\x1b[1mbold\x1b[0m"
        );
    }

    /// Conceal is the one attribute a reader must not get: a span painted
    /// with SGR 8 comes back visible, its other attributes intact.
    #[test]
    fn concealed_text_is_rendered_visible() {
        let lines = preformatted_lines("\x1b[8;1mhidden\x1b[0m shown");
        let hidden = every_span(&lines)
            .find(|span| span.content == "hidden")
            .expect("the concealed span is kept");
        assert!(!hidden.style.add_modifier.contains(Modifier::HIDDEN));
        assert!(hidden.style.add_modifier.contains(Modifier::BOLD));
        assert!(every_span(&lines).all(|span| !span.style.add_modifier.contains(Modifier::HIDDEN)));
        // The kept-SGR scrub (what the exit prints) drops it as well.
        assert_eq!(sanitize_ansi("\x1b[8mhidden\x1b[0m"), "hidden\x1b[0m");
        assert_eq!(
            sanitize_ansi("\x1b[8;1mx\x1b[1;8;4my"),
            "\x1b[1mx\x1b[1;4my"
        );
        assert_eq!(sanitize_ansi("\x1b[m\x1b[38;5;8mz"), "\x1b[m\x1b[38;5;8mz");
        assert_eq!(
            sanitize_ansi("\x1b[8;38;5;8;48;2;8;8;8mz"),
            "\x1b[38;5;8;48;2;8;8;8mz"
        );
        // A leading zero is still conceal.
        assert_eq!(sanitize_ansi("\x1b[08mhidden\x1b[0m"), "hidden\x1b[0m");
        assert_eq!(sanitize_ansi("\x1b[1;08mx"), "\x1b[1mx");
        let lines = preformatted_lines("\x1b[08mhidden\x1b[0m");
        assert!(every_span(&lines).all(|span| !span.style.add_modifier.contains(Modifier::HIDDEN)));
    }

    /// Invisible format characters that could reorder or hide approved
    /// text are dropped by both scrubs; a joiner inside an emoji stays.
    #[test]
    fn format_characters_that_reorder_or_hide_text_are_dropped() {
        let tricky =
            "rm \u{202E}txt.evil\u{202C} \u{200B}now\u{200F}\u{FEFF}\u{2066}x\u{2069}\u{00AD}";
        assert_eq!(plain_text(tricky), "rm txt.evil nowx");
        assert_eq!(sanitize_ansi(tricky), "rm txt.evil nowx");
        assert_eq!(
            sanitize_ansi("\u{202E}\x1b[1mbold\x1b[0m"),
            "\x1b[1mbold\x1b[0m"
        );
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(plain_text(family), family);
        assert_eq!(plain_text("tag\u{E0041}\u{E007F}"), "tag");
    }

    /// A spinner frame caught in the fd capture: the cursor move and the
    /// hide-cursor sequence go, the braille glyph stays. The other string
    /// controls end at ST or BEL like an OSC does.
    #[test]
    fn sanitize_drops_control_strings_and_keeps_the_spinner_glyph() {
        assert_eq!(sanitize_ansi("\x1b[0G\u{280b}\x1b[?25l"), "\u{280b}");
        assert_eq!(
            plain_text("\x1b[0G\u{280b}\x1b[?25l \x1b[1mx\x1b[0m"),
            "\u{280b} x"
        );
        assert_eq!(sanitize_ansi("a\x1bPq#0;2;0;0;0\x1b\\b"), "ab");
        assert_eq!(
            sanitize_ansi("a\x1bXsos\x07b\x1b^pm\x1b\\c\x1b_apc\x07d"),
            "abcd"
        );
        assert_eq!(sanitize_ansi("a\x1b_unterminated"), "a");
    }

    /// `CSI > ... m` and `CSI ? ... m` are vendor key-modifier settings
    /// shaped like SGR; they must not reach the terminal.
    #[test]
    fn sanitize_drops_private_marker_sequences_that_end_in_m() {
        assert_eq!(sanitize_ansi("a\x1b[>4;1mb\x1b[?1mc"), "abc");
        assert_eq!(
            sanitize_ansi("\x1b[<5m\x1b[=3m\x1b[1mx\x1b[0m"),
            "\x1b[1mx\x1b[0m"
        );
    }

    #[test]
    fn without_a_theme_bold_survives_and_no_colour_is_painted() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let lines = renderer.markdown_source("plain **bold** text", 80);
        let bold: Vec<&Span<'_>> = every_span(&lines)
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .collect();
        assert_eq!(bold.len(), 1, "{lines:?}");
        assert_eq!(bold[0].content.as_ref(), "bold");
        assert!(
            every_span(&lines).all(|span| matches!(span.style.fg, None | Some(Color::Reset))),
            "{lines:?}"
        );
    }

    /// A reply's own characters never pass for a wrap: an icon-font glyph
    /// in the private-use area is drawn, and a soft-break character in the
    /// source is dropped instead of marking a row.
    #[test]
    fn a_replys_own_characters_never_mark_a_wrap() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let source = format!("icon \u{E000} here and {SOFT_BREAK_SPACE}x{SOFT_BREAK_WORD}y");
        let lines = renderer.markdown_source(&source, 80);
        let text: String = lines.iter().map(Line::to_string).collect();
        assert!(text.contains("icon \u{E000} here and xy"), "{text:?}");
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| !is_wrap_mark(span)),
            "{lines:?}"
        );
    }

    /// The reset SGR 0 and 49 leave behind is no background at all, so the
    /// surface under the text shows; a background a tool painted stays.
    #[test]
    fn a_reset_background_is_dropped_and_a_painted_one_kept() {
        let theme: Theme = decode_bin(DARK_THEME).unwrap();
        let mut renderer = Renderer::new(RenderOptions {
            theme: Some(theme),
            truecolor: true,
            ..RenderOptions::default()
        })
        .unwrap();
        let lines = renderer.markdown_source(
            "# Title\n\n1. one **bold** `code` [x](https://e.com)\n\n```rust\nfn main() {}\n```\n",
            80,
        );
        assert!(every_span(&lines).count() > 10, "{lines:?}");
        for span in every_span(&lines) {
            assert_ne!(span.style.bg, Some(Color::Reset), "{span:?}");
        }
        let painted = preformatted_lines("\x1b[41mred\x1b[0m plain \x1b[49mback");
        let bgs: Vec<_> = every_span(&painted).map(|span| span.style.bg).collect();
        assert_eq!(bgs.first(), Some(&Some(Color::Red)), "{painted:?}");
        assert!(bgs[1..].iter().all(Option::is_none), "{painted:?}");
    }

    /// Under `NO_COLOR` no region is filled, so dropping a reset background
    /// changes no cell: the markdown a `NO_COLOR` renderer draws, and SGR
    /// text with resets in it, paint the same symbols, colours and
    /// attributes as the parser's lines with the reset kept.
    #[test]
    fn under_no_color_dropping_a_reset_background_draws_the_same_cells() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::{Paragraph, Widget};
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        renderer.no_color = true;
        let markdown = renderer.markdown_source(
            "# Title\n\n1. one **bold** `code` [x](https://e.com)\n\n```rust\nfn main() {}\n```\n",
            40,
        );
        let area = Rect::new(0, 0, 40, 12);
        let draw = |lines: Vec<Line<'static>>| {
            let mut buf = Buffer::empty(area);
            Paragraph::new(lines).render(area, &mut buf);
            buf
        };
        let kept = |lines: &[Line<'static>]| -> Vec<Line<'static>> {
            lines
                .iter()
                .cloned()
                .map(|mut line| {
                    for span in &mut line.spans {
                        if span.style.bg.is_none() {
                            span.style.bg = Some(Color::Reset);
                        }
                    }
                    line
                })
                .collect()
        };
        assert_eq!(draw(markdown.clone()), draw(kept(&markdown)));
        let sgr = "\x1b[1mbold\x1b[0m plain \x1b[49mback \x1b[2mdim\x1b[0m";
        let raw: Vec<Line<'static>> = sgr.into_text().unwrap().lines;
        assert_eq!(draw(preformatted_lines(sgr)), draw(raw));
    }

    #[test]
    fn a_theme_without_truecolor_paints_no_rgb() {
        let theme: Theme = decode_bin(DARK_THEME).unwrap();
        let options = RenderOptions {
            theme: Some(theme),
            truecolor: false,
            ..RenderOptions::default()
        };
        let mut renderer = Renderer::new(options).unwrap();
        let lines = renderer.markdown_source(
            "# Title\n\nSome `code` and a link [x](https://e.com)\n\n```rust\nfn main() {}\n```\n",
            80,
        );
        let painted = every_span(&lines)
            .filter(|span| span.style.fg.is_some())
            .count();
        assert!(painted > 0, "{lines:?}");
        for span in every_span(&lines) {
            for colour in [span.style.fg, span.style.bg, span.style.underline_color] {
                assert!(!matches!(colour, Some(Color::Rgb(..))), "{span:?}");
            }
        }
    }

    #[test]
    fn wrapping_measures_text_not_escapes() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let lines = renderer.markdown_source("one two **three** four five six seven", 12);
        assert!(lines.len() >= 3, "{lines:?}");
        for line in &lines {
            assert!(line.width() <= 12, "{line:?}");
        }
    }

    /// A table wider than the view is laid out to the width by the
    /// renderer, so every row still begins at a border and nothing is cut
    /// off mid-cell.
    #[test]
    fn a_table_wider_than_the_view_keeps_every_border_row_whole() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let source = "| name | description | status |\n\
                      |---|---|---|\n\
                      | alpha | the first of the three columns | ok |\n\
                      | beta | the second one, longer than the view | fine |\n";
        let lines = renderer.markdown_source(source, 30);
        assert!(lines.len() >= 5, "{lines:?}");
        for line in &lines {
            let text = line.to_string();
            assert!(
                text.starts_with(['┌', '╞', '├', '│', '└']),
                "a row does not start at a border: {text:?}"
            );
            assert!(line.width() <= 30, "{text:?}");
        }
        let body = lines.iter().map(Line::to_string).collect::<String>();
        assert!(body.contains("longer"), "{body}");
    }

    /// A fenced code line longer than the view stays one line with its
    /// highlighting whole; the view clips it. Wrapping the painted output
    /// by column would cut through an escape sequence.
    #[test]
    fn a_long_code_line_is_left_whole_for_the_view_to_clip() {
        let theme: Theme = decode_bin(DARK_THEME).unwrap();
        let mut renderer = Renderer::new(RenderOptions {
            theme: Some(theme),
            truecolor: true,
            ..Default::default()
        })
        .unwrap();
        let value = "v".repeat(40);
        let source = format!("```rust\nlet name = \"{value}\";\n```");
        let lines = renderer.markdown_source(&source, 20);
        let code: Vec<&Line<'static>> = lines
            .iter()
            .filter(|line| !line.to_string().starts_with("```"))
            .collect();
        assert_eq!(code.len(), 1, "{lines:?}");
        assert!(code[0].width() > 20, "{lines:?}");
        assert!(code[0].to_string().contains(&value), "{lines:?}");
        assert!(
            every_span(&lines).all(|span| !span.content.contains(ESC)),
            "{lines:?}"
        );
    }

    /// A styled line wider than the view is broken at the column
    /// boundary with every character kept and each piece in the style of
    /// the span it came from; lines that fit are left alone.
    #[test]
    fn hard_wrap_keeps_every_character_and_each_spans_style() {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let lines = vec![
            Line::from("fits"),
            Line::from(vec![Span::raw("abcdefgh"), Span::styled("ijklmnop", bold)]),
        ];
        let wrapped = hard_wrap(lines, 6);
        let rows: Vec<String> = wrapped.iter().map(Line::to_string).collect();
        assert_eq!(rows, ["fits", "abcdef", "ghijkl", "mnop"]);
        for line in &wrapped {
            assert!(line.width() <= 6, "{line:?}");
        }
        let styled: String = every_span(&wrapped)
            .filter(|span| span.style == bold)
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(styled, "ijklmnop");
        assert_eq!(hard_wrap(vec![Line::from("ab")], 0).len(), 2);
    }

    fn joins(lines: &[Line<'_>]) -> Vec<Option<&'static str>> {
        lines.iter().map(wrap_join).collect()
    }

    /// Only the rows a wrap made are marked: with a space where it broke
    /// at one, with nothing where it cut a word; a newline in the text
    /// starts an unmarked row.
    #[test]
    fn plain_lines_marks_wrapped_rows_only() {
        let lines = plain_lines("one two three\nfour", 8, Style::new());
        let rows: Vec<String> = lines.iter().map(Line::to_string).collect();
        assert_eq!(rows, ["one two", "three", "four"]);
        assert_eq!(joins(&lines), [None, Some(" "), None]);
        let lines = plain_lines("abcdefghijkl", 5, Style::new());
        assert_eq!(joins(&lines), [None, Some(""), Some("")]);
    }

    /// Every row a hard wrap cuts off joins back with nothing, and a mark
    /// the line already had stays on its first row.
    #[test]
    fn hard_wrap_marks_continuations_without_a_space() {
        let wrapped = hard_wrap(vec![Line::from("fits"), Line::from("abcdefgh")], 6);
        let rows: Vec<String> = wrapped.iter().map(Line::to_string).collect();
        assert_eq!(rows, ["fits", "abcdef", "gh"]);
        assert_eq!(joins(&wrapped), [None, None, Some("")]);
        let marked = Line::from(vec![wrap_mark(true), Span::raw("abcdefgh")]);
        assert_eq!(joins(&hard_wrap(vec![marked], 6)), [Some(" "), Some("")]);
    }

    /// Prose and list items the renderer wraps come back marked, a word
    /// too long for a row cut with no space, and the renderer's own
    /// characters for the marks are gone. Source lines are never joined.
    #[test]
    fn markdown_prose_marks_soft_wraps() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let prose = renderer.markdown_source("one two three four five six", 10);
        assert_eq!(joins(&prose), [None, Some(" "), Some(" ")], "{prose:?}");
        let list = renderer.markdown_source("- alpha beta gamma delta", 12);
        assert_eq!(joins(&list), [None, Some(" "), Some(" ")], "{list:?}");
        let word = renderer.markdown_source(&"x".repeat(25), 10);
        assert_eq!(joins(&word), [None, Some(""), Some("")], "{word:?}");
        let quote = renderer.markdown_source("> first line\n> second", 40);
        assert_eq!(joins(&quote), [None, None], "{quote:?}");
        for line in prose.iter().chain(&list).chain(&word) {
            let text = line.to_string();
            assert!(
                !text.contains([SOFT_BREAK_SPACE, SOFT_BREAK_WORD]),
                "{text:?}"
            );
        }
    }

    #[test]
    fn stable_tail_split_sits_at_the_last_blank_line_outside_a_fence() {
        assert_eq!(split_stable_tail("one paragraph"), 0);
        let two = "first\n\nsecond";
        assert_eq!(&two[..split_stable_tail(two)], "first\n");
        let fenced = "intro\n\n```\ncode\n\nmore code\n";
        assert_eq!(&fenced[..split_stable_tail(fenced)], "intro\n");
        let closed = "intro\n\n```\ncode\n```\n\ntail";
        assert_eq!(
            &closed[..split_stable_tail(closed)],
            "intro\n\n```\ncode\n```\n"
        );
    }

    #[test]
    fn an_open_fence_does_not_leak_into_the_next_render() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let _ = renderer.markdown_source("```rust\nfn main() {}", 80);
        let lines = renderer.markdown_source("plain **bold**", 80);
        assert!(
            every_span(&lines).any(|span| span.style.add_modifier.contains(Modifier::BOLD)),
            "{lines:?}"
        );
    }

    #[test]
    fn the_cache_reuses_the_stable_part_and_keeps_two_widths() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let mut cache = RenderCache::default();
        let source = "first paragraph\n\nsecond paragraph still streaming";
        let wide = cache.rendered(source, false, 80, true, &mut renderer).lines;
        let narrow = cache.rendered(source, false, 10, true, &mut renderer).lines;
        assert_eq!(cache.entries.len(), 2);
        assert!(narrow.len() > wide.len());
        let longer = format!("{source} and more");
        let again = cache
            .rendered(&longer, false, 80, true, &mut renderer)
            .lines;
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(again[0], wide[0]);
        cache.rendered(&longer, false, 40, true, &mut renderer);
        assert_eq!(cache.entries.len(), 2);
        assert!(cache.entries.iter().all(|e| e.width != 10));
        let done = cache.rendered(&longer, true, 80, true, &mut renderer).lines;
        assert!(
            done.iter()
                .any(|line| line.to_string().contains("and more")),
            "{done:?}"
        );
        assert_eq!(cache.entries.last().unwrap().stable_len, longer.len());
        assert_eq!(cache.finished(&longer, 80, true, &mut renderer).lines, done);
        assert_eq!(cache.entries.len(), 2);
    }

    const DIAGRAM: &str = "```mermaid\nflowchart TD\nA-->B\n```";

    fn faked(
        respond: impl Fn(&str, u16) -> Result<String, MermaidError> + Send + Sync + 'static,
    ) -> (Renderer, Arc<FakeMermaid>) {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let fake = Arc::new(FakeMermaid::new(respond));
        renderer.set_mermaid_backend(fake.clone());
        (renderer, fake)
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(Line::to_string).collect()
    }

    /// Without rich blocks a reply takes the markdown path whole, and a
    /// reply with no block draws the same either way.
    #[test]
    fn without_rich_blocks_a_reply_renders_as_plain_markdown() {
        let (mut renderer, fake) = faked(|_, _| Ok("drawn".to_string()));
        let source = format!("intro **bold**\n{DIAGRAM}\n- item");
        let plain = renderer.render_reply(&source, 40, false);
        assert!(plain.regions.is_empty());
        assert_eq!(plain.lines, renderer.markdown_source(&source, 40));
        assert!(texts(&plain.lines).contains(&"flowchart TD".to_string()));
        assert_eq!(fake.calls(), 0);
        let no_block =
            "# Title\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nfn x() {}\n```\n- one";
        assert_eq!(
            renderer.render_reply(no_block, 40, true),
            renderer.render_reply(no_block, 40, false)
        );
    }

    /// A drawn block takes the rows between its neighbours with no blank
    /// row added, and those rows copy as the fenced source.
    #[test]
    fn a_drawn_block_sits_between_its_neighbours_and_copies_as_source() {
        let (mut renderer, fake) = faked(|source, width| {
            assert_eq!(source, "flowchart TD\nA-->B\n");
            assert_eq!(width, 40);
            Ok("┌───┐\n│ A │\n└───┘\n".to_string())
        });
        let rendered = renderer.render_reply(&format!("before\n{DIAGRAM}\nafter"), 40, true);
        assert_eq!(
            texts(&rendered.lines),
            ["before", "┌───┐", "│ A │", "└───┘", "after"]
        );
        assert_eq!(
            rendered.regions,
            [CopyRegion::Block {
                rows: 1..4,
                source: Arc::from(DIAGRAM),
            }]
        );
        assert_eq!(fake.calls(), 1);
    }

    /// A table just before a block and a list just after it are laid out
    /// as they are with the block shown as source.
    #[test]
    fn a_table_before_and_a_list_after_a_block_render_as_in_source() {
        let (mut renderer, _) = faked(|_, _| Ok("drawn".to_string()));
        let source = format!("| a | b |\n|---|---|\n| 1 | 2 |\n{DIAGRAM}\n- one\n- two");
        let rich = texts(&renderer.render_reply(&source, 40, true).lines);
        let plain = texts(&renderer.render_reply(&source, 40, false).lines);
        let fence = plain
            .iter()
            .position(|line| line.starts_with("```mermaid"))
            .unwrap();
        assert!(fence > 3, "{plain:?}");
        assert_eq!(rich.iter().position(|line| line == "drawn"), Some(fence));
        assert_eq!(rich[..fence], plain[..fence]);
        assert_eq!(rich[fence + 1..], plain[fence + 4..]);
    }

    /// Every way the backend can fail shows the fenced source as it is
    /// highlighted in a markdown run, copying as that source, with one
    /// faded note under it that copies as nothing.
    #[test]
    fn each_failure_shows_the_source_and_a_note() {
        type Respond = fn(&str, u16) -> Result<String, MermaidError>;
        let failures: [(&str, Respond); 7] = [
            ("empty diagram", |_, _| Err(MermaidError::Empty)),
            ("unsupported diagram \"gantt\"", |_, _| {
                Err(MermaidError::Unsupported("gantt".to_string()))
            }),
            ("parse error: no arrow", |_, _| {
                Err(MermaidError::Parse("no arrow".to_string()))
            }),
            ("diagram needs 99 columns, 40 available", |_, _| {
                Err(MermaidError::TooWide {
                    needed: 99,
                    available: 40,
                })
            }),
            ("renderer failed: boom", |_, _| panic!("boom")),
            ("diagram needs 41 columns, 40 available", |_, _| {
                Ok("x".repeat(41))
            }),
            ("diagram longer than 400 lines or 16 KiB", |_, _| {
                Ok("drawn".to_string())
            }),
        ];
        for (why, respond) in failures {
            let (mut renderer, _) = faked(respond);
            let source = if why.contains("longer") {
                format!("```mermaid\n{}```", "A-->B\n".repeat(401))
            } else {
                DIAGRAM.to_string()
            };
            let rendered = renderer.render_reply(&source, 40, true);
            let mut expected = renderer.markdown_source(&source, 40);
            let note = format!("mermaid: {why} (showing source)");
            expected.push(Line::from(Span::styled(note, renderer.faded)));
            assert_eq!(rendered.lines, expected, "{why}");
            let row = expected.len() - 1;
            assert_eq!(
                rendered.regions,
                [
                    CopyRegion::Block {
                        rows: 0..row,
                        source: Arc::from(source.as_str()),
                    },
                    CopyRegion::Block {
                        rows: row..row + 1,
                        source: Arc::from(""),
                    },
                ],
                "{why}"
            );
        }
    }

    /// A reply cached in one mode is never served in the other, and the
    /// source view at a width leaves the drawn view at it cached.
    #[test]
    fn the_cache_keeps_drawn_and_source_renders_apart() {
        let (mut renderer, fake) = faked(|_, _| Ok("drawn".to_string()));
        let mut cache = RenderCache::default();
        let source = format!("intro\n\n{DIAGRAM}\n\nstill");
        let drawn = texts(
            &cache
                .rendered(&source, false, 40, true, &mut renderer)
                .lines,
        );
        let plain = cache.rendered(&source, false, 40, false, &mut renderer);
        assert!(drawn.contains(&"drawn".to_string()), "{drawn:?}");
        assert!(plain.regions.is_empty());
        let plain = texts(&plain.lines);
        assert!(plain.contains(&"flowchart TD".to_string()), "{plain:?}");
        assert!(!plain.contains(&"drawn".to_string()), "{plain:?}");
        cache.rendered(&source, false, 30, false, &mut renderer);
        cache.rendered(&source, false, 20, false, &mut renderer);
        assert_eq!(cache.entries.len(), 3);
        let again = texts(
            &cache
                .rendered(&source, false, 40, true, &mut renderer)
                .lines,
        );
        assert_eq!(again, drawn);
        assert_eq!(fake.calls(), 1);
        assert_eq!(
            cache.finished(&source, 40, false, &mut renderer).lines,
            renderer.markdown_source(&source, 40)
        );
    }

    /// A closed block in the streaming tail is drawn once and taken from
    /// the cache on later frames; a new width draws it again.
    #[test]
    fn a_block_in_the_streaming_tail_is_drawn_once_per_width() {
        let (mut renderer, fake) = faked(|_, _| Ok("drawn".to_string()));
        let mut cache = RenderCache::default();
        let source = format!("intro\n\n{DIAGRAM}\nstill");
        let first = cache.rendered(&source, false, 40, true, &mut renderer);
        let second = cache.rendered(&format!("{source} going"), false, 40, true, &mut renderer);
        assert_eq!(fake.calls(), 1);
        assert_eq!(first.regions, second.regions);
        assert!(texts(&second.lines).contains(&"drawn".to_string()));
        cache.rendered(&source, false, 30, true, &mut renderer);
        assert_eq!(fake.calls(), 2);
        let (mut failing, fake) = faked(|_, _| Err(MermaidError::Empty));
        failing.render_reply(DIAGRAM, 40, true);
        failing.render_reply(DIAGRAM, 40, true);
        assert_eq!(fake.calls(), 1, "a failure is cached too");
    }

    /// A chart drawn at one default height is not served from the cache
    /// once the default changes.
    #[test]
    fn a_new_chart_height_draws_the_chart_again() {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let source = "```chart\ntype: sparkline\ndata: [1, 2]\n```";
        let rows = |renderer: &mut Renderer| renderer.render_reply(source, 40, true).lines.len();
        assert_eq!(rows(&mut renderer), usize::from(chart::DEFAULT_HEIGHT));
        renderer.chart_height = 20;
        assert_eq!(rows(&mut renderer), 20);
    }

    /// A fence still streaming shows as source until it closes.
    #[test]
    fn an_unclosed_block_shows_as_source() {
        let (mut renderer, fake) = faked(|_, _| Ok("drawn".to_string()));
        let open = "```mermaid\nflowchart TD\nA-->B";
        let rendered = renderer.render_reply(open, 40, true);
        assert!(rendered.regions.is_empty());
        assert_eq!(rendered.lines, renderer.markdown_source(open, 40));
        assert_eq!(fake.calls(), 0);
    }

    fn math_faked(
        respond: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> (Renderer, Arc<FakeMath>) {
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let fake = Arc::new(FakeMath::new(respond));
        renderer.set_math_backend(fake.clone());
        (renderer, fake)
    }

    /// Display math is drawn two columns in, its TeX trimmed, and its rows
    /// copy as the formula's source.
    #[test]
    fn display_math_is_drawn_indented_and_copies_as_source() {
        let (mut renderer, _) = math_faked(|tex| {
            assert_eq!(tex, "\\frac{a}{b}");
            " a\n───\n b".to_string()
        });
        let formula = "$$\n\\frac{a}{b}\n$$";
        let rendered = renderer.render_reply(&format!("before\n{formula}\nafter"), 40, true);
        assert_eq!(
            texts(&rendered.lines),
            ["before", "   a", "  ───", "   b", "after"]
        );
        assert_eq!(
            rendered.regions,
            [CopyRegion::Block {
                rows: 1..4,
                source: Arc::from(formula),
            }]
        );
    }

    /// Display math that cannot be drawn is its literal text, a `*` in it
    /// kept rather than read as emphasis, and its rows copy as the source.
    #[test]
    fn display_math_that_cannot_be_drawn_shows_its_source_and_why() {
        let (mut renderer, fake) = math_faked(|_| "drawn".to_string());
        let formula = "$$\\begin{align*} a *b* &= c \\end{align*}$$";
        let rendered = renderer.render_reply(formula, 40, true);
        let mut expected = plain_lines(formula, 40, Style::new());
        let note = "math: \\begin{align*} is not supported (showing source)";
        expected.push(Line::from(Span::styled(note, renderer.faded)));
        assert_eq!(rendered.lines, expected);
        assert!(
            texts(&rendered.lines).concat().contains("a *b* &= c"),
            "{rendered:?}"
        );
        let row = expected.len() - 1;
        assert_eq!(
            rendered.regions[0],
            CopyRegion::Block {
                rows: 0..row,
                source: Arc::from(formula),
            }
        );
        assert_eq!(fake.calls(), 0);
    }

    /// An inline formula is drawn in its sentence by the real markdown
    /// renderer, styled as the words around it, and its columns copy as
    /// its source; without rich blocks the sentence is its source.
    #[test]
    fn inline_math_is_drawn_in_its_sentence() {
        let (mut renderer, _) = math_faked(|tex| {
            assert_eq!(tex, "x^2");
            "x²".to_string()
        });
        let source = "the **value $x^2$ here** and `$y$`";
        let rendered = renderer.render_reply(source, 80, true);
        assert_eq!(texts(&rendered.lines), ["the value x² here and $y$"]);
        let drawn = every_span(&rendered.lines)
            .find(|span| span.content.contains('²'))
            .unwrap();
        assert!(
            drawn.style.add_modifier.contains(Modifier::BOLD),
            "{drawn:?}"
        );
        assert_eq!(
            rendered.regions,
            [CopyRegion::Inline {
                row: 0,
                cols: 10..12,
                source: Arc::from("$x^2$"),
            }]
        );
        let plain = renderer.render_reply(source, 80, false);
        assert!(texts(&plain.lines)[0].contains("$x^2$"), "{plain:?}");
    }

    /// A formula glued to a word too long for a row is cut by the wrap
    /// with the word, each piece drawn where it lands and only the first
    /// copying as the source.
    #[test]
    fn an_inline_formula_split_by_the_wrap_is_drawn_on_both_rows() {
        let (mut renderer, _) = math_faked(|_| "abcdefghijklmnop".to_string());
        let word = "w".repeat(20);
        let rendered = renderer.render_reply(&format!("{word}$x$"), 24, true);
        assert_eq!(
            texts(&rendered.lines),
            [format!("{word}abcd"), "efghijklmnop".to_string()]
        );
        assert_eq!(
            rendered.regions,
            [
                CopyRegion::Inline {
                    row: 0,
                    cols: 20..24,
                    source: Arc::from("$x$"),
                },
                CopyRegion::Inline {
                    row: 1,
                    cols: 0..12,
                    source: Arc::from(""),
                },
            ]
        );
    }

    /// LLM-style replies mixing math, prices, code and broken TeX, drawn by
    /// the real crate at a narrow, a middling and a wide pane: nothing
    /// panics, and without rich blocks each reply is drawn as before.
    #[test]
    fn a_corpus_of_math_replies_never_panics() {
        let deep = format!("$${}$$", "{".repeat(200));
        let long = format!("${}$", "x^".repeat(3000));
        let corpus = [
            "The area is $\\pi r^2$.",
            "Euler: $e^{i\\pi} + 1 = 0$",
            "$$\\int_0^1 x^2\\,dx = \\frac{1}{3}$$",
            "$$\n\\sum_{k=1}^{n} k = \\frac{n(n+1)}{2}\n$$",
            "\\[ \\lim_{x \\to 0} \\frac{\\sin x}{x} = 1 \\]",
            "Inline \\(a^2 + b^2 = c^2\\) works.",
            "It costs $5 and $10, or $3.50 each.",
            "Set $HOME and $PATH, then run `echo $x$`.",
            "Use ${VAR} or $(cmd) in shell.",
            "```bash\necho \"$HOME $PATH\"\n```",
            "```\n$$\nnot math\n$$\n```",
            "$$\\begin{align*} a &= b \\\\ c &= d \\end{align*}$$",
            "$$\\sqrt[3]{x}$$",
            "$$\\frac{a}{$$",
            "$\\frac{1}{2$ unclosed brace",
            "$x$$y$",
            "$$$$",
            "$ $",
            "\\(\\)",
            "$\\unknowncommand{x}$",
            "$$\\begin{pmatrix} 1 & 2 \\\\ 3 & 4 \\end{pmatrix}$$",
            "$$f(x) = \\begin{cases} 1 & x > 0 \\\\ 0 & \\text{else} \\end{cases}$$",
            "- item with $\\alpha$\n- item with $\\beta_1$",
            "> quoted $x + y$ math",
            "| a | b |\n|---|---|\n| $x$ | $y^2$ |",
            "# Heading with $E = mc^2$",
            "A [link](https://e.com/$x$) and ![img](a$b$.png)",
            "<span title=\"$x$\">$y$</span>",
            "Escaped \\$5 and \\\\$x$",
            "$\\mathbb{R}^n \\to \\mathbb{R}$",
            "Matrix $\\begin{bmatrix} a & b \\end{bmatrix}$ inline",
            "Long $a_1 + a_2 + a_3 + a_4 + a_5 + a_6 + a_7 + a_8 + a_9 + a_{10}$ sum",
            "$$\n\\frac{\n",
            "\\[\nx\n\nnever closed",
            "Wide $\\text{\u{4e2d}\u{6587}} + x$ glyphs",
            "Icon \u{E000} glyph and $x$ math \u{E001}",
            "Nested $\\frac{\\frac{a}{b}}{\\frac{c}{d}}$ fraction",
            "$\\left( \\frac{a}{b} \\right)$",
            deep.as_str(),
            long.as_str(),
            "Dollar at end $",
            "$100 + $200 = $300",
            "\\$\\$ and $$ alone",
        ];
        assert!(corpus.len() >= 40);
        let mut renderer = Renderer::new(RenderOptions::default()).unwrap();
        let mut before = Renderer::new(RenderOptions::default()).unwrap();
        for reply in corpus {
            for width in [20, 40, 80] {
                renderer.render_reply(reply, width, true);
                assert_eq!(
                    renderer.render_reply(reply, width, false),
                    Rendered::plain(before.markdown_run(reply, width)),
                    "{reply:?} at {width}"
                );
            }
        }
    }
}
