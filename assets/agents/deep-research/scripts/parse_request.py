#!/usr/bin/env python3
"""Entry router for deep-research.

Reads the caller's prompt from state. If it contains a usable research
topic, stores it as `topic` and falls through to the static `next`
(plan). If the prompt is empty, routes to `ask_topic` so the user can
supply one interactively.

Routing (`_next`):
  - prompt present -> (no _next; static next: plan)
  - prompt empty   -> ask_topic

Fail-safe (R3): a crash here (e.g. malformed GRAPH_STATE) means the
caller's prompt cannot be read at all, so proceeding silently is
meaningless — the degraded route is `ask_topic`, which asks the user for
the topic directly, plus a PIPELINE-FAULT entry so the loss is visible in
the final report's Pipeline notes. Clobbering `pipeline_faults` is safe
here: this is the start node, so the list can only hold its initial [].
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
    prompt = (state.get("initial_prompt") or "").strip()
    if prompt:
        print(json.dumps({"topic": prompt}))
    else:
        print(json.dumps({"_next": "ask_topic"}))


try:
    main()
except Exception as e:  # noqa: BLE001 — a crashed entry router must degrade, not die
    print(
        json.dumps(
            {
                "_next": "ask_topic",
                "pipeline_faults": [
                    f"PIPELINE-FAULT: request parsing crashed — {e}; asked the "
                    "user for the research topic instead"
                ],
            }
        )
    )
