---
description: Fan-out exploration protocol — fire multiple research agents in parallel, wait for completion notifications, and never duplicate delegated work.
---
You are entering a research phase. Exploration is parallelizable; serial reads leave throughput on the table.

## Fan out, don't read serially

For any non-trivial codebase question, fire 2-5 `explore` agents in parallel, each scoped to a different angle:

- Auth implementation? → one for routes, one for middleware, one for token handling, one for error response shape.
- Bug investigation? → one for the failing path, one for similar working paths, one for recent changes near the area.

Each agent gets a NARROW slice. Narrow scope = fast, focused result. Broad scope = the agent over-reads and returns a wall of text.

## The wait protocol

After spawning background agents:

1. If you have **non-overlapping** work to do (work that doesn't depend on the delegated research), do it now.
2. If you don't, **end your response.** Do not call `agent__collect` immediately — the agent is still running.
3. The system notifies you when the agent completes (`pending_escalations` or completion event).
4. On notification, call `agent__collect` to retrieve results.

Polling `agent__collect` on a still-running agent blocks your turn for nothing.

## Anti-duplication rule (BLOCKING)

Once you delegate a search to an `explore` agent, **do not perform that same search yourself.**

Forbidden:
- After firing `explore` for "auth middleware", running `fs_grep` for "auth middleware" yourself
- "Just quickly checking" the same files the delegate is checking
- Re-doing the research while waiting impatiently

Allowed:
- Non-overlapping work in a different module
- Preparation work that doesn't depend on the delegated result
- Ending your response and waiting

Duplicate searches waste tokens, may contradict the delegate, and defeat the point of parallelism.

## Stop conditions

Stop searching when:

- The same information appears across multiple sources
- Two search iterations yield no new useful data
- A direct answer was found
- You have enough context to proceed confidently

Over-exploration is as bad as under-exploration. Time spent searching is time not spent shipping.

## Parallel + sequential composition

It is fine to fire `explore` and then `oracle` when oracle needs the explore results — just sequence them:

1. Fire explore(s) in parallel.
2. End response, wait for completion.
3. Synthesize findings, fire `oracle` with those findings as CONTEXT.
4. End response, wait for oracle.
5. Act on oracle's recommendation.

Don't fire oracle blind to "save a turn" — it will give worse advice.

## Anti-patterns

- One huge "explore everything about X" agent → slow, unfocused result
- Serial explores ("wait for first, then fire next") → unnecessary latency
- Firing 8+ parallel agents → diminishing returns, harder to synthesize
- Calling `agent__collect` immediately after spawn → wastes a turn
