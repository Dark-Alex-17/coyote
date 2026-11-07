#!/usr/bin/env python3
"""Fail-closed marker for a dead parse stage of the code-reviewer graph.

Reached ONLY via parse's `fallback:` route. parse's state_updates wrote an
"LLM node …failed: <chain>" string into `parse_failure` before routing here;
this node emits a complete PIPELINE-FAULT `verdict_out` and the graph jumps
straight to render — a review that could not even parse its request never
reads as MERGE-READY. render.py renders a PIPELINE-FAULT verdict despite the
empty changed-file list (its "No changes to review." early return skips
fault-reason verdicts).
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

ATTENTION_LINE = (
    "PIPELINE-FAULT: parse failed — the review request could not be parsed; "
    "the review did not run"
)


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def fault_verdict(detail):
    return {
        "verdict": "NEEDS-HUMAN",
        "reason": f"PIPELINE-FAULT: parse failed — {detail}",
        "counts": {},
        "deferred_count": 0,
        "dropped_count": 0,
        "dropped_titles": [],
        "attention": [ATTENTION_LINE],
        "findings_final": [],
    }


def main():
    state = load_state()
    raw = state.get("parse_failure")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if not detail.startswith("LLM node"):
        detail = "the parse stage died without recording a failure detail"
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    print(json.dumps({"verdict_out": fault_verdict(detail)}))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(json.dumps({"verdict_out": fault_verdict(f"fault-marker script error: {e}")}))
