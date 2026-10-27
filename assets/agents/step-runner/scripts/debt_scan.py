#!/usr/bin/env python3
"""Thin shim: the implementation lives in agents/.shared/debt_scan.py (shared
with the sisyphus `debt_scan` tool). Graph mode is selected automatically via
GRAPH_STATE/GRAPH_STATE_FILE; output is {"debt_hits": "<report>"} and the
node's static `next:` routes onward. Fail-open: the shared script never exits
non-zero for scan failures (it reports DEBT_SCAN_FAILED + the manual
pipeline), and a hard crash of this shim falls forward on the node's declared
`fallback:` edge (the same sweep its `next:` names) with the initial_state
DEBT_SCAN_FAILED default still in `debt_hits`."""

import os
import runpy

runpy.run_path(
    os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "..", ".shared", "debt_scan.py"
    ),
    run_name="__main__",
)
