#!/usr/bin/env python3
"""Fail-closed marker for a dead parse stage of the finding-verifier graph.

Reached ONLY via parse's `fallback:` route. parse's state_updates wrote an
"LLM node …failed: <chain>" string into `parse_failure` before routing here;
this node rewrites `verdicts` to a single schema-conformant UNVERIFIABLE
entry naming the fault and the graph jumps straight to `done` — the
FINDING_VERIFIER_RESULTS sentinel still renders, so the caller (e.g.
code-reviewer's verify lane) degrades instead of dying. Findings that were
never extracted cannot be verified; the caller's "UNVERIFIABLE = keep the
finding, marked unverified" semantics do the right thing with the entry.
"""

import json
import os

MAX_DETAIL_CHARS = 500


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def fault_verdicts(detail):
    return {
        "verdicts": [
            {
                "id": "pipeline-fault",
                "verdict": "UNVERIFIABLE",
                "evidence": "",
                "note": (
                    "PIPELINE-FAULT: parse failed — findings could not be extracted "
                    f"from the verification request; no finding was verified: {detail}"
                ),
            }
        ]
    }


def main():
    state = load_state()
    raw = state.get("parse_failure")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if not detail.startswith("LLM node"):
        detail = "the parse stage died without recording a failure detail"
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    print(json.dumps(fault_verdicts(detail)))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(json.dumps(fault_verdicts(f"fault-marker script error: {e}")))
