#!/usr/bin/env python3
"""Deterministic verdict math for the code-reviewer graph.

The model that wrote the findings never grades its own homework:
- verifier verdicts are applied mechanically (FALSE → drop, tallied;
  VERIFIED → evidence attached; UNVERIFIABLE → kept, marked);
- a finding's severity is the icon or level word its severity field states,
  else the one its block's Severity line states; a finding that states no
  level is held at 🔴 with a note for a reviewer to restate, never read as
  the lowest level;
- rigor folding is arithmetic over severity + [convention]/[correctness]
  markers, exactly per the published rules;
- MERGE-READY/NEEDS-HUMAN is computed from counts + always-human triggers
  (deterministic signals ∪ the synthesis's ADDITIVE-only flags).
Fail-closed: any PIPELINE-FAULT recorded upstream (dead domain lane, dead
verifier, verifier verdicts carrying a PIPELINE-FAULT note or the
`pipeline-fault` sentinel id, dead synthesis, missing domain reports)
forces NEEDS-HUMAN. Only a verifier refutation (FALSE) removes a finding: a
🔴/🟡 finding whose verification did not complete keeps its stated severity
and its rigor section, is counted, and carries an unverified note. Two cases:
  - verification failed as a whole (the verifier died, or verified nothing):
    the reason opens with `review incomplete:` and the review gauntlet names
    the lane as an incomplete review;
  - single findings went unverified (a per-finding fault, or no verdict
    returned for them): the count is in the reason and an attention line,
    never an incomplete review.
A finding the anchor gate could not anchor in the diff is counted like any
other: its anchor note is advisory and never lowers the verdict.
A degraded run can never read MERGE-READY. Faults
always lead the attention list; the display cap applies only to the
non-fault entries so a degraded run can never hide a fault.
The gate never crashes into a silent verdict: any internal error emits
NEEDS-HUMAN naming the error.
"""

import json
import os
import re
import sys

# State, diffs and reports carry emoji and non-ASCII paths; never decode them
# with the platform default (cp1252 on Windows).
for _stream in (sys.stdin, sys.stdout, sys.stderr):
    if hasattr(_stream, "reconfigure"):
        _stream.reconfigure(encoding="utf-8", errors="replace")

MAX_DETAIL_CHARS = 500
ICONS = ("🔴", "🟡", "🟢", "💡")
# Most severe first, so a text naming two levels keeps the higher one.
SEVERITY_WORDS = (
    ("🔴", re.compile(r"\b(?:critical|blocker|blocking|high|severe)\b", re.I)),
    ("🟡", re.compile(r"\b(?:warning|warn|medium|major|moderate)\b", re.I)),
    ("🟢", re.compile(r"\b(?:suggestion|low|minor)\b", re.I)),
    ("💡", re.compile(r"\b(?:nit|nitpick|info|hint)\b", re.I)),
)
BLOCK_SEVERITY = re.compile(r"^\W*severity\W*[:\s](.*)$", re.I | re.M)


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def stated_icon(text):
    for icon in ICONS:
        if icon in text:
            return icon
    for icon, words in SEVERITY_WORDS:
        if words.search(text):
            return icon
    return None


def sev_icon(severity, block=""):
    """(icon, note): the level the severity field states, else the one the
    block's Severity line states; with neither, 🔴 and a note saying so."""
    if icon := stated_icon(str(severity or "")):
        return icon, ""
    for m in BLOCK_SEVERITY.finditer(str(block or "")):
        if icon := stated_icon(m.group(1)):
            return icon, ""
    return "🔴", (
        f"severity unreadable ({str(severity or '')!r}); held at 🔴 until a reviewer "
        "states its level"
    )


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

    verifier_raw = state.get("verifier_output")
    verifier_dead = isinstance(verifier_raw, str) and verifier_raw.lstrip().startswith(
        "Agent node failed:"
    )
    verified_nothing = any(v.get("id") == "pipeline-fault" for v in verifier_entries)

    def is_fault_note(v):
        return isinstance(v.get("note"), str) and v["note"].lstrip().startswith("PIPELINE-FAULT:")

    kept, dropped_titles = [], []
    for f in findings:
        block = f.get("block") or f"#### {f.get('title', '(untitled)')}"
        icon, severity_note = sev_icon(f.get("severity"), block)
        if severity_note:
            block += f"\n- *({severity_note})*"
        v = verdicts.get(str(f.get("id")))
        incomplete = False
        if icon in ("🔴", "🟡"):
            # Verification that did not complete leaves the finding unverified
            # at its stated severity; only a completed refutation drops it.
            incomplete = verifier_dead or verified_nothing or not v or is_fault_note(v)
        if v and icon in ("🔴", "🟡"):
            if v.get("verdict") == "FALSE" and not incomplete:
                dropped_titles.append(f.get("title", f.get("id", "?")))
                continue
            if v.get("verdict") == "VERIFIED" and v.get("evidence") and not incomplete:
                block += f"\n- **Evidence** (verifier): {v['evidence']}"
            elif v.get("verdict") == "UNVERIFIABLE" or incomplete:
                block += "\n- *(unverified: " + (v.get("note") or "could not be established") + ")*"
        elif icon in ("🔴", "🟡") and not v:
            block += "\n- *(unverified: no verifier verdict returned for this finding)*"
        kept.append({**f, "icon": icon, "block": block, "verification_incomplete": incomplete})

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
    unverified = [f for f in blocking if f["verification_incomplete"]]

    counts = {i: sum(1 for f in blocking if f["icon"] == i) for i in ICONS}
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
    for note in state.get("generated_notes") or []:
        if isinstance(note, str) and note.strip():
            attention.append(note.strip())
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
    if verifier_dead:
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
    # stand UNVERIFIED at their stated severity (soft: no lane re-run, since
    # the verify_gate node already retried the verifier locally once).
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
            f"branch fault: {_fv_detail(soft_fv)}); they stay at their stated severity, "
            "marked unverified; verify them manually"
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
    # anchor_gate.py marks a completed run; initial_state seeds False, so a
    # gate that never ran is a fault rather than an unchecked pass.
    anchor_error = state.get("anchor_error")
    if isinstance(anchor_error, str) and anchor_error.startswith("PIPELINE-FAULT:"):
        faults.append(anchor_error.strip().replace("\n", " ")[:MAX_DETAIL_CHARS])
    elif state.get("anchor_checked") is False and findings:
        faults.append(
            "PIPELINE-FAULT: the anchor check did not run, so the findings were not "
            "checked against the diff"
        )
    unanchored = [u for u in (state.get("unanchored") or []) if isinstance(u, dict)]
    anchor_notes = []
    if unanchored:
        anchor_notes.append(
            f"Not anchored: {len(unanchored)} finding(s) cite a file or lines outside this "
            "diff's changes; they are reported at their stated severity (unless the verifier "
            "refuted them) and listed under 'Not anchored in the diff'"
        )
    if changed and not reports:
        faults.append(
            "PIPELINE-FAULT: the diff is non-empty but no domain reports were produced — "
            "the review did not actually run; do not trust this verdict."
        )
    # faults go FIRST and uncapped so the display cap can never hide one;
    # the trigger reason keys off the human-facing triggers alone
    has_triggers = bool(attention)
    verification_failed = verifier_dead or verified_nothing
    incomplete_notes = []
    if unverified and verification_failed:
        incomplete_notes.append(
            f"Review incomplete: finding verification failed, so {len(unverified)} "
            "finding(s) are unverified; they stay at their stated severity, marked "
            "unverified; re-run the review or check each one by hand"
        )
    elif unverified:
        incomplete_notes.append(
            f"Unverified: {len(unverified)} finding(s) could not be verified; they stay at "
            "their stated severity, marked unverified; check each one by hand"
        )
    attention = faults + incomplete_notes + soft_notes + anchor_notes + coverage_notes + attention[:5]

    reasons = []
    # CONTRACT: review-gauntlet/verdict_gate.py anchors on both wordings; the
    # whole-failure one must OPEN the reason.
    if unverified and verification_failed:
        reasons.append(
            f"review incomplete: finding verification failed, {len(unverified)} finding(s) "
            "unverified, kept at their stated severity"
        )
    elif unverified:
        reasons.append(f"{len(unverified)} finding(s) unverified, kept at their stated severity")
    if counts["🔴"]:
        reasons.append(f"{counts['🔴']} 🔴 CRITICAL finding(s)")
    if yellow_correctness:
        reasons.append(f"{yellow_correctness} 🟡 [correctness] finding(s) outside the deferred section")
    if faults:
        # CONTRACT: review-gauntlet/verdict_gate.py anchors on this wording
        reasons.append("pipeline fault(s) recorded — degraded run")
    if soft_notes:
        # Deliberately does NOT match the gauntlet's degraded-run anchors:
        # a per-finding verifier fault is not a re-runnable lane fault.
        reasons.append(f"{len(soft_fv)} finding(s) unverified (verifier branch fault)")
    generated = [g for g in (state.get("generated_files") or []) if isinstance(g, str)]
    if generated and not changed:
        # A diff of generated files only is a review of nothing: never the
        # "No changes to review." stub, never MERGE-READY.
        reasons.append(f"{len(generated)} generated file(s) changed, none reviewed")
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
                    "unverified_count": len(unverified),
                    "unanchored_count": len(unanchored),
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
