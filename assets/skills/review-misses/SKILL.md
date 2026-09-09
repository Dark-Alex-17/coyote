---
name: review-misses
description: |
  The review-miss ledger: escaped defects (bugs that survived review and were caught later)
  recorded as reusable detection patterns. Review orchestrators load this skill, read the
  ledger, and route matching entries to reviewers as first-class checks — this is the
  feedback loop that makes the review stack converge instead of plateau.
  Triggers: loaded by review orchestrators (e.g. code-reviewer) at review start; "record a miss", "review miss"
license: MIT
metadata:
  author: Alex Clarke
  version: 1.0.0
  tags: [review, feedback-loop, ledger]
---

# review-misses

A review stack without a feedback loop plateaus: it catches the same classes forever and
misses the same classes forever. This skill closes the loop — every defect that ESCAPED
review (shipped, then caught in prod, by the user, or by a later change) becomes a ledger
entry with a detection pattern, and every future review runs those patterns as
first-class checks.

## The ledger

Location: `$HOME/.config/coyote/share/review-misses.md` (expand `$HOME` at runtime —
never hardcode a user's home path). The `share/` subdirectory is the convention for
durable DATA files that are not configuration — it keeps the config root uncluttered
(the /usr/share analogy). The `record-miss` macro migrates a ledger from the legacy
config-root location automatically; when reading, if `share/review-misses.md` is absent,
also check the legacy `$HOME/.config/coyote/review-misses.md` before concluding "empty".

**Scope: global and permanent.** One ledger per environment, shared across every repo,
review, and reviewer — it is the institutional memory of escaped defects, not per-review
state. Reviews only READ it; nothing about a review is ever written to it. The `repo:`
and `surface:` fields are how one global ledger stays relevant per review (filtering,
not partitioning). Concurrent reviews are fine: reviewers are concurrent readers, and
writes happen only through the `record-miss` macro — a human act, append-only. It
deliberately lives under config rather than a cache directory: caches are disposable by
contract, and losing this file loses the learning loop.

- **Missing or empty ledger**: skip silently — a one-line note in the synthesis
  ("review-miss ledger: empty") and nothing else. Never invent entries.
- Entries are append-only history. Never rewrite or delete entries during a review.

Entry format:

```markdown
## MISS-NNN — <one-line title>
- date: YYYY-MM-DD
- repo: <owner/repo where it escaped>
- surface: <rest-api | worker | db-migration | cli | iac | library | ci-cd | any>
- what escaped: <the defect, concretely>
- why review missed it: <root cause in review terms — wrong context, unchecked caller, mock-assert test, …>
- detection pattern: <the concrete check/grep/question a reviewer runs to catch this CLASS of defect>
```

## Consuming the ledger (review orchestrators)

1. At review start, read the ledger (if present).
2. Filter entries to the ones RELEVANT to this review: matching `surface` (against the
   resolved surfaces), matching `repo`, or `surface: any`. Irrelevant entries stay out of
   worker context — the ledger must not bloat every review.
3. Paste each relevant entry's title + detection pattern into the CONTEXT section of the
   file-reviewers (or lanes) whose files it could apply to.
4. In the final report footer, note `review-miss patterns applied: MISS-004, MISS-011`
   (or `none relevant`).

## Applying a pattern (reviewers)

- A ledger detection pattern is a FIRST-CLASS check — same standing as a loaded skill's
  checklist item. Run it against the diff like any other check.
- When a pattern fires, the finding cites the entry: `recurrence of MISS-007` in the
  finding title. Recurrences of an escaped defect class default to 🔴 — the class has
  already burned us once.
- When a pattern doesn't fire, silence is fine — no per-entry accounting needed.

## Recording a miss

Record via the `record-miss` macro (`coyote --macro record-miss "<what escaped>"`), which
appends a numbered skeleton entry and prompts for the remaining fields. Or append by hand
in the entry format above.

To BOOTSTRAP a ledger from scratch (new team, new org, new environment), the
`mine-review-misses` macro (`coyote --macro mine-review-misses "<org | repo list | .>"`)
pulls real PR review history, mines the defect classes human reviewers keep enforcing,
and appends the candidates you approve — same entry format, same idempotent numbering.

Rules for good entries:

- **The detection pattern is the payload.** "Be more careful with X" is not a pattern;
  "grep for `time.Now()` in files that also touch period/interval math; flag any not going
  through the clock interface" is. If you can't state the check a reviewer runs, keep
  distilling before recording.
- One entry per defect CLASS, not per incident — if a new miss matches an existing
  entry's class, sharpen that entry's detection pattern instead of adding a duplicate.
- Never record secrets, customer data, or incident details beyond what the pattern needs.
