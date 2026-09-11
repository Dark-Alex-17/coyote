#!/usr/bin/env python3
"""Contract gate for finding-verifier's per-finding verify chain.

Validates the raw verifier output against the verdict contract:
  - a single JSON object
  - verdict in {VERIFIED, FALSE, UNVERIFIABLE}
  - VERIFIED / FALSE require non-empty evidence
  - UNVERIFIABLE requires a non-empty note

On a valid response it normalizes the object (the authoritative `id` is
stamped from the finding item itself — the model's echo is never trusted)
and writes it to the chain's output key. On an invalid response it routes
back to `verify_one` exactly once with the rejection reason; if the retry
is also invalid, it records an UNVERIFIABLE verdict naming the failure so
a malformed item can never sink the whole map or masquerade as a pass.

Deterministic by design: no LLM judgment happens here, and the script
always exits 0 with a valid JSON object on stdout.
"""

import json
import os
import re

VERDICTS = {"VERIFIED", "FALSE", "UNVERIFIABLE"}
MAX_RETRIES = 1


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def finding_id(state):
    finding = state.get("finding")
    if isinstance(finding, dict):
        fid = finding.get("id")
        if isinstance(fid, str) and fid.strip():
            return fid.strip()
    return "unknown"


def extract_json(text):
    if not isinstance(text, str) or not text.strip():
        raise ValueError("verifier output is empty or not text")
    text = re.sub(r"```[a-zA-Z]*", "", text).replace("```", "")
    start, end = text.find("{"), text.rfind("}")
    if start == -1 or end <= start:
        raise ValueError("no JSON object found in verifier output")
    return json.loads(text[start : end + 1])


def validate(obj):
    if not isinstance(obj, dict):
        raise ValueError("verifier output is not a JSON object")
    verdict = obj.get("verdict")
    if verdict not in VERDICTS:
        raise ValueError(f"verdict must be one of {sorted(VERDICTS)}, got {verdict!r}")
    evidence = obj.get("evidence") if isinstance(obj.get("evidence"), str) else ""
    note = obj.get("note") if isinstance(obj.get("note"), str) else ""
    if verdict in ("VERIFIED", "FALSE") and not evidence.strip():
        raise ValueError(
            f"{verdict} requires non-empty evidence quoting the decisive line(s) with path:line"
        )
    if verdict == "UNVERIFIABLE" and not note.strip():
        raise ValueError("UNVERIFIABLE requires a note explaining why it could not be established")
    return verdict, evidence, note


def main():
    state = load_state()
    fid = finding_id(state)
    attempts = state.get("gate_attempts")
    if not isinstance(attempts, int):
        attempts = 0

    try:
        verdict, evidence, note = validate(extract_json(state.get("verdict")))
        print(
            json.dumps(
                {"verdict": {"id": fid, "verdict": verdict, "evidence": evidence, "note": note}}
            )
        )
        return
    except Exception as e:  # noqa: BLE001 — every failure is a rejection reason
        reason = str(e)

    if attempts < MAX_RETRIES:
        print(
            json.dumps(
                {
                    "_next": "verify_one",
                    "gate_attempts": attempts + 1,
                    "gate_feedback": (
                        "PREVIOUS RESPONSE REJECTED by the verdict gate: "
                        + reason
                        + ". Respond with ONLY the JSON object described under"
                        " 'Output format' — no prose, no code fences."
                    ),
                }
            )
        )
        return

    print(
        json.dumps(
            {
                "verdict": {
                    "id": fid,
                    "verdict": "UNVERIFIABLE",
                    "evidence": "",
                    "note": f"verifier output failed machine validation after retry: {reason}",
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
                "verdict": {
                    "id": "unknown",
                    "verdict": "UNVERIFIABLE",
                    "evidence": "",
                    "note": f"verdict gate error: {e}",
                }
            }
        )
    )
