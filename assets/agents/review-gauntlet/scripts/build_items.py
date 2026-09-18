#!/usr/bin/env python3
"""Deterministic lane builder for the review gauntlet.

Selection = hard rules (the floor) ∪ LLM additions ∪ caller-forced lanes.
Forcing is ADDITIVE: it can only widen the computed selection — honoring a
forced list as the exact set once dropped the always-on code-review lane in
a live run. Emits one 0-or-1-item list per lane: an empty list makes that
lane's map node run zero branches (map-over-nothing), which IS the skip
mechanism — no dynamic routing anywhere.

The LLM's add_* flags can only ADD lanes. A flag that is false never
removes a lane the hard rules selected.

Unavailable diff signals (malformed spec, git failure) mean the surface is
UNKNOWN — selection degrades WIDER (security on; probe when a recipe
exists), never narrower.

A re-run pass (retry_gate emitted `retry_lanes`) skips selection: only the
named lanes get an item, stamped with their attempt number, and every other
lane maps over nothing — retry_gate restores their earlier reports.

Fail-closed: a builder crash emits all four lists empty plus a
PIPELINE-FAULT in signals_error — verdict_gate blocks on it and the crash
never kills the graph.
"""

import json
import os
import time

# Keep in sync with default_lanes.ALIASES — a caller-forced lane must
# canonicalize the same way on the degraded path as it does here.
ALIASES = {
    "code-review": "code-review",
    "code_review": "code-review",
    "code-reviewer": "code-review",
    "adversary": "adversary",
    "adversarial": "adversary",
    "adversarial-review": "adversary",
    "security": "security",
    "security-review": "security",
    "security-reviewer": "security",
    "probe": "probe",
    "usage": "probe",
    "usage-pattern": "probe",
    "usage-pattern-testing": "probe",
}

# Keep in sync with retry_gate.LANES
LANE_KEYS = {
    "code-review": "code_review_items",
    "adversary": "adversary_items",
    "security": "security_items",
    "probe": "probe_items",
}

# Filled by main() so the crash guard can append to an upstream fault
# instead of overwriting it, and keep the first pass's lanes_summary.
crash_context = {"signals_error": "", "lanes_summary": ""}


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    prior = state.get("signals_error")
    crash_context["signals_error"] = prior if isinstance(prior, str) else ""
    # retry_gate's wall-clock budget is measured from this stamp; signals.py
    # is the normal stamper, this is the defensive fallback.
    stamp = {} if state.get("gauntlet_started_at") else {"gauntlet_started_at": time.time()}

    def text(key):
        v = state.get(key)
        return v.strip() if isinstance(v, str) else ""

    crash_context["lanes_summary"] = text("lanes_summary")
    retry_lanes = [lane for lane in (state.get("retry_lanes") or []) if lane in LANE_KEYS]
    if retry_lanes:
        attempts = state.get("lane_attempts") or {}
        retried = [(lane, int(attempts.get(lane) or 0) + 1) for lane in retry_lanes]
        note = "- retried: [" + ", ".join(f"{lane} (attempt {n})" for lane, n in retried) + "]"
        summary = text("lanes_summary")
        print(
            json.dumps(
                {
                    **{key: [] for key in LANE_KEYS.values()},
                    **{
                        LANE_KEYS[lane]: [{"lane": lane, "attempt": n}] for lane, n in retried
                    },
                    "lanes_summary": f"{summary}\n{note}" if summary else note,
                    "retry_lanes": [],
                    **stamp,
                }
            )
        )
        return

    forced_raw = state.get("forced_lanes") or []
    forced, unknown = [], []
    for lane in forced_raw:
        canon = ALIASES.get(str(lane).strip().lower())
        (forced.append(canon) if canon else unknown.append(str(lane)))

    reasons = []
    lanes = {"code-review"}
    reasons.append(
        "code-review: always on — the gauntlet is only spawned for non-trivial work"
    )
    if text("plan_context"):
        lanes.add("adversary")
        reasons.append("adversary: a spec/plan/acceptance-criteria context is present")
    if (
        state.get("touches_auth")
        or state.get("touches_deps")
        or state.get("touches_exec")
        or state.get("security_posture") == "hardened"
    ):
        lanes.add("security")
        reasons.append(
            "security: attack-surface signals (auth/deps/exec) or hardened posture"
        )
    if state.get("consumer_surface") and text("probe_context"):
        lanes.add("probe")
        reasons.append(
            "probe: consumer-facing surface changed and a local-run recipe/suite pointer exists"
        )

    # Unavailable signals (malformed diff spec, git failure) = surface UNKNOWN.
    # Degrade WIDER, never narrower — a run without signals once silently
    # dropped the security and probe lanes.
    if text("signals_summary") == "signals unavailable":
        if "security" not in lanes:
            lanes.add("security")
            reasons.append(
                "security: diff signals unavailable — surface unknown, degrading wider"
            )
        if "probe" not in lanes and text("probe_context"):
            lanes.add("probe")
            reasons.append(
                "probe: diff signals unavailable and a local-run recipe/suite pointer "
                "exists — degrading wider"
            )

    # LLM judgment is additive-only.
    llm_why = text("lane_reasons")[:300]
    for flag, lane in (
        ("add_code_review", "code-review"),
        ("add_adversary", "adversary"),
        ("add_security", "security"),
        ("add_probe", "probe"),
    ):
        if state.get(flag) and lane not in lanes:
            if lane == "probe" and not text("probe_context"):
                reasons.append(
                    "probe: requested by lane judgment but skipped — no local-run "
                    "recipe/suite pointer (a probe without one is guaranteed INCONCLUSIVE)"
                )
                continue
            if lane == "adversary" and not text("plan_context"):
                reasons.append(
                    "adversary: requested by lane judgment but skipped — no spec/plan "
                    "to check conformance against"
                )
                continue
            lanes.add(lane)
            reasons.append(f"{lane}: added by lane-selection judgment — {llm_why}")

    # Caller-forced lanes are ADDITIVE: they widen the computed selection,
    # never replace it. (Honoring them as the exact set dropped code-review —
    # documented as always-on — and adversary in a live run.) Forcing also
    # bypasses the context guards above: the caller demanded the lane, so it
    # runs, with the expectation named in the summary.
    if forced:
        newly = sorted(set(forced) - lanes)
        lanes |= set(forced)
        reasons.append(
            f"forced lanes (additive): {sorted(set(forced))}"
            + (f" — newly added: {newly}" if newly else " — all already selected")
        )
        if "probe" in forced and not text("probe_context"):
            reasons.append(
                "probe: forced without a local-run recipe/suite pointer — expect INCONCLUSIVE"
            )
        if "adversary" in forced and not text("plan_context"):
            reasons.append(
                "adversary: forced without spec/plan context — it fails closed "
                "(DIVERGES) without extractable criteria"
            )

    if unknown:
        reasons.append(f"ignored unknown forced lane name(s): {unknown}")

    # A degraded lane selection (default_lanes fallback) must survive into the
    # final report; lanes_summary is the only channel this script doesn't
    # overwrite silently.
    degraded = text("lanes_degraded")
    if degraded:
        reasons.insert(0, degraded)

    print(
        json.dumps(
            {
                "code_review_items": [{"lane": "code-review"}] if "code-review" in lanes else [],
                "adversary_items": [{"lane": "adversary"}] if "adversary" in lanes else [],
                "security_items": [{"lane": "security"}] if "security" in lanes else [],
                "probe_items": [{"lane": "probe"}] if "probe" in lanes else [],
                "lanes_summary": "\n".join(f"- {r}" for r in reasons),
                **stamp,
            }
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 — a builder crash must never kill the graph
    fault = f"PIPELINE-FAULT: lane builder crashed — {e}; no lanes were run"
    if crash_context["signals_error"]:
        fault = f"{crash_context['signals_error']}; {fault}"
    print(
        json.dumps(
            {
                "code_review_items": [],
                "adversary_items": [],
                "security_items": [],
                "probe_items": [],
                "lanes_summary": crash_context["lanes_summary"],
                "signals_error": fault,
            }
        )
    )
