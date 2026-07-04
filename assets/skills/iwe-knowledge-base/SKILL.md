---
description: Navigate and curate markdown knowledge bases (plan repos, spec repos, companion docs) with IWE graph tools. Load when the workspace is or contains a markdown knowledge base and the task involves finding, reading, or reorganizing plans, specs, designs, or notes. Activates the iwe MCP server rooted at the current directory.
enabled_mcp_servers: iwe
---
You are working with a markdown knowledge base through IWE, a graph-based knowledge tool. The `iwe` MCP server is rooted at the current working directory (`--project .`), so the knowledge base is the directory Coyote was launched in. IWE derives structure from links: a link on its own line is an *inclusion link* (parent-child hierarchy); a link inside text is an *inline reference* (cross-reference, produces backlinks). The server watches the filesystem, so external edits are picked up automatically — never ask for a restart.

## When to use this (and when not)

Use IWE tools when the task involves a corpus of markdown documents: plan repositories, spec/design collections, companion docs repos, meeting notes, PKM vaults.

Do NOT use IWE tools for:

- **Agent memory** (`.coyote/memory/`, `COYOTE.md`) — use the `memory__*` tools; they own the index conventions there.
- **Semantic/similarity search over documents** — that is RAG's job. IWE search is fuzzy title/key matching plus structural traversal, not embeddings.
- **Source code** — IWE only understands markdown.

If unsure whether the current directory is actually a knowledge base, probe with `iwe_stats` first. Few or zero documents means this skill does not apply; unload it rather than forcing the tools.

## Orientation protocol (always start here)

Never guess document keys. Orient first:

1. `iwe_stats` — corpus size and shape. Cheap sanity check.
2. `iwe_find(query="<topic>")` — fuzzy search for entry points. Use `roots` behavior via structural selectors when you want top-level topics only.
3. `iwe_tree(key="<entry>", max_depth=2)` — see the hierarchy before reading bodies.
4. `iwe_retrieve(key="<entry>", depth=1, context=1)` — read with structure.

## Reading efficiently

`iwe_retrieve` is the workhorse. Control cost explicitly:

- `depth` — how many levels of included children to expand. Start at 1-2; increase only if needed.
- `context` — parent levels to include, so you know where a document sits. `context=1` is usually enough.
- `max_tokens` — ALWAYS set a budget (e.g. 2000-4000) on large corpora; results report truncation so you can drill further deliberately.
- `exclude` — pass keys you have already read to avoid re-retrieving known content.
- `links` / `backlinks` — include outbound/inbound references when tracing how a topic connects.

Scope searches structurally with selectors on `iwe_find`/`iwe_retrieve`/`iwe_tree`:

- `in` — only sub-documents of EVERY listed key (AND)
- `in_any` — sub-documents of at least one key (OR)
- `not_in` — exclude subtrees (e.g. archives)

Filter by frontmatter with the YAML query language: `status: draft`, `created: {$gte: "2026-01-01"}`, `tags: {$in: [urgent]}`, `reviewed: {$exists: true}`.

Use `iwe_squash(key=...)` to flatten a subtree into one linear document — good for producing a full plan readout or summary input.

## Writing and refactoring

Write tools: `iwe_create` (new doc from title + content), `iwe_update` (replace a doc's content), `iwe_delete` (remove + clean up references). Refactor tools: `iwe_rename` (key rename with automatic link updates everywhere), `iwe_extract` (split a section into its own doc, leaving an inclusion link), `iwe_inline` (merge a referenced doc back into its parent), `iwe_normalize` (reformat all docs consistently).

Rules:

- **Preview destructive operations**: `iwe_rename`, `iwe_delete`, `iwe_extract`, `iwe_inline`, and `iwe_normalize` support `dry_run` — use it first, show the user what will change, then apply.
- Never rename or delete by editing files directly; the refactor tools update every referencing document, manual edits break links.
- When adding a document, link it from an existing parent (inclusion link on its own line) so it joins the hierarchy instead of becoming an orphan.
- Match the corpus conventions: check an existing document's frontmatter fields before inventing your own schema.
- Do not run `iwe_normalize` across someone's knowledge base unprompted — it rewrites every file's formatting.

## Anti-patterns

- Retrieving with `depth=5` and no `max_tokens` "to get everything" — you will flood the context. Iterate: shallow first, drill selectively.
- Calling `iwe_find` repeatedly with rephrased queries when structural navigation (`iwe_tree`, selectors) would locate the document deterministically.
- Using IWE write tools on `.coyote/memory/` files — wrong tier; that corrupts the memory index.
- Creating documents without linking them into the hierarchy — orphans are invisible to depth-based retrieval.
