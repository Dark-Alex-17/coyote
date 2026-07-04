---
description: End-to-end protocol for executing one step of a phased implementation plan - orient, staleness check, checklist, implement, edge-case sweep, verify, review, handoff, approval. Grants shell access for build/test commands.
enabled_tools: execute_command
---
You are executing ONE step of a phased implementation plan. Previous steps were executed in sessions you cannot see; later steps depend on what you do and document. The protocol below is ordered — do not skip phases, do not reorder them.

Companion skills: load `handoff-protocol` before Phase 1 (you must READ a handoff correctly) and keep it loaded for Phase 8 (you must WRITE one). Load `verification-gates` for Phase 6. The plan schema is defined in `plan-authoring`.

## Phase 1 - Orient

1. Read the previous step's handoff (`plans/handoffs/`, highest step number below yours). If none exists, you are step 1.
2. Read the current step plan (`plans/steps/`). Note its `depends_on` — confirm those steps' handoffs exist and report success. If a dependency failed or is missing, STOP and escalate via `user__ask`.
3. Read `plans/NOTES.md` for durable facts discovered by earlier steps.
4. Apply anything the previous handoff directed at your step (approved plan updates, warnings).
5. Set the plan's frontmatter `status: in-progress`.

## Phase 2 - Staleness check (BEFORE any edit)

The plan was written before steps 1..N-1 changed the codebase. Verify its assumptions still hold:

- Grep the symbols the plan references — do they still exist, with the claimed signatures?
- Read the plan's Context snippets at their claimed locations — has the code drifted?
- Confirm the Test commands still work.

Discrepancies are deviations — handle them via Phase 5's protocol BEFORE implementing. Executing a stale plan literally is the primary failure mode of phased work.

## Phase 3 - Checklist

`todo__init` with the step objective, then one `todo__add` per task in the plan's Tasks section, in order. Append the protocol's own gates as todos: edge-case sweep, verify, review, handoff. Mark items done with `todo__done` as you go — never batch. The checklist is what survives context compression; keep it truthful.

When you spawn an agent whose session you may need to resume, embed its session_id in the corresponding todo item text (`"Implement task 3 (coder ses_abc123)"`). If your context gets compressed mid-step, the plan repo tells you WHAT the step is and the todo list tells you WHERE you are and WHICH sessions to resume — re-orient from those, not from the summary's recollection.

## Phase 4 - Implement

- Implement ONLY what the plan's Tasks and Objective ask. Out of scope means out of scope.
- Follow the patterns pasted in the plan's Context. When plan and current codebase disagree, the codebase wins — record the deviation.
- Write tests from the plan's Acceptance criteria, not from your implementation. Criteria-first tests catch what tautological tests cannot.
- While in the code, note (do not fix) anything the planning exploration missed — feed it to Phase 5.

## Phase 5 - Edge-case sweep and deviations

**Edge cases.** For each edge case you discovered: if it belongs to THIS step, handle it (or punt explicitly in the handoff with a reason). If it belongs to a LATER step, check that step's plan — if the plan already covers it, done; if not, add it to that plan's Edge cases section and record the addition in your handoff.

**Deviations.** Classify each:

| Class | Definition | Action |
|---|---|---|
| Minor | Same objective and scope, mechanics differ (renamed symbol, moved file, extra helper) | Resolve it, document in handoff |
| Major | Changes scope, approach, interfaces, or invalidates a later step's assumptions | Do NOT silently proceed. Either escalate via `user__ask`, or write a proposed downstream-plan diff into the handoff per `handoff-protocol` |

Never rewrite a later step's Objective, Tasks, or Out of scope directly — edge-case annotations are the only direct downstream edit you may make.

## Phase 6 - Verify (order matters)

1. Formatter (if configured) — format BEFORE collecting evidence, so evidence reflects final code.
2. Linter (if configured) — fix findings your change introduced.
3. Build/typecheck — exit code 0.
4. FULL test suite — not just your new tests; regressions in untouched code are your problem if your change caused them.

Capture commands and exit codes verbatim — they go in the handoff as evidence. Pre-existing failures: note explicitly, don't fix, don't hide. Apply the 3-strike rule: after 3 failed fix attempts, stop, revert to working state, escalate.

## Phase 7 - Review

Self-review the diff with `code-review` + `ai-slop-remover` loaded. For broad steps (5+ files or crossing architectural boundaries), request an independent pass (`code-reviewer` agent) instead. Fix blockers; re-run Phase 6 after any fix.

## Phase 8 - Handoff

Gate: every todo is either done or explicitly deferred with a reason. No silent drops.

Write the handoff per `handoff-protocol` — schema, pasted evidence, deviations, downstream updates, notes for the next step. Append durable, step-independent facts to `plans/NOTES.md`. Set the plan's frontmatter `status: complete`.

## Phase 9 - User approval

Present: what was done, deviations, downstream plan changes (made or proposed), evidence summary, handoff location. Then STOP — do not begin the next step. If the user requests changes, address them, re-run Phase 6, update the handoff, and present again.

## Anti-patterns

- Editing code before the staleness check — the primary source of mid-step surprises
- Implementing "while I'm here" improvements outside the plan's scope
- Tests derived from the implementation instead of the acceptance criteria
- Collecting build/test evidence BEFORE formatting/linting, then shipping different bytes
- Running only your new tests and claiming "tests pass"
- Silently absorbing a major deviation instead of escalating or proposing a plan diff
- Rewriting downstream plan scope directly instead of proposing per `handoff-protocol`
- Starting the next step without user approval
