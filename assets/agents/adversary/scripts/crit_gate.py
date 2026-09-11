#!/usr/bin/env python3
"""Contract gate for the adversary's per-criterion verify chain.

Validates each criterion verdict against the contract:
  - a single JSON object with status in {MET, PARTIAL, UNMET, DIVERGED}
  - MET requires non-empty evidence (the change AND the proving test)
  - non-MET requires a non-empty complaint

The authoritative id is stamped from the criterion item — the model's echo is
never trusted. Malformed output is rejected back to check_criterion exactly
once with the reason; a second failure records PARTIAL (unproven) so a
malformed item can never sink the map or masquerade as MET.
"""

import json
import os
import re

STATUSES = {"MET", "PARTIAL", "UNMET", "DIVERGED"}
MAX_RETRIES = 1


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def criterion_fields(state):
    c = state.get("criterion")
    if isinstance(c, dict):
        cid = c.get("id")
        text = c.get("text")
        return (
            cid.strip() if isinstance(cid, str) and cid.strip() else "unknown",
            text.strip() if isinstance(text, str) else "",
        )
    return "unknown", ""


def extract_json(text):
    if not isinstance(text, str) or not text.strip():
        raise ValueError("criterion-check output is empty or not text")
    text = re.sub(r"```[a-zA-Z]*", "", text).replace("```", "")
    start, end = text.find("{"), text.rfind("}")
    if start == -1 or end <= start:
        raise ValueError("no JSON object found in criterion-check output")
    return json.loads(text[start : end + 1])


def validate(obj):
    if not isinstance(obj, dict):
        raise ValueError("criterion-check output is not a JSON object")
    status = obj.get("status")
    if status not in STATUSES:
        raise ValueError(f"status must be one of {sorted(STATUSES)}, got {status!r}")
    evidence = obj.get("evidence") if isinstance(obj.get("evidence"), str) else ""
    complaint = obj.get("complaint") if isinstance(obj.get("complaint"), str) else ""
    if status == "MET" and not evidence.strip():
        raise ValueError(
            "MET requires non-empty evidence citing the satisfying change AND the "
            "test proving the behavior (file:line)"
        )
    if status != "MET" and not complaint.strip():
        raise ValueError(
            f"{status} requires a non-empty complaint (what the diff does/omits at "
            "file:line, and what would make it conform)"
        )
    return status, evidence, complaint


def main():
    state = load_state()
    cid, ctext = criterion_fields(state)
    attempts = state.get("gate_attempts")
    if not isinstance(attempts, int):
        attempts = 0

    try:
        status, evidence, complaint = validate(extract_json(state.get("crit_verdict")))
        print(
            json.dumps(
                {
                    "crit_verdict": {
                        "id": cid,
                        "text": ctext,
                        "status": status,
                        "evidence": evidence,
                        "complaint": complaint,
                    }
                }
            )
        )
        return
    except Exception as e:  # noqa: BLE001
        reason = str(e)

    if attempts < MAX_RETRIES:
        print(
            json.dumps(
                {
                    "_next": "check_criterion",
                    "gate_attempts": attempts + 1,
                    "gate_feedback": (
                        "PREVIOUS RESPONSE REJECTED by the verdict gate: " + reason +
                        ". Respond with ONLY the JSON object described in your instructions."
                    ),
                }
            )
        )
        return

    print(
        json.dumps(
            {
                "crit_verdict": {
                    "id": cid,
                    "text": ctext,
                    "status": "PARTIAL",
                    "evidence": "",
                    "complaint": (
                        "criterion check output failed machine validation after retry: "
                        + reason
                        + " — treat this criterion as unproven"
                    ),
                }
            }
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 — the gate must never crash the chain
    print(
        json.dumps(
            {
                "crit_verdict": {
                    "id": "unknown",
                    "text": "",
                    "status": "PARTIAL",
                    "evidence": "",
                    "complaint": f"criterion gate error: {e} — treat as unproven",
                }
            }
        )
    )
