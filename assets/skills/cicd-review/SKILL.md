---
description: Review CI/CD pipeline definitions - action/step pinning by SHA, token and credential permission scoping, secret exposure to fork-PR triggers, and cross-branch cache poisoning. Load when a diff touches workflow/pipeline files such as .github/workflows/*, GitLab CI config, or equivalent pipeline definitions. Findings fold into the standard code-review severity taxonomy. Grants read-only filesystem access for tracing workflows, triggers, and permission blocks.
enabled_tools: fs_read, fs_grep, fs_glob, fs_cat, fs_ls
---
You are reviewing CI/CD pipeline definitions. The generic correctness checklist asks "does this pipeline run?"; you ask **"what can this pipeline be made to do by someone who controls an input to it — a tag, a fork PR, a cache key?"** A workflow file is production code with production credentials that runs third-party code on every push, yet it is otherwise reviewed by nobody. Most pipeline incidents are not broken builds — they are a mutable action tag that started doing something new, a token scoped far beyond its job, or a secret handed to code from a fork.

## When to load this skill

The diff touches ANY of: workflow/pipeline files — `.github/workflows/*`, GitLab CI config, or equivalent pipeline definitions in other systems. If the diff is application code with unchanged pipelines — unload; this checklist has nothing for you.

## Marker semantics

Every checklist item below carries a severity emoji AND a `[convention]` or `[correctness]` marker; both ride in the finding title so downstream tooling can act on them mechanically. `[convention]` findings are rigor-foldable (the orchestrator may lower them under a relaxed quality bar) and rejectable — but ONLY with cited evidence: a repo convention at file:line, or a recorded plan decision. `[correctness]` is reserved for contract breaks; those findings are neither foldable nor rejectable.

## Linters and mechanized checks

The review orchestrator runs the domain's mechanized checker — `actionlint` for GitHub Actions workflows; your CONTEXT may already include its output — do not re-derive it. Spend your prose on what the linter cannot reach: whether a token's permissions match what the job actually does, what a fork-triggered run can see, whether a cache key crosses a trust boundary. If the repo plausibly warrants a linter config it lacks (workflow files but no `actionlint` wiring), emit a 🟢 `[convention]` finding naming the gap.

## The checklist

Severities below are the production bar. Each item is a context-sensitive question, not an absolute — read the trigger blocks and permission blocks before flagging, and state any exemption you rely on.

### 1. 🟡 `[convention]` Actions pinned by mutable tag instead of SHA

Does the diff reference third-party actions or pipeline steps by a mutable tag (`@v4`, `@main`) rather than a full commit SHA? A mutable tag means the code your pipeline runs — with its credentials — can change without any change in your repo; tags have been retargeted maliciously in the wild. The house fix is SHA-pinning with a tag comment (and a bot to update pins). First-party actions from the same repo/org can be exempt per repo convention — cite the convention if you rely on it.

### 2. 🟡 `[convention]` Token/credential permissions broader than the job needs

Does each job's token grant match what the job actually does? Look for missing explicit permission blocks (falling back to a broad default), write scopes on jobs that only read, and org-level credentials in jobs that need repo-level access. The finding names the scoped alternative: the specific permissions the job's steps use. A workflow-level broad grant with per-job narrowing is acceptable shape; per-job broad grants "to be safe" are the finding.

### 3. 🔴 `[convention]` Secrets exposed to fork-PR triggers

Can a pull request from a fork reach this workflow's secrets? The dangerous shapes: triggers that run with secret access on fork-controlled code (e.g. `pull_request_target` checking out the PR head), secrets passed into steps that execute fork-modified scripts, and label-gated runs where the gate is applied after checkout. This item is co-owned with `security-review`'s supply-chain checklist item — that skill owns the full exploitation analysis; you flag the exposure the moment the trigger/secret/checkout combination makes it possible. The severity stays 🔴 regardless of the declared quality bar — fork-reachable secrets are critical at every rigor, and this item should never be folded down. Workflows that run on fork PRs WITHOUT secrets, or with secrets only after a trusted-code boundary, are the correct shapes — verify the checkout ref before accepting them.

### 4. 🟢 `[convention]` Cross-branch cache poisoning

Do cache keys let an untrusted branch write cache entries that a trusted branch (main, release) later restores? Caches written by fork-PR or feature-branch runs and restored by default-branch runs let attacker-influenced artifacts flow into trusted builds. Check the cache key/scope construction and the platform's cache-isolation rules — some platforms already isolate caches by branch with one-way fallback; a pattern the platform provably isolates is exempt, and worth citing.

## Ground-truth discipline

- READ the trigger block and permission block of every workflow the diff touches — the risk is almost always in the trigger/checkout/secret combination, not in the step commands.
- `fs_grep` sibling workflows for the house idioms (SHA-pinning style, permission-block placement, cache-key construction) and cite the sibling at file:line when flagging deviation.
- Check what each referenced action actually is (first-party vs third-party, checkout target) before applying the pinning and fork-exposure items.
- Do not assert platform behavior (default permissions, cache isolation) from memory alone when the repo's config could override it — check the org/repo-level settings files if present, and state assumptions otherwise.

## What this skill does NOT check

- The full exploitation analysis of exposed secrets, injection via untrusted workflow inputs (`${{ }}` interpolation attacks), and supply-chain trust of the pinned actions themselves → `security-review` (item 3 above is explicitly co-owned with its supply-chain item).
- Whether deploy/release steps the pipeline runs are idempotent and safe to re-run → `transactional-integrity`.
- Log output conventions of pipeline steps → `logging-discipline`.
- Metrics/alerts on pipeline health and deploy outcomes → `observability-review`.
