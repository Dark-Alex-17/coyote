# sisyphus-next (CANDIDATE, upstream variant)

The decomposed sisyphus orchestrator as a session graph. Status: candidate - no agent
lists it as spawnable; the shipped `sisyphus` keeps running the prose protocol until
the artifact-equivalence replay passes and this directory replaces it.

## Shape

- **Spine** - script nodes (`scripts/gates.py`, one thin entry per node) own routing,
  every named loop bound, the ledgers, sentinel parsing and prompt assembly.
- **Judgment** - short single-question llm nodes with schemas; every schema carries the
  optional `anomalies` field.
- **Islands** - `coder`, `review-gauntlet`, `explore`/`librarian` (map fan-outs; team
  mode = `teammates: true`), `oracle`, `architecture-reviewer`, `step-runner`,
  `whetstone`, `engineer` (tier-3 consult) run as agent nodes exactly as shipped.
- **Durable run state** - `<cache dir>/sisyphus-next/runs/<run_id>/`, where the cache dir
  is `COYOTE_CACHE_DIR`, else `$XDG_CACHE_HOME/coyote`, else the platform cache directory
  (`~/Library/Caches/coyote` on macOS, `~/.cache/coyote` on Linux, `%LOCALAPPDATA%\coyote`
  on Windows); `SISYPHUS_RUNS_ROOT` overrides the whole path. The run dir holds the brief,
  state.json, context pack, decisions ledger, anomalies and report. A prompt containing
  `Sisyphus run id: <id>` re-enters that run.

Variables mirror `sisyphus` (names and defaults identical). The session block routes
work to the graph and conversation to the `engineer` chat lane (`chat_agent_for: sisyphus`).

## Landed-upstream adjustments

The directory is a verbatim copy of the upstream staging tree except for the rows
below. Later landings add their own rows.

| Adjustment | Where | What changed |
|---|---|---|
| em dashes | `graph.yaml` (2 lines in the `git_ops` instructions) | replaced with ASCII ` - ` so the repo's punctuation lint stays clean |
| `build_lock` variable | `graph.yaml` `variables:` | declared after `escalation_policy` with default `''`, mirroring the prose `sisyphus/config.yaml`; `scripts/check_session_block.sh` asserts the two variable lists match name-for-name, default-for-default (threaded to the child spawns in the `build_lock` threading row below) |
| run dir | `scripts/_lib.py` `cache_dir()` / `runs_root()`, this README | moved from `$XDG_STATE_HOME/coyote/sisyphus/runs` to `<cache dir>/sisyphus-next/runs` (precedence above); `scripts/smoke_test.py` pins each resolution tier |
| debris exemptions ledger merge | `graph.yaml` `triage_findings` output_schema + initial_state, `scripts/gates.py` `triage_route` | the schema emitted `debris_exemptions`, and the engine merges every schema property into state, so each triage round overwrote the run-wide ledger; the property is now `debris_exemptions_new` (seeded `[]` in initial_state), `triage_route` merges it into the ledger (staging's own merge line) and resets it to `[]` so a round that omits the property cannot re-merge a stale list; the smoke battery pins three rounds |
| smoke summary | `scripts/smoke_test.py` | the failure tally is taken after the last check so the summary line and exit code cover every check (the three `ci_gate` checks were recorded after it) |
| `debt_scan` fallback | `graph.yaml` `debt_scan` node, `scripts/debt_scan.py`, `scripts/gates.py` `bookkeeping_scan` | `fallback: bookkeeping_scan` added next to `next:` - the engine forwards a script crash only along `fallback:`, so the shim's crash-forward sentence now names that edge; the validator baseline is unchanged because `debt_scan` itself is entered only through a script `_next` edge, so nothing downstream becomes declared-edge reachable; `bookkeeping_scan` treats `debt_hits` as untrusted after a crash (the engine leaves the key as it was and `route_coder_result` re-enters `debt_scan` every coder round, so a later round's crash discards the previous report and substitutes a short DEBT_SCAN_FAILED note that the judge sees; the first pass keeps the initial_state default) |
| `triage` reset | `scripts/gates.py` `triage_route` | the update dict resets `triage` to `[]` beside `debris_exemptions_new` (both are schema properties of `triage_findings`; the engine merges nothing when an llm node fails, so without the reset a crashed round 2+ replayed round 1's verdicts); the fail-safe stays keyed on an empty list because an llm failure never sets `last_script_error` - it lands in `last_node_output`, which `note_script_error` already ledgers |
| gauntlet grammar | `scripts/gates.py` `build_gauntlet_prompt` / `_gauntlet_header` / `_gauntlet_section` / `_parse_gauntlet` / `route_gauntlet` / `_assemble_report`, `scripts/smoke_test.py` | settled lanes are the lanes whose whole status cell is exactly `GREEN` (`GREEN (attention)`, `BLOCKED`, `SKIPPED` and the `debris` lane never settle; lanes with findings last round are excluded); `Settled lanes:` is emitted only when non-empty and `Re-review delta: <sha>..HEAD` stays bare; the gauntlet's reply opens with `GAUNTLET: <verdict>` on line 1 and appends every lane's raw report under `## Full lane reports` inside `<details>` blocks, so `GAUNTLET:`, `GAUNTLET_REVIEW_INCOMPLETE:`, the lane table and the `## Blockers` section are read only from the header before the first column-0 `## Full lane reports` line (or before the first `<details>` when that heading is absent; `_gauntlet_header` is that one slice, shared by the parser and by `route_gauntlet`'s no-sentinel pre-check, so a sentinel that appears only inside a lane report is an incomplete pipeline), as column-0 whole lines with `[ \t]*` gaps, first match winning (prose-quoted, indented, trailing-text or split-line sentinels are ignored, so a missing whole-line verdict is BLOCKED, and a lane report that quotes `GAUNTLET: PASS` or a `\| security \| GREEN \|` row cannot override the header); the lane table is read as `\| lane \| status \| detail \|` from the first contiguous run of column-0 `\|` lines in the header only (an indented row, a later duplicate row for the same lane, or a column-0 row after the table has ended, such as one inside the LLM-authored `## Lane selection` prose, changes nothing: the first row per lane wins), with the whole status cell kept and the detail cell carried into the per-round ledger (`lane_details`, rendered on the report's round lines); `## Blockers` and `## Human attention required` are exact, case-sensitive, column-0 level-2 headings with column-0 `- ` / `* ` items, read only below the gauntlet's own `## Lane selection` heading (its lane reasons are model-written and can carry a column-0 heading), from the last such heading (attention below the Blockers heading it follows) up to the next column-0 `## ` heading, and an unbulleted `debris: Implementation debris` line counts only inside that Blockers section, so a look-alike `## Lane selection`, `## Blockers` or `## Human attention required` heading or debris line in the lane reasons adds no phantom blocker and cannot empty the real list, which would turn a BLOCKED round into a review-incomplete escalation. The parser guarantees that much and no more: a look-alike `## Full lane reports` or `<details>` line in the lane reasons would still end the header early, so the header boundary is guaranteed by the producer, which writes every model-written item on one line; the smoke fixtures use that table shape with one row per status value. A lane narrowed in one round reports `SKIPPED`, which the merged `gauntlet_lanes` records, so it is not settled for the round after: settlement lasts one narrowed round by design, and the lane runs again unless the next report leaves it whole-cell `GREEN` |
| `build_lock` threading | `graph.yaml` `run_coder`, `run_gauntlet`, `run_step` | `inputs: build_lock: '{{build_lock}}'` on the three children that declare the variable (coder, review-gauntlet and step-runner; the gauntlet forwards it to the adversary the same way). Every other child - the graph agent librarian and the config agents explore, oracle, architecture-reviewer, whetstone and engineer - receives it by variable inheritance and must not carry `inputs:`. Those six declare no `build_lock` and do not consume it themselves: the inherited value reaches them, but nothing in them runs a build under it (no file of theirs names `build_lock`, `gate_prefix` or `run_gate`). The three `inputs:` lines are belt-and-braces: the engine copies the parent's variables to every agent-node child, graph-agent children included (pinned by the `src/function/agents.rs` test `run_agent_for_graph_child_seats_the_parents_build_lock_over_its_default`), so inheritance alone would reach them. Not a schema change: no node, prompt, bound or timeout moved |
| V1: session-resume replacement (artifact re-entry) - the contract | this README | fix rounds and later re-entries hand the delegate the run's artifact paste (brief, state, context pack, decisions) by `Sisyphus run id`; nothing is ever resumed by a child session id, so there is no resume plumbing and no session-id ledger |
| engine facts | this README | agent-node resume: not available by decision (2026-10-28); E1 stays as the artifact contract; G6 escalation_timeout open |
| battery env hygiene | `scripts/smoke_test.py` | every `LLM_AGENT_VAR_*` variable is dropped from the environment at start so the battery cannot inherit the agent variables of the process running it (the standalone-brief intake check reads `LLM_AGENT_VAR_PARALLEL` through `_lib.var`) |

Recorded census against the staging tree (files that differ; hunks per file as counted by
`diff -u <staging file> <file> | grep -c '^@@'`):

```
graph.yaml              9 hunks  (variables: build_lock; initial_state debt_hits comment; initial_state debris_exemptions_new; debt_scan fallback; triage_findings schema property rename; git_ops instructions em dashes, 2 lines; build_lock inputs on run_coder, run_gauntlet and run_step, 3 hunks)
scripts/_lib.py         1 hunk   (new cache_dir; runs_root docstring; runs_root base path - three edits close enough to share one hunk)
scripts/smoke_test.py   7 hunks  (LLM_AGENT_VAR_* scrub + import _lib, one hunk; run dir resolution checks; bookkeeping_scan crash rows; gauntlet grammar fixtures: composer settled-lanes rows, dirty-tree row, table-shaped parser rows, the sentinel matrix, the indented-sentinel, indented-row, duplicate-row, post-table-row and indented-Blockers rows, and the sentinel-only-in-a-lane-report route_gauntlet row; triage reset rows + three-round exemptions ledger pin; the failure tally moved below the last check, 2 hunks; the pre-check/_has_verdict rows, the round-line lane-details row, the forged-Lane-selection-heading rows and the forged-debris-line row sit in the tally hunk)
scripts/gates.py        6 hunks  (the DEBT_HITS_STALE constant; bookkeeping_scan: debt_hits untrusted after a crash; build_gauntlet_prompt: whole-cell GREEN settling; one hunk for _gauntlet_header (the header slice shared by the parser and the router's pre-check), _has_verdict (the parser's verdict regex, shared with the router's pre-check), _gauntlet_section (exact level-2 section read below Lane selection, last heading wins), _parse_gauntlet (header-only, column-0 first-match sentinels, lane rows from the first contiguous column-0 table with the first row per lane winning, whole status cell + detail cell, Blockers and attention sections through _gauntlet_section with column-0 items, unbulleted debris lines read only inside the Blockers section) and route_gauntlet (six-tuple, lane_details, the GREEN-based lanes_with_findings and the header-sliced, regex-twinned pre-check); triage_route: the debris_exemptions_new + triage resets in the update dict, the merge line itself byte-equal to staging; _assemble_report: lane details on the round lines)
scripts/debt_scan.py    1 hunk   (docstring: the crash-forward sentence names the fallback edge and what bookkeeping_scan does with debt_hits)
README.md               this section, the run-state bullet above, the Verify section
```

Every other file is byte-identical to staging; the graph's node set, prompts, loop bounds
and node timeouts are unchanged, and the one schema change is the property rename above
(the `inputs:` additions pass a variable down, they change no schema).

Toolchain census (the gates must stay project-agnostic; verification commands come
from the brief, never from a hard-coded toolchain):

```
$ grep -nE '\b(cargo|npm|pnpm|yarn|go (test|build)|pytest|mvn|gradle|make)\b' scripts/*.py
scripts/gates.py:904:        lines += ["Observability pass: make these alert-as-code / instrumentation changes following the repo's existing "
scripts/smoke_test.py:603:u = gates.ci_gate({"git_report": "ok\nANOMALY: commits I did not make\n", "git_task": {"return_to": "final_report"}, "run_dir": TMP, "anomalies_log": []})
scripts/smoke_test.py:604:check("ci_gate: ANOMALY: lines harvested from git_report", any("commits I did not make" in e for e in u.get("anomalies_log", [])))
```

All three hits are the English verb `make` (one prompt sentence, two battery fixtures);
no gate names a toolchain command. The wrapper test `no_gate_names_a_toolchain_command`
keeps it that way with a `make`-less variant of the pattern: it matches
`\b(cargo|rustc|npm|pnpm|yarn|pytest|mvn|gradle|just)\b|\bgo (test|build|run)\b` against every
line of `graph.yaml` and of each `scripts/*.py` except `smoke_test.py`, and fails on any hit.

## Verify

```
python3 scripts/audit_graph.py .        # static edges + script _next edges, cycles vs bounds (needs mikefarah yq)
python3 scripts/smoke_test.py           # every gate, both sides of every bound, fail-open paths
bash scripts/check_session_block.sh     # session block + variable mirror vs sisyphus/config.yaml (needs mikefarah yq)
cargo test --test candidate_sisyphus_next   # the Rust wrapper: runs all three from the asset path
```

The wrapper also drives the composed re-review prompt through the real
`review-gauntlet/scripts/signals.py` and `build_items.py`, so a drift between what the
composer writes and what the gauntlet reads fails here rather than in a live run.
The Rust wrapper runs the batteries with `HOME`, `LLM_ROOT_DIR`, `COYOTE_CACHE_DIR`,
`TMPDIR` and `SISYPHUS_RUNS_ROOT` pointed at a scratch directory and
`PYTHONDONTWRITEBYTECODE=1`, so nothing under this directory or `~/.config/coyote` is
touched. `audit_graph.py` and `check_session_block.sh` need the Go `yq` by mikefarah
(`yq --version` names it); the wrapper skips both of those tests when a different `yq`
(or none) is on `PATH`. The wrapper also sweeps every bundled agent's `graph.yaml` and
`config.yaml` for a `spawnable_agents` entry, a `session.chat.agent` value or an `agent:`
key naming `sisyphus-next` or `architect-next`, and every `instructions.md` and
`config.yaml` for prose containing either name, pinning the candidate status above.

Measured 2026-10-29 after the adjustments above, from a fresh scratch dir: audit
`PROBLEMS: none`; smoke `237/237 checks passed`; session check ok (mikefarah yq
v4.53.3).
