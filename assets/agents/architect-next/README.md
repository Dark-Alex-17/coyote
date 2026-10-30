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

Variables mirror `architect` (names and defaults identical). The session block routes
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
| gauntlet grammar | `scripts/gates.py` `verify_gate` / `_code_tree_dirty` / `_gauntlet_header` / `_gauntlet_section` / `_has_verdict` / `_lane_rows` / `_lane_cells` / `_probe_verdict` / `_parse_gauntlet` / `route_verdict` / `wave_route` / `_lane_table`, `scripts/smoke_test.py`, `scripts/mode_matrix.py` | the gauntlet's reply opens with `GAUNTLET: <verdict>` on line 1 and appends every lane's raw report under `## Full lane reports` inside `<details>` blocks, so `GAUNTLET:`, `GAUNTLET_REVIEW_INCOMPLETE:`, the lane table and the `## Blockers` section are read only from the header before the first column-0 `## Full lane reports` line (or before the first `<details>` when that heading is absent), as column-0 whole lines with `[ \t]*` gaps, first match winning: a prose-quoted, indented or trailing-text sentinel is ignored, so a missing whole-line verdict is BLOCKED, and a `GAUNTLET: PASS` or a GREEN row injected below the header cannot override it; `route_verdict` and `wave_route` pre-check the same header slice with the parser's own regex (`_has_verdict`). The lane table is read as `\| lane \| status \| detail \|` from the first contiguous run of column-0 `\|` lines, first row per lane, with the whole status cell kept (`GREEN`, `GREEN (attention)`, `BLOCKED`, `SKIPPED`); staging's `GREEN` tuple of detail words is gone. `USAGE_PROBE:` stays a whole-line last match; the probe verdict is the probe row's detail cell (`PASS`, `FAIL`, `INCONCLUSIVE (environment)`), and the last `USAGE_PROBE:` line of the header counts only when there is no probe row. `ADVERSARIAL_REVIEW:` stays unanchored for the micro-mode adversary lane, and on that lane it is the only verdict: `route_verdict` tells the parser which lane it reads, the `GAUNTLET:` regex is not consulted there (a column-0 `GAUNTLET: PASS` in the adversary's verification output cannot pass a DIVERGES), and the adversary's blockers are the bullets under a `Blockers`, `Divergences` or `Complaints` heading plus the numbered items under its `Complaints:` line, else the DIVERGES fallback. The adversary report has no lane table, so it settles no lanes: a lane row it quotes is not a lane verdict and is not recorded. Settled lanes are the lanes whose whole status cell is exactly `GREEN`, minus the lanes with findings last round and never `debris`; `GREEN (attention)` never settles (a human must look, so the lane re-runs); `Settled lanes:` is written only when non-empty and `Re-review delta: <sha>..HEAD` stays bare. Only the immediately-prior round settles a lane, as the review gauntlet's own settled-lanes contract reads it, so a lane GREEN two rounds ago but SKIPPED last round runs again; this differs from the sisyphus-next twin, which reads lane state across rounds. Neither line is written while the code repo has uncommitted changes outside the planning store: when `plans_root` resolves inside the code repo its paths are excluded, as a literal pathspec, from that `git status --porcelain --untracked-files=normal` (untracked files count whatever the git config says), because the in-repo planning store carries uncommitted task-status edits until the task closes. This diverges from the sisyphus-next twin, which has no planning store and checks the whole tree. The header boundary is the review-gauntlet producer's guarantee (its verdict gate folds every header value onto one line, so nothing it writes above the lane reports can start a column-0 `## Full lane reports` or `<details>` line), and the consumer defends it as well. The verdict, the incomplete line, the lane table, the `## Blockers` section and the debris blocker lines (unbulleted `debris: Implementation debris` lines count only at column 0 inside that section) are read only from the header. There is one Blockers reader, as in the twin: the slice below the first `## Lane selection` heading after the lane table, from the last exact `## Blockers` heading to the next column-0 `## ` heading, supplies both the bulleted blockers and the unbulleted debris lines, so a look-alike `## Blockers` inside the lane-selection reasons never stands in for the real one. Four reads take the full reply on purpose: `ADVERSARIAL_REVIEW:` (the direct micro-mode adversary lane, whose report has no gauntlet header; its complaints and its own debris lines are read from the whole report too, so a `<details>` block above its `Complaints:` list cannot hide them), `PIPELINE-FAULT` (consulted only when the header carries no verdict, so the round is already BLOCKED), the plan-root phrases (the adversary's reasoning lives only in its lane report; consulted only on a round that already has real blockers, where they pick the plan-defect escalation over a fix round) and the probe's test-file paths (read only when the probe verdict is PASS, adoption capped at one round). When the reply carries a gauntlet header verdict, none of them can turn a BLOCKED into a PASS, or a PASS into anything but that one capped adopt round; the direct adversary lane has no header, so its `ADVERSARIAL_REVIEW:` line is the only verdict there |
| E9: variables reach config children too | `graph.yaml` `delegate` / `wave_delegate`, `scripts/gates.py` `_delegation_text` / `compose_delegation` / `wave_claim` | staging's "agent nodes have no variables channel for config children" holds only for a variable the child does not declare: the engine copies the parent's agent variables to every agent-node child, config and graph alike, and the child seats them over its own declared defaults. Of the variables both this graph and `sisyphus` declare (`project_dir`, `auto_confirm`, `escalation_policy`, `build_lock`), only `build_lock` is a value the delegate should inherit (the `build_lock` variable row). `review_cadence` and `parallel` are `sisyphus` variables this graph does not declare (the cadence is derived from micro mode), and `project_dir` is deliberately overridden with the task worktree, so those three stay in the `## MODE` block with the `mode-degraded:` ledger line. Every other child - the graph agent librarian and the config agents explore, gatekeeper, oracle, engineer and whetstone - receives `build_lock` by the same inheritance; a child that declares no `build_lock` does not consume it (no file of theirs names `build_lock`, `gate_prefix` or `run_gate`). Not a schema change: no node, prompt, bound or timeout moved |
| V1: session-resume replacement (artifact re-entry) - the contract | this README | fix rounds hand the `sisyphus` delegate the task's branch, commits and prior report verbatim, plus the `Sisyphus run id` that report carries, and later re-entries resume this graph's run by `Architect run id`; nothing is ever resumed by a child session id |
| engine facts | this README | agent-node resume: not available by decision (2026-10-28); E1 stays as the artifact contract; G6 escalation_timeout open |
| battery env hygiene | `scripts/smoke_test.py`, `scripts/mode_matrix.py`, `tests/candidate_architect_next.rs` | both batteries drop every `LLM_AGENT_VAR_*` variable at start so they cannot inherit the agent variables of the process running them (`_lib.var` reads the mode variables through them), drop `ARCHITECT_CORE_TOOLS`, `ARCHITECT_ASK_OWNER_BIN`, `ARCHITECT_SLACK_PING_BIN` and every `ARCHITECT_GH_*_CMD` so a caller's tool overrides cannot replace the gates' own `tools.sh`, `ask_owner`, `slack_ping` and `gh` calls, and drop `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY` and `GIT_COMMON_DIR` the same way, so a run from a git hook (which exports them) commits in the scratch repositories, not the caller's; the smoke battery's recording `tools.sh` restores any prior `ARCHITECT_CORE_TOOLS` when its call returns. Both also drop `GIT_CONFIG_PARAMETERS` and every inherited `GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_*` and `GIT_CONFIG_VALUE_*`, so a caller's command-line git config cannot reach the scratch repositories, then set `GIT_CONFIG_COUNT=1`, `GIT_CONFIG_KEY_0=user.useConfigOnly` and `GIT_CONFIG_VALUE_0=true`. After those drops both set a fixed `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME` and `GIT_COMMITTER_EMAIL`, which every gate and each battery's own git helper inherits: the scratch `HOME` holds no git identity, and git refuses to guess one when `user.useConfigOnly` is set or when the host name has no dot (it cannot build an email), so without the fixed identity every gate commit would fail on such a host; setting `user.useConfigOnly` makes every host behave that way. The Rust wrapper strips the repository variables from every git, script and binary command it starts and the agent variables and tool overrides from every script and binary command; it builds its scratch repository and runs the review-gauntlet scripts with `GIT_CONFIG_NOSYSTEM=1` and `GIT_CONFIG_GLOBAL=/dev/null`, so no system or user git config (signing, hooks) applies, and runs the batteries with the same `GIT_CONFIG_*` drop and `user.useConfigOnly` setting. Every binary command it starts drops every inherited `COYOTE_*` variable (`COYOTE_PROVIDER`, `COYOTE_PLATFORM`, `COYOTE_WORKSPACE_CONFIG_DIR`, `COYOTE_CONFIG_FILE` and the rest) before it sets its own `COYOTE_CONFIG_DIR` and `COYOTE_CACHE_DIR`. Every scratch config dir it starts the binary from holds an `mcp.json` that defines no server: the bundled one launches `engineer`'s `ddg-search` through `uvx`, which CI hosts lack, and the engine never installs over an existing file |
| ledger rollup scope | `scripts/gates.py` `hygiene_create` / `_group_members` / `_skip_empty_group` / `group_closeout` / `pr_body` / `_collect` / `_completed_tasks_dir`, `scripts/smoke_test.py` | every `collect_ledger_lines` call passes `--plan`: the prose `architect/tools.sh` declares it required and hands it to `plan_tasks.py`, which exits `plan not found` without it, so staging's calls always fell back to `(collect_ledger_lines unavailable: ...)`. In multi-PR mode the group-scoped calls (`hygiene_create`'s starting ledger and `pr_body`'s `assemble_pr_body`) also pass `--tasks <group members>`, as the prose architect does, where the members are the closing group's complete tasks (a skipped, failed or parked member is not in the completed task store, so naming it would only cost a body fix round); the run report (`_collect`) passes no `--tasks` and stays run-scoped, because it covers every PR group. A group with no complete task makes no group-scoped call, since an empty `--tasks` would widen it to the whole plan: `group_closeout` skips that group's PR with a ledger line and a follow-up naming the group, then moves to the next group (or finishes the plan at run scope); `pr_body` skips an empty group the same way and `hygiene_create` skips its hygiene chore. Under the dirstate task store all three read the `completed` task dir, where complete tasks live. The smoke battery records each call's arguments and runs the real `tools.sh` over a scratch plan and task log |

Recorded census against the staging tree (files that differ; hunks per file as counted by
`diff -u <staging file> <file> | grep -c '^@@'`):

```
graph.yaml                3 hunks   (description provenance; project_dir em dash; pr_strategy em dash + build_lock + task_granularity em dash)
scripts/_lib.py           1 hunk    (new cache_dir; runs_root docstring; runs_root base path)
scripts/smoke_test.py     8 hunks   (LLM_AGENT_VAR_*, GIT_* repository-variable and ARCHITECT_CORE_TOOLS / ARCHITECT_GH_*_CMD scrub, fixed git identity + import _lib; utils.sh source + the real-copy pin; run dir resolution checks; gauntlet_reply / verdict_state fixtures in the real table shape, composer settled-lanes, attention, in-repo planning-store, relpath ValueError, literal-pathspec, untracked-file, two-dot store and dirty-tree rows, GPASS / GPROBE / GBLOCK; the adversary-lane verdict rows; GINC / GDEB; the parser rows: prose-quoted, trailing-text, indented, first match, whole status cells, first table only, USAGE_PROBE last match, probe detail cell, injected header overrides, sentinel only in a lane report, debris scope, forged Blockers headings, a plan-root quote in a lane report leaving a PASS on post_round / conforms and an incomplete-only BLOCKED on its re-run; the wave_route header rows; the rollup-scope rows, the group-scoped fallback row and the real collect_ledger_lines rows)
scripts/mode_matrix.py    3 hunks   (LLM_AGENT_VAR_*, GIT_* repository-variable and ARCHITECT_CORE_TOOLS / ARCHITECT_GH_*_CMD scrub, fixed git identity; canned gauntlet reply in the real table shape; utils.sh source, fails loud instead of stubbing, confirmation line)
scripts/marker_map.json  25 hunks   (62 regions: two new, line numbers shifted, four first-lines reworded upstream)
scripts/gates.py         13 hunks   (GREEN tuple retired; verify_gate: dirty-tree guard + whole-cell GREEN settling; one hunk for _code_tree_dirty, _gauntlet_header, _gauntlet_section, _GAUNTLET_VERDICT_RE / _has_verdict, _lane_rows, _lane_cells, _probe_verdict and the header-only _parse_gauntlet with its adversary-lane flag and single Blockers reader; route_verdict: lane flag, header pre-check, probe verdict, lanes_with_findings; the probe adopt and INCONCLUSIVE checks, 2 hunks; _lane_table; dead hygiene_pending_for_chain guard in stack_gate removed; wave_route header pre-check; hygiene_create collect_ledger_lines --plan / --tasks; _group_members; pr_body --tasks and the group-scoped fallback list; _collect --plan)
README.md                 2 hunks   (design-record aside in the status paragraph; run-state bullet, this section and the Verify section)
```

Every other file is byte-identical to staging; the graph's node set, prompts, loop bounds
and node timeouts are unchanged.

Toolchain census (the gates must stay project-agnostic; verification commands come
from the brief, never from a hard-coded toolchain):

```
$ grep -nE '\b(cargo|npm|pnpm|yarn|go (test|build)|pytest|mvn|gradle|make)\b' scripts/*.py
scripts/_lib.py:547:    r"`[^`]+`|\b(go|cargo|npm|pnpm|yarn|make|pytest|python3?|bash|sh|grep|rg|curl|task|just|mvn|gradle|dotnet|bun)\b\s",
scripts/gates.py:41:    r"yarn\.lock$|pnpm-lock\.yaml$|Cargo\.(toml|lock)$|requirements.*\.txt$|poetry\.lock$",
scripts/gates.py:2621:        "file:line, at most 25 per run so iterate); identifiers that only make sense with the plan open; ...
scripts/gates.py:2630:        "failing test to make it pass; you still do NOT edit the plan or task files -- report instead.\n\n"
scripts/mode_matrix.py:174:`make run`
scripts/mode_matrix.py:269:            ... "local_run_recipe": "`make run`",
scripts/smoke_test.py:97:CODE_BASE = make_repo(CODE, {..., "CLAUDE.md": "Run `go test ./...` before committing.\n", ...
scripts/smoke_test.py:156:`make run`
scripts/smoke_test.py:470:u = gates.verify_gate(base_state(..., local_run_recipe="make run"))
scripts/smoke_test.py:830:   ... "acceptance_criteria": ["`go test ./...`"], ...
scripts/smoke_test.py:945:u = AA("probe_gap", "Supply the recipe: make run", ...)
```

None of these runs a toolchain: `_lib.py:547` is the alternation of the executable-criterion
classifier (`EXECUTABLE_RE` recognises a command-shaped acceptance criterion; it hardcodes no
command), `gates.py:41` is the dependency-manifest regex (the hits are `yarn`/`pnpm` inside
lockfile names), `gates.py:2621` and `:2630` are the English verb `make` in prompt prose, and
the `mode_matrix.py` and `smoke_test.py` lines are battery fixtures. The wrapper test
`no_gate_names_a_toolchain_command` keeps it that way with a `make`-less variant of the
pattern: it matches `\b(cargo|rustc|npm|pnpm|yarn|pytest|mvn|gradle|just)\b|\bgo (test|build|run)\b`
against every line of `graph.yaml` and of each `scripts/*.py` except `smoke_test.py`, and fails
on any hit outside four documented exemptions, each of which must match exactly one line
(so a deleted or moved line surfaces): the `_lib.py` classifier alternation, the `gates.py`
lockfile regex, and the English adverb `just` in a `gates.py` comment (`An amendment round
just ran`) and in a `gates.py` prompt sentence (`just do not write to them`).

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
  check still fails the run), session check, poison (the batteries never consult the user
  config dir), and the mode-matrix fail-loud test (`mode_matrix.py` exits before the walk
  when `.shared/utils.sh` is missing or a stub);
- run dir: run-dir precedence from a fresh process, and an empty `ARCHITECT_RUNS_ROOT`
  falling back to the cache-dir tier;
- graph and gates: `build_lock` position, the variable mirror (the candidate's variables
  match the prose `architect/config.yaml` name for name and default for default, without
  `yq`), the description pin (`description_names_no_file_outside_the_repo`), toolchain
  census, ASCII sweep, and the shim pin (every `scripts/<name>.py` shim calls
  `run("<name>")`, names `<name>` in its docstring, the shims map one-to-one onto the
  `type: script` nodes of `graph.yaml`, and every `run()` name is a key of `GATES` in
  `gates.py`);
- gauntlet grammar: a prior report rendered by the review gauntlet's own `verdict_gate.py`
  and `done` template, read by `route_verdict`, composed into the next round by
  `verify_gate` and narrowed through the gauntlet's real `signals.py` and `build_items.py`;
  the no-settled-lanes round; the parser rows; the planning-store dirty-tree rows;
- budgets and channels: graph, node, `initial_state` and `_lib.py` bounds against a literal
  copy of the staging values, the `anomalies` property on every llm node, and no
  `inputs: build_lock` anywhere; the real `tools.sh` `collect_ledger_lines` rollup; both
  batteries drop the same variables as the wrapper;
- README ground-truth pins: toolchain census, counts against the graph and the prose twin,
  the `build_lock` and E9 rows, `pr_body` passing `--tasks`, the dead-guard census row, the
  grammar, V1, engine-facts, env hygiene and ledger rows, and the battery figures against the
  wrapper floors;
- real-binary dry runs: four by name (from a seeded config dir; from an empty config dir that
  installs the candidate; re-entry by run id in the prompt; an unknown run id in the prompt
  starts a fresh run), the XDG-tier dry run (only `XDG_CACHE_HOME` set, the run lands under
  the engine's cache dir), and the no-design-doc dry run (without a readable design doc the
  run parks on the `need_design_doc` escalation before orienting).
- real-binary `--info`: by name it lists the graph with no error, renders `build_lock` with
  its empty default between `pr_strategy` and `task_granularity`, seats an
  `--agent-variable build_lock` override over that default, and renders exactly the prose
  twin's variable list in order.

The mode-matrix run is the one ignored test.

The Rust wrapper runs the batteries with `HOME`, `LLM_ROOT_DIR`, `COYOTE_CACHE_DIR`,
`TMPDIR` and `ARCHITECT_RUNS_ROOT` pointed at a scratch directory and
`PYTHONDONTWRITEBYTECODE=1`, and drops any inherited `GRAPH_STATE*` / `LLM_AGENT_VAR_*`
variables, the `ARCHITECT_CORE_TOOLS` / `ARCHITECT_GH_*_CMD` tool overrides and the git
repository variables,
so nothing under this directory or `~/.config/coyote` is touched and no agent session leaks
in. `audit_graph.py`, `check_session_block.sh` and `mode_matrix.py` need the Go `yq`
by mikefarah (`yq --version` names it); the wrapper's audit, session-check and poison tests,
and the ignored matrix, skip when a different `yq` (or none) is on `PATH`. The sweep that
keeps every bundled agent from naming this candidate lives in
`tests/candidate_sisyphus_next.rs` and covers both candidate names.

Measured 2026-10-30 after the adjustments above, from a fresh scratch dir: audit
`PROBLEMS: none`; smoke `349/349 checks passed`; mode matrix `53/53 checks passed`
(`62 live regions == 62 mapped`) in 3m08s wall clock; session check last line
`ok node ids / script paths consistent` (mikefarah yq v4.53.3); no unset-variable failure
from `architect/tools.sh` under the real `utils.sh`.
