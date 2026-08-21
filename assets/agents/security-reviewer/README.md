# Security Reviewer

A **security analyst** for code changes. Where [`code-reviewer`](../code-reviewer/README.md) asks
*"is this code good?"* and [`adversary`](../adversary/README.md) asks *"is this the code the plan
asked for?"*, `security-reviewer` asks the third orthogonal question:

> **"Can this code be abused?"**

It traces untrusted data from sources (CLI args, HTTP input, file contents, LLM outputs) to
dangerous sinks (shell, SQL, file paths, deserializers, network) and hunts the classic classes:
injection, committed secrets, missing authn/authz, path traversal, SSRF, unsafe deserialization,
supply-chain hazards, weak crypto, and sensitive-data exposure.

## Why it's a third reviewer

| | `code-reviewer` | `adversary` | `security-reviewer` |
|---|---|---|---|
| Question | Is the code correct/clean? | Does the code match the plan? | Can the code be abused? |
| Unit of analysis | per-file diffs (fan-out) | criteria ↔ diff mapping | **data flows across files** |
| Blind spot it covers | slop, bugs, coupling | skipped criteria, scope drift | source→sink paths, secrets, authz gaps |
| Output | severity-tagged findings | `CONFORMS` / `DIVERGES` | `PASS` / `FAIL` (posture-gated) |

Security flaws live in the path between an input in one file and a sink in another —
exactly what a per-file review fans out past, and what acceptance criteria almost never mention.

## Posture-gated blocking

Not every project needs production strictness — a POC shouldn't be blocked on missing rate
limiting. The `security_posture` variable (or an explicit posture in the spawn prompt) sets the
blocking threshold:

| Posture | Blocks (FAIL) | Intended for |
|---|---|---|
| `prototype` | 🔴 Critical only | POCs, spikes, demos, localhost-only tools |
| `standard` (default) | 🔴 Critical + 🟠 High | Anything deployed, shared, or built upon |
| `hardened` | 🔴 + 🟠 + 🟡 Medium | Auth, payments, secrets handling, public-facing, multi-tenant |

Two invariants that do not bend with posture:

1. **Critical always blocks.** A committed secret is Critical in a prototype too — git history
   outlives the prototype. Same for code that endangers the host machine or third-party systems.
2. **Posture gates the verdict, not the report.** Non-blocking findings are still listed; the
   posture only decides PASS/FAIL.

Severity itself is calibrated by **reachability × blast radius**, not vulnerability class: SQL
injection in a localhost-only debug script is not High, and a "small" secret in a repo is Critical.

## Verdict (blocking on FAIL)

The agent ends every review with one sentinel:

```
SECURITY_REVIEW: PASS
Posture: standard. Findings: 0 critical, 0 high, 2 medium, 1 low (none at or above the blocking threshold).
```

```
SECURITY_REVIEW: FAIL
Posture: standard. Findings: 0 critical, 1 high, 1 medium, 0 low.
Blocking findings:
1. 🟠 Path traversal — export.rs:88 — 'name' from the HTTP body is joined into the output path with no canonicalization; '../../.ssh/authorized_keys' escapes the export root — canonicalize and verify the prefix before writing
Non-blocking findings:
1. 🟡 Sensitive data in logs — auth.rs:41 — bearer token logged at debug level — redact before logging
```

A `FAIL` verdict **blocks** completion, exactly like adversary's `DIVERGES`. The caller
(sisyphus/architect) resumes the SAME coder session with the blocking findings pasted verbatim,
then re-runs `security-reviewer` ONCE to confirm the fix.

Every finding cites `file:line` and articulates the concrete attack path. Vague findings are not
emitted.

## How it reviews

Driven by the [`security-review`](../../skills/security-review/SKILL.md) skill:

1. **Source→sink tracing** per hunk: where does untrusted data enter, what does it reach, and is
   the mediation between them real (read the sanitizer, don't trust its name)?
2. **Ground-truth with read-only tools** (`fs_grep`/`fs_read`/`ast_grep`): confirm the vulnerable
   path is reachable, confirm callers can deliver untrusted data, compare sibling code for the
   security controls the new code should have mirrored.
3. **Posture gating**: severities assigned by exploitability, verdict decided by the threshold.

It is **read-only** — it produces a verdict, never a fix.

## Usage

Typically spawned by `sisyphus` alongside `code-reviewer`/`adversary`. The spawn prompt IS its
entire context, so include the diff (or a base ref), the posture, and any deployment context:

```sh
agent__spawn --agent security-reviewer --prompt "
## TASK
Security-review the recent changes. Return PASS/FAIL.

## POSTURE
standard  # or: prototype (this is a throwaway POC) / hardened (this touches auth)

## DIFF
Run get_diff (or --base main), or: <paste diff>

## DEPLOYMENT CONTEXT
<what this code is for, who can reach it, whether it will be deployed/shared>
"
```

Direct invocation for ad-hoc use:

```sh
coyote -a security-reviewer --agent-variable security_posture prototype \
  --agent-variable project_dir /path/to/repo \
  "Review the staged changes. This is a localhost-only spike."
```

### Tools

- `get_diff [--base <ref>]` — staged → unstaged → `HEAD~1` fallback (or an explicit base/PR branch).
- `get_changed_files [--base <ref>]` — quick map of the attack surface.
- Plus read-only `fs_*` and `ast_grep` for ground-truth checks.

## Related

- [`security-review`](../../skills/security-review/SKILL.md) — the methodology it runs on.
- [`code-reviewer`](../code-reviewer/README.md) — the quality reviewer it runs alongside.
- [`adversary`](../adversary/README.md) — the plan-conformance reviewer it runs alongside.
