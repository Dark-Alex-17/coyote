#!/usr/bin/env python3
"""Fail-closed marker for a dead verify branch of the finding-verifier graph.

Reached ONLY via verify_one's `fallback:` route inside the map branch.
Grounded in engine behavior: on llm-node failure the engine writes
"LLM node failed: <chain>" into `verdict` via the node's state_updates
BEFORE routing to the fallback, so the raw failure text is already in
state. This node normalizes it into a schema-conformant UNVERIFIABLE
verdict (authoritative id stamped from the finding item, mirroring
verdict_gate.py) and the chain ends there — the map collects a valid
verdict, so a dead verifier lane never sinks the whole map or masquerades
as verified.
"""

import json
import os
import sys

# State, diffs and reports carry emoji and non-ASCII paths; never decode them
# with the platform default (cp1252 on Windows).
for _stream in (sys.stdin, sys.stdout, sys.stderr):
    if hasattr(_stream, "reconfigure"):
        _stream.reconfigure(encoding="utf-8", errors="replace")

MAX_DETAIL_CHARS = 500


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def finding_id(state):
    finding = state.get("finding")
    if isinstance(finding, dict):
        fid = finding.get("id")
        if isinstance(fid, str) and fid.strip():
            return fid.strip()
    return "unknown"


def fault_verdict(fid, detail):
    return {
        "verdict": {
            "id": fid,
            "verdict": "UNVERIFIABLE",
            "evidence": "",
            "note": f"PIPELINE-FAULT: verifier lane failed after retries — {detail}",
        }
    }


def main():
    state = load_state()
    fid = finding_id(state)
    raw = state.get("verdict")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if not detail.startswith("LLM node"):
        detail = "the verifier lane died without recording a failure detail"
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    print(json.dumps(fault_verdict(fid, detail)))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(json.dumps(fault_verdict("unknown", f"fault-marker script error: {e}")))
