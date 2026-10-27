#!/usr/bin/env python3
"""Deterministic conformance verdict for the adversary graph.

CONFORMS requires: at least one criterion, every criterion MET, zero
holistic complaints, zero pipeline faults, zero recorded red
verification runs (exit≠0/timeout/skipped, or an ENVIRONMENT runner-error
marker when commands were declared — the declared commands never ran, so the
run is unproven), and zero implementation-debris hits from debris_lint.py.
Everything else -- including a missing plan or a degraded (faulted)
pipeline -- is DIVERGES (fail-closed).
Any "PIPELINE-FAULT:" marker recorded in `pipeline_faults` (or a holistic
llm-node failure captured in `holistic_failure`) becomes complaint #1;
per-criterion results are still reported. A criterion whose check DIED
(criterion_fault's UNMET verdict, evidence prefixed DIED_EVIDENCE_PREFIX) is
rendered as died rather than judged and counted as unmet regardless of the
status it carries. Assembles the exact sentinel format the callers route on.
Never crashes into a silent verdict.
"""

import json
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".shared"))

MAX_FAULT_DETAIL_CHARS = 300
MAX_MARKER_CHARS = 200
DIED_MARKER = "criterion check DIED (pipeline fault)"
DIED_EVIDENCE_PREFIX = "PIPELINE-FAULT: criterion "


def died(v):
    evidence = v.get("evidence")
    return isinstance(evidence, str) and evidence.startswith(DIED_EVIDENCE_PREFIX)


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def collect_pipeline_faults(state):
    """Recorded fault markers, plus a holistic llm-node failure if one occurred.

    `holistic_failure` holds the "LLM node …failed: <chain>" string when the
    holistic pass fell back to verdict; on a successful pass it holds the
    rendered schema output (never that prefix).
    """
    faults = [
        f.strip()
        for f in (state.get("pipeline_faults") or [])
        if isinstance(f, str) and f.strip()
    ]
    hf = state.get("holistic_failure")
    if isinstance(hf, str) and hf.strip().startswith("LLM node"):
        detail = hf.strip().replace("\n", " ")
        if len(detail) > MAX_FAULT_DETAIL_CHARS:
            detail = detail[: MAX_FAULT_DETAIL_CHARS - 1] + "…"
        faults.append(
            "PIPELINE-FAULT: holistic pass failed — criterion verdicts stand "
            f"but cross-cutting hunt did not run ({detail})"
        )
    return faults


def append_criterion_complaints(lines, verdicts, extra, start):
    """Numbered complaints for non-MET verdicts and holistic extras."""
    i = start
    for v in verdicts:
        if v.get("status") == "MET" and not died(v):
            continue
        i += 1
        text = (v.get("text") or "").strip().replace("\n", " ")
        if len(text) > 140:
            text = text[:137] + "…"
        complaint = (v.get("complaint") or "").strip()
        if died(v):
            if not complaint.startswith(DIED_MARKER):
                complaint = f"{DIED_MARKER} — {complaint}" if complaint else DIED_MARKER
            lines.append(f'{i}. Acceptance criterion "{text}" — {complaint}')
        else:
            lines.append(
                f'{i}. Acceptance criterion "{text}" — {v.get("status", "?").title()} — '
                f"{complaint}"
            )
    for c in extra:
        i += 1
        lines.append(
            f"{i}. {c.get('kind', 'violation')} — {c.get('location', '?')} — "
            f"{c.get('violation', '')} — {c.get('fix', '')}"
        )
    return i


def declared_nonempty(declared):
    """True when verification_commands names at least one command.

    Accepts a list or a JSON-encoded string of one. None, unparseable, or
    empty declarations count as not declared — run_checks already records
    a PIPELINE-FAULT for the invalid ones.
    """
    if isinstance(declared, str):
        try:
            declared = json.loads(declared)
        except json.JSONDecodeError:
            return False
    if not isinstance(declared, list):
        return False
    return any(isinstance(c, str) and c.strip() for c in declared)


def exec_run_complaints(exec_results, declared):
    """One complaint per red verification run recorded by run_checks.py.

    A red run is a record that was skipped (deadline) or exited nonzero
    (including the -1 TIMEOUT sentinel). An "ENVIRONMENT: …" runner-error
    marker with a non-empty declaration is one complaint: the declared
    commands never ran. Other string markers ("none declared",
    "PIPELINE-FAULT…") are not runs and yield nothing.
    """
    if isinstance(exec_results, str):
        marker = exec_results.strip().replace("\n", " ")
        if not marker.startswith("ENVIRONMENT:") or not declared_nonempty(declared):
            return []
        if len(marker) > MAX_MARKER_CHARS:
            marker = marker[: MAX_MARKER_CHARS - 1] + "…"
        return [
            f"Verification runner error — the declared command(s) never ran ({marker}); "
            "unproven runs block CONFORMS"
        ]
    if not isinstance(exec_results, list):
        return []
    out = []
    for r in exec_results:
        if not isinstance(r, dict):
            continue
        cmd = r.get("cmd", "?")
        if r.get("skipped"):
            out.append(f"Verification run `{cmd}` — never ran: deadline expired (unproven)")
            continue
        code = r.get("exit")
        if code == 0:
            continue
        outcome = "TIMEOUT (exit -1)" if code == -1 else f"exit {code}"
        out.append(
            f"Verification run `{cmd}` — {outcome} (recorded FAIL; a red declared "
            "command blocks CONFORMS regardless of criterion judgments)"
        )
    return out


def render_exec_results(exec_results):
    """Lines for the "Verification runs:" report section.

    exec_results is a list of {cmd, exit, duration_s, tail} records (or
    {cmd, skipped} for commands the runner's total deadline left no budget
    for) from run_checks.py, or a string marker ("none declared —…" /
    "ENVIRONMENT: …"), or empty when the run never reached the stage.
    """
    if isinstance(exec_results, list):
        out = []
        for r in exec_results:
            if not isinstance(r, dict):
                continue
            if r.get("skipped"):
                out.append(
                    f"- [SKIPPED] `{r.get('cmd', '?')}` — never ran: the runner's "
                    "total deadline expired first; covered criteria are unproven"
                )
                continue
            status = "PASS" if r.get("exit") == 0 else "FAIL"
            under = f" (under: {r['under']})" if r.get("under") else ""
            out.append(
                f"- [{status}] `{r.get('cmd', '?')}` — exit {r.get('exit', '?')}, "
                f"{r.get('duration_s', '?')}s"
                + under
            )
            if status == "FAIL":
                tail = (r.get("tail") or "").strip()
                out.extend("    " + t for t in tail.splitlines()[-15:])
        return out or ["- (no verification commands were run)"]
    if isinstance(exec_results, str) and exec_results.strip():
        return [f"- {exec_results.strip()}"]
    return ["- none recorded (the run did not reach the verification stage)"]


def debris_complaints(state):
    """Hit count and rendered lines for the findings debris_lint.py recorded.

    Also reported as the gauntlet's `debris` row when run under it; here each
    hit is a numbered complaint so the lane fails closed on its own. The
    shared module is imported only when there is something to render, and an
    unresolvable `.shared` becomes a complaint rather than the whole report
    collapsing into the crash stub. Only `null` and `[]` mean no findings;
    any other value, a falsy non-list included, is the lint's malformed shape.
    """
    raw = state.get("debris_findings")
    if raw is None or raw == []:
        return 0, []
    try:
        import debris
    except ImportError as e:
        return 1, [f"Implementation debris lint unavailable: {e} -- fail-closed"]
    kind, n = debris.classify_findings(raw)
    if kind != "hits":
        shape = (
            f"list with {n} non-dict item(s)"
            if isinstance(raw, list)
            else f"{type(raw).__name__}, not a list"
        )
        return 1, [f"Implementation debris findings malformed ({shape}) -- fail-closed"]
    return n, debris.render(raw)


def debris_summary_line(state):
    summary = state.get("debris_summary")
    if not isinstance(summary, str) or not summary.strip():
        return "Debris: no summary recorded (the lint did not run)"
    summary = summary.strip()
    if summary.startswith("debris: "):
        summary = summary[len("debris: ") :]
    return f"Debris: {summary}"


def main():
    state = load_state()
    verdicts = [v for v in (state.get("crit_verdicts") or []) if isinstance(v, dict)]
    extra = [c for c in (state.get("extra_complaints") or []) if isinstance(c, dict)]
    observations = (state.get("observations") or "").strip()
    faults = collect_pipeline_faults(state)
    reds = exec_run_complaints(
        state.get("exec_results"), state.get("verification_commands")
    )
    n_debris, debris_hits = debris_complaints(state)
    n = len(verdicts)

    met = [v for v in verdicts if v.get("status") == "MET" and not died(v)]
    partial = [v for v in verdicts if v.get("status") == "PARTIAL" and not died(v)]
    bad = [
        v for v in verdicts if died(v) or v.get("status") in ("UNMET", "DIVERGED")
    ]

    lines = []
    if faults:
        lines.append("ADVERSARIAL_REVIEW: DIVERGES")
        if n:
            lines.append(
                f"Criteria: {len(met)}/{n} met, {len(partial)} partial, "
                f"{len(bad)} unmet/diverged — degraded run: pipeline fault recorded "
                "(fail-closed)."
            )
        else:
            lines.append(
                "Criteria: none verified — degraded run: pipeline fault recorded "
                "(fail-closed)."
            )
        lines.append(debris_summary_line(state))
        lines.append("Complaints:")
        i = 0
        for f in faults:
            i += 1
            lines.append(f"{i}. {f}")
        i = append_criterion_complaints(lines, verdicts, extra, i)
        for c in reds:
            i += 1
            lines.append(f"{i}. {c}")
        for c in debris_hits:
            i += 1
            lines.append(f"{i}. {c}")
        if observations:
            lines.append("")
            lines.append("Non-blocking observations:")
            lines.append(observations)
    elif n == 0:
        lines.append("ADVERSARIAL_REVIEW: DIVERGES")
        lines.append("Criteria: none provided.")
        lines.append(debris_summary_line(state))
        lines.append("Complaints:")
        lines.append(
            "1. No acceptance criteria were provided or extractable — conformance cannot "
            "be judged without a spec (fail-closed). Supply the plan's Objective/Tasks/"
            "Acceptance criteria and re-run."
        )
        i = 1
        for c in reds + debris_hits:
            i += 1
            lines.append(f"{i}. {c}")
    elif not partial and not bad and not extra and not reds and not debris_hits:
        lines.append("ADVERSARIAL_REVIEW: CONFORMS")
        lines.append(f"Criteria: {n}/{n} met (all with tests).")
        lines.append(debris_summary_line(state))
        if observations:
            lines.append("")
            lines.append("Non-blocking observations:")
            lines.append(observations)
    else:
        lines.append("ADVERSARIAL_REVIEW: DIVERGES")
        header = f"Criteria: {len(met)}/{n} met, {len(partial)} partial, {len(bad)} unmet/diverged"
        notes = []
        if died_count := sum(1 for v in verdicts if died(v)):
            notes.append(f"degraded run: {died_count} criterion check(s) died (fail-closed)")
        if reds:
            notes.append(f"{len(reds)} red verification run(s) (fail-closed)")
        if debris_hits:
            notes.append(
                f"{n_debris} implementation-debris hit(s) "
                "(fail-closed; also reported as the gauntlet's `debris` row when run under it)"
            )
        lines.append(header + "".join(f" — {note}" for note in notes) + ".")
        lines.append(debris_summary_line(state))
        lines.append("Complaints:")
        i = append_criterion_complaints(lines, verdicts, extra, 0)
        for c in reds:
            i += 1
            lines.append(f"{i}. {c}")
        for c in debris_hits:
            i += 1
            lines.append(f"{i}. {c}")
        if observations:
            lines.append("")
            lines.append("Non-blocking observations:")
            lines.append(observations)

    # Per-criterion evidence appendix (MET entries, for auditability).
    if met:
        lines.append("")
        lines.append("Met criteria (evidence):")
        for v in met:
            text = (v.get("text") or "").strip().replace("\n", " ")
            if len(text) > 100:
                text = text[:97] + "…"
            lines.append(f'- "{text}" — {(v.get("evidence") or "").strip()}')

    lines.append("")
    lines.append("Verification runs:")
    lines.extend(render_exec_results(state.get("exec_results")))

    print(json.dumps({"adv_report": "\n".join(lines)}))


try:
    main()
except Exception as e:  # noqa: BLE001 — never a silent verdict
    print(
        json.dumps(
            {
                "adv_report": (
                    "ADVERSARIAL_REVIEW: DIVERGES\n"
                    f"Criteria: verdict computation error: {e} — treat as failed, not as conforming."
                )
            }
        )
    )
