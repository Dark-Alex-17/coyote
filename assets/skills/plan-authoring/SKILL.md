---
description: Author executable high-level plans and per-step implementation plans for phased work. Defines the plan repo layout and step-plan schema. Grants filesystem access for grounding plans in real code.
enabled_tools: fs_read, fs_grep, fs_glob, fs_ls, fs_cat, fs_write
---
You are writing implementation plans that a DIFFERENT agent will execute later, in a fresh session, with zero access to this conversation. The plan IS the executor's entire context. A plan that needs the conversation to make sense is a broken plan.

## Plan repo layout

Default layout (match the existing layout instead if the repo already has one):

```
plans/
  plan.md            # high-level plan; links each step plan
  steps/01-<slug>.md # one file per step, numbered in execution order
  handoffs/          # written by executors; see `handoff-protocol`
  NOTES.md           # rolling durable facts discovered during execution
```

In `plan.md`, link each step plan with an inclusion link (the link alone on its own line). This makes the plan repo an IWE hierarchy — agents navigating a large plan corpus can load `iwe-knowledge-base` and traverse it structurally instead of globbing.

## High-level plan requirements

- Ordered list of steps. Each step is independently implementable and independently verifiable — it compiles and its tests pass WITHOUT any later step existing.
- The dependency graph is explicit and acyclic. If step 4 needs step 2's API, step 4's plan says so.
- Steps are sized for one focused session: roughly 1-5 files of meaningful change. A step that needs "and then also..." is two steps.
- State what the plan does NOT cover. Scope creep starts where scope boundaries are implicit.

## Step plan schema

Every step plan starts with frontmatter:

```yaml
---
step: 3
title: Add retry policy to the fetch client
depends_on: [1, 2]
status: pending   # pending | in-progress | complete
---
```

And contains these sections, all mandatory:

| Section | Contents |
|---|---|
| Objective | 1-3 sentences: what exists after this step that didn't before |
| Context | File paths AND pasted code snippets (5-20 lines) showing the patterns to follow. Not just paths — actual code |
| Tasks | Ordered, atomic tasks. Each maps to one todo item for the executor |
| Acceptance criteria | Measurable behaviors. These become the tests |
| Test commands | Exact commands to run, from the repo root |
| Edge cases | Known edge cases this step must handle or explicitly punt on |
| Out of scope | What the executor must NOT touch, even if tempting |

## Writing for a context-free executor

- Paste code snippets from your exploration into Context. "Follow the pattern in foo.rs" forces the executor to re-do exploration you already did.
- Use repo-relative paths from the project root. Never "the file we discussed."
- Name symbols exactly: `RetryPolicy::backoff`, not "the backoff logic."
- If a decision was made in discussion (X over Y), record the decision AND the one-line reason. The executor will face the same fork and must not re-litigate it.
- Write acceptance criteria as observable behavior ("returns 429 after 3 failed attempts"), not implementation ("uses a for loop"). Criteria that describe implementation produce tautological tests.

## Grounding (before the plan is done)

Plans rot when written from memory. Before finalizing each step plan:

1. `fs_grep` every symbol the plan references — confirm it exists and is spelled right.
2. `fs_read` the files listed in Context — confirm the pasted snippets are current.
3. Confirm the test commands actually exist (check `justfile`, `Makefile`, `package.json` scripts, CI config).

A plan referencing a function that doesn't exist fails the executor at the worst possible time: mid-implementation.

## Edge cases are a first-class section

For every step, enumerate the edge cases you can foresee: empty inputs, concurrent access, error paths, partial failures, migration/compat concerns. If an edge case belongs to a LATER step, write it in that step's plan now — not in a comment, not in your head. Executors are instructed to propagate newly discovered edge cases downstream; make their diff small by having the section exist.

## Anti-patterns

- "As discussed above" / "per our conversation" — the executor has no conversation
- File paths without pasted snippets in Context — forces re-exploration
- Acceptance criteria like "works correctly" — unmeasurable, untestable
- A step that depends on a later step — cycle; re-order or merge
- Omitting Out of scope — the executor will helpfully refactor things you didn't ask for
- Frontmatter without `depends_on` or `status` — breaks status queries and dependency checks
