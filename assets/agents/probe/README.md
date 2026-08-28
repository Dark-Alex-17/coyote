# Probe

A **black-box usage-pattern verifier**. Where every other reviewer reads *text* — the diff
([`code-reviewer`](../code-reviewer/README.md)), the plan ([`adversary`](../adversary/README.md)),
the attack surface ([`security-reviewer`](../security-reviewer/README.md)) — `probe` asks the one
question none of them can answer without running the thing:

> **"Does the changed consumer-facing surface actually behave as the spec promises when used,
> starting from nothing?"**

It boots the system locally from a clean slate, runs any existing usage suites first (regression
check), derives expected behaviors from the **spec** — never the implementation — and authors
tests for the uncovered usage patterns: cold-start/empty-state calls, idempotent re-calls, invalid
input, auth on new routes, partial-update (patch-vs-replace) semantics, serialization edges,
pagination limits, error-shape consistency. These are exactly the defects invisible to static
review.

## Why it's separate from the other reviewers

| | `code-reviewer` | `adversary` | `security-reviewer` | `probe` |
|---|---|---|---|---|
| Question | Is the code good? | Does it match the plan? | Can it be abused? | Does it *work* when used? |
| Method | Reads the diff | Diff vs. criteria | Source→sink tracing | **Runs the system**, black-box |
| Blind spot it covers | slop, bugs, coupling | skipped criteria, drift | injection, authz gaps | behavioral quirks, regressions, contract surprises |
| Output | severity findings | `CONFORMS`/`DIVERGES` | `PASS`/`FAIL` | `PASS`/`FAIL`/`INCONCLUSIVE` |

The independence is behavioral: expectations are written from the spec/contract **before** reading
handler code, so the implementer's misreadings can't become the probe's assertions — the same
principle that makes `adversary` valuable, applied to runtime behavior.

## Verdict (blocking, three-way)

```
USAGE_PROBE: PASS
Surface: <...>. Existing suites: <N run, all green | none found>. New tests: <M authored at <path>, all green>.
```

```
USAGE_PROBE: FAIL
Behavioral findings:
1. <surface + case> — <spec'd behavior> — <observed behavior> — REPRO: <exact request + response> — <test file>
```

```
USAGE_PROBE: INCONCLUSIVE
Could not establish a clean local environment: <verbatim error>. Missing: <the recipe/fixture that would unblock>.
```

- **`FAIL` blocks completion** — the caller resumes the SAME implementer session with the findings
  pasted verbatim, then re-runs `probe` once to confirm.
- **`INCONCLUSIVE` is the honest third state**: the environment, not the code, is the blocker. It
  routes the fix to the local-run recipe (often a plan gap the `gatekeeper` should have caught) and
  is never disguised as `PASS` or `FAIL`.

Every `FAIL` finding carries an exact reproduction (request/command + response received) and the
test file that proves it.

## How it probes

Driven by the [`usage-pattern-testing`](../../skills/usage-pattern-testing/SKILL.md) skill:

1. **Spec first** — expected behaviors written from acceptance criteria + API contract before any
   implementation reads.
2. **Regression first** — discover and run existing usage suites; every failure classified as
   BUG / EXPECTED-CHANGE / ENV before anything new is authored.
3. **Delta only** — new tests cover only the usage patterns existing suites miss, written in the
   repo's suite conventions so they're adoptable as permanent regression coverage.
4. **Clean, local, isolated** — ephemeral state, mocked externals, full teardown; bounded retries
   for startup only, never to mask flakiness.

Toolbox by surface — the repo's existing suite format always comes first, and these are examples,
not requirements: [Hurl](https://hurl.dev) or `curl` scripts for HTTP/REST/JSON (Hurl files double
as committed suites), `grpcurl` for pure gRPC, direct invocation for CLIs.

Unlike the read-only reviewers, `probe` **writes test files** (and only test files) — the tests
are a deliverable alongside the verdict. It never modifies implementation code.

## Usage

Spawned by `sisyphus` (post-coder, when the change touches consumer-facing surface) or `architect`
(Phase E, alongside `adversary`). The spawn prompt IS its entire context — include the change, the
spec, and the local-run recipe:

```sh
agent__spawn --agent probe --prompt "
## TASK
Probe the changed API surface for TASK-NNN from the consumer's perspective. Return PASS/FAIL/INCONCLUSIVE.

## CHANGE
Run get_diff --base <ref>, or: <paste the changed-surface summary>

## SPEC — expected behavior to verify against
<paste acceptance criteria + API contract sections (or contract file paths) VERBATIM>

## LOCAL-RUN RECIPE
<how to boot the stack clean: build, deps/stubs, ports, migrations, teardown — or the doc that has it>

## EXISTING SUITES
<paths + run commands, or 'discover them'>
"
```

Direct invocation for ad-hoc use:

```sh
coyote -a probe --agent-variable project_dir /path/to/repo \
  "Probe the /widgets endpoints changed in the last commit against this spec: <paste spec>"
```

### Tools

- `get_diff [--base <ref>]` — staged → unstaged → `HEAD~1` fallback (or an explicit base SHA/branch) to locate the changed surface.
- `get_changed_files [--base <ref>]` — quick changed-file map.
- Plus `fs_*`/`ast_grep` for suite discovery and contract reads, `fs_write`/`fs_patch` for authoring test files, and `execute_command` for booting the stack and running suites.
- Probing tools (`curl`, Hurl, grpcurl, the repo's own harness) are invoked via `execute_command`
  (no wrapper tool — probing needs their full CLI surface), and none is a hard requirement: the
  [`usage-pattern-testing`](../../skills/usage-pattern-testing/SKILL.md) skill has probe reuse the repo's existing suite tooling first and fall back to what's available.
  The optional [`sbx-mixin.yaml`](sbx-mixin.yaml) preinstalls Hurl + grpcurl for sandbox runs.

## Related

- [`usage-pattern-testing`](../../skills/usage-pattern-testing/SKILL.md) — the methodology it runs on.
- [`adversary`](../adversary/README.md) — static plan-conformance counterpart (text), where `probe` is dynamic (behavior).
- [`gatekeeper`](../gatekeeper/README.md) — ensures plans ship the local-run recipe `probe` consumes.
