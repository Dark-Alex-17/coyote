#!/usr/bin/env python3
"""Deterministic verdict math for the code-reviewer graph.

The model that wrote the findings never grades its own homework:
- verifier verdicts are applied mechanically (FALSE → drop, tallied;
  VERIFIED → evidence attached; UNVERIFIABLE → kept, marked);
- rigor folding is arithmetic over severity + [convention]/[correctness]
  markers, exactly per the published rules;
- MERGE-READY/NEEDS-HUMAN is computed from counts + always-human triggers
  (deterministic signals ∪ the synthesis's ADDITIVE-only flags).
The gate never crashes into a silent verdict: any internal error emits
NEEDS-HUMAN naming the error.
"""

import json
import os
import re

def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def sev_icon(severity):
    for icon in ("🔴", "🟡", "🟢", "💡"):
        if icon in (severity or ""):
            return icon
    return "🟢"


def parse_verifier(text):
    """Extract {id: {verdict, evidence, note}} from finding-verifier output."""
    verdicts = {}
    if not isinstance(text, str) or not text.strip():
        return verdicts
    start, end = text.find("["), text.rfind("]")
    if start != -1 and end > start:
        try:
            for v in json.loads(text[start : end + 1]):
                if isinstance(v, dict) and v.get("id"):
                    verdicts[str(v["id"])] = v
            return verdicts
        except (ValueError, TypeError):
            pass
    for m in re.finditer(r'\{[^{}]*"id"[^{}]*\}', text):
        try:
            v = json.loads(m.group(0))
            if v.get("id"):
                verdicts[str(v["id"])] = v
        except (ValueError, TypeError):
            continue
    return verdicts


def main():
    state = load_state()
    findings = [f for f in (state.get("findings") or []) if isinstance(f, dict)]
    rigor = (state.get("resolved_rigor") or "production").lower()
    verdicts = parse_verifier(state.get("verifier_output"))

    kept, dropped_titles = [], []
    for f in findings:
        icon = sev_icon(f.get("severity"))
        v = verdicts.get(str(f.get("id")))
        block = f.get("block") or f"#### {f.get('title', '(untitled)')}"
        if v and icon in ("🔴", "🟡"):
            if v.get("verdict") == "FALSE":
                dropped_titles.append(f.get("title", f.get("id", "?")))
                continue
            if v.get("verdict") == "VERIFIED" and v.get("evidence"):
                block += f"\n- **Evidence** (verifier): {v['evidence']}"
            elif v.get("verdict") == "UNVERIFIABLE":
                block += "\n- *(unverified: " + (v.get("note") or "could not be established") + ")*"
        elif icon in ("🔴", "🟡") and not v:
            block += "\n- *(unverified: no verifier verdict returned for this finding)*"
        kept.append({**f, "icon": icon, "block": block})

    # --- rigor folding (arithmetic, never rewrites severities) --------------
    for f in kept:
        marker = (f.get("marker") or "").lower()
        icon = f["icon"]
        section = "blocking"
        if icon == "🔴":
            section = "blocking"
        elif rigor == "prototype" and ((icon == "🟡" and marker == "convention") or icon == "🟢"):
            section = "deferred"
        elif rigor == "poc":
            if icon == "🟡" and marker == "convention":
                section = "deferred"
            elif icon in ("🟢", "💡") and marker == "convention":
                section = "dropped-by-poc"
        f["section"] = section
    final = [f for f in kept if f["section"] != "dropped-by-poc"]
    blocking = [f for f in final if f["section"] == "blocking"]
    deferred = [f for f in final if f["section"] == "deferred"]

    counts = {i: sum(1 for f in blocking if f["icon"] == i) for i in ("🔴", "🟡", "🟢", "💡")}
    yellow_correctness = sum(
        1 for f in blocking if f["icon"] == "🟡" and (f.get("marker") or "").lower() == "correctness"
    )

    # --- always-human triggers ----------------------------------------------
    attention = []
    if state.get("destructive_migration"):
        attention.append("Destructive/irreversible migration in the diff (DROP/RENAME) — needs a human signature.")
    if state.get("touches_auth"):
        attention.append("Authentication/authorization surface changed — needs a human signature.")
    for flag in state.get("attention_flags") or []:
        if isinstance(flag, str) and flag.strip():
            attention.append(flag.strip())
    placement = (state.get("placement_notes") or "").strip()
    if placement:
        attention.append("Placement question (design intent, never auto-resolved): " + placement)
    # dedup, preserve order
    seen = set()
    attention = [a for a in attention if not (a.lower() in seen or seen.add(a.lower()))]

    if (state.get("changed_files") or []) and not (state.get("domain_reports") or []):
        attention.append(
            "PIPELINE FAULT: the diff is non-empty but no domain reports were produced — "
            "the review did not actually run; do not trust this verdict."
        )
    reasons = []
    if counts["🔴"]:
        reasons.append(f"{counts['🔴']} 🔴 CRITICAL finding(s)")
    if yellow_correctness:
        reasons.append(f"{yellow_correctness} 🟡 [correctness] finding(s) outside the deferred section")
    if attention:
        reasons.append("always-human trigger(s) fired")
    verdict = "NEEDS-HUMAN" if reasons else "MERGE-READY"
    reason_line = "; ".join(reasons) if reasons else "no blocking findings and no always-human triggers"

    print(
        json.dumps(
            {
                "verdict_out": {
                    "verdict": verdict,
                    "reason": reason_line,
                    "counts": counts,
                    "deferred_count": len(deferred),
                    "dropped_count": len(dropped_titles),
                    "dropped_titles": dropped_titles,
                    "attention": attention[:5],
                    "findings_final": final,
                }
            }
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 — never crash into a silent verdict
    print(
        json.dumps(
            {
                "verdict_out": {
                    "verdict": "NEEDS-HUMAN",
                    "reason": f"verdict computation error: {e} — treat as needing human review",
                    "counts": {}, "deferred_count": 0, "dropped_count": 0,
                    "dropped_titles": [], "attention": [f"verdict script error: {e}"],
                    "findings_final": [],
                }
            }
        )
    )
