#!/usr/bin/env python3
"""Implementation-debris lint node for the review gauntlet.

Thin wrapper over the shared debris module: scans the ADDED lines of the diff
signals.py already resolved and emits the findings verdict_gate renders as the
blocking `debris` row. The `debris_exclude` graph variable (a JSON array of
repo-relative directories as a string, or one directory; empty means none)
is read by the shared module's parse_excludes; a list in state is taken as
is. Any crash lands in `debris_error`, which the gate
reports as a PIPELINE-FAULT -- a dead lint is never a pass. Each git call is
capped at debris.GIT_TIMEOUT and the worst case is bounded (rev-parse or
merge-base, up to three diffs under `auto`, one check-attr, and one config
lookup per binary driver). The 120s node timeout is the envelope: if it is
exceeded the node is killed and the fallback routes to debris_fault, which
records the fault, so the lint still fails closed.
"""

import json
import os
import sys

# State, diffs and reports carry emoji and non-ASCII paths; never decode them
# with the platform default (cp1252 on Windows).
for _stream in (sys.stdin, sys.stdout, sys.stderr):
    if hasattr(_stream, "reconfigure"):
        _stream.reconfigure(encoding="utf-8", errors="replace")

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".shared"))


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    import debris

    state = load_state()
    proj = os.path.expanduser(state.get("project_dir") or ".")
    spec = (state.get("diff_spec_resolved") or state.get("diff_spec") or "").strip() or "worktree"
    result = debris.scan(
        debris.diff_text_for(
            proj,
            spec,
            timeout=debris.GIT_TIMEOUT,
            exclude=debris.parse_excludes(state.get("debris_exclude")),
        )
    )
    print(
        json.dumps(
            {"debris_findings": result["findings"], "debris_summary": debris.summary(result)}
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 -- a crashed lint must block, never pass
    print(
        json.dumps(
            {
                "debris_findings": [],
                "debris_summary": "",
                "debris_error": f"debris lint crashed: {e}",
            }
        )
    )
