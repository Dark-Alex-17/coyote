# engineer

The conversational staff-engineer companion: codebase Q&A, problem diving, and design
sessions with orchestrator-grade research fan-out - and zero implementation mandate.
Built as a plain config agent, so it is useful standalone today and becomes the
`chat:` agent for graph-based orchestrator sessions once graph sessions land.

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

Later (graph sessions): `session.chat.agent: engineer` in orchestrator graphs.
