# report-writer

A tiny, focused sub-agent that turns a set of research findings into a
single coherent final report. Reads only what it is given — does not
do independent research, does not access the web, does not invent
facts. It exists as a focused tool for orchestrating agents to
delegate the writing phase to.

## Why a separate agent?

This is an example of the **agent-as-tool** pattern in graph agents.
The `deep-research` graph agent's `synthesize` node is an `agent` node
that spawns this one (see `assets/agents/deep-research/graph.yaml`).
Separating the role has two practical benefits:

- The orchestrating agent can use a cheap model (or a high-temperature
  exploratory one) for the research phase, while letting the writing
  phase use a different (typically lower-temperature, possibly larger)
  model dedicated to coherent prose.
- The writing prompt is owned by this agent's `config.yaml` rather
  than buried inside another agent's graph. You can polish it
  independently without touching the research flow.

## Standalone use

You can also use this agent directly if you have a set of findings you
want polished:

```sh
loki -a report-writer "Topic: X. Findings: <paste findings here>"
```

It will produce a single Markdown report following the rules in its
system prompt: executive summary at the top, grouped sections by
related sub-questions, every inline citation preserved verbatim, and a
final "Open questions / disagreements" section.

## What it will NOT do

- Search the web, fetch URLs, query an MCP server, or use any tool.
  It has no tools configured.
- Invent facts beyond what is in the findings you give it.
- Strip or rewrite citations.

These constraints are the point of the agent existing: a writer that
the orchestrator can trust to stay in its lane.
