//! Blocks of a reply drawn as something other than markdown. The
//! segmenter splits a reply into markdown runs and closed fences of a
//! kind the TUI draws itself, and display math on lines of its own; a
//! drawn block keeps its fenced source, so a copy over its rows takes that
//! source rather than the drawing. A block that cannot be drawn shows its
//! source with a note saying why.

pub(crate) mod chart;
pub(crate) mod math;
pub(crate) mod math_render;
pub(crate) mod mermaid;

use super::render::plain_text;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

/// What a fenced block is drawn as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BlockKind {
    Mermaid,
    Chart,
    DisplayMath,
}

/// A closed fence or display formula the TUI draws itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Block<'a> {
    pub(crate) kind: BlockKind,
    /// The whole fence or formula, opening and closing lines included,
    /// without the newline after the closing one.
    pub(crate) fenced: &'a str,
    /// The lines between the fences, or the TeX between the delimiters.
    pub(crate) body: &'a str,
}

impl Block<'_> {
    /// What a copy over the block's rows takes: its fenced source with the
    /// reply's CRLF line ends as LF, as the rest of a copy has them.
    pub(crate) fn copy_source(&self) -> Arc<str> {
        Arc::from(self.fenced.replace("\r\n", "\n"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Segment<'a> {
    Markdown(&'a str),
    Block(Block<'a>),
}

/// Parts of a rendered reply that a copy does not take as drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CopyRegion {
    /// Any selection touching these rows copies `source` once, in place
    /// of the rows; an empty source copies nothing.
    Block {
        rows: Range<usize>,
        source: Arc<str>,
    },
    /// Any selection touching these columns of the row copies `source`
    /// once, in place of the characters there; an empty source copies
    /// nothing.
    Inline {
        row: usize,
        cols: Range<usize>,
        source: Arc<str>,
    },
}

impl CopyRegion {
    fn shifted(self, by: usize) -> Self {
        match self {
            CopyRegion::Block { rows, source } => CopyRegion::Block {
                rows: rows.start + by..rows.end + by,
                source,
            },
            CopyRegion::Inline { row, cols, source } => CopyRegion::Inline {
                row: row + by,
                cols,
                source,
            },
        }
    }
}

/// A reply's lines and the rows of them a copy maps back to source.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Rendered {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) regions: Vec<CopyRegion>,
}

impl Rendered {
    pub(crate) fn plain(lines: Vec<Line<'static>>) -> Self {
        Self {
            lines,
            regions: Vec::new(),
        }
    }

    /// Adds `other` below, its regions moved down past the rows already
    /// here.
    pub(crate) fn append(&mut self, other: Rendered) {
        let offset = self.lines.len();
        self.lines.extend(other.lines);
        self.regions.extend(
            other
                .regions
                .into_iter()
                .map(|region| region.shifted(offset)),
        );
    }
}

/// A line that opens or closes a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delimiter<'a> {
    /// A code fence: the columns of indent in front of it and the info
    /// string after the backticks.
    Fence { indent: usize, info: &'a str },
    /// The opener of display math: the closer it waits for and the byte
    /// of the line its TeX starts at.
    Math { closer: &'static str, body: usize },
}

impl Delimiter<'_> {
    /// The kind of block a fence opened by this line is drawn as, if
    /// any. A fence indented four columns or more is not one: markdown
    /// reads it as an indented code line.
    fn kind(self) -> Option<BlockKind> {
        match self {
            Delimiter::Fence { indent, .. } if indent > 3 => None,
            Delimiter::Fence { info, .. } => {
                let info = info.trim();
                if info.eq_ignore_ascii_case("mermaid") {
                    Some(BlockKind::Mermaid)
                } else if info.eq_ignore_ascii_case("chart") {
                    Some(BlockKind::Chart)
                } else {
                    None
                }
            }
            Delimiter::Math { .. } => Some(BlockKind::DisplayMath),
        }
    }
}

/// The delimiter `line` is, if any. Every line whose text starts with
/// three backticks counts as a fence, opening or closing, as it does for
/// the markdown renderer, so the stable split and the segmenter agree on
/// what is inside a fence. A line starting with `$$` or `\[` behind at
/// most three columns of indent opens display math.
pub(crate) fn block_delimiter(line: &str) -> Option<Delimiter<'_>> {
    let text = line.trim_start();
    let indent = indent_columns(line);
    if text.starts_with("```") {
        let info = text.trim_start_matches('`').trim_end_matches(['\n', '\r']);
        return Some(Delimiter::Fence { indent, info });
    }
    if indent > 3 {
        return None;
    }
    math::DELIMITERS
        .iter()
        .find(|(opener, _, display)| *display && text.starts_with(opener))
        .map(|(opener, closer, _)| Delimiter::Math {
            closer,
            body: line.len() - text.len() + opener.len(),
        })
}

/// The columns of whitespace `line` starts with, a tab counted as four.
pub(crate) fn indent_columns(line: &str) -> usize {
    let text = line.trim_start();
    line[..line.len() - text.len()]
        .chars()
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

/// What is open at a line of a reply read from the top: a code fence or
/// display math.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BlockScan {
    open: Option<Open>,
}

#[derive(Clone, Copy, Debug)]
enum Open {
    /// `kind` is None for a fence drawn as code.
    Fence {
        at: usize,
        body: usize,
        kind: Option<BlockKind>,
    },
    Math {
        at: usize,
        body: usize,
        closer: &'static str,
    },
}

/// A block a line closed: the byte it opens at, the bytes of its body and
/// its kind.
pub(crate) struct Closed {
    at: usize,
    body: Range<usize>,
    kind: BlockKind,
}

impl BlockScan {
    /// Reads `line`, which starts at byte `start` of the reply, and hands
    /// back the block it closes, if any. Inside a fence only a fence line
    /// counts, and inside display math only its closer. Display math must
    /// close with nothing but blanks after the closer on its line, and a
    /// blank line before the closer ends it unclosed, as TeX would: either
    /// way its lines stay prose.
    pub(crate) fn line(&mut self, line: &str, start: usize) -> Option<Closed> {
        let is_fence = |line| matches!(block_delimiter(line), Some(Delimiter::Fence { .. }));
        // The math left open, and the part of this line its closer is
        // looked for in, with the byte of the reply that part starts at.
        let (at, body, closer, text, offset) = match self.open.take() {
            None => match block_delimiter(line)? {
                fence @ Delimiter::Fence { .. } => {
                    self.open = Some(Open::Fence {
                        at: start,
                        body: start + line.len(),
                        kind: fence.kind(),
                    });
                    return None;
                }
                Delimiter::Math { closer, body } => {
                    (start, start + body, closer, &line[body..], start + body)
                }
            },
            Some(Open::Fence { at, body, kind }) if is_fence(line) => {
                return kind.map(|kind| Closed {
                    at,
                    body: body..start,
                    kind,
                });
            }
            Some(Open::Math { .. }) if line.trim().is_empty() => return None,
            Some(Open::Math { at, body, closer }) => (at, body, closer, line, start),
            still_open => {
                self.open = still_open;
                return None;
            }
        };
        let Some((end, alone)) = math::display_closer(text, closer) else {
            self.open = Some(Open::Math { at, body, closer });
            return None;
        };
        alone.then(|| Closed {
            at,
            body: body..offset + end - closer.len(),
            kind: BlockKind::DisplayMath,
        })
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.is_some()
    }
}

/// Splits `source` into markdown runs and the closed fences and display
/// formulas drawn as blocks. A fence of another language, one indented as
/// code, and a block not yet closed stay in their run, so a block still
/// streaming shows as source until its closer arrives. A run before a
/// block keeps the newline that ends it: the markdown renderer draws no
/// row for a final newline, so the rows of the runs and blocks add up to
/// those of the whole reply drawn as source.
pub(crate) fn segment(source: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let mut run_start = 0;
    let mut offset = 0;
    let mut scan = BlockScan::default();
    for line in source.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let Some(closed) = scan.line(line, start) else {
            continue;
        };
        if run_start < closed.at {
            segments.push(Segment::Markdown(&source[run_start..closed.at]));
        }
        let fenced = &source[closed.at..offset];
        let fenced = fenced.strip_suffix('\n').unwrap_or(fenced);
        segments.push(Segment::Block(Block {
            kind: closed.kind,
            fenced: fenced.strip_suffix('\r').unwrap_or(fenced),
            body: &source[closed.body],
        }));
        run_start = offset;
    }
    if run_start < source.len() {
        segments.push(Segment::Markdown(&source[run_start..]));
    }
    segments
}

/// The faded line under a block shown as source, saying why it was not
/// drawn, on one row: a newline or tab in the note is a space.
pub(crate) fn note_line(note: &str, faded: Style) -> Line<'static> {
    Line::from(Span::styled(
        plain_text(note).replace(['\n', '\t'], " "),
        faded,
    ))
}

/// The theme a drawn block paints with.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RichStyle {
    /// The colours of a chart's series in turn; with none, a series takes
    /// the default colour.
    pub(crate) palette: Vec<Color>,
    pub(crate) title: Style,
    /// A chart's axes, their labels and its legend.
    pub(crate) axis: Style,
    pub(crate) no_color: bool,
}

impl RichStyle {
    /// The colour of the `index`th series.
    pub(crate) fn series(&self, index: usize) -> Style {
        Style {
            fg: index
                .checked_rem(self.palette.len())
                .map(|index| self.palette[index]),
            ..Style::new()
        }
    }
}

/// The rows of a buffer a widget was drawn into, as styled lines. A cell
/// hidden under a wide symbol is skipped, runs of one style share a span,
/// a `Reset` colour is left unset so no background is painted, and the
/// default cells at the end of a row are dropped. Under `no_color` only
/// the attributes stay.
pub(crate) fn buffer_lines(buffer: &Buffer, no_color: bool) -> Vec<Line<'static>> {
    let area = buffer.area;
    let colour = |colour: Color| (!no_color && colour != Color::Reset).then_some(colour);
    (area.top()..area.bottom())
        .map(|y| {
            let mut cells: Vec<(&str, Style)> = Vec::new();
            let mut hidden = 0;
            for x in area.left()..area.right() {
                if hidden > 0 {
                    hidden -= 1;
                    continue;
                }
                let cell = &buffer[(x, y)];
                hidden = cell.symbol().width().saturating_sub(1);
                let style = Style {
                    fg: colour(cell.fg),
                    bg: colour(cell.bg),
                    add_modifier: cell.modifier,
                    ..Style::new()
                };
                cells.push((cell.symbol(), style));
            }
            while cells
                .last()
                .is_some_and(|(symbol, style)| *symbol == " " && *style == Style::new())
            {
                cells.pop();
            }
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (symbol, style) in cells {
                match spans.last_mut() {
                    Some(span) if span.style == style => span.content.to_mut().push_str(symbol),
                    _ => spans.push(Span::styled(symbol.to_string(), style)),
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// What drawing a block gave: its rows, or the note to show under its
/// source.
pub(crate) type BlockResult = Result<Vec<Line<'static>>, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RichKey {
    /// None for an inline formula.
    kind: Option<BlockKind>,
    body: u64,
    width: u16,
    /// The rows a chart takes unless its spec says otherwise.
    chart_height: u16,
    no_color: bool,
}

impl RichKey {
    pub(crate) fn new(block: &Block, width: u16, chart_height: u16, no_color: bool) -> Self {
        let mut hasher = DefaultHasher::new();
        block.body.hash(&mut hasher);
        Self {
            kind: Some(block.kind),
            body: hasher.finish(),
            width,
            chart_height,
            no_color,
        }
    }

    /// The key of inline formula `tex` drawn in `width` columns, which
    /// takes neither the chart height nor the colours.
    pub(crate) fn inline_math(tex: &str, width: u16) -> Self {
        let mut hasher = DefaultHasher::new();
        tex.hash(&mut hasher);
        Self {
            kind: None,
            body: hasher.finish(),
            width,
            chart_height: 0,
            no_color: false,
        }
    }
}

/// How many drawn blocks are kept; each is a few dozen short rows.
const RICH_CACHE_ENTRIES: usize = 128;

/// Drawn blocks and inline formulas, least recently used first. A closed
/// block or a formula can sit in the streaming tail, which is rendered
/// anew every frame, so this is what keeps a diagram from being laid out
/// again each time. Failures are kept too, so a backend that panics on a
/// formula does so once, not every frame.
#[derive(Default)]
pub(crate) struct RichCache {
    entries: VecDeque<(RichKey, Arc<BlockResult>)>,
}

impl RichCache {
    pub(crate) fn get(&mut self, key: &RichKey) -> Option<Arc<BlockResult>> {
        let index = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(index)?;
        let value = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(value)
    }

    pub(crate) fn insert(&mut self, key: RichKey, value: Arc<BlockResult>) {
        if self.entries.len() >= RICH_CACHE_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back((key, value));
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::render::split_stable_tail;
    use ratatui::style::Modifier;

    fn blocks<'a>(segments: &[Segment<'a>]) -> Vec<Block<'a>> {
        segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Block(block) => Some(*block),
                Segment::Markdown(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_closed_mermaid_fence_is_a_block_between_two_runs() {
        let source = "before\n```mermaid\nflowchart TD\nA-->B\n```\nafter";
        assert_eq!(
            segment(source),
            [
                Segment::Markdown("before\n"),
                Segment::Block(Block {
                    kind: BlockKind::Mermaid,
                    fenced: "```mermaid\nflowchart TD\nA-->B\n```",
                    body: "flowchart TD\nA-->B\n",
                }),
                Segment::Markdown("after"),
            ]
        );
    }

    /// The info string is matched trimmed and without regard to case,
    /// and up to three columns of indent still open a fence.
    #[test]
    fn the_info_string_is_trimmed_and_case_folded() {
        for opener in ["```mermaid", "``` Mermaid ", "   ```MERMAID", "````mermaid"] {
            let source = format!("{opener}\nA-->B\n```\n");
            assert_eq!(blocks(&segment(&source)).len(), 1, "{opener:?}");
        }
    }

    /// Another language, an info string with more in it, an indent of
    /// four columns and an unclosed fence all stay markdown.
    #[test]
    fn other_fences_stay_in_the_markdown_run() {
        for source in [
            "```rust\nfn main() {}\n```\n",
            "```mermaid extra\nA-->B\n```\n",
            "    ```mermaid\nA-->B\n    ```\n",
            "\t```mermaid\nA-->B\n```\n",
            "text\n```mermaid\nflowchart TD\nA-->B\n",
        ] {
            assert_eq!(segment(source), [Segment::Markdown(source)], "{source:?}");
        }
    }

    /// Any fence line closes the fence that is open, as it does for the
    /// markdown renderer: a mermaid opener inside a text fence closes it,
    /// and the fence after the diagram is the one left open.
    #[test]
    fn a_fence_line_closes_whatever_fence_is_open() {
        let source = "```text\n```mermaid\nA-->B\n```\n";
        assert_eq!(segment(source), [Segment::Markdown(source)]);
    }

    #[test]
    fn back_to_back_blocks_leave_no_empty_run() {
        let source = "```mermaid\nA\n```\n```mermaid\nB\n```\n";
        let segments = segment(source);
        assert_eq!(segments.len(), 2, "{segments:?}");
        assert_eq!(blocks(&segments).len(), 2);
    }

    #[test]
    fn a_closed_chart_fence_is_a_chart_block() {
        let source = "```chart\ntype: sparkline\ndata: [1, 2]\n```\n``` Chart\n{}\n```\n";
        let kinds: Vec<BlockKind> = blocks(&segment(source))
            .iter()
            .map(|block| block.kind)
            .collect();
        assert_eq!(kinds, [BlockKind::Chart, BlockKind::Chart]);
        for source in ["```charts\nx\n```\n", "```chart\ntype: bar\n"] {
            assert_eq!(segment(source), [Segment::Markdown(source)], "{source:?}");
        }
    }

    /// The stable part of every prefix of a reply and the tail after it,
    /// segmented apart, find the blocks the whole prefix holds: the split
    /// never falls inside a fence or display math.
    #[test]
    fn segmenting_the_stable_part_and_the_tail_apart_finds_the_same_blocks() {
        let reply = "Intro line.\n\n```mermaid\nflowchart TD\n\nA-->B\n```\n\n\
                     Some prose.\n\n```rust\nlet x = 1;\n\n```\n\n    ```mermaid\nx\n\n    ```\n\n\
                     ```mermaid\nsequenceDiagram\nA->>B: hi\n```\nend\n\n\
                     $$\n\\frac{a}{b}\n$$\n\nso $x$ and\n\\[ y \\]\n\n$$ open\n\nlate $$\n\n\
                     \\[\n```mermaid\nA\n```\n\\]\n\n$$x$$ junk\n\n```\n$$\n```\n$$z$$\n";
        let ends = reply.char_indices().map(|(i, _)| i).chain([reply.len()]);
        for end in ends {
            let prefix = &reply[..end];
            let split = split_stable_tail(prefix);
            let mut apart = blocks(&segment(&prefix[..split]));
            apart.extend(blocks(&segment(&prefix[split..])));
            assert_eq!(apart, blocks(&segment(prefix)), "{prefix:?}");
        }
        let kinds: Vec<BlockKind> = blocks(&segment(reply)).iter().map(|b| b.kind).collect();
        use BlockKind::{DisplayMath, Mermaid};
        assert_eq!(
            kinds,
            [
                Mermaid,
                Mermaid,
                DisplayMath,
                DisplayMath,
                DisplayMath,
                DisplayMath
            ]
        );
    }

    #[test]
    fn display_math_on_lines_of_its_own_is_a_block() {
        let source = "before\n$$\n\\frac{a}{b}\n$$\n  \\[ x^2 \\]  \nafter";
        assert_eq!(
            segment(source),
            [
                Segment::Markdown("before\n"),
                Segment::Block(Block {
                    kind: BlockKind::DisplayMath,
                    fenced: "$$\n\\frac{a}{b}\n$$",
                    body: "\n\\frac{a}{b}\n",
                }),
                Segment::Block(Block {
                    kind: BlockKind::DisplayMath,
                    fenced: "  \\[ x^2 \\]  ",
                    body: " x^2 ",
                }),
                Segment::Markdown("after"),
            ]
        );
    }

    /// Display math with text after its closer, or behind an indent of
    /// four columns, or not yet closed, or cut by a blank line, or inside
    /// a fence, stays markdown; so does an escaped closer.
    #[test]
    fn other_display_math_stays_in_the_markdown_run() {
        for source in [
            "$$x$$ and more\n",
            "$$\nx\n$$ and more\n",
            "    $$x$$\n",
            "$$\n\\frac{a}{b}\n",
            "\\[\nx\n\nstill\n\\]\n",
            "```\n$$x$$\n```\n",
            "$$x\\$$\n",
            "a $$x$$\n",
        ] {
            assert_eq!(segment(source), [Segment::Markdown(source)], "{source:?}");
        }
    }

    #[test]
    fn regions_move_down_past_the_rows_above() {
        let mut rendered = Rendered::plain(vec![Line::from("a"), Line::from("b")]);
        rendered.append(Rendered {
            lines: vec![Line::from("c")],
            regions: vec![CopyRegion::Block {
                rows: 0..1,
                source: Arc::from("src"),
            }],
        });
        assert_eq!(
            rendered.regions,
            [CopyRegion::Block {
                rows: 2..3,
                source: Arc::from("src"),
            }]
        );
    }

    #[test]
    fn the_cache_drops_the_least_recently_used_entry() {
        fn key(body: &str, width: u16) -> RichKey {
            let block = Block {
                kind: BlockKind::Mermaid,
                fenced: body,
                body,
            };
            RichKey::new(&block, width, 12, false)
        }
        let mut cache = RichCache::default();
        cache.insert(key("first", 80), Arc::new(Ok(Vec::new())));
        for i in 1..RICH_CACHE_ENTRIES {
            cache.insert(key(&format!("body {i}"), 80), Arc::new(Ok(Vec::new())));
        }
        assert!(cache.get(&key("first", 80)).is_some());
        cache.insert(key("one more", 80), Arc::new(Err(String::new())));
        assert_eq!(cache.len(), RICH_CACHE_ENTRIES);
        assert!(cache.get(&key("first", 80)).is_some());
        assert!(cache.get(&key("body 1", 80)).is_none());
        assert!(cache.get(&key("first", 40)).is_none());
    }

    #[test]
    fn a_note_is_one_row() {
        let line = note_line("chart: bad\n\tspec\x1b[2J", Style::new());
        assert_eq!(line.to_string(), "chart: bad  spec");
    }

    #[test]
    fn an_empty_palette_paints_series_in_the_default_colour() {
        let style = RichStyle {
            palette: Vec::new(),
            title: Style::new(),
            axis: Style::new(),
            no_color: false,
        };
        assert_eq!(style.series(3), Style::new());
        let style = RichStyle {
            palette: vec![Color::Red, Color::Blue],
            ..style
        };
        assert_eq!(style.series(3), Style::new().fg(Color::Blue));
    }

    /// A reply with CRLF line ends gives a block with no CR after its
    /// closer, and its copy has LF line ends.
    #[test]
    fn a_crlf_block_copies_with_lf_line_ends() {
        let source = "before\r\n```mermaid\r\nA-->B\r\n```\r\nafter";
        let found = blocks(&segment(source));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].fenced, "```mermaid\r\nA-->B\r\n```");
        assert_eq!(&*found[0].copy_source(), "```mermaid\nA-->B\n```");
    }

    fn buffer(width: u16, draw: impl FnOnce(&mut Buffer)) -> Buffer {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, width, 1));
        draw(&mut buffer);
        buffer
    }

    /// A wide symbol takes its own cell and the one it hides is not read,
    /// and the blank cells after the text are dropped.
    #[test]
    fn a_wide_symbol_hides_the_cell_after_it() {
        let buffer = buffer(8, |buffer| {
            buffer.set_string(0, 0, "a\u{4e2d}b", Style::new());
        });
        assert_eq!(
            buffer_lines(&buffer, false),
            [Line::from(Span::raw("a\u{4e2d}b"))]
        );
    }

    #[test]
    fn cells_of_one_style_share_a_span_and_reset_paints_nothing() {
        let red = Style::new().fg(Color::Red);
        let buffer = buffer(8, |buffer| {
            buffer.set_string(0, 0, "ab", red.bg(Color::Reset));
            buffer.set_string(2, 0, "cd", Style::new().fg(Color::Reset));
            buffer.set_string(4, 0, "e", red);
        });
        assert_eq!(
            buffer_lines(&buffer, false),
            [Line::from(vec![
                Span::styled("ab", red),
                Span::raw("cd"),
                Span::styled("e", red),
            ])]
        );
    }

    /// Under `NO_COLOR` the colours go and the attributes stay, so a
    /// coloured blank at the end is dropped like any other.
    #[test]
    fn no_color_keeps_only_the_attributes() {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let buffer = buffer(8, |buffer| {
            buffer.set_string(0, 0, "ab", bold.fg(Color::Red).bg(Color::Blue));
            buffer.set_string(2, 0, "c", Style::new().fg(Color::Green));
            buffer.set_string(3, 0, " ", Style::new().bg(Color::Green));
        });
        assert_eq!(
            buffer_lines(&buffer, true),
            [Line::from(vec![Span::styled("ab", bold), Span::raw("c")])]
        );
    }
}
