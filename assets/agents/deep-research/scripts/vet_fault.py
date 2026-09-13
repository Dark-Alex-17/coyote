#!/usr/bin/env python3
"""Degrade-visibly marker for a dead source-vetting stage of deep-research.

Reached ONLY via vet_sources' `fallback:` route. Grounded in engine
behavior: on llm-node failure the engine writes "LLM node …failed: <chain>"
into `source_assessment` via the node's state_updates BEFORE routing to the
fallback. Vetting is a quality lane, not the product, so this path degrades
instead of failing closed: it records a PIPELINE-FAULT (surfaced later in
the report's Pipeline notes) and rewrites `source_assessment` to a neutral
"unvetted" note so downstream prompts (critique, synthesize) never carry
raw engine error text, then continues to `critique` — the stage
vet_sources' own `next` pointed to.
"""

import json
import os

MAX_DETAIL_CHARS = 500

UNVETTED_NOTE = (
    "Source credibility assessment unavailable — the vetting stage failed "
    "(pipeline fault); treat every cited source as unvetted and needing "
    "corroboration."
)


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    faults = [f for f in (state.get("pipeline_faults") or []) if isinstance(f, str)]
    raw = state.get("source_assessment")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if not detail.startswith("LLM node"):
        detail = "the vetting stage died without recording a failure detail"
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    faults.append(f"PIPELINE-FAULT: source vetting failed — {detail}")
    print(json.dumps({"pipeline_faults": faults, "source_assessment": UNVETTED_NOTE}))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(
        json.dumps(
            {
                "pipeline_faults": [
                    f"PIPELINE-FAULT: source vetting failed — "
                    f"fault-marker script error: {e}"
                ],
                "source_assessment": UNVETTED_NOTE,
            }
        )
    )
