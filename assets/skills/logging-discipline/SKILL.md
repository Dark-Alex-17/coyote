---
description: Calibrate log output to the repository's existing logging conventions before writing code, and review diffs for under- and over-logging. Detects the repo's logging register from sibling files - logger/framework, message style (capitalization, length, tense), payload vs ID-only context, level semantics, error-path convention - and matches it; falls back to stated best-judgment defaults when no convention exists. Load when writing code that touches boundaries, error paths, jobs, or state transitions, or when reviewing such a diff. Complements security-review (which owns secrets/PII in logs) and incident-prior-art (which treats deleted log lines as operational leads).
---
You are writing or reviewing code that logs — or that should. LLMs fail in both directions: narrating every step (noise operators must grep past) and swallowing error paths silently (invisible failures at 3am). "Correct" is repo-relative: detect the register, match it; where no register exists, apply the best-judgment defaults below.

## Step 0: Check for a declared policy first

Check the workspace instructions already in your context (`COYOTE.md`/`AGENTS.md`, a logging section in `CONTRIBUTING.md`) for a stated logging convention. A declaration beats detection — obey it and skip Step 1.

## Step 1: Detect the register (during reads you already do)

Pattern-matching discipline already has you reading 2-3 sibling files before writing. While reading, note how THEY log:

1. **Logger and shape** — which logging library/facade, and is output structured (key-value fields) or printf-style interpolated strings? Never introduce a second logging mechanism alongside an established one.
2. **Message style** — capitalization (lowercase `"failed to connect"` vs sentence-case `"Failed to connect"`), punctuation (trailing periods or not), length (terse fragments vs full sentences), tense/mood ("connecting" / "connected" / "connect failed"). Match all of it — mixed message styles make logs harder to grep.
3. **Context convention** — what rides along with the message: full payloads, or IDs only? Which fields are customary (request/correlation ID, entity IDs, durations)? Attached as structured fields or interpolated into the string? If the repo logs IDs-only, do NOT log payloads — that's both a style break and a data-exposure risk.
4. **Level semantics in practice** — what does this repo actually use `error`/`warn`/`info`/`debug` for? Match observed usage over textbook definitions.
5. **Error-path convention** — do errors get logged where they occur and then propagated, or propagated silently and logged once at the top? Match it; this determines where YOUR log lines go.

Sample from the same language and layer you're editing — a chatty CLI layer and a quiet library core can coexist in one repo; the nearest siblings win.

## Step 2: When to log (and when not)

Warranted — a reader on-call should be able to see:

- **Boundaries**: calls to external systems (network, DB, queues) — at minimum their failures, with enough context to identify the failing operation.
- **Error paths**: every error is either logged or propagated to something that logs it — never silently swallowed, and **never both** (see invariants).
- **Lifecycle**: job/worker/process start, finish, and abnormal exit; consumed/produced messages where the repo's register does so.
- **State transitions an operator would care about** (order of magnitude: status changes, retries exhausted, fallbacks engaged).

Unwarranted:

- **Narration** — logging what the next line of code plainly does ("entering function", "about to save"). The comment-discipline rule, applied to logs.
- **Hot paths** — per-item logging inside loops or per-request debug logging in high-volume paths; aggregate or sample instead.
- **Log-and-rethrow** — logging an error AND re-raising it to a caller that logs again produces duplicate stacks that make incidents harder to read, not easier.
- **Payloads the register doesn't log** — and never full payloads containing credentials or personal data regardless of register (security-review owns that judgment; don't create the finding).

## Best-judgment defaults (weak or no signal)

A greenfield file, a repo with no discernible convention, or contradictory siblings — use these and note the choice:

- Structured logging if the ecosystem's standard library or dominant framework supports it; otherwise the language's idiomatic default.
- Terse, lowercase, no trailing period, present-tense messages ("failed to fetch invoice"), stable wording (log messages are grepped and alerted on — treat them as identifiers, not prose).
- IDs and small scalar fields, never payloads.
- `error` = someone may need to act, `warn` = degraded but coping, `info` = lifecycle, `debug` = development detail.
- When genuinely unsure whether a line earns its keep: boundaries and error paths yes, everything else no.

## Review-side checks (for diffs)

- **Underdone**: a new external call, error path, or background job with zero failure visibility — no log, no metric, no propagation to a logging caller. Cite the path and what an operator would be blind to.
- **Overdone**: narration logs, log-and-rethrow duplication, hot-loop logging, payload logging in an IDs-only repo. Cite the line and the register evidence.
- **Register mismatch**: new log lines that break the detected message style or use a different logger/mechanism than the siblings.
- **Deleted or reworded log lines**: operators and alerts grep for exact strings; flag deletions/rewordings of lines that look triage-relevant so the change is conscious, not accidental (incident-prior-art treats these as leads — same instinct at review time).
- Severity calibration: silent new failure paths are 🟡 findings; style/register mismatches are 🟢/💡.

## Invariants (register-independent)

1. **No error silently swallowed.** An empty catch/ignored error with no log, no metric, and no propagation is a finding in every repo.
2. **No double-logging of one error** along a single propagation path — one log per failure, at the level the repo's convention chooses.
3. **No secrets or personal data in logs**, ever, regardless of how payload-happy the register is.
4. **Never delete existing log lines as drive-by "cleanup"** — that's out-of-scope churn AND an operational hazard; if a line must go, say so explicitly in the change description.

## Anti-patterns

- Importing your favorite logging style into a repo that has one.
- Logging every function entry/exit because "more visibility is better" — noise is the enemy of visibility.
- Flagging a quiet pure-computation module for "missing logs" — the trigger surface is boundaries, error paths, jobs, and state transitions; inert code needs none.
- Rewording existing log messages to be "cleaner" — you just broke someone's saved Loki/CloudWatch query.
- Treating textbook level definitions as authoritative over the repo's observed usage.
