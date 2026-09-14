#!/usr/bin/env python3
"""Fail-closed marker for dead aux context lanes of the code-reviewer graph.

Reached ONLY via aux_lanes' `fallback:` route. aux_lanes' state_updates wrote
an "LLM node …failed: <chain>" string into `aux_failure` before routing here;
this node records the failure as a PIPELINE-FAULT in `aux_note`, which flows
into synthesize's "lane skips" prompt line. Aux lanes are ENRICHMENT, not
review coverage: the degradation is made VISIBLE in the report's inputs but
never blocks — verdict.py deliberately does NOT scan aux_note for faults.
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
    raw = state.get("aux_failure")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    print(
        json.dumps(
            {"aux_note": f"PIPELINE-FAULT: aux context lanes failed after retries — {detail}"}
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(
        json.dumps(
            {
                "aux_note": "PIPELINE-FAULT: aux context lanes failed after retries — "
                f"fault-marker script error: {e}"
            }
        )
    )
