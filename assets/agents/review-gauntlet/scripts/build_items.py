#!/usr/bin/env python3
"""Deterministic lane builder for the review gauntlet.

Selection = hard rules (the floor) ∪ LLM additions; forced lanes override
everything exactly. Emits one 0-or-1-item list per lane: an empty list makes
that lane's map node run zero branches (map-over-nothing), which IS the
skip mechanism — no dynamic routing anywhere.

The LLM's add_* flags can only ADD lanes. A flag that is false never
removes a lane the hard rules selected.

Fail-closed: a builder crash emits all four lists empty plus a
PIPELINE-FAULT in signals_error — verdict_gate blocks on it and the crash
never kills the graph.
"""

import json
import os

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


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()

    def text(key):
        v = state.get(key)
        return v.strip() if isinstance(v, str) else ""

    forced_raw = state.get("forced_lanes") or []
    forced, unknown = [], []
    for lane in forced_raw:
        canon = ALIASES.get(str(lane).strip().lower())
        (forced.append(canon) if canon else unknown.append(str(lane)))

    reasons = []
    if forced:
        lanes = set(forced)
        reasons.append(f"forced lanes honored exactly: {sorted(lanes)}")
    else:
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
            }
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 — a builder crash must never kill the graph
    print(
        json.dumps(
            {
                "code_review_items": [],
                "adversary_items": [],
                "security_items": [],
                "probe_items": [],
                "lanes_summary": "",
                "signals_error": f"PIPELINE-FAULT: lane builder crashed — {e}; no lanes were run",
            }
        )
    )
