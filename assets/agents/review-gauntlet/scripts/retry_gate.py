#!/usr/bin/env python3
"""Deterministic fault-only re-run gate for the review gauntlet.

Sits between the four lane maps and the verdict gate. Every lane map runs
on every pass — a lane that is not being re-run maps over [] — so a re-run
pass overwrites the untouched lanes' collected results with []. The gate's
hard rules:

  - every lane that ran this pass is stashed in kept_results, completed
    and faulted alike, and a lane whose map ran nothing this pass gets
    its stashed report restored — the verdict gate always sees each
    lane's LAST report;
  - a lane is FAULTED only when its report is missing, non-string, the
    lane_fault.py marker, sentinel-less, or carries the nested graph's own
    degraded-run wording; a real verdict (DIVERGES, FAIL, INCONCLUSIVE,
    NEEDS-HUMAN, …) is never re-run — the review's judgment stands;
  - a faulted lane is re-run only while it has attempts left AND the
    gauntlet is inside its wall-clock budget; otherwise it is recorded in
    review_incomplete and its fault flows to the verdict gate as-is;
  - the gate itself must never retry by accident: any internal error
    records a PIPELINE-FAULT in signals_error and falls through to the
    verdict gate, which blocks on it.
"""

import json
import os
import re
import time

MAX_RETRY_ELAPSED_SECS = 45600
MAX_ATTEMPTS = 3

# lane name -> (items key, results key)
LANES = {
    "code-review": ("code_review_items", "code_review_results"),
    "adversary": ("adversary_items", "adversary_results"),
    "security": ("security_items", "security_results"),
    "probe": ("probe_items", "probe_results"),
}

# Must stay identical to verdict_gate.py's sentinel regexes: a lane this
# gate calls completed must be one the verdict gate can parse, or a drift
# would re-run lanes the verdict gate accepts (or accept lanes it cannot
# read). A cross-pin test compares the literals in both files.
SENTINELS = {
    "code-review": r"Verdict:\**\s*\**\s*(MERGE-READY|NEEDS-HUMAN)",
    "adversary": r"ADVERSARIAL_REVIEW:\s*(CONFORMS|DIVERGES)",
    "security": r"SECURITY_REVIEW:\s*(PASS|FAIL)",
    "probe": r"USAGE_PROBE:\s*(PASS|FAIL|INCONCLUSIVE)",
}

# Filled by main() so the crash guard can append to an upstream fault
# instead of overwriting it, and report the lanes it had already declined.
crash_context = {"signals_error": "", "review_incomplete": []}


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def nested_degraded(lane, report):
    # The nested graphs complete with a sentinel even when a stage inside
    # them died; their own degraded-run wording marks a partial review,
    # which is not a completed one. Same anchoring as verdict_gate.py.
    if lane == "code-review":
        verdict_line = re.search(r"^\*\*Verdict: .*$", report, re.MULTILINE)
        return bool(
            verdict_line
            and re.search(
                r"^\*\*Verdict: NEEDS-HUMAN\*\* — .*"
                r"(pipeline fault\(s\) recorded|PIPELINE-FAULT:|report rendering error)",
                verdict_line.group(0),
            )
        )
    if lane == "adversary":
        return bool(
            re.search(
                r"^Criteria: .*degraded run: pipeline fault recorded",
                report,
                re.MULTILINE,
            )
        )
    return False


def is_faulted(lane, report):
    if not isinstance(report, str):
        return True
    if report.strip().startswith("PIPELINE-FAULT:"):
        return True
    if not re.search(SENTINELS[lane], report):
        return True
    return nested_degraded(lane, report)


def main():
    state = load_state()
    prior = state.get("signals_error")
    crash_context["signals_error"] = prior if isinstance(prior, str) else ""
    incomplete = list(state.get("review_incomplete") or [])
    crash_context["review_incomplete"] = incomplete
    attempts = dict(state.get("lane_attempts") or {})
    kept = dict(state.get("kept_results") or {})
    out = {}
    faulted = []

    for lane, (items_key, results_key) in LANES.items():
        results = state.get(results_key) or []
        if not state.get(items_key):
            if not results and lane in kept:
                out[results_key] = [kept[lane]]
            continue
        report = results[0] if results else None
        if report is not None:
            kept[lane] = report
        if is_faulted(lane, report):
            faulted.append(lane)
        attempts[lane] = int(attempts.get(lane) or 0) + 1

    # A missing/zero stamp reads as an ancient start and declines every
    # retry — the safe direction when the clock's origin is unknown.
    started = state.get("gauntlet_started_at") or 0
    within_budget = (time.time() - float(started)) <= MAX_RETRY_ELAPSED_SECS
    retry = [lane for lane in faulted if within_budget and attempts[lane] < MAX_ATTEMPTS]
    declined = [lane for lane in faulted if lane not in retry]
    incomplete = sorted(set(incomplete) | set(declined))
    crash_context["review_incomplete"] = incomplete

    out.update(
        {
            "lane_attempts": attempts,
            "kept_results": kept,
            "retry_lanes": retry,
            "review_incomplete": incomplete,
        }
    )
    if retry:
        out["_next"] = "build_items"
    print(json.dumps(out))


try:
    main()
except Exception as e:  # noqa: BLE001 — a gate crash must fall through, never retry
    fault = f"PIPELINE-FAULT: retry gate crashed — {e}"
    if crash_context["signals_error"]:
        fault = f"{crash_context['signals_error']}; {fault}"
    print(
        json.dumps(
            {
                "retry_lanes": [],
                "review_incomplete": crash_context["review_incomplete"],
                "signals_error": fault,
            }
        )
    )
