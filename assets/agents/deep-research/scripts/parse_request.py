#!/usr/bin/env python3
"""Entry router for deep-research.

Reads the caller's prompt from state. If it contains a usable research
topic, stores it as `topic` and falls through to the static `next`
(plan). If the prompt is empty, routes to `ask_topic` so the user can
supply one interactively.

Routing (`_next`):
  - prompt present -> (no _next; static next: plan)
  - prompt empty   -> ask_topic
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


if __name__ == "__main__":
    main()
