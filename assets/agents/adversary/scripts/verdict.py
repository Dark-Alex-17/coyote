#!/usr/bin/env python3
"""Deterministic conformance verdict for the adversary graph.

CONFORMS requires: at least one criterion, every criterion MET or INSPECTED
(met by inspection: no test could prove it, cited at file:line), zero
blocking holistic complaints, zero pipeline faults, zero blocking red
verification runs, and zero implementation-debris hits from debris_lint.py.
A red run (nonzero exit or timeout) blocks unless run_checks.py found the
same failure on the diff base (baseline status "pre-existing"); those are
reported in their own section. A red run whose baseline status is
"tree-state" (it exits 0 on a snapshot of the pre-run tree) and whose pass
condition is exit 0 does not block
unless a parsed criterion naming it ends neither MET nor INSPECTED, and
neither does one whose pass condition, recomputed from the criteria and
the request by pass_condition.py, is stated as something other than exit 0,
when parsed criteria name it and every one of them ends MET or INSPECTED;
both are reported as advisory red runs. A stated red run that no parsed
criterion names blocks, and so does a red run that timed out (exit -1) or was
killed by a signal (exit 128 or above), whatever its stated condition or
baseline status. A red run whose record says every failure in its output is
one the caller declared carried (`carried.all`, from run_checks.py) is
advisory too, unless a parsed criterion naming it ends neither MET nor
INSPECTED; a red run carrying only some of them blocks, and its complaint
names the failures not declared carried and the count of failure lines that
name no repo path. A `carried_note` on a record and a `carried_error` in the
state are reported as advisory lines. A
run that exited 0 under a stated condition is judged the same way as a
stated red run and is never a green run. A
command the runner's deadline skipped is
reported as not run and does not block by itself: the criterion checks judge
the criteria it covers. An ENVIRONMENT runner-error marker blocks when
commands were declared, since the declared commands never ran.
An INSPECTED verdict whose no_test_reason starts with "ENVIRONMENT:" (an
execution criterion judged from the diff because no recorded run covered
it) counts as met only when at least one recorded run is green (exit 0, not
skipped, held to exit 0); otherwise it is PARTIAL with a complaint naming the command to
declare, so nothing executed never conforms.
Holistic complaints block unless they carry `blocking: false`, or are scope
drift whose every location path lies under the configured `debris_exclude`
dirs (the run's own planning files); both are reported as non-blocking. A
criterion whose check output stayed unreadable (UNVERIFIED) takes the
holistic pass's format ruling for it when one exists
and passes the same contract crit_gate.py enforces (crit_contract.py), and
is otherwise a PIPELINE-FAULT naming the rejection. Rulings for any other
criterion are ignored, so the holistic pass never changes a criterion it did
not have to settle.
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
import re
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".shared"))

MAX_FAULT_DETAIL_CHARS = 300
MAX_MARKER_CHARS = 200
MAX_SECTION_TEXT_CHARS = 100
DIED_MARKER = "criterion check DIED (pipeline fault)"
DIED_EVIDENCE_PREFIX = "PIPELINE-FAULT: criterion "
RULED_MARKER = "(ruled by holistic: output format failed)"
ENVIRONMENT_PREFIX = "ENVIRONMENT:"
DECLARE_RE = re.compile(r"^declare\s+(.+?)\s+via the verification_commands variable", re.S)
PLANNING_DRIFT_NOTE = "not blocking: scope drift confined to the run's planning files"
SCOPE_DRIFT_KIND_RE = re.compile(r"scope drift|out-of-scope", re.I)
BARE_LINE_REF_RE = re.compile(r":\d+(?:[-:]\d+)*")
LINE_ANCHOR_RE = re.compile(r"(?::L?\d+(?:[-:]L?\d+)*|#L\d+(?:-L?\d+)?)$")
FILE_NAME_RE = re.compile(r"[\w.-]*\w\.\w+")


def died(v):
    evidence = v.get("evidence")
    return isinstance(evidence, str) and evidence.startswith(DIED_EVIDENCE_PREFIX)


def blocking(complaint):
    """Only an explicit `blocking: false` makes a holistic complaint advisory."""
    return complaint.get("blocking") is not False


def configured_excludes(state):
    """The checked `debris_exclude` dirs, or [] when none are configured or
    the value is unusable (debris_lint.py already faults on the latter)."""
    try:
        import debris

        return debris.parse_excludes(state.get("debris_exclude"))
    except (ImportError, ValueError):
        return []


def outside_names(state, excludes):
    """Basenames of the changed files that lie outside `excludes`."""
    import debris

    files = state.get("changed_files")
    if not isinstance(files, list):
        return set()
    return {
        f.rstrip("/").rsplit("/", 1)[-1]
        for f in files
        if isinstance(f, str) and not debris.under_excludes(f, excludes)
    }


def planning_drift(complaint, excludes, outside):
    """Whether a holistic complaint is scope drift located only in the run's
    planning files.

    File-like tokens of the location (a `/` in them, a file name with an
    extension, a `:N`, `:LN` or `#LN` line anchor, or a name in `outside`,
    the basenames of changed files outside `excludes`) must all lie under
    `excludes`, at least one must be present, and a bare `:N` line reference
    counts under the path before it.
    """
    kind = complaint.get("kind")
    location = complaint.get("location")
    if not excludes or not isinstance(kind, str) or not isinstance(location, str):
        return False
    if not SCOPE_DRIFT_KIND_RE.search(kind):
        return False
    import debris

    paths = 0
    for raw in re.split(r"[\s,;]+", location):
        token = raw.strip("()[]{}<>\"'`").rstrip(".:")
        if BARE_LINE_REF_RE.fullmatch(token):
            if not paths:
                return False
            continue
        anchor = LINE_ANCHOR_RE.search(token)
        path = token[: anchor.start()] if anchor else token
        if path in outside:
            return False
        if not anchor and "/" not in path and not FILE_NAME_RE.fullmatch(path):
            continue
        if not debris.under_excludes(path, excludes):
            return False
        paths += 1
    return paths > 0


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

    A ruling whose id names a verdict in any other status is ignored. A
    ruling that fails the criterion contract leaves the verdict UNVERIFIED
    with the rejection reason as its complaint. The contract module is
    imported only when a ruling must be checked; when it cannot be imported
    no ruling is applied, so the verdicts stay UNVERIFIED (fail-closed).
    """
    by_id = {}
    for r in rulings if isinstance(rulings, list) else []:
        if isinstance(r, dict) and isinstance(r.get("id"), str):
            by_id.setdefault(r["id"].strip(), r)
    out = []
    for v in verdicts:
        ruling = by_id.get(v.get("id")) if v.get("status") == "UNVERIFIED" and not died(v) else None
        if ruling is None:
            out.append(v)
            continue
        try:
            from crit_contract import validate
        except ImportError as e:
            original = str_field(v, "complaint")
            rejected = f"holistic format ruling not applied: criterion contract unavailable ({e})"
            out.append(dict(v, complaint=f"{original}; {rejected}" if original else rejected))
            continue
        try:
            status, evidence, complaint, no_test_reason = validate(ruling)
        except ValueError as e:
            original = str_field(v, "complaint")
            rejected = f"holistic format ruling rejected: {e}"
            out.append(dict(v, complaint=f"{original}; {rejected}" if original else rejected))
            continue
        evidence = evidence.strip()
        complaint = complaint.strip()
        out.append(
            dict(
                v,
                status=status,
                evidence=f"{RULED_MARKER} {evidence}".strip(),
                complaint=f"{RULED_MARKER} {complaint}".strip() if complaint else "",
                no_test_reason=no_test_reason,
            )
        )
    return out


def unverified_faults(verdicts):
    return [
        f'PIPELINE-FAULT: criterion verdict unreadable after re-asks, and the holistic '
        f'pass gave no usable format ruling for it -- "{short_text(v)}" '
        f"({(v.get('complaint') or '').strip()})"
        for v in verdicts
        if v.get("status") == "UNVERIFIED" and not died(v)
    ]


def green_run(exec_results, criteria, request):
    """Some recorded run exited 0 and is held to exit 0; exiting 0 proves nothing about a
    stated condition."""
    from pass_condition import EXIT_0, resolve

    return isinstance(exec_results, list) and any(
        isinstance(r, dict)
        and not r.get("skipped")
        and r.get("exit") == 0
        and resolve(r.get("cmd"), criteria, request)["condition"] == EXIT_0
        for r in exec_results
    )


def declared_command(reason):
    rest = reason[len(ENVIRONMENT_PREFIX) :].strip()
    m = DECLARE_RE.match(rest)
    return (m.group(1) if m else rest).strip()


def demote_unexecuted(verdicts, exec_results, criteria, request):
    """PARTIAL for each ENVIRONMENT-INSPECTED verdict when no recorded run is green."""
    if green_run(exec_results, criteria, request):
        return verdicts
    out = []
    for v in verdicts:
        reason = str_field(v, "no_test_reason")
        if v.get("status") != "INSPECTED" or died(v) or not reason.startswith(ENVIRONMENT_PREFIX):
            out.append(v)
            continue
        complaint = (
            "judged from the diff only and nothing was executed (no recorded "
            f"verification run is green); declare {declared_command(reason)} via the "
            "verification_commands variable so it runs"
        )
        out.append(dict(v, status="PARTIAL", complaint=complaint))
    return out


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


def exec_run_complaints(exec_results, declared, criteria=None, request="", verdicts=()):
    """(complaints, advisory): one complaint per blocking red verification run
    recorded by run_checks.py, and one report line per red run that does not block.

    A red run is a record that exited nonzero (including the -1 TIMEOUT
    sentinel). A "pre-existing" one is reported in its own section. A
    red run that timed out or was killed by a signal blocks whatever its
    baseline. For the rest the pass condition is recomputed from `criteria`
    and `request`, never read from the record, so a recorded run replays the
    same. One whose pass
    condition a parsed criterion states blocks unless every criterion naming
    it has a final status MET or INSPECTED in `verdicts`, and is otherwise
    advisory with their evidence as the reason; one whose condition only the
    request states blocks, whatever its baseline status. A run that exited 0
    under a stated condition is gated the same way. A red run held to exit 0
    with a "tree-state" baseline blocks unless every criterion naming it ends
    MET or INSPECTED, and is otherwise advisory; so does one whose `carried`
    says every failure is a carried one. Any other red run blocks and the
    complaint names the baseline outcome and any failures not declared
    carried. A record's `carried_note` is an advisory line. A record the deadline skipped is
    not a run and yields nothing here. An "ENVIRONMENT: ..." runner-error
    marker with a non-empty declaration is one complaint: the declared
    commands never ran. Other string markers ("none declared",
    "PIPELINE-FAULT...") are not runs and yield nothing.
    """
    if isinstance(exec_results, str):
        marker = exec_results.strip().replace("\n", " ")
        if not marker.startswith("ENVIRONMENT:") or not declared_nonempty(declared):
            return [], []
        if len(marker) > MAX_MARKER_CHARS:
            marker = marker[: MAX_MARKER_CHARS - 1] + "…"
        return [
            f"Verification runner error — the declared command(s) never ran ({marker}); "
            "unproven runs block CONFORMS"
        ], []
    if not isinstance(exec_results, list):
        return [], []
    from pass_condition import STATED, resolve

    by_id = {}
    for v in verdicts:
        by_id.setdefault(v.get("id"), v)
    out = []
    advisory = []
    for r in exec_results:
        if not isinstance(r, dict) or r.get("skipped") or pre_existing(r):
            continue
        cmd = r.get("cmd", "?")
        code = r.get("exit")
        condition = resolve(cmd, criteria, request)
        if code == 0:
            if condition["condition"] == STATED:
                blocks, text = stated_red(r, "exit 0", condition, by_id)
                (out if blocks else advisory).append(text)
            continue
        outcome = "TIMEOUT (exit -1)" if code == -1 else f"exit {code}"
        judged = not killed_or_timed_out(code)
        if note := str_field(r, "carried_note"):
            advisory.append(f"- `{cmd}` -- carried failures {note}")
        if judged and condition["condition"] == STATED:
            blocks, text = stated_red(r, outcome, condition, by_id)
            (out if blocks else advisory).append(text)
            continue
        if judged and baseline_status(r) == "tree-state":
            blocks, text = tree_state_red(r, outcome, condition["criteria"], by_id)
            (out if blocks else advisory).append(text)
            continue
        carried = r.get("carried") if isinstance(r.get("carried"), dict) else {}
        if judged and carried.get("all") is True:
            blocks, text = carried_red(r, outcome, condition["criteria"], by_id)
            (out if blocks else advisory).append(text)
            continue
        out.append(
            f"Verification run `{cmd}` — {outcome} (recorded FAIL; a red declared "
            f"command that did not already fail on the diff base blocks CONFORMS; {baseline_text(r)}"
            f"{carried_shortfall(carried)})"
        )
    return out, advisory


def killed_or_timed_out(code):
    return type(code) is int and (code == -1 or code >= 128)


def naming_judged_met(ids, by_id):
    """(every criterion in `ids` ends MET or INSPECTED, [(id, final status)] per criterion)."""
    naming = [(cid, final_status(by_id.get(cid))) for cid in ids]
    return all(status in ("MET", "INSPECTED") for _, status in naming), naming


def met_reasons(naming, by_id):
    return "; ".join(f"{cid} {status}: {str_field(by_id[cid], 'evidence')}" for cid, status in naming)


def tree_state_red(record, outcome, ids, by_id):
    """(blocks, text) for a red run that exits 0 on a clean checkout of the same content.
    A criterion naming the command may be about the uncommitted tree itself (files the
    change must leave untouched), so such a run blocks unless those criteria all end met."""
    cmd = record.get("cmd", "?")
    met, naming = naming_judged_met(ids, by_id)
    if met:
        reasons = f"{met_reasons(naming, by_id)} -- " if naming else ""
        return False, f"- `{cmd}` -- {outcome} -- {reasons}{baseline_text(record)}"
    statuses = ", ".join(f"{cid} {status}" for cid, status in naming)
    return True, (
        f"Verification run `{cmd}` -- {outcome} (recorded FAIL; red only in the uncommitted live "
        f"tree, and the criteria naming it are not all met: {statuses}; {baseline_text(record)})"
    )


def carried_red(record, outcome, ids, by_id):
    """(blocks, text) for a red run whose every failure the caller declared carried. A
    criterion naming the command may ask for it to pass, so such a run blocks unless those
    criteria all end met."""
    cmd = record.get("cmd", "?")
    met, naming = naming_judged_met(ids, by_id)
    if met:
        reasons = f"{met_reasons(naming, by_id)} -- " if naming else ""
        return False, f"- `{cmd}` -- {outcome} -- {reasons}{carried_text(record)} -- {baseline_text(record)}"
    statuses = ", ".join(f"{cid} {status}" for cid, status in naming)
    return True, (
        f"Verification run `{cmd}` -- {outcome} (recorded FAIL; {carried_text(record)}, and the "
        f"criteria naming it are not all met: {statuses}; {baseline_text(record)})"
    )


def failure_keys(keys):
    return [", ".join(str(p) for p in k) for k in keys if isinstance(k, list)]


def carried_text(record):
    """The matched carried failures grouped by owner, in the order they were located."""
    groups = {}
    for m in record["carried"].get("matched") or []:
        if isinstance(m, dict):
            owner = m.get("owner")
            label = "?" if owner is None or owner == "" else str(owner)
            groups.setdefault(label, []).extend(failure_keys([m.get("paths")]))
    owned = "; ".join(f"(owner {label}): {', '.join(keys)}" for label, keys in groups.items())
    return f"every failure is one the caller declared carried {owned}"


def carried_shortfall(carried):
    """The complaint suffix for a red run only some of whose failures the caller declared carried."""
    text = ""
    if new := failure_keys(carried.get("new") or []):
        text += f"; failures not declared carried: {'; '.join(new)}"
    unlocated = carried.get("unlocated")
    if type(unlocated) is int and unlocated > 0:
        text += f"; {unlocated} unlocated failure line(s)"
    return text


def stated_red(record, outcome, condition, by_id):
    """(blocks, text) for a red or exit-0 run whose pass condition is stated as something
    other than exit 0 by the parsed criteria naming it, or only by the request when none does."""
    cmd = record.get("cmd", "?")
    recorded = "recorded FAIL" if record.get("exit") != 0 else "not green"
    ids = condition["criteria"]
    if not ids:
        return True, (
            f"Verification run `{cmd}` -- {outcome} ({recorded}; its pass condition is stated "
            f"only in the request (\"{condition['line']}\") and no acceptance criterion names it; "
            f"{baseline_text(record)})"
        )
    met, naming = naming_judged_met(ids, by_id)
    if met:
        return False, f"- `{cmd}` -- {outcome} -- {met_reasons(naming, by_id)} -- {baseline_text(record)}"
    statuses = ", ".join(f"{cid} {status}" for cid, status in naming)
    return True, (
        f"Verification run `{cmd}` -- {outcome} ({recorded}; the criteria naming it state a "
        f"pass condition other than exit 0 and are not all met: {statuses}; {baseline_text(record)})"
    )


def final_status(verdict):
    if verdict is None:
        return "no verdict"
    return "DIED" if died(verdict) else str(verdict.get("status", "?"))


def pre_existing(record):
    return baseline_status(record) == "pre-existing"


def baseline_status(record):
    baseline = record.get("baseline")
    return baseline.get("status") if isinstance(baseline, dict) else None


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


def skipped_note(exec_results):
    """Header note counting deadline-skipped commands; they never block."""
    if not isinstance(exec_results, list):
        return ""
    skipped = sum(1 for r in exec_results if isinstance(r, dict) and r.get("skipped"))
    if not skipped:
        return ""
    return f"{skipped} declared command(s) not run (deadline), judged by the criterion checks"


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
    verdicts = demote_unexecuted(
        verdicts, state.get("exec_results"), state.get("criteria"), state.get("initial_prompt")
    )
    holistic = [c for c in (state.get("extra_complaints") or []) if isinstance(c, dict)]
    excludes = configured_excludes(state)
    outside = outside_names(state, excludes) if excludes else set()
    extra = []
    advisory = []
    for c in holistic:
        if not blocking(c):
            advisory.append((c, ""))
        elif planning_drift(c, excludes, outside):
            advisory.append((c, PLANNING_DRIFT_NOTE))
        else:
            extra.append(c)
    observations = (state.get("observations") or "").strip()
    faults = collect_pipeline_faults(state) + unverified_faults(verdicts)
    reds, advisory_reds = exec_run_complaints(
        state.get("exec_results"),
        state.get("verification_commands"),
        state.get("criteria"),
        state.get("initial_prompt"),
        verdicts,
    )
    if carried_error := str_field(state, "carried_error"):
        advisory_reds.append(f"- {carried_error}")
    n_debris, debris_hits = debris_complaints(state)
    not_run = skipped_note(state.get("exec_results"))
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
            f"{len(inspected)} by inspection)"
            + (f"; {not_run}" if not_run else "")
            + "."
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
        if not_run:
            notes.append(not_run)
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
        for c, note in advisory:
            lines.append(
                f"- {c.get('kind', 'violation')} -- {c.get('location', '?')} -- "
                f"{c.get('violation', '')} -- {c.get('fix', '')}"
                + (f" ({note})" if note else "")
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

    if advisory_reds:
        lines.append("")
        lines.append("Advisory red runs (not blocking):")
        lines.extend(advisory_reds)

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
