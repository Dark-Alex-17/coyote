#!/usr/bin/env python3
"""Fail-closed marker for a dead parse stage of the review gauntlet.

Reached ONLY via parse's `fallback:` route. parse's state_updates wrote an
"LLM node …failed: <chain>" string into `parse_failure` before routing here;
this node records the failure as a PIPELINE-FAULT in `signals_error` and the
graph jumps straight to the verdict gate. Every lane item list is still its
initial [] so every lane records SKIPPED, and verdict_gate's signals_error
rule makes the verdict BLOCKED — a gauntlet that could not even parse its
request never reads as a pass.
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


def main():
    state = load_state()
    raw = state.get("parse_failure")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    print(
        json.dumps(
            {
                "signals_error": "PIPELINE-FAULT: parse failed — cannot run the "
                f"gauntlet: {detail}"
            }
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(
        json.dumps(
            {
                "signals_error": "PIPELINE-FAULT: parse failed — cannot run the "
                f"gauntlet: fault-marker script error: {e}"
            }
        )
    )
