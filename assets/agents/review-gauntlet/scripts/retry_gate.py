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
  - a lane is FAULTED only when its report is missing (the map ran but
    collected nothing — replaced by a synthetic PIPELINE-FAULT marker so
    the verdict gate never reads it as SKIPPED), non-string, the
    lane_fault.py marker, sentinel-less, or carries the nested graph's own
    degraded-run wording; a real verdict (DIVERGES, FAIL, INCONCLUSIVE,
    NEEDS-HUMAN, …) is never re-run — the review's judgment stands;
  - a faulted lane is re-run only while it has attempts left AND the
    gauntlet is inside its wall-clock budget; otherwise it is recorded in
    review_incomplete and its fault flows to the verdict gate as-is;
  - malformed review_incomplete bookkeeping (not a list, or non-str
    entries) keeps its str entries and records a PIPELINE-FAULT in
    signals_error — never silently dropped, never split into lane names;
  - the gate itself must never retry by accident: any internal error
    records a PIPELINE-FAULT in signals_error and falls through to the
    verdict gate, which blocks on it.
"""

import json
import os
import re
import sys
import time

# 69600 (graph timeout) − 2 × 11400 (largest lane envelope) − 1200 (script stages)
# = 45600; 600 s of slack below that.
MAX_RETRY_ELAPSED_SECS = 45000
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

# Duplicated verbatim in the sibling gate script — the two copies must stay identical.
ADVERSARY_DEGRADED_HEADER = re.compile(
    r"Criteria: .*("
    r"degraded run: pipeline fault recorded"
    r"|degraded run: \d+ criterion check\(s\) died"
    r"|verdict computation error:"
    r")"
)


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def adversary_degraded(report):
    # The adversary's verdict.py emits its degraded-run header on the line
    # directly under the FIRST sentinel; anchoring there keeps a report that
    # merely quotes the wording later (observations, complaints) a real verdict.
    lines = report.splitlines()
    for i, ln in enumerate(lines):
        if re.search(r"ADVERSARIAL_REVIEW:\s*(CONFORMS|DIVERGES)", ln):
            nxt = lines[i + 1] if i + 1 < len(lines) else ""
            return bool(ADVERSARY_DEGRADED_HEADER.match(nxt))
    return False


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
        return adversary_degraded(report)
    return False


def is_faulted(lane, report):
    if not isinstance(report, str):
        return True
    if report.strip().startswith("PIPELINE-FAULT:"):
        return True
    if not re.search(SENTINELS[lane], report):
        return True
    return nested_degraded(lane, report)


def attempt_count(attempts, lane):
    try:
        return int(attempts.get(lane) or 0)
    except (TypeError, ValueError):
        return 0


def main():
    state = load_state()
    prior = state.get("signals_error")
    signals_error = prior if isinstance(prior, str) else ""
    crash_context["signals_error"] = signals_error
    out = {}
    declared = state.get("review_incomplete")
    if isinstance(declared, list):
        incomplete = [name for name in declared if isinstance(name, str)]
        malformed = len(incomplete) != len(declared)
    else:
        incomplete = []
        malformed = declared is not None
    if malformed:
        fault = (
            "PIPELINE-FAULT: malformed review_incomplete bookkeeping "
            f"({repr(declared)[:80]})"
        )
        signals_error = f"{signals_error}; {fault}" if signals_error else fault
        crash_context["signals_error"] = signals_error
        out["signals_error"] = signals_error
    crash_context["review_incomplete"] = incomplete
    raw_attempts = state.get("lane_attempts")
    attempts = dict(raw_attempts) if isinstance(raw_attempts, dict) else {}
    kept = dict(state.get("kept_results") or {})
    faulted = []

    for lane, (items_key, results_key) in LANES.items():
        results = state.get(results_key) or []
        if not state.get(items_key):
            if not results and lane in kept:
                out[results_key] = [kept[lane]]
            continue
        report = results[0] if results else None
        if report is None:
            report = f"PIPELINE-FAULT: {lane} lane produced no report"
            out[results_key] = [report]
        kept[lane] = report
        if is_faulted(lane, report):
            faulted.append(lane)
        attempts[lane] = attempt_count(attempts, lane) + 1

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
    prior_notes = state.get("retry_notes")
    notes = [n for n in prior_notes if isinstance(n, str)] if isinstance(prior_notes, list) else []
    for lane in declined:
        notes.append(
            f"lane {lane} faulted and was NOT re-run "
            f"(attempts {attempts.get(lane, '?')}/{MAX_ATTEMPTS}"
            + ("" if within_budget else "; wall-clock budget exhausted")
            + ") — recorded in review_incomplete"
        )
    for lane in retry:
        notes.append(
            f"lane {lane} faulted (missing / sentinel-less / degraded report) — "
            f"re-running: attempt {attempts.get(lane, 0) + 1}/{MAX_ATTEMPTS}"
        )
    if retry or declined:
        # stderr is discarded on success by today's engine but is the
        # forward-compatible live-visibility channel; retry_notes carries the
        # same lines into the final report either way.
        sys.stderr.write("WARN: " + " | ".join(notes[-(len(retry) + len(declined)):]) + "\n")
    out["retry_notes"] = notes
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
