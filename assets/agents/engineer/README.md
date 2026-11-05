# engineer

The conversational staff-engineer companion: codebase Q&A, problem diving, and design
sessions with orchestrator-grade research fan-out - and zero implementation mandate.
Built as a plain config agent, so it is useful standalone and doubles as the
`chat:` agent for graph-based orchestrator sessions.

## What it is

- The frontier-level conversations you can have with an orchestrator - learning a
  codebase, thinking through a problem, designing a feature - extracted into a
  single special-purpose agent with the same research machinery: parallel explore
  fan-outs (cross-repo via `reference_root` when set), librarian / deep-research for
  external material, oracle consults, teammates-pattern collection discipline.
- A judgment surface: no model pin, so it runs the session default model.

## What it is NOT

- An implementer. It never modifies source, never commits/pushes, never spawns
  coder/step-runner/review lanes, and never spawns sisyphus/architect/itself
  (no recursion). When work should be done it recommends the right vehicle and
  drafts the brief.
- Its only writes are design docs / decision ledgers under `plans/` (or a named
  design-doc path) - "decisions must escape the conversation."

## Deliberate divergences from the sisyphus config it mirrors

| Divergence | Why |
|---|---|
| Spawnables: consult set only (explore, librarian, deep-research, oracle) | read-only doctrine + no recursion |
| No `agent_session: temp` | The accumulated conversation IS the value; resume, never respawn |
| `summarization_threshold: 200000` | Long design sessions compress later than the architect (100k) and sisyphus (80k) |
| Write discipline in instructions, not tool grants | A future `conversational: true` certification flag takes over enforcement when it exists; until then the discipline is prose + owner review |

## Usage

```
coyote -a engineer                                                # standalone, cwd repo
coyote -a engineer --agent-variable project_dir ~/code/myrepo \
                   --agent-variable reference_root ~/code         # enable cross-repo reads
```

Graph sessions: `session.chat.agent: engineer` in an orchestrator graph, with
`chat_agent_for` set in that graph's `session.chat.variables`.

Standalone handoffs land in `plans/engineer-handoffs/<slug>.md`, outside the
numbered step handoffs a phased plan keeps in `plans/handoffs/`.

Attached to `architect-next`, the engineer also writes a design-doc seed there
when the user describes work to implement. Each seed and handoff starts with
`status: ready`, `project:`, `created:`, `title:` and `vehicle:` frontmatter.
A run started with no doc path offers the newest ready seed for its project
whose `vehicle` is `architect` (a seed with no `vehicle` key counts as
`architect`), and marks the one it uses `status: consumed` once the run has
oriented on it.

A run that parks on a question for discussion exposes it as a non-empty
`discussion` in its last-run record. `discussion.kind` names the question (a
design round, quality bar, checklist item or Gate 1 question, or any other
escalation the user replied to with a question, such as `git_setup_failed`); the record
carries the question, its options with the recommended one marked, the
user's `reply` that parked it, the answers so far and the plan path. The engineer
discusses it without editing the plan or answering for the user, then tells
the user to say `continue: <settled answer>` (or `continue` to be asked again)
so the run resumes at that question. `resume` works the same as `continue`;
the answer only counts after the colon.
