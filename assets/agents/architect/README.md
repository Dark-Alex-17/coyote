# Architect

A **design-doc orchestrator for any project**. Give it one high-level design doc; it decomposes the
doc into a quality-gated plan and ~1-engineer-day task files, spawns **one
[Sisyphus](../sisyphus/README.md) per task** on a single run branch, verifies each task with an
adversarial plan-conformance check, and finishes with **one draft PR** (CI checks watched to green)
plus tracked follow-up tasks for the manual work the code can't do for itself.

Architect does **not** write feature code itself. It owns the *process*; Sisyphus owns each *task*.

## The pipeline it drives

```mermaid
flowchart TD
    user([Design doc]) --> architect["Architect<br/>design-doc orchestrator"]
    architect --> orient["Phase A — Orient<br/>project conventions · build/test commands · design doc"]
    orient --> design["Phase B — design-session<br/>plans_dir/PLAN-&lt;slug&gt;.md + 1-day task breakdown"]

    design -. "grounding" .-> explore[["explore<br/>codebase grep<br/>× parallel"]]
    design -. "unfamiliar libraries" .-> librarian[["librarian<br/>docs + OSS grep"]]
    explore -. "findings ground<br/>the breakdown" .-> design
    librarian -. "findings ground<br/>the breakdown" .-> design

    design --> gatekeeper[["gatekeeper<br/>self-containedness audit<br/>(docker-container test)"]]
    gatekeeper --> g1{"PLAN_GATE?"}
    g1 -->|"LEAKY (≤ 2 cycles)"| amend["Answer the missing questions<br/>via explore / librarian / docs<br/>(user__ask only for business rules)<br/>→ amend the plan"]
    amend --> gatekeeper
    g1 -->|"LEAKY after 2 cycles"| escalate
    g1 -->|"SEALED"| oracle[["oracle<br/>plan-review<br/>(executability)"]]

    oracle --> g2{"PLAN_REVIEW?"}
    g2 -->|"REJECT — fix complaints,<br/>re-submit SAME session"| oracle
    g2 -->|"OKAY"| tasks["Phase D — materialize tasks<br/>plans_dir/tasks/TASK-NNN-*/ (task-tracking)"]

    tasks --> branch["Phase E — run branch<br/>feat/PLAN-&lt;slug&gt; off base_branch"]
    branch --> claim["Claim task (sequential, dependency order)<br/>status: in-progress + base SHA"]
    claim --> sisyphus[["sisyphus<br/>implement ONE task on the run branch<br/>commit + push — NO PR"]]
    sisyphus --> adversary[["adversary<br/>conformance check<br/>diff vs task base SHA"]]

    adversary --> verdict{"ADVERSARIAL_REVIEW?"}
    verdict -->|"DIVERGES — resume<br/>SAME sisyphus session (once)"| sisyphus
    verdict -->|"still DIVERGES"| escalate
    verdict -->|"CONFORMS"| taskdone["Close task<br/>status: complete · log commits + follow-ups"]

    taskdone --> more{"More tasks?"}
    more -->|"yes"| claim
    more -->|"no"| finish["Phase F: full build + tests<br/>on the integrated run branch<br/>+ run-closing hygiene pass (one chore task,<br/>sweeps the whole run diff for debris)"]

    finish --> pr["ONE DRAFT PR: run branch -> base_branch<br/>(never marked ready; user reviews first)<br/>body: task checklist + Hygiene pass + Design decisions + Follow-up / manual actions"]
    pr --> checks{"PR runs/checks<br/>green?"}
    checks -->|"failure — resume responsible<br/>sisyphus session, fix, push"| checks
    checks -->|"external flake /<br/>broken base branch"| escalate
    checks -->|"green"| followups["Create follow-up task files<br/>(type: followup, pending)<br/>→ picked up by the user post-merge"]
    followups --> backfill["Backfill PR link into PLAN + task logs<br/>PLAN status: implemented"]
    backfill --> validate["task-tracking consistency checks"]
    validate --> done([Run complete])

    escalate([user__ask — escalate to user])

    branch -. "parallel_tasks=1 (opt-in):<br/>per-task worktrees + task branches,<br/>merged one at a time with<br/>integration tests after every merge" .-> claim
```

## Where state lives

Everything is file-based in **`plans_dir`** (default `plans/`, resolved against the project):

```
<plans_dir>/
  PLAN-<slug>.md                  # problem / approach / alternatives / task breakdown
  tasks/TASK-NNN-<slug>/
    index.md                      # What / Steps / Acceptance criteria; status in frontmatter
    log.md                        # append-only audit trail (branch, commits, follow-ups, PR)
```

- `plans_dir` **inside the repo** (default) → planning files ride the run branch and land in the PR
  (self-documenting review).
- `plans_dir` **absolute, outside the repo** (e.g. a common runs directory) → nothing planning-related
  is ever committed.

Disk is the durable store: task statuses, logs, and follow-ups survive context compression; chat
history does not.

## The three review gates

| Gate | Agent | Question | When |
|------|-------|----------|------|
| Self-containedness | [`gatekeeper`](../gatekeeper/README.md) | "Can a context-free LLM implement from this plan alone?" | Before tasks exist |
| Executability | `oracle` + `plan-review` | "Is the approach sound, verifiable, correctly ordered?" | After sealing |
| Conformance | [`adversary`](../adversary/README.md) | "Is the built code what the plan asked for?" | After each task |

## Key conventions it enforces

- **One task = one engineer-day** — anything larger gets decomposed at the design stage.
- **Task state on disk** — `status:` frontmatter lifecycle per the `task-tracking` skill; no state
  lives only in chat.
- **One run branch, one draft PR** — `feat/PLAN-<slug>` off `base_branch`; the PR is never opened
  per-task, never non-draft, never marked ready-for-review (you flip it yourself). Larger plans can
  opt into one draft PR per plan-declared PR group instead — see "PR strategy" below.
- **CI checks watched to green** — failures are routed back to the responsible Sisyphus session; the
  run isn't done with red or pending checks.
- **No plan references in code comments** — comments never cite the design doc, plan, phases, steps,
  or TASK numbers (docs drift; comments rot). Plan references live in commit messages only. The one
  carve-out: a task id on the same line as a debt marker (`#[allow(...)]`, `#[expect(...)]` (outer or inner `#![...]` form),
  any `cfg_attr(...)` carrying an allow or expect, `todo!()`, `unimplemented!()`, `#[ignore]`, an env-gated or opt-in
  test), naming the task that removes it. The debris lint spares a task id only beside the markers
  it knows (`#[allow(`, `#![allow(`, `#[expect(`, `#![expect(`, `cfg_attr(`, `todo!`, `unimplemented!`,
  `#[ignore`); for a gate it cannot recognise (an env check, a `#[cfg(...)]`) the owner comment goes
  on the gate's code line (the `if` of the env check, the `fn` line under the `#[cfg(...)]`).
- **`.env` never lands in a repo** — only `.env.example` with placeholder keys; real values become a
  follow-up.
- **Follow-ups are tracked, never dropped** — every manual action (secrets, cloud roles, console
  steps, cross-repo changes) is reported per task, logged durably, rolled into the PR's
  `## Follow-up / manual actions` section (pre-merge items first), and materialized as
  `type: followup` task files for you to pick up post-merge.
- **Run-closing hygiene pass** -- before the PR opens, one `type: chore` task sweeps the whole run
  diff for implementation debris (the debt markers named in the code-review skill, lapsed
  scaffolding, opt-in-only tests, decisions written into CONTRIBUTING/README, non-ASCII punctuation
  or planning vocabulary in comments) and removes or justifies each hit; a kept lint hit carries the
  lint's `debris-exempt` token on its line so the run-wide lint goes green, and kept items land in
  the PR's `## Hygiene pass` section.

## Usage

```sh
# From the target project root (default autonomy: full)
coyote -a architect --agent-variable design_doc docs/design/my-feature.md \
  "Implement this design doc end to end"

# Approve the task breakdown once, then run autonomously
coyote -a architect \
  --agent-variable design_doc docs/design/my-feature.md \
  --agent-variable autonomy plan-gate \
  "Decompose and implement"

# Different project / plans outside the repo / PR against a non-main base
coyote -a architect \
  --agent-variable project_dir ~/code/my-service \
  --agent-variable plans_dir ~/architect-runs/my-service \
  --agent-variable base_branch develop \
  --agent-variable design_doc ~/docs/big-refactor.md \
  "Run the pipeline"
```

### Variables

| Variable | Default | Meaning |
|----------|---------|---------|
| `project_dir` | `.` | The target repo — the only WRITE target for feature code. |
| `plans_dir` | `plans` | Where PLAN + task files live. Relative → in-repo (rides the PR); absolute → outside git. |
| `design_doc` | *(empty)* | Path to the design doc; asked for if unset. |
| `base_branch` | `main` | Branch the run branch forks from and the PR targets. |
| `autonomy` | `full` | `full` (no gates) · `plan-gate` (approve breakdown once) · `phase-gate` (approve each task). |
| `escalation_policy` | `interactive` | `interactive` (pause and ask) · `autonomous` (self-resolve decisions covered by a standing ruling in `~/.config/coyote/share/standing-rulings.md` or a clearly recommended option; everything self-resolved lands in a “Decisions taken autonomously” digest in the Phase F report and PR body; secrets/money/destructive actions and novel conflicts still escalate). |
| `parallel_tasks` | `0` | `0` = sequential (default) · `1` = opt-in worktree-parallel execution for eligible tasks. |
| `pr_strategy` | `single` | `single` = one draft PR for the whole design doc (default) · `multi` = the plan partitions tasks into PR groups, each landing as its own stacked draft PR at group close-out. |
| `task_granularity` | `standard` | `standard` = ~1 engineer-day tasks (default; the render is byte-identical to before the variable existed) · `micro` = one-seam tasks (<=3 files, compile-atomicity floor) with exact signatures and EXECUTABLE acceptance criteria, grouped into plan-section batches (`batch:` / `protected:` task frontmatter). Cadence: per task, Sisyphus runs coder verification only (spawned with `review_cadence: batch`) + the architect's direct adversary criteria check; per batch (last task, or early at a `protected` task) the full review gauntlet over the batch delta, with a resume-fix-renarrow failure loop bounded at 3 strikes per batch. Gate 1 / Gate 2 invoke the `plan-gatekeeping` / `plan-authoring` **Micro-grain addendum** sections by name (inert otherwise). |
| `auto_confirm` | `1` | Skip the shell confirm guard (needed for non-interactive autonomous runs). |
| `build_lock` | *(empty)* | Command prefix (e.g. `flock -w 240 /tmp/coyote-cargo.lock`) forwarded to every Sisyphus spawn and from there to the coder/step-runner gates and the review-gauntlet/adversary verification runs, so parallel worktrees sharing one target directory serialise their builds. Empty runs them bare. The value is substituted raw into a JSON string on every hop, so it must be free of double quotes and backslashes; quote paths with single quotes (`flock '/tmp/my lock'`). |

### Dynamic instructions

`config.yaml` sets `dynamic_instructions: true`: at session start the `_instructions` tool
(tools.sh) renders `instructions.md`, resolving `<!-- when:<var>[=value] --> ... <!-- end -->`
blocks against the agent variables (`autonomy`, `escalation_policy`, `parallel_tasks`,
`pr_strategy`, `task_granularity`) via the shared `render_instructions` helper in
`agents/.shared/utils.sh`. Only the
sections matching the active values reach the model — a sequential run never carries the
parallel-mode worktree machinery, an interactive run never carries the autonomous-escalation
ladder, a single-PR run never carries the PR-group stacking mechanics — which trims context and
removes inapplicable-mode hallucination bait. The static `instructions:` field in `config.yaml` is
a placeholder; edit `instructions.md`.

### Bookkeeping tools (tools.sh)

The mechanical, ground-truth-on-disk Phase F steps are tool calls; judgment stays with the
model (the Problem/Approach summary, acceptance verdicts):

| Tool | Replaces |
|---|---|
| `collect_ledger_lines --tag ... --plan ... --dir ... [--tasks TASK-a,TASK-b]` | re-reading every `log.md` for `rejected-finding:` / `design-decision` / `debris-exempt` / `Follow-ups:` / `(deferred by quality bar)` rollups - grep-complete instead of attention-complete; `--plan` (required) keeps the read to the named plan's `status: complete` tasks, since the tasks dir is shared across plans (a task whose `index.md` has no `plan:` or no `status:` field is kept and named in a `NOTE`), and `--tasks` narrows it to a PR group's members; `plan_tasks.py` holds that one selection rule for both tools |
| `assemble_pr_body --plan ... --tasks-dir ... --summary-file ... [--tasks TASK-a,TASK-b] [--followups] [--quality-note ...]` | hand-assembling the draft PR body (Quality bar from plan `rigor:`, task checklist with `completed`-entry SHAs, ledger sections; `--tasks` scopes the checklist and the ledger rollups to a PR group's members); `assemble_pr_body.py` does the derivation and prints `NOTE` lines for anything it could not derive |

Both fail open to the manual path they replace: a non-zero return or a `NOTE` leaves the
architect exactly where the prose would have had it, and says what to fix.

## Autonomy

Fully autonomous end-to-end by default — it halts only for genuine blockers: scope-changing
ambiguity or unresolved design questions, a task that fails after Sisyphus's own recovery (consults
Oracle, then escalates), and any destructive/irreversible action. Use `plan-gate` or `phase-gate`
to insert approval checkpoints.

## Parallel task execution (opt-in)

By default (`parallel_tasks: 0`) tasks run **sequentially** on the single run branch. Setting
`parallel_tasks: 1` enables worktree-based parallelism:

- Eligible tasks (mutually unblocked, plan-declared file-disjoint, max 3 concurrent) each get an
  isolated `git worktree` + task branch forked from the run branch tip.
- Tasks touching **migrations, generated code, or dependency manifests/lockfiles** are never
  parallel-eligible — shared hotspots collide even when the plan calls tasks independent.
- Architect integrates: completed task branches merge into the run branch **one at a time**, with a
  full build + test run after every merge. Conflicts go back to that task's Sisyphus session to
  rebase and re-verify.
- The batch runs as a **messaging team**: every task's Sisyphus gets a teammate roster (task ids,
  agent ids, declared file scopes); an agent that must edit outside its scope STOPS and messages
  the architect + the owning teammate BEFORE editing (architect re-scopes or serializes — two
  in-flight tasks never knowingly edit the same file); each landed merge is broadcast to the
  remaining agents. The architect stays the sole writer to the run branch.
- Optionally, the `agent__task_*` queue may mirror the on-disk `blocked_by` graph so completing a
  task auto-dispatches its unblocked dependents. The queue is in-memory convenience only — disk task
  files remain the durable source of truth.
- Worktrees and task branches are cleaned up after each clean merge. Phase F (single draft PR +
  CI-check watch) is unchanged in both modes.

## PR strategy (opt-in multi-PR)

By default (`pr_strategy: single`) the whole run lands as one draft PR opened in Phase F. Setting
`pr_strategy: multi` makes the plan partition the task breakdown into **PR groups** (recorded as a
`## PR groups` section in the PLAN and audited by both Phase C gates), each landing as its own
draft PR:

- Each group gets its own branch (`feat/PLAN-<slug>-g<N>`), **stacked**: group 1 forks from
  `base_branch`; group N+1 forks from group N's branch tip (or from `base_branch` once group N's
  PR has merged).
- When a group's last task completes, the architect runs the Phase F close-out scoped to the group
  — final green build, a hygiene pass over the group's own diff, a draft PR targeting the branch
  the group forked from, CI checks watched to green — before forking the next group.
- Group PR bodies carry the same sections as the single-PR body (task checklist, Hygiene pass,
  Design decisions, Follow-up / manual actions), scoped to the group, plus the stack position.
- The architect never merges a group PR; when you merge a parent, it retargets the child PR to
  `base_branch` and rebases. The final report lists every group PR URL in stack order.

## Sub-agents it spawns

| Agent | Used for |
|-------|----------|
| [`sisyphus`](../sisyphus/README.md) | Implement ONE task's code (its own explore→coder→verify→review loop). One per task. |
| [`gatekeeper`](../gatekeeper/README.md) | Plan self-containedness gate (`PLAN_GATE: SEALED/LEAKY`). |
| [`adversary`](../adversary/README.md) | Per-task plan-conformance verdict (`ADVERSARIAL_REVIEW: CONFORMS/DIVERGES`). |
| [`oracle`](../oracle/README.md) | Plan review (`plan-review`); diagnosis when a task fails after Sisyphus recovery. |
| [`explore`](../explore/README.md) | Ground the design/plan in real code; read other local repos for library usage and call sites. |
| [`librarian`](../librarian/README.md) | External docs / OSS examples for unfamiliar libraries. |
| [`whetstone`](../whetstone/README.md) | Continuous improvement — spawned once before the final report when the run addressed PR review feedback from outside its own lanes; converts review misses into user-approved suite edits / ledger entries. Never blocks the run. |
| [`review-gauntlet`](../review-gauntlet/README.md) | Preferred per-task conformance gate — one spawn running adversary (+ probe for consumer-facing tasks) with a machine-parsed GAUNTLET: PASS/BLOCKED verdict; a `GAUNTLET_REVIEW_INCOMPLETE:` line is an infrastructure fault escalated to the user (retry / accept NEEDS-HUMAN / abort), never a divergence; direct adversary/probe spawns remain the fallback. |

## Related skills

- [`design-session`](../../skills/design-session/SKILL.md) — design doc → grounded proposal → PLAN + sized breakdown.
- [`task-tracking`](../../skills/task-tracking/SKILL.md) — the task-file schema, lifecycle, and consistency checks.
- [`plan-gatekeeping`](../../skills/plan-gatekeeping/SKILL.md) — the gatekeeper's self-containedness manifest.
- [`plan-authoring`](../../skills/plan-authoring/SKILL.md) / [`plan-review`](../../skills/plan-review/SKILL.md) — plan schema + oracle's executability review.
