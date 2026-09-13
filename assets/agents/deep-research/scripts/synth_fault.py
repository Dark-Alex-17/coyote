#!/usr/bin/env python3
"""Fail-closed marker for a dead synthesis stage of the deep-research graph.

Reached ONLY via synthesize's `fallback:` route. Grounded in engine
behavior: on agent-node failure the engine writes "Agent node failed:
{e:#}" into `report` via the node's state_updates BEFORE routing to the
fallback (on success the same key holds the report-writer's report, which
never starts with that prefix). No report means no product, so this path is
fail-closed: normalize the failure text, record the PIPELINE-FAULT, and
continue to `end_fault`, which renders the DEEP_RESEARCH FAILED sentinel.
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
    raw = state.get("report")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if not detail.startswith("Agent node failed:"):
        detail = "the synthesis stage died without recording a failure detail"
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    fault_text = f"report synthesis failed — no report was produced: {detail}"
    faults.append(f"PIPELINE-FAULT: {fault_text}")
    print(json.dumps({"fault_text": fault_text, "pipeline_faults": faults}))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    fault_text = f"report synthesis failed — fault-marker script error: {e}"
    print(
        json.dumps(
            {
                "fault_text": fault_text,
                "pipeline_faults": [f"PIPELINE-FAULT: {fault_text}"],
            }
        )
    )
