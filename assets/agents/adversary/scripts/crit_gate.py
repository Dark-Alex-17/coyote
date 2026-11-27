#!/usr/bin/env python3
"""Contract gate for the adversary's per-criterion verify chain.

Validates each criterion verdict against the contract in crit_contract.py:
  - a single JSON object with status in {MET, INSPECTED, PARTIAL, UNMET, DIVERGED}
  - MET requires non-empty evidence (the change AND the proving test)
  - INSPECTED requires evidence with at least one file:line citation and a
    non-empty `no_test_reason` saying why no test applies
  - every other status requires a non-empty complaint

The authoritative id is stamped from the criterion item — the model's echo is
never trusted. Malformed output is rejected back to check_criterion with the
reason, and the second rejection also quotes the rejected output verbatim.
When the re-asks are spent, the model's stated intent is followed only where
it cannot pass a criterion without the evidence the contract demands: a
status token with case or whitespace variance is normalized and re-validated,
and a not-met status (PARTIAL, UNMET, DIVERGED) is recorded even when its
complaint is missing or the JSON around it is broken. Anything else is
recorded as UNVERIFIED with the raw output as evidence, for the holistic pass
to rule on, so a malformed item can never sink the map or masquerade as met.
"""

import json
import os
import re
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from crit_contract import text_field, validate  # noqa: E402

NOT_MET = {"PARTIAL", "UNMET", "DIVERGED"}
MAX_RETRIES = 2
STATUS_TOKEN = re.compile(r'"?status"?\s*[:=]\s*"?\s*([A-Za-z]+)', re.I)


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


def salvage(raw, reason):
    """The model's stated intent, when it is recoverable without granting a
    met status the evidence does not support; None otherwise."""
    try:
        obj = extract_json(raw)
    except Exception:  # noqa: BLE001 -- broken JSON falls through to the token scan
        obj = None
    if isinstance(obj, dict) and isinstance(obj.get("status"), str):
        normalized = dict(obj, status=obj["status"].strip().upper())
        try:
            return validate(normalized)
        except ValueError:
            status = normalized["status"]
            if status in NOT_MET:
                return (
                    status,
                    text_field(obj, "evidence"),
                    f"{status} recorded without a usable complaint after re-asks ({reason})",
                    "",
                )
            return None
    m = STATUS_TOKEN.search(raw)
    if m and m.group(1).upper() in NOT_MET:
        status = m.group(1).upper()
        return (
            status,
            raw,
            f"{status} read from criterion-check output that failed validation after "
            f"re-asks ({reason}); the raw output is the evidence",
            "",
        )
    return None


def emit(cid, ctext, status, evidence, complaint, no_test_reason):
    print(
        json.dumps(
            {
                "crit_verdict": {
                    "id": cid,
                    "text": ctext,
                    "status": status,
                    "evidence": evidence,
                    "complaint": complaint,
                    "no_test_reason": no_test_reason,
                }
            }
        )
    )


def raw_output(state):
    raw = state.get("crit_verdict")
    if isinstance(raw, str):
        return raw
    return "" if raw is None else json.dumps(raw)


def main():
    state = load_state()
    cid, ctext = criterion_fields(state)
    attempts = state.get("gate_attempts")
    if not isinstance(attempts, int):
        attempts = 0
    raw = raw_output(state)

    try:
        emit(cid, ctext, *validate(extract_json(raw)))
        return
    except Exception as e:  # noqa: BLE001
        reason = str(e)

    if attempts < MAX_RETRIES:
        feedback = "PREVIOUS RESPONSE REJECTED by the verdict gate: " + reason + "."
        if attempts >= 1:
            feedback += (
                "\nYour previous response, verbatim between the markers:\n"
                "<<<PREVIOUS RESPONSE\n" + raw + "\nPREVIOUS RESPONSE>>>"
            )
        feedback += "\nRespond with ONLY the JSON object described in your instructions."
        print(
            json.dumps(
                {
                    "_next": "check_criterion",
                    "gate_attempts": attempts + 1,
                    "gate_feedback": feedback,
                }
            )
        )
        return

    if recovered := salvage(raw, reason):
        emit(cid, ctext, *recovered)
        return
    emit(
        cid,
        ctext,
        "UNVERIFIED",
        raw,
        "criterion check output failed machine validation after re-asks: " + reason,
        "",
    )


try:
    main()
except Exception as e:  # noqa: BLE001 — the gate must never crash the chain
    try:
        crashed_state = load_state()
        cid, ctext = criterion_fields(crashed_state)
        raw = raw_output(crashed_state)
    except Exception:  # noqa: BLE001 -- unreadable state may be what crashed the gate
        cid, ctext, raw = "unknown", "", ""
    emit(
        cid,
        ctext,
        "UNVERIFIED",
        raw,
        f"criterion gate error: {e} -- treat as unproven",
        "",
    )
