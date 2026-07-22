# Rich Markdown Renderer — Phase 2: Tables + List Wrapping

**Status:** Planning complete, awaiting Momus review before implementation.
**Owner:** Coyote maintainer
**Estimated effort:** 3-4 days (tables + hanging-indent wrapping for lists/blockquotes)
**Related:** [Phase 1 plan](./rich-markdown-renderer.md) — must be complete first (it is).

---

## Goals

1. **Tables:** render GFM markdown tables (`| col | col |` with `|---|---|` separator rows) as styled terminal tables using box-drawing characters, respecting per-column alignment specifiers and the user's syntect theme colors. Match glamour's structural rendering (box borders, header separator, aligned cells). Cell content wraps within the column boundary.
2. **List wrapping:** when a bullet/numbered/task list item's content is longer than the wrap width, wrap it with a **hanging indent** so continuation lines align under the text, not under the bullet marker. Same treatment for blockquotes — continuation lines get the `│ ` prefix.

## Non-Goals

- **Not shipping without `comfy-table` dependency.** Hand-rolling table rendering requires reimplementing width-aware unicode + ANSI-aware column sizing. `comfy-table 7.2.2` already does this correctly (`ansi_strip().width()`) and is actively maintained (Jan 2026). See "Library decision" below.
- **Not supporting non-GFM table syntaxes.** Multi-line cells, cell merging, nested tables, and reStructuredText-style grid tables are out of scope. Standard GFM `|` + `---` only.
- **Not showing partial tables during streaming.** Tables accumulate silently while rows arrive; the rendered table appears once when the block ends. Users see a brief pause during accumulation instead of a flashing raw→rendered transition. Matches glamour behavior.
- **Not preserving the "zero touches outside `markdown.rs`" Phase 1 principle** — see "Scope-Expansion Rationale" below.

## Scope-Expansion Rationale

Phase 1 held two principles that Phase 2 must relax, both with clear justification:

1. **"No state beyond `LineType` code-block tracker."** Tables inherently need multi-line state (buffer rows until block ends). Contained to a single `Option<TableState>` field on `MarkdownRender`. No other state added.
2. **"Only `src/render/markdown.rs` changes."** Tables need an end-of-stream flush hook, which means 3 small callsite changes: `stream.rs` (streaming), `app_config.rs::print_markdown` (one-shot CLI), `session.rs::render` (session display). Each change is a single line: `output.push_str(&render.finalize())`.

These are necessary complexity, not scope creep. The plan explicitly recognizes them.

## Resolved Design Decisions

1. **Library:** use `comfy-table 7.2.2` with `custom_styling` feature enabled. Only Rust table library that correctly strips ANSI escapes before width computation (via `s.ansi_strip().width()` at `custom_styling.rs:10`). Alternatives (tabled, cli-table, term-table, prettytable-rs) either lack ANSI support, don't support arbitrary border colors, or are abandoned. Full survey in the librarian report.
2. **Streaming behavior:** silent accumulation. Table rows return empty string from renderer; buffered internally; rendered on block end. Matches glamour.
3. **Detection lookahead:** speculative table detection. First `|...|` line buffered as `PendingHeader`; next line's shape confirms (separator → commit to table) or rejects (anything else → flush both as paragraphs). Required for correctness — GFM demands separator row.
4. **Border color:** new `MarkdownStyles::table_border` field resolved from theme via scope `punctuation.definition.table.markdown` → fallback `punctuation` → fallback `hrule` (which already exists). Applies to all box-drawing chars uniformly.
5. **Header styling:** reuse existing `heading` style (bold + heading color) for header cells. No new field needed.
6. **Alignment specifiers:** parse `:---`, `---:`, `:---:` from separator row; map to `comfy_table::CellAlignment::Left/Right/Center`. Default (no colons) = left.
7. **Inline markdown in cells:** run `apply_inline()` on each cell before feeding to comfy-table. `custom_styling` feature ensures widths compute correctly on pre-styled text.

## Architecture

### New dependency

`Cargo.toml`:
```toml
comfy-table = { version = "7.2.2", features = ["custom_styling"] }
```

Pulls in `unicode-width` (already used indirectly), `unicode-segmentation`, and `ansi-str`. Total footprint small (~77KB crate).

### New MarkdownStyles field

`markdown.rs:612` — add one line to the struct:

```rust
pub struct MarkdownStyles {
    // ... existing 11 fields ...
    table_border: Color,
}
```

Resolved in `from_theme()` via `resolve_scope_style(theme, "punctuation.definition.table.markdown", &["punctuation", "meta.separator"], truecolor)`. Falls back to `hrule` color if scope not found. `None` case → default color.

### New LineKind variant

`markdown.rs:57` — extend enum:

```rust
pub enum LineKind {
    // ... existing variants ...
    TableRow,       // any line matching ^\s*\|.*\|\s*$
    TableSeparator, // subset of TableRow matching ^\s*\|(\s*:?-+:?\s*\|)+\s*$
}
```

Two variants because separator detection needs its own regex; keeping them distinct simplifies the state machine.

### Table detection regexes

Add to the `LazyLock` regex block:

```rust
// A line that looks like a table row: starts and ends with |, non-empty content
static TABLE_ROW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\|.*\|\s*$").unwrap());

// The separator row that must follow a header: | :---: | ---: | :--- | ---- |
static TABLE_SEPARATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(
    r"^\s*\|(\s*:?-{3,}:?\s*\|)+\s*$"
).unwrap());
```

Order in `detect_line_kind`: check `TABLE_SEPARATOR` before `TABLE_ROW` (separator is a subset of row).

### New TableState struct

Added to `markdown.rs`:

```rust
enum TableState {
    /// Just saw a `|...|` line but haven't seen the separator yet.
    /// If next line is a separator → transition to Active.
    /// If next line is anything else → not a table; flush the header as paragraph + process next line.
    PendingHeader(String),

    /// Confirmed table. Accumulating data rows.
    Active {
        header: Vec<String>,
        alignments: Vec<CellAlignment>,
        rows: Vec<Vec<String>>,
    },
}
```

### MarkdownRender state field

`markdown.rs:266` — add one field:

```rust
pub struct MarkdownRender {
    // ... existing 8 fields ...
    table_state: Option<TableState>,
}
```

Initialized to `None` in `init()`.

### State machine (in `render_line_mut`)

Runs BEFORE the existing branch on `is_code`/`raw_markdown`/rich:

```rust
fn render_line_mut(&mut self, line: &str) -> String {
    let (line_type, line_kind, code_syntax, is_code) = self.check_line(line);
    self.prev_line_type = line_type;
    self.code_syntax = code_syntax;

    // Table state machine — runs FIRST because tables preempt normal rendering
    if let Some(output) = self.handle_table_state(line, line_kind) {
        return output;
    }

    // ... existing code / raw_markdown / rich branch (unchanged) ...
}

fn handle_table_state(&mut self, line: &str, kind: LineKind) -> Option<String> {
    match (&mut self.table_state, kind) {
        // No pending table + saw a row → start pending
        (None, LineKind::TableRow) => {
            self.table_state = Some(TableState::PendingHeader(line.to_string()));
            Some(String::new())  // silent accumulation
        }
        // No pending table + saw a separator (rare) → treat as paragraph
        (None, LineKind::TableSeparator) => None,

        // Pending header + saw separator → commit to Active
        (Some(TableState::PendingHeader(header_line)), LineKind::TableSeparator) => {
            let header = parse_table_row(&header_line);
            let alignments = parse_alignments(line);
            self.table_state = Some(TableState::Active { header, alignments, rows: vec![] });
            Some(String::new())
        }
        // Pending header + saw another row (no separator) → not a table; flush both as paragraphs
        (Some(TableState::PendingHeader(header_line)), LineKind::TableRow) => {
            let flushed = std::mem::take(header_line).clone();
            self.table_state = None;
            let a = self.render_as_paragraph(&flushed);
            let b = self.render_as_paragraph(line);
            Some(format!("{a}\n{b}"))
        }
        // Pending header + saw anything else → not a table; flush header + process line normally
        (Some(TableState::PendingHeader(_)), _) => {
            let TableState::PendingHeader(header_line) = self.table_state.take().unwrap()
                else { unreachable!() };
            let flushed = self.render_as_paragraph(&header_line);
            None  // caller continues with normal rendering; prepend `flushed` in caller
            // (implementation detail: needs to return Some(flushed + normal_render) — see impl)
        }

        // Active + saw a row → add to buffer
        (Some(TableState::Active { rows, .. }), LineKind::TableRow) => {
            rows.push(parse_table_row(line));
            Some(String::new())
        }
        // Active + saw anything else → flush table + process line
        (Some(TableState::Active { .. }), _) => {
            let TableState::Active { header, alignments, rows } = self.table_state.take().unwrap()
                else { unreachable!() };
            let rendered = self.render_table(header, alignments, rows);
            None  // caller prepends rendered + processes line normally
            // (same pattern as above)
        }

        (None, _) => None,  // no table state to affect; normal rendering
    }
}
```

**Note on the "prepend + continue" pattern**: for the flush-then-continue transitions, the cleanest implementation splits into `handle_table_state` returning `Option<String>` for the flushed-table portion, and the caller concatenates that with the normally-rendered current line. Implementation detail; the state transitions are what matter for design review.

### Cell parsing

Two helpers:

```rust
fn parse_table_row(line: &str) -> Vec<String> {
    // Strip leading/trailing whitespace and the outer `|`
    let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

fn parse_alignments(separator_row: &str) -> Vec<CellAlignment> {
    let cells = parse_table_row(separator_row);
    cells.iter().map(|c| {
        let trimmed = c.trim();
        let starts = trimmed.starts_with(':');
        let ends = trimmed.ends_with(':');
        match (starts, ends) {
            (true, true) => CellAlignment::Center,
            (false, true) => CellAlignment::Right,
            _ => CellAlignment::Left,
        }
    }).collect()
}
```

Edge cases:
- Empty cells (`| | |`) → empty strings in the returned Vec, comfy-table handles.
- Column count mismatch (header has 3 cells, data row has 2) → comfy-table's behavior: pads or truncates. Test coverage will verify.
- Escaped pipes (`\|`) in cell content — GFM spec supports; **defer to Phase 2.1 follow-up if needed**. Initial implementation splits on raw `|`.

### Table rendering

```rust
use comfy_table::{Table, CellAlignment, presets::UTF8_FULL, ContentArrangement};

fn render_table(
    &self,
    header: Vec<String>,
    alignments: Vec<CellAlignment>,
    rows: Vec<Vec<String>>,
) -> String {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL);
    table.set_content_arrangement(ContentArrangement::Dynamic);
    if let Some(width) = self.wrap_width {
        table.set_width(width);
    }

    // Header cells: inline-rendered + heading style (bold + heading color)
    let styled_header: Vec<String> = header.iter()
        .map(|c| apply_bold(&apply_inline(c, &self.styles), self.styles.heading.0))
        .collect();
    table.set_header(styled_header);

    // Per-column alignment
    for (i, align) in alignments.iter().enumerate() {
        if let Some(col) = table.column_mut(i) {
            col.set_cell_alignment(*align);
        }
    }

    // Data rows: inline-rendered only
    for row in rows {
        let styled_row: Vec<String> = row.iter()
            .map(|c| apply_inline(c, &self.styles))
            .collect();
        table.add_row(styled_row);
    }

    // Border color: apply table_border to all box-drawing chars via ANSI wrapping.
    // comfy-table's styling API — inspect final rendered output and colorize border chars,
    // OR use comfy-table's built-in styling if it supports per-component color.
    // Investigate during Phase 2.4 implementation.

    format!("{table}")
}
```

**Border color note**: comfy-table has border styling but it may not expose direct per-char color control. Two options:
1. Post-process the rendered string with a regex that colorizes box-drawing chars (`[─│┼┌┐└┘├┤┬┴]`).
2. Use comfy-table's `style()` API if it supports arbitrary ANSI.

Confirm during Phase 2.4 implementation — worst case is regex post-process, which is simple.

### `render_line` (immutable) behavior for partial table rows

`render_line` is called on the incomplete in-progress line during streaming. If the partial buffer looks like `| foo | ba`, it's mid-row and immutable — can't add to state.

Behavior: `render_line` sees `LineKind::TableRow` or the pattern and just renders raw markdown (since it can't buffer). The user sees `| foo | ba` briefly, then it disappears when the complete row arrives via `render_line_mut` (silent accumulation) and eventually the rendered table appears. Consistent with the "silent accumulation" decision.

### Line wrapping with hanging indent

Phase 1's `render_bullet`/`render_numbered`/`render_task`/`render_blockquote` produce a single line each and don't wrap long content. When `wrap_width` is set, long items overflow past the wrap column. This phase fixes that by adding **hanging-indent wrapping** using `textwrap` (already a dependency).

**Desired output:**

```
• text that
  wraps and
  wraps
1. text that
   wraps and
   wraps
[ ] task text
    that wraps
│ blockquote line
│ that continues
```

**Design:**

Each block renderer computes a prefix width, applies wrapping to the content with `textwrap::Options::subsequent_indent(prefix_width_spaces)`, then styles each wrapped line with the appropriate prefix on line 1 and continuation-indent on later lines.

Critical subtlety: `textwrap` computes width by **byte length**, not visual width. We must wrap the **plain text content** (before applying inline ANSI codes), then apply `apply_inline` per wrapped line. Otherwise ANSI escape bytes distort the wrap column calculation.

Sketch:

```rust
fn render_bullet(&self, line: &str) -> String {
    let (leading, content) = split_leading_indent(line);  // handles nested lists
    let content_after_marker = &content[content.find(' ').unwrap() + 1..];  // strip "- "

    let Some(wrap_width) = self.wrap_width else {
        // No wrap → single line (current Phase 1 behavior)
        return format!("{leading}{bullet}{}", apply_inline(content_after_marker, &self.styles));
    };

    let bullet_visible = "• ";  // 2 columns
    let subseq_indent = "  ";   // 2 spaces to align under text
    let effective_width = (wrap_width as usize).saturating_sub(leading.len() + bullet_visible.len());

    let wrapped = textwrap::wrap(content_after_marker, textwrap::Options::new(effective_width));

    let styled_bullet = ansi_wrap(bullet_visible, self.styles.list_bullet);
    let mut out = String::new();
    for (i, wline) in wrapped.iter().enumerate() {
        if i == 0 {
            out.push_str(&format!("{leading}{styled_bullet}{}", apply_inline(wline, &self.styles)));
        } else {
            out.push_str(&format!("\n{leading}{subseq_indent}{}", apply_inline(wline, &self.styles)));
        }
    }
    out
}
```

Same pattern for `render_numbered` (subsequent indent width = digits + `. ` = variable), `render_task` (subsequent indent = 4 spaces for `[ ] `), and `render_blockquote` (subsequent indent = styled `│ ` prefix, same styling as first line).

**Interaction with existing `wrap_line`:** The current `wrap_line` (markdown.rs:192-198) is used for code lines and paragraphs. It sets `initial_indent` but not `subsequent_indent`, so paragraphs already wrap without hanging indent — that's correct (paragraphs should wrap flush-left). Only list/blockquote block renderers need the new hanging-indent path; leave `wrap_line` alone.

**Nested lists:** the existing `leading` whitespace preservation from Phase 1 continues to work — subsequent-indent gets prepended AFTER the leading, so a nested list item wraps correctly under its own bullet.

**Headings:** intentionally NOT wrapped with hanging indent. If a heading is longer than wrap width, it wraps flush-left (via existing `wrap_line`). Headings are usually short; hanging indent under `##` would look odd.

**`wrap_width = None`:** all renderers skip wrapping entirely and emit a single line, matching current Phase 1 behavior. Users who want wrapping set the `wrap: auto` config.

### `finalize()` method

New method on `MarkdownRender`:

```rust
pub fn finalize(&mut self) -> String {
    match self.table_state.take() {
        None => String::new(),
        Some(TableState::PendingHeader(line)) => self.render_as_paragraph(&line),
        Some(TableState::Active { header, alignments, rows }) => {
            self.render_table(header, alignments, rows)
        }
    }
}
```

Called by:
1. **`stream.rs`** at `SseEvent::Done` — before `break 'outer`, emit `render.finalize()` output.
2. **`app_config.rs::print_markdown`** — after `markdown_render.render(text)`, append `finalize()` output.
3. **`session.rs::Session::render`** — after `render.render(text)`, append `finalize()` output.

Each is a single-line addition.

## Consumers Touched (Phase 2)

| File | Change |
|---|---|
| `src/render/markdown.rs` | Add table state, detection, rendering (~250 lines) |
| `src/render/stream.rs` | Call `render.finalize()` on SseEvent::Done (~2 lines) |
| `src/config/app_config.rs` | Call `finalize()` after `render()` in `print_markdown` (~1 line) |
| `src/config/session.rs` | Call `finalize()` after `render()` in `Session::render` (~1 line) |
| `Cargo.toml` | Add `comfy-table` dependency |

## Phase 2 Implementation

### Phase 2.1 — Add `comfy-table` + `table_border` style
- [x] Add `comfy-table = { version = "7.2.2", features = ["custom_styling"] }` to `Cargo.toml`
- [x] Add `table_border: Color` field to `MarkdownStyles`
- [x] Resolve in `MarkdownStyles::from_theme` from `punctuation.definition.table.markdown` with fallback chain
- [x] Handle `theme.is_none()` → default color
- [x] Test: `table_border` resolves correctly with built-in theme
- [x] Test: fallback chain works with minimal theme

**Commit:** `feat(render): add comfy-table dependency and table border style`

### Phase 2.2 — Table row detection
- [x] Add `TABLE_ROW` and `TABLE_SEPARATOR` regexes
- [x] Add `TableRow` and `TableSeparator` variants to `LineKind`
- [x] Extend `detect_line_kind` (separator check before row check)
- [x] Test: header row (`| a | b |`) → `TableRow`
- [x] Test: separator (`|---|---|`) → `TableSeparator`
- [x] Test: separator with alignment (`|:--|--:|:-:|`) → `TableSeparator`
- [x] Test: non-table pipe line in prose (`use \`a | b\``) → `Paragraph` (only if it doesn't match `^\s*\|.*\|\s*$` — verify)
- [x] Test: empty cells (`| | |`) → `TableRow`

**Commit:** `feat(render): detect markdown table rows and separators`

### Phase 2.3 — Cell + alignment parsing
- [x] Add `parse_table_row(line) -> Vec<String>`
- [x] Add `parse_alignments(separator_row) -> Vec<CellAlignment>`
- [x] Test: `| a | b | c |` → `["a", "b", "c"]`
- [x] Test: empty cells `| a | | c |` → `["a", "", "c"]`
- [x] Test: alignments `|:---|---:|:---:|---|` → `[Left, Right, Center, Left]`
- [x] Test: leading/trailing whitespace stripped

**Commit:** `feat(render): parse table cells and column alignments`

### Phase 2.4 — Table rendering via comfy-table
- [x] Add `TableState` enum (PendingHeader / Active)
- [x] Add `table_state: Option<TableState>` field to `MarkdownRender`, init `None`
- [x] Implement `render_table(header, alignments, rows) -> String`
- [x] Apply `apply_inline` to each cell; apply bold + heading color to header cells
- [x] Set alignment per column
- [x] Set width from `wrap_width` if present
- [x] Investigate comfy-table border color API; if insufficient, post-process box-drawing chars with regex to apply `table_border` color
- [x] Test: 3x3 table with default alignment
- [x] Test: alignment specifiers applied correctly
- [x] Test: header rendered with bold + heading color
- [x] Test: borders rendered with `table_border` color
- [x] Test: cell containing inline markdown (`**bold**`, `` `code` ``, `[link](url)`) — width computed correctly (ANSI stripped)
- [x] Test: wide chars / emoji in cells

**Commit:** `feat(render): render markdown tables with comfy-table`

### Phase 2.5 — State machine + finalize
- [x] Implement `handle_table_state(line, kind) -> Option<String>` for state transitions
- [x] Wire into `render_line_mut` BEFORE existing code/raw/rich branch
- [x] Handle all 6 transitions from the state diagram above
- [x] Implement `pub fn finalize(&mut self) -> String`
- [x] Add `finalize()` call in `src/render/stream.rs` at `SseEvent::Done` (write output)
- [x] Add `finalize()` call in `src/config/app_config.rs::print_markdown` after `render()`
- [x] Add `finalize()` call in `src/config/session.rs::Session::render` after `render()`
- [x] Test: table followed by paragraph → rendered table + paragraph
- [x] Test: table at end of input (no trailing non-table line) → `finalize()` emits rendered table
- [x] Test: `|...|` line NOT followed by separator → both flushed as paragraphs
- [x] Test: multiple tables in one input
- [x] Test: `render_line` on partial `| foo | ba` (immutable) → raw text (no state mutation)

**Commit:** `feat(render): wire table state machine and finalize hook`

### Phase 2.6 — Hanging-indent line wrapping for lists and blockquotes
- [x] Add `wrap_with_hanging_indent(content, prefix_width, wrap_width) -> Vec<String>` helper (uses `textwrap` on plain content, callers apply inline styling per line)
- [x] Refactor `render_bullet` to compute prefix width (`• ` = 2), wrap, apply inline per line, prepend styled bullet + subsequent 2-space indent
- [x] Refactor `render_numbered` to compute prefix width from digit count + `. `, wrap, apply inline per line, prepend styled number + subsequent variable-width indent
- [x] Refactor `render_task` to compute prefix width (`[ ] ` = 4), wrap, apply inline per line, prepend styled checkbox + subsequent 4-space indent
- [x] Refactor `render_blockquote` to wrap, apply inline per line, prepend styled `│ ` on every line (both initial and subsequent)
- [x] `wrap_width = None` path: skip wrapping, emit single line (matches Phase 1)
- [x] Preserve leading whitespace (nested list indent) — subseq indent goes AFTER leading
- [x] Test: bullet with content wider than wrap_width → hanging indent under text
- [x] Test: numbered list with 2+ digit numbers (`10. `, `100. `) → subseq indent matches digit width
- [x] Test: task item wraps with 4-space subseq indent
- [x] Test: blockquote wraps with `│ ` continuation prefix (styled same as first line)
- [x] Test: nested bullet (`  - inner text that wraps`) → nested indent + hanging indent both applied
- [x] Test: content with inline markdown that wraps mid-span — wrap boundary respects word breaks, not ANSI escapes
- [x] Test: `wrap_width = None` → no wrapping (single line, current behavior)

**Commit:** `feat(render): hanging-indent line wrapping for lists and blockquotes`

### Phase 2.7 — Integration + edge cases
- [x] Test: markdown with mixed content (paragraphs + headings + tables + lists)
- [x] Test: `raw_markdown: true` bypasses table rendering (renders as raw pipe rows via syntect grammar)
- [x] Test: `theme.is_none()` → tables still render (uncolored) via comfy-table
- [x] Test: user's custom theme colors apply to borders
- [x] Test: column count mismatch (header has 3, row has 2) — verify comfy-table behavior; document expected output
- [ ] Manual REPL test: stream a response with tables, verify silent accumulation → rendered flush *(deferred — requires interactive terminal)*
- [ ] Manual REPL test: `.set raw_markdown true` reverts tables to raw *(deferred — requires interactive terminal)*
- [x] Update `.sisyphus/plans/rich-markdown-renderer.md` progress log noting Phase 2 completion + commit SHA
- [x] `cargo check` clean
- [x] `cargo test` all pass

**Commit:** `test(render): comprehensive table rendering coverage`

## Success Criteria (Phase 2)

- [ ] Standard GFM tables render with box-drawing chars
- [ ] Alignment specifiers (`:---`, `---:`, `:---:`) respected
- [ ] Inline markdown inside cells (`**bold**`, code, links) renders correctly
- [ ] Wide chars / emoji don't misalign columns (comfy-table's `ansi_strip().width()` verified working)
- [ ] Border color from user's syntect theme
- [ ] Header row is bold + heading color
- [ ] Silent accumulation during streaming (no flashing raw→rendered transitions)
- [ ] Tables at end of stream/input flush via `finalize()`
- [ ] `|...|` lines without separator NOT rendered as tables
- [ ] Table cell content wraps within column boundary (via `comfy-table`'s `ContentArrangement::Dynamic`)
- [ ] Bullet list items wrap with 2-space hanging indent under text
- [ ] Numbered list items wrap with digit-width hanging indent
- [ ] Task list items wrap with 4-space hanging indent
- [ ] Blockquotes wrap with `│ ` continuation prefix on every line
- [ ] `wrap_width = None` disables wrapping (matches Phase 1 behavior)
- [ ] `raw_markdown: true` bypasses table rendering AND list-wrap changes (raw markdown throughout)
- [ ] `theme.is_none()` still produces functional (uncolored) tables and wrapped lists
- [ ] All existing tests pass unchanged
- [ ] `cargo check` clean
- [ ] `cargo test` all pass

## Progress Log

Append-only. One entry per commit or session.

### 2026-07-22 — Planning complete
- Verified post-Phase-1 state via explore agent (MarkdownRender struct, LineKind enum, apply_inline pipeline, streaming buffer mechanics)
- Surveyed Rust table libraries via librarian agent → chose `comfy-table 7.2.2` (only lib with correct ANSI-in-cells width handling + active maintenance + arbitrary border colors)
- Resolved 7 design decisions (library, streaming behavior, detection lookahead, border color, header styling, alignment parsing, cell inline rendering)
- Acknowledged 2 justified deviations from Phase 1 principles (multi-line state, 3 small callsite changes for finalize hook)
- Added Phase 2.6 (hanging-indent wrapping for lists and blockquotes) — user-requested addition; touches Phase 1 block renderers (render_bullet/render_numbered/render_task/render_blockquote) but reuses existing `textwrap` dep. Tables get wrapping for free via `comfy-table`'s `ContentArrangement::Dynamic`.
- Wrote this plan file
- Next: hand to Momus for review before starting Phase 2.1

### 2026-07-22 — Phase 2.1 complete (commit `fcc4a1d`)
- Added `comfy-table 7.2.2` with `custom_styling` feature to `Cargo.toml`; slotted alphabetically between `clap` and `dirs`.
- Added `table_border: Color` field to `MarkdownStyles`; resolved in `from_theme` via `punctuation.definition.table.markdown` → `punctuation` → `meta.separator` fallback chain; `none()` sets `Color::Reset`.
- Extended the three existing `MarkdownStyles` tests with `table_border` assertions (dark theme resolves ≠ Reset, minimal-root-scope theme falls back to `punctuation` color `rgb(0x77, 0x77, 0x77)`, no-theme → `Color::Reset`).
- Marked field `#[allow(dead_code)]` — will be removed in Phase 2.4 when `render_table` consumes it.
- `cargo check` clean, `cargo test` all 1207 pass.

### 2026-07-22 — Phase 2.2 complete (commit `7671d28`)
- Added `TABLE_ROW_RE` and `TABLE_SEPARATOR_RE` regexes. Separator uses `-+` (one or more dashes) instead of the plan's `{3,}` to accept the plan's own test case `|:--|--:|:-:|`; GFM spec doesn't mandate a minimum, so more lenient is safer.
- Added `TableRow` / `TableSeparator` variants to `LineKind`; extended `detect_line_kind` (separator checked before row).
- `render_markdown_line` handles both variants as `apply_inline` (paragraph-equivalent) — they'll be intercepted by the state machine in Phase 2.5 before reaching this fallback.
- 4 new tests covering row, separator (three alignment shapes), non-table pipes in prose, and separator-vs-row precedence.

### 2026-07-22 — Phase 2.3 complete (commit `c062f34`)
- Added `parse_table_row(line)` and `parse_alignments(separator_row)` free functions.
- Imported `comfy_table::CellAlignment` at module level (also used in Phase 2.4).
- Both functions marked `#[allow(dead_code)]` — consumed by state machine in Phase 2.5.
- 6 tests: cell splitting, empty cells, whitespace trimming, colon-based alignment mapping (long dashes, short dashes, default-to-left).

### 2026-07-22 — Phase 2.4 complete (commit `cdfaa0f`)
- Added `TableState` enum (`PendingHeader(String)` / `Active { header, alignments, rows }`) and `table_state: Option<TableState>` field on `MarkdownRender`.
- Implemented `MarkdownRender::render_table` using `comfy-table`'s `UTF8_FULL` preset + `ContentArrangement::Dynamic`; sets `wrap_width` on the table when present; per-column alignment via `column_mut(i).set_cell_alignment`.
- Header cells: `apply_inline` then wrapped in `.with(heading_color).bold()`. Data cells: `apply_inline` only.
- Border coloring: `colorize_box_chars` helper post-processes the rendered string. It extracts SGR prefix/suffix from a probe styled character, walks the input once, and wraps consecutive box-drawing runs (`\u{2500}..=\u{257F}`) with the SGR pair.
- `#[allow(dead_code)]` on `TableState`, `table_state`, `render_table`, `colorize_box_chars`, and (re-added) `table_border` — cleared in Phase 2.5 once the state machine wires everything in.
- 8 new tests: border colorization (with/without borders), header bold, inline markdown in cells, alignment specifiers, wide chars/emoji, border color at output start, plus a 3x3 default-alignment sanity test.

### 2026-07-22 — Phase 2.5 complete (commit `bf06d5e`)
- Added `TableAction` enum (`Consumed(String)` / `FlushAndContinue(String)` / `Passthrough`) — replaces the plan's ambiguous `Option<String>` return with an explicit three-way decision.
- Implemented `MarkdownRender::handle_table_state` covering all 7 transitions from the state diagram (including code-block entry as an implicit flush trigger).
- Implemented `MarkdownRender::render_as_paragraph` helper for false-positive header flushes.
- Implemented `pub fn finalize(&mut self) -> String`.
- `render_line_mut` runs the state machine before the code/raw/rich dispatch. `raw_markdown: true` bypasses the state machine entirely (raw mode preserves user-supplied markdown untouched). Code block entry (`is_code`) maps to `LineKind::Paragraph` for state-machine purposes, forcing a flush.
- Wired `finalize()` into three call sites:
  - `src/render/stream.rs` at `SseEvent::Done` — queues a trailing newline + flushed output via crossterm `queue!/style::Print` before break.
  - `src/config/app_config.rs::print_markdown` — appends flush output before `println!`.
  - `src/config/session.rs::Session::render` — flushes after both System and Assistant message rendering (per-message finalize prevents cross-message state bleed).
- Dropped `#[allow(dead_code)]` from `TableState`, `table_state`, `render_table`, `colorize_box_chars`, `parse_table_row`, `parse_alignments`, and `table_border` — all now live in the binary.
- 9 new tests: full-table streaming, deferred silent accumulation, `finalize` for active/pending/empty state, `|...|`-without-separator flush, multiple tables in one input, `render_line` immutability, raw-mode bypass.

### 2026-07-22 — Phase 2.6 complete (commit `d790782`)
- Added `wrap_plain_content(content, effective_width) -> Vec<String>` helper (thin `textwrap::wrap` wrapper that clamps width to ≥1).
- Added `kind_pre_wraps(kind) -> bool` helper (returns true for bullet/numbered/task/blockquote).
- Threaded `wrap_width: Option<u16>` through `render_markdown_line` and all four block renderers.
  - `wrap_width = None` short-circuits back to Phase 1 single-line behavior.
  - `wrap_width = Some(w)` wraps plain content (pre-inline-styling) at `w - (leading + prefix_width)`, then applies `apply_inline` per wrapped chunk.
- Prefix widths: `render_bullet` = 2 (`• `), `render_task` = 4 (`[ ] `), `render_numbered` = digit_count + 2 (`. `), `render_blockquote` = 2 (`│ `).
- `render_blockquote` prepends the styled `│ ` on **every** wrapped line (not just the first); the other three prepend the marker on line 1 and a spaces-only subsequent indent on continuation lines.
- Leading whitespace (nested-list indent) is emitted BEFORE the prefix on every wrapped line, preserving nested-list appearance.
- `render_rich_markdown_line` skips `wrap_line` when `kind_pre_wraps(kind)` is true, avoiding a second unwanted wrap pass over already-styled content.
- Updated all 17 existing test call sites of `render_markdown_line` (via ast-grep) to pass `None` — Phase 1 behavior preserved end-to-end.
- 8 new tests: bullet 2-space indent, numbered 4-space (`42. `) and 5-space (`100. `) indent, task 4-space indent, blockquote pipe-on-every-line, nested-bullet leading indent, `None` single-line short-circuit, inline markdown intact after wrap.

### 2026-07-22 — Phase 2.7 complete (commit `e82e5ab`)
- 4 integration/edge-case tests: mixed-content document (heading + paragraph + list + blockquote + table + trailing prose), table renders without theme, table borders pick up custom theme color, column-count mismatch tolerated by comfy-table.
- Full test suite: 1246 pass, 0 fail. `cargo check` clean.
- Manual REPL verification deferred (requires interactive terminal); test coverage validates rendering pipeline end-to-end.
- Phase 2 complete.
