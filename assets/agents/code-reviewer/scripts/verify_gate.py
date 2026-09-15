#!/usr/bin/env python3
"""Local verifier-fault retry gate for the code-reviewer graph.

finding-verifier converts its own branch faults into DATA (UNVERIFIABLE
verdicts with a PIPELINE-FAULT: note, or parse_fault's pipeline-fault
sentinel), so the verify agent node completes "successfully" and its
max_attempts never fires. Without this gate the fault escalates all the
way to the gauntlet, which re-runs the ENTIRE code-review lane (~1 h) to
recover a ~4-minute verifier — the wrong trade. This gate re-runs just
the verifier ONCE; if faults persist, verdict.py downgrades them to a
proportionate NEEDS-HUMAN (findings stand, unverified).

Anchored detection only: the JSON-encoded note prefix and the sentinel id
— a finding merely QUOTING the marker mid-text does not trip it. Always
exits 0 with valid JSON; a crash falls forward on the declared edge.
"""
import json
import os
import sys


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


try:
    state = load_state()
    raw = state.get("verifier_output")
    text = raw if isinstance(raw, str) else (json.dumps(raw) if raw is not None else "")
    faulted = (
        text.lstrip().startswith("Agent node failed:")
        or '"note": "PIPELINE-FAULT:' in text
        or "'note': 'PIPELINE-FAULT:" in text
        or '"id": "pipeline-fault"' in text
    )
    try:
        retries = int(state.get("verify_retries") or 0)
    except (TypeError, ValueError):
        retries = 0
    if faulted and retries < 1:
        sys.stderr.write(
            "WARN: verifier output carries pipeline-fault markers — re-running "
            "finding-verifier locally (attempt 2/2) before computing the verdict.\n"
        )
        print(json.dumps({"_next": "verify", "verify_retries": retries + 1}))
    else:
        print(json.dumps({"verify_retries": retries}))
except Exception as e:  # never block the verdict
    print(json.dumps({"verify_gate_error": f"verify_gate crashed: {e}"}))
