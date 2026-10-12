#!/usr/bin/env python3
"""Fail-closed marker for a debris_lint node the engine killed.

Reached ONLY via debris_lint's `fallback:` route. debris_lint.py catches its
own crashes and writes `debris_error` itself; this node covers the case where
the process never got to print (node timeout, signal, missing interpreter).
The engine records the failure in `last_script_error` before routing here;
this node copies it into `debris_error` and clears it, so verdict_gate turns
it into a PIPELINE-FAULT `debris` row and `GAUNTLET_REVIEW_INCOMPLETE: debris`
and a later node never reads the stale text; then it continues to
select_lanes so the review lanes still run.
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
    raw = state.get("last_script_error")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 3] + "..."
    print(
        json.dumps(
            {
                "debris_findings": [],
                "debris_summary": "",
                "debris_error": f"debris lint node killed: {detail or '(no engine detail recorded)'}",
                "last_script_error": "",
            }
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 -- the fault marker itself must never crash
    print(
        json.dumps(
            {
                "debris_findings": [],
                "debris_summary": "",
                "debris_error": f"debris lint node killed: fault-marker script error: {e}",
                "last_script_error": "",
            }
        )
    )
