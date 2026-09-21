#!/usr/bin/env python3
"""Fail-closed marker for a dead research lane of the deep-research graph.

Reached ONLY via research_one_question's `fallback:` route inside the
`research_each_question` map branch (branch-local — a map branch's fallback
must stay inside the branch subgraph). Grounded in engine behavior: on
branch failure the engine writes its failure text into `finding` via the
node's state_updates BEFORE routing to the fallback — "Agent node failed:
<chain>" now that the branch spawns a librarian agent, "LLM node …failed:
<chain>" for an llm branch (on success the same key holds the researched
findings, which never start with either prefix). This node normalizes the failure text into a
PIPELINE-FAULT finding and the chain ends here — the map collects the fault
as the lane's `finding`, so one dead research lane never sinks the whole
map. combine_findings later lifts PIPELINE-FAULT findings into
`pipeline_faults` so the fault also surfaces in the report's Pipeline notes.
The question text is part of the fault so distinct dead lanes with identical
engine failure text do not collapse into one entry there.
"""

import json
import os

MAX_DETAIL_CHARS = 500
MAX_QUESTION_CHARS = 200


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def fault_finding(question, detail):
    return {
        "finding": (
            "PIPELINE-FAULT: question research failed — finding unavailable — "
            f'{detail} (question: "{question}")'
        )
    }


def question_label(state):
    raw = state.get("question")
    question = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if not question:
        return "(unknown question)"
    if len(question) > MAX_QUESTION_CHARS:
        question = question[: MAX_QUESTION_CHARS - 1] + "…"
    return question


def main():
    state = load_state()
    raw = state.get("finding")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if not detail.startswith(("LLM node", "Agent node")):
        detail = "the research lane died without recording a failure detail"
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    print(json.dumps(fault_finding(question_label(state), detail)))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(
        json.dumps(
            fault_finding("(unknown question)", f"fault-marker script error: {e}")
        )
    )
