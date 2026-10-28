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
| em dashes | `graph.yaml` (2 lines in the `final_report` prompt) | replaced with ASCII ` - ` so the repo's punctuation lint stays clean |
| `build_lock` variable | `graph.yaml` `variables:` | declared after `escalation_policy` with default `''`, mirroring the prose `sisyphus/config.yaml`; `scripts/check_session_block.sh` asserts the two variable lists match name-for-name, default-for-default (threading it to child spawns is a separate adjustment) |
| run dir | `scripts/_lib.py` `cache_dir()` / `runs_root()`, this README | moved from `$XDG_STATE_HOME/coyote/sisyphus/runs` to `<cache dir>/sisyphus-next/runs` (precedence above); `scripts/smoke_test.py` pins each resolution tier |
| dead state read | `scripts/gates.py` `triage_route` | dropped the read of `debris_exemptions_new`, a key no node writes (the `triage_findings` schema emits `debris_exemptions`); the repo's bundled-graph walker requires a declared writer for every consumed key |
| smoke summary | `scripts/smoke_test.py` | the failure tally is taken after the last check so the summary line and exit code cover every check (the three `ci_gate` checks were recorded after it) |

Recorded `diff -r` census against the staging tree (files that differ, hunks per file):

```
graph.yaml              2 hunks  (variables: build_lock; final_report prompt em dashes)
scripts/_lib.py         1 hunk   (cache_dir + runs_root)
scripts/smoke_test.py   2 hunks  (import _lib; run dir resolution checks)
scripts/gates.py        1 hunk   (triage_route: dead debris_exemptions_new read)
README.md               this section, the run-state bullet above, the Verify section
```

Every other file is byte-identical to staging; the graph's node set, prompts, schemas,
loop bounds and node timeouts are unchanged.

Toolchain census (the gates must stay project-agnostic; verification commands come
from the brief, never from a hard-coded toolchain):

```
$ grep -nE '\b(cargo|npm|pnpm|yarn|go (test|build)|pytest|mvn|gradle|make)\b' scripts/*.py
scripts/gates.py:902:        lines += ["Observability pass: make these alert-as-code / instrumentation changes following the repo's existing "
scripts/smoke_test.py:506:u = gates.ci_gate({"git_report": "ok\nANOMALY: commits I did not make\n", ...})
scripts/smoke_test.py:507:check("ci_gate: ANOMALY: lines harvested from git_report", any("commits I did not make" in e for e in ...))
```

All three hits are the English verb `make` (one prompt sentence, two battery fixtures);
no gate names a toolchain command.

## Verify

```
python3 scripts/audit_graph.py .        # static edges + script _next edges, cycles vs bounds
python3 scripts/smoke_test.py           # every gate, both sides of every bound, fail-open paths
bash scripts/check_session_block.sh     # session block + variable mirror vs sisyphus/config.yaml (needs mikefarah yq)
cargo test --test candidate_sisyphus_next   # the Rust wrapper: runs all three from the asset path
```

The Rust wrapper runs the batteries with `HOME`, `LLM_ROOT_DIR`, `COYOTE_CACHE_DIR`,
`TMPDIR` and `SISYPHUS_RUNS_ROOT` pointed at a scratch directory and
`PYTHONDONTWRITEBYTECODE=1`, so nothing under this directory or `~/.config/coyote` is
touched. `check_session_block.sh` needs the Go `yq` by mikefarah (`yq --version` names it);
the wrapper skips that one test when a different `yq` (or none) is on `PATH`. The wrapper
also sweeps every bundled agent for a `spawnable_agents` entry, a `session.chat.agent`
value or an `agent:` node naming `sisyphus-next`, pinning the candidate status above.
