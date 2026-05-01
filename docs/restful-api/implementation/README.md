# Implementation Notes

This directory holds per-step implementation notes for the Loki REST API
refactor. Each note captures what was actually built during one step, how
it differed from the plan, any decisions made mid-implementation, and
what the next step needs to know to pick up cleanly.

## Why this exists

The refactor is spread across multiple phases and many steps. The
implementation plans in `docs/PHASE-*-IMPLEMENTATION-PLAN.md` describe
what _should_ happen; these notes describe what _did_ happen. Reading
the plan plus the notes for the most recent completed step is enough
context to start the next step without re-deriving anything from the
conversation history or re-exploring the codebase.

## Naming convention

One file per completed step:

```
PHASE-<phase>-STEP-<step>-NOTES.md
```

Examples:

- `PHASE-1-STEP-1-NOTES.md`
- `PHASE-1-STEP-2-NOTES.md`
- `PHASE-2-STEP-3-NOTES.md`

## Contents of each note

Every note has the same sections so they're easy to scan:

1. **Status** — done / in progress / blocked
2. **Plan reference** — which phase plan + which step section this
   implements
3. **Summary** — one or two sentences on what shipped
4. **What was changed** — file-by-file changelist with links
5. **Key decisions** — non-obvious choices made during implementation,
   with the reasoning
6. **Deviations from plan** — where the plan said X but reality forced
   Y, with explanation
7. **Verification** — what was tested, what passed
8. **Handoff to next step** — what the next step needs to know, any
   preconditions, any gotchas

## Lifetime

This directory is transitional. When Phase 1 Step 10 lands and the
`GlobalConfig` type alias is removed, the Phase 1 notes become purely
historical. When all six phases ship, this whole directory can be
archived into `docs/archive/implementation-notes/` or deleted outright —
the plans and final code are what matters long-term, not the
step-by-step reconstruction.
