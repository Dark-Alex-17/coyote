#!/usr/bin/env python3
"""Deterministic conformance verdict for the adversary graph.

CONFORMS requires: at least one criterion, every criterion MET, and zero
holistic complaints. Everything else — including a missing plan — is
DIVERGES (fail-closed). Assembles the exact sentinel format the callers
route on. Never crashes into a silent verdict.
"""

import json
import os

def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    verdicts = [v for v in (state.get("crit_verdicts") or []) if isinstance(v, dict)]
    extra = [c for c in (state.get("extra_complaints") or []) if isinstance(c, dict)]
    observations = (state.get("observations") or "").strip()
    n = len(verdicts)

    met = [v for v in verdicts if v.get("status") == "MET"]
    partial = [v for v in verdicts if v.get("status") == "PARTIAL"]
    bad = [v for v in verdicts if v.get("status") in ("UNMET", "DIVERGED")]

    lines = []
    if n == 0:
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
        i = 0
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
