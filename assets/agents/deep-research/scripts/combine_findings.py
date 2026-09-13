#!/usr/bin/env python3
"""Join the per-question map outputs into a single `findings` string.

The `research_each_question` map writes `question_findings` (an array,
one entry per sub-question, in input order). Downstream nodes
(`vet_sources`, `critique`, `synthesize`) read `{{findings}}` as a
single block, so this script renders the array as a Markdown document
with one section per question.

Fault visibility: a research lane that died lands here as a finding
starting with "PIPELINE-FAULT:" (written by question_fault inside the map
branch, where `pipeline_faults` itself is out of reach — map branches only
surface their collected output_key). This script lifts such findings into
`pipeline_faults` (deduplicated, since a reflexion or feedback loop can
re-run the map) so dead lanes surface in the report's Pipeline notes.

Fail-safe (R3): on a crash (e.g. malformed GRAPH_STATE), `findings`
becomes the fault text itself and a PIPELINE-FAULT entry is recorded —
the pipeline proceeds visibly degraded, and the human approval gate is
the backstop. Existing faults are preserved best-effort.
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
    questions = state.get("questions") or []
    per_question = state.get("question_findings") or []
    faults = [f for f in (state.get("pipeline_faults") or []) if isinstance(f, str)]

    sections = []
    for idx, q in enumerate(questions):
        body = per_question[idx] if idx < len(per_question) else ""
        if isinstance(body, dict) or isinstance(body, list):
            body = json.dumps(body, indent=2)
        if isinstance(body, str) and body.startswith("PIPELINE-FAULT:") and body not in faults:
            faults.append(body)
        sections.append(f"## {q}\n\n{body}")

    findings = "\n\n".join(sections) if sections else "No findings gathered."
    print(json.dumps({"findings": findings, "pipeline_faults": faults}))


try:
    main()
except Exception as e:  # noqa: BLE001 — a crashed join must degrade, not die
    faults = safe_faults()
    fault = (
        f"PIPELINE-FAULT: combining findings crashed — {e}; per-question "
        "findings may exist in state but could not be joined"
    )
    faults.append(fault)
    print(json.dumps({"findings": fault, "pipeline_faults": faults}))
