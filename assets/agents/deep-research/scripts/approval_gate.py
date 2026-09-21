#!/usr/bin/env python3
"""Autonomy router for deep-research's approval gate.

Runs after verify_sources. When the `autonomy` variable is
'autonomous' (the run was spawned by another agent — architect or
sisyphus research tiers — with no human at the terminal), the report
skips the human approval node and ends accepted: the reflexion loop
already served as the quality gate, and end_autonomous carries the
source check + pipeline notes with the report so the caller sees them.
Any other value (default 'supervised') proceeds to the human approval
gate unchanged.

Routing (`_next`):
  - autonomy == 'autonomous' -> end_autonomous
  - anything else            -> approve

Fail-safe: a crash here fails FORWARD via the node's `next` to
`approve` — the safe, human-gated direction.
"""
import json
import os


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    autonomy = str(state.get("autonomy") or "supervised").strip().lower()
    target = "end_autonomous" if autonomy == "autonomous" else "approve"
    print(json.dumps({"_next": target}))


main()
