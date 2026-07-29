---
description: File-based task tracking for plan-driven runs on any project. Defines the TASK-NNN directory schema (index.md + append-only log.md), the frontmatter lifecycle (pending/in-progress/blocked/complete), numbering, the completion protocol, and follow-up task creation. The tasks directory on disk is the durable run state - it survives context compression. Grants filesystem access for managing task files.
enabled_tools: fs_read, fs_grep, fs_glob, fs_ls, fs_cat, fs_write, fs_patch, fs_mkdir
---
You are tracking implementation tasks as files. The task directory is the durable source of truth for run state — anything that lives only in chat history is lost to context compression. Keep it current at every state change, not in batches.

## Layout

```
<plans_dir>/
  PLAN-<slug>.md                  # the plan (see design-session / plan-authoring)
  tasks/
    TASK-001-<slug>/
      index.md                    # current state: frontmatter + What/Steps/Acceptance criteria
      log.md                      # append-only audit trail
    TASK-002-<slug>/
      ...
```

## index.md schema

```markdown
---
title: <short imperative title>
status: pending        # pending | in-progress | blocked | complete
type: feature          # feature | chore | followup
points: 1.0            # engineer-days; ~1.0 per the sizing rule
plan: PLAN-<slug>.md
blocked_by: []         # TASK ids that must be complete first
created: YYYY-MM-DD
---

## What

One paragraph: what this task produces, named concretely (files, symbols, behaviors).

## Steps

- [ ] Concrete step — name the file, function, or migration
- [ ] ...

## Acceptance criteria

- [ ] Observable behavior, measurable ("returns 429 after 3 failed attempts")
- [ ] ...
```

Status lives in frontmatter — there are no lifecycle directories. `status: complete` plus all boxes checked IS done.

## log.md conventions

Append-only. Each entry is an H2: `## YYYY-MM-DD — <short label>` (`created`, `started`, `implemented`, `diverged`, `completed`, ...). Body is 1-3 sentences of prose; structured data lives in markdown links (branch URLs, commit SHAs, PR links). Never rewrite an old entry — add a new one.

## Numbering

Scan `tasks/TASK-*` for the highest NNN and increment (zero-padded to 3). This assumes a single writer per plans directory; if multiple agents or people share one, serialize task creation.

## Lifecycle protocol

| Transition | Do |
|---|---|
| Create | `fs_mkdir` the dir; write `index.md` (status: pending) + `log.md` with a `created` entry |
| Claim | frontmatter `status: in-progress`; log `started` (note the branch + base SHA) |
| Blocked | `status: blocked`; log why and what unblocks it |
| Complete | Check off every Step and Acceptance criterion (verified, not aspirational); log `completed` with commit SHAs AND any follow-ups reported by the implementer, VERBATIM; set `status: complete` |

Never mark a criterion checked without evidence. Never batch state changes — update at the moment of transition.

## Follow-up tasks

When implementation surfaces manual/out-of-scope actions (secrets to create, cloud roles to provision, console steps, cross-repo changes): create a task per item (group small related ones) with `type: followup`, `status: pending`, the WHAT/WHERE/WHY/WHEN in its What section, and a note of which TASK surfaced it. Follow-ups are deliverables to hand to the user — never implement them in the current run.

## Consistency checks (run at the end of a run)

- Every task dir has both `index.md` and `log.md`.
- Every `blocked_by` reference resolves to an existing task.
- No task is `complete` with unchecked Steps/Acceptance criteria.
- Every `complete` task's log has a `completed` entry with commit references.
- The PLAN's breakdown table rows all map to task dirs (and vice versa).

## Anti-patterns

- Run state that exists only in chat — session ids, decisions, and follow-ups belong in task files.
- `status: complete` with unchecked boxes, or checked boxes without evidence.
- Rewriting log history instead of appending.
- Hand-picking a task number without scanning (collisions).
- Follow-ups mentioned in a summary but never materialized as task files.
