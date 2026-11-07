#!/usr/bin/env python3
"""Report-contract gate for each domain-reviewer's output.

"Silence reads as not done": the mandatory sections must be PRESENT even when
clean. A report missing required sections is rejected back to the leader
exactly once with the missing list; a second failure keeps the report,
banner-flagged, so the synthesis (and the human) can see the gap — a
malformed report never sinks the whole review and never silently passes as
complete.
"""

import json
import os
import re

REQUIRED = [
    ("## Domain:", "the `## Domain:` header"),
    ("### Findings", "the `### Findings` section"),
    ("### Mandatory checks", "the `### Mandatory checks` section (report blast radius / test adequacy / guard symmetry even when clean)"),
    ("DOMAIN_REVIEW_COMPLETE", "the `DOMAIN_REVIEW_COMPLETE` sentinel"),
]
MAX_RETRIES = 1


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    report = state.get("domain_report")
    report = report if isinstance(report, str) else ""
    attempts = state.get("gate_attempts")
    if not isinstance(attempts, int):
        attempts = 0

    missing = [desc for marker, desc in REQUIRED if marker not in report]
    if not missing:
        print(json.dumps({}))
        return

    if attempts < MAX_RETRIES:
        print(
            json.dumps(
                {
                    "_next": "review_domain",
                    "gate_attempts": attempts + 1,
                    # The rejection is a FORMAT violation; the rejected report is
                    # the leader's own same-tree analysis. Hand it back so the
                    # retry reuses it instead of re-reviewing the slice from
                    # scratch (a full leader-lane re-run to recover a header).
                    "gate_prior_report": (
                        "## YOUR PREVIOUS REPORT (rejected by the completeness gate "
                        "for the missing sections named in MUST DO - it is your own "
                        "analysis of this same diff. REUSE it: re-emit the full, "
                        "corrected report; re-run ONLY checks that are genuinely "
                        "absent below, and never invent content for a check you did "
                        "not run.)\n\n" + report
                    ),
                    "gate_feedback": (
                        "- COMPLETENESS GATE REJECTED your previous report — missing: "
                        + "; ".join(missing)
                        + ". Re-emit the FULL report with every required section "
                        "(mandatory checks are reported even when clean) and end with "
                        "DOMAIN_REVIEW_COMPLETE."
                    ),
                }
            )
        )
        return

    group = state.get("domain_group")
    domain = group.get("domain", "?") if isinstance(group, dict) else "?"
    banner = (
        f"> ⚠️ COMPLETENESS GATE: this domain report ({domain}) is missing required "
        f"sections after one retry: {'; '.join(missing)}. Treat the missing checks as "
        "NOT DONE, not as clean.\n\n"
    )
    print(json.dumps({"domain_report": banner + report}))


try:
    main()
except Exception as e:  # noqa: BLE001 — a gate crash must never sink the map
    print(json.dumps({"domain_report": f"> ⚠️ COMPLETENESS GATE ERROR: {e}\n\n(report unavailable)"}))
