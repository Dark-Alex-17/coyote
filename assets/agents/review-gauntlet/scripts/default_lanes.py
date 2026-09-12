#!/usr/bin/env python3
"""Deterministic lane-selection fallback for the review gauntlet.

Reached ONLY via select_lanes' `fallback:` route, when the additive LLM
judgment dies after retries. Emits `forced_lanes` — code-review + adversary,
plus probe when the deterministic signals flagged a consumer-facing surface,
plus security when the attack-surface signals (auth/deps/exec) or a hardened
posture demand it, unioned with any lanes the caller forced (canonicalized
with the same aliases build_items uses) — which build_items honors exactly,
so selection degrades WIDER, never narrower. The degradation note goes into
`lanes_degraded`; build_items folds it into lanes_summary so the final
report names the degradation.
"""

import json
import os

MAX_DETAIL_CHARS = 500

# Keep in sync with build_items.ALIASES — a caller-forced lane must
# canonicalize the same way here as it would on the non-degraded path.
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

# Stable emission order keeps the fallback deterministic.
LANE_ORDER = ["code-review", "adversary", "security", "probe"]


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    lanes = {"code-review", "adversary"}
    if state.get("consumer_surface"):
        lanes.add("probe")
    # The deterministic security signals are already in state when this
    # fallback runs; a degraded selection must never drop the security lane
    # build_items' hard rules would have selected.
    if (
        state.get("touches_auth")
        or state.get("touches_deps")
        or state.get("touches_exec")
        or state.get("security_posture") == "hardened"
    ):
        lanes.add("security")
    # Caller-forced lanes survive degradation — union, never overwrite.
    for lane in state.get("forced_lanes") or []:
        if canon := ALIASES.get(str(lane).strip().lower()):
            lanes.add(canon)
    forced = [lane for lane in LANE_ORDER if lane in lanes]
    raw = state.get("select_lanes_failure")
    detail = raw.strip().replace("\n", " ") if isinstance(raw, str) else ""
    if len(detail) > MAX_DETAIL_CHARS:
        detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
    note = "lane selection degraded to deterministic defaults"
    if detail:
        note += f" — {detail}"
    print(json.dumps({"forced_lanes": forced, "lanes_degraded": note}))


try:
    main()
except Exception as e:  # noqa: BLE001 — the fallback itself must never crash
    print(
        json.dumps(
            {
                "forced_lanes": ["code-review", "adversary"],
                "lanes_degraded": "lane selection degraded to deterministic "
                f"defaults (fallback script error: {e})",
            }
        )
    )
