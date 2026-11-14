"""The criterion-verdict contract shared by crit_gate.py and verdict.py.

A verdict is a JSON object with status in STATUSES. MET needs non-empty
evidence, INSPECTED needs evidence with a file:line citation and a non-empty
`no_test_reason`, and every other status needs a non-empty complaint.
"""

import re

STATUSES = {"MET", "INSPECTED", "PARTIAL", "UNMET", "DIVERGED"}
CITATION = re.compile(r"[\w./-]+:\d+")


def text_field(obj, key):
    value = obj.get(key)
    return value if isinstance(value, str) else ""


def validate(obj):
    """(status, evidence, complaint, no_test_reason) or ValueError with the reason."""
    if not isinstance(obj, dict):
        raise ValueError("criterion-check output is not a JSON object")
    status = obj.get("status")
    if status not in STATUSES:
        raise ValueError(f"status must be one of {sorted(STATUSES)}, got {status!r}")
    evidence = text_field(obj, "evidence")
    complaint = text_field(obj, "complaint")
    no_test_reason = text_field(obj, "no_test_reason")
    if status == "MET" and not evidence.strip():
        raise ValueError(
            "MET requires non-empty evidence citing the satisfying change AND the "
            "test proving the behavior (file:line)"
        )
    if status == "INSPECTED":
        if not evidence.strip():
            raise ValueError(
                "INSPECTED requires non-empty evidence citing the inspected change (file:line)"
            )
        if not CITATION.search(evidence):
            raise ValueError(
                "INSPECTED evidence must contain at least one file:line citation"
            )
        if not no_test_reason.strip():
            raise ValueError(
                "INSPECTED requires a non-empty no_test_reason saying in one line why "
                "no test could prove this criterion"
            )
    if status not in ("MET", "INSPECTED") and not complaint.strip():
        raise ValueError(
            f"{status} requires a non-empty complaint (what the diff does/omits at "
            "file:line, and what would make it conform)"
        )
    if status != "INSPECTED":
        no_test_reason = ""
    return status, evidence, complaint, no_test_reason.strip()
