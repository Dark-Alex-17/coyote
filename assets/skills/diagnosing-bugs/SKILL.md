---
description: Feedback-loop-first debugging discipline for hard code bugs and performance regressions. Build a tight, red-capable reproduction loop BEFORE forming any hypothesis, minimise it, then test 3-5 falsifiable hypotheses with tagged instrumentation and lock the fix with a regression test at a correct seam. Load when a fix isn't obvious from the error, a bug survives a first fix attempt, behavior is intermittent, or the user reports something broken/failing/slow. Complements diagnostics (which owns ops/system troubleshooting - services, networking, containers); this owns bugs in code. Grants shell access for running reproduction loops.
enabled_tools: execute_command
---
You are hunting a bug in code. The failure mode this skill prevents is the one every debugger falls into: reading code, forming a theory, and "fixing" the theory instead of the bug. The discipline: **no hypothesis until a feedback loop exists.** Skip phases only when you can say why.

**Redact every secret** in commands, outputs, and captured artifacts you show — write `<REDACTED>`; keep credentials in env vars, not in what you print. Quote only the lines of captured artifacts that carry signal.

## Phase 1: Build the feedback loop (this IS the skill)

Everything else is mechanical. A **tight** pass/fail signal — one that goes red on *this* bug — makes bisection, hypothesis-testing, and instrumentation trivial. Without one, no amount of code-reading will save you. Spend disproportionate effort here.

Ways to construct one, in rough order of preference:

1. **Failing test** at whatever seam reaches the bug: unit, integration, e2e.
2. **curl / HTTP script** against a running dev server.
3. **CLI invocation** with a fixture input, diffing output against a known-good snapshot.
4. **Headless browser script** driving the UI and asserting on DOM/console/network.
5. **Replay a captured trace** — save a real request/payload/event log, replay it through the code path in isolation.
6. **Throwaway harness** — a minimal subset of the system (one service, mocked deps) exercising the bug path with a single call.
7. **Property/fuzz loop** — for "sometimes wrong output", run 1000 random inputs and hunt the failure mode.
8. **Bisection harness** — bug appeared between two known states? Automate "boot at X, check" so `git bisect run` can consume it.
9. **Differential loop** — same input through old vs new version (or two configs), diff the outputs.

Once you have *a* loop, **tighten** it: faster (cache setup, narrow scope), sharper (assert the specific symptom, not "didn't crash"), more deterministic (pin time, seed RNG, isolate filesystem, freeze network). A 2-second deterministic loop is a debugging superpower; a 30-second flaky one is barely better than nothing.

**Non-deterministic bugs**: the goal is a higher reproduction *rate*, not a clean repro. Loop the trigger 100×, parallelise, add stress, inject sleeps to widen timing windows. A 50% flake is debuggable; 1% is not.

**Phase 1 is complete** when you can name ONE command you have already run at least once (show the invocation and output, redacted) that is:

- [ ] **Red-capable** — drives the actual bug path and asserts the user's exact symptom; it can go red on this bug and green once fixed.
- [ ] **Deterministic** — same verdict every run (or a pinned, high reproduction rate).
- [ ] **Fast** — seconds, not minutes.
- [ ] **Agent-runnable** — you can run it unattended.

If you catch yourself reading code to build a theory before this command exists — STOP. That is the exact failure this skill exists to prevent. If you genuinely cannot build a loop: say so explicitly, list what you tried, and ask the user for environment access, a redacted captured artifact (HAR, log dump, recording), or permission to add temporary instrumentation. Do NOT proceed to hypothesise without a loop.

## Phase 2: Reproduce + minimise

Run the loop; watch it go red. Confirm it produces the failure the USER described — not a nearby different failure (wrong bug = wrong fix) — and capture the exact symptom for later verification.

Then **minimise**: shrink to the smallest scenario that still goes red. Cut inputs, callers, config, and steps one at a time, re-running after each cut. Done when every remaining element is load-bearing (removing any one goes green). A minimal repro shrinks the Phase 3 hypothesis space and becomes the Phase 5 regression test.

## Phase 3: Hypothesise

Generate **3-5 ranked hypotheses** before testing ANY of them — single-hypothesis generation anchors on the first plausible idea. Each must be **falsifiable**: "if X is the cause, then changing Y makes the bug disappear / changing Z makes it worse." Can't state the prediction? It's a vibe — discard or sharpen.

Show the ranked list to the user before testing — they often re-rank instantly ("we just deployed a change to #3") — but don't block on them; proceed with your ranking if they're away.

## Phase 4: Instrument

Every probe maps to a specific Phase 3 prediction. **One variable at a time.**

1. Prefer a debugger/REPL if the environment supports it — one breakpoint beats ten logs.
2. Otherwise targeted logs at the boundaries that DISTINGUISH hypotheses. Never "log everything and grep".
3. **Tag every debug log with a unique prefix** (e.g. `[DEBUG-a4f2]`) so cleanup is a single grep. Untagged logs survive into production; tagged logs die.

**Performance regressions**: logs are usually the wrong tool. Establish a baseline measurement first (timing harness, profiler, query plan), then bisect. Measure first, fix second.

## Phase 5: Fix + regression test

Write the regression test BEFORE the fix — but only at a **correct seam**: one where the test exercises the real bug pattern as it occurs at the call site. A test at a too-shallow seam (unit test that can't replicate the triggering chain) gives false confidence.

**If no correct seam exists, that itself is a finding** — the architecture is preventing the bug from being locked down. Document it; don't fake the test.

With a correct seam: turn the minimised repro into a failing test → watch it fail → apply the fix → watch it pass → re-run the Phase 1 loop against the ORIGINAL un-minimised scenario.

## Phase 6: Cleanup (required before declaring done)

- [ ] Original repro no longer reproduces (re-run the Phase 1 loop, show the output)
- [ ] Regression test passes — or the absence of a correct seam is documented
- [ ] All `[DEBUG-...]` instrumentation removed (grep the prefix to prove it)
- [ ] Throwaway harnesses/prototypes deleted
- [ ] The winning hypothesis stated in the commit/report, so the next debugger learns

## Anti-patterns

- Hypothesising from code-reading before a red-capable loop exists.
- "Fixing" until the loop goes green without ever confirming the loop reproduced the USER's symptom.
- Shotgun instrumentation — untargeted logs that distinguish nothing.
- Declaring victory on the minimised repro without re-running the original scenario.
- Deleting or weakening the failing test to get green.
