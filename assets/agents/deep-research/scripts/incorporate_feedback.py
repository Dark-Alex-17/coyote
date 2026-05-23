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
"""
import json
import os


def load_state():
    path = os.environ.get("GRAPH_STATE_FILE")
    if path:
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


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


if __name__ == "__main__":
    main()
