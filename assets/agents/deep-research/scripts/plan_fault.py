#!/usr/bin/env python3
"""Fail-closed marker for a dead planning stage of the deep-research graph.

Reached ONLY via plan's `fallback:` route. Grounded in engine behavior: on
llm-node failure the engine writes "LLM node …failed: <chain>" into
`plan_failure` via the node's state_updates BEFORE routing to the fallback
(on success the same key holds the node's structured JSON output, which
never starts with the "LLM node" prefix). No plan means no sub-questions —
there is nothing to research — so this path is fail-closed: record the
PIPELINE-FAULT and continue to `end_fault`, which renders the
DEEP_RESEARCH FAILED sentinel.

The planner runs inside the static fan-out [plan, knowledge_lookup]. While
this marker routes to end_fault, the sibling branch proceeds one more
super-step into `research_each_question`, whose `{{questions}}` resolves to
the initial_state [] — the doomed lane maps over zero items and the graph
returns end_fault's output at the next join.
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
    raw = state.get("plan_failure")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if not detail.startswith("LLM node"):
        detail = "the planning stage died without recording a failure detail"
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    fault_text = f"planning failed — nothing to research: {detail}"
    faults.append(f"PIPELINE-FAULT: {fault_text}")
    print(json.dumps({"fault_text": fault_text, "pipeline_faults": faults}))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    fault_text = f"planning failed — fault-marker script error: {e}"
    print(
        json.dumps(
            {
                "fault_text": fault_text,
                "pipeline_faults": [f"PIPELINE-FAULT: {fault_text}"],
            }
        )
    )
