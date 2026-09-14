#!/usr/bin/env python3
"""Deterministic verdict gate for the review gauntlet.

Parses each configured lane's verdict SENTINEL with a regex — no LLM
re-reads, no judgment. The gate's hard rules:

  - a missing sentinel is a LANE FAILURE and blocks — never a pass;
  - a non-string lane result (the map collected an object instead of the
    lane's rendered text) blocks — it cannot carry a sentinel;
  - a PIPELINE-FAULT lane result (the lane's agent died after retries;
    detected only when the report IS the lane_fault.py marker or
    retry_gate.py's no-report marker, i.e. it STARTS with it — a real
    review merely quoting the marker flows to the normal rules) or a
    PIPELINE-FAULT in signals_error (builder/parse fault) blocks — a
    degraded pipeline is never a pass;
  - a code-review lane whose OWN verdict line records a pipeline fault
    (code-reviewer's verdict.py / render.py degraded-run wording) blocks —
    the nested graph completed, but with a dead domain lane, verifier, or
    synthesis behind it, so its critical count is not trustworthy;
  - an adversary lane whose OWN header line (the line directly under its
    sentinel) records a degraded run — a pipeline fault, died criterion
    checks, or its verdict script's crash stub — blocks as a PIPELINE-FAULT,
    not as a DIVERGES finding: the nested graph completed with a dead stage
    behind it, so its verdict is not a judgment on the code;
  - a non-zero critical count in the code-review report blocks regardless
    of the lane's own verdict line — derived from render.py's deterministic
    summary line (`*Reviewed N files, found X critical, …*`); the raw 🔴
    count is used only when that summary line is missing;
  - NEEDS-HUMAN without 🔴 passes but is surfaced as attention required;
  - probe INCONCLUSIVE blocks with an ENVIRONMENT note — it is never
    treated as PASS or FAIL (fix the environment per the plan's local-run
    recipe, then re-run);
  - a PIPELINE-FAULT lane row carries its attempt count (` ×N`) when retry_gate
    re-ran it, so the reader can tell a first-pass death from an exhausted
    retry budget;
  - an incomplete review — lanes retry_gate gave up on (review_incomplete)
    or a pipeline-level fault (reported under the `pipeline` pseudo-lane) —
    emits the GAUNTLET_REVIEW_INCOMPLETE: machine line and a "Review
    incomplete" section: that is a fault to re-run, never a code finding.
    Every lane named in review_incomplete is forced to a BLOCKED row, so a
    restored earlier report can never turn an incomplete review into a
    pass: a non-empty machine line structurally implies GAUNTLET: BLOCKED.
    Malformed review_incomplete bookkeeping (not a list, or non-str
    entries) is itself recorded as a `pipeline` fault, never dropped;
  - the gate itself must never crash into a silent pass: any internal
    error emits GAUNTLET: BLOCKED naming the error, with the pipeline
    marked incomplete.
"""

import json
import os
import re

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


def main():
    state = load_state()
    blockers, attention, reports = [], [], []
    rows = {}
    malformed = set()
    lane_attempts = state.get("lane_attempts")
    if not isinstance(lane_attempts, dict):
        lane_attempts = {}

    def lane_report(key):
        arr = state.get(key) or []
        if not arr:
            return None
        item = arr[0]
        if isinstance(item, str):
            return item
        malformed.add(key)
        return json.dumps(item)

    def record(lane, status, detail=""):
        rows[lane] = (status, detail)

    def attempt_count(lane):
        try:
            return int(lane_attempts.get(lane) or 0)
        except (TypeError, ValueError):
            return 0

    def attempts_suffix(lane):
        n = attempt_count(lane)
        return f" ×{n}" if n > 1 else ""

    def fault_excerpt(report):
        line = next(
            (ln for ln in report.splitlines() if "PIPELINE-FAULT:" in ln), report
        )
        return line.strip()[:200]

    def is_lane_fault(report):
        # A genuine lane fault is the ENTIRE report — the lane_fault.py marker
        # or retry_gate.py's no-report marker — which always begins with
        # "PIPELINE-FAULT:". Matching the marker anywhere would let a real
        # review that merely QUOTES it bypass 🔴 counting and sentinel
        # parsing.
        return report.strip().startswith("PIPELINE-FAULT:")

    def lane_fault_blocker(lane, report):
        blockers.append(
            f"{lane}: PIPELINE-FAULT — the lane failed after retries and produced "
            f"no verdict; a degraded lane is never a pass ({fault_excerpt(report)})"
        )
        record(lane, "BLOCKED", f"PIPELINE-FAULT (lane failed{attempts_suffix(lane)})")

    def malformed_blocker(lane, report):
        blockers.append(f"{lane}: malformed lane result (non-string item) — never a pass")
        record(lane, "BLOCKED", "malformed lane result (non-string)")
        reports.append((lane, report))

    # --- code-review ------------------------------------------------------
    cr = lane_report("code_review_results")
    if cr is None:
        record("code-review", "SKIPPED", "not selected")
    elif "code_review_results" in malformed:
        malformed_blocker("code-review", cr)
    elif is_lane_fault(cr):
        lane_fault_blocker("code-review", cr)
        reports.append(("code-review", cr))
    else:
        reports.append(("code-review", cr))
        m = re.search(r"Verdict:\**\s*\**\s*(MERGE-READY|NEEDS-HUMAN)", cr)
        # code-reviewer's verdict.py forces NEEDS-HUMAN on its own internal
        # faults; render.py emits that as `**Verdict: NEEDS-HUMAN** — <reason>`
        # (CONTRACT comments at both emitters). Scoped to the FIRST rendered
        # verdict line — render.py emits it before any finding body — so a
        # finding that quotes the wording cannot trip it.
        verdict_line = re.search(r"^\*\*Verdict: .*$", cr, re.MULTILINE)
        m_fault = verdict_line and re.search(
            r"^\*\*Verdict: NEEDS-HUMAN\*\* — .*"
            r"(pipeline fault\(s\) recorded|PIPELINE-FAULT:|report rendering error)",
            verdict_line.group(0),
        )
        # Same quoted-marker class as the PIPELINE-FAULT prefix anchoring: a
        # 🔴 quoted inside finding text or a Changes-table row is not a
        # finding. render.py's summary line is the authoritative count; the
        # raw count is the fail-closed fallback when that line is absent. The
        # LAST match wins: render.py emits the line after every finding body,
        # so a quoted look-alike earlier in the report cannot shadow it. The
        # pattern is the FULL shape of render.py's line (CONTRACT comment
        # there) so a partial look-alike in LLM-authored text never matches.
        sums = re.findall(
            r"^\*Reviewed \d+ files, found (\d+) critical, \d+ warnings, \d+ suggestions, "
            r"\d+ nitpicks \(\d+ deferred by quality bar\)\*$",
            cr,
            re.MULTILINE,
        )
        reds = int(sums[-1]) if sums else cr.count("🔴")
        if m_fault:
            blockers.append(
                "code-review: the lane reported an internal PIPELINE-FAULT "
                "(degraded review) — a degraded lane is never a pass"
            )
        if reds:
            blockers.append(
                f"code-review: {reds} 🔴 CRITICAL finding(s) — fix before claiming done"
            )
        suffix = attempts_suffix("code-review")
        if m_fault and reds:
            record("code-review", "BLOCKED", f"PIPELINE-FAULT (degraded lane{suffix}); {reds} 🔴")
        elif m_fault:
            record("code-review", "BLOCKED", f"PIPELINE-FAULT (degraded lane{suffix})")
        elif reds:
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
    elif "adversary_results" in malformed:
        malformed_blocker("adversary", adv)
    elif is_lane_fault(adv):
        lane_fault_blocker("adversary", adv)
        reports.append(("adversary", adv))
    else:
        reports.append(("adversary", adv))
        m = re.search(r"ADVERSARIAL_REVIEW:\s*(CONFORMS|DIVERGES)", adv)
        if adversary_degraded(adv):
            blockers.append(
                "adversary: PIPELINE-FAULT — degraded run, pipeline fault recorded "
                "inside the adversary; a degraded lane is never a pass"
            )
            record(
                "adversary",
                "BLOCKED",
                f"PIPELINE-FAULT (degraded lane{attempts_suffix('adversary')})",
            )
        elif m and m.group(1) == "CONFORMS":
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
    elif "security_results" in malformed:
        malformed_blocker("security", sec)
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
    elif "probe_results" in malformed:
        malformed_blocker("probe", pb)
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

    # retry_gate restores an exhausted lane's LAST report, which may be an
    # earlier clean verdict — an incomplete lane must block regardless of
    # what its row parsed to.
    declared = state.get("review_incomplete")
    incomplete = set()
    malformed_bookkeeping = None
    if isinstance(declared, list):
        incomplete = {name for name in declared if isinstance(name, str)}
        if any(not isinstance(name, str) for name in declared):
            malformed_bookkeeping = repr(declared)[:80]
    elif declared is not None:
        malformed_bookkeeping = repr(declared)[:80]
    for lane in sorted(incomplete - {"pipeline"}):
        if rows.get(lane, ("", ""))[0] == "BLOCKED":
            continue
        after = f" after {n} attempt(s)" if (n := attempt_count(lane)) else ""
        blockers.append(
            f"{lane}: review incomplete{after} — the lane never produced a usable verdict"
        )
        record(lane, "BLOCKED", f"review incomplete{after}")

    # A gauntlet-level fault (build_items crash, dead parse stage) means the
    # lanes above were never built — SKIPPED rows alone must not read as PASS.
    signals_error = state.get("signals_error")
    pipeline_fault = isinstance(signals_error, str) and "PIPELINE-FAULT:" in signals_error
    if pipeline_fault:
        blockers.append(f"pipeline: {signals_error}")
        incomplete.add("pipeline")
    if malformed_bookkeeping is not None:
        blockers.append(
            "pipeline: PIPELINE-FAULT — malformed review_incomplete bookkeeping "
            f"({malformed_bookkeeping}); never a pass"
        )
        incomplete.add("pipeline")
    if "pipeline" in incomplete and not any(b.startswith("pipeline:") for b in blockers):
        blockers.append(
            "pipeline: review incomplete — a pipeline stage never completed "
            "(no fault text recorded)"
        )

    verdict = "BLOCKED" if blockers or incomplete else "PASS"

    incomplete_lines = []
    for name in sorted(incomplete):
        if name == "pipeline":
            incomplete_lines.append(
                f"- pipeline: {signals_error}"
                if pipeline_fault
                else "- pipeline: review incomplete (no fault text recorded)"
            )
        elif n := attempt_count(name):
            incomplete_lines.append(f"- {name}: no completed review after {n} attempt(s)")
        else:
            incomplete_lines.append(f"- {name}: no completed review")
    incomplete_line = (
        "\nGAUNTLET_REVIEW_INCOMPLETE: " + ", ".join(sorted(incomplete))
        if incomplete
        else ""
    )

    md = "\n".join(
        ["| lane | status | detail |", "|------|--------|--------|"]
        + [f"| {lane} | {status} | {detail} |" for lane, (status, detail) in rows.items()]
    )
    md += "\n\n## Lane selection\n" + (state.get("lanes_summary") or "(none recorded)")
    if blockers:
        md += "\n\n## Blockers\n" + "\n".join(f"- {b}" for b in blockers)
    if attention:
        md += "\n\n## Human attention required\n" + "\n".join(f"- {a}" for a in attention)
    if state.get("signals_error"):
        md += f"\n\n> {state['signals_error']}"
    if incomplete_lines:
        md += "\n\n## Review incomplete\n" + "\n".join(incomplete_lines)
    md += "\n\n## Full lane reports\n"
    for lane, rep in reports:
        md += f"\n<details><summary>{lane}</summary>\n\n{rep}\n\n</details>\n"

    print(
        json.dumps(
            {
                "gauntlet_verdict": verdict,
                "gauntlet_report": md,
                "gauntlet_incomplete_line": incomplete_line,
            }
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 — the gate must never crash into a silent pass
    print(
        json.dumps(
            {
                "gauntlet_verdict": "BLOCKED",
                "gauntlet_report": f"verdict gate error: {e} — treat as a failed gate, not a pass.",
                "gauntlet_incomplete_line": "\nGAUNTLET_REVIEW_INCOMPLETE: pipeline",
            }
        )
    )
