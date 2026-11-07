# architect-next (CANDIDATE, upstream variant)

The decomposed architect orchestrator as a session graph. Status: candidate - no agent
lists it as spawnable; the shipped `architect` keeps running the prose protocol until the
artifact-equivalence replay passes and this directory replaces it. The delegate is the
graph `sisyphus-next` (two `agent:` lines, `delegate` and `wave_delegate`), which takes
its review cadence, parallel flag and project directory through `inputs:` (the design
record lives in the operator's local planning store, not in this repo).

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
  consult) and the `sisyphus-next` delegate run as agent nodes exactly as shipped.
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
| `build_lock` variable | `graph.yaml` `variables:` | declared after `pr_strategy` with default `''`, mirroring the prose `architect/config.yaml`; `scripts/check_session_block.sh` asserts the two variable lists match name-for-name, default-for-default. No `inputs: build_lock` is needed on any child node: the engine hands a parent's agent variables to every agent-node child, config and graph alike (`src/graph/agent.rs` runs the child through `run_agent_for_graph`, whose `RequestContext::new_for_child` copies the parent's agent variables, and `src/graph/executor.rs` `seed_variables` seats them over the child graph's declared defaults; pinned by `src/function/agents.rs` `run_agent_for_graph_child_seats_the_parents_build_lock_over_its_default`), so `delegate` and `wave_delegate` (`sisyphus-next`), `run_gauntlet` and `wave_gauntlet` (`review-gauntlet`) and `run_adversary` (`adversary`) all receive the value by inheritance; each of the three agents behind those five nodes declares `build_lock` itself |
| run dir | `scripts/_lib.py` `cache_dir()` / `runs_root()`, this README | moved from `$XDG_STATE_HOME/coyote/architect/runs` to `<cache dir>/architect-next/runs` (precedence above); `scripts/smoke_test.py` pins each resolution tier |
| `utils.sh` source | `scripts/smoke_test.py`, `scripts/mode_matrix.py` | both batteries copy the repo asset `.shared/utils.sh` into the scratch `LLM_ROOT_DIR` instead of the user-config copy, which never existed under the re-pointed `HOME`, so the stub branch always ran; the real `set -euo pipefail` is now live inside `architect/tools.sh` under both batteries, with no unset-variable fallout. A smoke check pins that the copy is the real file; the matrix has no stub branch left: it exits before the first combination if the asset is missing or lacks `set -euo pipefail`, and prints `utils.sh: real asset` otherwise, which the ignored wrapper test asserts |
| marker map | `scripts/marker_map.json` | regenerated against the landed `architect/instructions.md`, which carries 62 marker regions (staging mapped 60): the two regions added since staging are the micro-mode `review_cadence: batch` line on the parallel wave spawn (mapped to the wave item's `review_cadence`, which `wave_claim` sets and `wave_delegate` passes through `inputs:`, with the `## MODE` block `_delegation_text` builds for `compose_delegation` and `wave_claim` stating it too) and the multi-PR `--tasks <group members>` line, mapped to both of its homes: `pr_body` (`assemble_pr_body --tasks` from `_group_members`) and `hygiene_create` (`collect_ledger_lines --tasks` from the `_hygiene_units` group unit), both in multi-PR mode only, so a group's body and its starting ledger cover just that group's tasks. The 60 known regions keep their mechanism and home; four recorded first-lines changed because upstream reworded or ASCII-normalised those lines; `first` keeps `\u2014` JSON escapes for 13 lines of the prose file |
| dead `hygiene_pending_for_chain` guard | `scripts/gates.py` `stack_gate` | two lines deleted: the guard read `state.get("hygiene_pending_for_chain", {})`, a key no script, variable, `state_updates` target or output schema ever writes, so it was always falsy and the `continue` it guarded never ran; the repo's state-key walker (`bundled_graph_output_schemas_declare_every_key_relied_on`) rejects a consumed key with no declared writer, and deleting the read keeps behaviour identical rather than inventing a writer |
| gauntlet grammar | `scripts/gates.py` `verify_gate` / `_code_tree_dirty` / `_gauntlet_header` / `_gauntlet_section` / `_has_verdict` / `_lane_rows` / `_lane_cells` / `_probe_verdict` / `_parse_gauntlet` / `route_verdict` / `wave_route` / `_lane_table`, `scripts/smoke_test.py`, `scripts/mode_matrix.py` | the gates read the review gauntlet's reply by its header grammar, not by free-text search; the Gauntlet grammar subsection below records the whole contract |
| E9: variables reach every agent-node child | `graph.yaml` `delegate` / `wave_delegate`, `scripts/gates.py` `_delegation_text` / `compose_delegation` / `wave_claim` | staging's "agent nodes have no variables channel for config children" holds only for a variable the child does not declare: the engine copies the parent's agent variables to every agent-node child, config and graph alike, and the child seats them over its own declared defaults. Of the variables both this graph and `sisyphus-next` declare (`project_dir`, `auto_confirm`, `escalation_policy`, `build_lock`), only `build_lock` is a value the delegate should inherit (the `build_lock` variable row). `review_cadence` and `parallel` are `sisyphus-next` variables this graph does not declare (the cadence is derived from micro mode), and `project_dir` is deliberately overridden with the code repo or the task worktree, so `delegate` and `wave_delegate` pass those three through `inputs:`, which the engine seeds into the child's initial state ahead of the inherited variables: `delegate` from the `delegate_review_cadence`, `delegate_parallel` and `delegate_project_dir` keys `compose_delegation` sets, `wave_delegate` as `parallel: '1'` with the wave item's `worktree` and `review_cadence` (`wave_claim` gives every item `batch` in micro mode, else `task`). The `## MODE` block stays in the prompt as a note for the delegate's llm nodes, and no `mode-degraded:` ledger line is written. Every other child - the graph agent librarian and the config agents explore, gatekeeper, oracle, engineer and whetstone - receives `build_lock` by the same inheritance; a child that declares no `build_lock` does not consume it (no file of theirs names `build_lock`, `gate_prefix` or `run_gate`). Engine evidence: a map node sets its `as:` item on each branch's forked state (`src/graph/map.rs:77-82`), `resolve_inputs` interpolates every `inputs:` value raw, so a lone `{{wtask.worktree}}` keeps the item's value (`src/graph/agent.rs:324-342`), and the resolved inputs extend the child's initial state after the carried keys (`src/graph/dispatch.rs:360`); the engine test `map_item_field_reaches_the_child_through_agent_node_inputs` in `src/graph/map.rs` runs that chain end to end. `teammates: true` on `wave_delegate` reaches a graph child too: the engine gives each flagged agent node in the frontier a teammate seat (`src/graph/executor.rs:564-569`) and, since a graph child has no system prompt, appends the roster to the seed its graph reads as `initial_prompt` (`src/function/agents.rs:744-760`) |
| V1: session-resume replacement (artifact re-entry) - the contract | this README | fix rounds hand the `sisyphus-next` delegate the task's branch, commits and prior report verbatim, plus the `Sisyphus run id` that report carries, and later re-entries resume this graph's run by `Architect run id`; nothing is ever resumed by a child session id |
| engine facts | this README | agent-node resume: not available by decision (2026-10-28); E1 stays as the artifact contract; G6 escalation_timeout open |
| battery env hygiene | `scripts/smoke_test.py`, `scripts/mode_matrix.py`, `tests/candidate_architect_next.rs` | both batteries drop every `LLM_AGENT_VAR_*` variable at start so they cannot inherit the agent variables of the process running them (`_lib.var` reads the mode variables through them), drop `ARCHITECT_CORE_TOOLS`, `ARCHITECT_ASK_OWNER_BIN`, `ARCHITECT_SLACK_PING_BIN` and every `ARCHITECT_GH_*_CMD` so a caller's tool overrides cannot replace the gates' own `tools.sh`, `ask_owner`, `slack_ping` and `gh` calls, and drop `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY` and `GIT_COMMON_DIR` the same way, so a run from a git hook (which exports them) commits in the scratch repositories, not the caller's; the smoke battery's recording `tools.sh` restores any prior `ARCHITECT_CORE_TOOLS` when its call returns. Both also drop `GIT_CONFIG_PARAMETERS` and every inherited `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_*` and `GIT_CONFIG_VALUE_*`, so a caller's command-line git config cannot reach the scratch repositories, then set `GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=user.useConfigOnly` and `GIT_CONFIG_VALUE_0=true`, and set `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null` so a direct run ignores the system and user git config (signing, hooks) as well. After those drops both set a fixed `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME` and `GIT_COMMITTER_EMAIL`, which every gate and each battery's own git helper inherits: the scratch `HOME` holds no git identity, and git refuses to guess one when `user.useConfigOnly` is set or when the host name has no dot (it cannot build an email), so without the fixed identity every gate commit would fail on such a host; setting `user.useConfigOnly` makes every host behave that way. The Rust wrapper strips the repository variables from every git, script and binary command it starts and the agent variables and tool overrides from every script and binary command; it builds its scratch repository and runs the review-gauntlet scripts with `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null`, so no system or user git config (signing, hooks) applies, and runs the batteries with the same `GIT_CONFIG_*` drop and `user.useConfigOnly` setting. Every binary command it starts drops every inherited `COYOTE_*` variable (`COYOTE_PROVIDER`, `COYOTE_PLATFORM`, `COYOTE_WORKSPACE_CONFIG_DIR`, `COYOTE_CONFIG_FILE` and the rest) before it sets its own `COYOTE_CONFIG_DIR` and `COYOTE_CACHE_DIR`. Every scratch config dir it starts the binary from holds an `mcp.json` that defines no server: the bundled one launches `engineer`'s `ddg-search` through `uvx`, which CI hosts lack, and the engine never installs over an existing file |
| ledger rollup scope | `scripts/gates.py` `hygiene_create` / `_group_members` / `_skip_empty_group` / `group_closeout` / `pr_body` / `_collect` / `_completed_tasks_dir`, `scripts/smoke_test.py` | every `collect_ledger_lines` call passes `--plan`: the prose `architect/tools.sh` declares it required and hands it to `plan_tasks.py`, which exits `plan not found` without it, so staging's calls always fell back to `(collect_ledger_lines unavailable: ...)`. In multi-PR mode the group-scoped calls (`hygiene_create`'s starting ledger and `pr_body`'s `assemble_pr_body`) also pass `--tasks <group members>`, as the prose architect does, where the members are the closing group's complete tasks (a skipped, failed or parked member is not in the completed task store, so naming it would only cost a body fix round); the run report (`_collect`) passes no `--tasks` and stays run-scoped, because it covers every PR group. A group with no complete task makes no group-scoped call, since an empty `--tasks` would widen it to the whole plan: `group_closeout` skips that group's PR with a ledger line and a follow-up naming the group, then moves to the next group (or finishes the plan at run scope); `pr_body` skips an empty group the same way and `hygiene_create` skips its hygiene chore. Under the dirstate task store all three read the `completed` task dir, where complete tasks live. The smoke battery records each call's arguments and runs the real `tools.sh` over a scratch plan and task log |
| frozen budgets | `graph.yaml` `settings.max_loop_iterations`, `batch_gauntlet_build`, `ci_poll`, `ci_evidence`, `ci_apply`, `flip_ready`, `review_route` and `review_update`, `scripts/_lib.py` `GH_CALL_SECS` / `GH_LOOP_SECS` / `REVIEW_UPDATE_BUDGET` / `PERM_LOOKUPS_PER_POLL` / `POLL_VISIT_BUDGET`, `tests/candidate_architect_next.rs` | the engine counts `max_loop_iterations` per node per run and never resets it, and `ci_poll` revisits itself up to `CI_MAX_ROUNDS` (240) times per PR before it routes `ci_status=timeout`, so staging's cap of 100 ended a long CI watch as an engine error; the cap is now 1500, still a finite defect detector, which covers four group PRs each watched to the 240-poll wall cap, and the wrapper pins it at four times or more the `CI_MAX_ROUNDS` that `_lib.py` computes. `batch_gauntlet_build` declared no `timeout:`; it only sets the batch verify scope and routes to `verify_gate`, so it now carries `verify_gate`'s `timeout: 60`. `review_poll` revisits itself up to `REVIEW_MAX_POLLS` (144) times per PR, so 1500 also covers four group PRs each watched to the review wall cap, and the pin checks both, plus `REVIEW_MAX_POLLS` and `REVIEW_MAX_ROUNDS` staying below the cap (`loop_cap_exceeds_the_ci_and_review_poll_rounds_lib_computes`). A poll node's timeout must outlast its sleep plus every gh call it makes, so a node timeout never kills a poll mid-read: `ci_poll` carries `timeout: 180` (staging 120) over `CI_POLL_SLEEP` 60 plus one `GH_CALL_SECS` 90, and `review_poll` 900 over `REVIEW_POLL_SLEEP` 600 plus (1 + `PERM_LOOKUPS_PER_POLL`) * `GH_CALL_SECS`, the review read and at most `PERM_LOOKUPS_PER_POLL` (2) permission reads, 870 in all. `ci_apply`, `ci_evidence` and `flip_ready` make gh calls in a loop (workflow re-runs, failed-log reads, draft-flag reads) and start no new one once `GH_LOOP_SECS` (180) has passed since the gate began, ledgering what they skipped; each carries `timeout: 390` (staging 120, 300 and 300), at least `GH_LOOP_SECS` + 2 * `GH_CALL_SECS` + 30: the call started just inside the limit runs a full `GH_CALL_SECS` past it, one more call can fall outside the loop (the `gh pr ready` flip in `flip_ready`), and 30 seconds cover the gate's own work. `review_update` carries 1020, at least `REVIEW_UPDATE_BUDGET` + 4 * `GH_CALL_SECS` (the PR head read before the budget, the reply and resolve of the thread that straddles it, and the closing update comment), and `review_route` carries 120. `POLL_VISIT_BUDGET` (`4 * CI_MAX_ROUNDS + CI_MAX_ROUNDS`, 1200) counts `ci_poll` and `review_poll` visits across the whole run: it covers four group PRs each watched to the CI wall cap plus one more watch of headroom, fits one full review watch (144), and sits below the cap of 1500, so the watch stops by name through `watch_visit_cap` before the engine cap. The wrapper pins these rules: `poll_node_timeouts_cover_the_sleep_and_one_gh_call` (`ci_poll`, `review_poll`, `review_update` and the three looping CI nodes, each against the formula above) and `poll_visit_budget_stops_the_watch_before_the_engine_cap` (below the cap, at least four CI watches plus a quarter of one, at least one full watch of each poll, and the `ci_polls_run` and `review_polls_run` counters seeded at 0), and `graph_and_gate_budgets_equal_the_frozen_baseline` holds every graph, node and `_lib.py` bound to the shipped baseline |
| gh command seams | `scripts/gates.py` | `ARCHITECT_GH_COMMENT_CMD`, `ARCHITECT_GH_CREATE_CMD`, `ARCHITECT_GH_CHECKS_CMD`, `ARCHITECT_GH_HEAD_CMD`, `ARCHITECT_GH_LOG_CMD`, `ARCHITECT_GH_PERM_CMD`, `ARCHITECT_GH_READY_CMD`, `ARCHITECT_GH_RERUN_CMD`, `ARCHITECT_GH_REVIEWS_CMD`, `ARCHITECT_GH_RESOLVE_CMD` and `ARCHITECT_GH_VIEW_CMD` replace the gates' `gh pr comment`, `gh pr create` and `gh pr checks` calls, the `gh pr view` read of the PR head commit, the `gh run view --log-failed` call, the `gh api repos/{owner}/{repo}/collaborators/{login}/permission` read of a reviewer's permission, the `gh pr ready` and `gh run rerun --failed` calls, the `gh api graphql` review read, the `gh api graphql` thread reply and resolve mutations, and the `gh pr view` reads of a PR's state and draft flag. Each runs its value from the operator's own environment through `bash -c` with positional args (the comment seam gets the PR number and the body, the head seam the PR number, the perm seam the reviewer login and prints the permission word, the rerun seam the run id, the resolve seam the review thread id and the reply body, the view seam the PR number or branch and the JSON field; the create, log and ready seams get none), except the checks and reviews seams, which run through `/bin/sh -c` with no args. A set rerun seam replaces `gh run rerun`, once per distinct run id; with no rerun seam, a set checks seam skips `gh run rerun` instead of re-running a real workflow. Every `gh` call and every seam run is bounded by `GH_CALL_SECS` (90s, `scripts/_lib.py`), and the looped log, rerun and draft-flag calls in `ci_evidence`, `ci_apply` and `flip_ready` also stop once `GH_LOOP_SECS` (180s) has passed. They are test and operator seams, documented rather than gated: both batteries and the Rust wrapper drop any inherited value before they set their own |
| `pr_tracking` review watch | `graph.yaml` `variables:` / `initial_state` / `ci_evidence`, `ci_apply`, `flip_ready`, `review_poll`, `review_route`, `review_triage`, `review_apply`, `review_update` and the `ci_route` description; `scripts/_lib.py`; `scripts/gates.py` `intake` / `apply_answer` / `fix_round` / `post_round` / `fix_done` / `ci_poll` / `ci_route` / `ci_evidence` / `ci_apply` / `flip_ready` / `review_poll` / `review_route` / `review_apply` / `review_update` / `final_report`; the new `scripts/review_poll.py`, `review_route.py`, `review_apply.py` and `review_update.py` entry shims; `scripts/audit_graph.py` bound labels; `scripts/check_session_block.sh`; `scripts/smoke_test.py`; `tests/candidate_architect_next.rs` | a new variable `pr_tracking`, default `ci`. `ci` keeps today's routing except these hardening fixes, which apply in both modes: the `ci_poll` timeout 180; the `ci_apply`, `ci_evidence` and `flip_ready` timeouts 390, with their looped gh calls stopping after `GH_LOOP_SECS` (180) and `ci_apply` re-runs deduped by run id (two failing checks of one workflow run make one re-run call, and both checks' re-run counters count it); a stale or killed poll counted as one read failure; the run-wide `POLL_VISIT_BUDGET` (1200) that stops both polls by name below `settings.max_loop_iterations` (1500); the hidden own-comment marker `<!-- architect-next:own -->` on every comment and reply the gates post; every gh call and seam run bounded by `GH_CALL_SECS` (90); and the outer fix return target kept in `fix_origin_return`, so a CI or review fix round whose re-review comes back BLOCKED, or that adopts a probe, still returns to `ci_poll` or `review_update` once it passes. `fix_round` sets `fix_origin_return` only for ci and review fix rounds; integration and batch fix rounds leave it empty and route as before. Otherwise the green PR is flipped ready and the plan finishes (or the next group starts), `mode_line` carries no `pr_tracking`, and the mode matrix still walks 53/53 without it; an unknown value is a `mode_invalid` escalation. `review` keeps watching the PR after the flip. The watch covers single and PR-group PRs; per-task PRs are not watched. Multi mode with one stacked PR per task exists only in the other fork (this upstream variant always groups), and there `pr_tracking=review` makes `intake` ledger `review-watch: not started: per-task PRs are not watched (pr_tracking=review needs pr_strategy single or PR groups)` and add it as a follow-up. `flip_ready` arms the watch only when `gh pr ready` succeeded: when the flip fails it ledgers `review-watch: PR #<n> still draft; review watch not started`, adds that line as a follow-up and moves on as `ci` does. Once armed it routes to `review_poll`, a bounded self-loop (one read every `REVIEW_POLL_SLEEP`, 600s) over the PR's state, approval, reviews, issue comments and review threads that skips the watch's own comments and every item already seen. Each read takes the last 100 reviews, the last 100 issue comments and the last 100 review threads, each thread as its root comment plus its last 50 comments (a long thread loses only its middle); a list whose `totalCount` is above the page of 100, or a thread list with a previous page, stops the watch as `review_read_truncated` before any item or approval is read from it. The issue comment list is the exception: when it overflows, it counts as truncated only when the oldest fetched comment (by `createdAt`) is newer than the watch start and is neither already seen nor the watch's own, since otherwise every unfetched comment is older still or already handled; with no fetched comment, or a `createdAt` that does not parse, it counts as truncated. Approval: a non-empty `reviewDecision` decides on its own (only APPROVED approves; CHANGES_REQUESTED or REVIEW_REQUIRED never approves, whatever the reviews say), and an APPROVED `reviewDecision` approves before any permission read; only when it is empty does the watch reduce the reviews to each author's latest APPROVED, CHANGES_REQUESTED or DISMISSED verdict, and it approves when at least one trusted author's latest is APPROVED on the head commit and no author's latest is CHANGES_REQUESTED. Trust: an OWNER is trusted outright; MEMBER and COLLABORATOR are only a pre-filter and alone are not enough, as the author also needs write, maintain or admin permission on the repo, read through `gh api repos/{owner}/{repo}/collaborators/{login}/permission` (seam `ARCHITECT_GH_PERM_CMD`) and cached per login for the run; any other association is untrusted with no lookup. A poll makes at most `PERM_LOOKUPS_PER_POLL` (2) uncached reads: with more uncached logins it reads two, ledgers the deferral and polls again, so the rest wait for the next poll. A failed permission read is recorded in `review_perm_failed` (emptied each time the watch is armed), that login is not read again for the rest of the watch, and it counts as untrusted. Review comment bodies are untrusted data: only trusted authors' comments are read and triaged, and even those reach the triage node and the fix note only between `<untrusted-review-data>` markers as data, never as instructions; a literal `<untrusted-review-data>` or `</untrusted-review-data>` tag inside a reviewer body (in any letter case, with or without inner spaces) is replaced before wrapping, so a body cannot close the markers; any untrusted author's comment is ledgered and escalated to the owner by id, author and association alone, with no body in any coder-bound field. A thread with a stray untrusted comment still triages its trusted comments: the stray's body is left out of the thread text and the stray is listed by id only, as its own untrusted item; a thread whose reviewer comments are all untrusted is one untrusted item. The untrusted alert is batched to one ledger line and one `slack_ping` per poll naming every untrusted item, and `review_update` sends one ping per update rather than one per addressed item. Answering "address" on an untrusted `review_owner_judgment` escalation is refused: the refusal is ledgered and the same escalation is raised again with the refusal prefixed once. Every comment and reply the gates post ends with the hidden marker `<!-- architect-next:own -->`, and only a trusted author's comment can count as the watch's own (by that marker, or, for comments posted before the marker existed, by a `## Update @`, `## Review round` or `## Batch review round` heading or a bare `Addressed in <sha>` reply), so a copied marker hides nothing. A thread item is keyed by its newest trusted reviewer comment (its newest reviewer comment when none is trusted), so a reviewer's answer on a thread the watch already handled surfaces again, and a resolved thread re-surfaces when a reviewer answered after the watch's reply; outdated threads are still watched, and their items are flagged `outdated` for triage. `review_route` sends new trusted items to the `review_triage` llm node, which marks each as fix, owner or none (owner when unsure; an item it leaves out counts as owner), and raises `review_owner_judgment` for untrusted items without triaging them; `review_apply` turns the fix items into a fix round on the existing fix path (`fix_round`, the delegate, `fix_done`) with a fresh gauntlet budget each round (the owning task's conformance and debris re-run counters reset), and the fix delegation is push-only: the PR is already open and ready, so the delegate pushes its commits and never opens, re-publishes, retitles, edits or converts the PR to draft. `fix_done` returns to `review_update` (through `fix_origin_return`, even after a BLOCKED re-review or a probe adoption inside the round), which first checks that the local HEAD is the PR head (`review_push_mismatch` when it is not: re-check the PR head, or stop watching and report), then replies `Addressed in <sha>` on each addressed thread and resolves it within `REVIEW_UPDATE_BUDGET` (600s of thread replies; threads past it are listed as not resolved and come back on the next poll), feeds each addressed trusted item to whetstone through `external_feedback` with its body wrapped in the `<untrusted-review-data>` markers, posts a `## Update @ <sha>` comment mapping each item to the head SHA and naming the items left unaddressed, then re-watches CI through `ci_poll`; `ci_route` green while the watch runs goes back to `review_poll` instead of flipping again. A poll killed at its node timeout, or one that leaves a stale pending or empty status, is counted by `ci_route` or `review_route` as a read failure and polled again, so only `CI_POLL_FAILS` (3) failures in a row escalate. Both polls count their visits across the whole run, and at `POLL_VISIT_BUDGET` (1200) each stops by name through the `watch_visit_cap` escalation before the engine's `max_loop_iterations` cap (1500) would end the run as an error. Stop conditions: approved (by the approval rule above) stops the watch, reports the outcome and continues; comments get a fix round, a push, a CI re-watch and then the watch again; `ci_exhausted` (CI red with the fix budget spent, or the CI re-watch ending without a verdict), `ci_infra` (a CI infrastructure failure) and `ci_unsure` (a CI diagnosis that stays unsure) each offer `Stop watching and report` during the watch, and stopping there finishes the plan, never the next group; `review_external_close` stops on a PR closed or merged externally, closed with a follow-up that finishes the plan, merged continuing; `review_round_cap` fires when new comments arrive after `REVIEW_MAX_ROUNDS` (3) fix rounds per PR (one more fix round, or stop); `review_exhausted` fires at the wall-clock or poll cap (`REVIEW_WALL_SECS` 86400s, `REVIEW_MAX_POLLS` 144) or after `CI_POLL_FAILS` consecutive read errors (re-arm the watch, or stop); `review_read_truncated` fires on an overflowing review list (re-arm the watch, or stop; when the issue comment list is the only one that overflowed it offers only `Stop watching and report`, because re-arming moves the watch start past the unread comments); `review_owner_judgment` is a hard trigger for an item the triage marks owner (address it as a fix round, skip it, or stop) or for untrusted activity (skip it, or stop); `review_push_mismatch` fires when the fix is missing from the PR head (re-check, or stop); `watch_visit_cap` fires when either poll reaches `POLL_VISIT_BUDGET` (stop watching and report). Stopping on `review_exhausted`, `review_read_truncated`, `review_round_cap` or `review_owner_judgment` moves on as an approval does; stopping on `review_push_mismatch` or `watch_visit_cap` finishes the plan. Every watch event is a ledger line `review-watch: PR #<n> ...`, and all but the CI-side stop lines, the per-item addressed and not-resolved lines, the permission-deferral line and the refusal line also send a `slack_ping`; the run report gains a `## Review watch` section with each PR's outcome. In group mode each group PR is watched and the next group starts only after that watch ends: stacking would let the next group start earlier, but a review fix pushed to group N would then need a restack of group N+1. `ci_rounds` stays cumulative per PR across review fix rounds, while the CI poll counters and wall clock are re-armed for each re-watch. The watch is an in-graph poll self-loop because a graph node cannot leave a background child running past its own exit: llm nodes reap their jobs and the pending-agent guardrail cancels spawned agents. `pr_tracking` is graph-only (the prose `architect/config.yaml` has no such variable), so `scripts/check_session_block.sh` and the Rust mirror checks compare the candidate's variables less the declared graph-only set (`pr_tracking`) against the prose twin, and a separate pin holds that `pr_tracking` is declared with default `ci` and is the only graph-only variable |
| flexible intake | `graph.yaml` session `route:` instructions, `initial_state` (`seed_path` and the `draft_*` seeds), the `intake` description and the `confirm_seed`, `draft_brief`, `draft_route`, `draft_fault`, `confirm_draft` and `draft_edit` nodes; `scripts/_lib.py` `DRAFT_ROUNDS_MAX` and `STATE_KEYS`; `scripts/gates.py` `intake` / `_seed_time` / `_find_seed` / `_consume_seed` / `_seed_question` / `_need_design_doc_escalation` / `_start_draft` / `draft_route` / `draft_fault` / `research_dispatch` / `_TERMINAL_PROMPTS` / `parse_answer` / `_typed_doc_path` / `apply_answer`; the new `scripts/draft_route.py` and `draft_fault.py` entry shims; `scripts/audit_graph.py`; `scripts/smoke_test.py`; `tests/candidate_architect_next.rs` | with no design doc named, the run no longer stops on `need_design_doc` first: it offers the engineer's newest ready `vehicle: architect` seed for this project through the hard-trigger `seed_confirm` escalation, and with no seed it drafts a minimal design doc from the message and the earlier session turns and confirms it through the hard-trigger `draft_confirm` escalation (use, edit and redraft up to `DRAFT_ROUNDS_MAX`, or abort); an explicit doc path behaves as before, except that a ready seed named explicitly is consumed like a confirmed one, in `research_dispatch` once orientation succeeded. The Seed pickup and Drafted brief paragraphs below record the contract. The audit reads the terminal prompts from `_TERMINAL_PROMPTS` and fails loud without it; the wrapper pins the new node bounds, seed, `_lib.py` bound and `anomalies` channel, the engineer's seed and handoff contracts (`engineer_architect_block_states_the_seed_contract`, `engineer_standalone_block_states_the_handoff_contract`) and the bare go-ahead route sentence (`route_instructions_send_a_bare_go_ahead_to_run`), and its two sabotage pins turn the seed and draft rows red |
| multiple-choice design questions | `graph.yaml` session `expose:` (`discussion`), the `route:` continue sentence, `carry:` (`discussion_run_id`), the `choice_*`, `reply_*`, `discussion*` and `settled_answer` `initial_state` seeds, the `author_plan` and `answer_leaks` instructions and schemas, the `open_questions_gate` and `leak_route` descriptions and the `frame_questions`, `frame_questions_route`, `ask_choice_2`, `ask_choice_3`, `ask_choice_4`, `classify_reply`, `choice_reply_route`, `choice_park`, `choice_resume`, `choice_reask` and `end_discussion` nodes; `scripts/_lib.py` `CHOICE_OPTIONS_MIN` / `CHOICE_OPTIONS_MAX` / `DISCUSSION_PARKS_MAX`, `CONTINUE_RE`, `STATE_KEYS`, `render_question` and `choice_label`; `scripts/gates.py` `intake` / `resume_route` / `normalize_choices` / `CHOICE_KIND_GATES` / `_start_choices` / `_end_choices` / `_ask_choice` / `_choice_pick` / `_record_choice` / `open_questions_gate` / `frame_questions_route` / `quality_bar_gate` / `_quality_bar_answer` / `checklist_round` / `leak_route` / `_leak_item` / `_leak_amend` / `_autonomy_key` / `escalate_gate` / `ladder_route` / `_TERMINAL_PROMPTS` / `_terminal_prompt` / `ask_owner_poll` / `parse_answer` / `apply_answer` / `choice_reply_route` / `choice_park` / `choice_resume` / `choice_reask`; the new `scripts/frame_questions_route.py`, `choice_reply_route.py`, `choice_park.py`, `choice_resume.py` and `choice_reask.py` entry shims; `scripts/audit_graph.py`; `scripts/smoke_test.py`; `scripts/mode_matrix.py`; the engineer's `instructions.md` and `README.md`; `src/config/request_context.rs`; `tests/candidate_architect_next.rs` | the design escalations `design_round`, `quality_bar`, `checklist_accept` and `leak_question` are asked one question per multiple-choice prompt (2-4 options, exactly one recommended and shown first, plus the engine's `Other (custom)`), with one re-frame for a question that is not askable; typed text is classified as an answer or a discussion, and a discussion parks the run for the engineer chat lane until `continue` or `continue: <answer>`, at most `DISCUSSION_PARKS_MAX` times per question. A settled answer is taken only after the colon of `continue:` or its alias `resume:`; a bare `continue` re-asks, and other continue prose starts a fresh run. Option texts that differ only in case collapse to one, the recommended copy kept; a typed answer to a checklist item reaches `amend_plan` as an owner ruling line, never as an accept; each multiple-choice question asked through `ask_owner` gets its own remote thread, `<run_id>-<kind>-<index>`; and once the re-asks are spent, an empty reply takes the recommended option, with a ledger line. The Design questions paragraph below records the contract. The audit harvests the `CHOICE_KIND_GATES` edges and bounds the new cycles (`open_questions_gate` and `leak_route` re-frame once per round and walk a list-bounded `choice_index`, `choice_park` stops at `DISCUSSION_PARKS_MAX`, `choice_reask` waits on the owner); the smoke rows cover the queue, the re-frame, each kind's walk, park and resume, the classifier, the park cap and the continue re-entry; the wrapper pins the new node bounds, `_lib.py` bounds and `anomalies` channels, the engineer's discussion contract (`engineer_architect_block_states_the_discussion_contract`), the exposed and carried keys (`graph_exposes_the_discussion_and_carries_its_run_id`) and the continue route sentence (`route_instructions_send_a_discussion_continue_to_run`), and its ignored sabotage pin (`choice_smoke_rows_go_red_under_the_recorded_sabotages`) turns the choice rows red |
| free-text escalation replies and branch setup | `graph.yaml` the `reply_verdict`, `reply_option`, `reply_instruction`, `reply_value`, `reply_clarify`, `reply_accepts`, `reply_interpreted`, `run_branch_override`, `base_branch_override` and `code_dir_override` `initial_state` seeds, the `interpret_reply` and `interpret_route` nodes and the `parse_answer` and `choice_reask` descriptions; `scripts/_lib.py` `STATE_KEYS` and `raise_escalation`; `scripts/gates.py` `intake` / `_run_branch` / `WORKTREE_HELD_RE` / `_branch_start` / `_git_setup_escalation` / `_use_code_dir` / `branch_setup` / `parse_answer` / `ACCEPTED_INSTRUCTIONS` / `INSTRUCTION_HELP` / `INSTRUCTION_ANSWERS` / `_accepts_text` / `_apply_instruction` / `interpret_route` / `_discussion_message` / `choice_park` / `choice_resume` / `choice_reask` / `_answered` / `apply_answer`; the new `scripts/interpret_route.py` entry shim; `scripts/audit_graph.py`; `scripts/smoke_test.py`; `tests/candidate_architect_next.rs` | staging re-asked a typed reply that named no option up to three times and then took it raw, so "Use a different branch" on `git_setup_failed` was asked again instead of acted on. Now the first such reply to any escalation that is not multiple choice goes to the `interpret_reply` llm node, which reads it as an option, an instruction the kind accepts (`ACCEPTED_INSTRUCTIONS`), a discussion or unclear: an option or an applied instruction goes to `apply_answer`, a discussion parks the run through `choice_park`, and an unclear reply gets one clarifying question that quotes it; after that, and on an interpreter fault, staging's re-ask applies. Staging's `branch_setup` ran `git checkout <base>` in the code dir, which fails when another worktree holds the base and switches the user's checkout; it now cuts the run branch from the fetched `origin/<base>` (or the local base, see Branch setup below) without checking the base out, pulls an existing run branch only when it has an upstream, and offers fitting options when another worktree holds the run branch. The wrapper's `interpret_smoke_rows_go_red_under_the_recorded_sabotages` tail turns the key rows red |
| sisyphus-next delegate | `graph.yaml` the `delegate` and `wave_delegate` nodes (agent, description, `inputs:`) and the description's islands sentence; `scripts/gates.py` `compose_delegation` / `wave_claim` / `_report_failed` / `_parse_report`; `scripts/marker_map.json` (the micro-mode region's home); `scripts/smoke_test.py`; `tests/candidate_architect_next.rs` | both delegate nodes run `sisyphus-next` instead of the prose `sisyphus` and pass `review_cadence`, `parallel` and `project_dir` through `inputs:` (the E9 row); `wave_claim` gives every wave item a `review_cadence`, and the two `mode-degraded:` ledger lines are gone. `_report_failed` also treats `SISYPHUS_REDIRECT` and `SISYPHUS_AWAITING_DISCUSSION` as a failed delegation: sisyphus-next ends with them before any work lands (a plan-shaped brief, or a question parked for discussion), so they go to the oracle diagnosis instead of the conformance gate. `_parse_report` ends a ledger section at any markdown heading as well as at the next named section, because the sisyphus-next report follows `Design decisions:` with `## Observability` and `FOLLOW-UPS:` with `## Anomalies` and `## Git / PR`. The smoke battery renders both nodes' `inputs:` for sequential, micro, wave and micro-wave state, and runs the real sisyphus-next `final_report` and parses its output: HEAD SHA, run id and every ledger section, filled and empty |

Recorded census against the staging tree (files that differ; hunks per file as counted by
`diff --minimal -u <staging file> <file> | grep -c '^@@'`, with `/dev/null` standing in for a
staging file that does not exist; `--minimal` because the default heuristic of the BSD `diff`
shipped with macOS folds a heavily edited file such as `scripts/smoke_test.py` into one hunk,
so the count would depend on the host's `diff`):

```
graph.yaml               27 hunks   (description provenance; letter-digit labels dropped from the description's cutover sentence; project_dir em dash; pr_strategy em dash + build_lock + task_granularity em dash + the pr_tracking variable, whose description names the hardening fixes and leaves per-task PRs unwatched, + max_loop_iterations 1500 with its CI_MAX_ROUNDS, REVIEW_MAX_POLLS and POLL_VISIT_BUDGET comment; the session report exposes discussion; the bare go-ahead sentence and the discussion continue sentence (a design question or any other escalation) in the route instructions, with resume as the alias of continue and other continue or resume prose starting a fresh run; carry: discussion_run_id; the initial_state.seed_path seed and the draft_* seeds (draft_path, draft_rounds, draft_feedback, draft_text and the drafting node's draft_possible / draft_title / draft_markdown / draft_reason outputs); the choice_kind / choice_queue / choice_index / choice_answers / choice_reframed / choice_parks / choice_forced / choice_invalid / framed_questions seeds and the reply_kind / reply_reason / reply_verdict / reply_option / reply_instruction / reply_value / reply_clarify / reply_accepts / reply_interpreted / discussion / discussion_run_id / discussion_message / settled_answer / run_branch_override / base_branch_override / code_dir_override seeds; the initial_state.skipped_group_indexes: [] seed; the initial_state.fix_origin_return seed; the ci_polls_run / review_polls_run visit counters and the review watch initial_state seeds, review_truncated, review_permissions and review_perm_failed among them; the intake description names the seed pickup and the drafted brief; author_plan's open_questions item shape; the open_questions_gate description and the frame_questions llm node and frame_questions_route; answer_leaks proposes options for each unanswerable question, with that item shape in its output schema, and the leak_route description; the confirm_seed approval node and the draft_brief, draft_route, draft_fault, confirm_draft and draft_edit nodes; batch_gauntlet_build timeout: 60; ci_poll timeout: 180 + the ci_route description names the review watch; ci_evidence timeout: 390; ci_apply timeout: 390; flip_ready timeout: 390 + the review_poll, review_route, review_triage, review_apply and review_update nodes; the ask_choice_2 / ask_choice_3 / ask_choice_4 approval nodes, the classify_reply llm node and the choice_reply_route, choice_park, choice_resume and choice_reask nodes, the choice_reask description naming any question; the interpret_reply llm node and the interpret_route node, with the parse_answer description naming the interpretation; the end_discussion terminal node; the delegate and wave_delegate nodes run sisyphus-next with review_cadence / parallel / project_dir inputs, and the description's islands sentence names sisyphus-next)
scripts/_lib.py           9 hunks   (import time; CHOICE_OPTIONS_MIN, CHOICE_OPTIONS_MAX, DISCUSSION_PARKS_MAX, DRAFT_ROUNDS_MAX, GH_CALL_SECS, GH_LOOP_SECS, the REVIEW_POLL_SLEEP / REVIEW_WALL_SECS / REVIEW_MAX_POLLS / REVIEW_MAX_ROUNDS / REVIEW_UPDATE_BUDGET bounds, PERM_LOOKUPS_PER_POLL, POLL_VISIT_BUDGET and PR_TRACKING_ENUM; RESUME_RE takes the -<n> suffix of a claimed run id; CONTINUE_RE, which takes a settled answer only after a colon; the _now clock a test can move; new cache_dir, runs_root docstring and base path, new_run_id claims its run dir with os.mkdir; seed_path, the draft_path / draft_rounds / draft_feedback / draft_text keys, the design_rounds and choice_* keys and the open question, quality bar, checklist and Gate 1 leak round keys a resumed choice round reads, and the run_branch_override / base_branch_override / code_dir_override keys, in STATE_KEYS; render_question lists a multiple-choice question's options under the static choice_label labels; raise_escalation resets reply_interpreted; core_tool drops inherited argc_* variables)
scripts/smoke_test.py    24 hunks   (import ast, re and types; ARCHITECT_REVIEW_SLEEP=0; LLM_AGENT_VAR_*, GIT_* repository-variable, ARCHITECT_CORE_TOOLS / ARCHITECT_ASK_OWNER_BIN / ARCHITECT_SLACK_PING_BIN / ARCHITECT_GH_*_CMD and GIT_CONFIG_* drop, user.useConfigOnly, no system or user git config, one fixed git identity + import _lib with the CI and review bounds, GH_CALL_SECS, GH_LOOP_SECS and POLL_VISIT_BUDGET; the git helper's own GIT_ENV retired; utils.sh source + the real-copy pin; run dir resolution checks, the battery-env-restored check, the run-id claim and RESUME_RE rows; the engineer seed pickup and drafted brief rows, the seed_confirm unmatched reply interpreted first; the multiple-choice design question rows: the design_round queue, normalize_choices with the case-insensitive duplicate drop, the re-frame and the carried or blocked unaskable question, frame_questions_route, the quality bar and the per-item checklist through the queue, the declining and accepting checklist Other answers as owner ruling lines, the CHOICE_KIND_GATES resume rows, the resume chains of a parked Gate 1 question and a parked checklist item, the Gate 1 questions one per prompt with the string re-frame and the gatekeeper verdict tail as evidence, the carried fallback and the amendment of held and asked answers, the classifier, an "Option 2, but ..." reply recorded in full via other, discussion park, re-ask, park cap, forced reply, the recommended option for an empty reply once the re-asks are spent, the per-question ask_owner thread id, the continue: / resume: answer, bare continue re-ask and continue prose fresh-run rows, the autonomous, ladder and navigator rows, and the graph rows for the new nodes; the empty-group stack rows (branch_setup stacks past a skipped group); the branch setup rows over scratch repos with a worktree holding main (the fetched origin base, a local base ahead kept, a diverged base asked, no remote, an existing run branch with and without an upstream, the run branch held by a worktree with its worktree and branch name answers, the durable overrides); the typed-reply rows (interpret_reply on the first unmatched reply to any kind, an option, an accepted, an unaccepted and an unapplicable instruction, a discussion parked and resumed, one clarifying question for an unclear reply, an interpreter fault re-asked, the park cap and the reset on a new escalation, a revision that mentions approve, and the interpret_reply graph row); gauntlet_reply / verdict_state fixtures in the real table shape, lanes_with_findings, composer settled-lanes, attention, in-repo planning-store, relpath ValueError, literal-pathspec, untracked-file, two-dot store, prior-round-only and dirty-tree rows, GPASS / GPROBE / GBLOCK; the adversary-lane verdict rows, the adversary probe rows (a quoted probe PASS or INCONCLUSIVE starts no probe round), GINC / GDEB; the parser rows: prose-quoted, trailing-text, indented, first match, whole status cells, first table only, USAGE_PROBE last match, probe detail cell, injected header overrides, sentinel only in a lane report, debris scope, forged Blockers headings, a plan-root quote in a lane report leaving a PASS on post_round / conforms and an incomplete-only BLOCKED on its re-run; the fix_done return-target rows with an empty fix_origin_return and the verify, post_round and select_tasks targets; the wave_route header rows; the pr_tracking review watch rows, with the graph.yaml read through mikefarah yq as one FAIL row instead of a crash, the watch hardening, trust-boundary, gh seam and own-marker rows, the cleanup, CI loop-budget, approval, fix-origin, truncation and permission rows, the createdAt comment-overflow, per-task scope, ready-gate, marker-breakout, review-clock and permission-stall rows; the rollup-scope rows, the argc_* scrub row with argc_dir checked as dir=unset, the skipped-member and group-scoped fallback rows, the dirstate completed-dir row, the real collect_ledger_lines rows and the empty-group rows, including the skipped-group record and PR state reset; the sisyphus-next delegate rows: delegate and wave_delegate inputs rendered for sequential, micro, wave and micro-wave state, review_cadence on every wave item and no mode-degraded line, the REDIRECT and AWAITING_DISCUSSION sentinels to oracle_diagnose, and the real sisyphus-next final_report parsed, filled and empty)
scripts/mode_matrix.py   10 hunks   (LLM_AGENT_VAR_*, GIT_* repository-variable, ARCHITECT_CORE_TOOLS / ARCHITECT_ASK_OWNER_BIN / ARCHITECT_SLACK_PING_BIN / ARCHITECT_GH_*_CMD and GIT_CONFIG_* drop, user.useConfigOnly, no system or user git config, one fixed git identity; the git helper's own GIT_ENV retired; the yq guard in load_graph; canned gauntlet reply in the real table shape; the frame_questions stub answers the re-frame node; main imports only CORE_TOOLS_REL; main removes the scratch HOME in a finally around walk_combinations; utils.sh source, fails loud instead of stubbing, confirmation line; the `if True` dropped; the old end-of-walk cleanup and summary moved into main)
scripts/_fixture.py       1 hunk    (the GIT_ENV identity overlay removed; the git helper inherits the battery environment and its one fixed git identity)
scripts/marker_map.json  25 hunks   (62 regions: two new, line numbers shifted, four first-lines reworded upstream; the micro-mode region's home names the delegate's review_cadence input)
scripts/gates.py         69 hunks   (import datetime for the comment createdAt check; import _lib and the PR_TRACKING_ENUM / REVIEW_* / GH_CALL_SECS / GH_LOOP_SECS / PERM_LOOKUPS_PER_POLL / POLL_VISIT_BUDGET / DRAFT_ROUNDS_MAX / TAIL_CHARS import, with the CHOICE_OPTIONS_MIN / CHOICE_OPTIONS_MAX / DISCUSSION_PARKS_MAX / CONTINUE_RE / choice_label / render_question import; GREEN tuple retired; intake's pr_tracking check and mode_line suffix, with the seed pickup and drafted brief helpers _need_design_doc_escalation (with an evidence param), _start_draft and _DRAFT_OUTPUT_RESET, _seed_time (created read with datetime.fromisoformat, a trailing Z as UTC, the mtime when it does not parse), _find_seed (a directory that cannot be listed is skipped; only vehicle: architect seeds, a missing vehicle counting as architect), _consume_seed and _seed_question; intake's continue / resume re-entry on the carried discussion_run_id, a settled answer taken only after continue: or resume:, a bare continue re-asking the parked question, other continue prose starting a fresh run, and the carried key cleared, with the owner's base and code dir overrides re-applied on re-entry; resume_route sends a run parked on a discussion to choice_resume; the mode_invalid option default for pr_tracking and intake's per-task not-watched ledger line and follow-up, with the hard-trigger seed_confirm escalation when no doc path was given and a ready seed exists, and the route to draft_brief when there is no seed but a message or earlier turns; the draft_route and draft_fault gates, draft_route handing off to draft_fault with no run_dir; the multiple-choice queue: normalize_choices (2-4 options, the first recommended kept and moved first, texts that differ only in case collapsed to one with the recommended copy kept and a ledger line), DISCUSSION_PARKED, the CHOICE_KIND_GATES kind to gate table, CHECKLIST_ACCEPT / CHECKLIST_DROP, _start_choices, _end_choices, _ask_choice with an evidence param, _choice_pick (a pick only when nothing but punctuation follows the option) and _record_choice; open_questions_gate asks one design question per prompt, re-frames once through frame_questions and carries or blocks what stays unaskable, with one amendment once the queue is done; frame_questions_route; quality_bar_gate asks the quality bar through the queue, with QUALITY_RIGORS and _quality_bar_answer; checklist_round asks accept or drop per item and passes a typed Other answer to amend_plan as an owner ruling line apart from the accepted items; leak_route asks each unanswerable Gate 1 question through the queue with the gatekeeper verdict tail as evidence, with _leak_item, _leak_amend, LEAK_CARRIED and LEAK_HEADING; research_dispatch consumes a confirmed or explicitly named ready seed once orientation succeeded; branch_setup walks the stack base back past skipped groups and, with _run_branch's run_branch_override, WORKTREE_HELD_RE, _branch_start, _git_setup_escalation and _use_code_dir, cuts the run branch from the fetched origin base (or the local base: no remote, a failed fetch, or unpushed commits on it) without checking the base out, asks when the two diverged, pulls an existing run branch only with an upstream, and names a worktree that holds the run branch with fitting options; _pr_merged reads through the new _pr_view and its view seam; the push-only line in a fix delegation during the review watch; verify_gate: dirty-tree guard + whole-cell GREEN settling; one hunk for _code_tree_dirty, _gauntlet_header, _gauntlet_section, _GAUNTLET_VERDICT_RE / _has_verdict, _lane_rows, _lane_cells, _probe_verdict and the header-only _parse_gauntlet with its adversary-lane flag, the adversary-lane probe guard and single Blockers reader; route_verdict: lane flag, header pre-check, probe verdict behind the adversary-lane probe guard, lanes_with_findings; the probe adopt and INCONCLUSIVE checks, 2 hunks; fix_round seeds fix_origin_return for ci and review rounds only + _lane_table + _pr_comment's own marker, positional args and GH_CALL_SECS; post_round sends a round with a fix origin to fix_done; fix_done returns to fix_origin_return first and clears it; fix_done returns a review fix round to review_update; dead hygiene_pending_for_chain guard in stack_gate removed; wave_route header pre-check; _autonomy_key, so escalate_gate and ladder_route rule each multiple-choice question autonomously once; _TERMINAL_PROMPTS, the escalation-kind to terminal-prompt map _terminal_prompt reads (review_incomplete, seed_confirm, draft_confirm, and choice_2 / choice_3 / choice_4 for a multiple-choice question by its option count); ask_owner_poll's --id carries a multiple-choice question's index, one remote thread per question; parse_answer sends typed text that names no option to classify_reply, sends the first typed reply to any other escalation that names no option to interpret_reply with the kind's accepted instructions, records a forced question's reply, takes a draft_confirm reply that names no option as redraft feedback, never a re-ask, and a seed_confirm reply naming an existing file verbatim, and once the re-asks are spent takes the recommended option for an empty multiple-choice reply, with a ledger line; ACCEPTED_INSTRUCTIONS, INSTRUCTION_HELP, INSTRUCTION_ANSWERS, _accepts_text, _apply_instruction and the interpret_route gate (an option or an applied instruction to apply_answer, a discussion to choice_park, one clarifying question for an unclear reply, parse_answer's re-ask on a fault); _typed_doc_path; _answered prefers a prefix match over a substring; the _leave_in_draft helper; apply_answer records a multiple-choice answer through the queue and returns to the kind's gate, its free-text leak_question branch removed; apply_answer answers draft_confirm before the generic abort check (use the draft, use a typed path, abort, or redraft with the output reset, and need_design_doc past DRAFT_ROUNDS_MAX or with the draft file gone), parses pr_tracking from a mode_invalid answer and answers seed_confirm (abort, use the seed, use a typed path, draft a brief instead, or fall back to need_design_doc); apply_answer answers git_setup_failed's worktree and branch name options and the branch_name escalation; four apply_answer abort and fail paths clear fix_origin_return, 4 hunks; apply_answer ends the review watch when a watched PR is left red and answers the review_exhausted / review_read_truncated / review_round_cap / review_owner_judgment / watch_visit_cap / review_push_mismatch / review_external_close escalations, refusing a fix round for untrusted items; the choice_reply_route, choice_park (with _discussion_message, which names resume as the alias of continue; any escalation, keyed by its kind outside multiple choice), choice_resume (escalate_gate for a kind CHOICE_KIND_GATES lacks; parse_answer or escalate_gate for an escalation that is not multiple choice) and choice_reask gates; hygiene_create skips an empty group's chore; hygiene_create collect_ledger_lines --plan / --tasks over the completed tasks dir; _completed_tasks_dir, _group_members and _skip_empty_group, which records the skipped group index and resets the per-group PR state; pr_body: empty-group skip, --tasks, the completed tasks dir and the group-scoped fallback list; the gh command seams comment in open_pr, naming all eleven seams, and the create call on GH_CALL_SECS; ci_poll's run-wide visit budget; ci_poll's checks read on GH_CALL_SECS; ci_route counts a killed poll or stale status as a read failure, goes back to review_poll on green while the watch runs, and raises watch_visit_cap and the watch's ci_exhausted; ci_evidence's log reads on GH_CALL_SECS, skipped past GH_LOOP_SECS; the ci_evidence skipped-log ledger line; ci_apply's start clock; ci_apply's stop-watching option during the watch; the rerun seam, one re-run per run id, skipped past GH_LOOP_SECS; ci_exhausted during the watch; ci_infra and ci_unsure during the watch, with flip_ready's draft-flag loop bound, arming the watch only after gh pr ready succeeds (the still-draft line otherwise) and never for per-task PRs, reading draft flags through _pr_view, with _after_flip; the review_poll / review_route / review_apply / review_update gates and their helpers (_per_task_prs, permission trust with the review_perm_failed list, the APPROVED reviewDecision before any permission read, the latest-verdict _approved, the truncated-read check with the createdAt rule for issue comments and the stop-only comments overflow, the data-marker tag escape in _defang, the batched untrusted alert and update ping, the marked whetstone feedback, review_update on the _lib._now clock); _collect --plan over the completed tasks dir; final_report's Review watch section; group_closeout skips an empty group; the draft_route / draft_fault, frame_questions_route, choice_*, interpret_route and review_* GATES entries; compose_delegation and wave_claim drop the mode-degraded lines and every wave item carries review_cadence; _report_failed counts SISYPHUS_REDIRECT and SISYPHUS_AWAITING_DISCUSSION; _parse_report ends a section at any markdown heading)
scripts/audit_graph.py    5 hunks   (the docstring names the CHOICE_KIND_GATES harvest; the open_questions_gate bound label names the re-frame and choice_index bounds; the interpret_route, leak_route, choice_reask and choice_park bound labels; the apply_answer bound label names the draft rounds bound and DRAFT_ROUNDS_MAX; the review_poll and review_apply bound labels; the _TERMINAL_PROMPTS and CHOICE_KIND_GATES dicts read from the gates AST through one dict_values helper, failing loud when either is missing; the _terminal_prompt edges taken from the first, and every gate of the second an edge of each function that reads it)
scripts/check_session_block.sh  1 hunk    (the variable mirror leaves out the graph-only pr_tracking)
scripts/draft_fault.py    1 hunk    (new entry shim for the draft_fault node)
scripts/draft_route.py    1 hunk    (new entry shim for the draft_route node)
scripts/frame_questions_route.py  1 hunk    (new entry shim for the frame_questions_route node)
scripts/choice_reply_route.py  1 hunk    (new entry shim for the choice_reply_route node)
scripts/choice_park.py    1 hunk    (new entry shim for the choice_park node)
scripts/choice_resume.py  1 hunk    (new entry shim for the choice_resume node)
scripts/choice_reask.py   1 hunk    (new entry shim for the choice_reask node)
scripts/interpret_route.py  1 hunk    (new entry shim for the interpret_route node)
scripts/review_poll.py    1 hunk    (new entry shim for the review_poll node)
scripts/review_route.py   1 hunk    (new entry shim for the review_route node)
scripts/review_apply.py   1 hunk    (new entry shim for the review_apply node)
scripts/review_update.py  1 hunk    (new entry shim for the review_update node)
README.md                 2 hunks   (design-record aside in the status paragraph; run-state bullet, the variables-mirror sentence, this section with the multiple-choice design questions and free-text escalation replies rows, the Design questions, Typed replies and Branch setup paragraphs and the Verify section)
```

Every other file is byte-identical to staging. The graph adds the five review watch nodes
(`review_poll` with `timeout: 900`, `review_triage` with 600, `review_apply` with 120,
`review_update` with 1020, and `review_route` with 120), the `draft_brief` llm node
(`max_iterations: 10`, `timeout: 600`), and the multiple-choice nodes: the `ask_choice_2`,
`ask_choice_3` and `ask_choice_4` approval nodes, the `classify_reply` llm node
(`timeout: 120`), the `frame_questions` llm node (`max_iterations: 10`, `timeout: 600`),
the `frame_questions_route`, `choice_reply_route`, `choice_park`, `choice_resume` and
`choice_reask` script nodes and the `end_discussion` end node, and the `interpret_reply` llm
node (`timeout: 120`) with its `interpret_route` script node; `scripts/_lib.py` adds the five review
bounds (`REVIEW_POLL_SLEEP` 600, `REVIEW_WALL_SECS` 86400, `REVIEW_MAX_POLLS` 144,
`REVIEW_MAX_ROUNDS` 3, `REVIEW_UPDATE_BUDGET` 600), `GH_CALL_SECS` 90, `GH_LOOP_SECS` 180,
`PERM_LOOKUPS_PER_POLL` 2, `POLL_VISIT_BUDGET` 1200, `DRAFT_ROUNDS_MAX` (`STRIKE_LIMIT`, 3),
`CHOICE_OPTIONS_MIN` 2, `CHOICE_OPTIONS_MAX` 4 and `DISCUSSION_PARKS_MAX` 3, as the
`pr_tracking` review watch, flexible intake, multiple-choice design questions and frozen
budgets rows record. Every staging node's prompt is unchanged except the `author_plan` and
`answer_leaks` instructions, which now ask for each open question as 2-4 options with
exactly one recommended, and so is every staging loop bound and node
timeout in `graph.yaml`, `scripts/_lib.py` and `scripts/gates.py` except six in
`graph.yaml`: `settings.max_loop_iterations` is 1500 (staging 100), `batch_gauntlet_build`
carries `timeout: 60` (staging none), `ci_poll` carries 180 (staging 120), and `ci_apply`
(staging 120), `ci_evidence` (staging 300) and `flip_ready` (staging 300) each carry 390, as
the frozen budgets row records. The gates' per-call `gh` and seam timeouts, between 60 and
180 seconds in staging, are all `GH_CALL_SECS` now, as the gh command seams row records.

Toolchain census (the gates must stay project-agnostic; verification commands come
from the brief, never from a hard-coded toolchain):

```
$ grep -nE '\b(cargo|npm|pnpm|yarn|go (test|build)|pytest|mvn|gradle|make)\b' scripts/*.py
scripts/_lib.py:602:    r"`[^`]+`|\b(go|cargo|npm|pnpm|yarn|make|pytest|python3?|bash|sh|grep|rg|curl|task|just|mvn|gradle|dotnet|bun)\b\s",
scripts/gates.py:46:    r"yarn\.lock$|pnpm-lock\.yaml$|Cargo\.(toml|lock)$|requirements.*\.txt$|poetry\.lock$",
scripts/gates.py:3558:        "file:line, at most 25 per run so iterate); identifiers that only make sense with the plan open; ...
scripts/gates.py:3567:        "failing test to make it pass; you still do NOT edit the plan or task files -- report instead.\n\n"
scripts/mode_matrix.py:176:`make run`
scripts/mode_matrix.py:273:            ... "local_run_recipe": "`make run`",
scripts/smoke_test.py:114:CODE_BASE = make_repo(CODE, {..., "CLAUDE.md": "Run `go test ./...` before committing.\n", ...
scripts/smoke_test.py:173:`make run`
scripts/smoke_test.py:1067:u = gates.verify_gate(base_state(..., local_run_recipe="make run"))
scripts/smoke_test.py:1441:   ... "acceptance_criteria": ["`go test ./...`"], ...
scripts/smoke_test.py:2007:u = AA("probe_gap", "Supply the recipe: make run", ...)
```

None of these runs a toolchain: `_lib.py:602` is the alternation of the executable-criterion
classifier (`EXECUTABLE_RE` recognises a command-shaped acceptance criterion; it hardcodes no
command), `gates.py:46` is the dependency-manifest regex (the hits are `yarn`/`pnpm` inside
lockfile names), `gates.py:3558` and `:3567` are the English verb `make` in prompt prose, and
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
python3 scripts/smoke_test.py           # every gate, both sides of every bound, fail-open paths (needs mikefarah yq)
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
  keeping its own git identity overlay; the sabotage pin
  (`smoke_rows_go_red_under_the_recorded_sabotages`: each recorded guard in `gates.py`,
  removed from a scratch copy, turns its smoke row red: read permission trusted, the
  CHANGES_REQUESTED veto, the `fix_origin_return` target, the overflow stop, the visit budget
  `>=`, the PR head check, the trusted author of an own comment, the failed-read exclusion,
  the ready-seed filter, the seed `status: consumed` write and the hard trigger on
  `draft_confirm`), and its ignored intake tail
  (`intake_smoke_rows_go_red_under_the_recorded_sabotages`, run with `-- --ignored`: the
  ready re-check before a seed is consumed, the consumption in `research_dispatch`, the hard
  trigger on `seed_confirm`, the `confirm_seed` and `confirm_draft` terminal prompts,
  explicit-path precedence over a seed, the project filter, the blank-input stop, the
  `draft_possible: false` and empty-draft fallbacks, `draft_fault`, the `DRAFT_ROUNDS_MAX`
  check, `_DRAFT_OUTPUT_RESET`, the `draft_confirm` answer read before any abort check, both
  explicit Abort branches, and the confirmed draft becoming the design doc), and its ignored
  choice tail (`choice_smoke_rows_go_red_under_the_recorded_sabotages`, run with
  `-- --ignored`: only the first recommended option kept and moved first, an unmatched typed
  reply sent to the classifier, a discussion verdict parking the run, `resume_route` sending
  a discussion park to `choice_resume`, the settled answer recorded, the
  `DISCUSSION_PARKS_MAX` `>=`, the carried `discussion_run_id` cleared at intake, the
  Gate 1 carried fallback, the scope flag kept by `frame_questions_route`, the
  multiple-choice branch of `_terminal_prompt`, a forced reply skipping the classifier, the
  duplicate-option drop in `normalize_choices`, a settled answer read only after
  `continue:` or `resume:`, the held Gate 1 answers and finished checklist labels restored
  on resume, "Option 2, but ..." falling through to the classifier, and a checklist Other
  answer passed as an owner ruling);
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
  baseline (the staging values plus the frozen budgets row, the review watch nodes, seeds
  and bounds, and the `draft_brief` node, the `draft_rounds` seed and `DRAFT_ROUNDS_MAX`),
  the loop cap at four times or more both the `CI_MAX_ROUNDS` and the
  `REVIEW_MAX_POLLS` that `_lib.py` computes, with `REVIEW_MAX_POLLS` and `REVIEW_MAX_ROUNDS`
  below it, the poll node timeouts against their sleep plus one `GH_CALL_SECS` and
  `review_update` against `REVIEW_UPDATE_BUDGET` plus four `GH_CALL_SECS`, with `review_poll`
  also covering `PERM_LOOKUPS_PER_POLL` permission reads and `ci_apply`, `ci_evidence` and
  `flip_ready` against `GH_LOOP_SECS` plus two `GH_CALL_SECS` plus 30
  (`poll_node_timeouts_cover_the_sleep_and_one_gh_call`), `POLL_VISIT_BUDGET` below the loop
  cap, at least four CI watches plus a quarter of one, and at least one full watch of each poll
  (`poll_visit_budget_stops_the_watch_before_the_engine_cap`), the `anomalies` property on
  every llm node (`review_triage` and `draft_brief` included), and no
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
  `pr_tracking` row naming every stop condition `apply_answer` answers
  (`readme_pr_tracking_row_names_every_stop_condition`) and each hardening fix with the number
  the shipped files carry (`readme_pr_tracking_row_lists_each_hardening_fix_with_its_number`),
  the battery figures against the wrapper floors, and the recorded staging census against a live
  `diff --minimal -u` when `ARCHITECT_NEXT_STAGING_DIR` names the staging tree (a file staging lacks is
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

Seed pickup: when neither the `design_doc` variable nor a `.md` path in the message names a
design doc, `intake` looks for the newest `status: ready` seed the engineer wrote for this
project under `plans/engineer-handoffs/` (the planning store's and the code repo's, by
`created:` read as ISO-8601 with a trailing `Z` as UTC, then mtime; a `created:` that does not
parse counts as the mtime, and a directory that cannot be listed is skipped) and asks before
using it. Only `vehicle: architect` seeds are picked; a seed with no `vehicle:` key counts as
an architect seed, since every seed written before the key existed was one, and a
`vehicle: sisyphus` handoff is left alone. The question is a hard-trigger
`seed_confirm` escalation, so it goes through `escalate_gate` and `ask_dispatch` (parked
snapshot, owner-away Slack path) to `confirm_seed` even under `escalation_policy=autonomous`.
"Use this seed" makes it the design doc; the seed is marked `status: consumed` (with
`consumed_by` and `consumed_at`) only in `research_dispatch`, the step after a successful
orientation, so a run abandoned before then leaves it ready, and the `status: ready` check is
repeated just before the write. The same consumption applies to a ready seed named as an
explicit path, so no later go-ahead picks it again. "Use this seed" when the file is gone falls
back to `need_design_doc`; a reply that names an existing file is used as the design doc and
leaves the seed ready; "Draft a brief from this conversation instead" leaves it ready and
drafts a brief as below; any other answer leaves it ready and falls back to `need_design_doc`.
An explicit path never looks for a seed or drafts one; an explicit path that is not readable
stops on `need_design_doc` as before.

Drafted brief: with no design doc named and no ready seed, `intake` routes to the
`draft_brief` llm node as long as the message or the earlier turns are non-blank (both blank
stops on `need_design_doc`). The node drafts a minimal design doc from `{{brief}}` (the
message) and `{{previous_turns}}`, the transcript of the session the run was started in,
which the engine seeds into `initial_state` (`seed_initial_state` and `previous_turns_for`
in `src/graph/dispatch.rs`); the engineer chat lane's turns live in that same session, so a
design discussion with the engineer reaches the draft. `draft_route` writes the draft to
`design-draft.md` in the run dir and raises a hard-trigger `draft_confirm` escalation, which
reaches `confirm_draft` the same way `seed_confirm` reaches `confirm_seed`. "Use this draft"
makes the file the design doc and goes on to `orient_read`; "Abort the run" ends the run;
"Edit the draft" asks what to change through the `draft_edit` input node and redrafts. A reply
whose first token is an existing file is used as the design doc instead of the draft. Any other
reply that names no option, from a parent answering the prompt or from the owner on Slack, is
taken verbatim as the requested changes and redrafts without a re-ask. `apply_answer` reads the
`draft_confirm` answer before its generic abort check, so an edit such as "do not abort, add
a --dry-run flag" redrafts instead of aborting. Redrafts are bounded by `DRAFT_ROUNDS_MAX`
(`STRIKE_LIMIT`, 3): one more edit past it falls back to `need_design_doc`, as do a
`draft_possible: false` or empty draft (the reason goes into the evidence), a failed drafting
node (`draft_fault`, which `draft_route` also hands off to when the run has no `run_dir` to
write the draft into) and "Use this draft" when the file is gone. Every draft, first or
redraft, starts with the drafting node's output keys (`draft_possible`, `draft_title`,
`draft_markdown`, `draft_reason`) cleared, so a reply that omits one cannot replay the last
draft. The session block's `route:` instructions send a bare go-ahead ("okay, go ahead and
get started") after a conversation that discussed work to RUN, where the seed pickup or the
drafted brief takes over. `sisyphus-next` has its own go-ahead intake, which picks up only
`vehicle: sisyphus` handoffs (see its README).

Design questions: the design escalations `design_round` (the open questions of a design
frontier round), `quality_bar` (rigor and surfaces), `checklist_accept` (accept or drop each
best-practice checklist item) and `leak_question` (each Gate 1 question the code and docs
cannot answer) are asked one question per multiple-choice prompt. The prompt body lists each
option's text and evidence under a static label, `Option 1 (recommended)` through `Option 4`,
the recommended option always first, and the engine adds its own `Other (custom)` choice. A
question without 2-4 options and exactly one recommended gets one re-frame through
`frame_questions`; a scope-changing question that still is not askable ends the run blocked,
and any other one is carried (a design question as a plan Open question, a Gate 1 question as
an open `Gate 1 leak:` amendment line for the next gatekeeper pass). A reply that names an
option is recorded as that option; typed text goes to the `classify_reply` llm node, which
decides whether it answers the question (recorded as given) or opens a discussion (unclear
counts as a discussion). A typed answer to a checklist item is never taken as an accept: it
reaches `amend_plan` as its own `Owner ruling on <item>: <answer>; apply only if it accepts
the item` line, apart from the accepted items, so the round goes to `amend_plan` unless every
item was dropped. A discussion parks the run: `choice_park` keeps the queue and the
answers so far, snapshots state.json, sets the exposed `discussion` state key (the question,
its options with the recommended one marked, the reply, the answers so far and the plan path)
for the engineer chat lane, carries `discussion_run_id` into the next run, and ends at
`end_discussion` with an `ARCHITECT_AWAITING_DISCUSSION` message. The next message
`continue` asks the parked question again (trailing punctuation allowed), and
`continue: <answer>` records that answer and goes on to the remaining questions; `resume` is
an alias of `continue`. Any other message, including prose such as `continue please`, starts
a fresh run and leaves the parked one. A question parks at most `DISCUSSION_PARKS_MAX` (3) times; after that it is asked
again with a note, and its next typed reply is recorded as the answer without the
classifier. Only three take raw text, because their answer is not a pick from a list:
`need_design_doc` (a typed path to an existing design doc), `branch_name` (the run branch
name) and the `draft_edit` input node (the changes to make to a drafted brief). The
multiple-choice kinds above and `draft_confirm` (whose unmatched reply is redraft feedback)
are excluded from interpretation by design. Every other escalation, `clarify` and
`gate1_stuck` included, takes typed replies as the next paragraph describes.

Typed replies to other escalations: a reply to an escalation that is not multiple choice is
matched to an option by number or text first. The first reply that names no option goes to
the `interpret_reply` llm node, given the question, the reply and the instructions that
escalation kind accepts (`ACCEPTED_INSTRUCTIONS` in `gates.py`; `git_setup_failed`, for
example, accepts a different base branch, a different run branch name, or reusing an
existing worktree as the code dir). `interpret_route` then acts on its verdict:

- `option`: the option is recorded and applied, but only when the interpreter names it by its
  exact text (case-insensitive) or its 1-based number; anything looser is unclear;
- `instruction`: applied only when the kind accepts that shape and it can be carried out (a
  valid branch name, an existing file, a worktree of the same repository: its `git
  rev-parse --git-common-dir` matches the code repo's), then answered with the option it
  implies. Otherwise it is ledgered and treated as unclear. The free-form shapes
  (`revision`, `answers`, `rescope`, `recipe`) carry the owner's reply verbatim; only the
  structured shapes (`base_branch`, `branch_name`, `reuse_worktree`, `path`, `mode_values`)
  take the value the interpreter extracted;
- `discussion`: the run parks through the same `choice_park` as a design question, keyed by
  the escalation kind. `continue` asks the escalation again, and `continue: <answer>` takes
  the answer as a fresh reply;
- `unclear`: ONE clarifying question that quotes the reply, sent through `ask_dispatch` so a
  remote owner sees it too.

Each reading and the action taken are ledgered. The interpretation happens once per reply
(`reply_interpreted`, reset by every new escalation and every resume). After the
clarifying question, after an interpreter fault, and at the park cap, an unmatched reply
gets up to `STRIKE_LIMIT` (3) re-asks and then stands as typed. A typed existing path on
`need_design_doc` or `seed_confirm` is taken as given, never interpreted. Stop and abort
are read from the whole reply or the option it names, never from a word inside prose, so
a `gate1_stuck` answer such as "the worker should stop retrying" stays an answer and
`fix/abort-handling` is a valid `branch_name`.

Branch setup: `branch_setup` never checks out the base branch, so a base held by another
worktree stays where it is. A new run branch is cut with `git checkout --no-track -b <branch>
<start>`, then pushed with `-u` when there is a remote. `<start>` is chosen as follows:

- `origin/<base>` after `git fetch origin <base>`;
- the local `<base>` when there is no remote, the fetch fails, or the local base already
  contains `origin/<base>` (its unpushed commits are kept);
- when the two have diverged, nothing is cut and `git_setup_failed` asks.

An existing run branch is checked out and pulled with `--ff-only` only when it has an
upstream. When another worktree holds the run branch, `git_setup_failed` names that
worktree and offers "Use that worktree as the code dir", "Pick a different branch name"
(asked through the free-text `branch_name` escalation) and "Abort the run". The owner's
branch, base and code dir overrides are durable state, so a resumed run keeps them (a code
dir that no longer exists falls back to the live variable). The worktree option takes the
named path only when it is a checkout of the same repository. In PR-group mode a typed
branch name loses any `-g<n>` suffix, since the group suffix is added per group, and the
override applies from the group it was given in onwards; earlier groups keep their names.

Four tests are ignored and run with `-- --ignored`: the mode-matrix run and the intake,
choice and interpretation sabotage tails (`intake_smoke_rows_go_red_under_the_recorded_sabotages`,
`choice_smoke_rows_go_red_under_the_recorded_sabotages`,
`interpret_smoke_rows_go_red_under_the_recorded_sabotages`), which keep the default sabotage
pin near two minutes.

The Rust wrapper runs the batteries with `HOME`, `LLM_ROOT_DIR`, `COYOTE_CACHE_DIR`,
`TMPDIR` and `ARCHITECT_RUNS_ROOT` pointed at a scratch directory and
`PYTHONDONTWRITEBYTECODE=1`, and drops any inherited `GRAPH_STATE*` / `LLM_AGENT_VAR_*`
variables, the `ARCHITECT_CORE_TOOLS` / `ARCHITECT_GH_*_CMD` tool overrides and the git
repository variables,
so nothing under this directory or `~/.config/coyote` is touched and no agent session leaks
in. `audit_graph.py`, `check_session_block.sh`, `mode_matrix.py` and `smoke_test.py` need
the Go `yq` by mikefarah (`yq --version` names it). Without it `smoke_test.py` does not
crash: it records one `graph.yaml: read through mikefarah yq` FAIL row and fails the
remaining graph rows against an empty graph, so the battery exits 1. The wrapper's audit,
session-check, poison and smoke tests (the pass, tally and sabotage tests; the env-restore
test expects a failing battery and runs either way), and the ignored matrix, skip when a
different `yq` (or none) is on `PATH`, and fail instead when `ARCHITECT_NEXT_REQUIRE_YQ` is
set to a non-empty value. The sweep that
keeps every bundled agent from naming this candidate lives in
`tests/candidate_sisyphus_next.rs` and covers both candidate names.

Measured 2026-11-02 after the adjustments above, from a fresh scratch dir: audit
`PROBLEMS: none`; smoke `827/827 checks passed` (audit and smoke re-measured 2026-11-06 after
the sisyphus-next delegate swap); mode matrix `53/53 checks passed` (`62 live regions == 62 mapped`) in
3m21s wall clock; session check last line
`ok node ids / script paths consistent` (mikefarah yq v4.53.3); no unset-variable failure
from `architect/tools.sh` under the real `utils.sh`.
