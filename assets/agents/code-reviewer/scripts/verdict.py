#!/usr/bin/env python3
"""Deterministic verdict math for the code-reviewer graph.

The model that wrote the findings never grades its own homework:
- verifier verdicts are applied mechanically (FALSE → drop, tallied;
  VERIFIED → evidence attached; UNVERIFIABLE → kept, marked);
- rigor folding is arithmetic over severity + [convention]/[correctness]
  markers, exactly per the published rules;
- MERGE-READY/NEEDS-HUMAN is computed from counts + always-human triggers
  (deterministic signals ∪ the synthesis's ADDITIVE-only flags).
Fail-closed: any PIPELINE-FAULT recorded upstream (dead domain lane, dead
verifier, verifier verdicts carrying a PIPELINE-FAULT note or the
`pipeline-fault` sentinel id, dead synthesis, missing domain reports)
forces NEEDS-HUMAN — a degraded run can never read MERGE-READY. Faults
always lead the attention list; the display cap applies only to the
non-fault entries so a degraded run can never hide a fault.
The gate never crashes into a silent verdict: any internal error emits
NEEDS-HUMAN naming the error.
"""

import json
import os
import re

MAX_DETAIL_CHARS = 500


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


def parse_verifier_entries(text):
    """Extract the raw list of {id, verdict, evidence, note} dicts from
    finding-verifier output. Order and duplicates are preserved so fault
    accounting can count every entry (crash-guard entries all carry the
    same "unknown" id)."""
    if not isinstance(text, str) or not text.strip():
        return []
    start, end = text.find("["), text.rfind("]")
    if start != -1 and end > start:
        try:
            return [
                v for v in json.loads(text[start : end + 1]) if isinstance(v, dict) and v.get("id")
            ]
        except (ValueError, TypeError):
            pass
    entries = []
    for m in re.finditer(r'\{[^{}]*"id"[^{}]*\}', text):
        try:
            v = json.loads(m.group(0))
            if v.get("id"):
                entries.append(v)
        except (ValueError, TypeError):
            continue
    return entries


def parse_verifier(entries):
    """Index verifier entries by finding id; the last entry per id wins."""
    return {str(v["id"]): v for v in entries}


def main():
    state = load_state()
    findings = [f for f in (state.get("findings") or []) if isinstance(f, dict)]
    rigor = (state.get("resolved_rigor") or "production").lower()
    verifier_entries = parse_verifier_entries(state.get("verifier_output"))
    verdicts = parse_verifier(verifier_entries)

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

    # --- pipeline-fault accounting (fail closed) -----------------------------
    # PREFIX-anchored: a report that merely QUOTES a marker mid-text must not
    # trip it. Any fault forces NEEDS-HUMAN.
    faults = []
    changed = state.get("changed_files") or []
    reports = state.get("domain_reports") or []
    faulted = sum(
        1 for r in reports if isinstance(r, str) and r.lstrip().startswith("> ⚠️ PIPELINE-FAULT:")
    )
    if faulted:
        faults.append(
            f"PIPELINE-FAULT: {faulted} of {len(reports)} domain review lane(s) "
            "failed after retries — their slices were not reviewed"
        )
    verifier_raw = state.get("verifier_output")
    if isinstance(verifier_raw, str) and verifier_raw.lstrip().startswith("Agent node failed:"):
        faults.append(
            "PIPELINE-FAULT: finding verification failed — findings render as unverified"
        )
    # finding-verifier completes "successfully" with faults INSIDE the payload:
    # parse_fault's `pipeline-fault` sentinel entry, or a per-finding
    # PIPELINE-FAULT note from verify_fault or verdict_gate's crash guard.
    # Anchored on the sentinel id / note prefix only. Counted over the raw
    # entries, not the id-indexed dict, so N crash-guard entries sharing the
    # "unknown" id count as N.
    fv_faults = [
        v
        for v in verifier_entries
        if v.get("id") == "pipeline-fault"
        or (isinstance(v.get("note"), str) and v["note"].lstrip().startswith("PIPELINE-FAULT:"))
    ]
    # parse_fault's sentinel = the verifier verified NOTHING (hard fault, lane-
    # degrading). Per-finding verify_fault / crash-guard notes = those findings
    # simply stand UNVERIFIED (soft: proportionate NEEDS-HUMAN attention — a
    # ~1 h lane re-run to re-check a few findings is the wrong trade, and the
    # verify_gate node already retried the verifier locally once).
    hard_fv = [v for v in fv_faults if v.get("id") == "pipeline-fault"]
    soft_fv = [v for v in fv_faults if v.get("id") != "pipeline-fault"]

    def _fv_detail(entries):
        first = entries[0].get("note")
        detail = first.strip().replace("\n", " ") if isinstance(first, str) else ""
        if len(detail) > MAX_DETAIL_CHARS:
            detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
        return detail

    soft_notes = []
    if hard_fv:
        faults.append(
            "PIPELINE-FAULT: finding verification degraded — the verifier verified "
            f"nothing ({_fv_detail(hard_fv)})"
        )
    if soft_fv:
        soft_notes.append(
            f"Verifier: {len(soft_fv)} finding(s) could not be verified (verifier "
            f"branch fault: {_fv_detail(soft_fv)}) — they stand above UNVERIFIED; "
            "verify them manually"
        )
    # Files beyond facts.py's 300-file cap were never grouped or reviewed —
    # disclose the gap and force NEEDS-HUMAN with wording that does NOT match
    # the gauntlet's degraded-run anchors (a lane re-run cannot fix a cap).
    coverage_notes = []
    try:
        total_files = int(state.get("file_count") or 0)
    except (TypeError, ValueError):
        total_files = 0
    if total_files > len(changed):
        coverage_notes.append(
            f"Coverage gap: {total_files - len(changed)} of {total_files} changed files "
            "were NOT reviewed (file-list cap) — review them manually or split the PR"
        )
    # Keyed off synth_failure + a non-empty diff, NEVER off findings == [] —
    # a clean review legitimately has zero findings and stays MERGE-READY.
    synth_failure = state.get("synth_failure")
    if isinstance(synth_failure, str) and synth_failure.startswith("LLM node") and changed:
        detail = synth_failure.strip().replace("\n", " ")
        if len(detail) > MAX_DETAIL_CHARS:
            detail = detail[: MAX_DETAIL_CHARS - 1] + "…"
        faults.append(f"PIPELINE-FAULT: synthesis failed — findings unavailable; {detail}")
    if changed and not reports:
        faults.append(
            "PIPELINE-FAULT: the diff is non-empty but no domain reports were produced — "
            "the review did not actually run; do not trust this verdict."
        )
    # faults go FIRST and uncapped so the display cap can never hide one;
    # the trigger reason keys off the human-facing triggers alone
    has_triggers = bool(attention)
    attention = faults + soft_notes + coverage_notes + attention[:5]

    reasons = []
    if counts["🔴"]:
        reasons.append(f"{counts['🔴']} 🔴 CRITICAL finding(s)")
    if yellow_correctness:
        reasons.append(f"{yellow_correctness} 🟡 [correctness] finding(s) outside the deferred section")
    if faults:
        # CONTRACT: review-gauntlet/verdict_gate.py anchors on this wording
        reasons.append("pipeline fault(s) recorded — degraded run")
    if soft_notes:
        # Deliberately does NOT match the gauntlet's degraded-run anchors:
        # unverified findings are a human-attention item, not a re-runnable
        # lane fault.
        reasons.append(f"{len(soft_fv)} finding(s) unverified (verifier branch fault)")
    if coverage_notes:
        reasons.append(f"{total_files - len(changed)} changed file(s) not reviewed (file cap)")
    if has_triggers:
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
                    "attention": attention,
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
                    # PIPELINE-FAULT prefix is load-bearing: render.py's
                    # empty-diff stub bypass anchors on it — without it a
                    # crashed gate on an empty diff collapses into the
                    # "No changes to review." stub.
                    "reason": f"PIPELINE-FAULT: verdict computation error: {e} — human review required",
                    "counts": {}, "deferred_count": 0, "dropped_count": 0,
                    "dropped_titles": [], "attention": [f"PIPELINE-FAULT: verdict script error: {e}"],
                    "findings_final": [],
                }
            }
        )
    )
