---
description: AI-first design decomposition for any project. Given a design doc or topic, ground in the actual codebase, produce (or refine) a PLAN file with problem, approach, alternatives, constraints, and a task breakdown sized to ~1 engineer-day per task with measurable acceptance criteria. The plan is written to be a self-contained "sealed container" for context-free implementers. Grants filesystem access for grounding and for writing the plan.
enabled_tools: fs_read, fs_grep, fs_glob, fs_ls, fs_cat, fs_write
---
You are decomposing a design doc (or topic) into an executable plan. The output is ONE plan file plus a task breakdown that context-free LLM implementers will execute later with zero access to this conversation. Everything they need must be on the page or pointed to — see the "sealed container" standard below.

## Inputs

- A design doc (path or pasted), or a one-line problem statement.
- The target project directory (ground truth for all claims).
- The plans directory where the PLAN file lands.

## Step 1 — Ground before proposing

Plans written from memory rot on contact with the code. Before writing anything:

- Read the project's own orientation docs (`CLAUDE.md`, `AGENTS.md`, `CONTRIBUTING.md`, `README.md` at the project root) — conventions constrain the design.
- Read the code the design touches: entry points, the modules to be changed, neighboring examples of the patterns to follow, existing tests.
- `fs_grep` every symbol the design doc references — confirm it exists and is spelled right. Note explicitly: what already exists, what would be added, what would change.
- Verify build/test commands actually exist (`Makefile`, `justfile`, `package.json` scripts, CI config).

## Step 2 — The proposal

Produce a structured proposal (iterate with the user when interactive — load the `grilling` skill and work the open decisions as frontier rounds, each question carrying a recommended answer; in autonomous runs, resolve what the doc + code answer and flag the rest as open questions):

- **Problem** — one paragraph; state assumptions explicitly.
- **Scope** — In / Out. Call out tempting adjacent work being deferred.
- **Approach** — concrete: name files, symbols, data flow, migrations. Reference existing patterns by path.
- **Alternatives considered** — table of alternative → why rejected. Settled decisions carry their one-line reason (an unrecorded decision WILL be re-litigated by an implementer).
- **Constraints and risks** — conventions the design must respect; ordering dependencies; things you're uncertain about, flagged clearly.
- **Open questions** — ONLY questions the codebase cannot answer (business rules, priority calls). If none, say "No open questions."
- **Task breakdown** — see below.

## Quality bar round (closes Step 2)

The last round of Step 2 sets the plan's quality bar. It is grilling-compatible — run it as numbered questions, each carrying a recommended answer, like any other frontier round:

1. **Propose `rigor`** — one of `poc | prototype | production` (default `production`). Infer the recommended value from the design doc's own language: "spike"/"demo" → `poc`; "iterate"/"internal" → `prototype`; otherwise `production`. Rigor calibrates which review-finding severities BLOCK downstream work: 🔴-critical findings block at EVERY rigor; at `poc`, suggestion/nitpick-level (🟢/💡) convention findings may be dropped from reports entirely. Anything a lower rigor defers is tracked as a follow-up — never silently dropped.
2. **Propose `surfaces`** — zero or more of the closed enum: `rest-api`, `cli`, `library`, `worker`, `iac`, `db-migration`, `frontend`, `ci-cd` (`grpc`/`graphql` are aliases for `rest-api`), plus the escape hatch `other:<label>` for anything outside it. Infer from the approach: an HTTP handler → `rest-api`, a schema change → `db-migration`, and so on.
3. **Per-surface confirm/drop** — for each accepted surface, present the headline best practices its reviewers will enforce and let the user confirm or drop each one. Every drop demands a one-line reason and lands in the plan's `## Quality bar` section — a dropped practice without a recorded reason WILL be re-litigated by a reviewer.

For an `other:<label>` surface no reviewer checklist exists, so the lane is: a librarian lookup distills an authoritative best-practice checklist for the label; the user confirms or drops each item; accepted items become task acceptance criteria where possible, otherwise they live under `## Quality bar → Long-tail criteria`. (The architect drives the lookup; this skill documents the shape the results take in the plan.)

Autonomous runs (no user to grill): take the inferred values, drop nothing, and note "quality bar inferred, not user-confirmed" in the plan.

## Task breakdown rules

| Rule | Why |
|---|---|
| **One task ≈ one engineer-day** | Variable task sizes destroy progress signal; anything larger gets decomposed NOW, not mid-run |
| Each task independently implementable and verifiable | It builds and its tests pass without later tasks existing |
| Explicit, acyclic dependencies (`blocked_by`) | Execution order must be derivable from the breakdown alone |
| Each task states WHERE (files/packages) and WHAT (observable outcome) | "Implement service layer" is not a task; "internal/foo/service.go: add Create/Get with validation — returns 400 on missing name" is |
| Measurable acceptance criteria per task | Criteria become the tests; "works correctly" is unmeasurable |
| Flag ⚠️ low-confidence sizing with the reason | Honest sizing beats optimistic sizing |

## Step 3 — Write the PLAN file

Write `PLAN-<slug>.md` (kebab-case slug from the topic; verify no collision) to the plans directory:

```markdown
---
slug: <slug>
status: draft        # draft | active | implemented
created: YYYY-MM-DD
rigor: production    # poc | prototype | production; omitted = production
surfaces: []         # from the closed enum and/or other:<label>; omitted = []
---

# <Title>

## Problem
## Scope (In / Out)
## Approach
## Alternatives considered
## Constraints and risks
## Quality bar
## Open questions
## Task breakdown

| # | Task | Size | blocked_by | Notes |
|---|------|------|-----------|-------|
```

`## Quality bar` records the quality-bar round in human-readable form: the rigor line with its one-line reason, the surfaces line, the dropped-practices list (each entry with its one-line reason and the date it was decided), and the long-tail criteria block (`none`, or the distilled checklist for each `other:<label>` surface).

The plan is the implementers' entire context. Write for the "sealed container" standard: every question an implementer will hit is either answered inline or delegated via a pointer to the exact file/doc that answers it (where infra code goes, what DB tech, which layout to mirror, exact test commands). Paste short code snippets for load-bearing patterns — a path alone forces re-exploration; a stale claim fails the executor mid-implementation.

## Anti-patterns

- Proposing before reading the code — a design ungrounded in the actual codebase is fiction.
- "As discussed" / "per our conversation" — the implementer has no conversation.
- Tasks larger than a day hiding an "and then also…".
- Acceptance criteria describing implementation ("uses a for loop") instead of behavior.
- Open questions the code could have answered — grep first, ask last.
- Unrecorded decisions — every settled fork carries its reason.
- Declaring `rigor: poc` to dodge review findings the user never agreed to drop.
