//! Blocks of a reply drawn as something other than markdown. The
//! segmenter splits a reply into markdown runs and closed fences of a
//! kind the TUI draws itself; a drawn block keeps its fenced source, so a
//! copy over its rows takes that source rather than the drawing. A block
//! that cannot be drawn shows its source with a note saying why.

pub(crate) mod mermaid;

use super::render::plain_text;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::Arc;

/// What a fenced block is drawn as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum BlockKind {
    Mermaid,
}

/// A closed fence the TUI draws itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Block<'a> {
    pub(crate) kind: BlockKind,
    /// The whole fence, opening and closing lines included, without the
    /// newline after the closing one.
    pub(crate) fenced: &'a str,
    /// The lines between the fences.
    pub(crate) body: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Segment<'a> {
    Markdown(&'a str),
    Block(Block<'a>),
}

/// Rows of a rendered reply that a copy does not take as drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CopyRegion {
    /// Any selection touching these rows copies `source` once, in place
    /// of the rows; an empty source copies nothing.
    Block {
        rows: Range<usize>,
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
}

impl Delimiter<'_> {
    /// The kind of block a fence opened by this line is drawn as, if
    /// any. A fence indented four columns or more is not one: markdown
    /// reads it as an indented code line.
    fn kind(self) -> Option<BlockKind> {
        match self {
            Delimiter::Fence { indent, info } => (indent <= 3
                && info.trim().eq_ignore_ascii_case("mermaid"))
            .then_some(BlockKind::Mermaid),
        }
    }
}

/// The delimiter `line` is, if any. Every line whose text starts with
/// three backticks counts as a fence, opening or closing, as it does for
/// the markdown renderer, so the stable split and the segmenter agree on
/// what is inside a fence.
pub(crate) fn block_delimiter(line: &str) -> Option<Delimiter<'_>> {
    let text = line.trim_start();
    if !text.starts_with("```") {
        return None;
    }
    let indent = line[..line.len() - text.len()]
        .chars()
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum();
    let info = text.trim_start_matches('`').trim_end_matches(['\n', '\r']);
    Some(Delimiter::Fence { indent, info })
}

/// Splits `source` into markdown runs and the closed fences drawn as
/// blocks. A fence of another language, one indented as code, and one
/// not yet closed stay in their run, so a block still streaming shows as
/// source until its closing fence arrives. A run before a block keeps the
/// newline that ends it: the markdown renderer draws no row for a final
/// newline, so the rows of the runs and blocks add up to those of the
/// whole reply drawn as source.
pub(crate) fn segment(source: &str) -> Vec<Segment<'_>> {
    let mut segments = Vec::new();
    let mut run_start = 0;
    let mut offset = 0;
    let mut open: Option<(usize, usize, Option<BlockKind>)> = None;
    for line in source.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let Some(delimiter) = block_delimiter(line) else {
            continue;
        };
        match open.take() {
            None => open = Some((start, offset, delimiter.kind())),
            Some((opened_at, body_start, Some(kind))) => {
                if run_start < opened_at {
                    segments.push(Segment::Markdown(&source[run_start..opened_at]));
                }
                let fenced = &source[opened_at..offset];
                segments.push(Segment::Block(Block {
                    kind,
                    fenced: fenced.strip_suffix('\n').unwrap_or(fenced),
                    body: &source[body_start..start],
                }));
                run_start = offset;
            }
            Some((_, _, None)) => {}
        }
    }
    if run_start < source.len() {
        segments.push(Segment::Markdown(&source[run_start..]));
    }
    segments
}

/// The faded line under a block shown as source, saying why it was not
/// drawn.
pub(crate) fn note_line(note: &str, faded: Style) -> Line<'static> {
    Line::from(Span::styled(plain_text(note), faded))
}

/// What drawing a block gave: its rows, or the note to show under its
/// source.
pub(crate) type BlockResult = Result<Vec<Line<'static>>, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RichKey {
    kind: BlockKind,
    body: u64,
    width: u16,
    no_color: bool,
}

impl RichKey {
    pub(crate) fn new(block: &Block, width: u16, no_color: bool) -> Self {
        let mut hasher = DefaultHasher::new();
        block.body.hash(&mut hasher);
        Self {
            kind: block.kind,
            body: hasher.finish(),
            width,
            no_color,
        }
    }
}

/// How many drawn blocks are kept; each is a few dozen short rows.
const RICH_CACHE_ENTRIES: usize = 128;

/// Drawn blocks, least recently used first. A closed block can sit in the
/// streaming tail, which is rendered anew every frame, so this is what
/// keeps a diagram from being laid out again each time. Failures are kept
/// too.
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

    /// The stable part of every prefix of a reply and the tail after it,
    /// segmented apart, find the blocks the whole prefix holds: the split
    /// never falls inside a fence.
    #[test]
    fn segmenting_the_stable_part_and_the_tail_apart_finds_the_same_blocks() {
        let reply = "Intro line.\n\n```mermaid\nflowchart TD\n\nA-->B\n```\n\n\
                     Some prose.\n\n```rust\nlet x = 1;\n\n```\n\n    ```mermaid\nx\n\n    ```\n\n\
                     ```mermaid\nsequenceDiagram\nA->>B: hi\n```\nend\n";
        let ends = reply.char_indices().map(|(i, _)| i).chain([reply.len()]);
        for end in ends {
            let prefix = &reply[..end];
            let split = split_stable_tail(prefix);
            let mut apart = blocks(&segment(&prefix[..split]));
            apart.extend(blocks(&segment(&prefix[split..])));
            assert_eq!(apart, blocks(&segment(prefix)), "{prefix:?}");
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
            RichKey::new(&block, width, false)
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
}
