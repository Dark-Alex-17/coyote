#!/usr/bin/env python3
"""Fail-closed fault marker for the adversary graph.

Reached ONLY via `fallback:` routes (parse, facts). Appends a machine-readable
"PIPELINE-FAULT:" marker to `pipeline_faults` and continues to `verdict`,
which turns any recorded fault into DIVERGES — the graph never dies without
its sentinel, and a degraded run can never read as CONFORMS.

Stage attribution: an llm-node failure writes an "LLM node …failed: <chain>"
string into `parse_failure` via parse's `state_updates`; anything else means
the deterministic facts (diff resolution) script failed, which leaves no
failure text in state by design.
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
    if detail.startswith("LLM node"):
        detail = detail.replace("\n", " ")
        if len(detail) > MAX_DETAIL_CHARS:
            detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
        faults.append(f"PIPELINE-FAULT: parse failed — cannot review: {detail}")
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
