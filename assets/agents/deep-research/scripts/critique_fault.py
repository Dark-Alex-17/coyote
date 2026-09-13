#!/usr/bin/env python3
"""Degrade-visibly marker for a dead critique stage of deep-research.

Reached ONLY via critique's `fallback:` route. Grounded in engine behavior:
on llm-node failure the engine writes "LLM node …failed: <chain>" into
`critique` via the node's state_updates BEFORE routing to the fallback.
Critique is a quality lane, not the product, so this path degrades instead
of failing closed: it records a PIPELINE-FAULT (surfaced later in the
report's Pipeline notes) and rewrites `critique` to a neutral note, then
continues to `reflexion_gate` — the stage critique's own `next` pointed to.

The rewritten note deliberately contains NO "VERDICT:" line:
reflexion_gate's parse_verdict defaults to PASS when no verdict line is
found (its documented malformed-critique behavior), so a dead critique
proceeds to synthesis instead of burning the revision budget on a fault.
"""

import json
import os

MAX_DETAIL_CHARS = 500

CRITIQUE_NOTE = (
    "Critique unavailable — the critique stage failed (pipeline fault); "
    "proceeding to synthesis without an automated review pass."
)


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    faults = [f for f in (state.get("pipeline_faults") or []) if isinstance(f, str)]
    raw = state.get("critique")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if not detail.startswith("LLM node"):
        detail = "the critique stage died without recording a failure detail"
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    faults.append(f"PIPELINE-FAULT: critique failed — {detail}")
    print(json.dumps({"pipeline_faults": faults, "critique": CRITIQUE_NOTE}))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(
        json.dumps(
            {
                "pipeline_faults": [
                    f"PIPELINE-FAULT: critique failed — fault-marker script error: {e}"
                ],
                "critique": CRITIQUE_NOTE,
            }
        )
    )
