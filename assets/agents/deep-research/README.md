# deep-research

A deep web research agent, built as a Loki graph agent. It plans an
investigation, decomposes it into sub-questions researched in
parallel, grounds the work in a local knowledge corpus, vets the
credibility of cited sources, runs a reflexion self-critique loop to
revise weak findings, delegates the final write-up to a focused
sub-agent, checks that the cited sources are reachable, and gates the
result behind human approval.

Unlike a regular agent (which takes a goal and improvises the steps),
this agent runs a fixed graph: every request goes through the same
`plan -> parallel research -> vet -> critique -> synthesize -> verify -> approve`
pipeline.

This agent is also the **canonical reference for the Loki graph
system**: it exercises every node type (`script`, `llm`, `rag`, `map`,
`agent`, `input`, `approval`, `end`) and both static fan-out and
dynamic `map` fan-out. If you are learning how to build a graph
agent, this is the file to read alongside the
[Graph-Agents wiki](https://github.com/Dark-Alex-17/loki/wiki/Graph-Agents).

## Workflow

17 nodes. `->` is the static route; a script node can also route
dynamically via `_next`. The `▶▶` line is a parallel super-step —
those branches run concurrently:

```
parse_request (script)              -> bootstrap_research   (or -> ask_topic if no topic)
ask_topic (input)                   -> bootstrap_research
bootstrap_research (script)         -> [plan, knowledge_lookup]   ▶▶ parallel
plan (llm + output_schema)          -> research_each_question
knowledge_lookup (rag)              -> research_each_question
research_each_question (map)        -> combine_findings    (spawns one branch per question)
  └─ research_one_question (llm)    (atomic; runs N×, joins at map)
combine_findings (script)           -> vet_sources
vet_sources (llm + custom tool)     -> critique
critique (llm)                      -> reflexion_gate
reflexion_gate (script)             -> synthesize  (or -> research_each_question: reflexion loop)
synthesize (agent: report-writer)   -> verify_sources
verify_sources (script)             -> approve
approve (approval)                  -> end_accepted          ("accept")
                                    -> end_rejected          ("reject")
                                    -> incorporate_feedback   (any free-form answer)
incorporate_feedback (script)       -> research_each_question (the human-feedback loop)
```

### Node-type breakdown

| Type | Nodes |
|---|---|
| `script` (Python) | `parse_request`, `bootstrap_research`, `combine_findings`, `reflexion_gate`, `verify_sources`, `incorporate_feedback` |
| `llm` (tools: `[]`) | `plan`, `critique` |
| `llm` (with tool whitelist) | `research_one_question`, `vet_sources` |
| `rag` | `knowledge_lookup` — local corpus retrieval |
| `map` | `research_each_question` — dynamic fan-out per sub-question |
| `agent` | `synthesize` — spawns the `report-writer` sub-agent |
| `input` | `ask_topic` |
| `approval` | `approve` |
| `end` | `end_accepted`, `end_rejected` |

## Parallel execution

The graph has two parallel super-steps where Loki's BSP scheduler runs
branches concurrently.

**1. Context loading (`plan` ‖ `knowledge_lookup`)** — after
`bootstrap_research`, the LLM planner (which decomposes the topic into
sub-questions) and the RAG retrieval over the local `knowledge/`
corpus run side by side. They write disjoint state keys (`plan` writes
`research_plan` and `questions`; `knowledge_lookup` writes
`local_context` and `local_sources`) so no reducer is needed.

**2. Per-question research (`research_each_question` map)** — the
plan emits a `questions` array (3-5 entries, enforced by its
`output_schema`). The `map` node spawns one parallel branch per
question (`max_concurrency: 3`). Each branch is an isolated
`research_one_question` LLM invocation with web tools, instructed to
investigate exactly its assigned question. Outputs collect into
`question_findings` in input order, then `combine_findings` joins
them into a single `findings` Markdown document for downstream nodes.

`settings.max_concurrency: 4` is the graph-wide cap; the per-`map`
override (`max_concurrency: 3` on `research_each_question`) is
deliberately lower to leave headroom for the planner's tool calls
running alongside RAG.

## Local knowledge corpus

`knowledge_lookup` is a `rag` node — it runs hybrid (vector + keyword)
retrieval over every file in `knowledge/`. The directory ships with a
small `research-style-notes.md` so the RAG node has something to
retrieve against on a clean install; drop your own Markdown notes,
PDFs, or text files into `knowledge/` to bias the research toward
your local context.

The knowledge base is built once, at agent-load time, into
`~/.config/loki/agents/deep-research/knowledge_lookup.yaml`. Because
the node fully specifies its build config (`embedding_model`,
`chunk_size`, `chunk_overlap`), the build is non-interactive. Delete
that cached file after adding or changing knowledge to force a
rebuild.

## Sub-agent: report-writer

The `synthesize` node is an `agent` node that spawns the
`report-writer` sub-agent (`assets/agents/report-writer/`). This is
the agent-as-tool pattern: the orchestrating graph delegates the
writing phase to a focused sub-agent dedicated to coherent prose,
while the research phase uses different (typically cheaper) LLM nodes
for fast-and-many-question investigation.

The `report-writer` sub-agent has no tools — it cannot access the
web, cannot search, and cannot invent facts. It reads only the
findings it is given and produces a final Markdown report preserving
every inline citation. See `assets/agents/report-writer/README.md`
for details.

## Tools and tool scoping

This agent demonstrates Loki's three tool sources and how an `llm`
node's `tools:` whitelist scopes them per node.

The agent's full tool universe, declared in `graph.yaml`:

- **Global tools** (`global_tools`): `web_search_loki`,
  `fetch_url_via_curl`, `search_arxiv` - Loki's built-in tool scripts.
- **MCP server** (`mcp_servers`): `ddg-search` - a DuckDuckGo web
  search MCP server. Referenced in a whitelist as `mcp:ddg-search`.
- **Custom agent tool** (`tools.sh`): `classify_source` - a
  deterministic source-credibility classifier shipped with this agent.

No node receives all of these. Each `llm` node's `tools:` whitelist
narrows the universe to exactly what that step needs:

| Node | `tools:` whitelist | Draws from |
|---|---|---|
| `plan`, `critique` | `[]` | nothing - pure reasoning |
| `research_one_question` | `web_search_loki`, `fetch_url_via_curl`, `search_arxiv`, `mcp:ddg-search` | global tools + MCP |
| `vet_sources` | `classify_source` | the custom tool only |

`research_one_question` (each parallel branch of the map) can search
and fetch but cannot classify sources; `vet_sources` can classify
sources but cannot touch the web. That separation is the point of the
`tools:` whitelist: a node gets only the tools its job calls for,
never the agent's full set.

The `classify_source` custom tool (`tools.sh`) takes a URL and returns
a credibility tier (government, academic, preprint, organization,
unverified) derived from the host and top-level domain. It is
deterministic - exactly the kind of logic a tool should own rather than
the LLM guessing.

Web search may require API-key configuration; see the
[Tools](https://github.com/Dark-Alex-17/loki/wiki/Tools) docs.
`fetch_url_via_curl`, `search_arxiv`, and `classify_source` work
without a key.

## Setup

`research_one_question` (each parallel branch of the `map`) uses the
`ddg-search` MCP server via `mcp:ddg-search`. It is one of Loki's
default MCP servers; make sure it is registered in
`~/.config/loki/mcp.json` (run `loki --install mcp_config` to restore
the default template if it is missing). If `ddg-search` is unavailable,
the branches still have their global web-search tools to fall back on.

The `synthesize` node spawns the `report-writer` sub-agent. Both
agents ship with `loki agents install`; if you install one manually,
install both so the agent reference resolves.

## Reflexion

The agent has two loops, both built with script nodes that route via
`_next`. The engine allows back-edges at runtime; the validator only
rejects cycles built from static `next` / `routes` edges, so script
`_next` loops are always allowed.

**Automated reflexion loop.** After the parallel research map and
`vet_sources`, the `critique` node reviews the merged findings
against the research plan and the source credibility assessment, and
emits `VERDICT: PASS` or `VERDICT: REVISE` with specific feedback.
`reflexion_gate.py` then:

- `PASS` -> continue to `synthesize`.
- `REVISE`, budget remaining -> loop back to `research_each_question`,
  with the critique injected as `research_feedback` so every parallel
  branch sees it on the retry.
- `REVISE`, budget spent -> continue to `synthesize` anyway (the human
  approval step is the final backstop).

The budget is `MAX_REFLEXION_REVISIONS` in `reflexion_gate.py`
(default 2, so the research map runs at most 3 times per pass).

**Human-feedback loop.** At `approve` the user answers `accept`,
`reject`, or types their own feedback. A free-form answer routes via
the approval node's `on_other` to `incorporate_feedback.py`, which
folds that text into `research_feedback` and loops back to
`research_each_question` for another parallel pass.

`settings.max_loop_iterations` (40) is the engine's infinite-loop
backstop: it caps the total visits to any single node.

## Running

```sh
loki agents install                  # ships deep-research
loki -a deep-research "How does HTTP/3 differ from HTTP/2?"
loki -a deep-research "Recent advances in solid-state batteries"
loki -a deep-research                # no prompt -> triggers ask_topic
```

## Anti-hallucination

- `research_one_question` (each map branch) is instructed to back
  every claim with a real retrieved source and never to fabricate
  URLs, titles, or DOIs.
- `vet_sources` classifies every cited source so weak sources are
  visible to the critique step.
- `critique` independently reviews the merged findings and sends weak
  or uncited work back for another parallel research pass.
- `synthesize` (the `report-writer` sub-agent) is grounded: it may use
  only the gathered findings and must keep each claim's inline source.
  It has no tools and cannot browse the web.
- `verify_sources` probes every cited URL / DOI with an HTTP HEAD
  request and reports which are unreachable, so the human reviewer
  sees broken citations before approving.

## Customizing

- **Loop budget.** `MAX_REFLEXION_REVISIONS` in `reflexion_gate.py`.
- **Map concurrency.** The `research_each_question` node's
  `max_concurrency: 3` caps simultaneous web-research branches.
  Raise to investigate more questions in parallel; lower to be gentle
  on rate-limited providers.
- **Per-node model.** Add `model: anthropic:...` to any `llm` node.
  Cheap models work well for `plan` / `critique` / `vet_sources`; the
  heavy intelligence is needed in `research_one_question` and the
  `report-writer` sub-agent.
- **Tool scope.** Narrow the `research_one_question` node's `tools:`
  list to constrain where each branch looks (for example, drop
  `web_search_loki` and `mcp:ddg-search` to force arXiv-only
  research).
- **Local knowledge.** Drop files into `knowledge/` to bias every
  research branch toward your local context (see the *Local
  knowledge corpus* section above).
- **Different writer.** Replace `agent: report-writer` on the
  `synthesize` node with the name of any other agent. The
  orchestrator does not care what kind of agent the writer is.
- **Skip approval.** Point both `approve` routes at `end_accepted`,
  or wire `verify_sources` straight to an `end` node.

## Files

```
assets/agents/deep-research/
  graph.yaml                    - agent config + 17-node workflow
  tools.sh                      - classify_source custom tool
  README.md                     - this file
  knowledge/
    README.md                   - corpus-format notes
    research-style-notes.md     - starter knowledge file (replace with your notes)
  scripts/
    parse_request.py            - _next: bootstrap_research, or ask_topic if no topic
    bootstrap_research.py       - fan-out source: next [plan, knowledge_lookup]
    combine_findings.py         - joins map output (question_findings) into findings
    reflexion_gate.py           - _next: research_each_question (revise) or synthesize
    verify_sources.py           - HTTP HEAD on cited URLs / DOIs
    incorporate_feedback.py     - _next: research_each_question, with user feedback
```

See also `assets/agents/report-writer/` — the sub-agent the
`synthesize` node spawns.
