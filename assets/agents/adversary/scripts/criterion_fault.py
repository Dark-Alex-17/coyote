#!/usr/bin/env python3
"""Fail-closed per-criterion fault marker for the adversary graph.

Reached ONLY via check_criterion's `fallback:` route, after the engine has
written its "LLM node …failed: <chain>" text into `crit_verdict` through
state_updates. Converts that text into a schema-shaped UNMET verdict whose
evidence carries a "PIPELINE-FAULT:" marker, so one criterion check that dies
(max_iterations exhausted, API failure) is recorded as DIED rather than
judged, the other criteria still get verdicts, and the run is DIVERGES. The
map collects it like any other verdict — the branch never sinks the map.
"""

import json
import os

MAX_DETAIL_CHARS = 500


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def criterion_fields(state):
    c = state.get("criterion")
    if isinstance(c, dict):
        cid = c.get("id")
        text = c.get("text")
        return (
            cid.strip() if isinstance(cid, str) and cid.strip() else "unknown",
            text.strip() if isinstance(text, str) else "",
        )
    return "unknown", ""


def failure_detail(state):
    captured = state.get("crit_verdict")
    detail = captured.strip() if isinstance(captured, str) else ""
    if not detail.startswith("LLM node"):
        return "no failure text captured"
    detail = detail.replace("\n", " ")
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    return detail


def main():
    state = load_state()
    cid, ctext = criterion_fields(state)
    detail = failure_detail(state)
    print(
        json.dumps(
            {
                "crit_verdict": {
                    "id": cid,
                    "text": ctext,
                    "status": "UNMET",
                    "evidence": f"PIPELINE-FAULT: criterion check failed — {detail}",
                    "complaint": (
                        f"criterion check DIED (pipeline fault) — {detail}; the "
                        "criterion was NOT verified and is treated as unmet "
                        "(fail-closed)"
                    ),
                }
            }
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(
        json.dumps(
            {
                "crit_verdict": {
                    "id": "unknown",
                    "text": "",
                    "status": "UNMET",
                    "evidence": f"PIPELINE-FAULT: criterion fault-marker script error: {e}",
                    "complaint": (
                        f"criterion check DIED (pipeline fault) — fault-marker script "
                        f"error: {e}; the criterion was NOT verified and is treated as "
                        "unmet (fail-closed)"
                    ),
                }
            }
        )
    )
