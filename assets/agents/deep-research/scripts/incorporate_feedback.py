#!/usr/bin/env python3
"""Fold a reviewer's free-form feedback back into the research loop.

Runs when the user answers the approval step with their own text
instead of "accept" or "reject". That text (saved by the approval node
as `decision`) becomes `research_feedback`, and the graph loops back to
`research_each_question` for another informed pass (each sub-question is
re-researched in parallel with the new feedback in context). The
reflexion counter is reset so the user-driven pass gets a fresh revision
budget.

Routing (`_next`): always research_each_question.

Fail-safe (R3): a crash here means the user's feedback text cannot be
recovered, but their INTENT — another research pass — is unambiguous
(they chose neither accept nor reject). The degraded route therefore
still loops to `research_each_question`, with a generic feedback note
naming the loss and a PIPELINE-FAULT entry (best-effort preserving
existing faults) that surfaces in the next approval's Pipeline notes.
"""
import json
import os


def load_state():
    path = os.environ.get("GRAPH_STATE_FILE")
    if path:
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def safe_faults():
    try:
        state = load_state()
        return [f for f in (state.get("pipeline_faults") or []) if isinstance(f, str)]
    except Exception:  # noqa: BLE001 — best-effort preservation only
        return []


def main():
    state = load_state()
    feedback = (state.get("decision") or "").strip()
    output = {
        "_next": "research_each_question",
        "research_attempts": 0,
        "research_feedback": (
            "The user reviewed the report and asked for changes. Treat "
            "this as the top priority for the next pass:\n\n" + feedback
        ),
    }
    print(json.dumps(output))


try:
    main()
except Exception as e:  # noqa: BLE001 — a crashed feedback fold must degrade, not die
    faults = safe_faults()
    faults.append(
        f"PIPELINE-FAULT: feedback incorporation crashed — {e}; the reviewer's "
        "feedback text could not be recovered"
    )
    print(
        json.dumps(
            {
                "_next": "research_each_question",
                "research_attempts": 0,
                "research_feedback": (
                    "The user reviewed the report and asked for changes, but "
                    "the feedback text could not be recovered (pipeline "
                    "fault). Re-run the research pass with extra care and "
                    "note this loss."
                ),
                "pipeline_faults": faults,
            }
        )
    )
