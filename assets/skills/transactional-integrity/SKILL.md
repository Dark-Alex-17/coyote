---
description: Review state-changing code for transactional integrity - atomicity gaps, read-modify-write races, non-idempotent handlers of at-least-once inputs, dual-writes to a DB plus an external system, side effects that escape rollback or re-fire on retry, and isolation-level assumptions. Store-agnostic (SQL transactions, DynamoDB conditional writes, Redis MULTI, document stores). Load when a diff touches DB writes, transactions, queue/webhook/job handlers, or external side effects. Findings fold into the standard code-review severity taxonomy. Grants read-only filesystem access for tracing transaction boundaries.
enabled_tools: fs_read, fs_grep, fs_glob, fs_cat, fs_ls
---
You are reviewing state-changing code. The generic correctness checklist asks "does this work?"; you ask the three questions that page people at 3am: **"what happens when this runs twice? halfway? concurrently?"** Most production data-corruption incidents are not wrong business logic — they are correct logic executed under a failure mode the author never considered: a retry, a crash between two writes, or a second copy of the process.

## When to load this skill

The diff touches ANY of: database writes, transaction blocks, queue/stream consumers, webhook handlers, scheduled/background jobs, retry logic, or calls to external state-holding systems (payment providers, email, other services). If the diff is pure reads, UI, or stateless computation — unload; this checklist has nothing for you.

## The checklist

### 1. Atomicity: do multi-step writes share a transaction?

Find every place the diff performs two or more writes that must succeed or fail together (insert parent + child, update balance + write ledger entry, state transition + audit row). Then verify they actually share an atomic unit:

- SQL: same transaction — and confirm it by READING the enclosing scope, not by assuming; a helper called from two places may run with and without a wrapping transaction.
- DynamoDB: `TransactWriteItems` or a single-item design, not two `PutItem` calls.
- Redis: `MULTI`/`EXEC` or a Lua script, not sequential commands.
- Document stores: single-document update or multi-document transaction, not two updates.

A crash between unguarded writes is a FINDING: name the two writes, the window, and the resulting inconsistent state.

### 2. Read-modify-write: what happens when two run concurrently?

Every `read → decide → write` sequence is a lost-update race unless something serializes it:

- `SELECT` then `UPDATE` with no `FOR UPDATE`, no optimistic version/etag check, no atomic `UPDATE ... SET x = x + 1`, no conditional write (`ConditionExpression`, compare-and-set).
- Check-then-insert uniqueness ("does username exist?" then insert) with no DB unique constraint backing it — application-level checks NEVER close the race; the constraint is the fix, the check is UX.
- In-memory caches of DB state mutated alongside the DB without invalidation ordering.

Ask: is there exactly one writer, structurally guaranteed (singleton job, partition ownership)? If yes, note the assumption and move on — flagging single-writer code for races is noise. If concurrency is possible, the missing guard is a finding.

### 3. Idempotency: is every at-least-once input handled at-most-once?

Queue consumers, webhook handlers, scheduled jobs, and anything retried WILL run more than once with the same input. For each handler the diff adds or modifies:

- Is there an idempotency key, dedupe table, `INSERT ... ON CONFLICT DO NOTHING`, or conditional state transition (`WHERE status = 'pending'`) that makes the second delivery a no-op?
- Does the handler complete its side effects BEFORE acknowledging/deleting the message? Ack-then-process loses work; process-then-ack requires idempotency.
- Partial failure: if the handler does A, B, C and crashes after B, the redelivery re-runs A and B — are they safe to re-run?

An error path that neither succeeds nor removes the message (so it redelivers forever into a DLQ) is also a finding — poison-message handling is part of idempotency.

### 4. Dual-write: DB + external system with no reconciliation

The diff writes to the local DB AND to an external state holder (payment provider, email service, another service's API, a search index) in one flow. One succeeds, the other fails — now the two systems disagree:

- Look for the outbox pattern (write intent to DB in the transaction, deliver asynchronously), a saga/compensation step, or at minimum an explicit reconciliation job.
- "Call external API inside the DB transaction" is not a fix — it holds locks across network I/O and still diverges when the commit itself fails after the call succeeded.
- Order matters: charging a card before durably recording the intent to charge means a crash produces a charged-but-unprovisioned customer; the reverse produces a recorded-but-unfulfilled intent, which is recoverable.

Flag the divergence window and which side wins on replay.

### 5. Side effects vs rollback and retry

- Anything non-transactional fired INSIDE a transaction (email sent, event published, cache invalidated) happens even when the transaction rolls back. It must move after commit (or into an outbox).
- Anything fired inside a RETRIED scope (job framework with automatic retries, HTTP client with retry middleware) re-fires per attempt unless guarded.
- Metrics/logs are exempt — do not flag observability as a side-effect violation.

### 6. Isolation assumptions

Code that is only correct under SERIALIZABLE but runs at the store's default (READ COMMITTED in Postgres, REPEATABLE READ in MySQL) is a latent race. Watch for: aggregate checks before writes ("sum of debits ≤ balance"), multi-row invariants enforced in application code, and phantom-sensitive queries. If the diff's correctness depends on an isolation level, verify the code SETS it rather than assumes it.

## Ground-truth discipline

- READ the enclosing scope of every write the diff touches — transaction boundaries live up-stack from the hunk. `fs_grep` the function's callers to learn whether it already runs inside a transaction.
- `fs_grep` the handler registration/config for retry counts, DLQ wiring, and delivery semantics before claiming "this retries."
- Check sibling handlers for the idempotency pattern the codebase already uses (dedupe table, conditional transition) — a new handler skipping the established guard is the strongest form of evidence.
- Do not flag theoretical races in structurally single-writer code; note the single-writer assumption instead so the next reviewer sees it was considered.

## Finding format and severity

Fold findings into the standard review severities — no separate verdict:

- 🔴 CRITICAL — money/data loss or corruption under a realistic failure (dual-write with no reconciliation on a paid flow; lost-update race on a balance; non-idempotent charge handler).
- 🟡 WARNING — inconsistency window or unbounded redelivery with operational (not monetary) blast radius; missing constraint behind a check-then-act.
- 🟢 SUGGESTION — hardening: add the missing unique constraint even though the race is improbable, move the email after commit.

Every finding names: the writes involved (file:line), the failure mode that triggers it (crash between X and Y / concurrent execution / redelivery), and the concrete fix. "This might have race conditions" is noise; "second `invoice.paid` delivery re-runs the provisioning insert at handler.go:88 because there is no dedupe on event_id — add the unique index the payment handler at handler.go:41 already uses" is signal.

## Anti-patterns

- Flagging every read-then-write as a race without checking who else writes.
- Demanding SERIALIZABLE everywhere — the finding is an UNSTATED isolation assumption, not a low isolation level.
- Treating logs/metrics as dual-writes.
- Reviewing the hunk without reading the enclosing transaction scope — most false positives and false negatives in this domain come from not knowing whether you're already inside a transaction.
- Accepting "the framework handles it" without grepping the framework config that proves it.
