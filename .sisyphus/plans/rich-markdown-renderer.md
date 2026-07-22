# Rich Markdown Renderer for the REPL

**Status:** Planning complete, awaiting Momus review before implementation.
**Owner:** Coyote maintainer
**Estimated effort:** Phase 1 = 4-5 days, Phase 2 (tables) = +1-2 days
**Related flag:** `raw_markdown` (already plumbed; see commit history for the plumbing PR)

---

## Goal

Replace Coyote's current syntect-only markdown rendering with a rich renderer that transforms markdown syntax into styled terminal output (headings become colored + bold text, `**bold**` becomes actual bold, backticks strip and stylize, blockquotes get a `│` prefix, etc.), matching glamour's structural output while preserving the user's existing syntect `.tmTheme` colors.

The current renderer just applies syntect's markdown grammar for syntax highlighting — the markdown syntax characters (`#`, `**`, `` ` ``) stay in the output, just colored. Users get raw markdown with color, not rendered markdown. The new renderer actually transforms the markdown into styled output like glamour (github.com/charmbracelet/glamour) does.

## Non-Goals

- **Not replacing the renderer's public API.** `MarkdownRender::init`, `render`, `render_line`, and `RenderOptions` all keep their existing signatures. Callers (`stream.rs`, `session.rs`, `app_config.rs::print_markdown`, `request_context.rs::session_info`) do not change.
- **Not changing streaming architecture.** `stream.rs` still calls `render()` on complete lines and `render_line()` on the incomplete tail. New renderer must fit this line-by-line contract.
- **Not touching code block rendering.** Fenced code blocks (` ```lang ... ``` `) continue to route to syntect language-specific highlighting via `find_syntax_by_token`. The new renderer only affects markdown syntax rendering, never code content.
- **Not adding new dependencies.** All work uses existing `syntect`, `fancy-regex`, `crossterm`, `textwrap`.
- **Not shipping tables in Phase 1.** Tables require multi-line buffering, which conflicts with the stateless streaming model. Table rows render as raw `| col | col |` until Phase 2.
- **Not implementing OSC 8 hyperlink fallback logic.** Emit OSC 8 codes unconditionally + always show URL visibly. Terminals that don't support OSC 8 strip the codes and see plain "text URL" text.

## Design Principles

1. **Colors from user theme, layout from glamour.** Every construct extracts its color from the user's syntect theme via scope lookup with fallback chains. The structural layout (prefixes, indents, borders, box-drawing) matches glamour's default dark style.
2. **`raw_markdown: true` = current behavior byte-identical.** The existing syntect-on-markdown-grammar path is preserved as the "raw" branch and reachable via config/CLI/REPL. Zero regression risk for users who want the old behavior.
3. **Preserve line-by-line rendering.** No state beyond the existing `LineType` code-block tracker. Stateless per-line rendering means the streaming's `render_line` for partial buffer works identically to the mutating `render_line_mut` for complete lines.
4. **Regex-based inline parsing, not pulldown-cmark.** A full markdown parser needs the complete document to disambiguate. Regexes match balanced spans and gracefully leave unclosed spans as raw text — exactly right for streaming's mid-token partial-line rendering.
5. **Only `src/render/markdown.rs` changes.** Scope containment: the entire implementation lives in one file. No touches to `stream.rs`, `mod.rs`, `session.rs`, `app_config.rs`, `request_context.rs`.

## Resolved Design Decisions

Recorded here so future sessions don't re-litigate them:

1. **Tables:** deferred to Phase 2. Phase 1 leaves table rows as raw markdown.
2. **H2-H6 hash prefixes:** matched to glamour — keep `##`, `###`, `####`, `#####`, `######` visible in the heading color as a level indicator. H1 gets padded ` text ` treatment.
3. **Link rendering:** OSC 8 hyperlink codes wrapping visible `{text} {url}` — modern terminals show a clickable link, older terminals show plain styled text. Matches glamour exactly. Users on broken terminals can fall back to `.set raw_markdown true`.

## Architecture

### Data structures (added to `MarkdownRender`)

```rust
struct MarkdownStyles {
    heading: (Color, bool /* force_bold */),
    bold: Color,
    italic: Color,
    inline_code_fg: Color,
    inline_code_bg: Option<Color>,
    blockquote: Color,
    list_bullet: Color,
    link_text: Color,
    link_url: Color,
    strikethrough: Color,
    hrule: Color,
}
```

Populated once in `MarkdownRender::init` via a new `resolve_scope_style(theme, primary_scope, fallbacks)` helper that generalizes the existing `get_code_color()` pattern (markdown.rs:299).

When `options.theme.is_none()`, all styles collapse to defaults (raw text output with no colors — matches current behavior).

### Line-type detection

Extended `check_line` returns a new `LineKind` enum (only for non-code lines):

| Regex | LineKind |
|---|---|
| `^\s*(#{1,6}) +.+` | Heading(level) |
| `^\s*> ?.*` | Blockquote |
| `^(\s*)- \[[ xX]\] +.+` | TaskItem(checked) |
| `^(\s*)[-*+] +.+` | BulletItem |
| `^(\s*)\d+\. +.+` | NumberedItem |
| `^\s*(-{3,}|_{3,}|\*{3,})\s*$` | HorizontalRule |
| `^\s*\|.*\|\s*$` | (Phase 2: TableRow) — treated as paragraph for now |
| default | Paragraph |

**Stateless:** line-type detection carries no state beyond the existing `prev_line_type`/`code_syntax` fields for code block tracking. Streaming's partial-line `render_line` works identically to complete-line `render_line_mut`.

### Block-level rendering

Each `LineKind` triggers a block transformation that strips syntax markers and applies structural styling. All block types then run their remaining text content through the inline pipeline.

| LineKind | Transformation |
|---|---|
| `Heading(1)` | Prefix ` `, suffix ` ` (single spaces), apply bold + heading color to entire line |
| `Heading(2..=6)` | Keep visible `##`/`###`/etc. prefix, apply bold + heading color |
| `Blockquote` | Replace `> ` with `│ ` (styled blockquote color); apply blockquote color to remaining content |
| `BulletItem` | Replace `-`/`*`/`+` with `•` (styled list_bullet color); preserve leading whitespace for nesting |
| `NumberedItem` | Preserve number, style the `.` in list_bullet color |
| `TaskItem(false)` | Replace `[ ]` with `[ ]` styled in list_bullet color |
| `TaskItem(true)` | Replace `[x]` with `[✓]` styled |
| `HorizontalRule` | Emit `────────` (8-char box-drawing) styled with hrule color (typically dim/gray) |
| `Paragraph` | No block transform, inline pass only |

### Inline rendering (regex pipeline, applied in order)

Order matters — inline code first prevents re-parsing code content as bold/italic:

1. **Inline code** (`` `text` ``) — regex `` `([^`\n]+)` ``, strip backticks, apply `inline_code_fg` + optional `inline_code_bg`.
2. **Images** (`![alt](url)`) — regex `!\[([^\]]*)\]\(([^)]+)\)`, emit `Image: {alt} → {url}` styled with `link_url`. Wrap in OSC 8 hyperlink codes.
3. **Links** (`[text](url)`) — regex `\[([^\]]+)\]\(([^)]+)\)`, emit `{text} {url}` with `link_text` on the label and `link_url` on the URL. Wrap in OSC 8 hyperlink codes.
4. **Bold** (`**text**` or `__text__`) — regex `\*\*([^*\n]+)\*\*` and `__([^_\n]+)__`, strip markers, apply bold ANSI + `bold` color.
5. **Italic** (`*text*` or `_text_`) — regex `(?<![*\w])\*([^*\n]+)\*(?!\*)` and `(?<![_\w])_([^_\n]+)_(?!_)` — lookbehind/lookahead prevents word-internal `_` from matching (e.g., `some_var_name`). `fancy-regex` supports lookbehind.
6. **Strikethrough** (`~~text~~`) — regex `~~([^~\n]+)~~`, strip markers, apply ANSI strikethrough (`\x1b[9m`).

**Partial-span handling for streaming:** regexes only match balanced spans. Unclosed spans (`**bold` with no closing) stay raw. When the closing marker arrives on the next token, the complete-line pass renders the full span correctly.

### OSC 8 hyperlinks

```
\x1b]8;;{url}\x1b\\{visible_text}\x1b]8;;\x1b\\
```

Emit unconditionally around links and images. Unsupported terminals strip the codes and see plain visible text. Zero degradation.

### Branching in `highlight_line`

```rust
fn highlight_line(&self, line: &str, syntax: &SyntaxReference, is_code: bool) -> String {
    if is_code {
        // unchanged — code block content via language-specific syntect
        self.highlight_code_syntect(line, syntax)
    } else if self.options.raw_markdown {
        // preserved current behavior: syntect on markdown grammar
        self.highlight_markdown_syntect(line, &self.md_syntax)
    } else {
        // new rich rendering path
        self.render_markdown_line(line)
    }
}
```

Code blocks route to syntect regardless of `raw_markdown` — the flag only affects markdown syntax rendering.

## Consumers Verified

Complete map of `MarkdownRender` consumers (from explore agent research). All continue to work without modification because the public API is unchanged:

1. `src/render/mod.rs:16-33` — `render_stream()` (streaming path via `markdown_stream()`)
2. `src/render/stream.rs:67-171` — `markdown_stream_inner()` calls `render.render(head)` and `render.render_line(&buffer)`
3. `src/config/app_config.rs:420-429` — `print_markdown()` (CLI one-shot)
4. `src/config/request_context.rs:1706-1723` — `session_info()` (`.info` REPL command)
5. `src/config/session.rs:278-396` — `Session::render()` (per assistant message)
6. `src/render/markdown.rs:311-397` — existing tests

## Phase 1 Implementation

### Phase 1.1 — Scope lookup helper + precomputed styles
- [x] Add `resolve_scope_style(theme, primary, fallbacks)` helper (generalizes `get_code_color()`)
- [x] Add `MarkdownStyles` struct + populate in `MarkdownRender::init` for all 10 constructs
- [x] Handle `theme.is_none()` gracefully (all styles = defaults)
- [x] Test: verify each style resolves correctly with the built-in dark theme
- [x] Test: verify each style falls back correctly with a minimal theme that only defines root scopes

**Commit:** `feat(render): precompute markdown scope styles for rich rendering`

### Phase 1.2 — Line-type detection
- [x] Add `LineKind` enum + `detect_line_kind()` function
- [x] Wire into `check_line` — return `LineKind` alongside existing `LineType`
- [x] Test each pattern in isolation (heading, blockquote, bullets, numbered, task, hrule, paragraph)
- [x] Test edge cases: `## ` vs `##text` (no space, not a heading), indented list items, empty blockquote

**Commit:** `feat(render): detect markdown block-level line types`

### Phase 1.3 — Inline rendering pipeline
- [x] Add regex constants (LazyLock) for each inline construct
- [x] Add `apply_inline(text: &str, styles: &MarkdownStyles) -> String` that runs the pipeline in order
- [x] Test each construct in isolation
- [x] Test order-dependence: `**foo `bar` baz**` — bold wraps inline code correctly
- [x] Test partial spans stay raw: `**unclosed` → `**unclosed`
- [x] Test italic doesn't false-positive: `some_var_name`, `a * b * c` (math-like expression)
- [x] Test OSC 8 emission for links and images

**Commit:** `feat(render): rich inline markdown rendering (bold, italic, code, links)`

### Phase 1.4 — Block-level rendering
- [x] Add `render_markdown_line(line)` that dispatches on `LineKind`
- [x] Implement each block transform (heading, blockquote, bullet, numbered, task, hrule, paragraph)
- [x] After block transform, always run `apply_inline` on the content
- [x] Test each block type with inline styling nested inside (bold in heading, code in list item, link in blockquote)

**Commit:** `feat(render): rich block-level markdown rendering (headings, quotes, lists, hr)`

### Phase 1.5 — Wire into `highlight_line` with `raw_markdown` branch
- [x] Refactor `highlight_line` to branch on `options.raw_markdown`
- [x] Remove `#[allow(dead_code)]` from `RenderOptions::raw_markdown`
- [x] Verify all existing tests pass with `raw_markdown: true` (byte-identical output)
- [ ] Manual REPL test: send a message with a mix of constructs, verify output matches expectations
- [ ] Manual streaming test: verify no flashing, partial spans render smoothly

**Commit:** `feat(render): activate rich markdown renderer as default`

### Phase 1.6 — Test coverage
- [x] Heading levels 1-6 (transforms + styling)
- [x] Bold, italic, inline code, strikethrough
- [x] Inline code strips backticks
- [x] `some_var_name` NOT italicized
- [x] `a * b * c` math not italicized
- [x] Blockquote `│ ` prefix
- [x] Bullet `•` transformation
- [x] Numbered list preservation
- [x] Task items `[ ]` / `[✓]`
- [x] Horizontal rule
- [x] Links: styled text + URL, OSC 8 codes present
- [x] Images: `Image: {alt} → {url}` format, OSC 8 codes present
- [x] Nested inline in blocks (bold in heading, code in list)
- [x] Partial spans in `render_line`
- [x] `theme=None` degrades to raw stripped text (no colors, but syntax stripped)
- [x] `raw_markdown=true` matches current behavior byte-for-byte

**Commit:** `test(render): comprehensive coverage for rich markdown renderer`

## Phase 2 (Follow-up PR) — Tables

Deferred scope. Rough sketch:

- Add `Option<TableBuffer>` field to `MarkdownRender`
- On table row detection, accumulate rows in buffer (emit raw markdown for now to keep streaming visible)
- On non-table line (or blank), flush the buffer: compute column widths, render with box-drawing chars, emit
- Handle streaming: use cursor-erase to replace raw rows with rendered table when buffer flushes
- Test coverage: single-column, multi-column, alignment specifiers (`:---`, `---:`, `:---:`), empty cells, long content wrapping

## Success Criteria (Phase 1)

- [x] All existing tests pass with `raw_markdown: true`
- [x] All new tests pass with `raw_markdown: false`
- [x] `cargo check` clean
- [x] `cargo test` all pass
- [ ] Manual REPL test: streaming looks smooth (no flashing, no visible partial spans getting re-rendered)
- [ ] Manual REPL test: `.set raw_markdown true` reverts to current behavior
- [ ] Manual test: user's custom theme colors apply to headings/bold/etc. (not just default)

## Progress Log

Append-only. One entry per commit or session.

### 2026-07-22 — Planning complete
- Scoped implementation via research (glamour source, syntect scope conventions, current renderer consumers)
- Resolved 3 open design questions (tables deferred, glamour hash prefixes matched, OSC 8 with fallback)
- Wrote this plan file
- Next: hand to Momus for review before starting Phase 1.1

### 2026-07-22 — Phase 1.1 complete (`d2940a8`)
- Added `resolve_scope_style` helper + `MarkdownStyles` struct with 10 constructs, precomputed once in `MarkdownRender::init`; new struct is `#[allow(dead_code)]` until Phase 1.5 wires it in. 6 new tests cover primary/fallback/default paths, `theme.is_none()`, built-in dark theme, and a minimal-root-scopes theme.

### 2026-07-22 — Phase 1.2 complete (`f40ba4c`)
- Added `LineKind` enum (Heading/Blockquote/TaskItem/BulletItem/NumberedItem/HorizontalRule/Paragraph) and `detect_line_kind()` using `fancy_regex` for the 6 block patterns. Wired into `check_line` — signature now returns `(LineType, LineKind, Option<SyntaxReference>, bool)`; callers ignore `LineKind` with `_` until Phase 1.4. 8 new tests cover each pattern plus edge cases (`##notheading`, `-nospace`, `--`, indented items, empty blockquote).

### 2026-07-22 — Phase 1.3 complete (`89db5b3`)
- Added inline regexes (INLINE_CODE, IMAGE, LINK, BOLD_AST, BOLD_US, ITALIC_AST, ITALIC_US, STRIKETHROUGH, CODE_PLACEHOLDER) and `apply_inline()` running the plan's 6-step pipeline. Refined italic regexes with `(?!\s)` opener + `(?<!\s)` closer to prevent `a * b * c` false-positives while still requiring the word-boundary lookbehind for `some_var_name`. Inline code is masked with `\x00C{idx}\x00` placeholders before other transforms so its content is never re-parsed. Links/images wrap in OSC 8 hyperlink codes. 15 new tests cover each construct, order-dependence, partial spans, italic false-positives, OSC 8 emission, and image-before-link ordering.

### 2026-07-22 — Phase 1.4 complete (`9890cf0`)
- Added `render_markdown_line(line, kind, styles)` dispatcher plus per-`LineKind` block renderers (`render_heading`, `render_blockquote`, `render_bullet`, `render_numbered`, `render_task`, `render_hrule`). H1 gets space-padded, H2-6 keep their `##...` prefix; blockquotes get `│ `; bullets → `•`; numbered items keep the number and style only the `.`; task items → `[ ]` / `[✓]`; hrules render as `────────`. All block variants delegate leftover content to `apply_inline`. 15 new tests cover each block type, indent preservation, and nested inline (code in bullet, link in blockquote).

### 2026-07-22 — Phase 1.5 complete (`d65d63e`)
- Wired the rich renderer into `render_line` / `render_line_mut`: code lines still route to syntect; non-code lines branch on `options.raw_markdown` (true → existing markdown-grammar syntect path, false → `render_rich_markdown_line`). Removed `#[allow(dead_code)]` from `RenderOptions::raw_markdown`, `MarkdownStyles`, `LineKind`, `detect_line_kind`, `render_markdown_line`, `apply_inline`, and the `styles` field. Updated the 3 existing tests (`no_theme`, `no_wrap_code`, `wrap_all`) to set `raw_markdown: true` — they still produce byte-identical output, proving the raw path is preserved. Manual REPL/streaming tests deferred to user.

### 2026-07-22 — Phase 1.6 complete (`b0eeba1`)
- 6 more tests filling out the coverage checklist: bold nested inside a heading, partial bold/link spans via `render_line` (streaming path), rich rendering with `theme=None` still strips syntax and emits block glyphs, rich vs raw paths diverge on the same input, and fenced code blocks still route through syntect. Total: 55 markdown tests, 1207 total tests pass, `cargo check` clean.

### 2026-07-22 — Phase 2 complete (Phase 1 successor shipped)
- Phase 2 (tables + hanging-indent list/blockquote wrapping) is complete on top of this foundation. See `rich-markdown-renderer-tables.md` for the full plan and per-sub-phase progress log. Final commits: `fcc4a1d` (2.1) → `7671d28` (2.2) → `c062f34` (2.3) → `cdfaa0f` (2.4) → `bf06d5e` (2.5) → `d790782` (2.6) → `336b374` (2.7). Total markdown tests grew from 55 → 94; total test suite 1207 → 1246, all passing, `cargo check` clean.
