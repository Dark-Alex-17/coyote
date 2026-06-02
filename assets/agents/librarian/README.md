# Librarian

The "external grep" sibling of [Explore](../explore/README.md). Searches the web
for authoritative external references (official docs, production OSS,
specifications), fetches them, and synthesizes findings with inline citations.

Designed to be delegated to by **[Sisyphus](../sisyphus/README.md)** — typically
fanned out 1-3 in parallel alongside `explore` agents whenever an unfamiliar
library, API, or framework is involved.

## Workflow

```
search (llm + ddg-search)         identify 3-5 authoritative sources
   ↓
synthesize (llm + fetch_url_via_curl)   fetch, extract, cite, synthesize
   ↓
end_success / end_failure         LIBRARIAN_COMPLETE / LIBRARIAN_FAILED
```

Iteration 1 (this) is the happy-path MVP: single search pass, single synthesis
pass, no quality-check loop. Future iterations may add:

- `quality_check` LLM node + back-edge to `search` with a refined query if
  the initial findings are thin or off-topic
- `gh` CLI / GitHub MCP integration for first-class OSS-example retrieval
- Reranking the search results before synthesis
- Cache of recently-fetched URLs across invocations

## Trigger phrases (when sisyphus should spawn it)

- "How do I use [library]?"
- "What's the best practice for [framework feature]?"
- "Why does [external dependency] behave this way?"
- "Find examples of [library] usage"
- Any unfamiliar npm/pip/cargo/crate package surfaced by the user

## Source priority

1. Official documentation (docs.X.org, readthedocs.io, MDN, vendor docs)
2. Production OSS examples (1000+ stars on GitHub)
3. Specifications (RFCs, W3C, ECMA, IEEE)
4. Credible secondary references — only when 1-3 are sparse

Explicitly excluded: random blog posts, marketing pages, stale tutorials,
"what is X" beginner articles (unless that is literally the user's question).

## Outcomes

- `LIBRARIAN_COMPLETE` — found and synthesized authoritative sources. Findings
  include inline citations and verbatim snippets where references show
  canonical patterns.
- `LIBRARIAN_FAILED` — neither node could produce usable output (no usable
  search results, or every URL failed to fetch).

## Pro-Tip: Override search/fetch tooling

The MVP uses `ddg-search` for search and `fetch_url_via_curl` for retrieval. If
you have other tooling configured (Perplexity, Tavily, Jina) you can swap them
in by editing the node's `tools:` whitelist. Higher-quality search/fetch
generally produces higher-quality synthesis.
