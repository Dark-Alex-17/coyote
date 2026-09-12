#!/usr/bin/env python3
"""Deterministic lane-selection fallback for the review gauntlet.

Reached ONLY via select_lanes' `fallback:` route, when the additive LLM
judgment dies after retries. Emits `forced_lanes` — code-review + adversary,
plus probe when the deterministic signals flagged a consumer-facing
surface — which build_items honors exactly, so selection degrades WIDER,
never narrower. The degradation note goes into `lanes_degraded`; build_items
folds it into lanes_summary so the final report names the degradation.
"""

import json
import os

MAX_DETAIL_CHARS = 500


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    forced = ["code-review", "adversary"]
    if state.get("consumer_surface"):
        forced.append("probe")
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
