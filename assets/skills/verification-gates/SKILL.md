---
description: Evidence requirements before claiming completion — diagnostics, build exit code, tests. No completion without proof. Grants shell access for running build/test commands.
enabled_tools: execute_command
---
You are about to mark work complete. Before claiming "done," produce evidence. "I'm fairly confident it works" is not evidence.

## Hard gates

A task is NOT complete until:

| Change kind | Required evidence |
|---|---|
| File edit | Read the file to confirm the change landed; output is clean (or only pre-existing issues, explicitly noted) |
| Build command exists | `execute_command` the build; exit code 0 |
| Test command exists | `execute_command` the tests; pass (or explicit note of pre-existing failures unrelated to this change) |
| Delegation | The delegate's result was received AND verified against your acceptance criteria |

**No evidence = not complete.** Marking a todo done without evidence is dishonest reporting.

## The verification loop

After every meaningful edit:

1. Read the changed file region (confirm the change actually landed where intended).
2. If there's a project-level lint/typecheck command, run it on the touched files.
3. Run the project's build/check command if one exists.
4. Run the project's test command if one exists.
5. Only then mark the corresponding todo `completed`.

If any step fails: do not mark complete. Fix the issue or surface it explicitly.

## Build/test detection (fallback)

If no build/test command is configured, try standard ones for the project:

- Rust: `cargo check`, `cargo test`
- Node/TS: `npm run build`, `npm test`, or `pnpm` / `yarn` equivalents
- Python: `pytest`, `python -m mypy <pkg>`, `ruff check`
- Go: `go build ./...`, `go test ./...`

Run from the project root. Capture exit codes.

## Distinguishing your failures from pre-existing failures

If build or tests fail, identify the cause:

- Caused by your change? → fix it before reporting complete.
- Pre-existing (unrelated)? → note it explicitly: "Done. Build passes. Note: 3 lint errors pre-existing in unrelated files, not touched."

Never silently leave broken state behind. Never delete a failing test to make CI green.

## Anti-patterns (BLOCKING)

- "It should work" without running anything
- Marking a todo complete based on intent, not verified outcome
- Suppressing errors with `@ts-ignore`, `as any`, `#[allow(...)]` on unfamiliar lints, empty catch blocks
- Deleting failing tests to "pass"
- Reporting "all green" when you only ran a subset

## Reporting completion

When the work is verifiably done, report in one sentence:

> "Done. Build passes, 47 tests pass. Modified `auth.rs:42-58` to add JWT validation."

Not a paragraph. Not a victory lap. Specific, terse, evidence-backed.
