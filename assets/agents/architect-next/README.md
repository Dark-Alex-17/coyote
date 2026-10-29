# architect-next (CANDIDATE, upstream variant)

The decomposed architect orchestrator as a session graph. Status: candidate - no agent
lists it as spawnable; the shipped `architect` keeps running the prose protocol until the
artifact-equivalence replay passes and this directory replaces it. The delegate is
`sisyphus` (two `agent:` lines); the swap to the graph sisyphus is one config edit plus
three `inputs:` lines (see the design record).

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
| `build_lock` variable | `graph.yaml` `variables:` | declared after `pr_strategy` with default `''`, mirroring the prose `architect/config.yaml`; `scripts/check_session_block.sh` asserts the two variable lists match name-for-name, default-for-default. Recorded as a gap: the graph does not yet thread the variable to its graph-agent children, so `run_gauntlet` and `wave_gauntlet` (`review-gauntlet`) and `run_adversary` (`adversary`) carry no `inputs: build_lock`. The config-agent children `delegate` and `wave_delegate` (`sisyphus`) already receive it by variable inheritance (the child context carries the parent's agent variables), so they need no `inputs:` and no delegation-text carry. The next adjustment to this agent threads it to the three graph nodes and removes this gap |
| run dir | `scripts/_lib.py` `cache_dir()` / `runs_root()`, this README | moved from `$XDG_STATE_HOME/coyote/architect/runs` to `<cache dir>/architect-next/runs` (precedence above); `scripts/smoke_test.py` pins each resolution tier |
| `utils.sh` source | `scripts/smoke_test.py`, `scripts/mode_matrix.py` | both batteries copy the repo asset `.shared/utils.sh` into the scratch `LLM_ROOT_DIR` instead of the user-config copy, which never existed under the re-pointed `HOME`, so the stub branch always ran; the real `set -euo pipefail` is now live inside `architect/tools.sh` under both batteries, with no unset-variable fallout, and a smoke check pins that the copy is the real file |
| marker map | `scripts/marker_map.json` | regenerated against the landed `architect/instructions.md`, which carries 62 marker regions (staging mapped 60): the two regions added since staging are the micro-mode `review_cadence: batch` line on the parallel wave spawn (mapped to the `## MODE` block `_delegation_text` builds for `compose_delegation` and `wave_claim`) and the multi-PR `--tasks <group members>` line for `assemble_pr_body` (mapped to `pr_body` and recorded as a gap: the graph still calls the tool without `--tasks`, so a group body covers every complete task of the plan). The 60 known regions keep their mechanism and home; four recorded first-lines changed because upstream reworded or ASCII-normalised those lines; `first` keeps `\u2014` JSON escapes for 13 lines of the prose file |
| dead `hygiene_pending_for_chain` guard | `scripts/gates.py` `stack_gate` | two lines deleted: the guard read `state.get("hygiene_pending_for_chain", {})`, a key no script, variable, `state_updates` target or output schema ever writes, so it was always falsy and the `continue` it guarded never ran; the repo's state-key walker (`bundled_graph_output_schemas_declare_every_key_relied_on`) rejects a consumed key with no declared writer, and deleting the read keeps behaviour identical rather than inventing a writer |

Recorded census against the staging tree (files that differ; hunks per file as counted by
`diff -u <staging file> <file> | grep -c '^@@'`):

```
graph.yaml                2 hunks   (project_dir em dash; pr_strategy em dash + build_lock + task_granularity em dash)
scripts/_lib.py           1 hunk    (new cache_dir; runs_root docstring; runs_root base path)
scripts/smoke_test.py     3 hunks   (import _lib; utils.sh source + the real-copy pin; run dir resolution checks)
scripts/mode_matrix.py    1 hunk    (utils.sh source)
scripts/marker_map.json  25 hunks   (62 regions: two new, line numbers shifted, four first-lines reworded upstream)
scripts/gates.py          1 hunk    (dead hygiene_pending_for_chain guard in stack_gate removed)
README.md                 this section, the run-state bullet above, the Verify section
```

Every other file is byte-identical to staging; the graph's node set, prompts, loop bounds
and node timeouts are unchanged.

Toolchain census (the gates must stay project-agnostic; verification commands come
from the brief, never from a hard-coded toolchain):

```
$ grep -nE '\b(cargo|npm|pnpm|yarn|go (test|build)|pytest|mvn|gradle|make)\b' scripts/*.py
scripts/_lib.py:546:    r"`[^`]+`|\b(go|cargo|npm|pnpm|yarn|make|pytest|python3?|bash|sh|grep|rg|curl|task|just|mvn|gradle|dotnet|bun)\b\s",
scripts/gates.py:42:    r"yarn\.lock$|pnpm-lock\.yaml$|Cargo\.(toml|lock)$|requirements.*\.txt$|poetry\.lock$",
scripts/gates.py:2511:        "file:line, at most 25 per run so iterate); identifiers that only make sense with the plan open; ...
scripts/gates.py:2520:        "failing test to make it pass; you still do NOT edit the plan or task files -- report instead.\n\n"
scripts/mode_matrix.py:158:`make run`
scripts/mode_matrix.py:253:            ... "local_run_recipe": "`make run`",
scripts/smoke_test.py:84:CODE_BASE = make_repo(CODE, {..., "CLAUDE.md": "Run `go test ./...` before committing.\n", ...
scripts/smoke_test.py:143:`make run`
scripts/smoke_test.py:455:u = gates.verify_gate(base_state(..., local_run_recipe="make run"))
scripts/smoke_test.py:634:   ... "acceptance_criteria": ["`go test ./...`"], ...
scripts/smoke_test.py:749:u = AA("probe_gap", "Supply the recipe: make run", ...)
```

None of these runs a toolchain: `_lib.py:546` is the alternation of the executable-criterion
classifier (`EXECUTABLE_RE` recognises a command-shaped acceptance criterion; it hardcodes no
command), `gates.py:42` is the dependency-manifest regex (the hits are `yarn`/`pnpm` inside
lockfile names), `gates.py:2511` and `:2520` are the English verb `make` in prompt prose, and
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
cargo test --test candidate_architect_next              # the Rust wrapper: audit, smoke, session check, poison and precedence tests
cargo test --test candidate_architect_next -- --ignored # the mode matrix through the wrapper (three to five minutes; opt-in)
```

The Rust wrapper runs the batteries with `HOME`, `LLM_ROOT_DIR`, `COYOTE_CACHE_DIR`,
`TMPDIR` and `ARCHITECT_RUNS_ROOT` pointed at a scratch directory and
`PYTHONDONTWRITEBYTECODE=1`, so nothing under this directory or `~/.config/coyote` is
touched. `audit_graph.py` and `check_session_block.sh` need the Go `yq` by mikefarah
(`yq --version` names it); the wrapper skips both of those tests when a different `yq`
(or none) is on `PATH`. The sweep that keeps every bundled agent from naming this
candidate lives in `tests/candidate_sisyphus_next.rs` and covers both candidate names.

Measured 2026-10-28 after the adjustments above, from a fresh scratch dir: audit
`PROBLEMS: none`; smoke `304/304 checks passed`; mode matrix `53/53 checks passed`
(`62 live regions == 62 mapped`) in 3m53s wall clock; session check ok (mikefarah yq
v4.53.3); no unset-variable failure from `architect/tools.sh` under the real `utils.sh`.
