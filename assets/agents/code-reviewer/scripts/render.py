#!/usr/bin/env python3
"""Deterministic report assembly for the code-reviewer graph.

Builds the standard report format from the structured findings and prose
sections. Pure templating — no judgment, no re-summarization, no finding
ever added or removed here.
"""

import json
import os

def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def main():
    state = load_state()
    v = state.get("verdict_out") or {}
    findings = v.get("findings_final") or []
    counts = v.get("counts") or {}
    changed = state.get("changed_files") or []
    rigor = state.get("resolved_rigor") or "production"

    if not changed:
        print(json.dumps({"final_report": "# Code Review Summary\n\nNo changes to review."}))
        return

    verdict = v.get("verdict", "NEEDS-HUMAN")
    lines = [
        "# Code Review Summary",
        "",
        f"**Verdict: {verdict}** — {v.get('reason', '')}",
        "",
        "## Walkthrough",
        state.get("walkthrough") or "(no walkthrough provided)",
        "",
        "## Changes",
        "",
        "| File | Changes | Findings |",
        "|------|---------|----------|",
    ]

    desc_by_file = {}
    for row in state.get("changes_rows") or []:
        if isinstance(row, dict) and row.get("file"):
            desc_by_file[row["file"]] = row.get("desc", "")
    blocking = [f for f in findings if f.get("section") == "blocking"]
    deferred = [f for f in findings if f.get("section") == "deferred"]
    for f in changed:
        per = [x for x in blocking if x.get("file") == f]
        tally = " ".join(
            f"{icon} {n}" for icon, n in
            ((i, sum(1 for x in per if x.get("icon") == i)) for i in ("🔴", "🟡", "🟢", "💡"))
            if n
        ) or "—"
        lines.append(f"| `{f}` | {desc_by_file.get(f, '')} | {tally} |")

    lines += ["", "## Detailed Findings", ""]
    by_file = {}
    for x in blocking:
        by_file.setdefault(x.get("file", "(unattributed)"), []).append(x)
    if by_file:
        for f in sorted(by_file):
            lines += [f"### `{f}`", ""]
            for x in by_file[f]:
                lines += [x.get("block", ""), ""]
    else:
        lines += ["No blocking findings.", ""]

    lines += ["## Cross-File Concerns", state.get("cross_file") or "None", ""]
    if (state.get("operational_history") or "").strip():
        lines += ["## Operational history", state["operational_history"], ""]
    if (state.get("org_context_section") or "").strip():
        lines += ["## Org context", state["org_context_section"], ""]
    if deferred and rigor != "production":
        lines += ["## Deferred by quality bar", ""]
        for x in deferred:
            lines += [x.get("block", ""), ""]
    attention = v.get("attention") or []
    if verdict == "NEEDS-HUMAN" and attention:
        lines += ["## Human attention required", ""]
        lines += [f"- {a}" for a in attention]
        lines += [""]

    miss_ids = sorted({
        m for x in findings for m in
        __import__("re").findall(r"MISS-\d+", (x.get("title") or "") + (x.get("block") or ""))
    })
    lines += [
        "---",
        f"*Reviewed {len(changed)} files, found {counts.get('🔴', 0)} critical, "
        f"{counts.get('🟡', 0)} warnings, {counts.get('🟢', 0)} suggestions, "
        f"{counts.get('💡', 0)} nitpicks ({v.get('deferred_count', 0)} deferred by quality bar)*",
        f"*Quality bar: {rigor} — provenance: {state.get('bar_provenance', '?')}; "
        f"surfaces: {state.get('resolved_surfaces') or 'none'}*",
        f"*Verdict: {verdict}; verifier: dropped {v.get('dropped_count', 0)} provably-false"
        + (f" ({', '.join(v.get('dropped_titles', [])[:5])})" if v.get("dropped_count") else "")
        + f"; review-miss patterns applied: {', '.join(miss_ids) if miss_ids else 'none relevant'}*",
    ]
    print(json.dumps({"final_report": "\n".join(lines)}))


try:
    main()
except Exception as e:  # noqa: BLE001
    print(json.dumps({"final_report": f"# Code Review Summary\n\n**Verdict: NEEDS-HUMAN** — report rendering error: {e}"}))
