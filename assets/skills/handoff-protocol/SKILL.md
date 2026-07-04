---
description: Schema and discipline for writing and reading step handoff documents - the only channel between implementation steps. Evidence must be pasted, downstream plan changes proposed not imposed. Grants filesystem access for reading and writing handoffs.
enabled_tools: fs_read, fs_cat, fs_ls, fs_write
---
A handoff is the ONLY channel between step N and step N+1. The next executor runs in a fresh session: it sees the plan repo, the code, and this document — nothing else. Whatever you learned that isn't in the handoff (or in `plans/NOTES.md`) is lost. Write accordingly.

Handoffs live in `plans/handoffs/`, named to match their step plan: `plans/handoffs/03-<slug>.md` for `plans/steps/03-<slug>.md`.

## Required schema (writer)

Frontmatter:

```yaml
---
step: 3
title: Add retry policy to the fetch client
result: complete   # complete | partial | blocked
---
```

Sections, all mandatory (write "None" rather than omitting — an absent section is indistinguishable from a forgotten one):

| Section | Contents |
|---|---|
| Summary | 2-4 sentences: what exists now that didn't before |
| Completed | Task-by-task, mirroring the plan's Tasks section |
| Not completed | Deferred or dropped tasks, each WITH a reason |
| Deviations | Every departure from the plan: what the plan said, what you did, why |
| Downstream plan updates | Edge-case annotations made directly (which plan, which section) and proposed diffs awaiting approval (see below) |
| Edge cases discovered | Found during implementation — including ones you handled, so the next step knows they're covered |
| Evidence | Pasted verbatim: format/lint/build/test commands, exit codes, salient output lines. Note pre-existing failures explicitly |
| Notes for next step | Warnings, gotchas, invariants the next executor must not violate |

## Evidence rules

Assertions are not evidence. "Tests pass" is a claim; this is evidence:

```
$ cargo test
   ...
test result: ok. 47 passed; 0 failed; exit code 0
```

- Paste the command, the exit code, and the decisive output lines (not the full log).
- Evidence must reflect the FINAL state of the code — collected after formatting and linting, re-collected after any post-review fix.
- If a check was skipped (no formatter configured, etc.), say so explicitly.

## Downstream plan updates: annotate vs propose

Two classes, with different authority:

- **Annotations (make directly).** Adding an entry to a later plan's Edge cases section. Additive, non-scope-changing. Record each in Downstream plan updates.
- **Proposals (never apply directly).** Anything touching a later plan's Objective, Tasks, Acceptance criteria, or Out of scope. Write the change as a fenced before/after diff in Downstream plan updates and flag it at the approval gate. The user applies or rejects it.

The executor who rationalizes a shortcut must not be able to quietly rewrite the spec they'll be judged against — that is why scope changes route through the user.

## Rolling notes vs handoff

- **Handoff**: step-scoped. What happened in THIS step.
- **`plans/NOTES.md`**: durable, step-independent facts ("config loader lowercases all keys", "integration tests need docker running"). Append; never rewrite others' entries. Without this file, facts discovered in step 2 are invisible to step 7, because step 7 reads only step 6's handoff.

## Reading a handoff (start of a step)

1. Check `result`. `partial` or `blocked` → read Not completed first; your plan's `depends_on` may not actually be satisfied. Escalate rather than build on missing ground.
2. Trust what has pasted evidence. Re-verify bare assertions before depending on them.
3. Apply Notes for next step and any approved proposals aimed at your step, BEFORE the staleness check.
4. Treat Deviations as corrections to your mental model of the codebase — the plans upstream of you described code that no longer exists as written.
5. Read `plans/NOTES.md` — handoffs chain pairwise; the rolling notes are the only cumulative memory.

## Anti-patterns

- "All tests pass" with nothing pasted — a claim, not a handoff
- Omitting a section instead of writing "None" — forgotten or empty, the reader can't tell
- Editing a later plan's Tasks or scope directly instead of proposing a diff
- Burying a major deviation in prose instead of the Deviations section
- Durable facts in the handoff only — lost after one more step
- Evidence collected before the formatter ran — the pasted output describes bytes that no longer exist
- Writing the handoff before the completion gate (todos done or deferred-with-reason) is satisfied
