#!/usr/bin/env python3
"""Fail-closed fault marker for the adversary graph.

Reached ONLY via `fallback:` routes (parse, facts, run_checks, debris_lint). Appends a
machine-readable "PIPELINE-FAULT:" marker to `pipeline_faults` and continues
to `verdict`, which turns any recorded fault into DIVERGES — the graph never
dies without its sentinel, and a degraded run can never read as CONFORMS.

Stage attribution -- grounded in engine behavior. A SCRIPT node's fallback
route records `last_script_error` as "script '<node>' failed: <err>"
(src/graph/executor.rs script arm), which names the dead stage directly and
is read first. The presence heuristics below remain the fallback for a state
without it (an llm-node death, or an engine that predates the key):
- parse (llm): its `state_updates` wrote an "LLM node ...failed: <chain>"
  string into `parse_failure` -- attribute to parse.
- debris_lint vs the rest (scripts): run_checks.py writes a non-empty
  `exec_results` (a record list or a string marker) on every path, so a
  truthy `exec_results` proves it completed and the dead stage is the lint.
- run_checks vs facts (scripts): diff_facts.py unconditionally writes a
  NONEMPTY `diff_text` (even its internal-failure path writes
  "(diff unavailable)"), while the graph's initial value is ''. A nonempty
  `diff_text` proves facts completed, so the fault is the verification
  runner (run_checks killed at the node level, e.g. by the node timeout);
  an empty `diff_text` means facts itself died.
"""

import json
import os
import re

MAX_DETAIL_CHARS = 500
SCRIPT_ERROR_RE = re.compile(r"^script '([^']+)' failed: (.*)$", re.S)
STAGE_FAULTS = {
    "debris_lint": (
        "debris lint killed -- the debris_lint stage died at the node level; "
        "implementation-debris findings are unknown"
    ),
    "run_checks": (
        "verification runner killed -- the run_checks stage died at the node "
        "level; execution criteria are unproven"
    ),
    "facts": (
        "diff resolution failed -- changed files and diff text could not be "
        "resolved; criterion verification never ran"
    ),
}


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def clip(detail):
    detail = detail.strip().replace("\n", " ")
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 3] + "..."
    return detail


def engine_attributed_fault(state):
    """The marker for the stage `last_script_error` names, or None without one."""
    err = state.get("last_script_error")
    if not isinstance(err, str) or not (m := SCRIPT_ERROR_RE.match(err.strip())):
        return None
    node, detail = m.group(1), clip(m.group(2))
    reason = STAGE_FAULTS.get(node, f"script stage '{node}' died at the node level")
    return f"PIPELINE-FAULT: {reason} ({detail})"


def main():
    state = load_state()
    faults = [f for f in (state.get("pipeline_faults") or []) if isinstance(f, str)]
    parse_failure = state.get("parse_failure")
    detail = parse_failure.strip() if isinstance(parse_failure, str) else ""
    diff_text = state.get("diff_text")
    facts_completed = isinstance(diff_text, str) and bool(diff_text.strip())
    run_checks_completed = bool(state.get("exec_results"))
    if engine := engine_attributed_fault(state):
        faults.append(engine)
    elif detail.startswith("LLM node"):
        detail = detail.replace("\n", " ")
        if len(detail) > MAX_DETAIL_CHARS:
            detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
        faults.append(f"PIPELINE-FAULT: parse failed — cannot review: {detail}")
    elif run_checks_completed:
        faults.append(
            "PIPELINE-FAULT: debris lint killed -- the debris_lint stage died at "
            "the node level after run_checks recorded exec_results; "
            "implementation-debris findings are unknown"
        )
    elif facts_completed:
        faults.append(
            "PIPELINE-FAULT: verification runner killed — the run_checks stage "
            "died at the node level before recording exec_results; execution "
            "criteria are unproven"
        )
    else:
        faults.append(
            "PIPELINE-FAULT: diff resolution failed — changed files and diff text "
            "could not be resolved; criterion verification never ran"
        )
    print(json.dumps({"pipeline_faults": faults}))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fault marker itself must never crash
    print(
        json.dumps(
            {
                "pipeline_faults": [
                    f"PIPELINE-FAULT: fault-marker script error: {e} — treat the "
                    "run as degraded"
                ]
            }
        )
    )
