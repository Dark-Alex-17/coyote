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
below. Later landings add their own rows (grammar reconciliation, fast-path re-entry).

| Adjustment | Where | What changed |
|---|---|---|
| em dashes | `graph.yaml` (2 lines in the `git_ops` instructions) | replaced with ASCII ` - ` so the repo's punctuation lint stays clean |
| `build_lock` variable | `graph.yaml` `variables:` | declared after `escalation_policy` with default `''`, mirroring the prose `sisyphus/config.yaml`; `scripts/check_session_block.sh` asserts the two variable lists match name-for-name, default-for-default (threading it to child spawns is a separate adjustment) |
| run dir | `scripts/_lib.py` `cache_dir()` / `runs_root()`, this README | moved from `$XDG_STATE_HOME/coyote/sisyphus/runs` to `<cache dir>/sisyphus-next/runs` (precedence above); `scripts/smoke_test.py` pins each resolution tier |
| debris exemptions ledger merge | `graph.yaml` `triage_findings` output_schema + initial_state, `scripts/gates.py` `triage_route` | the schema emitted `debris_exemptions`, and the engine merges every schema property into state, so each triage round overwrote the run-wide ledger; the property is now `debris_exemptions_new` (seeded `[]` in initial_state), `triage_route` merges it into the ledger (staging's own merge line) and resets it to `[]` so a round that omits the property cannot re-merge a stale list; the smoke battery pins three rounds |
| smoke summary | `scripts/smoke_test.py` | the failure tally is taken after the last check so the summary line and exit code cover every check (the three `ci_gate` checks were recorded after it) |
| `debt_scan` fallback | `graph.yaml` `debt_scan` node, `scripts/debt_scan.py`, `scripts/gates.py` `bookkeeping_scan` | `fallback: bookkeeping_scan` added next to `next:` - the engine forwards a script crash only along `fallback:`, so the shim's crash-forward sentence now names that edge; the validator baseline is unchanged because `debt_scan` itself is entered only through a script `_next` edge, so nothing downstream becomes declared-edge reachable; `bookkeeping_scan` treats `debt_hits` as untrusted after a crash (the engine leaves the key as it was and `route_coder_result` re-enters `debt_scan` every coder round, so a later round's crash discards the previous report and substitutes a short DEBT_SCAN_FAILED note that the judge sees; the first pass keeps the initial_state default) |
| `triage` reset | `scripts/gates.py` `triage_route` | the update dict resets `triage` to `[]` beside `debris_exemptions_new` (both are schema properties of `triage_findings`; the engine merges nothing when an llm node fails, so without the reset a crashed round 2+ replayed round 1's verdicts); the fail-safe stays keyed on an empty list because an llm failure never sets `last_script_error` - it lands in `last_node_output`, which `note_script_error` already ledgers |

Recorded census against the staging tree (files that differ; hunks per file as counted by
`diff -u <staging file> <file> | grep -c '^@@'`):

```
graph.yaml              6 hunks  (variables: build_lock; initial_state debt_hits comment; initial_state debris_exemptions_new; debt_scan fallback; triage_findings schema property rename; git_ops instructions em dashes, 2 lines)
scripts/_lib.py         1 hunk   (new cache_dir; runs_root docstring; runs_root base path - three edits close enough to share one hunk)
scripts/smoke_test.py   6 hunks  (import _lib; run dir resolution checks; bookkeeping_scan crash rows; triage reset rows + three-round exemptions ledger pin; the failure tally moved below the last check, 2 hunks)
scripts/gates.py        3 hunks  (the DEBT_HITS_STALE constant; bookkeeping_scan: debt_hits untrusted after a crash; triage_route: the debris_exemptions_new + triage resets in the update dict, the merge line itself byte-equal to staging)
scripts/debt_scan.py    1 hunk   (docstring: the crash-forward sentence names the fallback edge and what bookkeeping_scan does with debt_hits)
README.md               this section, the run-state bullet above, the Verify section
```

Every other file is byte-identical to staging; the graph's node set, prompts, loop bounds
and node timeouts are unchanged, and the one schema change is the property rename above.

Toolchain census (the gates must stay project-agnostic; verification commands come
from the brief, never from a hard-coded toolchain):

```
$ grep -nE '\b(cargo|npm|pnpm|yarn|go (test|build)|pytest|mvn|gradle|make)\b' scripts/*.py
scripts/gates.py:902:        lines += ["Observability pass: make these alert-as-code / instrumentation changes following the repo's existing "
scripts/smoke_test.py:513:u = gates.ci_gate({"git_report": "ok\nANOMALY: commits I did not make\n", ...})
scripts/smoke_test.py:514:check("ci_gate: ANOMALY: lines harvested from git_report", any("commits I did not make" in e for e in ...))
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

The Rust wrapper runs the batteries with `HOME`, `LLM_ROOT_DIR`, `COYOTE_CACHE_DIR`,
`TMPDIR` and `SISYPHUS_RUNS_ROOT` pointed at a scratch directory and
`PYTHONDONTWRITEBYTECODE=1`, so nothing under this directory or `~/.config/coyote` is
touched. `audit_graph.py` and `check_session_block.sh` need the Go `yq` by mikefarah
(`yq --version` names it); the wrapper skips both of those tests when a different `yq`
(or none) is on `PATH`. The wrapper also sweeps every bundled agent's `graph.yaml` and
`config.yaml` for a `spawnable_agents` entry, a `session.chat.agent` value or an `agent:`
key naming `sisyphus-next` or `architect-next`, and every `instructions.md` and
`config.yaml` for prose containing either name, pinning the candidate status above.

Measured 2026-10-28 after the adjustments above, from a fresh scratch dir: audit
`PROBLEMS: none`; smoke `205/205 checks passed`; session check ok (mikefarah yq
v4.53.3).
