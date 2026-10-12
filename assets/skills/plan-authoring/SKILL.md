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

## Names a stranger can read

Every new module, type and public function the plan or breakdown names must be meaningful to a
reader who has not read the plan. Coinages and acronyms the plan invented get a descriptive name
BEFORE tasks are created: a name that reaches 30 files is never questioned again, and the
implementer will not stop to rename what the plan told them to type.

- Bad: `mod lrq` (the design doc's shorthand for the transport's request/response pair).
- Good: `mod link_request`.

## Documents, directories and scaffolding a plan adds

Every document, directory, example target, dependency added ahead of use, or smoke test the plan
adds must name its lifetime -- `permanent`, or `replaced by <X> at <milestone>` -- AND the task
that removes it. Record this in the plan as a table under a section named `## Added artefacts`:

| Artefact | Lifetime | Removed by |
|---|---|---|
| `tests/support/smoke_deps.rs` | replaced by the transport client at step 4 | step 4 |
| `docs/link-request.md` | permanent | -- |

Test-only binaries live under `tests/` support (or are declared as test targets in the build
manifest with a comment saying so), never under `examples/` as if they were examples.
Scaffolding tasks ("add dependencies ahead of use", "smoke the deps") must name the later task
that supersedes them, and that task's acceptance criteria include deleting the scaffolding.

## Writing for a context-free executor

- Paste code snippets from your exploration into Context. "Follow the pattern in foo.rs" forces the executor to re-do exploration you already did.
- Use repo-relative paths from the project root. Never "the file we discussed."
- Name symbols exactly: `RetryPolicy::backoff`, not "the backoff logic."
- If a decision was made in discussion (X over Y), record the decision AND the one-line reason in the PLAN FILE (or the task's log once execution starts) -- never in CONTRIBUTING, README or code comments. The executor will face the same fork and must not re-litigate it.
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
- A plan coinage carried into code -- the module is named for a shorthand nobody outside the plan recognises
- An added document, directory or example without a lifetime -- it outlives the thing it scaffolded
- A scaffolding task with no superseding task named -- "add deps ahead of use" with nothing scheduled to delete it
- Decisions recorded in CONTRIBUTING or README -- they belong in the plan file or the task log
