# Design Retrospective

A **run-closing design retrospective**. Once a large body of work has passed its reviews, it
reads the whole change (every touched file in full, plus the modules it calls into and the
callers that call it) and asks: if this were built again, would the same choices be made to
prioritise maintainability, cleanliness and reuse? Where a design pattern that was not used would
clean the code up, make it reusable, or close off a class of bugs, it writes a refactor item.

Three things it is deliberately **not**:

1. **Not a gate.** The review stack ([`review-gauntlet`](../review-gauntlet/README.md) and its
   lanes) judges a change pass/fail, lane by lane, on diff signals. The retrospective reads the
   finished change once and returns refactor items, never a verdict; nothing waits on it.
2. **Not an implementer.** It never edits files. The caller adopts or declines each item and
   implements the adopted ones itself, under its normal review gates.
3. **Not interactive.** It never asks the user and never stops for a pick.

## When callers spawn it

- [`sisyphus`](../sisyphus/README.md) runs it once after the review passes are green on a change
  it judges to be a large body of work (a new module or subsystem, several modules changed
  together, a new abstraction other code will build on). Under `review_cadence: batch` the
  parent orchestrator runs it instead, over the whole batch.
- [`architect`](../architect/README.md) runs it once at run close (Phase F), over the run diff,
  before the hygiene pass. Adopted items become one refactor task implemented like any other
  task, and the hygiene pass then sweeps that task's debris too.

The caller passes the project, the diff range, what the work set out to do, and optionally the
plans dir (read as bookkeeping, never reviewed as code).

## Output contract

The first line is machine-parseable:

```
DESIGN_RETROSPECTIVE: REFACTORS <n>
DESIGN_RETROSPECTIVE: CLEAN
```

Then `## Refactor items`: numbered items, each with **Title**, **Where** (file:line), **Today**
(the current shape and its friction), **Better shape**, **Why** (maintainability, reuse, or the
bug class it prevents), **Size** (small / medium / large) and **Confidence** (Strong / Worth
doing / Speculative). Then `## Choices I would make again`, which a `CLEAN` report carries too.

## How it works

Driven by the [`codebase-design`](../../skills/codebase-design/SKILL.md) skill (module,
interface, depth, seam, adapter, leverage, locality; the deletion test; design-it-twice). Its
prompt lists example shapes to look for (duplicated logic, shallow modules, leaked seams, flag
soup, hand-synced lists, tests coupled to implementation) and states that they prompt judgement
rather than form a checklist. An item needs evidence that the better shape is concretely better
for this code; taste alone, recorded decisions and untouched code do not earn one. For a large
change it fans out up to three [`explore`](../explore/README.md) agents to walk the neighbours.

## Usage

```sh
coyote -a design-retrospective \
  "Project: /path/to/repo
Diff: main..HEAD
What the work set out to do: add the export pipeline"
```

## Related

- [`architecture-reviewer`](../architecture-reviewer/README.md) - the on-demand, codebase-wide
  counterpart: scouts deepening opportunities anywhere, not just in one change.
- [`codebase-design`](../../skills/codebase-design/SKILL.md) - the vocabulary and principles it
  runs on.
