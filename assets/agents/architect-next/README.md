# architect-next (CANDIDATE, upstream variant)

The decomposed architect orchestrator as a session graph. Status: candidate - no agent
lists it as spawnable; the shipped `architect` keeps running the prose protocol until the
artifact-equivalence replay passes and this directory replaces it. The delegate is
`sisyphus` (two `agent:` lines); the swap to the graph sisyphus is one config edit plus
three `inputs:` lines (the design record lives in the operator's local planning store,
not in this repo).

## Shape

- **Spine** - script nodes (`scripts/gates.py`, one thin entry per node) own the mode
  routing (pr_strategy x parallel_tasks x task_granularity x autonomy x
  escalation_policy - every marker region of the prose architect's instructions.md is
  mapped in `scripts/marker_map.json`), the task scheduler, every named loop bound, the
  ledgers, sentinel parsing, prompt assembly, and the plans-dir task store per the
  `task-tracking` skill; `collect_ledger_lines` and `assemble_pr_body` are CALLED from
  `architect/tools.sh`, never reimplemented.
- **Judgment** - short single-question llm nodes with schemas; every schema carries the
  optional `anomalies` field; free-text nodes use the `ANOMALY: ` line convention.
- **Islands** - `gatekeeper`, `oracle`, `explore`/`librarian` (map fan-outs; team mode =
  `teammates: true`), `review-gauntlet`, `adversary`, `whetstone`, `engineer` (tier-3
  consult) and the `sisyphus` delegate run as agent nodes exactly as shipped.
- **Durable run state** - `<cache dir>/architect-next/runs/<run_id>/`, where the cache dir
  is `COYOTE_CACHE_DIR`, else `$XDG_CACHE_HOME/coyote`, else the platform cache directory
  (`~/Library/Caches/coyote` on macOS, `~/.cache/coyote` on Linux, `%LOCALAPPDATA%\coyote`
  on Windows); `ARCHITECT_RUNS_ROOT` overrides the whole path. The run dir holds the brief,
  state.json, tasks.json, research.md, reports/, rounds/, decisions ledger, anomalies and
  report. A prompt containing `Architect run id: <id>` re-enters that run.

Variables mirror `architect` (names and defaults identical), except the graph-only
`pr_tracking` (the `pr_tracking` review watch row). The session block routes
work to the graph and conversation to the `engineer` chat lane (`chat_agent_for: architect`).

## Landed-upstream adjustments

The directory is a verbatim copy of the upstream staging tree except for the rows
below. Later landings add their own rows.

| Adjustment | Where | What changed |
|---|---|---|
| em dashes | `graph.yaml` (3 lines: the `project_dir`, `pr_strategy` and `task_granularity` variable descriptions) | replaced with ASCII ` - ` so the repo's punctuation lint stays clean |
| description provenance | `graph.yaml` `description:` | the shipped description drops two references to planning files that live only in the operator's local planning store, outside the repo; every other word is staging's |
| `build_lock` variable | `graph.yaml` `variables:` | declared after `pr_strategy` with default `''`, mirroring the prose `architect/config.yaml`; `scripts/check_session_block.sh` asserts the two variable lists match name-for-name, default-for-default. No `inputs: build_lock` is needed on any child node: the engine hands a parent's agent variables to every agent-node child, config and graph alike (`src/graph/agent.rs` runs the child through `run_agent_for_graph`, whose `RequestContext::new_for_child` copies the parent's agent variables, and `src/graph/executor.rs` `seed_variables` seats them over the child graph's declared defaults; pinned by `src/function/agents.rs` `run_agent_for_graph_child_seats_the_parents_build_lock_over_its_default`), so `delegate` and `wave_delegate` (`sisyphus`), `run_gauntlet` and `wave_gauntlet` (`review-gauntlet`) and `run_adversary` (`adversary`) all receive the value by inheritance; each of the three agents behind those five nodes declares `build_lock` itself |
| run dir | `scripts/_lib.py` `cache_dir()` / `runs_root()`, this README | moved from `$XDG_STATE_HOME/coyote/architect/runs` to `<cache dir>/architect-next/runs` (precedence above); `scripts/smoke_test.py` pins each resolution tier |
| `utils.sh` source | `scripts/smoke_test.py`, `scripts/mode_matrix.py` | both batteries copy the repo asset `.shared/utils.sh` into the scratch `LLM_ROOT_DIR` instead of the user-config copy, which never existed under the re-pointed `HOME`, so the stub branch always ran; the real `set -euo pipefail` is now live inside `architect/tools.sh` under both batteries, with no unset-variable fallout. A smoke check pins that the copy is the real file; the matrix has no stub branch left: it exits before the first combination if the asset is missing or lacks `set -euo pipefail`, and prints `utils.sh: real asset` otherwise, which the ignored wrapper test asserts |
| marker map | `scripts/marker_map.json` | regenerated against the landed `architect/instructions.md`, which carries 62 marker regions (staging mapped 60): the two regions added since staging are the micro-mode `review_cadence: batch` line on the parallel wave spawn (mapped to the `## MODE` block `_delegation_text` builds for `compose_delegation` and `wave_claim`) and the multi-PR `--tasks <group members>` line, mapped to both of its homes: `pr_body` (`assemble_pr_body --tasks` from `_group_members`) and `hygiene_create` (`collect_ledger_lines --tasks` from the `_hygiene_units` group unit), both in multi-PR mode only, so a group's body and its starting ledger cover just that group's tasks. The 60 known regions keep their mechanism and home; four recorded first-lines changed because upstream reworded or ASCII-normalised those lines; `first` keeps `\u2014` JSON escapes for 13 lines of the prose file |
| dead `hygiene_pending_for_chain` guard | `scripts/gates.py` `stack_gate` | two lines deleted: the guard read `state.get("hygiene_pending_for_chain", {})`, a key no script, variable, `state_updates` target or output schema ever writes, so it was always falsy and the `continue` it guarded never ran; the repo's state-key walker (`bundled_graph_output_schemas_declare_every_key_relied_on`) rejects a consumed key with no declared writer, and deleting the read keeps behaviour identical rather than inventing a writer |
| gauntlet grammar | `scripts/gates.py` `verify_gate` / `_code_tree_dirty` / `_gauntlet_header` / `_gauntlet_section` / `_has_verdict` / `_lane_rows` / `_lane_cells` / `_probe_verdict` / `_parse_gauntlet` / `route_verdict` / `wave_route` / `_lane_table`, `scripts/smoke_test.py`, `scripts/mode_matrix.py` | the gates read the review gauntlet's reply by its header grammar, not by free-text search; the Gauntlet grammar subsection below records the whole contract |
| E9: variables reach config children too | `graph.yaml` `delegate` / `wave_delegate`, `scripts/gates.py` `_delegation_text` / `compose_delegation` / `wave_claim` | staging's "agent nodes have no variables channel for config children" holds only for a variable the child does not declare: the engine copies the parent's agent variables to every agent-node child, config and graph alike, and the child seats them over its own declared defaults. Of the variables both this graph and `sisyphus` declare (`project_dir`, `auto_confirm`, `escalation_policy`, `build_lock`), only `build_lock` is a value the delegate should inherit (the `build_lock` variable row). `review_cadence` and `parallel` are `sisyphus` variables this graph does not declare (the cadence is derived from micro mode), and `project_dir` is deliberately overridden with the task worktree, so those three stay in the `## MODE` block with the `mode-degraded:` ledger line. Every other child - the graph agent librarian and the config agents explore, gatekeeper, oracle, engineer and whetstone - receives `build_lock` by the same inheritance; a child that declares no `build_lock` does not consume it (no file of theirs names `build_lock`, `gate_prefix` or `run_gate`). Not a schema change: no node, prompt, bound or timeout moved |
| V1: session-resume replacement (artifact re-entry) - the contract | this README | fix rounds hand the `sisyphus` delegate the task's branch, commits and prior report verbatim, plus the `Sisyphus run id` that report carries, and later re-entries resume this graph's run by `Architect run id`; nothing is ever resumed by a child session id |
| engine facts | this README | agent-node resume: not available by decision (2026-10-28); E1 stays as the artifact contract; G6 escalation_timeout open |
| battery env hygiene | `scripts/smoke_test.py`, `scripts/mode_matrix.py`, `tests/candidate_architect_next.rs` | both batteries drop every `LLM_AGENT_VAR_*` variable at start so they cannot inherit the agent variables of the process running them (`_lib.var` reads the mode variables through them), drop `ARCHITECT_CORE_TOOLS`, `ARCHITECT_ASK_OWNER_BIN`, `ARCHITECT_SLACK_PING_BIN` and every `ARCHITECT_GH_*_CMD` so a caller's tool overrides cannot replace the gates' own `tools.sh`, `ask_owner`, `slack_ping` and `gh` calls, and drop `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY` and `GIT_COMMON_DIR` the same way, so a run from a git hook (which exports them) commits in the scratch repositories, not the caller's; the smoke battery's recording `tools.sh` restores any prior `ARCHITECT_CORE_TOOLS` when its call returns. Both also drop `GIT_CONFIG_PARAMETERS` and every inherited `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_*` and `GIT_CONFIG_VALUE_*`, so a caller's command-line git config cannot reach the scratch repositories, then set `GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=user.useConfigOnly` and `GIT_CONFIG_VALUE_0=true`, and set `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null` so a direct run ignores the system and user git config (signing, hooks) as well. After those drops both set a fixed `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME` and `GIT_COMMITTER_EMAIL`, which every gate and each battery's own git helper inherits: the scratch `HOME` holds no git identity, and git refuses to guess one when `user.useConfigOnly` is set or when the host name has no dot (it cannot build an email), so without the fixed identity every gate commit would fail on such a host; setting `user.useConfigOnly` makes every host behave that way. The Rust wrapper strips the repository variables from every git, script and binary command it starts and the agent variables and tool overrides from every script and binary command; it builds its scratch repository and runs the review-gauntlet scripts with `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null`, so no system or user git config (signing, hooks) applies, and runs the batteries with the same `GIT_CONFIG_*` drop and `user.useConfigOnly` setting. Every binary command it starts drops every inherited `COYOTE_*` variable (`COYOTE_PROVIDER`, `COYOTE_PLATFORM`, `COYOTE_WORKSPACE_CONFIG_DIR`, `COYOTE_CONFIG_FILE` and the rest) before it sets its own `COYOTE_CONFIG_DIR` and `COYOTE_CACHE_DIR`. Every scratch config dir it starts the binary from holds an `mcp.json` that defines no server: the bundled one launches `engineer`'s `ddg-search` through `uvx`, which CI hosts lack, and the engine never installs over an existing file |
| ledger rollup scope | `scripts/gates.py` `hygiene_create` / `_group_members` / `_skip_empty_group` / `group_closeout` / `pr_body` / `_collect` / `_completed_tasks_dir`, `scripts/smoke_test.py` | every `collect_ledger_lines` call passes `--plan`: the prose `architect/tools.sh` declares it required and hands it to `plan_tasks.py`, which exits `plan not found` without it, so staging's calls always fell back to `(collect_ledger_lines unavailable: ...)`. In multi-PR mode the group-scoped calls (`hygiene_create`'s starting ledger and `pr_body`'s `assemble_pr_body`) also pass `--tasks <group members>`, as the prose architect does, where the members are the closing group's complete tasks (a skipped, failed or parked member is not in the completed task store, so naming it would only cost a body fix round); the run report (`_collect`) passes no `--tasks` and stays run-scoped, because it covers every PR group. A group with no complete task makes no group-scoped call, since an empty `--tasks` would widen it to the whole plan: `group_closeout` skips that group's PR with a ledger line and a follow-up naming the group, then moves to the next group (or finishes the plan at run scope); `pr_body` skips an empty group the same way and `hygiene_create` skips its hygiene chore. Under the dirstate task store all three read the `completed` task dir, where complete tasks live. The smoke battery records each call's arguments and runs the real `tools.sh` over a scratch plan and task log |
| frozen budgets | `graph.yaml` `settings.max_loop_iterations` and `batch_gauntlet_build`, `tests/candidate_architect_next.rs` | the engine counts `max_loop_iterations` per node per run and never resets it, and `ci_poll` revisits itself up to `CI_MAX_ROUNDS` (240) times per PR before it routes `ci_status=timeout`, so staging's cap of 100 ended a long CI watch as an engine error; the cap is now 1000, still a finite defect detector, which covers four group PRs each watched to the 240-poll wall cap, and the wrapper pins it at four times or more the `CI_MAX_ROUNDS` that `_lib.py` computes. `batch_gauntlet_build` declared no `timeout:`; it only sets the batch verify scope and routes to `verify_gate`, so it now carries `verify_gate`'s `timeout: 60`. `review_poll` revisits itself up to `REVIEW_MAX_POLLS` (144) times per PR, so 1000 also covers four group PRs each watched to the review wall cap, and the pin now checks both, plus `REVIEW_MAX_POLLS` and `REVIEW_MAX_ROUNDS` staying below the cap |
| gh command seams | `scripts/gates.py` | `ARCHITECT_GH_COMMENT_CMD`, `ARCHITECT_GH_CREATE_CMD`, `ARCHITECT_GH_CHECKS_CMD`, `ARCHITECT_GH_LOG_CMD`, `ARCHITECT_GH_READY_CMD`, `ARCHITECT_GH_REVIEWS_CMD` and `ARCHITECT_GH_RESOLVE_CMD` replace the gates' `gh pr comment`, `gh pr create`, `gh pr checks`, `gh run view --log-failed` and `gh pr ready` calls, the `gh api graphql` review read and the `gh api graphql` thread reply and resolve mutations; a set checks seam also skips `gh run rerun`. Each runs its value from the operator's own environment through `bash -c` (the checks and reviews seams through `/bin/sh -c`; the resolve seam gets the review thread id as `$1`). They are test and operator seams, documented rather than gated: both batteries and the Rust wrapper drop any inherited value before they set their own |
| `pr_tracking` review watch | `graph.yaml` `variables:` / `initial_state` / `review_poll`, `review_route`, `review_triage`, `review_apply`, `review_update` and the `ci_route` description; `scripts/_lib.py`; `scripts/gates.py` `intake` / `apply_answer` / `fix_done` / `ci_route` / `flip_ready` / `review_poll` / `review_route` / `review_apply` / `review_update` / `final_report`; the new `scripts/review_poll.py`, `review_route.py`, `review_apply.py` and `review_update.py` entry shims; `scripts/audit_graph.py` bound labels; `scripts/check_session_block.sh`; `scripts/smoke_test.py`; `tests/candidate_architect_next.rs` | a new variable `pr_tracking`, default `ci`. `ci` is today's routing unchanged: the green PR is flipped ready and the plan finishes (or the next group starts), `mode_line` carries no `pr_tracking`, and the mode matrix still walks 53/53 without it; an unknown value is a `mode_invalid` escalation. `review` keeps watching the PR after the flip: `flip_ready` arms the watch and routes to `review_poll`, a bounded self-loop (one read every `REVIEW_POLL_SLEEP`, 600s) over the PR's state, approval, reviews, issue comments and unresolved threads that skips the watch's own `## Update @` and `## Review round` comments and every item already seen; `review_route` sends new items to the `review_triage` llm node, which marks each as fix, owner or none (owner when unsure; an item it leaves out counts as owner); `review_apply` turns the fix items into a fix round on the existing fix path (`fix_round`, the delegate, `fix_done`), and `fix_done` returns to `review_update`, which replies `Addressed in <sha>` on each addressed thread and resolves it, posts a `## Update @ <sha>` comment mapping each item to the head SHA and naming the items left unaddressed, then re-watches CI through `ci_poll`; `ci_route` green while the watch runs goes back to `review_poll` instead of flipping again. Stop conditions: approved (GitHub reports the PR approved, or an APPROVED review sits on the head commit) stops the watch, reports the outcome and continues; comments get a fix round, a push, a CI re-watch and then the watch again; CI red with the fix budget spent, or a CI infra failure, raises the existing `ci_exhausted` or `ci_infra` escalation, and leaving the PR there finishes the plan, never the next group; a PR closed externally stops the watch with a follow-up and finishes the plan, a PR merged externally stops it and continues; the review round cap (`REVIEW_MAX_ROUNDS`, 3 fix rounds per PR, `review_round_cap`) and the wall-clock and poll caps (`REVIEW_WALL_SECS` 86400s, `REVIEW_MAX_POLLS` 144, or `CI_POLL_FAILS` consecutive read errors, `review_exhausted`) escalate; a comment that needs the owner's judgment escalates as a hard trigger (`review_owner_judgment`). Every event is a ledger line `review-watch: PR #<n> ...` and a `slack_ping`, and the run report gains a `## Review watch` section with each PR's outcome. In group mode each group PR is watched and the next group starts only after that watch ends: stacking would let the next group start earlier, but a review fix pushed to group N would then need a restack of group N+1. `ci_rounds` stays cumulative per PR across review fix rounds, while the CI poll counters and wall clock are re-armed for each re-watch. The watch is an in-graph poll self-loop because a graph node cannot leave a background child running past its own exit: llm nodes reap their jobs and the pending-agent guardrail cancels spawned agents. `pr_tracking` is graph-only (the prose `architect/config.yaml` has no such variable), so `scripts/check_session_block.sh` and the Rust mirror checks compare the candidate's variables less the declared graph-only set (`pr_tracking`) against the prose twin, and a separate pin holds that `pr_tracking` is declared with default `ci` and is the only graph-only variable |

Recorded census against the staging tree (files that differ; hunks per file as counted by
`diff -u <staging file> <file> | grep -c '^@@'`, with `/dev/null` standing in for a staging
file that does not exist):

```
graph.yaml                9 hunks   (description provenance; letter-digit labels dropped from the description's cutover sentence; project_dir em dash; pr_strategy em dash + build_lock + task_granularity em dash + the pr_tracking variable + max_loop_iterations 1000 with its CI_MAX_ROUNDS and REVIEW_MAX_POLLS comment; the initial_state.skipped_group_indexes: [] seed; the review watch initial_state seeds; batch_gauntlet_build timeout: 60; the ci_route description names the review watch; the review_poll, review_route, review_triage, review_apply and review_update nodes)
scripts/_lib.py           4 hunks   (the REVIEW_POLL_SLEEP / REVIEW_WALL_SECS / REVIEW_MAX_POLLS / REVIEW_MAX_ROUNDS bounds and PR_TRACKING_ENUM; RESUME_RE takes the -<n> suffix of a claimed run id; new cache_dir, runs_root docstring and base path, new_run_id claims its run dir with os.mkdir; core_tool drops inherited argc_* variables)
scripts/smoke_test.py    11 hunks   (import types; ARCHITECT_REVIEW_SLEEP=0; LLM_AGENT_VAR_*, GIT_* repository-variable, ARCHITECT_CORE_TOOLS / ARCHITECT_ASK_OWNER_BIN / ARCHITECT_SLACK_PING_BIN / ARCHITECT_GH_*_CMD and GIT_CONFIG_* drop, user.useConfigOnly, no system or user git config, one fixed git identity + import _lib with REVIEW_MAX_POLLS / REVIEW_MAX_ROUNDS; the git helper's own GIT_ENV retired; utils.sh source + the real-copy pin; run dir resolution checks, the battery-env-restored check, the run-id claim and RESUME_RE rows; the empty-group stack rows (branch_setup stacks past a skipped group); gauntlet_reply / verdict_state fixtures in the real table shape, lanes_with_findings, composer settled-lanes, attention, in-repo planning-store, relpath ValueError, literal-pathspec, untracked-file, two-dot store, prior-round-only and dirty-tree rows, GPASS / GPROBE / GBLOCK; the adversary-lane verdict rows, the adversary probe rows (a quoted probe PASS or INCONCLUSIVE starts no probe round), GINC / GDEB; the parser rows: prose-quoted, trailing-text, indented, first match, whole status cells, first table only, USAGE_PROBE last match, probe detail cell, injected header overrides, sentinel only in a lane report, debris scope, forged Blockers headings, a plan-root quote in a lane report leaving a PASS on post_round / conforms and an incomplete-only BLOCKED on its re-run; the wave_route header rows; the pr_tracking review watch rows; the rollup-scope rows, the argc_* scrub row with argc_dir checked as dir=unset, the skipped-member and group-scoped fallback rows, the dirstate completed-dir row, the real collect_ledger_lines rows and the empty-group rows, including the skipped-group record and PR state reset)
scripts/mode_matrix.py    9 hunks   (LLM_AGENT_VAR_*, GIT_* repository-variable, ARCHITECT_CORE_TOOLS / ARCHITECT_ASK_OWNER_BIN / ARCHITECT_SLACK_PING_BIN / ARCHITECT_GH_*_CMD and GIT_CONFIG_* drop, user.useConfigOnly, no system or user git config, one fixed git identity; the git helper's own GIT_ENV retired; the yq guard in load_graph; canned gauntlet reply in the real table shape; main imports only CORE_TOOLS_REL; main removes the scratch HOME in a finally around walk_combinations; utils.sh source, fails loud instead of stubbing, confirmation line; the `if True` dropped; the old end-of-walk cleanup and summary moved into main)
scripts/_fixture.py       1 hunk    (the GIT_ENV identity overlay removed; the git helper inherits the battery environment and its one fixed git identity)
scripts/marker_map.json  25 hunks   (62 regions: two new, line numbers shifted, four first-lines reworded upstream)
scripts/gates.py         27 hunks   (the PR_TRACKING_ENUM / REVIEW_* import; GREEN tuple retired; intake's pr_tracking check and mode_line suffix; the mode_invalid option default for pr_tracking; branch_setup walks the stack base back past skipped groups; verify_gate: dirty-tree guard + whole-cell GREEN settling; one hunk for _code_tree_dirty, _gauntlet_header, _gauntlet_section, _GAUNTLET_VERDICT_RE / _has_verdict, _lane_rows, _lane_cells, _probe_verdict and the header-only _parse_gauntlet with its adversary-lane flag, the adversary-lane probe guard and single Blockers reader; route_verdict: lane flag, header pre-check, probe verdict behind the adversary-lane probe guard, lanes_with_findings; the probe adopt and INCONCLUSIVE checks, 2 hunks; _lane_table; fix_done returns a review fix round to review_update; dead hygiene_pending_for_chain guard in stack_gate removed; wave_route header pre-check; apply_answer parses pr_tracking from a mode_invalid answer; apply_answer ends the review watch when a watched PR is left red and answers the review_exhausted / review_round_cap / review_owner_judgment escalations; hygiene_create skips an empty group's chore; hygiene_create collect_ledger_lines --plan / --tasks over the completed tasks dir; _completed_tasks_dir, _group_members and _skip_empty_group, which records the skipped group index and resets the per-group PR state; pr_body: empty-group skip, --tasks, the completed tasks dir and the group-scoped fallback list; the gh command seams comment in open_pr, now naming the reviews and resolve seams; ci_route green goes back to review_poll while the watch runs; flip_ready arms the watch, with _after_flip, the review_poll / review_route / review_apply / review_update gates and their helpers; _collect --plan over the completed tasks dir; final_report's Review watch section; group_closeout skips an empty group; the review_* GATES entries)
scripts/audit_graph.py    1 hunk    (the review_poll and review_apply bound labels)
scripts/check_session_block.sh  1 hunk    (the variable mirror leaves out the graph-only pr_tracking)
scripts/review_poll.py    1 hunk    (new entry shim for the review_poll node)
scripts/review_route.py   1 hunk    (new entry shim for the review_route node)
scripts/review_apply.py   1 hunk    (new entry shim for the review_apply node)
scripts/review_update.py  1 hunk    (new entry shim for the review_update node)
README.md                 2 hunks   (design-record aside in the status paragraph; run-state bullet, the variables-mirror sentence, this section and the Verify section)
```

Every other file is byte-identical to staging. The graph adds the five review watch nodes
(`review_poll` with `timeout: 900`, `review_triage` with 600, `review_apply` with 120,
`review_update` with 300, and `review_route` with none) and `scripts/_lib.py` the four review
bounds (`REVIEW_POLL_SLEEP` 600, `REVIEW_WALL_SECS` 86400, `REVIEW_MAX_POLLS` 144,
`REVIEW_MAX_ROUNDS` 3), as the `pr_tracking` review watch row records. Every staging node's
prompt is unchanged, and so is every staging loop bound and node timeout in `graph.yaml`,
`scripts/_lib.py` and `scripts/gates.py` except two in `graph.yaml`:
`settings.max_loop_iterations` is 1000 (staging 100) and `batch_gauntlet_build` carries
`timeout: 60` (staging none), as the frozen budgets row records.

Toolchain census (the gates must stay project-agnostic; verification commands come
from the brief, never from a hard-coded toolchain):

```
$ grep -nE '\b(cargo|npm|pnpm|yarn|go (test|build)|pytest|mvn|gradle|make)\b' scripts/*.py
scripts/_lib.py:566:    r"`[^`]+`|\b(go|cargo|npm|pnpm|yarn|make|pytest|python3?|bash|sh|grep|rg|curl|task|just|mvn|gradle|dotnet|bun)\b\s",
scripts/gates.py:42:    r"yarn\.lock$|pnpm-lock\.yaml$|Cargo\.(toml|lock)$|requirements.*\.txt$|poetry\.lock$",
scripts/gates.py:2663:        "file:line, at most 25 per run so iterate); identifiers that only make sense with the plan open; ...
scripts/gates.py:2672:        "failing test to make it pass; you still do NOT edit the plan or task files -- report instead.\n\n"
scripts/mode_matrix.py:176:`make run`
scripts/mode_matrix.py:273:            ... "local_run_recipe": "`make run`",
scripts/smoke_test.py:102:CODE_BASE = make_repo(CODE, {..., "CLAUDE.md": "Run `go test ./...` before committing.\n", ...
scripts/smoke_test.py:161:`make run`
scripts/smoke_test.py:500:u = gates.verify_gate(base_state(..., local_run_recipe="make run"))
scripts/smoke_test.py:872:   ... "acceptance_criteria": ["`go test ./...`"], ...
scripts/smoke_test.py:987:u = AA("probe_gap", "Supply the recipe: make run", ...)
```

None of these runs a toolchain: `_lib.py:566` is the alternation of the executable-criterion
classifier (`EXECUTABLE_RE` recognises a command-shaped acceptance criterion; it hardcodes no
command), `gates.py:42` is the dependency-manifest regex (the hits are `yarn`/`pnpm` inside
lockfile names), `gates.py:2663` and `:2672` are the English verb `make` in prompt prose, and
the `mode_matrix.py` and `smoke_test.py` lines are battery fixtures. The wrapper test
`no_gate_names_a_toolchain_command` keeps it that way with a `make`-less variant of the
pattern: it matches `\b(cargo|rustc|npm|pnpm|yarn|pytest|mvn|gradle|just)\b|\bgo (test|build|run)\b`
against every line of `graph.yaml` and of each `scripts/*.py` except `smoke_test.py`, and fails
on any hit outside four documented exemptions, each of which must match exactly one line
(so a deleted or moved line surfaces): the `_lib.py` classifier alternation, the `gates.py`
lockfile regex, and the English adverb `just` in a `gates.py` comment (`An amendment round
just ran`) and in a `gates.py` prompt sentence (`just do not write to them`).

### Gauntlet grammar

The gauntlet's reply opens with `GAUNTLET: <verdict>` on line 1 and appends every lane's
raw report under `## Full lane reports` inside `<details>` blocks, so `GAUNTLET:`,
`GAUNTLET_REVIEW_INCOMPLETE:`, the lane table and the `## Blockers` section are read only
from the header before the first column-0 `## Full lane reports` line (or before the first
`<details>` when that heading is absent), as column-0 whole lines with `[ \t]*` gaps, first
match winning: a prose-quoted, indented or trailing-text sentinel is ignored, so a missing
whole-line verdict is BLOCKED, and a `GAUNTLET: PASS` or a GREEN row injected below the
header cannot override it; `route_verdict` and `wave_route` pre-check the same header slice
with the parser's own regex (`_has_verdict`).

The lane table is read as `| lane | status | detail |` from the first contiguous run of
column-0 `|` lines, first row per lane, with the whole status cell kept (`GREEN`,
`GREEN (attention)`, `BLOCKED`, `SKIPPED`); staging's `GREEN` tuple of detail words is
gone. `USAGE_PROBE:` stays a whole-line last match; the probe verdict is the probe row's
detail cell (`PASS`, `FAIL`, `INCONCLUSIVE (environment)`), and the last `USAGE_PROBE:`
line of the header counts only when there is no probe row.

`ADVERSARIAL_REVIEW:` stays unanchored for the micro-mode adversary lane, and on that lane
it is the only verdict: `route_verdict` tells the parser which lane it reads, the
`GAUNTLET:` regex is not consulted there (a column-0 `GAUNTLET: PASS` in the adversary's
verification output cannot pass a DIVERGES), and the adversary's blockers are the bullets
under a `Blockers`, `Divergences` or `Complaints` heading plus the numbered items under its
`Complaints:` line, else the DIVERGES fallback. The adversary report has no lane table, so
it settles no lanes: a lane row it quotes is not a lane verdict and is not recorded.

Settled lanes are the lanes whose whole status cell is exactly `GREEN`, minus the lanes
with findings last round and never `debris`; `GREEN (attention)` never settles (a human
must look, so the lane re-runs); `Settled lanes:` is written only when non-empty and
`Re-review delta: <sha>..HEAD` stays bare. Only the immediately-prior round settles a lane,
as the review gauntlet's own settled-lanes contract reads it, so a lane GREEN two rounds
ago but SKIPPED last round runs again; this differs from the sisyphus-next twin, which
reads lane state across rounds. Neither line is written while the code repo has
uncommitted changes outside the planning store: when `plans_root` resolves inside the code
repo its paths are excluded, as a literal pathspec, from that
`git status --porcelain --untracked-files=normal` (untracked files count whatever the git
config says), because the in-repo planning store carries uncommitted task-status edits
until the task closes. This diverges from the sisyphus-next twin, which has no planning
store and checks the whole tree.

The header boundary is the review-gauntlet producer's guarantee (its verdict gate folds
every header value onto one line, so nothing it writes above the lane reports can start a
column-0 `## Full lane reports` or `<details>` line), and the consumer defends it as well.
The verdict, the incomplete line, the lane table, the `## Blockers` section and the debris
blocker lines (unbulleted `debris: Implementation debris` lines count only at column 0
inside that section) are read only from the header. There is one Blockers reader, as in
the twin: the slice below the first `## Lane selection` heading after the lane table, from
the last exact `## Blockers` heading to the next column-0 `## ` heading, supplies both the
bulleted blockers and the unbulleted debris lines, so a look-alike `## Blockers` inside the
lane-selection reasons never stands in for the real one.

Four reads take the full reply on purpose: `ADVERSARIAL_REVIEW:` (the direct micro-mode
adversary lane, whose report has no gauntlet header; its complaints and its own debris
lines are read from the whole report too, so a `<details>` block above its `Complaints:`
list cannot hide them), `PIPELINE-FAULT` (consulted only when the header carries no
verdict, so the round is already BLOCKED), the plan-root phrases (the adversary's
reasoning lives only in its lane report; consulted only on a round that already has real
blockers, where they pick the plan-defect escalation over a fix round) and the probe's
test-file paths (read only when the probe verdict is PASS, adoption capped at one round).
When the reply carries a gauntlet header verdict, none of them can turn a BLOCKED into a
PASS, or a PASS into anything but that one capped adopt round; the direct adversary lane
has no header, so its `ADVERSARIAL_REVIEW:` line is the only verdict there.

## Verify

```
python3 scripts/audit_graph.py .        # static edges + script _next edges, cycles vs bounds (needs mikefarah yq)
bash scripts/check_session_block.sh     # session/schema rules; variable mirror vs architect/config.yaml (needs mikefarah yq)
python3 scripts/smoke_test.py           # every gate, both sides of every bound, fail-open paths
python3 scripts/mode_matrix.py          # marker regions mapped + all 48 mode combinations walk to end_complete
cargo test --test candidate_architect_next              # the Rust wrapper (live test groups listed below)
cargo test --test candidate_architect_next -- --ignored # the mode matrix through the wrapper (three to five minutes; opt-in; needs mikefarah yq)
```

The wrapper's live test groups:

- batteries: audit, smoke (plus a tally check that a failure recorded by the battery's last
  check still fails the run, and an env-restore check that fails when the battery leaves
  `ARCHITECT_RUNS_ROOT` changed), session check, poison (the batteries never consult the user
  config dir), the mode-matrix fail-loud test (`mode_matrix.py` exits before the walk
  when `.shared/utils.sh` is missing or a stub), the matrix naming a failed `yq` read
  instead of a traceback, no `if True` block in `mode_matrix.py`, and no battery script
  keeping its own git identity overlay;
- run dir: run-dir precedence from a fresh process, and an empty `ARCHITECT_RUNS_ROOT`
  falling back to the cache-dir tier;
- graph and gates: `build_lock` position, the variable mirror (the candidate's variables,
  less the graph-only `pr_tracking`, match the prose `architect/config.yaml` name for name
  and default for default, without `yq`), the graph-only pin (`pr_tracking` is declared with
  default `ci`, is the only variable the prose twin lacks, and is the set
  `check_session_block.sh` leaves out of its mirror), the marker-map triples (every
  `(ordinal, condition, first)` of `marker_map.json`
  against the prose `architect/instructions.md`, the check the matrix makes, without `yq`),
  the description pins (`description_names_no_file_outside_the_repo`, and no
  letter-digit label in the description), toolchain census, ASCII sweep, and the shim pin
  (every `scripts/<name>.py` shim calls
  `run("<name>")`, names `<name>` in its docstring, the shims map one-to-one onto the
  `type: script` nodes of `graph.yaml`, and every `run()` name is a key of `GATES` in
  `gates.py`);
- gauntlet grammar: a prior report rendered by the review gauntlet's own `verdict_gate.py`
  and `done` template, read by `route_verdict`, composed into the next round by
  `verify_gate` and narrowed through the gauntlet's real `signals.py` and `build_items.py`;
  the no-settled-lanes round; the parser rows; the planning-store dirty-tree rows;
- budgets and channels: graph, node, `initial_state` and `_lib.py` bounds against the shipped
  baseline (the staging values plus the frozen budgets row and the review watch nodes, seeds
  and bounds), the loop cap at four times or more both the `CI_MAX_ROUNDS` and the
  `REVIEW_MAX_POLLS` that `_lib.py` computes, with `REVIEW_MAX_POLLS` and `REVIEW_MAX_ROUNDS`
  below it, the `anomalies` property on every llm node (`review_triage` included), and no
  `inputs: build_lock` anywhere; the real `tools.sh` `collect_ledger_lines` rollup; both
  batteries drop the same variables as the wrapper;
- wrapper hygiene: every binary runner drops the inherited `COYOTE_*` variables before its
  first `env` (and later sets survive the drop), the batteries' runner drops the inherited
  `GIT_CONFIG_*` before it sets `user.useConfigOnly`, and every scratch config dir holds an
  `mcp.json` that defines no server; the timeout helper kills a child that runs past its
  limit; the `yq` lookup skips a `yq` that is not the mikefarah build, and fails instead
  when `ARCHITECT_NEXT_REQUIRE_YQ` is set;
- README ground-truth pins: toolchain census, counts against the graph and the prose twin,
  the `build_lock` and E9 rows, `pr_body` passing `--tasks`, the dead-guard census row, the
  grammar subsection, V1, engine-facts, env hygiene, ledger and gh command seams rows, the
  battery figures against the wrapper floors, and the recorded staging census against a live
  `diff -u` when `ARCHITECT_NEXT_STAGING_DIR` names the staging tree (a file staging lacks is
  diffed against `/dev/null`; without the variable the test prints a skip line and checks
  nothing; set to a path that is not a directory, it fails);
- real-binary dry runs: four by name (from a seeded config dir; from an empty config dir that
  installs the candidate; re-entry by run id in the prompt; an unknown run id in the prompt
  starts a fresh run), the XDG-tier dry run (only `XDG_CACHE_HOME` set, the run lands under
  the engine's cache dir), the platform-tier dry run (no cache variable set, the run lands
  under the platform cache dir of the scratch `HOME`, the root `_lib.runs_root()` names too),
  and the no-design-doc dry run (without a readable design doc the run parks on the
  `need_design_doc` escalation before orienting).
- real-binary `--info`: by name it lists the graph with no error, renders `build_lock` with
  its empty default between `pr_strategy` and `task_granularity`, seats an
  `--agent-variable build_lock` override over that default, and renders exactly the prose
  twin's variable list in order once the graph-only `pr_tracking` is left out.

The mode-matrix run is the one ignored test.

The Rust wrapper runs the batteries with `HOME`, `LLM_ROOT_DIR`, `COYOTE_CACHE_DIR`,
`TMPDIR` and `ARCHITECT_RUNS_ROOT` pointed at a scratch directory and
`PYTHONDONTWRITEBYTECODE=1`, and drops any inherited `GRAPH_STATE*` / `LLM_AGENT_VAR_*`
variables, the `ARCHITECT_CORE_TOOLS` / `ARCHITECT_GH_*_CMD` tool overrides and the git
repository variables,
so nothing under this directory or `~/.config/coyote` is touched and no agent session leaks
in. `audit_graph.py`, `check_session_block.sh` and `mode_matrix.py` need the Go `yq`
by mikefarah (`yq --version` names it); the wrapper's audit, session-check and poison tests,
and the ignored matrix, skip when a different `yq` (or none) is on `PATH`, and fail instead
when `ARCHITECT_NEXT_REQUIRE_YQ` is set to a non-empty value. The sweep that
keeps every bundled agent from naming this candidate lives in
`tests/candidate_sisyphus_next.rs` and covers both candidate names.

Measured 2026-11-02 after the adjustments above, from a fresh scratch dir: audit
`PROBLEMS: none`; smoke `423/423 checks passed`; mode matrix `53/53 checks passed`
(`62 live regions == 62 mapped`) in 3m27s wall clock; session check last line
`ok node ids / script paths consistent` (mikefarah yq v4.53.3); no unset-variable failure
from `architect/tools.sh` under the real `utils.sh`.
