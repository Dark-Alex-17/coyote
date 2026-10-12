#!/usr/bin/env python3
"""Implementation-debris lint node for the adversary graph.

Thin wrapper over the shared debris module: scans the ADDED lines of the diff
the parse stage named and emits the findings verdict.py renders as DIVERGES
complaints. The `debris_exclude` graph variable (a JSON array of
repo-relative directories as a string, or one directory; empty means none)
is handed to the shared module as its exclude list; a list in state is taken
as is. Any crash appends a PIPELINE-FAULT marker to `pipeline_faults`
(keeping prior entries), which the verdict turns into DIVERGES -- a dead lint
is never a pass. Each git call is capped at debris.GIT_TIMEOUT; the worst
case is bounded (rev-parse or merge-base, up to three diffs under `auto`,
one check-attr, and one config lookup per binary driver).
The 120s node timeout is the envelope: if it is exceeded the node is killed
and the fallback routes to pipeline_fault, which records the fault, so the
lint still fails closed.
"""

import json
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".shared"))


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def excludes_from(state):
    value = state.get("debris_exclude")
    if value is None:
        return []
    if isinstance(value, str):
        value = value.strip()
        if value == "":
            return []
        if value.startswith("["):
            try:
                value = json.loads(value)
            except ValueError as e:
                raise ValueError(f"debris_exclude {value!r} is not a JSON array: {e}") from None
        elif not value.startswith('"'):
            value = [value]
    if not isinstance(value, list) or not all(isinstance(p, str) for p in value):
        raise ValueError(
            f"debris_exclude must be a JSON array of strings or one directory, got {value!r}"
        )
    return value


def main():
    import debris

    state = load_state()
    proj = os.path.expanduser(state.get("project_dir") or ".")
    spec = (state.get("diff_spec") or "").strip() or "auto"
    result = debris.scan(
        debris.diff_text_for(
            proj, spec, timeout=debris.GIT_TIMEOUT, exclude=excludes_from(state)
        )
    )
    print(
        json.dumps(
            {"debris_findings": result["findings"], "debris_summary": debris.summary(result)}
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 -- a crashed lint must fail closed, never pass
    try:
        prior = [f for f in (load_state().get("pipeline_faults") or []) if isinstance(f, str)]
    except Exception:  # noqa: BLE001 -- unreadable state still gets the marker
        prior = []
    print(
        json.dumps(
            {
                "debris_findings": [],
                "debris_summary": "",
                "pipeline_faults": prior + [f"PIPELINE-FAULT: debris lint crashed -- {e}"],
            }
        )
    )
