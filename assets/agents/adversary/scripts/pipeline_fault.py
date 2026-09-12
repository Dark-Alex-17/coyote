#!/usr/bin/env python3
"""Fail-closed fault marker for the adversary graph.

Reached ONLY via `fallback:` routes (parse, facts, run_checks). Appends a
machine-readable "PIPELINE-FAULT:" marker to `pipeline_faults` and continues
to `verdict`, which turns any recorded fault into DIVERGES — the graph never
dies without its sentinel, and a degraded run can never read as CONFORMS.

Stage attribution — grounded in engine behavior: a SCRIPT node's fallback
route writes NOTHING into state (src/graph/executor.rs script arm routes to
the fallback without recording the error), so the two script stages are
discriminated by which stage outputs are present:
- parse (llm): its `state_updates` wrote an "LLM node …failed: <chain>"
  string into `parse_failure` — attribute to parse.
- run_checks vs facts (scripts): diff_facts.py unconditionally writes a
  NONEMPTY `diff_text` (even its internal-failure path writes
  "(diff unavailable)"), while the graph's initial value is ''. A nonempty
  `diff_text` proves facts completed, so the fault is the verification
  runner (run_checks killed at the node level, e.g. by the node timeout);
  an empty `diff_text` means facts itself died.
"""

import json
import os

MAX_DETAIL_CHARS = 500


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    faults = [f for f in (state.get("pipeline_faults") or []) if isinstance(f, str)]
    parse_failure = state.get("parse_failure")
    detail = parse_failure.strip() if isinstance(parse_failure, str) else ""
    diff_text = state.get("diff_text")
    facts_completed = isinstance(diff_text, str) and bool(diff_text.strip())
    if detail.startswith("LLM node"):
        detail = detail.replace("\n", " ")
        if len(detail) > MAX_DETAIL_CHARS:
            detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
        faults.append(f"PIPELINE-FAULT: parse failed — cannot review: {detail}")
    elif facts_completed:
        faults.append(
            "PIPELINE-FAULT: verification runner killed — the run_checks stage "
            "died at the node level before recording exec_results; execution "
            "criteria are unproven"
        )
    else:
        faults.append(
            "PIPELINE-FAULT: diff resolution failed — changed files and diff text "
            "could not be resolved; criterion verification never ran"
        )
    print(json.dumps({"pipeline_faults": faults}))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(
        json.dumps(
            {
                "pipeline_faults": [
                    f"PIPELINE-FAULT: fault-marker script error: {e} — treat the "
                    "run as degraded"
                ]
            }
        )
    )
