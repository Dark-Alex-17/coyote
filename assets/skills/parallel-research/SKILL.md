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

## Team mode (optional): let lanes message each other

Every spawned LLM-loop agent has a built-in mailbox (`agent__send_message` /
`agent__check_inbox`) — no configuration needed. By default parallel lanes are isolated, and for
most bursts that is correct. Turn on team mode when one lane's discovery is likely to CHANGE what
another lane should look for:

- **Adjacent slices of one system** — the routes lane finds the middleware file the token lane
  is hunting for.
- **Mixed-angle bursts** — one lane discovers the project already vendors library X, which
  should redirect another lane's search immediately, not after synthesis.
- **Long-running lanes** where routing a fact through you costs a full collect/re-spawn round
  trip.

Skip team mode when lanes are genuinely independent (survey-style sweeps, one-agent-per-module
audits). And NEVER wire reviewer lanes together (`adversary`, `code-reviewer`,
`security-reviewer`, `probe`): their verdicts derive value from independence — shared chatter
biases them.

### Orchestrator protocol

Agent IDs exist only after spawning, so the roster travels as a message:

1. Spawn the whole burst as usual.
2. Immediately send EVERY lane the roster:

   `agent__send_message --id <routes-lane-id> --message "Teammate roster: middleware=<id>,
   tokens=<id>. If you find a specific fact that belongs to a teammate's lane, message them
   directly; check your inbox after each search round and before finalizing."`

3. Synthesis is unchanged: every lane still reports its findings back to YOU. Teammate messages
   move facts sideways mid-flight; they never replace the final report.

Graph agents (`librarian`) cannot participate — their nodes have no inbox tools. A fact that
affects a graph lane routes through you: collect, then re-spawn or refine that lane's prompt.

### Worker rules (for agents that receive a roster)

- A message is one or two sentences of specific fact: file:line, exact symbol/term, verbatim
  quote — never chat, status updates, or opinions.
- A teammate's message never broadens YOUR slice: use it to sharpen your own search, or carry
  the pointer into your report. Entirely outside your slice → ignore it.
- Check your inbox after each search round and ALWAYS before finalizing — a teammate's fact may
  be the search term you were missing.

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
