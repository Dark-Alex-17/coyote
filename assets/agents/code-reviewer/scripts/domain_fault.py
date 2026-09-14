#!/usr/bin/env python3
"""Fail-closed domain lane fault marker for the code-reviewer graph.

Reached ONLY via review_domain's `fallback:` route inside the map branch.
Grounded in engine behavior: on agent-node failure the engine writes
"Agent node failed: {e:#}" into `domain_report` via the node's state_updates
BEFORE routing to the fallback, so the raw failure text is already in state.
This node normalizes it into a "> ⚠️ PIPELINE-FAULT: domain review lane
failed after retries — …" banner the map collects as the lane's report;
verdict.py counts any banner-prefixed report as a fault and forces
NEEDS-HUMAN — a dead domain lane never reads as a clean slice.

Slice identity comes from `domain_group` (the map item cover_gate emitted,
which carries `domain` and `files`); a missing/malformed item degrades to
the failure detail alone.
"""

import json
import os

MAX_DETAIL_CHARS = 500

FAULT_PREFIX = "> ⚠️ PIPELINE-FAULT: domain review lane failed after retries — "


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    slice_desc = ""
    group = state.get("domain_group")
    if isinstance(group, dict):
        domain = group.get("domain")
        files = [f for f in (group.get("files") or []) if isinstance(f, str)]
        if isinstance(domain, str) and domain.strip():
            slice_desc = f"domain '{domain.strip()}' (files: {', '.join(files)}): "
    raw = state.get("domain_report")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    print(json.dumps({"domain_report": f"{FAULT_PREFIX}{slice_desc}{detail}"}))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(
        json.dumps(
            {
                "domain_report": "> ⚠️ PIPELINE-FAULT: domain review lane failed "
                f"after retries — fault-marker script error: {e}"
            }
        )
    )
