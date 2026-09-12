#!/usr/bin/env python3
"""Deterministic conformance verdict for the adversary graph.

CONFORMS requires: at least one criterion, every criterion MET, zero
holistic complaints, and zero pipeline faults. Everything else — including
a missing plan or a degraded (faulted) pipeline — is DIVERGES (fail-closed).
Any "PIPELINE-FAULT:" marker recorded in `pipeline_faults` (or a holistic
llm-node failure captured in `holistic_failure`) becomes complaint #1;
per-criterion results are still reported. Assembles the exact sentinel
format the callers route on. Never crashes into a silent verdict.
"""

import json
import os

MAX_FAULT_DETAIL_CHARS = 300


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
        if v.get("status") == "MET":
            continue
        i += 1
        text = (v.get("text") or "").strip().replace("\n", " ")
        if len(text) > 140:
            text = text[:137] + "…"
        lines.append(
            f'{i}. Acceptance criterion "{text}" — {v.get("status", "?").title()} — '
            f'{(v.get("complaint") or "").strip()}'
        )
    for c in extra:
        i += 1
        lines.append(
            f"{i}. {c.get('kind', 'violation')} — {c.get('location', '?')} — "
            f"{c.get('violation', '')} — {c.get('fix', '')}"
        )
    return i


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
            out.append(
                f"- [{status}] `{r.get('cmd', '?')}` — exit {r.get('exit', '?')}, "
                f"{r.get('duration_s', '?')}s"
            )
            if status == "FAIL":
                tail = (r.get("tail") or "").strip()
                out.extend("    " + t for t in tail.splitlines()[-15:])
        return out or ["- (no verification commands were run)"]
    if isinstance(exec_results, str) and exec_results.strip():
        return [f"- {exec_results.strip()}"]
    return ["- none recorded (the run did not reach the verification stage)"]


def main():
    state = load_state()
    verdicts = [v for v in (state.get("crit_verdicts") or []) if isinstance(v, dict)]
    extra = [c for c in (state.get("extra_complaints") or []) if isinstance(c, dict)]
    observations = (state.get("observations") or "").strip()
    faults = collect_pipeline_faults(state)
    n = len(verdicts)

    met = [v for v in verdicts if v.get("status") == "MET"]
    partial = [v for v in verdicts if v.get("status") == "PARTIAL"]
    bad = [v for v in verdicts if v.get("status") in ("UNMET", "DIVERGED")]

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
        lines.append("Complaints:")
        i = 0
        for f in faults:
            i += 1
            lines.append(f"{i}. {f}")
        append_criterion_complaints(lines, verdicts, extra, i)
        if observations:
            lines.append("")
            lines.append("Non-blocking observations:")
            lines.append(observations)
    elif n == 0:
        lines.append("ADVERSARIAL_REVIEW: DIVERGES")
        lines.append("Criteria: none provided.")
        lines.append("Complaints:")
        lines.append(
            "1. No acceptance criteria were provided or extractable — conformance cannot "
            "be judged without a spec (fail-closed). Supply the plan's Objective/Tasks/"
            "Acceptance criteria and re-run."
        )
    elif not partial and not bad and not extra:
        lines.append("ADVERSARIAL_REVIEW: CONFORMS")
        lines.append(f"Criteria: {n}/{n} met (all with tests).")
        if observations:
            lines.append("")
            lines.append("Non-blocking observations:")
            lines.append(observations)
    else:
        lines.append("ADVERSARIAL_REVIEW: DIVERGES")
        lines.append(
            f"Criteria: {len(met)}/{n} met, {len(partial)} partial, {len(bad)} unmet/diverged."
        )
        lines.append("Complaints:")
        append_criterion_complaints(lines, verdicts, extra, 0)
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
