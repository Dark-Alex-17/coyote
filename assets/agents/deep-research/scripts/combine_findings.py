#!/usr/bin/env python3
"""Join the per-question map outputs into a single `findings` string.

The `research_each_question` map writes `question_findings` (an array,
one entry per sub-question, in input order). Downstream nodes
(`vet_sources`, `critique`, `synthesize`) read `{{findings}}` as a
single block, so this script renders the array as a Markdown document
with one section per question.
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
    questions = state.get("questions") or []
    per_question = state.get("question_findings") or []

    sections = []
    for idx, q in enumerate(questions):
        body = per_question[idx] if idx < len(per_question) else ""
        if isinstance(body, dict) or isinstance(body, list):
            body = json.dumps(body, indent=2)
        sections.append(f"## {q}\n\n{body}")

    findings = "\n\n".join(sections) if sections else "No findings gathered."
    print(json.dumps({"findings": findings}))


if __name__ == "__main__":
    main()
