# whetstone

> The review suite reviews code. Whetstone reviews the review.

A continuous-improvement agent for the review stack: it takes the human review feedback a
PR actually received, compares it against what the automated suite flagged for the same
change, and turns every generalizable miss into a permanent upgrade — a new rule in the
generic review skills when the class is portable, a review-miss ledger entry when it's
org-specific. Nothing lands without the user approving each proposal.

## Why it exists

An empirical gap study of automated-review misses vs human review feedback showed the suite's misses
cluster into learnable classes — and that a miss, once *recorded*, converts to a catch on
the next review. The study was a one-time manual harvest; whetstone is the same loop run
continuously, at the moment the signal is strongest: when human feedback on a PR is being
addressed, the spawner (sisyphus/architect) knows BOTH what the humans said and what the
suite's own lanes flagged — ground truth that offline comment mining could only re-infer from formats.

## How it fits

- **Spawned by** sisyphus (after a run that addressed human review feedback) or architect
  (same trigger at run end), or invoked directly against a PR. Runs AFTER review, never
  during — it is not a reviewer lane, never posts to the PR, and never judges the code.
- **Classifies** each retained finding as CATCH / PARTIAL / MISS against the suite's
  findings. Lane hygiene excludes only what would be circular: author self-replies and
  the suite's OWN posted output. A teammate's review agent or another team's bot is an
  independent reviewer — its findings count as miss signal when the author actually
  addressed them (ignored agent comments are unvalidated noise and are dropped).
- **Generalization gate** for each miss, in order: taste → dropped; one-off → appended to
  the candidates inbox (`share/review-miss-candidates.jsonl`) for recurrence tracking;
  org-specific class → drafted as a ledger entry; general class → drafted as a suite edit
  in house style. General rules must pass the noise firewall: concrete check, crisp
  trigger, zero org nouns, not already covered (sharpen over duplicate).
- **Propose → approve → apply**: every proposal shows the quoted evidence, the class
  statement, and the full draft; the user checkboxes what to apply. Ledger entries append
  idempotently. Suite edits go where the suite lives: with a `suite_repo` configured (a
  source-of-truth bundle repo), edits land in its working tree — never
  committed (that stays a human act) — and one further approval mirrors them into the
  live runtime config, drift-guarded (runtime files diverging from the pre-edit repo
  version are reported and skipped; runtime-only changes are never made). Without a
  `suite_repo` — most users, whose live config IS the suite — edits apply directly under
  `runtime_config`, with per-file backups to `share/whetstone-backups/` as the revert
  path. Empty proposal lists are a fine outcome; whetstone never invents findings to
  justify its spawn.

## Relationship to the other ledger tools

| Tool | Sees | When |
|------|------|------|
| `whetstone` | Human feedback on PRs YOUR runs addressed, with suite ground truth | Real-time, per run |
| Review-round reconciliation | Human-caught, panel-missed defects on any PR the panel re-reviews → candidates inbox (`share/review-miss-candidates.jsonl`) | Automatic, every round |
| `record-miss` | Defects that escaped review entirely (prod, incidents) | One-off, when it bites |

Collection is automatic end to end: whetstone captures misses the moment feedback is
addressed, round reconciliation captures them on any PR the panel re-reviews (teammates'
PRs included), and `record-miss` covers the misses no one ever commented on. Nothing
auto-appends to the LEDGER itself — candidates wait in the inbox until a human promotes them.

## Rules that keep it safe

Additive only (never weakens or removes an existing rule); writes only to the suite
skills/review-agent configs (repo when configured, live config otherwise — always
backed up first), the ledger, the candidates inbox, and — with its own approval,
drift-guarded — the runtime mirror of this run's applied repo edits; never commits,
never pushes, never touches source repos, never makes runtime-only changes when a
suite repo is configured; one proposal per defect class; verbatim citations only.
