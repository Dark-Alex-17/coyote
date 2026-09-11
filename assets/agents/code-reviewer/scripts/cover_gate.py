#!/usr/bin/env python3
"""Exact-cover gate for the refined domain groups.

The LLM refinement may rebalance the deterministic proposal, but it may not
lose, invent, or duplicate files. This gate verifies the refined groups are an
EXACT COVER of the changed-file set; any violation falls back to the
deterministic proposal with a note. The LLM can shape the grouping, never
shrink it. Emits `group_items` — the map node's item list.

Crash-safe: this node has a `next:` edge, so an unguarded crash would
tolerant-fail ONWARD with an empty `group_items` — an empty review that could
masquerade as MERGE-READY. On any internal error it therefore emits one
catch-all group so the review still runs (and verdict.py's pipeline tripwire
catches the empty-report case independently).
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
    changed = state.get("changed_files") or []
    proposed = state.get("proposed_groups") or []
    refined = state.get("groups") or []

    note = ""
    chosen = None
    if isinstance(refined, list) and refined:
        seen = []
        valid = True
        for g in refined:
            if not isinstance(g, dict) or not isinstance(g.get("files"), list) or not g.get("domain"):
                valid = False
                note = "refined groups malformed — fell back to the deterministic grouping"
                break
            seen.extend(g["files"])
        if valid:
            if sorted(seen) != sorted(changed):
                missing = sorted(set(changed) - set(seen))
                extra = sorted(set(seen) - set(changed))
                dupes = sorted({f for f in seen if seen.count(f) > 1})
                note = (
                    "refined groups failed the exact-cover check "
                    f"(missing={missing[:5]} extra={extra[:5]} duplicated={dupes[:5]}) — "
                    "fell back to the deterministic grouping"
                )
            else:
                chosen = [{"domain": g["domain"], "files": g["files"]} for g in refined]
    if chosen is None:
        chosen = [{"domain": g["domain"], "files": g["files"]} for g in proposed if isinstance(g, dict)]

    print(json.dumps({"group_items": chosen, "groups_note": note}))


try:
    main()
except Exception as e:  # noqa: BLE001 — a crash must never tolerant-fail into an empty review
    try:
        files = load_state().get("changed_files") or []
    except Exception:  # noqa: BLE001
        files = []
    print(
        json.dumps(
            {
                "group_items": [{"domain": "all-changes", "files": files}],
                "groups_note": f"cover gate error: {e} — fell back to a single catch-all group",
            }
        )
    )
