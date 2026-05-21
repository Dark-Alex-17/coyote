#!/usr/bin/env python3
"""Reflexion gate for deep-research.

Runs after `critique` has reviewed the current research findings. If the
critique's verdict is REVISE and the reflexion budget is not spent,
loops back to `research` with the critique attached as
`research_feedback`, so the retry is informed rather than a blind
re-run. Otherwise it proceeds to `synthesize`.

Routing (`_next`):
  - verdict PASS                     -> synthesize
  - verdict REVISE, budget remaining -> research_each_question  (+ research_feedback)
  - verdict REVISE, budget spent     -> synthesize

Reflexion is a best-effort quality booster, not a hard gate: once the
budget is spent the workflow proceeds anyway, and the human approval
step is the final backstop.
"""
import json
import os
import re

# Automated revision passes allowed. `research` runs at most
# MAX_REFLEXION_REVISIONS + 1 times per user pass. Bump to allow more.
MAX_REFLEXION_REVISIONS = 2


def load_state():
    path = os.environ.get("GRAPH_STATE_FILE")
    if path:
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def as_int(value, default=0):
    try:
        return int(value)
    except (TypeError, ValueError):
        return default


def parse_verdict(critique):
    """Pull PASS/REVISE from the critique's `VERDICT:` line. Defaults to
    PASS when no verdict line is found, so a malformed critique lets the
    workflow proceed instead of burning the whole revision budget."""
    match = re.search(r"VERDICT:\s*([A-Za-z]+)", critique, re.IGNORECASE)
    if not match:
        return "PASS"
    return match.group(1).upper()


def main():
    state = load_state()
    critique = state.get("critique") or ""
    verdict = parse_verdict(critique)
    attempts = as_int(state.get("research_attempts"))

    if verdict == "REVISE" and attempts < MAX_REFLEXION_REVISIONS:
        feedback = (
            "A reviewer judged the previous research pass incomplete. "
            "Address every point in the critique below:\n\n" + critique
        )
        output = {
            "_next": "research_each_question",
            "research_attempts": attempts + 1,
            "research_feedback": feedback,
        }
    else:
        output = {"_next": "synthesize"}

    print(json.dumps(output))


if __name__ == "__main__":
    main()
