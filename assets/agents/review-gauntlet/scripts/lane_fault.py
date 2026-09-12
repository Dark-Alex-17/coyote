#!/usr/bin/env python3
"""Fail-closed lane fault marker for the review gauntlet.

Reached ONLY via the run_* lane nodes' `fallback:` routes inside their map
branches. Grounded in engine behavior: on agent-node failure the engine
writes "Agent node failed: {e:#}" into `lane_out` via the node's
state_updates BEFORE routing to the fallback, so the raw failure text is
already in state. This node normalizes it into a
"PIPELINE-FAULT: <lane> lane failed after retries — …" marker that the map
collects as the lane result; verdict_gate turns any PIPELINE-FAULT lane
result into BLOCKED — a degraded lane can never read as a pass.

Lane identity comes from `lane_ctx` (the map item build_items emitted, which
carries a `lane` field); a missing/malformed item degrades to "unknown".
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
    lane = "unknown"
    lane_ctx = state.get("lane_ctx")
    if isinstance(lane_ctx, dict):
        name = lane_ctx.get("lane")
        if isinstance(name, str) and name.strip():
            lane = name.strip()
    raw = state.get("lane_out")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    print(
        json.dumps(
            {"lane_out": f"PIPELINE-FAULT: {lane} lane failed after retries — {detail}"}
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(
        json.dumps(
            {
                "lane_out": "PIPELINE-FAULT: lane failed after retries — "
                f"fault-marker script error: {e}"
            }
        )
    )
