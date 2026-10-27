#!/usr/bin/env python3
"""Deterministic parse of the findings JSON embedded in the spawn prompt.

The finding-verifier's sole caller (code-reviewer's `verify` node) pastes the
synthesize stage's already-structured findings array into the prompt:

    Verify these code-review findings against the working tree.

    Project: <dir>

    Findings (JSON; ...):
    [ {"id": ..., "severity": ..., "marker": ..., "file": ..., "lines": ...,
       "title": ..., "block": ...}, ... ]

Paying an LLM call to re-extract JSON that is already JSON is pure overhead,
so this script handles the structured case and routes to the legacy LLM
`parse` node on ANY anomaly (fail-open: unparseable JSON, missing/mistyped
fields, unclassifiable severity, absent prompt). A crash in this script falls
forward to the same LLM node via the declared `next:` edge. Runs exactly
once; forward edges only - no loops introduced.

Deliberate divergence from the LLM parse, judged safe: the LLM instructions
split a finding citing multiple locations into `-b`/`-c` suffixed entries.
This script emits exactly ONE entry per finding under its ORIGINAL id -
verdict.py maps verifier entries back by exact id (verdict.py:70,83), so the
unsplit form is the one every downstream consumer is guaranteed to handle;
the claim text still carries the extra locations and verify_one has the fs
tools (and budget) to read them. FALSE verdicts require proof, so a partially
-true multi-location claim cannot be wrongly dropped by the unsplit form.
"""

import json
import os
import re
import sys

RED = "\U0001f534"  # 🔴
YELLOW = "\U0001f7e1"  # 🟡
GREEN = "\U0001f7e2"  # 🟢
HINT = "\U0001f4a1"  # 💡

REQUIRED_FIELDS = ("id", "severity", "file", "lines", "title", "block")


def load_state():
    path = os.environ.get("GRAPH_STATE_FILE")
    if path and os.path.exists(path):
        with open(path, encoding="utf-8") as fh:
            return json.load(fh)
    raw = os.environ.get("GRAPH_STATE")
    if raw:
        return json.loads(raw)
    return {}


def fall_back(reason):
    """Route to the legacy LLM parse node; today's behavior, unchanged."""
    print(json.dumps({"_next": "parse", "parse_script_note": reason}))
    sys.exit(0)


def classify(sev, marker):
    """True = verify (🔴/🟡), False = skip (🟢/💡), None = unclassifiable."""
    text = f"{sev} {marker}"
    if RED in text or YELLOW in text:
        return True
    if GREEN in text or HINT in text:
        return False
    upper = text.upper()
    if "CRITICAL" in upper or "WARNING" in upper:
        return True
    if "SUGGESTION" in upper or "NITPICK" in upper or "NIT" in upper:
        return False
    return None


def main():
    try:
        state = load_state()
    except (json.JSONDecodeError, OSError) as exc:
        fall_back(f"state unreadable: {exc}")

    prompt = state.get("initial_prompt") or ""
    if not prompt.strip():
        fall_back("no initial_prompt in state")

    # Project dir: the caller's "Project: <dir>" line, else the graph default.
    m = re.search(r"^Project:\s*(\S.*?)\s*$", prompt, re.MULTILINE)
    project_dir = m.group(1) if m else (state.get("project_dir") or ".")

    # The findings JSON array: start at the first '[' after the "Findings"
    # header (or the first '[' at all), end at the last ']'.
    header = re.search(r"^Findings\b.*$", prompt, re.MULTILINE)
    search_from = header.end() if header else 0
    start = prompt.find("[", search_from)
    if start == -1:
        start = prompt.find("[")
    end = prompt.rfind("]")
    if start == -1 or end <= start:
        fall_back("no JSON array found in prompt")

    try:
        raw = json.loads(prompt[start : end + 1])
    except json.JSONDecodeError as exc:
        fall_back(f"findings JSON did not parse: {exc}")

    if not isinstance(raw, list):
        fall_back("findings JSON is not a list")

    findings_list = []
    for i, item in enumerate(raw):
        if not isinstance(item, dict):
            fall_back(f"finding #{i} is not an object")
        entry = {}
        for field in REQUIRED_FIELDS:
            value = item.get(field)
            if isinstance(value, (int, float)):
                value = str(value)
            if field == "lines" and value is None:
                value = ""
            if not isinstance(value, str):
                fall_back(f"finding #{i} field {field!r} missing or mistyped")
            entry[field] = value
        wanted = classify(entry["severity"], str(item.get("marker", "")))
        if wanted is None:
            fall_back(f"finding #{i} severity unclassifiable: {entry['severity']!r}")
        if not wanted:
            continue  # 🟢/💡 post unverified by design
        findings_list.append(
            {
                "id": entry["id"] or f"f{i + 1}",
                "severity": entry["severity"],
                "path": entry["file"],
                "lines": entry["lines"],
                # Caller contract: `title` + `block` together are the claim.
                "claim": (entry["title"] + "\n\n" + entry["block"]).strip(),
            }
        )

    print(
        json.dumps(
            {
                "findings_list": findings_list,
                "project_dir": project_dir,
                "_next": "verify_each",
            }
        )
    )


if __name__ == "__main__":
    main()
