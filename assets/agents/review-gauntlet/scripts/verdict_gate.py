#!/usr/bin/env python3
"""Deterministic verdict gate for the review gauntlet.

Parses each configured lane's verdict SENTINEL with a regex — no LLM
re-reads, no judgment. The gate's hard rules:

  - a missing sentinel is a LANE FAILURE and blocks — never a pass;
  - a PIPELINE-FAULT lane result (the lane's agent died after retries;
    detected only when the report IS the lane_fault.py marker, i.e. it
    STARTS with it — a real review merely quoting the marker flows to the
    normal rules) or a PIPELINE-FAULT in signals_error (builder/parse
    fault) blocks — a degraded pipeline is never a pass;
  - any 🔴 finding in the code-review report blocks regardless of the
    lane's own verdict line;
  - NEEDS-HUMAN without 🔴 passes but is surfaced as attention required;
  - probe INCONCLUSIVE blocks with an ENVIRONMENT note — it is never
    treated as PASS or FAIL (fix the environment per the plan's local-run
    recipe, then re-run);
  - the gate itself must never crash into a silent pass: any internal
    error emits GAUNTLET: BLOCKED naming the error.
"""

import json
import os
import re

def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    blockers, attention, lane_lines, reports = [], [], [], []

    def lane_report(key):
        arr = state.get(key) or []
        if not arr:
            return None
        item = arr[0]
        return item if isinstance(item, str) else json.dumps(item)

    def record(lane, status, detail=""):
        lane_lines.append(f"| {lane} | {status} | {detail} |")

    def fault_excerpt(report):
        line = next(
            (ln for ln in report.splitlines() if "PIPELINE-FAULT:" in ln), report
        )
        return line.strip()[:200]

    def is_lane_fault(report):
        # A genuine lane fault is the ENTIRE report emitted by lane_fault.py,
        # which always begins with the marker. Matching the marker anywhere
        # would let a real review that merely QUOTES "PIPELINE-FAULT:" bypass
        # 🔴 counting and sentinel parsing.
        return report.strip().startswith("PIPELINE-FAULT:")

    def lane_fault_blocker(lane, report):
        blockers.append(
            f"{lane}: PIPELINE-FAULT — the lane failed after retries and produced "
            f"no verdict; a degraded lane is never a pass ({fault_excerpt(report)})"
        )
        record(lane, "BLOCKED", "PIPELINE-FAULT (lane failed)")

    # --- code-review ------------------------------------------------------
    cr = lane_report("code_review_results")
    if cr is None:
        record("code-review", "SKIPPED", "not selected")
    elif is_lane_fault(cr):
        lane_fault_blocker("code-review", cr)
        reports.append(("code-review", cr))
    else:
        reports.append(("code-review", cr))
        m = re.search(r"Verdict:\**\s*\**\s*(MERGE-READY|NEEDS-HUMAN)", cr)
        reds = cr.count("🔴")
        if reds:
            blockers.append(
                f"code-review: {reds} 🔴 CRITICAL finding(s) — fix before claiming done"
            )
            record("code-review", "BLOCKED", f"{reds} 🔴 finding(s)")
        elif m and m.group(1) == "MERGE-READY":
            record("code-review", "GREEN", "MERGE-READY")
        elif m:
            attention.append(
                "code-review: NEEDS-HUMAN — see the lane report's "
                "'Human attention required' section"
            )
            record("code-review", "GREEN (attention)", "NEEDS-HUMAN, no 🔴")
        else:
            blockers.append(
                "code-review: no Review Verdict sentinel found — the lane failed; "
                "a missing verdict is never a pass"
            )
            record("code-review", "BLOCKED", "missing verdict sentinel")

    # --- adversary --------------------------------------------------------
    adv = lane_report("adversary_results")
    if adv is None:
        record("adversary", "SKIPPED", "not selected")
    elif is_lane_fault(adv):
        lane_fault_blocker("adversary", adv)
        reports.append(("adversary", adv))
    else:
        reports.append(("adversary", adv))
        m = re.search(r"ADVERSARIAL_REVIEW:\s*(CONFORMS|DIVERGES)", adv)
        if m and m.group(1) == "CONFORMS":
            record("adversary", "GREEN", "CONFORMS")
        elif m:
            blockers.append(
                "adversary: DIVERGES — the implementation does not conform to the plan"
            )
            record("adversary", "BLOCKED", "DIVERGES")
        else:
            blockers.append(
                "adversary: no ADVERSARIAL_REVIEW sentinel found — lane failed; never a pass"
            )
            record("adversary", "BLOCKED", "missing verdict sentinel")

    # --- security ---------------------------------------------------------
    sec = lane_report("security_results")
    if sec is None:
        record("security", "SKIPPED", "not selected")
    elif is_lane_fault(sec):
        lane_fault_blocker("security", sec)
        reports.append(("security", sec))
    else:
        reports.append(("security", sec))
        m = re.search(r"SECURITY_REVIEW:\s*(PASS|FAIL)", sec)
        if m and m.group(1) == "PASS":
            record("security", "GREEN", "PASS")
        elif m:
            blockers.append("security: SECURITY_REVIEW: FAIL — exploitable finding(s)")
            record("security", "BLOCKED", "FAIL")
        else:
            blockers.append(
                "security: no SECURITY_REVIEW sentinel found — lane failed; never a pass"
            )
            record("security", "BLOCKED", "missing verdict sentinel")

    # --- probe ------------------------------------------------------------
    pb = lane_report("probe_results")
    if pb is None:
        record("probe", "SKIPPED", "not selected")
    elif is_lane_fault(pb):
        lane_fault_blocker("probe", pb)
        reports.append(("probe", pb))
    else:
        reports.append(("probe", pb))
        m = re.search(r"USAGE_PROBE:\s*(PASS|FAIL|INCONCLUSIVE)", pb)
        if m and m.group(1) == "PASS":
            record("probe", "GREEN", "PASS")
        elif m and m.group(1) == "FAIL":
            blockers.append("probe: USAGE_PROBE: FAIL — observed behavior contradicts the contract")
            record("probe", "BLOCKED", "FAIL")
        elif m:
            blockers.append(
                "probe: INCONCLUSIVE — the ENVIRONMENT could not be established; this is "
                "not a code verdict. Follow the plan's local-run recipe to fix the "
                "environment and re-run. INCONCLUSIVE is never treated as PASS or FAIL."
            )
            record("probe", "BLOCKED", "INCONCLUSIVE (environment)")
        else:
            blockers.append(
                "probe: no USAGE_PROBE sentinel found — lane failed; never a pass"
            )
            record("probe", "BLOCKED", "missing verdict sentinel")

    # A gauntlet-level fault (build_items crash, dead parse stage) means the
    # lanes above were never built — SKIPPED rows alone must not read as PASS.
    signals_error = state.get("signals_error")
    if isinstance(signals_error, str) and "PIPELINE-FAULT:" in signals_error:
        blockers.append(f"pipeline: {signals_error}")

    verdict = "BLOCKED" if blockers else "PASS"

    md = "\n".join(
        ["| lane | status | detail |", "|------|--------|--------|"] + lane_lines
    )
    md += "\n\n## Lane selection\n" + (state.get("lanes_summary") or "(none recorded)")
    if blockers:
        md += "\n\n## Blockers\n" + "\n".join(f"- {b}" for b in blockers)
    if attention:
        md += "\n\n## Human attention required\n" + "\n".join(f"- {a}" for a in attention)
    if state.get("signals_error"):
        md += f"\n\n> {state['signals_error']}"
    md += "\n\n## Full lane reports\n"
    for lane, rep in reports:
        md += f"\n<details><summary>{lane}</summary>\n\n{rep}\n\n</details>\n"

    print(json.dumps({"gauntlet_verdict": verdict, "gauntlet_report": md}))


try:
    main()
except Exception as e:  # noqa: BLE001 — the gate must never crash into a silent pass
    print(
        json.dumps(
            {
                "gauntlet_verdict": "BLOCKED",
                "gauntlet_report": f"verdict gate error: {e} — treat as a failed gate, not a pass.",
            }
        )
    )
