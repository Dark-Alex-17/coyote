#!/usr/bin/env python3
"""Deterministic conformance verdict for the adversary graph.

CONFORMS requires: at least one criterion, every criterion MET or INSPECTED
(met by inspection: no test could prove it, cited at file:line), zero
blocking holistic complaints, zero pipeline faults, zero blocking red
verification runs, and zero implementation-debris hits from debris_lint.py.
A red run (nonzero exit or timeout) blocks unless run_checks.py found the
same failure on the diff base (baseline status "pre-existing"); those are
reported in their own section. A command the runner's deadline skipped is
reported as not run and does not block by itself: the criterion checks judge
the criteria it covers. An ENVIRONMENT runner-error marker blocks when
commands were declared, since the declared commands never ran.
Holistic complaints block unless they carry `blocking: false`; those are
reported as non-blocking. A criterion whose check output stayed unreadable
(UNVERIFIED) takes the holistic pass's format ruling for it when one exists,
and is otherwise a PIPELINE-FAULT. Rulings for any other criterion are
ignored, so the holistic pass never changes a criterion it did not have to
settle.
Everything else -- including a missing plan or a degraded (faulted)
pipeline -- is DIVERGES (fail-closed).
Any "PIPELINE-FAULT:" marker recorded in `pipeline_faults` (or a holistic
llm-node failure captured in `holistic_failure`) becomes complaint #1;
per-criterion results are still reported. A criterion whose check DIED
(criterion_fault's UNMET verdict, evidence prefixed DIED_EVIDENCE_PREFIX) is
rendered as died rather than judged and counted as unmet regardless of the
status it carries. Assembles the exact sentinel format the callers route on.
Never crashes into a silent verdict.
"""

import json
import os
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".shared"))

MAX_FAULT_DETAIL_CHARS = 300
MAX_MARKER_CHARS = 200
MAX_SECTION_TEXT_CHARS = 100
DIED_MARKER = "criterion check DIED (pipeline fault)"
DIED_EVIDENCE_PREFIX = "PIPELINE-FAULT: criterion "
RULED_MARKER = "(ruled by holistic: output format failed)"
RULING_STATUSES = {"MET", "INSPECTED", "PARTIAL", "UNMET", "DIVERGED"}


def died(v):
    evidence = v.get("evidence")
    return isinstance(evidence, str) and evidence.startswith(DIED_EVIDENCE_PREFIX)


def blocking(complaint):
    """Only an explicit `blocking: false` makes a holistic complaint advisory."""
    return complaint.get("blocking") is not False


def str_field(d, key):
    value = d.get(key)
    return value.strip() if isinstance(value, str) else ""


def short_text(v):
    text = (v.get("text") or "").strip().replace("\n", " ")
    if len(text) > MAX_SECTION_TEXT_CHARS:
        text = text[: MAX_SECTION_TEXT_CHARS - 3] + "..."
    return text


def apply_format_rulings(verdicts, rulings):
    """Settle each UNVERIFIED verdict with the holistic ruling for its id.

    A ruling whose id names a verdict in any other status, or whose status is
    outside RULING_STATUSES, is ignored.
    """
    by_id = {}
    for r in rulings if isinstance(rulings, list) else []:
        if isinstance(r, dict) and isinstance(r.get("id"), str) and r.get("status") in RULING_STATUSES:
            by_id.setdefault(r["id"].strip(), r)
    out = []
    for v in verdicts:
        ruling = by_id.get(v.get("id")) if v.get("status") == "UNVERIFIED" and not died(v) else None
        if ruling is None:
            out.append(v)
            continue
        evidence = str_field(ruling, "evidence")
        complaint = str_field(ruling, "complaint")
        out.append(
            dict(
                v,
                status=ruling["status"],
                evidence=f"{RULED_MARKER} {evidence}".strip(),
                complaint=f"{RULED_MARKER} {complaint}".strip() if complaint else "",
                no_test_reason=str_field(ruling, "no_test_reason"),
            )
        )
    return out


def unverified_faults(verdicts):
    return [
        f'PIPELINE-FAULT: criterion verdict unreadable after re-asks, and the holistic '
        f'pass gave no format ruling for it -- "{short_text(v)}" '
        f"({(v.get('complaint') or '').strip()})"
        for v in verdicts
        if v.get("status") == "UNVERIFIED" and not died(v)
    ]


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def collect_pipeline_faults(state):
    """Recorded fault markers, plus a holistic llm-node failure if one occurred.

    `holistic_failure` holds the "LLM node …failed: <chain>" string when the
    holistic pass fell back to verdict; on a successful pass it holds the
    rendered schema output (never that prefix).
    """
    faults = [
        f.strip()
        for f in (state.get("pipeline_faults") or [])
        if isinstance(f, str) and f.strip()
    ]
    hf = state.get("holistic_failure")
    if isinstance(hf, str) and hf.strip().startswith("LLM node"):
        detail = hf.strip().replace("\n", " ")
        if len(detail) > MAX_FAULT_DETAIL_CHARS:
            detail = detail[: MAX_FAULT_DETAIL_CHARS - 1] + "…"
        faults.append(
            "PIPELINE-FAULT: holistic pass failed — criterion verdicts stand "
            f"but cross-cutting hunt did not run ({detail})"
        )
    return faults


def append_criterion_complaints(lines, verdicts, extra, start):
    """Numbered complaints for not-met verdicts and blocking holistic extras.

    UNVERIFIED verdicts are left out: they are already pipeline faults.
    """
    i = start
    for v in verdicts:
        if v.get("status") in ("MET", "INSPECTED", "UNVERIFIED") and not died(v):
            continue
        i += 1
        text = (v.get("text") or "").strip().replace("\n", " ")
        if len(text) > 140:
            text = text[:137] + "…"
        complaint = (v.get("complaint") or "").strip()
        if died(v):
            if not complaint.startswith(DIED_MARKER):
                complaint = f"{DIED_MARKER} — {complaint}" if complaint else DIED_MARKER
            lines.append(f'{i}. Acceptance criterion "{text}" — {complaint}')
        else:
            lines.append(
                f'{i}. Acceptance criterion "{text}" — {v.get("status", "?").title()} — '
                f"{complaint}"
            )
    for c in extra:
        i += 1
        lines.append(
            f"{i}. {c.get('kind', 'violation')} — {c.get('location', '?')} — "
            f"{c.get('violation', '')} — {c.get('fix', '')}"
        )
    return i


def declared_nonempty(declared):
    """True when verification_commands names at least one command.

    Accepts a list or a JSON-encoded string of one. None, unparseable, or
    empty declarations count as not declared — run_checks already records
    a PIPELINE-FAULT for the invalid ones.
    """
    if isinstance(declared, str):
        try:
            declared = json.loads(declared)
        except json.JSONDecodeError:
            return False
    if not isinstance(declared, list):
        return False
    return any(isinstance(c, str) and c.strip() for c in declared)


def exec_run_complaints(exec_results, declared):
    """One complaint per blocking red verification run recorded by run_checks.py.

    A red run is a record that exited nonzero (including the -1 TIMEOUT
    sentinel). It blocks unless its baseline status is "pre-existing"; the
    complaint names the baseline outcome. A record the deadline skipped is
    not a run and yields nothing here. An "ENVIRONMENT: ..." runner-error
    marker with a non-empty declaration is one complaint: the declared
    commands never ran. Other string markers ("none declared",
    "PIPELINE-FAULT...") are not runs and yield nothing.
    """
    if isinstance(exec_results, str):
        marker = exec_results.strip().replace("\n", " ")
        if not marker.startswith("ENVIRONMENT:") or not declared_nonempty(declared):
            return []
        if len(marker) > MAX_MARKER_CHARS:
            marker = marker[: MAX_MARKER_CHARS - 1] + "…"
        return [
            f"Verification runner error — the declared command(s) never ran ({marker}); "
            "unproven runs block CONFORMS"
        ]
    if not isinstance(exec_results, list):
        return []
    out = []
    for r in exec_results:
        if not isinstance(r, dict) or r.get("skipped") or pre_existing(r):
            continue
        cmd = r.get("cmd", "?")
        code = r.get("exit")
        if code == 0:
            continue
        outcome = "TIMEOUT (exit -1)" if code == -1 else f"exit {code}"
        out.append(
            f"Verification run `{cmd}` — {outcome} (recorded FAIL; a red declared "
            f"command that did not already fail on the diff base blocks CONFORMS; {baseline_text(r)})"
        )
    return out


def pre_existing(record):
    baseline = record.get("baseline")
    return isinstance(baseline, dict) and baseline.get("status") == "pre-existing"


def baseline_text(record):
    baseline = record.get("baseline")
    if not isinstance(baseline, dict):
        return "baseline on the diff base: none recorded"
    text = f"baseline on the diff base: {baseline.get('status', '?')}"
    if baseline.get("exit") is not None:
        text += f", base exit {baseline['exit']}"
    if note := str_field(baseline, "note"):
        text += f", {note}"
    return text


def render_tail(record):
    tail = (record.get("tail") or "").strip()
    return ["    " + t for t in tail.splitlines()[-15:]]


def render_exec_results(exec_results):
    """Lines for the "Verification runs:" report section.

    exec_results is a list of {cmd, exit, duration_s, tail} records (or
    {cmd, skipped} for commands the runner's total deadline left no budget
    for) from run_checks.py, or a string marker ("none declared ..." /
    "ENVIRONMENT: ..."), or empty when the run never reached the stage.
    A red record may carry a `baseline` object; a pre-existing failure's
    tail is rendered in its own section instead of here.
    """
    if isinstance(exec_results, list):
        out = []
        for r in exec_results:
            if not isinstance(r, dict):
                continue
            if r.get("skipped"):
                out.append(
                    f"- [NOT RUN] `{r.get('cmd', '?')}` -- never ran: the runner's "
                    "total deadline expired first (not blocking by itself; the criteria "
                    "it covers are judged unproven)"
                )
                continue
            status = "PASS" if r.get("exit") == 0 else "FAIL"
            under = f" (under: {r['under']})" if r.get("under") else ""
            out.append(
                f"- [{status}] `{r.get('cmd', '?')}` — exit {r.get('exit', '?')}, "
                f"{r.get('duration_s', '?')}s"
                + under
            )
            if status == "FAIL":
                out.append(f"    {baseline_text(r)}")
                if not pre_existing(r):
                    out.extend(render_tail(r))
        return out or ["- (no verification commands were run)"]
    if isinstance(exec_results, str) and exec_results.strip():
        return [f"- {exec_results.strip()}"]
    return ["- none recorded (the run did not reach the verification stage)"]


def debris_complaints(state):
    """Hit count and rendered lines for the findings debris_lint.py recorded.

    Also reported as the gauntlet's `debris` row when run under it; here each
    hit is a numbered complaint so the lane fails closed on its own. The
    shared module is imported only when there is something to render, and an
    unresolvable `.shared` becomes a complaint rather than the whole report
    collapsing into the crash stub. Only `null` and `[]` mean no findings;
    any other value, a falsy non-list included, is the lint's malformed shape.
    """
    raw = state.get("debris_findings")
    if raw is None or raw == []:
        return 0, []
    try:
        import debris
    except ImportError as e:
        return 1, [f"Implementation debris lint unavailable: {e} -- fail-closed"]
    kind, n = debris.classify_findings(raw)
    if kind != "hits":
        shape = (
            f"list with {n} non-dict item(s)"
            if isinstance(raw, list)
            else f"{type(raw).__name__}, not a list"
        )
        return 1, [f"Implementation debris findings malformed ({shape}) -- fail-closed"]
    return n, debris.render(raw)


def debris_summary_line(state):
    summary = state.get("debris_summary")
    if not isinstance(summary, str) or not summary.strip():
        return "Debris: no summary recorded (the lint did not run)"
    summary = summary.strip()
    if summary.startswith("debris: "):
        summary = summary[len("debris: ") :]
    return f"Debris: {summary}"


def main():
    state = load_state()
    verdicts = [v for v in (state.get("crit_verdicts") or []) if isinstance(v, dict)]
    verdicts = apply_format_rulings(verdicts, state.get("format_rulings"))
    holistic = [c for c in (state.get("extra_complaints") or []) if isinstance(c, dict)]
    extra = [c for c in holistic if blocking(c)]
    advisory = [c for c in holistic if not blocking(c)]
    observations = (state.get("observations") or "").strip()
    faults = collect_pipeline_faults(state) + unverified_faults(verdicts)
    reds = exec_run_complaints(
        state.get("exec_results"), state.get("verification_commands")
    )
    n_debris, debris_hits = debris_complaints(state)
    n = len(verdicts)

    met = [v for v in verdicts if v.get("status") in ("MET", "INSPECTED") and not died(v)]
    proven = [v for v in met if v.get("status") == "MET"]
    inspected = [v for v in met if v.get("status") == "INSPECTED"]
    unverified = sum(1 for v in verdicts if v.get("status") == "UNVERIFIED" and not died(v))
    partial = [v for v in verdicts if v.get("status") == "PARTIAL" and not died(v)]
    bad = [
        v for v in verdicts if died(v) or v.get("status") in ("UNMET", "DIVERGED")
    ]

    lines = []
    if faults:
        lines.append("ADVERSARIAL_REVIEW: DIVERGES")
        if n:
            lines.append(
                f"Criteria: {len(met)}/{n} met, {len(partial)} partial, "
                f"{f'{unverified} unverified, ' if unverified else ''}"
                f"{len(bad)} unmet/diverged — degraded run: pipeline fault recorded "
                "(fail-closed)."
            )
        else:
            lines.append(
                "Criteria: none verified — degraded run: pipeline fault recorded "
                "(fail-closed)."
            )
        lines.append(debris_summary_line(state))
        lines.append("Complaints:")
        i = 0
        for f in faults:
            i += 1
            lines.append(f"{i}. {f}")
        i = append_criterion_complaints(lines, verdicts, extra, i)
        for c in reds:
            i += 1
            lines.append(f"{i}. {c}")
        for c in debris_hits:
            i += 1
            lines.append(f"{i}. {c}")
        if observations:
            lines.append("")
            lines.append("Non-blocking observations:")
            lines.append(observations)
    elif n == 0:
        lines.append("ADVERSARIAL_REVIEW: DIVERGES")
        lines.append("Criteria: none provided.")
        lines.append(debris_summary_line(state))
        lines.append("Complaints:")
        lines.append(
            "1. No acceptance criteria were provided or extractable — conformance cannot "
            "be judged without a spec (fail-closed). Supply the plan's Objective/Tasks/"
            "Acceptance criteria and re-run."
        )
        i = 1
        for c in reds + debris_hits:
            i += 1
            lines.append(f"{i}. {c}")
    elif not partial and not bad and not extra and not reds and not debris_hits:
        lines.append("ADVERSARIAL_REVIEW: CONFORMS")
        lines.append(
            f"Criteria: {n}/{n} met ({len(proven)} test-proven, "
            f"{len(inspected)} by inspection)."
        )
        lines.append(debris_summary_line(state))
        if observations:
            lines.append("")
            lines.append("Non-blocking observations:")
            lines.append(observations)
    else:
        lines.append("ADVERSARIAL_REVIEW: DIVERGES")
        header = f"Criteria: {len(met)}/{n} met, {len(partial)} partial, {len(bad)} unmet/diverged"
        notes = []
        if died_count := sum(1 for v in verdicts if died(v)):
            notes.append(f"degraded run: {died_count} criterion check(s) died (fail-closed)")
        if reds:
            notes.append(f"{len(reds)} red verification run(s) (fail-closed)")
        if debris_hits:
            notes.append(
                f"{n_debris} implementation-debris hit(s) "
                "(fail-closed; also reported as the gauntlet's `debris` row when run under it)"
            )
        lines.append(header + "".join(f" — {note}" for note in notes) + ".")
        lines.append(debris_summary_line(state))
        lines.append("Complaints:")
        i = append_criterion_complaints(lines, verdicts, extra, 0)
        for c in reds:
            i += 1
            lines.append(f"{i}. {c}")
        for c in debris_hits:
            i += 1
            lines.append(f"{i}. {c}")
        if observations:
            lines.append("")
            lines.append("Non-blocking observations:")
            lines.append(observations)

    if advisory:
        lines.append("")
        lines.append("Non-blocking holistic complaints:")
        for c in advisory:
            lines.append(
                f"- {c.get('kind', 'violation')} -- {c.get('location', '?')} -- "
                f"{c.get('violation', '')} -- {c.get('fix', '')}"
            )

    if inspected:
        lines.append("")
        lines.append("Inspected, not test-proven:")
        for v in inspected:
            lines.append(
                f'- "{short_text(v)}" -- evidence: {str_field(v, "evidence")} -- '
                f'no test applies: {str_field(v, "no_test_reason")}'
            )

    # Per-criterion evidence appendix (MET entries, for auditability).
    if proven:
        lines.append("")
        lines.append("Met criteria (evidence):")
        for v in proven:
            text = (v.get("text") or "").strip().replace("\n", " ")
            if len(text) > 100:
                text = text[:97] + "…"
            lines.append(f'- "{text}" — {(v.get("evidence") or "").strip()}')

    exec_results = state.get("exec_results")
    old_failures = [
        r for r in exec_results if isinstance(r, dict) and pre_existing(r)
    ] if isinstance(exec_results, list) else []
    if old_failures:
        lines.append("")
        lines.append("Pre-existing failures (not blocking):")
        for r in old_failures:
            lines.append(f"- `{r.get('cmd', '?')}` -- exit {r.get('exit', '?')} -- {baseline_text(r)}")
            lines.extend(render_tail(r))

    lines.append("")
    lines.append("Verification runs:")
    lines.extend(render_exec_results(exec_results))

    print(json.dumps({"adv_report": "\n".join(lines)}))


try:
    main()
except Exception as e:  # noqa: BLE001 — never a silent verdict
    print(
        json.dumps(
            {
                "adv_report": (
                    "ADVERSARIAL_REVIEW: DIVERGES\n"
                    f"Criteria: verdict computation error: {e} — treat as failed, not as conforming."
                )
            }
        )
    )
