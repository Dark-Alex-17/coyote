#!/usr/bin/env python3
"""Fail-closed marker for a dead research lane of the deep-research graph.

Reached ONLY via research_one_question's `fallback:` route inside the
`research_each_question` map branch (branch-local — a map branch's fallback
must stay inside the branch subgraph). Grounded in engine behavior: on
llm-node failure the engine writes "LLM node …failed: <chain>" into
`finding` via the node's state_updates BEFORE routing to the fallback (on
success the same key holds the researched findings, which never start with
the "LLM node" prefix). This node normalizes the failure text into a
PIPELINE-FAULT finding and the chain ends here — the map collects the fault
as the lane's `finding`, so one dead research lane never sinks the whole
map. combine_findings later lifts PIPELINE-FAULT findings into
`pipeline_faults` so the fault also surfaces in the report's Pipeline notes.
"""

import json
import os

MAX_DETAIL_CHARS = 500


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def fault_finding(detail):
    return {
        "finding": (
            f"PIPELINE-FAULT: question research failed — finding unavailable — {detail}"
        )
    }


def main():
    state = load_state()
    raw = state.get("finding")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if not detail.startswith("LLM node"):
        detail = "the research lane died without recording a failure detail"
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    print(json.dumps(fault_finding(detail)))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(json.dumps(fault_finding(f"fault-marker script error: {e}")))
