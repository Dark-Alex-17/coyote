"""The architect-next gate functions - one per script node.

Each function takes the graph state and returns the updates to merge, including
`_next` when the node routes dynamically. Entry scripts call `run("<name>")`.
Gates on the critical path raise on internal errors (the engine then takes the
node's declared fallback and records last_script_error); advisory gates catch
and declare their degradation instead.
"""

import hashlib
import json
import os
import re
import subprocess
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _lib import (  # noqa: E402
    AMEND_CYCLES_MAX, ARCHIVIST_AVAILABLE, CI_FLAKE_RERUNS, CI_MAX_ROUNDS, CI_POLL_FAILS, CI_POLL_SLEEP,
    CI_WALL_SECS, CODE_VAR, CONFLICT_CYCLES_MAX, CONVENTIONS_LINE, CORE_TOOLS_REL, DESIGN_ROUNDS_MAX,
    GATE1_HOUSE_LINE, IDIOM_LINE, IS_UPSTREAM, LOG_EVENT_LINE, MODE_ENUMS, MULTI_IS_GROUPS,
    NO_COMMIT_MAIN_LINE, OWNER_POLL_MAX, OWNER_POLL_MINUTES, OWNER_REMOTE_INPUT, OWNER_WAIT_MINUTES,
    PLANNING_VAR, PR_URL_RE, PROBE_HOUSE_NOTE, REPO_HINT, RERUN_ONCE, RESUME_RE, SHA_RE, SIBLING_HINT,
    SISYPHUS_RUN_RE, STACK_LINKING, STORE_LABEL, STORE_TOOL_PREFIX, STRIKE_LIMIT, TASKSTORE, TASK_ID_RE,
    TEST_FILE_RE, VERIFY_CMD_SECS, VERIFY_SET_SECS, WAVE_CAP,
    append_artifact, as_dict, as_int, as_list, commit_planning, context_pack, core_tool, emit,
    ensure_run_dir, escalation, frontmatter, git, git_out, harvest_anomalies, harvest_anomaly_lines,
    is_micro, is_parallel, is_single, ledger, load_state, load_tasks, log_event, looks_executable,
    match_option, mode, new_run_id, new_task, note_script_error, now_iso, option, plan_section,
    raise_escalation, read_artifact, read_file, run_dir_for, save_tasks, set_frontmatter,
    set_task_status, sh, snapshot, tail, task_by_id, task_dir_for, task_section, today, truthy, var,
    write_artifact, write_file,
)

SHARED_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".shared")
GAUNTLET_LANES = ("code-review", "adversary", "security", "probe", "debris")
HOTSPOT_RE = re.compile(
    r"(^|/)migrations?/|\.sql$|_gen\.go$|\.pb\.go$|generated/|(^|/)go\.(mod|sum)$|package(-lock)?\.json$|"
    r"yarn\.lock$|pnpm-lock\.yaml$|Cargo\.(toml|lock)$|requirements.*\.txt$|poetry\.lock$",
    re.I,
)


# ====================================================================== 2.1 orient
def _normalize_paths(state):
    """D1: both forks' variables -> one state vocabulary (design doc section 7)."""
    u = {}
    if not IS_UPSTREAM:
        store = os.path.abspath(os.path.expanduser(var(state, PLANNING_VAR, "")))
        code = os.path.abspath(os.path.expanduser(var(state, CODE_VAR, ".")))
        u.update({
            "plans_root": store, "tasks_root": os.path.join(store, "tasks"), "code_repo_path": code,
            "base_branch_name": "main", "owner_name": var(state, "owner", ""),
            "reference_root_path": os.path.abspath(os.path.expanduser(var(state, "reference_root", ""))),
        })
    else:
        code = os.path.abspath(os.path.expanduser(var(state, "project_dir", ".")))
        plans = var(state, "plans_dir", "plans")
        plans = plans if os.path.isabs(plans) else os.path.join(code, plans)
        u.update({
            "plans_root": plans, "tasks_root": os.path.join(plans, "tasks"), "code_repo_path": code,
            "base_branch_name": var(state, "base_branch", "main") or "main", "owner_name": "",
            "reference_root_path": "",
        })
    u["core_tools"] = CORE_TOOLS_REL
    return u


def intake(state):
    brief = state.get("initial_prompt") or ""
    u = _normalize_paths(state)
    mv = mode(state)
    bad = {k: v for k, v in mv.items() if v not in MODE_ENUMS[k]}
    u["mode_vector"] = mv
    u["mode_line"] = (f"pr_strategy={mv['pr_strategy']} parallel_tasks={mv['parallel_tasks']} "
                      f"task_granularity={mv['task_granularity']} autonomy={mv['autonomy']} "
                      f"escalation_policy={mv['escalation_policy']}")
    u["brief"] = brief
    u["sizing_rule"] = (
        "each task is micro-sized: one file or one tight seam (hard cap 3 files including tests; the compile-atomicity "
        "floor wins over the cap), exact signatures/types/target paths stated, and every acceptance criterion an EXECUTABLE "
        "COMMAND with its expected output - a prose criterion is a plan defect. The table carries a `Batch` column (one batch "
        "per plan section) and a `Protected` column (money / auth / secrets / migrations). Every task gets a `Context pack` "
        "subsection with the exact snippets and signatures INLINE. Author it per plan-authoring's Micro-grain addendum."
        if mv["task_granularity"] == "micro" else
        "each task is sized to ~1 engineer-day (the single most important discipline - decompose anything bigger)."
    )
    away = os.path.join(os.path.expanduser("~"), ".config", "coyote", "share", "away")
    u["owner_away"] = OWNER_REMOTE_INPUT and os.path.isfile(away)
    u["top_level"] = True
    code = u["code_repo_path"]
    u["git_root"] = git_out(code, "rev-parse", "--show-toplevel", default="")

    # Design doc: the variable, else the first .md path in the prompt.
    dd = var(state, "design_doc", "")
    if not dd:
        m = re.search(r"(?<!\S)((?:~|/|\.{1,2}/)?[\w./-]+\.md)\b", brief)
        dd = m.group(1) if m else ""
    if dd and not os.path.isabs(dd):
        for base in (u["plans_root"], code, os.getcwd()):
            cand = os.path.join(base, dd)
            if os.path.isfile(cand):
                dd = cand
                break
    u["design_doc_path"] = os.path.expanduser(dd) if dd else ""

    # Owner identity: the store's whoami file wins over the variable (dirstate store).
    if not IS_UPSTREAM:
        who = read_file(os.path.join(u["plans_root"], "whoami"))
        if m := re.search(r"(?m)^name:\s*(\S+)", who):
            if m.group(1) != u["owner_name"]:
                u["owner_note"] = f"owner variable {u['owner_name']!r} disagrees with whoami {m.group(1)!r}; whoami wins"
                u["owner_name"] = m.group(1)
        if os.path.isdir(os.path.join(u["plans_root"], ".git")):
            git(u["plans_root"], "pull", "--quiet", "--ff-only")
        u["tasks_root"] = os.path.join(u["plans_root"], "tasks")

    # Re-entry (A2).
    m = RESUME_RE.search(brief)
    prior = {}
    if m:
        raw = read_artifact(run_dir_for(m.group(1)), "state.json")
        try:
            prior = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            prior = {}
    if prior:
        run_id = m.group(1)
        u.update({"run_id": run_id, "run_dir": ensure_run_dir(run_id), "resume_mode": True})
        for k, v in prior.items():
            if k in ("phase", "updated_at") or v is None:
                continue
            if k in ("mode_vector", "code_repo_path", "tasks_root", "plans_root", "base_branch_name", "owner_name",
                     "reference_root_path"):
                continue  # recomputed from the live variables
            u[k] = v
        u["resume_phase"] = prior.get("phase", "")
        raw_tasks = read_artifact(u["run_dir"], "tasks.json")
        try:
            u["tasks"] = json.loads(raw_tasks) if raw_tasks else []
        except json.JSONDecodeError:
            u["tasks"] = []
        append_artifact(u["run_dir"], "brief.md", f"\n\n## Re-entry {now_iso()}\n\n{brief}")
        ledger(state, u, f"run: re-entry into {run_id} at phase {u['resume_phase']!r}")
        u["_next"] = "resume_route"
    else:
        if m:
            u["resume_note"] = f"The prompt names run {m.group(1)} but no durable state exists for it; treated as a fresh run."
        run_id = state.get("run_id") or new_run_id(brief)
        u.update({"run_id": run_id, "run_dir": ensure_run_dir(run_id)})
        write_artifact(u["run_dir"], "brief.md", brief)
        ledger(state, u, f"run: {run_id} mode[{u['mode_line']}] code={code} plans={u['plans_root']}")
        if u.get("owner_note"):
            ledger(state, u, u["owner_note"])
        if bad:
            snapshot(state, u, "intake")
            return raise_escalation(state, u, escalation(
                "mode_invalid", f"Unknown mode value(s): {bad}. Which value should apply?",
                [option(f"{k} = {MODE_ENUMS[k][0]}", recommended=True, evidence="the documented default") for k in bad]
                + [option("Abort the run")], tier3_ok=False))
        if not u["design_doc_path"] or not os.path.isfile(u["design_doc_path"]):
            snapshot(state, u, "intake")
            return raise_escalation(state, u, escalation(
                "need_design_doc", "No readable design doc was given (design_doc variable or a .md path in the message). "
                                   "Reply with the path, or abort.", [option("Abort the run")], tier3_ok=False))
        u["_next"] = "orient_read"
    snapshot(state, u, "intake")
    return u


def resume_route(state):
    u = {}
    phase = state.get("resume_phase") or ""
    if state.get("parked"):
        u["_next"] = "escalate_gate"
    elif phase.startswith("phase_f") or phase.startswith("ci") or phase.startswith("pr"):
        u["_next"] = "phase_f_gate"
    elif phase.startswith("task") or phase.startswith("wave") or phase.startswith("batch"):
        u["_next"] = "select_tasks"
    elif truthy(state.get("sealed")) and state.get("gate2_verdict") == "OKAY":
        u["_next"] = "select_tasks" if as_list(state.get("tasks")) else "promote_tasks"
    elif state.get("plan_path"):
        u["_next"] = "reseal_gate"
    else:
        u["_next"] = "orient_read"
    ledger(state, u, f"resume: phase {phase!r} -> {u['_next']}")
    return u


def orient_fault(state):
    u = {}
    note_script_error(state, u, "orient_read")
    return raise_escalation(state, u, escalation(
        "clarify", f"The design doc at {state.get('design_doc_path')} could not be read/oriented. Give the correct path, or abort.",
        [option("Abort the run")], tier3_ok=False))


# ====================================================================== 2.2 design
def research_dispatch(state):
    u = {}
    harvest_anomalies(state, u, "orient_read")
    harvest_anomalies(state, u, "plan_research")
    note_script_error(state, u, "plan_research")
    slices = [s for s in as_list(state.get("explore_slices")) if isinstance(s, str) and s.strip()]
    if not slices:
        slices = [f"Confirm the design doc's assumptions about the code in {state.get('code_repo_path')}: the files, "
                  f"symbols, call sites and conventions the task breakdown will name (design doc: {state.get('design_doc_path')})"]
        u["explore_slices"] = slices
        ledger(state, u, "research: plan_research produced no slices; degraded to one grounding slice")
    arch = [q for q in as_list(state.get("archivist_questions")) if isinstance(q, str) and q.strip()]
    u["archivist_questions"] = arch if ARCHIVIST_AVAILABLE else []
    if arch and not ARCHIVIST_AVAILABLE:
        ledger(state, u, "research: archivist lane unavailable in this fork; org-context questions carried to the plan's Open questions")
        u["carried_questions"] = arch
    u["librarian_questions"] = [q for q in as_list(state.get("librarian_questions")) if isinstance(q, str) and q.strip()]
    team = truthy(state.get("team_mode")) and len(slices) > 1
    u["_next"] = "explore_fanout_team" if team else "explore_fanout_solo"
    return u


def explore_fault(state):
    return {"finding": f"PIPELINE-FAULT: explore slice {state.get('slice', '')!r} produced no result ({tail(state.get('finding', ''), 300)})"}


def archivist_fault(state):
    return {"finding": f"PIPELINE-FAULT: archivist question {state.get('question', '')!r} produced no result"}


def librarian_fault(state):
    return {"finding": f"PIPELINE-FAULT: librarian question {state.get('question', '')!r} produced no result"}


def checklist_fault(state):
    return {"finding": f"PIPELINE-FAULT: checklist for {state.get('label', '')!r} unavailable"}


def _write_research(state, u):
    lines = ["# Research findings (verbatim)", ""]
    for name in ("explore_findings", "archivist_findings", "librarian_findings"):
        items = as_list(state.get(name))
        if items:
            lines += [f"## {name}", ""] + [f"### {i}\n\n{x}\n" for i, x in enumerate(items, 1)]
    write_artifact(state.get("run_dir", ""), "research.md", "\n".join(lines))


def plan_route(state):
    u = {}
    harvest_anomalies(state, u, "author_plan")
    note_script_error(state, u, "author_plan")
    _write_research(state, u)
    path = state.get("plan_path") or ""
    text = read_file(path) if path else ""
    retries = as_int(state.get("author_retries"))
    defects = []
    if not text.strip():
        defects.append(f"plan file missing or empty at {path!r}")
    else:
        fm, body = frontmatter(text)
        if not re.search(r"(?im)^##\s+task breakdown", body):
            defects.append("no `## Task breakdown` section")
        rows = [l for l in plan_section(body, r"Task breakdown").splitlines() if l.strip().startswith("|")]
        rows = [r for r in rows if not re.match(r"^\|\s*-", r) and "Task" not in r.split("|")[1] if len(r.split("|")) > 1]
        if not rows:
            defects.append("task breakdown table has no rows")
        if is_micro(state):
            if rows and not re.search(r"(?im)^\|[^\n]*\bBatch\b", plan_section(body, r"Task breakdown")):
                defects.append("micro mode: the breakdown table has no `Batch` column")
            crit = [l.strip()[1:].strip() for l in body.splitlines()
                    if re.match(r"^\s*[-*]\s+", l) and re.search(r"(?i)criteri", "\n".join(body.splitlines()[:0]) + l) is None and "`" not in l
                    and re.search(r"(?i)\b(must|should|returns?|passes|green|exit)\b", l)]
            bad = [c for c in as_list(state.get("acceptance_criteria")) if isinstance(c, str) and not looks_executable(c)]
            if bad:
                defects.append("micro mode: non-executable acceptance criteria: " + "; ".join(bad[:5]))
    if defects:
        if retries < RERUN_ONCE:
            u["author_retries"] = retries + 1
            u["author_defects"] = "\n".join(f"- {d}" for d in defects)
            ledger(state, u, f"plan: structural defects -> one author re-run: {defects}")
            u["_next"] = "author_plan"
            return u
        return raise_escalation(state, u, escalation(
            "plan_unauthored", "The plan could not be authored to the structural bar after a retry. Abort, or continue "
                               "with the plan as written (the gatekeeper will judge it)?",
            [option("Abort the run", recommended=True), option("Continue to the gates with the plan as written")],
            evidence="\n".join(defects), tier3_ok=False))
    u["plan_revision"] = hashlib.sha1(text.encode("utf-8", "replace")).hexdigest()[:10]
    fm, _ = frontmatter(text)
    if not state.get("plan_slug"):
        u["plan_slug"] = fm.get("slug") or re.sub(r"[^a-z0-9-]+", "-", (fm.get("title") or os.path.basename(os.path.dirname(path)) or "plan").lower()).strip("-")
    ledger(state, u, f"plan: authored at {path} (rev {u['plan_revision']})")
    carried = as_list(state.get("carried_questions"))
    if carried:
        oq = list(as_list(state.get("open_questions")))
        oq += [{"question": q, "options": [], "changes_scope": False} for q in carried]
        u["open_questions"] = oq
        u["carried_questions"] = []
    snapshot(state, u, "plan_authored")
    u["_next"] = "open_questions_gate"
    return u


def open_questions_gate(state):
    u = {}
    qs = [q for q in as_list(state.get("open_questions")) if isinstance(q, dict) and q.get("question")]
    if not qs:
        u["_next"] = "quality_bar_gate"
        return u
    rounds = as_int(state.get("design_rounds"))
    if rounds >= DESIGN_ROUNDS_MAX:
        scope = [q for q in qs if truthy(q.get("changes_scope"))]
        if scope:
            u["failure_reason"] = (f"open design questions still unresolved after {DESIGN_ROUNDS_MAX} rounds and at least one "
                                   f"changes scope: {scope[0]['question']}")
            ledger(state, u, u["failure_reason"])
            u["_next"] = "end_blocked"
            return u
        ledger(state, u, f"design: {len(qs)} non-scope question(s) carried as plan Open questions after {rounds} rounds")
        u["open_questions"] = []
        u["_next"] = "quality_bar_gate"
        return u
    lines, opts = [], []
    for i, q in enumerate(qs, 1):
        lines.append(f"{i}. {q['question']}")
        for o in as_list(q.get("options")):
            if isinstance(o, dict):
                mark = " (recommended)" if o.get("recommended") else ""
                ev = f" -- {o.get('evidence')}" if o.get("evidence") else ""
                lines.append(f"   - {o.get('text', '')}{mark}{ev}")
    u["design_rounds"] = rounds + 1
    return raise_escalation(state, u, escalation(
        "design_round",
        f"Design frontier round {rounds + 1}: answer each numbered question (a number alone takes its recommended option).\n"
        + "\n".join(lines), [], evidence="", data={"questions": qs}))


def quality_bar_gate(state):
    u = {}
    if state.get("quality_rigor") and state.get("quality_surfaces") is not None and state.get("quality_recorded"):
        u["_next"] = "checklist_gate"
        return u
    rigor = state.get("rigor_proposal") or "production"
    surfaces = state.get("surfaces_proposal") or ""
    return raise_escalation(state, u, escalation(
        "quality_bar", f"Quality bar for this plan: rigor and surfaces. Proposal: rigor={rigor}, surfaces={surfaces or '(none)'}.",
        [option(f"rigor={rigor}, surfaces={surfaces}", recommended=True, evidence=state.get("quality_evidence") or "the design doc's own signals"),
         option("rigor=production, surfaces=(as proposed)"), option("rigor=prototype, surfaces=(as proposed)"),
         option("rigor=poc, surfaces=(as proposed)")],
        data={"rigor": rigor, "surfaces": surfaces}))


def record_quality_bar(state):
    u = {}
    path = state.get("plan_path") or ""
    rigor = state.get("quality_rigor") or "production"
    surfaces = state.get("quality_surfaces") or ""
    if path and os.path.isfile(path):
        set_frontmatter(path, "rigor", rigor)
        set_frontmatter(path, "surfaces", surfaces or "[]")
        text = read_file(path)
        if not re.search(r"(?im)^##\s+Quality bar", text):
            write_file(path, text.rstrip("\n") + f"\n\n## Quality bar\n\nrigor: {rigor}\nsurfaces: {surfaces or '(none)'}\n\n"
                                                 "### Dropped practices\n\n(none)\n\n### Long-tail criteria\n\n(none)\n")
    u["quality_recorded"] = True
    ledger(state, u, f"quality bar: rigor={rigor} surfaces={surfaces}")
    u["_next"] = "checklist_gate"
    return u


def checklist_gate(state):
    u = {}
    labels = [s.split(":", 1)[1].strip() for s in re.split(r"[,\s]+", str(state.get("quality_surfaces") or "")) if s.startswith("other:")]
    done = as_list(state.get("checklist_done"))
    todo = [l for l in labels if l and l not in done]
    if not todo:
        u["_next"] = "reseal_gate"
        return u
    u["checklist_labels"] = todo
    u["_next"] = "checklist_fanout"
    return u


def checklist_round(state):
    u = {}
    labels = as_list(state.get("checklist_labels"))
    findings = as_list(state.get("checklist_findings"))
    items = []
    for label, text in zip(labels, findings):
        for line in str(text).splitlines():
            if re.match(r"^\s*[-*\d]+[.)]?\s+\S", line) and "PIPELINE-FAULT" not in line:
                items.append(f"[{label}] " + re.sub(r"^\s*[-*\d]+[.)]?\s+", "", line).strip())
    u["checklist_done"] = list(as_list(state.get("checklist_done"))) + labels
    if not items:
        ledger(state, u, f"checklist: no items returned for {labels}; nothing to accept")
        u["_next"] = "reseal_gate"
        return u
    u["checklist_items"] = items
    return raise_escalation(state, u, escalation(
        "checklist_accept", "Best-practice checklist round: reply with the item numbers to ACCEPT (e.g. '1,3,4'), "
                            "'all', or 'none'. Accepted items become acceptance criteria / long-tail criteria in the plan.",
        [option(it) for it in items[:40]], data={"items": items}))


# ====================================================================== 2.3 gates
def reseal_gate(state):
    u = {}
    harvest_anomalies(state, u, "amend_plan")
    harvest_anomaly_lines(state, u, state.get("amend_summary") or "", "amend_plan")
    note_script_error(state, u, "amend_plan")
    path = state.get("plan_path") or ""
    text = read_file(path)
    rev = hashlib.sha1(text.encode("utf-8", "replace")).hexdigest()[:10] if text else ""
    cycles = as_int(state.get("amend_cycles"))
    if state.get("pending_amendments"):
        # An amendment round just ran (or failed); it counts whether or not the file changed.
        cycles += 1
        u["amend_cycles"] = cycles
        u["pending_amendments"] = []
        changed = rev != state.get("plan_revision")
        ledger(state, u, f"amend: cycle {cycles}/{AMEND_CYCLES_MAX} ({'plan changed' if changed else 'plan unchanged - amendment did not land'})")
        if cycles > AMEND_CYCLES_MAX:
            return raise_escalation(state, u, escalation(
                "amend_cap", f"The plan has been amended {AMEND_CYCLES_MAX} times this run (A1 cap). Continue amending, "
                             "seal as-is and proceed to Gate 2, or stop?",
                [option("Allow one more amendment cycle"), option("Stop and report", recommended=True)],
                evidence=tail(state.get("amend_summary", ""), 1500), hard_trigger=True))
    u.update({"plan_revision": rev, "sealed": False, "gate2_verdict": "", "gate1_verdict": ""})
    snapshot(state, u, "reseal")
    u["_next"] = "gate1_build"
    return u


def _gate_prior(state, key):
    prior = state.get(key) or ""
    return f"\n\nPrior verdict (the plan was amended since; re-audit the FULL plan, not the delta):\n{tail(prior, 3000)}" if prior else ""


def gate1_build(state):
    u = {}
    plan = state.get("plan_path", "")
    code = state.get("code_repo_path", "")
    text = (f"Audit this plan for self-containedness. Return SEALED/LEAKY. Plan: {plan}. Target project: {code}.")
    if GATE1_HOUSE_LINE:
        text += GATE1_HOUSE_LINE
    if is_micro(state):
        text += ("\nAudit to the Micro-grain addendum of plan-gatekeeping: any task exceeding one seam / 3 files "
                 "(compile-atomicity exceptions must be stated), any non-executable acceptance criterion, any signature "
                 "left to the implementer's judgment, any task without a Context pack or a `Batch` value is a LEAK.")
    if MULTI_IS_GROUPS and not is_single(state):
        text += ("\nAlso audit the PLAN's `## PR groups` section: is each group a self-contained, independently "
                 "reviewable slice?")
    text += _gate_prior(state, "gate1_result")
    u["gate1_prompt"] = text
    ledger(state, u, f"gate1: spawned over plan rev {state.get('plan_revision')}")
    return u


def _leak_fingerprint(text):
    qs = [re.sub(r"\W+", " ", l.lower()).strip() for l in text.splitlines()
          if "?" in l or re.match(r"^\s*[-*\d]+[.)]?\s+", l)]
    qs = sorted(set(q for q in qs if len(q) > 12))
    return hashlib.sha1("\n".join(qs).encode()).hexdigest()[:10], qs


def gate1_route(state):
    u = {}
    note_script_error(state, u, "gate1")
    text = state.get("gate1_result") or ""
    if not text or text.startswith("Agent node failed") or "PLAN_GATE" not in text:
        return raise_escalation(state, u, escalation(
            "gate_fault", "The gatekeeper produced no verdict (agent fault). Retry the gate, or abort?",
            [option("Retry the gatekeeper", recommended=True), option("Abort the run")], evidence=tail(text, 1000),
            data={"gate": "gate1"}))
    m = re.search(r"PLAN_GATE:\s*(SEALED|LEAKY|FRICTION)", text)
    verdict = m.group(1) if m else "LEAKY"
    friction_only = verdict == "FRICTION" or (verdict == "SEALED" and "FRICTION" in text)
    u["gate1_verdict"] = verdict
    if verdict == "SEALED" or verdict == "FRICTION":
        if friction_only and verdict == "FRICTION":
            ledger(state, u, "gate1: FRICTION-only verdict sealed at discretion (tier 1); accepted findings noted in the plan")
            path = state.get("plan_path", "")
            if path and os.path.isfile(path):
                with open(path, "a", encoding="utf-8") as f:
                    f.write(f"\n\n<!-- gatekeeper friction accepted {today()}: see run ledger -->\n")
        u.update({"sealed": True, "leak_fingerprints": []})
        ledger(state, u, f"gate1: {verdict} -> sealed")
        snapshot(state, u, "sealed")
        u["_next"] = "gate2_build"
        return u
    fp, qs = _leak_fingerprint(text)
    prior = as_list(state.get("leak_fingerprints"))
    u["leak_fingerprints"] = prior + [fp]
    u["leak_questions"] = qs
    if fp in prior:
        ledger(state, u, "gate1: LEAKY on the same question set after the revise-and-re-seal retry (A1) -> owner")
        return raise_escalation(state, u, escalation(
            "gate1_stuck", "Gate 1 is still LEAKY on the SAME questions after one revise-and-re-seal retry (A1). "
                           "The gatekeeper verdict is attached. Answer the questions here, or stop?",
            [option("Stop and report", recommended=True), option("I will answer: (type the answers)")],
            evidence=tail(text, 3000), hard_trigger=True))
    ledger(state, u, f"gate1: LEAKY ({len(qs)} question lines, fp {fp}) -> answer_leaks")
    u["_next"] = "answer_leaks"
    return u


def leak_route(state):
    u = {}
    harvest_anomalies(state, u, "answer_leaks")
    note_script_error(state, u, "answer_leaks")
    answers = [a for a in as_list(state.get("leak_answers")) if isinstance(a, dict) and a.get("answer")]
    unanswerable = [q for q in as_list(state.get("leak_unanswerable")) if isinstance(q, str) and q.strip()]
    if not answers and not unanswerable:
        unanswerable = as_list(state.get("leak_questions")) or ["the gatekeeper's questions (see the verdict)"]
    if unanswerable:
        u["leak_pending_answers"] = answers
        return raise_escalation(state, u, escalation(
            "leak_question", "Gate 1 questions that cannot be answered from code or docs (business rules / priority calls). "
                             "Answer each; the plan is amended with your answers and re-sealed.\n"
                             + "\n".join(f"{i}. {q}" for i, q in enumerate(unanswerable, 1)), [],
            evidence=tail(state.get("gate1_result", ""), 1500), data={"questions": unanswerable}))
    u["pending_amendments"] = [f"Gate 1 leak: {a.get('question', '')} -> {a['answer']}" for a in answers]
    u["_next"] = "amend_plan"
    return u


def gate2_build(state):
    u = {}
    sizing = ("micro sizing per plan-authoring's Micro-grain addendum - one seam / <=3 files per task with "
              "compile-atomicity exceptions stated, executable acceptance criteria, batch grouping that respects dependency order"
              if is_micro(state) else "one-engineer-day sizing")
    text = (f"Load skills plan-review and plan-authoring. Review the plan at {state.get('plan_path')} - its task breakdown "
            f"and approach - for ground-truth accuracy against {state.get('code_repo_path')}, {sizing}, dependency ordering, "
            "and verifiability. Return PLAN_REVIEW: OKAY or REJECT with line-referenced complaints.")
    if MULTI_IS_GROUPS and not is_single(state):
        text += " Also review the PLAN's `## PR groups`: grouping coherence, ordering, and that no task's blocker lives in a later group."
    text += _gate_prior(state, "gate2_result")
    u["gate2_prompt"] = text
    ledger(state, u, "gate2: spawned")
    return u


def gate2_route(state):
    u = {}
    note_script_error(state, u, "gate2")
    text = state.get("gate2_result") or ""
    if not text or text.startswith("Agent node failed") or "PLAN_REVIEW" not in text:
        return raise_escalation(state, u, escalation(
            "gate_fault", "The oracle produced no PLAN_REVIEW verdict (agent fault). Retry, or abort?",
            [option("Retry the oracle review", recommended=True), option("Abort the run")], evidence=tail(text, 1000),
            data={"gate": "gate2"}))
    m = re.search(r"PLAN_REVIEW:\s*(OKAY|REJECT)", text)
    verdict = m.group(1) if m else "REJECT"
    u["gate2_verdict"] = verdict
    if verdict == "OKAY":
        ledger(state, u, "gate2: OKAY")
        snapshot(state, u, "reviewed")
        u["_next"] = "reseal_return"
        return u
    rejects = as_int(state.get("oracle_rejects")) + 1
    u["oracle_rejects"] = rejects
    if rejects > STRIKE_LIMIT:
        return raise_escalation(state, u, escalation(
            "gate2_stuck", f"The oracle still REJECTS the plan after {STRIKE_LIMIT} fix rounds. Stop, or override and promote?",
            [option("Stop and report", recommended=True), option("Override: promote the plan as reviewed (recorded)")],
            evidence=tail(text, 3000), hard_trigger=True))
    u["pending_amendments"] = [f"Oracle complaint (round {rejects}): {tail(text, 4000)}"]
    ledger(state, u, f"gate2: REJECT round {rejects} -> amend_plan (re-seal follows, A3)")
    u["_next"] = "amend_plan"
    return u


def autonomy_gate(state):
    u = {}
    if mode(state)["autonomy"] == "plan-gate":
        u["ask_question"] = (f"plan-gate: the breakdown at {state.get('plan_path')} is SEALED and OKAY'd. Approve it to create "
                             "tasks and run them autonomously?")
        u["escalation"] = escalation("approve_breakdown", u["ask_question"], [option("Approve the breakdown", recommended=True)])
        u["_next"] = "approve_breakdown"
    else:
        u["_next"] = "promote_tasks"
    return u


# ====================================================================== 2.4 promote
def promote_verify(state):
    u = {}
    harvest_anomaly_lines(state, u, state.get("promote_report") or "", "promote_tasks")
    note_script_error(state, u, "promote_tasks")
    tasks = load_tasks(state)
    text = read_file(state.get("plan_path", ""))
    fm, body = frontmatter(text)
    rows = [l for l in plan_section(body, r"Task breakdown").splitlines() if l.strip().startswith("|")]
    rows = [r for r in rows if len(r.split("|")) > 2 and not re.match(r"^\|\s*:?-", r) and not re.search(r"(?i)\|\s*(#|task|title)\s*\|", r)]
    defects = []
    if TASKSTORE == "dirstate":
        promoted = os.path.join(state.get("plans_root", ""), "plans", f"PLAN-{state.get('plan_slug')}.md")
        if not os.path.isfile(promoted):
            cands = [p for p in (as_list(state.get("promoted_plan_path")) or []) if isinstance(p, str) and os.path.isfile(p)]
            promoted = cands[0] if cands else promoted
        if not os.path.isfile(promoted):
            defects.append(f"promoted PLAN not found at {promoted}")
        else:
            u["plan_path"] = promoted
            tasks = load_tasks({**state, "plan_path": promoted})
    if rows and len(tasks) < len(rows):
        defects.append(f"{len(rows)} breakdown rows but {len(tasks)} task dirs belong to the plan")
    if is_micro(state):
        for t in tasks:
            if not t.get("batch"):
                defects.append(f"{t['id']}: micro task without `batch:` frontmatter")
    if TASKSTORE == "dirstate" and os.path.isfile(os.path.join(state.get("plans_root", ""), "bin", "validate")):
        code, out = sh([os.path.join(state["plans_root"], "bin", "validate"), "--quiet"], cwd=state["plans_root"], timeout=300)
        if code != 0:
            defects.append(f"{STORE_LABEL} ./bin/validate is RED:\n" + tail(out, 2000))
    if defects:
        if as_int(state.get("promote_retries")) < RERUN_ONCE:
            u["promote_retries"] = as_int(state.get("promote_retries")) + 1
            u["promote_defects"] = "\n".join(f"- {d}" for d in defects)
            ledger(state, u, f"promote: defects -> one retry: {defects[:3]}")
            u["_next"] = "promote_tasks"
            return u
        return raise_escalation(state, u, escalation(
            "promote_failed", "Promotion left mechanical defects after a retry. Abort, or continue with the tasks as they are?",
            [option("Abort the run", recommended=True), option("Continue with the tasks as promoted")],
            evidence="\n".join(defects), hard_trigger=True))
    ok, out = commit_planning(state if "plan_path" not in u else {**state, **u},
                              f"plan({state.get('plan_slug')}): promote intake packet - {len(tasks)} task(s)")
    if not ok:
        ledger(state, u, f"promote: planning commit failed: {tail(out, 300)}")
        if TASKSTORE == "dirstate":
            return raise_escalation(state, u, escalation(
                "promote_failed", f"The {STORE_LABEL} commit/push after promotion failed (validate RED or push rejected). Fix and retry, or abort?",
                [option("Retry the planning commit", recommended=True), option("Abort the run")], evidence=tail(out, 2000)))
    for t in tasks:
        t.setdefault("unit", _pr_unit(state, t, tasks))
    save_tasks(state, u, tasks)
    ledger(state, u, f"promote: {len(tasks)} task(s) recorded in tasks.json; units={sorted(set(t['unit'] for t in tasks))}")
    snapshot(state, u, "promoted")
    u["_next"] = "branch_setup"
    return u


def _pr_unit(state, task, tasks):
    """PR unit: single -> run; per-task-stack multi -> the task; PR-group multi -> its PLAN group."""
    if is_single(state):
        return "run"
    if MULTI_IS_GROUPS:
        return task.get("group") or "g1"
    return task["id"]


# ====================================================================== 2.5 per-task loop
def _run_branch(state):
    return f"feat/PLAN-{state.get('plan_slug')}"


def branch_setup(state):
    u = {}
    code = state.get("code_repo_path", "")
    base = state.get("base_branch_name") or "main"
    if is_single(state) or MULTI_IS_GROUPS:
        if is_single(state):
            branch = _run_branch(state)
        else:
            gi = as_int(state.get("group_index")) or 1
            u["group_index"] = gi
            branch = f"{_run_branch(state)}-g{gi}"
            prev = f"{_run_branch(state)}-g{gi - 1}" if gi > 1 else ""
            if prev:
                merged = _pr_merged(state, prev)
                base = base if merged else prev
        u["run_branch"] = branch
        u["unit_base_branch"] = base
        exists = git_out(code, "rev-parse", "--verify", "--quiet", f"refs/heads/{branch}", default="") or \
            git_out(code, "rev-parse", "--verify", "--quiet", f"refs/remotes/origin/{branch}", default="")
        if exists:
            steps = [("checkout", branch), ("pull", "--quiet", "--ff-only")]
        else:
            steps = [("checkout", "--quiet", base), ("pull", "--quiet", "--ff-only"), ("checkout", "-b", branch), ("push", "-u", "origin", branch)]
        for s in steps:
            code_, out, err = git(code, *s)
            if code_ != 0 and not (s[0] == "pull" and "no tracking" in (out + err).lower()):
                if s[0] == "push":
                    ledger(state, u, f"branch: push -u failed ({tail(err, 200)}); continuing on the local branch")
                    continue
                ledger(state, u, f"branch: git {' '.join(s)} failed: {tail(err or out, 300)}")
                return raise_escalation(state, u, escalation(
                    "git_setup_failed", f"Could not set up the branch {branch} from {base} in {code}: git {' '.join(s)} failed. Retry, or abort?",
                    [option("Retry the branch setup", recommended=True), option("Abort the run")], evidence=tail(err or out, 1500)))
        u["unit_base_sha"] = git_out(code, "rev-parse", "HEAD", default="")
        ledger(state, u, f"branch: {branch} ready ({'existing' if exists else 'created'} from {base}) @ {u['unit_base_sha']}")
    else:
        u["run_branch"] = ""
        ledger(state, u, "branch: multi (per-task stacked PRs) - Sisyphus cuts each task branch at claim")
    snapshot(state, u, "tasks")
    u["_next"] = "select_tasks"
    return u


def _pr_merged(state, branch):
    code = state.get("code_repo_path", "")
    rc, out = sh(["gh", "pr", "view", branch, "--json", "state", "--jq", ".state"], cwd=code, timeout=60)
    return rc == 0 and out.strip().upper() == "MERGED"


def _blockers_done(task, tasks):
    return all((task_by_id(tasks, b) or {}).get("status") == "complete" for b in task.get("blocked_by", []))


def _hotspot(files):
    return any(HOTSPOT_RE.search(f) for f in files)


def _disjoint(a, b):
    fa, fb = set(a.get("files") or []), set(b.get("files") or [])
    if not fa or not fb:
        return False
    return not any(x == y or x.startswith(y.rstrip("/") + "/") or y.startswith(x.rstrip("/") + "/") for x in fa for y in fb)


def select_tasks(state):
    u = {}
    note_script_error(state, u, "select_tasks")
    tasks = as_list(state.get("tasks"))
    fixes = list(as_list(state.get("pending_fixes")))
    if fixes:
        f = fixes.pop(0)
        u.update({"pending_fixes": fixes, "current_task": f.get("task"), "delegation_kind": "fix",
                  "fix_items": as_list(f.get("items")), "fix_source": f.get("source", "review"),
                  "fix_return_to": f.get("return_to", "select_tasks"), "fix_note": f.get("note", "")})
        ledger(state, u, f"schedule: fix round for {f.get('task')} ({f.get('source')})")
        u["_next"] = "compose_delegation"
        return u
    pending = [t for t in tasks if t.get("status") in ("pending", "")]
    in_flight = [t for t in tasks if t.get("status") == "in-progress"]
    if MULTI_IS_GROUPS and not is_single(state):
        gi = as_int(state.get("group_index")) or 1
        groups = []
        for t in tasks:
            if t.get("unit") not in groups:
                groups.append(t.get("unit"))
        cur = groups[gi - 1] if gi - 1 < len(groups) else None
        group_pending = [t for t in pending if t.get("unit") == cur]
        if cur and not group_pending and not [t for t in in_flight if t.get("unit") == cur]:
            if state.get("closeout_done_for") == cur:
                if gi < len(groups):
                    u["group_index"] = gi + 1
                    u["closeout_scope"] = "group"
                    ledger(state, u, f"schedule: group {cur} closed; starting group {gi + 1}")
                    u["_next"] = "branch_setup"
                    return u
            else:
                u["closeout_scope"] = "group" if gi < len(groups) else "run"
                u["closeout_unit"] = cur
                u["_next"] = "group_closeout"
                return u
        pending = group_pending
    if not pending and not in_flight:
        u["closeout_scope"] = "run"
        u["_next"] = "phase_f_gate"
        return u
    eligible = [t for t in pending if _blockers_done(t, tasks)]
    if is_micro(state):
        batches = []
        for t in tasks:
            if t.get("batch") and t["batch"] not in batches:
                batches.append(t["batch"])
        open_batches = [b for b in batches if any(t.get("batch") == b and t.get("status") != "complete" for t in tasks)]
        if open_batches:
            eligible = [t for t in eligible if t.get("batch") == open_batches[0]] or eligible
    if not eligible:
        blocked_on = sorted({b for t in pending for b in t.get("blocked_by", [])
                             if (task_by_id(tasks, b) or {}).get("status") in ("failed", "parked")})
        return raise_escalation(state, u, escalation(
            "tasks_stuck", f"{len(pending)} task(s) remain but none is runnable (blocked on {blocked_on or 'in-flight/failed tasks'}). "
                           "Retry the failed blocker, skip the blocked tasks (reported as follow-ups), or stop?",
            [option("Retry the failed blocker"), option("Skip the blocked tasks and finish (follow-ups)"),
             option("Stop and report", recommended=True)], hard_trigger=True))
    if is_parallel(state):
        wave = []
        for t in eligible:
            if t.get("protected") or _hotspot(t.get("files") or []) or not t.get("files"):
                continue
            if all(_disjoint(t, w) for w in wave) and (not wave or not is_micro(state) or t.get("batch") == wave[0].get("batch")):
                wave.append(t)
            if len(wave) >= WAVE_CAP:
                break
        if len(wave) >= 2:
            u["wave_ids"] = [t["id"] for t in wave]
            ledger(state, u, f"schedule: parallel wave {u['wave_ids']} (cap {WAVE_CAP}; file-disjoint; no hotspots; no protected)")
            u["_next"] = "wave_claim"
            return u
    u["current_task"] = eligible[0]["id"]
    u["delegation_kind"] = "implement"
    u["fix_items"] = []
    ledger(state, u, f"schedule: next task {eligible[0]['id']} ({eligible[0].get('title', '')})")
    u["_next"] = "claim_task"
    return u


def _resolve_pr_base(state, task, tasks):
    """Per-task-stack multi: an open blocker PR -> its branch; else the base branch. Single/groups: the unit branch."""
    if is_single(state) or MULTI_IS_GROUPS:
        return state.get("run_branch") or _run_branch(state)
    for b in task.get("blocked_by", []):
        bt = task_by_id(tasks, b)
        if bt and bt.get("branch") and bt.get("pr_number") and not _pr_merged(state, bt["branch"]):
            return bt["branch"]
    return state.get("base_branch_name") or "main"


def _claim(state, u, task, tasks):
    """Shared claim mechanics for sequential and wave members (updates the task dict in place)."""
    code = state.get("code_repo_path", "")
    tdir = task_dir_for(state, task)
    if TASKSTORE == "dirstate":
        active = os.path.join(state.get("tasks_root", ""), "active", state.get("owner_name", ""))
        if tdir and not tdir.startswith(active):
            os.makedirs(active, exist_ok=True)
            dst = os.path.join(active, os.path.basename(tdir))
            rc, out, err = git(state.get("plans_root", ""), "mv", tdir, dst)
            if rc == 0:
                tdir = dst
        ok, out = core_tool(STORE_TOOL_PREFIX + "claim", task_dir=tdir, note=f"architect-next run {state.get('run_id')}: implementing {task['id']}")
        if not ok:
            ledger(state, u, f"claim: store claim tool unavailable/failed for {task['id']}: {tail(out, 200)}")
    else:
        set_task_status(state, task, "in-progress")
    task["dir"] = tdir or task.get("dir", "")
    task["status"] = "in-progress"
    task["pr_base"] = _resolve_pr_base(state, task, tasks)
    task["branch"] = (state.get("run_branch") if (is_single(state) or MULTI_IS_GROUPS)
                      else f"feat/{task['slug'].lower()}")
    if is_single(state) or MULTI_IS_GROUPS:
        task["base_sha"] = git_out(code, "rev-parse", "HEAD", default="")
    else:
        git(code, "fetch", "--quiet", "origin")
        task["base_sha"] = git_out(code, "rev-parse", f"origin/{task['pr_base']}", default="") or git_out(code, "rev-parse", "HEAD", default="")
    log_event(state, task, f"started: architect-next run {state.get('run_id')}; BASE SHA {task['base_sha']}; PR base {task['pr_base']}")
    return task


def claim_task(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task"))
    if not task:
        raise RuntimeError(f"claim_task: unknown task {state.get('current_task')!r}")
    _claim(state, u, task, tasks)
    rounds = dict(as_dict(state.get("task_rounds")))
    rounds.setdefault(task["id"], {})
    u["task_rounds"] = rounds
    save_tasks(state, u, tasks)
    ledger(state, u, f"claim: {task['id']} base={task['base_sha']} pr_base={task['pr_base']}")
    snapshot(state, u, f"task {task['id']}")
    u["_next"] = "compose_delegation"
    return u


def _quality_bar_text(plan_text):
    sec = plan_section(plan_text, r"Quality bar")
    return sec or "(the plan carries no `## Quality bar` section)"


def _prior_report(state, task_id):
    run_dir = state.get("run_dir", "")
    n = 0
    while os.path.isfile(os.path.join(run_dir, "reports", f"{task_id}-r{n + 1}.md")):
        n += 1
    return (read_artifact(run_dir, os.path.join("reports", f"{task_id}-r{n}.md")) if n else ""), n


def _delegation_text(state, task, kind, fix_items=(), fix_note="", project_dir=None, roster=""):
    """The MANDATORY delegation template, field by field (markers 22-28, 369-447)."""
    mv = mode(state)
    single = is_single(state) or MULTI_IS_GROUPS
    code = project_dir or state.get("code_repo_path", "")
    plan_text = read_file(state.get("plan_path", ""))
    tdir = task_dir_for(state, task)
    index_text = read_file(os.path.join(tdir, "index.md")) if tdir else ""
    _, index_body = frontmatter(index_text)
    slice_word = "one micro-task (one seam)" if is_micro(state) else "one one-engineer-day slice"
    run_branch = state.get("run_branch") or _run_branch(state)
    task_branch = task.get("branch") or ""
    repo_hint = REPO_HINT
    L = [f"## TASK",
         f"Implement {task['id']} ({task.get('title', '')}) in {repo_hint} {code}. This is {slice_word} of "
         f"PLAN-{state.get('plan_slug')}. ALL code you WRITE goes in {code}. You MAY freely READ"
         + SIBLING_HINT.format(reference_root=state.get("reference_root_path"))
         + " to understand internal libraries, legacy patterns, call sites, shared contracts, and conventions - just do not write to them.",
         "",
         "## SOURCE OF TRUTH",
         f"- Task file: {os.path.join(tdir, 'index.md') if tdir else '(task dir missing)'} (read its What / Steps / Acceptance criteria - implement EXACTLY these, nothing more)",
         f"- Plan: {state.get('plan_path')}",
         "- Conventions: " + CONVENTIONS_LINE.format(plans_root=state.get("plans_root"), code=code),
         "",
         "## EXPECTED OUTCOME"]
    if single:
        L += [f"Every acceptance criterion met; build + full test suite green in {code}; the work committed and pushed to the "
              f"EXISTING branch {task_branch or run_branch} (already checked out - NOT commit-to-main, and NOT a new branch). Do NOT open a PR - "
              "one draft PR for the whole plan is opened at the end of the run by the orchestrator."]
    else:
        L += [f"Every acceptance criterion met; build + full test suite green in {code}; a DRAFT PR opened (feature branch "
              f"{task_branch}, NOT commit-to-main) with base `{task.get('pr_base') or state.get('base_branch_name')}`, flipped to ready once its CI checks are green and the work is reviewable."]
    L += ["", "## MUST DO", f"- {IDIOM_LINE}", "- Derive tests from the task's Acceptance criteria."]
    if single:
        L += [f"- Work on the CURRENT branch ({task_branch or run_branch}) - git pull before starting. Commit with messages referencing the "
              f"task ID (e.g. \"feat({task['id']}): ...\"), push to that branch, and report the commit SHA(s)."
              + LOG_EVENT_LINE.format(plans_root=state.get("plans_root"))]
    else:
        L += [f"- Cut your branch `{task_branch}` from `origin/{task.get('pr_base')}` and open the PR as a DRAFT (`gh pr create --draft`) with base "
              f"`{task.get('pr_base')}`; report its URL AND number. Flip it to ready (`gh pr ready`) ONLY once every acceptance criterion is "
              "verified AND the PR's CI checks are green (`gh pr checks --watch` after the final push); if you can't get there, leave it "
              "in draft and report exactly what's blocking." + LOG_EVENT_LINE.format(plans_root=state.get("plans_root"))]
        if task.get("pr_base") and task["pr_base"] != (state.get("base_branch_name") or "main"):
            L += [f"- STACKED PR: your branch is cut from `{task['pr_base']}`, which is NOT merged yet. Open your PR with base "
                  f"`{task['pr_base']}` - this stacks it on the blocker's PR. NEVER retarget it to {state.get('base_branch_name') or 'main'}, and never "
                  "rebase or merge the base branch into yours - its changes are already in your history. If a push is rejected "
                  "non-fast-forward, a lower PR merged and GitHub rebased your remote branch: git fetch origin, rebase your unpushed "
                  "commits onto origin/<task branch>, and never force-push over it."]
    L += ["- End your final summary with a \"FOLLOW-UPS:\" section (manual / out-of-scope actions: WHAT, WHERE, WHY, WHEN; or \"FOLLOW-UPS: none\"), "
          "and the report sections `Debt left behind:`, `Debris exemptions:`, `Design decisions:` as the sisyphus contract defines them. "
          "Report the HEAD SHA of your last commit.",
          "", "## MUST NOT DO",
          "- Do NOT implement other tasks' scope. Do NOT touch the plan or task files except via the sanctioned log entry.",
          f"- Do NOT WRITE code in any repo other than {code} (reading them is fine).",
          "- Do NOT suppress errors or delete failing tests."]
    if single:
        L += ["- Do NOT open a PR. Do NOT create or switch branches. Do NOT merge or rebase onto main."]
    if NO_COMMIT_MAIN_LINE:
        L += [NO_COMMIT_MAIN_LINE]
    L += ["- Do NOT diverge from the task's stated scope; if the plan is wrong, STOP and report back.",
          "", "## CONTEXT",
          f"Quality bar: rigor={state.get('quality_rigor') or 'production'}, surfaces={task.get('surfaces') or state.get('quality_surfaces') or '(plan)'}",
          _quality_bar_text(plan_text), "", "### Task file (verbatim)", index_body.strip() or "(empty)", "",
          "### Plan section (verbatim)", task_section(plan_text, task["id"], task.get("title", "")) or "(no section names this task)"]
    if is_micro(state):
        cp = context_pack(plan_text, task["id"], task.get("title", "")) or context_pack(index_text, task["id"])
        L += ["", "### Context pack (verbatim)", cp or "(the plan carries no Context pack for this task - a plan defect; report it)"]
    mode_lines = []
    if is_micro(state):
        mode_lines.append("review_cadence=batch: run coder verification only (build + tests green) and NO review gauntlet of your own; "
                          "the orchestrator runs the full panel once per batch.")
    if roster:
        mode_lines.append("parallel=1: you are one task agent in a parallel batch (isolated worktree, own task branch). Stay on your "
                          "branch; never edit files outside your declared scope - message the parent and the owning teammate FIRST "
                          "and wait for a re-scope; leave the worktree removable (everything committed and pushed, no scratch files).")
        mode_lines.append("Teammate roster:\n" + roster)
    if mode_lines:
        L += ["", "## MODE", *[f"- {m}" for m in mode_lines]]
    if kind == "fix":
        prior, n = _prior_report(state, task["id"])
        L += ["", f"## FIX ROUND {n}",
              f"This is a fix round on {task['id']} (artifact re-entry: the task's branch, commits and prior report are your context).",
              fix_note or "", "### Findings to address (verbatim)"] + [f"- {f}" for f in fix_items] + \
             ["", "### Your prior report (verbatim)", tail(prior, 8000) or "(none recorded)"]
        if m := SISYPHUS_RUN_RE.search(prior):
            L += ["", f"Sisyphus run id: {m.group(1)}"]
    if kind == "hygiene":
        L += ["", "## HYGIENE BRIEF", state.get("hygiene_brief") or "(missing)"]
    return "\n".join(x for x in L if x is not None)


def compose_delegation(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task"))
    if not task:
        raise RuntimeError(f"compose_delegation: unknown task {state.get('current_task')!r}")
    kind = state.get("delegation_kind") or "implement"
    text = _delegation_text(state, task, kind, as_list(state.get("fix_items")), state.get("fix_note") or "")
    u["delegation_prompt"] = text
    u["delegate_review_cadence"] = "batch" if is_micro(state) else "task"
    u["delegate_parallel"] = "0"
    u["delegate_project_dir"] = state.get("code_repo_path", "")
    if is_micro(state):
        ledger(state, u, f"mode-degraded: {task['id']}: review_cadence=batch carried in the prompt (config-agent child; E9)")
    ledger(state, u, f"delegate: {task['id']} kind={kind}" + (f" items={len(as_list(state.get('fix_items')))}" if kind == "fix" else ""))
    snapshot(state, u, f"task {task['id']} delegated")
    return u


def _report_failed(text):
    if not text or text.startswith("Agent node failed"):
        return True
    if re.search(r"SISYPHUS_(FAILED|BLOCKED|ABORTED)", text):
        return True
    head = "\n".join(text.splitlines()[:30] + text.splitlines()[-30:])
    return bool(re.search(r"(?im)^\W*(STOP(PED)?|BLOCKED|FAILED)\b.*\b(plan|scope|cannot|could not|unable)", head)
                or re.search(r"(?i)\b(cannot|could not|unable to)\s+(complete|implement|proceed|finish)\b", head))


def _parse_report(text):
    """The sisyphus report sections the architect ledgers."""
    def sec(name):
        m = re.search(rf"(?ims)^\W*{name}\s*:?\s*\n(.*?)(?=^\W*(FOLLOW-UPS|Debt left behind|Debris exemptions|Design decisions|Decisions taken autonomously|Sisyphus run id)\b|\Z)", text)
        if not m:
            m2 = re.search(rf"(?im)^\W*{name}\s*:\s*(.+)$", text)
            return [m2.group(1).strip()] if m2 and m2.group(1).strip().lower() not in ("none", "- none") else []
        return [re.sub(r"^\s*[-*]\s*", "", l).strip() for l in m.group(1).splitlines()
                if l.strip() and l.strip().lower() not in ("none", "- none", "- none.")]
    r = {
        "follow_ups": sec("FOLLOW-UPS"), "debt": sec("Debt left behind"), "exemptions": sec("Debris exemptions"),
        "design": sec("Design decisions"), "autonomous": sec("Decisions taken autonomously"),
        "shas": [], "pr_url": "", "pr_number": "", "head_sha": "", "sisyphus_run_id": "",
    }
    r["design_asks"] = [d for d in r["design"] if d.upper().startswith("ASK --") or d.upper().startswith("ASK -")]
    r["design"] = [d for d in r["design"] if d not in r["design_asks"]]
    if m := PR_URL_RE.search(text):
        r["pr_url"], r["pr_number"] = m.group(0), m.group(2)
    if m := re.search(r"(?i)HEAD(?: SHA)?[:\s]+([0-9a-f]{7,40})", text):
        r["head_sha"] = m.group(1)
    r["shas"] = [s for s in dict.fromkeys(SHA_RE.findall(text)) if s not in ("deadbeef", "cafebabe")][:20]
    if m := SISYPHUS_RUN_RE.search(text):
        r["sisyphus_run_id"] = m.group(1)
    return r


def route_delegate(state):
    u = {}
    note_script_error(state, u, "delegate")
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task"))
    text = state.get("delegate_result") or ""
    _, n = _prior_report(state, task["id"])
    write_artifact(state.get("run_dir", ""), os.path.join("reports", f"{task['id']}-r{n + 1}.md"), text or "(empty)")
    parsed = _parse_report(text)
    u["report_parsed"] = parsed
    if parsed["pr_number"]:
        task["pr_number"], task["pr_url"] = parsed["pr_number"], parsed["pr_url"]
        prs = dict(as_dict(state.get("pr_numbers")))
        prs[task["id"]] = parsed["pr_number"]
        u["pr_numbers"] = prs
    if parsed["head_sha"]:
        task["head_sha"] = parsed["head_sha"]
    if parsed["sisyphus_run_id"]:
        task["sisyphus_run_id"] = parsed["sisyphus_run_id"]
    save_tasks(state, u, tasks)
    if _report_failed(text):
        u["failure_evidence"] = tail(text, 4000)
        ledger(state, u, f"delegate: {task['id']} reported failure / STOP -> oracle diagnosis")
        u["_next"] = "oracle_diagnose"
        return u
    ledger(state, u, f"delegate: {task['id']} returned (report r{n + 1}; shas={len(parsed['shas'])}; pr={parsed['pr_number'] or '-'})")
    if (state.get("delegation_kind") == "fix" and state.get("fix_source") in ("probe_adopt",)):
        u["_next"] = "post_round"
        return u
    u["verify_scope"] = "hygiene" if state.get("delegation_kind") == "hygiene" else "task"
    u["_next"] = "verify_gate"
    return u


def task_failed_route(state):
    u = {}
    note_script_error(state, u, "oracle_diagnose")
    task_id = state.get("current_task")
    rounds = dict(as_dict(state.get("task_rounds")))
    r = rounds.setdefault(task_id, {})
    used = as_int(r.get("retry_rounds"))
    oracle = state.get("oracle_result") or ""
    opts = []
    if used < RERUN_ONCE:
        opts.append(option("Retry with the oracle's direction (one round)", recommended=not oracle.startswith("Agent node failed"),
                           evidence=tail(oracle, 400)))
    opts += [option("Re-scope: amend the plan/task (type the amendment)"), option("Abort this task and stop",
                                                                                   recommended=used >= RERUN_ONCE)]
    return raise_escalation(state, u, escalation(
        "task_failed", f"{task_id} failed after Sisyphus's own recovery. Oracle diagnosis attached. How to proceed?",
        opts, evidence=tail(state.get("failure_evidence", ""), 2000) + "\n\nOracle:\n" + tail(oracle, 2000), hard_trigger=True))


def _verification_commands(state):
    v = state.get("verification_commands")
    if isinstance(v, list):
        return json.dumps(v)
    return v if isinstance(v, str) and v.strip().startswith("[") else "[]"


def verify_gate(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    scope = state.get("verify_scope") or "task"
    code = state.get("code_repo_path", "")
    task = task_by_id(tasks, state.get("current_task")) or {}
    head = git_out(code, "rev-parse", "HEAD", default=task.get("head_sha", ""))
    unit = state.get("current_batch") if scope == "batch" else task.get("id", "run")
    rounds_log = dict(as_dict(state.get("round_log")))
    rl = rounds_log.setdefault(unit, [])
    n = len(rl) + 1
    plan_text = read_file(state.get("plan_path", ""))
    tdir = task_dir_for(state, task) if task else ""
    idx = read_file(os.path.join(tdir, "index.md")) if tdir else ""
    _, body = frontmatter(idx)
    crit = plan_section(body, r"Acceptance criteria") or body
    recipe = plan_section(plan_text, r"Local[- ]run recipe") or state.get("local_run_recipe") or ""
    consumer = truthy(task.get("consumer_surface")) or bool(re.search(r"(?i)endpoint|rpc|cli|api|route|request|response|http|grpc|graphql", task.get("surfaces", "") + " " + crit))
    if is_single(state) or MULTI_IS_GROUPS or scope in ("batch", "hygiene"):
        base = task.get("base_sha", "") if scope != "batch" else state.get("batch_base_sha", "")
        diff = f"{base}..HEAD" if base else "worktree"
    else:
        diff = f"get_diff --base {task.get('pr_base') or state.get('base_branch_name')} in {code}"
    lines = ["Post-implementation review gate.", f"Project: {state.get('delegate_project_dir') or code}", f"Diff: {diff}"]
    forced = ["adversary"]
    if scope == "batch":
        forced = ["code-review", "adversary", "security"]
        members = [t for t in tasks if t.get("batch") == state.get("current_batch")]
        consumer = any(truthy(t.get("consumer_surface")) for t in members) or consumer
        crit = "\n\n".join(f"### {t['id']} {t.get('title', '')}\n" + (plan_section(frontmatter(read_file(os.path.join(task_dir_for(state, t), 'index.md')))[1], r"Acceptance criteria") or "(see task file)") for t in members)
    if consumer and recipe:
        forced.append("probe")
    if scope == "hygiene":
        lines += ["Forced lanes: adversary", "Plan / acceptance criteria:", state.get("hygiene_criteria", "")]
    else:
        lines += [f"Forced lanes: {', '.join(forced)}", f"Rigor: {state.get('quality_rigor') or 'production'}",
                  "Plan / acceptance criteria:", crit or "(none recorded)"]
    if rl:
        prior = as_list(state.get("rejected_findings"))
        if prior:
            lines += ["Prior review decisions:"] + [f"- {p}" for p in prior]
        last = rl[-1]
        if last.get("head") and last["head"] != head and not _code_tree_dirty(state, code):
            lines += [f"Re-review delta: {last['head']}..HEAD"]
            green = [l for l, v in as_dict(last.get("lanes")).items()
                     if l != "debris" and str(v).strip() == "GREEN" and l not in as_list(last.get("lanes_with_findings"))]
            if green:
                lines += [f"Settled lanes: {', '.join(green)}"]
    if recipe or consumer:
        note = recipe or "discover them"
        if PROBE_HOUSE_NOTE and consumer:
            note += "\n" + PROBE_HOUSE_NOTE
        lines += ["Local-run recipe / usage suites:", note]
    u["gauntlet_prompt"] = "\n".join(lines)
    u["adversary_prompt"] = (f"Adversarially review the changes for {task.get('id', unit)} against its plan. Return CONFORMS/DIVERGES.\n\n"
                             f"DIFF: {'run ' + diff if diff.startswith('get_diff') else 'run get_diff --base ' + diff.split('..')[0] + ' in ' + code}\n\n"
                             f"PLAN - acceptance criteria to check against:\n{crit}\n\n{_quality_bar_text(plan_text)}")
    cmds = _verification_commands(state)
    if scope == "hygiene" and state.get("hygiene_lint_cmd"):
        arr = json.loads(cmds)
        arr.append(state["hygiene_lint_cmd"])
        cmds = json.dumps(arr)
    u["verification_commands_json"] = cmds
    u["debris_exclude"] = state.get("debris_exclude") or ""
    rl.append({"round": n, "head": head, "scope": scope, "started": now_iso()})
    u["round_log"] = rounds_log
    u["verify_unit"] = unit
    use_adversary = scope == "task" and is_micro(state)
    u["verify_lane"] = "adversary" if use_adversary else "gauntlet"
    ledger(state, u, f"verify: {unit} scope={scope} round {n} @ {head or '(no git)'} via {u['verify_lane']} lanes={forced}")
    snapshot(state, u, f"task {unit} verify r{n}")
    u["_next"] = "run_adversary" if use_adversary else "run_gauntlet"
    return u


# A planning store inside the code repo carries uncommitted task status edits
# until the task closes; only changes outside it leave the delta incomplete.
def _code_tree_dirty(state, code):
    pathspec = []
    if state.get("plans_root") and code:
        rel = os.path.relpath(os.path.realpath(state["plans_root"]), os.path.realpath(code))
        if rel != "." and not rel.startswith(".."):
            pathspec = ["--", ".", f":(exclude){rel}"]
    return bool(git_out(code, "status", "--porcelain", *pathspec))


def _gauntlet_header(text):
    if m := re.search(r"^## Full lane reports[ \t]*$", text, re.M):
        return text[:m.start()]
    if (i := text.find("<details>")) >= 0:
        return text[:i]
    return text


_GAUNTLET_VERDICT_RE = re.compile(r"^GAUNTLET:[ \t]*(PASS|BLOCKED)[ \t]*$", re.M)


def _has_verdict(header):
    return bool(_GAUNTLET_VERDICT_RE.search(header))


def _lane_rows(header):
    lines = header.splitlines()
    first = next((i for i, l in enumerate(lines) if l.startswith("|")), len(lines))
    rows = []
    for line in lines[first:]:
        if not line.startswith("|"):
            break
        rows.append(line)
    return rows


def _lane_cells(header):
    cells_by_lane = {}
    for line in _lane_rows(header):
        cells = [c.strip() for c in line.strip("|").split("|")]
        lane = cells[0].lower()
        if len(cells) >= 2 and lane in GAUNTLET_LANES and lane not in cells_by_lane:
            cells_by_lane[lane] = (cells[1], cells[2] if len(cells) >= 3 else "")
    return cells_by_lane


# The gauntlet's probe row carries the probe verdict in its detail cell
# ("INCONCLUSIVE (environment)" included); a report without that row is the
# probe's own output, whose last USAGE_PROBE line is its verdict.
def _probe_verdict(text):
    row = _lane_cells(_gauntlet_header(text)).get("probe")
    if row:
        m = re.match(r"(PASS|FAIL|INCONCLUSIVE)\b", row[1])
        return m.group(1) if m else ""
    verdicts = re.findall(r"^USAGE_PROBE:[ \t]*(PASS|FAIL|INCONCLUSIVE)[ \t]*$", text, re.M)
    return verdicts[-1] if verdicts else ""


def _parse_gauntlet(text):
    header = _gauntlet_header(text)
    verdict = "BLOCKED"
    if m := _GAUNTLET_VERDICT_RE.search(header):
        verdict = m.group(1)
    elif m := re.search(r"ADVERSARIAL_REVIEW:\s*(CONFORMS|DIVERGES)", text):
        verdict = "PASS" if m.group(1) == "CONFORMS" else "BLOCKED"
    incomplete = []
    if m := re.search(r"^GAUNTLET_REVIEW_INCOMPLETE:[ \t]*([^\n]+?)[ \t]*$", header, re.M):
        incomplete = [x.strip() for x in m.group(1).split(",") if x.strip()]
    if "PIPELINE-FAULT" in text and not _has_verdict(header) and "ADVERSARIAL_REVIEW:" not in text:
        incomplete = ["pipeline"]
    lanes = {lane: status for lane, (status, _) in _lane_cells(header).items()}
    blockers = []
    sec = re.search(r"(?ims)^#+\s*(Blockers?|Divergences?|Complaints?)\s*\n(.*?)(?=^#+\s|\Z)", header)
    if sec:
        blockers = [l.strip()[1:].strip() for l in sec.group(2).splitlines() if l.strip().startswith(("-", "*"))]
    if verdict == "BLOCKED" and not blockers and "ADVERSARIAL_REVIEW: DIVERGES" in text:
        blockers = [l.strip()[1:].strip() for l in text.splitlines() if l.strip().startswith(("-", "*"))][:20] or ["adversary: DIVERGES (see report)"]
    blockers += [l.strip() for l in text.splitlines() if l.strip().startswith("debris: Implementation debris")]
    plan_root = bool(re.search(r"(?i)plan (itself )?is the root cause|impossible or contradictory criterion|criterion (is|cannot be) (impossible|unverifiable)", text))
    probe_files = re.findall(TEST_FILE_RE, text) if _probe_verdict(text) == "PASS" else []
    return verdict, incomplete, lanes, blockers, plan_root, probe_files


def route_verdict(state):
    u = {}
    note_script_error(state, u, state.get("verify_lane") or "run_gauntlet")
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task")) or {}
    scope = state.get("verify_scope") or "task"
    unit = state.get("verify_unit") or task.get("id") or "run"
    adversary_lane = state.get("verify_lane") == "adversary"
    text = (state.get("adversary_result") if adversary_lane else state.get("gauntlet_result")) or ""
    verdict, incomplete, lanes, blockers, plan_root, probe_files = _parse_gauntlet(text)
    has_verdict = "ADVERSARIAL_REVIEW:" in text if adversary_lane else _has_verdict(_gauntlet_header(text))
    if not text or text.startswith("Agent node failed") or not has_verdict:
        verdict, incomplete, blockers = "BLOCKED", incomplete or ["pipeline"], []
    probe = _probe_verdict(text)
    rounds_log = dict(as_dict(state.get("round_log")))
    rl = rounds_log.setdefault(unit, [])
    if rl:
        rl[-1].update({"verdict": verdict, "lanes": lanes, "incomplete": incomplete,
                       "lanes_with_findings": [l for l, v in lanes.items() if v not in ("GREEN", "GREEN (attention)", "SKIPPED", "")]})
    write_artifact(state.get("run_dir", ""), os.path.join("rounds", f"{unit}-r{len(rl)}.md"), text or "(empty)")
    u["round_log"] = rounds_log
    u["last_verdict"] = verdict
    rounds = dict(as_dict(state.get("task_rounds")))
    tr = rounds.setdefault(task.get("id", unit), {})
    u["task_rounds"] = rounds
    ledger(state, u, f"verdict: {unit} round {len(rl)} -> {verdict} lanes={lanes} incomplete={incomplete}")
    real = [b for b in blockers if "PIPELINE-FAULT" not in b and "review incomplete" not in b.lower()]
    if verdict == "PASS" and not real:
        if probe == "PASS" and probe_files and as_int(tr.get("adopt_rounds")) < RERUN_ONCE and scope == "task":
            tr["adopt_rounds"] = as_int(tr.get("adopt_rounds")) + 1
            where = "as a commit on the run branch" if (is_single(state) or MULTI_IS_GROUPS) else "into the task's PR"
            u.update({"delegation_kind": "fix", "fix_source": "probe_adopt", "fix_return_to": "post_round",
                      "fix_items": [f"Adopt the probe's new test file {f} {where} so it ships as permanent regression coverage" for f in probe_files],
                      "fix_note": "The usage probe PASSED and wrote new test files; commit them (no behavior change)."})
            ledger(state, u, f"verdict: probe PASS wrote {len(probe_files)} test file(s) -> one adopt round")
            u["_next"] = "compose_delegation"
            return u
        if scope == "batch":
            u["_next"] = "batch_close"
        else:
            u["_next"] = "post_round"
        snapshot(state, u, f"task {unit} verified")
        return u
    if real:
        if probe == "INCONCLUSIVE" and all("probe" in b.lower() or "inconclusive" in b.lower() for b in real):
            return raise_escalation(state, u, escalation(
                "probe_gap", "The usage probe is INCONCLUSIVE: the local-run recipe is missing or broken (a PLAN gap, never a code "
                             "verdict). Supply/fix the recipe (the plan is amended and re-sealed, then the probe re-runs once), or surface the gap?",
                [option("Supply the recipe (type it); amend the plan and re-run the probe once"),
                 option("Surface the gap as a follow-up and proceed", recommended=True)], evidence=tail(text, 2000)))
        if scope == "batch":
            u["gauntlet_blockers"] = real
            u["_next"] = "batch_triage"
            return u
        if plan_root:
            return raise_escalation(state, u, escalation(
                "plan_defect", "The adversary reports the PLAN itself as the root cause (impossible or contradictory criterion). "
                               "Never fixed by changing scope: amend the plan (re-sealed), or stop?",
                [option("Amend the plan (type the amendment); re-seal; re-verify", recommended=True), option("Stop and report")],
                evidence="\n".join(f"- {b}" for b in real)[:3000], hard_trigger=True))
        used = as_int(tr.get("conformance_rounds"))
        if used >= RERUN_ONCE:
            return raise_escalation(state, u, escalation(
                "conformance_stuck", f"{unit} still fails conformance after one fix round and one re-run (protocol: re-run ONCE). "
                                     "How to proceed?",
                [option("Stop and report", recommended=True), option("One more fix round anyway"),
                 option("Accept the findings unresolved and close the task (recorded)")],
                evidence="\n".join(f"- {b}" for b in real)[:3000], hard_trigger=True))
        tr["conformance_rounds"] = used + 1
        u.update({"delegation_kind": "fix", "fix_source": "review", "fix_return_to": "verify",
                  "fix_items": real, "fix_note": "Fix these plan-conformance / review failures (verbatim below)."})
        ledger(state, u, f"verdict: {len(real)} finding(s) -> fix round {used + 1} (re-run once after)")
        u["_next"] = "compose_delegation"
        return u
    # incomplete only
    if incomplete == ["debris"] and as_int(tr.get("debris_reruns")) < RERUN_ONCE:
        tr["debris_reruns"] = as_int(tr.get("debris_reruns")) + 1
        ledger(state, u, "verdict: debris lint produced no verdict; one re-run (transient assumed)")
        u["_next"] = "verify_gate"
        return u
    lanes_txt = ", ".join(incomplete or ["pipeline"])
    return raise_escalation(state, u, escalation(
        "review_incomplete",
        f"The review of {unit} could not be completed: {lanes_txt} still faulted (not a code finding). Retry, accept "
        f"NEEDS-HUMAN (proceed without the {lanes_txt} review, recorded in the report / PR body), or abort?",
        [option("Retry the review again"),
         option("Accept NEEDS-HUMAN: proceed without the incomplete review (recorded in the report / PR body)"),
         option("Abort this task")], evidence=tail(text, 1500)))


def fix_round(state):
    """Entry for externally-queued fix rounds (integration / CI / batch): sets the delegation context."""
    u = {}
    f = as_dict(state.get("fix_request"))
    u.update({"current_task": f.get("task") or state.get("current_task"), "delegation_kind": "fix",
              "fix_items": as_list(f.get("items")), "fix_source": f.get("source", "review"),
              "fix_return_to": f.get("return_to", "select_tasks"), "fix_note": f.get("note", ""), "fix_request": {}})
    u["_next"] = "compose_delegation"
    return u


def _lane_table(text):
    return "\n".join(_lane_rows(_gauntlet_header(text or ""))[:12])


def _pr_comment(state, pr_number, body):
    code = state.get("code_repo_path", "")
    if not pr_number:
        return False, "no PR"
    cmd = os.environ.get("ARCHITECT_GH_COMMENT_CMD")
    if cmd:
        return sh(["bash", "-c", cmd], timeout=60)[0] == 0, "stub"
    rc, out = sh(["gh", "pr", "comment", str(pr_number), "--body", body], cwd=code, timeout=120)
    return rc == 0, out


def post_round(state):
    u = {}
    note_script_error(state, u, "delegate")
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task")) or {}
    unit = state.get("verify_unit") or task.get("id") or "run"
    rl = as_list(as_dict(state.get("round_log")).get(unit))
    n = len(rl)
    head = rl[-1].get("head") if rl else ""
    text = read_artifact(state.get("run_dir", ""), os.path.join("rounds", f"{unit}-r{n}.md")) if n else ""
    title = f"## {'Batch review round' if state.get('verify_scope') == 'batch' else 'Review round'} {n} @ {head}"
    body = f"{title}\n\n{_lane_table(text) or '(lane table unavailable)'}\n\nVerdict: {state.get('last_verdict', '')}"
    if n > 1:
        body += f"\n\nSince round {n - 1}: {rl[-2].get('head', '')}..{head}"
    if (is_single(state) or MULTI_IS_GROUPS):
        pending = list(as_list(state.get("pending_pr_comments")))
        pending.append({"unit": unit, "body": body, "task": task.get("id", "")})
        u["pending_pr_comments"] = pending
    elif task.get("pr_number"):
        ok, out = _pr_comment(state, task["pr_number"], body)
        ledger(state, u, f"pr-comment: {task['id']} PR #{task['pr_number']} round {n}: {'posted' if ok else 'FAILED ' + tail(out, 120)}")
    if state.get("fix_source") == "probe_adopt" and state.get("delegation_kind") == "fix":
        u.update({"delegation_kind": "implement", "fix_source": "", "fix_items": []})
    if task.get("status") == "complete" and state.get("delegation_kind") == "fix":
        u["_next"] = "fix_done"
    else:
        u["_next"] = "harvest_report"
    return u


def fix_done(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task")) or {}
    src = state.get("fix_source") or "review"
    ret = state.get("fix_return_to") or "select_tasks"
    log_event(state, task, f"fix round ({src}) landed; HEAD {task.get('head_sha', '')}; verdict {state.get('last_verdict', '')}")
    ledger(state, u, f"fix-done: {task.get('id')} ({src}) -> {ret}")
    u.update({"delegation_kind": "implement", "fix_items": [], "fix_source": "", "fix_return_to": ""})
    if ret == "batch":
        if as_list(state.get("pending_fixes")):
            u["_next"] = "select_tasks"
        else:
            u["_next"] = "batch_gauntlet_build"
    elif ret == "integration":
        u["_next"] = "integration_test"
    elif ret == "integration_check":
        u["_next"] = "integration_check"
    elif ret == "ci":
        u.update({"ci_polls": 0, "ci_poll_fails": 0})
        u["_next"] = "ci_poll"
    else:
        u["_next"] = "select_tasks"
    return u


def harvest_report(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task")) or {}
    p = as_dict(state.get("report_parsed"))
    lines = []
    rejected = list(as_list(state.get("rejected_findings")))
    for d in as_list(p.get("design")):
        lines.append(f"design-decision: {d}")
    for e in as_list(p.get("exemptions")):
        lines.append(f"debris-exempt: {e}")
    for f in as_list(p.get("follow_ups")):
        if f.lower() != "none":
            lines.append(f"Follow-ups: {f}")
    for a in as_list(p.get("autonomous")):
        lines.append(f"autonomous (child): {a}")
    text = read_artifact(state.get("run_dir", ""), os.path.join("reports", f"{task.get('id')}-r{_prior_report(state, task.get('id', ''))[1]}.md"))
    for m in re.finditer(r"(?im)^\W*rejected-finding:\s*(.+)$", text):
        line = m.group(1).strip()
        if re.search(r"[\w./-]+:\d+", line) or "quality bar" in line.lower():
            lines.append(f"rejected-finding: {line}")
            if line not in rejected:
                rejected.append(line)
        else:
            ledger(state, u, f"harvest: uncited rejection stands as a finding: {tail(line, 120)}")
    u["rejected_findings"] = rejected
    u["ledger_lines"] = lines
    u["design_asks"] = as_list(p.get("design_asks"))
    u["debt_items"] = as_list(p.get("debt"))
    fus = list(as_list(state.get("follow_ups")))
    for f in as_list(p.get("follow_ups")):
        if f.lower() != "none":
            fus.append(f"({task.get('id')}) {f}")
    u["follow_ups"] = fus
    u["_next"] = "design_ask_gate"
    return u


def design_ask_gate(state):
    u = {}
    asks = as_list(state.get("design_asks"))
    if not asks:
        u["_next"] = "debt_assign"
        return u
    q = asks[0]
    u["design_asks"] = asks[1:]
    return raise_escalation(state, u, escalation(
        "design_ask", f"Design question: {q}", [option("Adopt the pattern (work: one fix round follows)"),
                                                option("Record the decision without adopting", recommended=False)],
        evidence="A `Design: ASK --` line from the implementer's report; never self-answered.", hard_trigger=True, tier3_ok=False))


def debt_assign(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task")) or {}
    items = as_list(state.get("debt_items"))
    lines = list(as_list(state.get("ledger_lines")))
    for it in items:
        owner = None
        if m := TASK_ID_RE.search(it):
            cand = m.group(0)
            if cand != task.get("id") and task_by_id(tasks, cand):
                owner = cand
        if not owner:
            slug = re.sub(r"[^a-z0-9-]+", "-", f"remove-debt-{it[:30].lower()}").strip("-")[:40]
            tid, tdir = new_task(state, slug, f"Remove debt: {it[:60]}", f"## What\n\nRemove the debt marker left by {task.get('id')}: {it}\n\n## Acceptance criteria\n\n- the marker is gone from the diff\n",
                                 blocked_by=(task.get("id"),), task_type="chore", plan_name=os.path.basename(state.get("plan_path", "")))
            if tid:
                tasks.append({"id": tid, "slug": os.path.basename(tdir), "dir": tdir, "title": f"Remove debt: {it[:60]}", "status": "pending",
                              "blocked_by": [task.get("id")], "batch": task.get("batch", ""), "protected": False, "surfaces": "",
                              "type": "chore", "files": [], "group": task.get("group", ""), "unit": task.get("unit", "run")})
                owner = tid
                ledger(state, u, f"debt: no owner named -> chore task {tid}")
            else:
                ledger(state, u, f"debt: could not create a chore task for {tail(it, 80)}: {tdir}")
        if owner:
            odir = task_dir_for(state, owner)
            if odir:
                idx = os.path.join(odir, "index.md")
                with open(idx, "a", encoding="utf-8") as f:
                    f.write(f"\n- [ ] Remove the debt marker left by {task.get('id')}: {it}\n")
                log_event(state, task_by_id(tasks, owner) or {"id": owner, "dir": odir}, f"debt-assigned: from {task.get('id')}: {it}")
            lines.append(f"debt-assigned: {owner}: {it}")
    u["ledger_lines"] = lines
    save_tasks(state, u, tasks)
    u["_next"] = "task_done"
    return u


def task_done_verify(state):
    u = {}
    harvest_anomaly_lines(state, u, state.get("task_done_report") or "", "task_done")
    note_script_error(state, u, "task_done")
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task")) or {}
    tdir = task_dir_for(state, task)
    log_path = os.path.join(tdir, "log.md") if tdir else ""
    log_text = read_file(log_path) if log_path else ""
    p = as_dict(state.get("report_parsed"))
    if not re.search(r"(?im)^##.*completed", log_text):
        shas = ", ".join(p.get("shas") or []) or "none recorded"
        log_event(state, task, f"completed: architect-next run {state.get('run_id')}; " + (f"PR {p.get('pr_url')}" if p.get("pr_url") else f"run-branch commit SHAs {shas}"))
        ledger(state, u, f"task-done: completed entry was missing for {task.get('id')}; appended by the script")
        log_text = read_file(log_path)
    missing = [l for l in as_list(state.get("ledger_lines")) if l.split(":", 1)[-1].strip()[:40] not in log_text]
    if missing and log_path:
        with open(log_path, "a", encoding="utf-8") as f:
            f.write("\n" + "\n".join(f"- {l}" for l in missing) + "\n")
        ledger(state, u, f"task-done: {len(missing)} ledger line(s) re-appended verbatim for {task.get('id')}")
    if TASKSTORE == "dirstate":
        ok, out = core_tool(STORE_TOOL_PREFIX + "close", task_dir=tdir, title=task.get("title", ""))
        if not ok:
            ledger(state, u, f"task-done: store close tool failed for {task.get('id')}: {tail(out, 300)}")
            return raise_escalation(state, u, escalation(
                "promote_failed", f"The store close for {task.get('id')} failed (validate RED or push rejected). Fix and retry, or abort?",
                [option("Retry the close", recommended=True), option("Abort the run")], evidence=tail(out, 2000), hard_trigger=True))
        task["dir"] = os.path.join(state.get("tasks_root", ""), "completed", os.path.basename(tdir)) if tdir else task.get("dir", "")
    else:
        set_task_status(state, task, "complete")
        commit_planning(state, f"chore(plan): complete {task.get('id')}")
    task["status"] = "complete"
    task["completed_at"] = now_iso()
    save_tasks(state, u, tasks)
    ledger(state, u, f"task-done: {task.get('id')} complete")
    snapshot(state, u, f"task {task.get('id')} done")
    u["_next"] = "stack_gate"
    return u


def _chains(tasks):
    """Connected components of the blocked_by graph among tasks with PRs (per-task-stack multi)."""
    ids = [t["id"] for t in tasks]
    parent = {i: i for i in ids}

    def find(x):
        while parent[x] != x:
            parent[x] = parent[parent[x]]
            x = parent[x]
        return x

    for t in tasks:
        for b in t.get("blocked_by", []):
            if b in parent:
                parent[find(t["id"])] = find(b)
    comps = {}
    for i in ids:
        comps.setdefault(find(i), []).append(i)
    return list(comps.values())


def _order_chain(tasks, chain):
    done, order = set(), []
    members = [task_by_id(tasks, i) for i in chain]
    while len(order) < len(members):
        progressed = False
        for t in members:
            if t["id"] in done:
                continue
            if all(b in done or b not in chain for b in t.get("blocked_by", [])):
                order.append(t)
                done.add(t["id"])
                progressed = True
        if not progressed:
            order += [t for t in members if t["id"] not in done]
            break
    return order


def stack_gate(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    if not STACK_LINKING or is_single(state) or MULTI_IS_GROUPS:
        u["_next"] = "task_done_route"
        return u
    linked = list(as_list(state.get("linked_chains")))
    for chain in _chains(tasks):
        key = ",".join(sorted(chain))
        if key in linked or len(chain) < 2:
            continue
        members = _order_chain(tasks, chain)
        if not all(t.get("pr_number") for t in members):
            continue
        if state.get("closeout_scope") != "run" and any(t.get("status") != "complete" for t in members):
            continue
        repo = ""
        if m := PR_URL_RE.search(members[0].get("pr_url", "")):
            repo = m.group(1)
        ok, out = core_tool("link_stack", repo=repo, prs=",".join(t["pr_number"] for t in members), base=state.get("base_branch_name") or "main")
        verdict = (re.search(r"STACK_LINK:\s*(.+)", out or "") or [None, "UNLINKED (tool unavailable: " + tail(out, 120) + ")"])[1]
        linked.append(key)
        ledger(state, u, f"stack: chain {chain} -> {verdict}")
        for t in members:
            log_event(state, t, f"stack: {verdict}")
    u["linked_chains"] = linked
    u["_next"] = "task_done_route"
    return u


def task_done_route(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task")) or {}
    if is_micro(state) and task.get("batch"):
        members = [t for t in tasks if t.get("batch") == task["batch"]]
        closed = all(t.get("status") == "complete" for t in members) or truthy(task.get("protected"))
        bs = dict(as_dict(state.get("batch_state")))
        b = bs.setdefault(task["batch"], {"strikes": 0, "passed": False, "base_sha": ""})
        if closed and not b.get("passed"):
            first = min(members, key=lambda t: int(re.search(r"\d+", t["id"]).group()))
            b["base_sha"] = b.get("base_sha") or first.get("base_sha", "")
            u["batch_state"] = bs
            u["current_batch"] = task["batch"]
            u["batch_base_sha"] = b["base_sha"]
            ledger(state, u, f"batch: {task['batch']} closed by {task['id']} ({'protected' if truthy(task.get('protected')) else 'last member'}) -> full gauntlet")
            u["_next"] = "batch_gauntlet_build"
            return u
    if task.get("worktree"):
        u["_next"] = "wave_cleanup"
        return u
    if mode(state)["autonomy"] == "phase-gate":
        u["ask_question"] = f"phase-gate: {task.get('id')} ({task.get('title', '')}) is complete. Continue to the next task?"
        u["escalation"] = escalation("approve_next", u["ask_question"], [option("Continue", recommended=True)])
        u["_next"] = "approve_next"
        return u
    u["_next"] = "select_tasks"
    return u


# ====================================================================== 2.6 parallel
def wave_claim(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    code = state.get("code_repo_path", "")
    ids = as_list(state.get("wave_ids"))
    members = [task_by_id(tasks, i) for i in ids if task_by_id(tasks, i)]
    git(code, "fetch", "--quiet", "origin")
    # Worktrees live inside the repo; keep them out of `git status` without touching tracked files.
    exclude = os.path.join(code, ".git", "info", "exclude")
    if os.path.isdir(os.path.dirname(exclude)) and ".worktrees/" not in read_file(exclude):
        with open(exclude, "a", encoding="utf-8") as f:
            f.write("\n.worktrees/\n")
    roster_lines = [f"- {t['id']} ({t.get('title', '')}) scope: {', '.join(t.get('files') or [])}" for t in members]
    wave = []
    for t in members:
        _claim(state, u, t, tasks)
        if is_single(state) or MULTI_IS_GROUPS:
            t["branch"] = f"{state.get('run_branch') or _run_branch(state)}-task-{t['id'].split('-', 1)[1].lower()}"
            start = state.get("run_branch") or _run_branch(state)
        else:
            start = f"origin/{t['pr_base']}"
        wt = os.path.join(code, ".worktrees", f"task-{t['id'].split('-', 1)[1].lower()}")
        rc, out, err = git(code, "worktree", "add", wt, "-b", t["branch"], start)
        if rc != 0:
            ledger(state, u, f"wave: worktree add failed for {t['id']}: {tail(err, 200)}")
        t["worktree"] = wt
        t["base_sha"] = git_out(wt, "rev-parse", "HEAD", default=t.get("base_sha", ""))
        roster = "\n".join(l for l in roster_lines if not l.startswith(f"- {t['id']}")) or "(no siblings)"
        prompt = _delegation_text(state, t, "implement", project_dir=wt, roster=roster)
        wave.append({"task": t["id"], "prompt": prompt, "worktree": wt, "branch": t["branch"], "base_sha": t["base_sha"]})
        ledger(state, u, f"mode-degraded: {t['id']}: parallel=1 + project_dir={wt} carried in the prompt (config-agent child; E9)")
    u["wave"] = wave
    u["wave_fix_rounds"] = 0
    save_tasks(state, u, tasks)
    snapshot(state, u, "wave")
    return u


def wave_verify_build(state):
    """Fork-local: the member's conformance prompt (same shape as verify_gate at task scope)."""
    u = {}
    w = as_dict(state.get("wtask"))
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, w.get("task")) or {"id": w.get("task", ""), "base_sha": w.get("base_sha", "")}
    sub = dict(state)
    sub.update({"current_task": task["id"], "verify_scope": "task", "delegate_project_dir": w.get("worktree", ""),
                "round_log": {}, "tasks": tasks})
    inner = verify_gate(sub)
    u.update({k: inner[k] for k in ("gauntlet_prompt", "adversary_prompt", "verification_commands_json", "debris_exclude") if k in inner})
    u["wave_report"] = state.get("wave_delegate_result") or ""
    return u


def wave_route(state):
    u = {}
    w = as_dict(state.get("wtask"))
    text = state.get("wave_gauntlet_result") or ""
    report = state.get("wave_report") or state.get("wave_delegate_result") or ""
    verdict, incomplete, lanes, blockers, plan_root, _ = _parse_gauntlet(text)
    if not text or text.startswith("Agent node failed") or not _has_verdict(_gauntlet_header(text)):
        verdict, incomplete = "BLOCKED", incomplete or ["pipeline"]
    real = [b for b in blockers if "PIPELINE-FAULT" not in b and "review incomplete" not in b.lower()]
    result = {"task": w.get("task"), "worktree": w.get("worktree"), "branch": w.get("branch"), "base_sha": w.get("base_sha"),
              "report": tail(report, 12000), "verdict": verdict, "lanes": lanes, "review": tail(text, 6000)}
    if _report_failed(report):
        result.update({"status": "failed", "escalation": {"kind": "task_failed", "evidence": tail(report, 2000)}})
        return {"wave_result": result}
    if verdict == "PASS" and not real:
        result["status"] = "conforms"
        return {"wave_result": result}
    if real and not plan_root and as_int(state.get("wave_fix_rounds")) < RERUN_ONCE:
        fixed = dict(w)
        fixed["prompt"] = (w.get("prompt", "") + "\n\n## FIX ROUND 1\nFix these plan-conformance / review failures (verbatim):\n"
                           + "\n".join(f"- {b}" for b in real) + "\n\n### Your prior report (verbatim)\n" + tail(report, 8000))
        return {"wtask": fixed, "wave_fix_rounds": as_int(state.get("wave_fix_rounds")) + 1, "_next": "wave_delegate"}
    kind = "plan_defect" if plan_root else ("review_incomplete" if not real else "conformance_stuck")
    result.update({"status": "needs_escalation", "escalation": {"kind": kind, "evidence": "\n".join(f"- {b}" for b in real)[:3000] or tail(text, 1500),
                                                                 "incomplete": incomplete}})
    return {"wave_result": result}


def wave_fault(state):
    w = as_dict(state.get("wtask"))
    return {"wave_result": {"task": w.get("task"), "worktree": w.get("worktree"), "branch": w.get("branch"), "status": "failed",
                            "report": tail(state.get("wave_delegate_result") or state.get("wave_gauntlet_result") or "", 2000),
                            "escalation": {"kind": "task_failed", "evidence": "branch fault: " + tail(state.get("last_script_error", "") or "agent node failed", 500)}}}


def wave_integrate(state):
    u = {}
    note_script_error(state, u, "wave_map")
    results = list(as_list(state.get("wave_results")))
    idx = as_int(state.get("wave_index"))
    tasks = as_list(state.get("tasks"))
    if idx >= len(results):
        u.update({"wave_index": 0, "wave_results": [], "wave": [], "wave_ids": []})
        if mode(state)["autonomy"] == "phase-gate" and results:
            done = [r.get("task") for r in results if isinstance(r, dict)]
            u["ask_question"] = f"phase-gate: parallel wave {done} is integrated. Continue to the next task(s)?"
            u["escalation"] = escalation("approve_next", u["ask_question"], [option("Continue", recommended=True)])
            u["_next"] = "approve_next"
            return u
        u["_next"] = "select_tasks"
        return u
    r = results[idx] if isinstance(results[idx], dict) else {}
    task = task_by_id(tasks, r.get("task")) or {}
    u["wave_index"] = idx + 1
    u["current_task"] = r.get("task")
    write_artifact(state.get("run_dir", ""), os.path.join("reports", f"{r.get('task')}-r{_prior_report(state, r.get('task', ''))[1] + 1}.md"), r.get("report") or "(empty)")
    task.update({"worktree": r.get("worktree", task.get("worktree")), "branch": r.get("branch", task.get("branch"))})
    parsed = _parse_report(r.get("report") or "")
    u["report_parsed"] = parsed
    if parsed["pr_number"]:
        task["pr_number"], task["pr_url"] = parsed["pr_number"], parsed["pr_url"]
    save_tasks(state, u, tasks)
    status = r.get("status")
    if status in ("needs_escalation", "failed"):
        esc = as_dict(r.get("escalation"))
        task["status"] = "parked"
        save_tasks(state, u, tasks)
        ledger(state, u, f"wave: {r.get('task')} {status} -> escalation {esc.get('kind')} (deferred to the wave boundary, E7)")
        return raise_escalation(state, u, escalation(
            "wave_escalation", f"Parallel task {r.get('task')} ended {status} ({esc.get('kind')}). How to proceed?",
            [option("Retry the task sequentially (one fix round with the evidence)", recommended=True),
             option("Accept as-is and close the task (recorded)"), option("Abort this task (worktree removed, branch kept)")],
            evidence=tail(esc.get("evidence", ""), 2500), hard_trigger=True, return_to="wave_integrate"))
    if is_single(state) or MULTI_IS_GROUPS:
        u["integration_task"] = r.get("task")
        u["_next"] = "integration_test"
        return u
    ledger(state, u, f"wave: {r.get('task')} conforms; PR #{task.get('pr_number') or '?'} opened by Sisyphus")
    u["_next"] = "harvest_report"
    return u


def _run_verification(state, cwd):
    cmds = json.loads(_verification_commands(state))
    if not cmds:
        return True, "(no verification commands declared)"
    out_all, start = [], time.time()
    for c in cmds:
        if time.time() - start > VERIFY_SET_SECS:
            return False, "\n".join(out_all) + f"\nverification set exceeded {VERIFY_SET_SECS}s"
        rc, out = sh(["bash", "-lc", c], cwd=cwd, timeout=VERIFY_CMD_SECS)
        out_all.append(f"$ {c}\n{tail(out, 3000)}\n(exit {rc})")
        if rc != 0:
            return False, "\n".join(out_all)
    return True, "\n".join(out_all)


def integration_test(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    tid = state.get("integration_task") or state.get("current_task")
    task = task_by_id(tasks, tid) or {}
    code = state.get("code_repo_path", "")
    run_branch = state.get("run_branch") or _run_branch(state)
    rounds = dict(as_dict(state.get("task_rounds")))
    tr = rounds.setdefault(tid, {})
    u["task_rounds"] = rounds
    u["current_task"] = tid
    if not truthy(tr.get("merged")):
        git(code, "checkout", "--quiet", run_branch)
        rc, out, err = git(code, "merge", "--no-ff", "--no-edit", task.get("branch", ""))
        if rc != 0:
            git(code, "merge", "--abort")
            cycles = as_int(tr.get("conflict_cycles")) + 1
            tr["conflict_cycles"] = cycles
            ledger(state, u, f"integration: merge conflict for {tid} (cycle {cycles}/{CONFLICT_CYCLES_MAX})")
            if cycles > CONFLICT_CYCLES_MAX:
                return raise_escalation(state, u, escalation(
                    "integration_conflict", f"{tid} conflicts with the run branch after {CONFLICT_CYCLES_MAX} rebase cycles. How to proceed?",
                    [option("Stop and report", recommended=True), option("One more rebase cycle"), option("Abort this task (branch kept)")],
                    evidence=tail(out + err, 2000), hard_trigger=True))
            u["fix_request"] = {"task": tid, "source": "integration", "return_to": "integration",
                                "items": [f"Merge conflict: rebase your task branch {task.get('branch')} onto the current {run_branch}, resolve, re-verify, re-push"],
                                "note": tail(out + err, 3000)}
            u["_next"] = "fix_round"
            return u
        tr["merged"] = True
    ok, out = _run_verification(state, code)
    if not ok:
        strikes = as_int(tr.get("integration_strikes")) + 1
        tr["integration_strikes"] = strikes
        ledger(state, u, f"integration: suite RED after merging {tid} (strike {strikes}/{STRIKE_LIMIT})")
        if strikes > STRIKE_LIMIT:
            return raise_escalation(state, u, escalation(
                "integration_red", f"The run branch suite is still RED after {STRIKE_LIMIT} fix rounds following {tid}'s merge. How to proceed?",
                [option("Stop and report", recommended=True), option("One more fix round anyway")], evidence=tail(out, 2500), hard_trigger=True))
        u["fix_request"] = {"task": tid, "source": "integration", "return_to": "integration",
                            "items": [f"Post-merge integration failure on {run_branch} (the task was verified against its fork point, not siblings' merged work)"],
                            "note": tail(out, 4000)}
        u["_next"] = "fix_round"
        return u
    tr["merged"] = False
    git(code, "push", "--quiet", "origin", run_branch)
    task["head_sha"] = git_out(code, "rev-parse", "HEAD", default="")
    save_tasks(state, u, tasks)
    ledger(state, u, f"integration: {tid} merged into {run_branch} @ {task['head_sha']}; suite green; pushed")
    u["_next"] = "harvest_report"
    return u


def wave_cleanup(state):
    """The four cleanup rules (markers 4-6 / 72-74): the ONLY unasked destructive ops."""
    u = {}
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task")) or {}
    code = state.get("code_repo_path", "")
    wt = task.get("worktree", "")
    if wt:
        rc, out, err = git(code, "worktree", "remove", wt)
        if rc != 0:
            git(code, "worktree", "remove", "--force", wt)
        task["worktree"] = ""
        ledger(state, u, f"cleanup: worktree {wt} removed")
    if (is_single(state) or MULTI_IS_GROUPS) and task.get("status") == "complete" and task.get("branch") and task["branch"] != (state.get("run_branch") or _run_branch(state)):
        git(code, "branch", "-D", task["branch"])
        git(code, "push", "--quiet", "origin", "--delete", task["branch"])
        ledger(state, u, f"cleanup: merged task branch {task['branch']} deleted (local + remote)")
        task["branch"] = state.get("run_branch") or _run_branch(state)
    elif task.get("branch"):
        ledger(state, u, f"cleanup: task branch {task['branch']} kept ({'multi: never deleted' if not (is_single(state) or MULTI_IS_GROUPS) else 'not cleanly merged'})")
    git(code, "worktree", "prune")
    save_tasks(state, u, tasks)
    u["_next"] = "wave_integrate" if as_list(state.get("wave_results")) else "select_tasks"
    return u


# ====================================================================== 2.7 micro batches
def batch_gauntlet_build(state):
    u = {}
    batch = state.get("current_batch") or ""
    bs = dict(as_dict(state.get("batch_state")))
    b = bs.setdefault(batch, {"strikes": 0, "passed": False, "base_sha": state.get("batch_base_sha", "")})
    u["batch_state"] = bs
    u["batch_base_sha"] = b.get("base_sha") or state.get("batch_base_sha", "")
    u["verify_scope"] = "batch"
    u["verify_unit"] = batch
    members = [t["id"] for t in as_list(state.get("tasks")) if t.get("batch") == batch]
    u["current_task"] = members[-1] if members else state.get("current_task")
    ledger(state, u, f"batch: {batch} gauntlet over {u['batch_base_sha']}..HEAD (members {members})")
    u["_next"] = "verify_gate"
    return u


def batch_fix_route(state):
    u = {}
    harvest_anomalies(state, u, "batch_triage")
    note_script_error(state, u, "batch_triage")
    batch = state.get("current_batch") or ""
    tasks = as_list(state.get("tasks"))
    bs = dict(as_dict(state.get("batch_state")))
    b = bs.setdefault(batch, {"strikes": 0, "passed": False, "base_sha": state.get("batch_base_sha", "")})
    b["strikes"] = as_int(b.get("strikes")) + 1
    u["batch_state"] = bs
    blockers = as_list(state.get("gauntlet_blockers"))
    routed = [r for r in as_list(state.get("finding_routes")) if isinstance(r, dict) and r.get("finding")]
    members = [t for t in tasks if t.get("batch") == batch]
    if not routed:
        last = members[-1]["id"] if members else state.get("current_task")
        routed = [{"finding": bl, "task": last} for bl in blockers]
    if b["strikes"] > STRIKE_LIMIT:
        return raise_escalation(state, u, escalation(
            "batch_strikeout", f"Batch {batch} is still BLOCKED after {STRIKE_LIMIT} gauntlet rounds (3 strikes per batch). How to proceed?",
            [option("Stop and report", recommended=True), option("One more batch round anyway"), option("Accept the batch unresolved (recorded)")],
            evidence="\n".join(f"- {r.get('finding')}" for r in routed)[:3000], hard_trigger=True))
    by_task = {}
    for r in routed:
        tid = r.get("task") or ""
        if task_by_id(tasks, tid) and tid in [m["id"] for m in members]:
            by_task.setdefault(tid, []).append(r["finding"])
        else:
            by_task.setdefault("__cross__", []).append(r["finding"])
    fixes = list(as_list(state.get("pending_fixes")))
    for tid, items in by_task.items():
        if tid == "__cross__":
            continue
        fixes.append({"task": tid, "items": items, "source": "batch", "return_to": "batch",
                      "note": f"Batch {batch} gauntlet finding(s) routed to this task by file:line."})
    cross = by_task.get("__cross__", [])
    spec = as_dict(state.get("cross_task_fix"))
    if cross or spec.get("title"):
        title = spec.get("title") or f"Batch {batch} interaction fix"
        body = ("## What\n\n" + (spec.get("outcome") or "Fix the cross-task interaction findings from the batch gauntlet:\n" + "\n".join(f"- {c}" for c in cross))
                + "\n\n## Acceptance criteria\n\n" + "\n".join(f"- {c}" for c in as_list(spec.get("acceptance_criteria")) or ["- the batch gauntlet's interaction findings no longer reproduce (`<command from the finding>`)"]) + "\n")
        tid, tdir = new_task(state, f"{batch.lower()}-interaction-fix", title, body, blocked_by=tuple(m["id"] for m in members),
                             task_type="chore", batch=batch, plan_name=os.path.basename(state.get("plan_path", "")))
        if tid:
            tasks.append({"id": tid, "slug": os.path.basename(tdir), "dir": tdir, "title": title, "status": "pending", "blocked_by": [m["id"] for m in members],
                          "batch": batch, "protected": False, "surfaces": "", "type": "chore", "files": as_list(spec.get("files")),
                          "group": members[0].get("group", "") if members else "", "unit": members[0].get("unit", "run") if members else "run"})
            ledger(state, u, f"batch: cross-task findings -> fix micro-task {tid} in batch {batch}")
            b["passed"] = False
        else:
            ledger(state, u, f"batch: could not create the interaction fix task: {tdir}")
    u["pending_fixes"] = fixes
    save_tasks(state, u, tasks)
    ledger(state, u, f"batch: {batch} strike {b['strikes']}/{STRIKE_LIMIT}; {len(fixes)} fix round(s) queued; cross={len(cross)}")
    snapshot(state, u, f"batch {batch} fixes")
    u["_next"] = "select_tasks"
    return u


def batch_close(state):
    u = {}
    batch = state.get("current_batch") or ""
    tasks = as_list(state.get("tasks"))
    bs = dict(as_dict(state.get("batch_state")))
    b = bs.setdefault(batch, {})
    b["passed"] = True
    head = git_out(state.get("code_repo_path", ""), "rev-parse", "HEAD", default="")
    b["closed_at"] = head
    u["batch_state"] = bs
    for t in tasks:
        if t.get("batch") == batch:
            log_event(state, t, f"batch-closed @ {head}")
    ledger(state, u, f"batch: {batch} PASS; batch-closed @ {head} logged on {sum(1 for t in tasks if t.get('batch') == batch)} task(s)")
    inner = post_round({**state, "verify_scope": "batch", "verify_unit": batch, "current_task": state.get("current_task")})
    for k in ("pending_pr_comments", "decisions"):
        if k in inner:
            u[k] = inner[k]
    u["verify_scope"] = "task"
    snapshot(state, u, f"batch {batch} closed")
    u["_next"] = "select_tasks"
    return u


# ====================================================================== 2.8 escalation ladder
def escalate_gate(state):
    u = {}
    esc = as_dict(state.get("escalation"))
    kind = esc.get("kind", "unknown")
    ledger(state, u, f"escalation: {kind}: {tail(esc.get('question', ''), 200)}")
    policy = str(var(state, "escalation_policy", "interactive")).strip().lower()
    failed_once = kind in as_list(state.get("autonomous_kinds"))
    if policy == "autonomous" and not esc.get("hard_trigger") and not failed_once:
        rulings = ""
        path = os.path.join(os.path.expanduser("~"), ".config", "coyote", "share", "standing-rulings.md")
        if os.path.isfile(path):
            with open(path, encoding="utf-8", errors="replace") as f:
                rulings = f.read()
        u["standing_rulings"] = rulings or "(no standing rulings file)"
        u["_next"] = "ladder_judge"
    else:
        if failed_once:
            ledger(state, u, f"ladder: {kind} already resolved autonomously once; hard trigger -> owner")
        u["_next"] = "ask_dispatch"
    return u


def ladder_route(state):
    u = {}
    harvest_anomalies(state, u, "ladder_judge")
    esc = as_dict(state.get("escalation"))
    res = state.get("ladder_resolution") or "escalate"
    if res in ("ruling", "recommended"):
        choice = match_option(state.get("ladder_choice") or "", esc.get("options") or [])
        if choice is None and res == "recommended":
            rec = [o for o in esc.get("options") or [] if o.get("recommended")]
            choice = rec[0]["text"] if len(rec) == 1 else None
        if choice is None and not esc.get("options"):
            choice = state.get("ladder_choice") or ""
        if choice:
            basis = state.get("ladder_basis") or res
            line = f"{esc.get('kind')}: {choice} (basis: {basis})"
            u.update({"autonomous_digest": list(as_list(state.get("autonomous_digest"))) + [line],
                      "autonomous_kinds": list(as_list(state.get("autonomous_kinds"))) + [esc.get("kind")],
                      "last_answer": choice, "last_answer_text": choice})
            ledger(state, u, f"autonomous: {line}")
            u["_next"] = "apply_answer"
            return u
        ledger(state, u, f"ladder: {res} named no matching option; escalating")
    if res == "consult" and esc.get("tier3_ok", True):
        u["_next"] = "consult_navigator"
        return u
    u["_next"] = "ask_dispatch"
    return u


def consult_route(state):
    u = {}
    note_script_error(state, u, "consult_navigator")
    esc = as_dict(state.get("escalation"))
    text = state.get("consult_result") or ""
    if m := re.search(r"CONSULT:\s*ANSWER\s+(.+)", text):
        choice = match_option(m.group(1).strip(), esc.get("options") or []) or m.group(1).strip()
        u.update({"last_answer": choice, "last_answer_text": choice})
        ledger(state, u, f"consult: {esc.get('kind')}: {choice} (navigator)")
        u["_next"] = "apply_answer"
        return u
    reason = "deferred" if "CONSULT: DEFER" in text else "no parseable consult verdict (timeout/fault)"
    ledger(state, u, f"consult: {esc.get('kind')}: {reason} -> owner")
    u["_next"] = "ask_dispatch"
    return u


def _terminal_prompt(esc):
    return "ask_review_incomplete" if esc.get("kind") == "review_incomplete" else "ask_user"


def _slack_ping(state, text):
    if not (OWNER_REMOTE_INPUT and truthy(state.get("top_level"))):
        return
    binary = os.environ.get("ARCHITECT_SLACK_PING_BIN", "slack_ping")
    try:
        subprocess.run([binary, "--text", text], capture_output=True, text=True, timeout=60)
    except Exception:  # noqa: BLE001
        pass


def ask_dispatch(state):
    u = {}
    esc = as_dict(state.get("escalation"))
    u["parked"] = esc.get("kind", "")
    snapshot(state, u, "parked")
    _slack_ping(state, f"architect run paused: {esc.get('kind')} - {tail(esc.get('question', ''), 120)} (run {state.get('run_id')})")
    if OWNER_REMOTE_INPUT and truthy(state.get("top_level")) and truthy(state.get("owner_away")):
        u["owner_polls"] = 0
        u["_next"] = "ask_owner_poll"
    else:
        u["_next"] = _terminal_prompt(esc)
    return u


def ask_owner_poll(state):
    u = {}
    esc = as_dict(state.get("escalation"))
    polls = as_int(state.get("owner_polls")) + 1
    u["owner_polls"] = polls
    opts = " | ".join(f"{i}) {o.get('text', '')}" for i, o in enumerate(esc.get("options") or [], 1))
    binary = os.environ.get("ARCHITECT_ASK_OWNER_BIN", "ask_owner")
    cmd = [binary, "--text", esc.get("question", ""), "--id", f"{state.get('run_id', 'run')}-{esc.get('kind', 'q')}",
           "--wait-minutes", str(OWNER_WAIT_MINUTES), "--poll-minutes", str(OWNER_POLL_MINUTES)]
    if opts:
        cmd += ["--options", opts]
    try:
        p = subprocess.run(cmd, capture_output=True, text=True, timeout=OWNER_POLL_MINUTES * 60 + 120)
        out = (p.stdout or "") + (p.stderr or "")
    except FileNotFoundError:
        out = "NOT_DELIVERED: ask_owner tool unavailable"
    except subprocess.TimeoutExpired:
        out = "TIMEOUT: ask_owner poll exceeded its budget"
    u["owner_poll_result"] = tail(out, 2000)
    u["_next"] = "ask_owner_route"
    return u


def ask_owner_route(state):
    u = {}
    esc = as_dict(state.get("escalation"))
    out = state.get("owner_poll_result") or ""
    if m := re.search(r"ANSWERED[:\s]+(.+)", out):
        u["last_answer_text"] = m.group(1).strip()
        ledger(state, u, f"owner: {esc.get('kind')}: answered via Slack")
        u["_next"] = "parse_answer"
    elif "PENDING" in out and as_int(state.get("owner_polls")) < OWNER_POLL_MAX:
        u["_next"] = "ask_owner_poll"
    else:
        ledger(state, u, f"owner: {esc.get('kind')}: {out[:60].strip() or 'no reply'} -> terminal prompt")
        u["_next"] = _terminal_prompt(esc)
    return u


def parse_answer(state):
    u = {}
    esc = as_dict(state.get("escalation"))
    text = (state.get("last_answer_text") or state.get("last_answer") or "").strip()
    options = esc.get("options") or []
    free_text_kinds = ("design_round", "leak_question", "checklist_accept", "need_design_doc")
    choice = text if esc.get("kind") in free_text_kinds else match_option(text, options)
    if choice is None:
        rounds = as_int(state.get("reask_rounds"))
        if rounds < STRIKE_LIMIT:
            u["reask_rounds"] = rounds + 1
            u["ask_question"] = (f"Could not match '{text}' to an option (reply with a number or the option text; "
                                 f"free text stands after {STRIKE_LIMIT} tries).\n\n" + state.get("ask_question", ""))
            u["_next"] = _terminal_prompt(esc)
            return u
        choice = text
    u.update({"last_answer": choice, "last_answer_text": text, "parked": ""})
    ledger(state, u, f"decision: {esc.get('kind')}: {tail(choice, 300)}")
    u["_next"] = "apply_answer"
    return u


def _answered(state, choices, default=None):
    ans = (state.get("last_answer") or "").strip().lower()
    for key, words in choices.items():
        if any(ans.startswith(w) or w in ans for w in words):
            return key
    return default


def apply_answer(state):
    u = {}
    esc = as_dict(state.get("escalation"))
    kind = esc.get("kind", "")
    answer = state.get("last_answer") or state.get("last_answer_text") or ""
    u["parked"] = ""
    tasks = as_list(state.get("tasks"))
    task = task_by_id(tasks, state.get("current_task")) or {}
    abort = _answered(state, {"abort": ["abort"]}, None) == "abort"
    stop = _answered(state, {"stop": ["stop and report", "stop;", "stop"]}, None) == "stop"

    if kind in ("mode_invalid", "need_design_doc", "clarify", "plan_unauthored", "promote_failed", "git_setup_failed",
                "gate_fault") and abort:
        u["failure_reason"] = f"aborted at the user's request ({kind}): {answer}"
        u["_next"] = "end_aborted"
    elif kind == "mode_invalid":
        for k in MODE_ENUMS:
            if m := re.search(rf"{k}\s*=\s*([\w-]+)", answer):
                u[k] = m.group(1)
        u["_next"] = "orient_read" if state.get("design_doc_path") else "intake"
    elif kind == "need_design_doc":
        p = os.path.expanduser(answer.strip().split()[0]) if answer.strip() else ""
        if p and os.path.isfile(p):
            u["design_doc_path"] = os.path.abspath(p)
            u["_next"] = "orient_read"
        else:
            u["failure_reason"] = f"no readable design doc at {p!r}"
            u["_next"] = "end_blocked"
    elif kind == "clarify":
        p = os.path.expanduser(answer.strip().split()[0]) if answer.strip() else ""
        if p and os.path.isfile(p):
            u["design_doc_path"] = os.path.abspath(p)
        u["_next"] = "orient_read"
    elif kind == "design_round":
        qs = as_list(as_dict(esc.get("data")).get("questions"))
        u["pending_amendments"] = [f"Design round answers (apply each to the plan, resolving its Open question):\nQuestions:\n"
                                   + "\n".join(f"{i}. {q.get('question')}" for i, q in enumerate(qs, 1)) + f"\nAnswers:\n{answer}"]
        u["open_questions"] = []
        u["_next"] = "amend_plan"
    elif kind == "quality_bar":
        d = as_dict(esc.get("data"))
        rigor = (re.search(r"rigor\s*=\s*([\w-]+)", answer) or [None, d.get("rigor", "production")])[1]
        surfaces = (re.search(r"surfaces\s*=\s*([^;\n]+)", answer) or [None, d.get("surfaces", "")])[1]
        if "(as proposed)" in str(surfaces):
            surfaces = d.get("surfaces", "")
        u.update({"quality_rigor": rigor.strip(), "quality_surfaces": str(surfaces).strip()})
        u["_next"] = "record_quality_bar"
    elif kind == "checklist_accept":
        items = as_list(as_dict(esc.get("data")).get("items"))
        a = answer.strip().lower()
        if a == "none" or not a:
            accepted = []
        elif a == "all":
            accepted = items
        else:
            nums = [int(n) for n in re.findall(r"\d+", a)]
            accepted = [items[n - 1] for n in nums if 1 <= n <= len(items)] or [it for it in items if it.lower() in a]
        if accepted:
            u["pending_amendments"] = ["Accepted best-practice checklist items - write each as a measurable acceptance criterion on the "
                                       "relevant task where possible, otherwise under `## Quality bar -> Long-tail criteria`:\n"
                                       + "\n".join(f"- {x}" for x in accepted)]
            ledger(state, u, f"checklist: {len(accepted)}/{len(items)} items accepted")
            u["_next"] = "amend_plan"
        else:
            ledger(state, u, "checklist: no items accepted")
            u["_next"] = "checklist_gate"
    elif kind == "plan_unauthored":
        u["_next"] = "quality_bar_gate"
    elif kind == "amend_cap":
        if stop:
            u["failure_reason"] = f"stopped: the A1 amendment cap ({AMEND_CYCLES_MAX}) was reached"
            u["_next"] = "end_blocked"
        else:
            u["amend_cycles"] = AMEND_CYCLES_MAX - 1
            u["_next"] = "gate1_build"
    elif kind == "gate1_stuck":
        if stop or abort:
            u["failure_reason"] = "stopped: Gate 1 LEAKY on the same questions after the A1 retry"
            u["_next"] = "end_blocked"
        else:
            u["pending_amendments"] = [f"Owner's answers to the gatekeeper's open questions:\n{answer}"]
            u["leak_fingerprints"] = []
            u["_next"] = "amend_plan"
    elif kind == "leak_question":
        pend = [f"Gate 1 leak: {a.get('question', '')} -> {a['answer']}" for a in as_list(state.get("leak_pending_answers")) if isinstance(a, dict) and a.get("answer")]
        u["pending_amendments"] = pend + [f"Owner's answers to the unanswerable Gate 1 questions:\n{answer}"]
        u["leak_pending_answers"] = []
        u["_next"] = "amend_plan"
    elif kind == "gate2_stuck":
        if stop or abort:
            u["failure_reason"] = "stopped: the oracle still rejects the plan after 3 rounds"
            u["_next"] = "end_blocked"
        else:
            ledger(state, u, "gate2: owner override - promoting a REJECTed plan (recorded)")
            u["gate2_verdict"] = "OKAY"
            u["_next"] = "reseal_return"
    elif kind == "gate_fault":
        gate = as_dict(esc.get("data")).get("gate", "gate1")
        if as_int(state.get("gate_fault_retries")) < RERUN_ONCE or "retry" in answer.lower():
            u["gate_fault_retries"] = as_int(state.get("gate_fault_retries")) + 1
            u["_next"] = f"{gate}_build"
        else:
            u["failure_reason"] = f"{gate} kept faulting"
            u["_next"] = "end_failure"
    elif kind == "approve_breakdown":
        a = _answered(state, {"approve": ["approve"], "revise": ["revise"], "abort": ["abort"]}, "approve")
        if a == "approve":
            u["_next"] = "promote_tasks"
        elif a == "revise":
            u["pending_amendments"] = [f"Owner's revision at plan-gate:\n{answer}"]
            u["_next"] = "amend_plan"
        else:
            u["failure_reason"] = "aborted at plan-gate"
            u["_next"] = "end_aborted"
    elif kind == "promote_failed":
        if "retry" in answer.lower() or "continue" in answer.lower():
            if "retry the close" in answer.lower():
                u["_next"] = "task_done_verify"
            elif "retry" in answer.lower():
                u["promote_retries"] = 0
                u["_next"] = "promote_verify"
            else:
                u["promote_retries"] = RERUN_ONCE + 1
                u["skip_promote_defects"] = True
                u["_next"] = "branch_setup"
        else:
            u["failure_reason"] = "aborted after a promotion failure"
            u["_next"] = "end_aborted"
    elif kind == "git_setup_failed":
        u["_next"] = "branch_setup"
    elif kind == "tasks_stuck":
        a = _answered(state, {"retry": ["retry"], "skip": ["skip"], "stop": ["stop"]}, "stop")
        if a == "retry":
            for t in tasks:
                if t.get("status") in ("failed", "parked"):
                    t["status"] = "pending"
            save_tasks(state, u, tasks)
            u["_next"] = "select_tasks"
        elif a == "skip":
            fus = list(as_list(state.get("follow_ups")))
            for t in tasks:
                if t.get("status") in ("pending", "", "parked", "failed"):
                    fus.append(f"({t['id']}) skipped at the user's say-so: blocked on a failed/parked task")
                    t["status"] = "skipped"
            u["follow_ups"] = fus
            save_tasks(state, u, tasks)
            u["closeout_scope"] = "run"
            u["_next"] = "phase_f_gate"
        else:
            u["failure_reason"] = "stopped: remaining tasks are blocked"
            u["_next"] = "end_blocked"
    elif kind == "task_failed":
        a = _answered(state, {"retry": ["retry"], "rescope": ["re-scope", "rescope", "amend"], "abort": ["abort"]}, "abort")
        rounds = dict(as_dict(state.get("task_rounds")))
        tr = rounds.setdefault(task.get("id", ""), {})
        if a == "retry" and as_int(tr.get("retry_rounds")) < RERUN_ONCE:
            tr["retry_rounds"] = as_int(tr.get("retry_rounds")) + 1
            u["task_rounds"] = rounds
            u.update({"delegation_kind": "fix", "fix_source": "oracle", "fix_return_to": "verify",
                      "fix_items": ["Apply the oracle's direction (below) and complete the task"],
                      "fix_note": "Oracle diagnosis:\n" + tail(state.get("oracle_result", ""), 4000)})
            u["_next"] = "compose_delegation"
        elif a == "rescope":
            u["pending_amendments"] = [f"Re-scope of {task.get('id')} after a failed implementation:\n{answer}"]
            task["status"] = "pending"
            save_tasks(state, u, tasks)
            u["_next"] = "amend_plan"
        else:
            task["status"] = "failed"
            save_tasks(state, u, tasks)
            u["failure_reason"] = f"{task.get('id')} failed after Sisyphus recovery; aborted at the user's request"
            u["_next"] = "end_aborted"
    elif kind == "conformance_stuck":
        a = _answered(state, {"stop": ["stop"], "round": ["one more", "round"], "accept": ["accept"]}, "stop")
        if a == "round":
            u.update({"delegation_kind": "fix", "fix_source": "review", "fix_return_to": "verify",
                      "fix_items": [l[2:] for l in esc.get("evidence", "").splitlines() if l.startswith("- ")],
                      "fix_note": "One more fix round at the user's request."})
            u["_next"] = "compose_delegation"
        elif a == "accept":
            u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [f"({task.get('id')}) conformance findings accepted unresolved at the user's say-so"]
            u["_next"] = "post_round"
        else:
            u["failure_reason"] = f"stopped: {task.get('id')} fails conformance after the re-run"
            u["_next"] = "end_blocked"
    elif kind == "plan_defect":
        if stop:
            u["failure_reason"] = f"stopped: the plan is the root cause of a conformance failure on {task.get('id')}"
            u["_next"] = "end_blocked"
        else:
            u["pending_amendments"] = [f"Plan defect found by the adversary on {task.get('id')} (criterion amended, never the scope):\n{answer}\n\nEvidence:\n{esc.get('evidence', '')}"]
            u["after_reseal"] = "verify"
            u["_next"] = "amend_plan"
    elif kind == "probe_gap":
        if _answered(state, {"supply": ["supply"]}, None) == "supply":
            u["pending_amendments"] = [f"Local-run recipe supplied by the owner for the probe:\n{answer}"]
            u["after_reseal"] = "verify"
            u["_next"] = "amend_plan"
        else:
            u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [f"({task.get('id')}) consumer-facing change could not be exercised locally (probe INCONCLUSIVE): provide the local-run recipe and re-probe"]
            u["_next"] = "post_round" if state.get("verify_scope") != "batch" else "batch_close"
    elif kind == "review_incomplete":
        a = _answered(state, {"retry": ["retry"], "accept": ["accept"], "abort": ["abort"]}, "retry")
        if a == "retry":
            u["_next"] = "verify_gate"
        elif a == "accept":
            rl = as_list(as_dict(state.get("round_log")).get(state.get("verify_unit") or task.get("id", ""), []))
            lanes = as_list((rl[-1] if rl else {}).get("incomplete")) or ["pipeline"]
            u["skipped_reviews"] = list(as_list(state.get("skipped_reviews"))) + [f"{state.get('verify_unit') or task.get('id')}: {', '.join(lanes)} (NEEDS-HUMAN accepted)"]
            log_event(state, task, f"skipped review (NEEDS-HUMAN accepted): {', '.join(lanes)}") if task else None
            u["_next"] = "post_round" if state.get("verify_scope") != "batch" else "batch_close"
        else:
            log_event(state, task, "aborted at the user's request: the review could not be completed") if task else None
            u["failure_reason"] = f"aborted: the review of {task.get('id')} could not be completed"
            u["_next"] = "end_aborted"
    elif kind == "design_ask":
        q = esc.get("question", "").replace("Design question: ", "", 1)
        u["design_decisions"] = list(as_list(state.get("design_decisions"))) + [f"{q} -> {answer}"]
        lines = list(as_list(state.get("ledger_lines"))) + [f"design-decision: {q} -> {answer}"]
        u["ledger_lines"] = lines
        ledger(state, u, f"design-decision: {q} -> {answer}")
        rounds = dict(as_dict(state.get("task_rounds")))
        tr = rounds.setdefault(task.get("id", ""), {})
        adopting = (state.get("last_answer") or "").strip().lower().startswith("adopt")
        if adopting and as_int(tr.get("design_ask_rounds")) < RERUN_ONCE:
            tr["design_ask_rounds"] = as_int(tr.get("design_ask_rounds")) + 1
            u["task_rounds"] = rounds
            fixes = list(as_list(state.get("pending_fixes")))
            fixes.insert(0, {"task": task.get("id"), "items": [f"Adopt the pattern the user chose: {answer}"], "source": "design_ask",
                             "return_to": "select_tasks", "note": f"Design question answered: {q} -> {answer}"})
            u["pending_fixes"] = fixes
        u["_next"] = "design_ask_gate"
    elif kind == "batch_strikeout":
        a = _answered(state, {"stop": ["stop"], "round": ["one more", "round"], "accept": ["accept"]}, "stop")
        batch = state.get("current_batch") or ""
        bs = dict(as_dict(state.get("batch_state")))
        if a == "round":
            bs.setdefault(batch, {})["strikes"] = STRIKE_LIMIT - 1
            u["batch_state"] = bs
            u["_next"] = "batch_fix_route"
        elif a == "accept":
            bs.setdefault(batch, {})["passed"] = True
            u["batch_state"] = bs
            u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [f"(batch {batch}) gauntlet findings accepted unresolved at the user's say-so"]
            u["_next"] = "select_tasks"
        else:
            u["failure_reason"] = f"stopped: batch {batch} struck out"
            u["_next"] = "end_blocked"
    elif kind == "wave_escalation":
        a = _answered(state, {"retry": ["retry"], "accept": ["accept"], "abort": ["abort"]}, "retry")
        if a == "retry":
            task["status"] = "in-progress"
            save_tasks(state, u, tasks)
            u.update({"delegation_kind": "fix", "fix_source": "wave", "fix_return_to": "verify",
                      "fix_items": [l[2:] for l in esc.get("evidence", "").splitlines() if l.startswith("- ")] or ["Address the evidence below"],
                      "fix_note": tail(esc.get("evidence", ""), 3000), "delegate_project_dir": task.get("worktree") or state.get("code_repo_path", "")})
            u["_next"] = "compose_delegation"
        elif a == "accept":
            task["status"] = "in-progress"
            save_tasks(state, u, tasks)
            u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [f"({task.get('id')}) parallel task accepted as-is at the user's say-so"]
            u["_next"] = "integration_test" if (is_single(state) or MULTI_IS_GROUPS) else "harvest_report"
        else:
            task["status"] = "failed"
            save_tasks(state, u, tasks)
            u["_next"] = "wave_cleanup"
    elif kind in ("integration_conflict", "integration_red"):
        a = _answered(state, {"stop": ["stop"], "round": ["one more", "cycle", "round"], "abort": ["abort"]}, "stop")
        rounds = dict(as_dict(state.get("task_rounds")))
        tr = rounds.setdefault(task.get("id", ""), {})
        if a == "round":
            key = "conflict_cycles" if kind == "integration_conflict" else "integration_strikes"
            tr[key] = as_int(tr.get(key)) - 1
            u["task_rounds"] = rounds
            u["_next"] = "integration_test" if state.get("integration_task") else "integration_check"
        elif a == "abort":
            task["status"] = "failed"
            save_tasks(state, u, tasks)
            u["_next"] = "wave_cleanup"
        else:
            u["failure_reason"] = f"stopped: {kind} on {task.get('id')}"
            u["_next"] = "end_blocked"
    elif kind == "approve_next":
        a = _answered(state, {"continue": ["continue"], "park": ["stop here", "park"], "abort": ["abort"]}, "continue")
        if a == "continue":
            u["_next"] = "select_tasks"
        elif a == "park":
            u["parked"] = "approve_next"
            u["failure_reason"] = "parked at the user's request (phase-gate); re-enter with the run id to continue"
            u["_next"] = "end_blocked"
        else:
            u["failure_reason"] = "aborted at phase-gate"
            u["_next"] = "end_aborted"
    elif kind == "hygiene_base_missing":
        if abort or stop:
            u["failure_reason"] = "stopped: the hygiene base could not be resolved"
            u["_next"] = "end_blocked"
        else:
            u["follow_ups"] = list(as_list(state.get("follow_ups"))) + ["Run-closing hygiene pass skipped for one unit: merge-base unresolvable"]
            u["_next"] = "hygiene_create"
    elif kind == "scope_surprise":
        u["failure_reason"] = f"stopped: the design requires writing outside the code repo: {answer}"
        u["_next"] = "end_blocked"
    elif kind in ("ci_unsure", "ci_exhausted", "ci_infra"):
        a = _answered(state, {"rerun": ["re-run", "rerun", "keep waiting", "wait"], "fix": ["resume", "coder", "fix"], "leave": ["leave", "draft", "report"]}, "leave")
        if a == "rerun":
            u.update({"ci_polls": 0, "ci_poll_fails": 0, "ci_rounds": 0, "ci_started_at": int(time.time())})
            u["_next"] = "ci_poll"
        elif a == "fix":
            u["fix_request"] = {"task": state.get("ci_owning_task") or (tasks[-1]["id"] if tasks else ""), "source": "ci", "return_to": "ci",
                                "items": [f"CI failure (user-directed fix round): {tail(state.get('ci_diagnosis', ''), 300)}"], "note": tail(state.get("ci_evidence", ""), 4000)}
            u["_next"] = "fix_round"
        else:
            u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [f"PR #{state.get('pr_number')} left in draft with CI {state.get('ci_status') or 'unresolved'}: {tail(state.get('ci_diagnosis', ''), 300)}"]
            u["_next"] = "finish_plan"
    elif kind == "pr_body_notes":
        u["_next"] = "open_pr"
    else:
        ledger(state, u, f"apply_answer: unknown escalation kind {kind!r}; stopping safely")
        u["failure_reason"] = f"unknown escalation kind {kind!r}"
        u["_next"] = "end_failure"
    snapshot(state, u, f"answered {kind}")
    return u


# ====================================================================== 2.9 finish
def _hygiene_units(state, tasks):
    if is_single(state) or MULTI_IS_GROUPS:
        return [{"id": state.get("closeout_unit") or "run", "members": [t["id"] for t in tasks if t.get("status") == "complete" and t.get("type") != "hygiene"
                                                                        and (is_single(state) or t.get("unit") == (state.get("closeout_unit") or t.get("unit")))]}]
    units = []
    for chain in _chains([t for t in tasks if t.get("type") != "hygiene"]):
        members = _order_chain(tasks, chain)
        if len([t for t in members if t.get("pr_number")]) >= 2:
            units.append({"id": "chain-" + members[0]["id"], "members": [t["id"] for t in members], "top_branch": members[-1].get("branch", "")})
    return units


def phase_f_gate(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    if is_micro(state):
        open_b = [b for b, v in as_dict(state.get("batch_state")).items() if not truthy(v.get("passed"))]
        pending_batches = sorted({t.get("batch") for t in tasks if t.get("batch") and t.get("status") == "complete"} - set(as_dict(state.get("batch_state")).keys()))
        if open_b or pending_batches:
            b = (open_b or pending_batches)[0]
            u["current_batch"] = b
            first = min([t for t in tasks if t.get("batch") == b], key=lambda t: int(re.search(r"\d+", t["id"]).group()))
            u["batch_base_sha"] = as_dict(state.get("batch_state")).get(b, {}).get("base_sha") or first.get("base_sha", "")
            ledger(state, u, f"phase_f: batch {b} never PASSed -> its gauntlet runs first (anti-pattern guard)")
            u["_next"] = "batch_gauntlet_build"
            return u
    u["closeout_scope"] = state.get("closeout_scope") or "run"
    units = [x for x in _hygiene_units(state, tasks) if x["id"] not in as_list(state.get("hygiene_done"))]
    u["hygiene_units"] = units
    code = state.get("code_repo_path", "")
    stale = [l for l in git_out(code, "worktree", "list", default="").splitlines() if ".worktrees/" in l]
    if stale:
        ledger(state, u, f"phase_f: {len(stale)} stale worktree(s) remain: {stale}")
    if (is_single(state) or MULTI_IS_GROUPS) and not truthy(state.get("integration_checked")):
        u["_next"] = "integration_check"
    else:
        u["_next"] = "integrate_findings"
    snapshot(state, u, "phase_f")
    return u


def integration_check(state):
    u = {}
    code = state.get("code_repo_path", "")
    tasks = as_list(state.get("tasks"))
    ok, out = _run_verification(state, code)
    if ok:
        u["integration_checked"] = True
        ledger(state, u, "phase_f: full suite green on the run branch")
        u["_next"] = "integrate_findings"
        return u
    strikes = as_int(state.get("final_integration_strikes")) + 1
    u["final_integration_strikes"] = strikes
    ledger(state, u, f"phase_f: full suite RED on the run branch (strike {strikes}/{STRIKE_LIMIT})")
    if strikes > STRIKE_LIMIT:
        return raise_escalation(state, u, escalation(
            "integration_red", f"The run branch suite is still RED after {STRIKE_LIMIT} fix rounds at Phase F. How to proceed?",
            [option("Stop and report", recommended=True), option("One more fix round anyway")], evidence=tail(out, 2500), hard_trigger=True))
    owner = state.get("ci_owning_task") or (tasks[-1]["id"] if tasks else "")
    u["fix_request"] = {"task": owner, "source": "integration", "return_to": "integration_check",
                        "items": ["Phase F integration: the full build + test suite is RED on the run branch (integrated result of all tasks)"],
                        "note": tail(out, 4000)}
    u["_next"] = "fix_round"
    return u


def integrate_route(state):
    u = {}
    harvest_anomalies(state, u, "integrate_findings")
    note_script_error(state, u, "integrate_findings")
    fus = list(as_list(state.get("follow_ups")))
    for f in as_list(state.get("integration_follow_ups")):
        if isinstance(f, str) and f.strip() and f not in fus:
            fus.append(f"(integration) {f}")
    u["follow_ups"] = fus
    amends = [a for a in as_list(state.get("integration_amendments")) if isinstance(a, str) and a.strip()]
    if amends and not truthy(state.get("integration_amended")):
        u["integration_amended"] = True
        u["pending_amendments"] = [f"Integration finding: {a}" for a in amends]
        u["after_reseal"] = "phase_f"
        ledger(state, u, f"integrate: {len(amends)} plan amendment(s) -> amend_plan (re-seal)")
        u["_next"] = "amend_plan"
        return u
    u["_next"] = "hygiene_create"
    return u


HYGIENE_CRITERIA = (
    "These two are the ONLY criteria:\n"
    "- Behaviour unchanged: the full build and test suite is green and no production code path changed other than removal of "
    "unreachable code and lapsed scaffolding; a `debris-exempt` token added to a justified line is not a code path change\n"
    "- Debris gone: every `debris-exempt` token in the {scope} diff sits on a line named in the HYGIENE list pasted below, every other "
    "HYGIENE row cites its justification, and the {scope}-wide lint over the {SCOPE} base exits 0\n"
)


def _hygiene_brief(state, base, scope_word, ledger_lines, lint_cmd, pr_line):
    code = state.get("code_repo_path", "")
    return (
        f"Sweep the {scope_word.upper()}'s whole diff -- `git -C {code} diff --no-color --no-ext-diff {base}..HEAD` -- for: the debt "
        "markers named in the code-review skill (every owning task is complete by the time this pass runs, so a surviving marker, owner "
        "named or not, is REMOVED; what cannot be removed safely is reported as a follow-up instead); premise phrases (\"ahead of\", "
        "\"until TASK\", \"temporary\", \"will be used by\", \"for a later\", \"placeholder\", \"stub\", \"once it lands\", \"waits for\"); "
        "test files that duplicate a CI lane or only run opt-in; files added for the implementer's own experiments; test-only targets "
        "under `examples/`; decisions or check-ledgers written into CONTRIBUTING/README; comments or docs with non-ASCII punctuation or "
        f"planning-artefact vocabulary, found by running the shared debris lint: `{lint_cmd}` (exit 1 on hits, each printed as "
        "file:line, at most 25 per run so iterate); identifiers that only make sense with the plan open; every `debris-exempt` token "
        f"already in the {scope_word} diff (`git -C {code} diff --no-color --no-ext-diff {base}..HEAD | grep -n 'debris-exempt'`): each one "
        "needs a `HYGIENE:` entry with a citing justification -- the starting ledger below supplies it for exemptions earlier tasks "
        "justified -- or the token comes off and the line is fixed.\n"
        "For each hit: REMOVE it, or JUSTIFY it -- a justification must cite a repo convention at file:line or a recorded `## Quality bar` "
        "drop -- and report every kept item under a `HYGIENE:` heading (file:line, what, justification). A lint hit may be kept ONLY for "
        "the reasons the code-review skill sanctions (the glyph is the point, or the line must name the needles); such a line is marked "
        f"with the lint's `debris-exempt` token. Then run the full suite{pr_line}.\n"
        "Carve-outs: you MAY touch any task's files; you MAY delete opt-in-only or CI-duplicate tests and lapsed scaffolding, NEVER a "
        "failing test to make it pass; you still do NOT edit the plan or task files -- report instead.\n\n"
        f"Starting ledger (exemptions earlier tasks justified; verbatim):\n{ledger_lines or '(none)'}\n"
    )


def hygiene_create(state):
    u = {}
    tasks = as_list(state.get("tasks"))
    units = as_list(state.get("hygiene_units"))
    if not units or truthy(state.get("hygiene_skipped")):
        ledger(state, u, "hygiene: no unit needs a run-closing pass" + (" (standalone PRs)" if not (is_single(state) or MULTI_IS_GROUPS) else ""))
        u["_next"] = "pr_summary" if (is_single(state) or MULTI_IS_GROUPS) else "stack_gate_final"
        return u
    unit = units[0]
    code = state.get("code_repo_path", "")
    base_branch = state.get("base_branch_name") or "main"
    git(code, "fetch", "--quiet", "origin")
    if is_single(state) or MULTI_IS_GROUPS:
        base = state.get("unit_base_sha") or git_out(code, "merge-base", f"origin/{base_branch}", "HEAD", default="")
        scope_word, pr_line = "run", " and push the run branch -- NO separate hygiene PR: the hygiene commits ride the run branch into the single PR"
        top = state.get("run_branch") or _run_branch(state)
    else:
        top = unit.get("top_branch", "")
        base = git_out(code, "merge-base", f"origin/{base_branch}", f"origin/{top}", default="")
        scope_word, pr_line = "chain", f", and open the hygiene PR as a DRAFT with base `{top}`"
    if not base:
        return raise_escalation(state, u, escalation(
            "hygiene_base_missing", f"The {scope_word} base (merge-base of origin/{base_branch} and {top}) is empty - the lint would fall back to the "
                                    "worktree and report a vacuous clean. Skip the hygiene pass (recorded), or stop?",
            [option("Stop and report", recommended=True), option("Skip the hygiene pass (recorded as a follow-up)")], hard_trigger=True))
    ok, ledger_out = core_tool("collect_ledger_lines", tag="debris-exempt", dir=state.get("tasks_root", ""))
    ledger_lines = ledger_out.strip() if ok else "(collect_ledger_lines unavailable: " + tail(ledger_out, 120) + ")"
    agents_dir = os.path.join(os.environ.get("LLM_ROOT_DIR", os.path.expanduser("~/.config/coyote")), "agents")
    lint_cmd = f"python3 {agents_dir}/.shared/debris.py {code} {base}"
    slug = "run-hygiene" if (is_single(state) or MULTI_IS_GROUPS) else f"run-hygiene-{unit['id']}".lower()
    body = ("## What\n\nRun-closing hygiene pass over the " + scope_word + " diff " + base + "..HEAD.\n\n## Acceptance criteria\n\n"
            + HYGIENE_CRITERIA.format(scope=scope_word, SCOPE=scope_word.upper()))
    tid, tdir = new_task(state, slug, f"Run-closing hygiene pass ({unit['id']})", body, blocked_by=tuple(unit.get("members", [])),
                         task_type="chore", plan_name=os.path.basename(state.get("plan_path", "")))
    if not tid:
        ledger(state, u, f"hygiene: could not create the chore task: {tdir}; pass skipped (follow-up)")
        u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [f"Run-closing hygiene pass for {unit['id']} not run: task creation failed"]
        u["hygiene_done"] = list(as_list(state.get("hygiene_done"))) + [unit["id"]]
        u["_next"] = "hygiene_create"
        return u
    task = {"id": tid, "slug": os.path.basename(tdir), "dir": tdir, "title": f"Run-closing hygiene pass ({unit['id']})", "status": "pending",
            "blocked_by": list(unit.get("members", [])), "batch": "", "protected": False, "surfaces": "", "type": "hygiene", "files": [],
            "group": "", "unit": unit["id"], "pr_base": top if not (is_single(state) or MULTI_IS_GROUPS) else (state.get("run_branch") or _run_branch(state))}
    tasks.append(task)
    _claim(state, u, task, tasks)
    if not (is_single(state) or MULTI_IS_GROUPS):
        task["branch"] = f"feat/{task['slug'].lower()}"
        task["pr_base"] = top
    save_tasks(state, u, tasks)
    u.update({
        "current_task": tid, "delegation_kind": "hygiene", "fix_items": [], "hygiene_base": base, "hygiene_lint_cmd": lint_cmd,
        "hygiene_criteria": HYGIENE_CRITERIA.format(scope=scope_word, SCOPE=scope_word.upper()) + "\nHYGIENE list (context, not criteria):\n(pasted after the pass)",
        "hygiene_brief": _hygiene_brief(state, base, scope_word, ledger_lines, lint_cmd, pr_line),
        "hygiene_done": list(as_list(state.get("hygiene_done"))) + [unit["id"]], "hygiene_units": units[1:],
    })
    ledger(state, u, f"hygiene: {tid} created for {unit['id']} (base {base}); lint `{lint_cmd}`")
    snapshot(state, u, f"phase_f hygiene {tid}")
    u["_next"] = "compose_delegation"
    return u


def stack_gate_final(state):
    """Per-task-stack multi Phase F tail: link every complete chain (hygiene PR on top), then the report."""
    u = stack_gate({**state, "closeout_scope": "run"})
    u["_next"] = "flip_ready"
    return u


def pr_body(state):
    u = {}
    harvest_anomalies(state, u, "pr_summary")
    note_script_error(state, u, "pr_summary")
    run_dir = state.get("run_dir", "")
    summary = state.get("pr_summary_text") or ""
    if not summary.strip():
        plan = read_file(state.get("plan_path", ""))
        _, body = frontmatter(plan)
        summary = (plan_section(body, r"Problem") + "\n\n" + plan_section(body, r"Approach")).strip() or "(summary unavailable)"
        ledger(state, u, "pr: summary node unavailable; Problem/Approach pasted verbatim (declared)")
    write_artifact(run_dir, "pr-summary.md", summary)
    hyg = as_list(state.get("hygiene_items"))
    if hyg:
        write_artifact(run_dir, "pr-hygiene.md", "\n".join(f"- {h}" for h in hyg))
    tasks_dir = os.path.join(state.get("tasks_root", ""), "completed") if TASKSTORE == "dirstate" else state.get("tasks_root", "")
    ok, out = core_tool("assemble_pr_body", plan=state.get("plan_path", ""), tasks_dir=tasks_dir,
                        summary_file=os.path.join(run_dir, "pr-summary.md"), out=os.path.join(run_dir, "pr-body.md"),
                        hygiene_file=os.path.join(run_dir, "pr-hygiene.md") if hyg else None,
                        quality_note=state.get("quality_note") or None, followups=(TASKSTORE != "dirstate"))
    notes = [l for l in (out or "").splitlines() if "NOTE" in l]
    if not ok:
        ledger(state, u, f"pr: assemble_pr_body failed ({tail(out, 200)}); body = summary + task checklist assembled locally")
        tasks = [t for t in as_list(state.get("tasks")) if t.get("status") == "complete"]
        body = summary + "\n\n## Tasks\n" + "\n".join(f"- [x] {t['id']} - {t.get('title', '')} ({t.get('head_sha') or 'see log'})" for t in tasks)
        body += "\n\n## Hygiene pass\n" + ("\n".join(f"- {h}" for h in hyg) if hyg else "clean") + "\n"
        write_artifact(run_dir, "pr-body.md", body)
    u["pr_body_path"] = os.path.join(run_dir, "pr-body.md")
    u["pr_body_notes"] = "\n".join(notes)
    if notes and as_int(state.get("body_fix_rounds")) < RERUN_ONCE:
        u["body_fix_rounds"] = as_int(state.get("body_fix_rounds")) + 1
        ledger(state, u, f"pr: {len(notes)} NOTE line(s) -> one body fix round")
        u["_next"] = "pr_body_fix"
    else:
        u["_next"] = "open_pr"
    return u


def open_pr(state):
    u = {}
    harvest_anomaly_lines(state, u, state.get("pr_body_fix_report") or "", "pr_body_fix")
    note_script_error(state, u, "pr_body_fix")
    code = state.get("code_repo_path", "")
    branch = state.get("run_branch") or _run_branch(state)
    base = state.get("unit_base_branch") or state.get("base_branch_name") or "main"
    title = f"PLAN-{state.get('plan_slug')}: {state.get('design_title') or state.get('plan_slug')}"
    if MULTI_IS_GROUPS and not is_single(state):
        gi = as_int(state.get("group_index")) or 1
        total = len({t.get("unit") for t in as_list(state.get("tasks"))}) or 1
        title = f"PLAN-{state.get('plan_slug')} [g{gi}/{total}]: {state.get('closeout_unit') or ''}"
    if state.get("pr_number"):
        u["_next"] = "pr_round_comments"
        return u
    cmd = os.environ.get("ARCHITECT_GH_CREATE_CMD")
    if cmd:
        rc, out = sh(["bash", "-c", cmd], timeout=120)
    else:
        rc, out = sh(["gh", "pr", "create", "--draft", "--base", base, "--head", branch, "--title", title, "--body-file", state.get("pr_body_path", "")], cwd=code, timeout=180)
    m = PR_URL_RE.search(out or "")
    if rc != 0 or not m:
        ledger(state, u, f"pr: gh pr create failed: {tail(out, 300)}")
        u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [f"Open the draft PR by hand from {branch} -> {base}: gh pr create failed"]
        u["_next"] = "finish_plan"
        return u
    u.update({"pr_number": m.group(2), "pr_url": m.group(0), "ci_polls": 0, "ci_poll_fails": 0, "ci_rounds": 0, "ci_started_at": int(time.time())})
    prs = dict(as_dict(state.get("pr_numbers")))
    prs[state.get("closeout_unit") or "run"] = m.group(2)
    u["pr_numbers"] = prs
    ledger(state, u, f"pr: draft PR #{m.group(2)} opened from {branch} -> {base}: {m.group(0)}")
    snapshot(state, u, "pr_opened")
    u["_next"] = "pr_round_comments"
    return u


def pr_round_comments(state):
    u = {}
    pending = list(as_list(state.get("pending_pr_comments")))
    pr = state.get("pr_number")
    posted = 0
    for c in pending:
        if isinstance(c, dict) and pr:
            ok, _ = _pr_comment(state, pr, c.get("body", ""))
            posted += 1 if ok else 0
    u["pending_pr_comments"] = []
    if pending:
        ledger(state, u, f"pr: {posted}/{len(pending)} deferred review-round comment(s) posted on PR #{pr}")
    u["_next"] = "ci_poll" if pr else "finish_plan"
    return u


def ci_poll(state):
    u = {}
    polls = as_int(state.get("ci_polls"))
    rounds = as_int(state.get("ci_rounds"))
    started = as_int(state.get("ci_started_at")) or int(time.time())
    if polls > 0:
        time.sleep(int(os.environ.get("ARCHITECT_CI_SLEEP", CI_POLL_SLEEP)))
    u["ci_polls"] = polls + 1
    u["ci_rounds"] = rounds + 1
    if time.time() - started > CI_WALL_SECS or rounds + 1 > CI_MAX_ROUNDS:
        u["ci_status"] = "timeout"
        u["_next"] = "ci_route"
        return u
    code = state.get("code_repo_path", "")
    cmd = os.environ.get("ARCHITECT_GH_CHECKS_CMD")
    try:
        if cmd:
            p = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=90)
        else:
            p = subprocess.run(["gh", "pr", "checks", str(state.get("pr_number")), "--json", "name,state,bucket,link"], cwd=code,
                               capture_output=True, text=True, timeout=90)
        data = json.loads(p.stdout) if p.returncode == 0 and p.stdout.strip() else None
    except Exception:  # noqa: BLE001
        data = None
    if data is None:
        fails = as_int(state.get("ci_poll_fails")) + 1
        u["ci_poll_fails"] = fails
        u["ci_status"] = "poll_error" if fails >= CI_POLL_FAILS else "pending"
        u["_next"] = "ci_route" if fails >= CI_POLL_FAILS else "ci_poll"
        return u
    u["ci_poll_fails"] = 0
    buckets = [(c.get("name", ""), (c.get("bucket") or c.get("state") or "").lower(), c.get("link", "")) for c in data]
    failing = [(n, l) for n, b, l in buckets if b in ("fail", "failure", "cancelled")]
    pending = [n for n, b, _ in buckets if b in ("pending", "queued", "in_progress", "")]
    u["ci_checks"] = buckets
    if failing:
        u.update({"ci_status": "red", "ci_failing": failing, "_next": "ci_route"})
    elif pending:
        u.update({"ci_status": "pending", "_next": "ci_poll"})
    else:
        u.update({"ci_status": "green", "_next": "ci_route"})
    return u


def ci_route(state):
    u = {}
    note_script_error(state, u, "ci_poll")
    status = state.get("ci_status") or "poll_error"
    if status == "green":
        ledger(state, u, f"ci: PR #{state.get('pr_number')} green")
        u["_next"] = "flip_ready"
    elif status == "red":
        u["_next"] = "ci_evidence"
    else:
        return raise_escalation(state, u, escalation(
            "ci_exhausted", f"The CI watch on PR #{state.get('pr_number')} ended without a verdict ({status}: rounds {state.get('ci_rounds')}, "
                            f"wall {CI_WALL_SECS}s). Keep waiting, or leave the PR in draft and report?",
            [option("Keep waiting (re-run the watch)"), option("Leave the PR in draft and report", recommended=True,
                                                                 evidence="a draft with a named blocker beats a ready PR on red/pending")],
            evidence=tail(state.get("ci_evidence", ""), 1500)))
    return u


INFRA_RE = re.compile(r"runner (was )?lost|lost communication|The operation was canceled|timed out|timeout|HTTP 429|rate limit|"
                      r"concurrency|cancelled by|No space left|ECONNRESET", re.I)
SIGNATURE_RE = re.compile(r"(?im)^(?:.*\b(FAIL|FAILED|panic:|error\[|Error:|AssertionError|exit code \d+|--- FAIL:|not ok)\b.*)$")


def ci_evidence(state):
    """Section 10.12 (1): bounded evidence pack with failure signatures and infra pre-classification."""
    u = {}
    code = state.get("code_repo_path", "")
    pack, sigs, infra = [], [], []
    for name, link in as_list(state.get("ci_failing")):
        pack.append(f"### {name}\n{link}")
        log = ""
        if m := re.search(r"/actions/runs/(\d+)", link or ""):
            cmd = os.environ.get("ARCHITECT_GH_LOG_CMD")
            rc, log = sh(["bash", "-c", cmd], timeout=60) if cmd else sh(["gh", "run", "view", m.group(1), "--log-failed"], cwd=code, timeout=120)
        trimmed = tail(log, 4000)
        pack.append(trimmed or "(log unavailable)")
        sigs += [f"[{name}] {l.strip()[:200]}" for l in log.splitlines() if SIGNATURE_RE.match(l)][:15]
        if INFRA_RE.search(log or "") or INFRA_RE.search(name):
            infra.append(name)
    u["ci_evidence"] = tail("\n".join(pack), 12000)
    u["ci_signatures"] = sigs
    u["ci_infra_hint"] = infra
    u["ci_evidence_trimmed"] = any(len(p) > 4000 for p in pack)
    ledger(state, u, f"ci: evidence pack built ({len(sigs)} signature(s); infra hint {infra or 'none'}; trimmed={u['ci_evidence_trimmed']})")
    return u


def _owning_task(state, diagnosis):
    tasks = [t for t in as_list(state.get("tasks")) if t.get("status") == "complete"]
    named = state.get("ci_owning_task_llm") or ""
    if named and task_by_id(tasks, named):
        return named
    text = (diagnosis or "") + " " + " ".join(as_list(state.get("ci_signatures")))
    for t in tasks:
        if any(f and f in text for f in t.get("files") or []):
            return t["id"]
    return tasks[-1]["id"] if tasks else ""


def ci_apply(state):
    u = {}
    harvest_anomalies(state, u, "ci_diagnose")
    note_script_error(state, u, "ci_diagnose")
    verdict = state.get("ci_verdict") or "unsure"
    pr = str(state.get("pr_number") or "")
    checks = [c for c in as_list(state.get("failing_checks")) if isinstance(c, str)] or [n for n, _ in as_list(state.get("ci_failing"))]
    reruns = dict(as_dict(state.get("ci_reruns")))
    strikes = dict(as_dict(state.get("ci_fix_strikes")))
    pr_reruns = dict(as_dict(reruns.get(pr)))
    pr_strikes = dict(as_dict(strikes.get(pr)))
    if verdict == "flake":
        exhausted = [c for c in checks if as_int(pr_reruns.get(c)) >= CI_FLAKE_RERUNS]
        if exhausted:
            verdict = "real"
            ledger(state, u, f"ci: flake re-run budget exhausted for {exhausted} on PR #{pr}; a third red is real by definition")
        else:
            code = state.get("code_repo_path", "")
            for name, link in as_list(state.get("ci_failing")):
                pr_reruns[name] = as_int(pr_reruns.get(name)) + 1
                if m := re.search(r"/actions/runs/(\d+)", link or ""):
                    if not os.environ.get("ARCHITECT_GH_CHECKS_CMD"):
                        sh(["gh", "run", "rerun", m.group(1), "--failed"], cwd=code, timeout=60)
                ledger(state, u, f"flake-rerun: PR #{pr} {name} (re-run {pr_reruns[name]}/{CI_FLAKE_RERUNS})")
            reruns[pr] = pr_reruns
            u["ci_reruns"] = reruns
            u.update({"ci_polls": 0, "ci_poll_fails": 0})
            u["_next"] = "ci_poll"
            return u
    if verdict == "real":
        owner = _owning_task(state, state.get("ci_diagnosis", ""))
        u["ci_owning_task"] = owner
        over = []
        for c in checks or ["(unnamed check)"]:
            pr_strikes[c] = as_int(pr_strikes.get(c)) + 1
            if pr_strikes[c] > STRIKE_LIMIT:
                over.append(c)
        strikes[pr] = pr_strikes
        u["ci_fix_strikes"] = strikes
        if over:
            return raise_escalation(state, u, escalation(
                "ci_exhausted", f"CI still red on {over} after {STRIKE_LIMIT} fix rounds (per check-name per PR #{pr}). How to proceed?",
                [option("Resume with one more coder fix round"), option("Leave the PR in draft and report", recommended=True)],
                evidence=tail(state.get("ci_evidence", ""), 2000), hard_trigger=True))
        u["fix_request"] = {"task": owner, "source": "ci", "return_to": "ci",
                            "items": [f"CI failure on PR #{pr} ({', '.join(checks)}): " + tail(state.get("ci_diagnosis", ""), 400)],
                            "note": "Evidence pack:\n" + tail(state.get("ci_evidence", ""), 5000)
                                    + "\nPost a NEW top-level PR comment '## Update @ <full head SHA>' mapping the fix to its commit; never rewrite the description."}
        ledger(state, u, f"ci: real failure -> fix round to {owner} (strikes {pr_strikes})")
        u["_next"] = "fix_round"
        return u
    if verdict == "infra":
        return raise_escalation(state, u, escalation(
            "ci_infra", "CI failed for infrastructure reasons (runner lost / timeout / 429 / cancelled). Wait and re-watch, or leave in draft?",
            [option("Keep waiting (re-run the watch)", recommended=True, evidence=tail(state.get("ci_diagnosis", ""), 300)),
             option("Leave the PR in draft and report")]))
    return raise_escalation(state, u, escalation(
        "ci_unsure", "CI diagnosis is unsure (unsure defaults up, never to a re-run). How to proceed?",
        [option("Re-run the failing checks once"), option("Resume with a coder fix round"), option("Leave the PR in draft and report", recommended=True)],
        evidence=tail(state.get("ci_evidence", ""), 2000)))


def flip_ready(state):
    u = {}
    code = state.get("code_repo_path", "")
    tasks = as_list(state.get("tasks"))
    if is_single(state) or MULTI_IS_GROUPS:
        pr = state.get("pr_number")
        if pr:
            cmd = os.environ.get("ARCHITECT_GH_READY_CMD")
            rc, out = sh(["bash", "-c", cmd], timeout=60) if cmd else sh(["gh", "pr", "ready", str(pr)], cwd=code, timeout=60)
            u["flipped_ready"] = rc == 0
            ledger(state, u, f"pr: #{pr} {'flipped to ready' if rc == 0 else 'flip FAILED: ' + tail(out, 200)}")
    drafts = []
    for t in tasks:
        if t.get("pr_number") and not (is_single(state) or MULTI_IS_GROUPS):
            rc, out = sh(["gh", "pr", "view", str(t["pr_number"]), "--json", "isDraft", "--jq", ".isDraft"], cwd=code, timeout=60)
            if rc == 0 and out.strip() == "true":
                drafts.append(f"{t['id']} PR #{t['pr_number']}")
    if drafts:
        u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [f"PR still in draft (checks never went green or verification unfinished): {d}" for d in drafts]
        ledger(state, u, f"pr: {len(drafts)} task PR(s) still in draft - reported, never silently")
    if MULTI_IS_GROUPS and not is_single(state) and state.get("closeout_scope") == "group":
        u["closeout_done_for"] = state.get("closeout_unit")
        u.update({"pr_number": "", "pr_url": "", "integration_checked": False, "integration_amended": False})
        ledger(state, u, f"group: {state.get('closeout_unit')} closed out; next group")
        u["_next"] = "select_tasks"
        return u
    u["_next"] = "finish_plan"
    return u


def finish_plan(state):
    u = {}
    note_script_error(state, u, "flip_ready")
    plans_root = state.get("plans_root", "")
    plan = state.get("plan_path", "")
    if TASKSTORE == "dirstate" and os.path.isfile(os.path.join(plans_root, "bin", "validate")):
        rc, out = sh([os.path.join(plans_root, "bin", "validate"), "--quiet"], cwd=plans_root, timeout=300)
        u["validate_note"] = "green" if rc == 0 else "RED: " + tail(out, 1000)
    if plan and os.path.isfile(plan):
        text = read_file(plan)
        text = re.sub(r"(?im)^(status|Status):\s*.*$", lambda m: f"{m.group(1)}: implemented" if m.group(1) == "status" else "Status: Implemented", text, count=1) \
            if re.search(r"(?im)^(status|Status):", text) else text
        if state.get("pr_url") and state["pr_url"] not in text:
            text = text.rstrip("\n") + f"\n\n**PR:** {state['pr_url']}\n"
        fus = as_list(state.get("follow_ups"))
        if fus and "**Follow-ups:**" not in text:
            text = text.rstrip("\n") + "\n\n**Follow-ups:** " + "; ".join(tail(f, 120) for f in fus[:20]) + "\n"
        write_file(plan, text)
    created = []
    if TASKSTORE != "dirstate":
        for i, f in enumerate(as_list(state.get("follow_ups"))[:20], 1):
            tid, _ = new_task(state, f"followup-{i}", f"Follow-up: {tail(f, 60)}", f"## What\n\n{f}\n\n## Acceptance criteria\n\n- the follow-up is done or explicitly declined\n",
                              task_type="followup", plan_name=os.path.basename(plan))
            if tid:
                created.append(tid)
        for t in as_list(state.get("tasks")):
            if t.get("status") == "complete":
                log_event(state, t, f"pr-opened: {state.get('pr_url') or '(no PR)'}")
    u["followup_tasks"] = created
    ok, out = commit_planning(state, f"plan({state.get('plan_slug')}): implemented" + (f" - PR {state.get('pr_url')}" if state.get("pr_url") else ""))
    if not ok:
        ledger(state, u, f"finish: planning commit failed: {tail(out, 300)}")
    ledger(state, u, f"finish: plan status implemented; follow-up tasks {created or 'n/a'}; validate {u.get('validate_note', 'n/a')}")
    snapshot(state, u, "phase_f finished")
    u["_next"] = "final_report"
    return u


def _collect(state, tag):
    tasks_dir = os.path.join(state.get("tasks_root", ""), "completed") if TASKSTORE == "dirstate" else state.get("tasks_root", "")
    ok, out = core_tool("collect_ledger_lines", tag=tag, dir=tasks_dir)
    if not ok:
        return [f"(collect_ledger_lines unavailable for {tag}: {tail(out, 100)})"]
    return [l for l in out.splitlines() if l.strip() and l.strip() != "(none)"]


def final_report(state):
    u = {}
    harvest_anomalies(state, u, "final")
    note_script_error(state, u, "final")
    tasks = as_list(state.get("tasks"))
    mv = mode(state)
    L = ["# Architect run report", "", f"Architect run id: {state.get('run_id')}", f"Run directory: {state.get('run_dir')}",
         f"Plan: {state.get('plan_path')}", f"Mode: {state.get('mode_line')}", ""]
    done = [t for t in tasks if t.get("status") == "complete"]
    L += [f"## Tasks shipped ({len(done)}/{len(tasks)})"]
    if is_micro(state):
        bs = as_dict(state.get("batch_state"))
        for b in sorted({t.get("batch") for t in tasks if t.get("batch")}):
            info = as_dict(bs.get(b))
            L += [f"### Batch {b} - gauntlet {'PASS' if truthy(info.get('passed')) else 'OPEN'}; batch-closed @ {info.get('closed_at', '-')}; strikes {info.get('strikes', 0)}"]
            L += [f"- {t['id']} {t.get('title', '')} [{t.get('status')}]" + (f" PR {t['pr_url']}" if t.get("pr_url") else "") for t in tasks if t.get("batch") == b]
        L += ["", "## Batch gauntlets"] + [f"- {b}: strikes {as_dict(v).get('strikes', 0)}, verdict {'PASS' if truthy(as_dict(v).get('passed')) else 'OPEN'}" for b, v in bs.items()]
    else:
        L += [f"- {t['id']} {t.get('title', '')} [{t.get('status')}]" + (f" PR {t['pr_url']}" if t.get("pr_url") else "") + (f" @ {t['head_sha']}" if t.get("head_sha") else "") for t in tasks]
    L += [""]
    if state.get("pr_url"):
        L += [f"## PR", f"{state.get('pr_url')} (draft -> {'ready' if truthy(state.get('flipped_ready')) else 'still draft'}; CI {state.get('ci_status') or 'not watched'})", ""]
    if STACK_LINKING and not is_single(state):
        L += ["## Stacks (bottom -> top; merge bottom-up: merging the bottom PR auto-rebases the remaining branches and retargets the next PR to main; no layer merges until every PR below it also passes main's protections)"]
        L += [f"- {d}" for d in as_list(state.get("decisions")) if str(d).startswith("stack:")] or ["- none linked"]
        L += [""]
    L += ["## Review decisions"] + ([f"- {x}" for x in _collect(state, "rejected-finding:")] or ["- none"]) + [""]
    L += ["## Hygiene pass"] + ([f"- {x}" for x in _collect(state, "HYGIENE:")] or ["- clean" if (is_single(state) or MULTI_IS_GROUPS) else "- not required (standalone PRs) / clean"]) + [""]
    L += ["## Design decisions"] + ([f"- {x}" for x in _collect(state, "design-decision")] or ["- none"]) + [""]
    deferred = _collect(state, "(deferred by quality bar)")
    rigor = state.get("quality_rigor") or "production"
    L += ["## Deferred / follow-ups"] + ([f"Quality bar: {rigor}"] if rigor != "production" else []) + \
         ([f"- {x}" for x in deferred] or []) + ([f"- {f}" for f in as_list(state.get("follow_ups"))] or ["- none"]) + [""]
    digest = as_list(state.get("autonomous_digest"))
    if digest or mv["escalation_policy"] == "autonomous":
        L += ["## Decisions taken autonomously"] + ([f"- {d}" for d in digest] or ["- none"]) + [""]
        if mv["escalation_policy"] == "autonomous":
            L += ["Novel decisions that reached the user: record them with `coyote --macro record-ruling` so that class never asks again.", ""]
    skipped = as_list(state.get("skipped_reviews"))
    if skipped:
        L += ["## Skipped reviews (NEEDS-HUMAN)"] + [f"- {s}" for s in skipped] + [""]
    anomalies = as_list(state.get("anomalies_log"))
    if anomalies:
        L += ["## Anomalies (observed outside any task's scope)"] + [f"- {a}" for a in anomalies] + [""]
    if state.get("failure_reason"):
        L += ["## Stop reason", state["failure_reason"], ""]
    u["report"] = "\n".join(L)
    write_artifact(state.get("run_dir", ""), "report.md", u["report"])
    snapshot(state, u, "done")
    _slack_ping(state, f"architect run finished: PLAN-{state.get('plan_slug')} {len(done)}/{len(tasks)} tasks; "
                       f"PR {state.get('pr_url') or 'n/a'} CI {state.get('ci_status') or 'n/a'} (run {state.get('run_id')})")
    u["_next"] = "whetstone_gate"
    return u


def whetstone_gate(state):
    u = {}
    fb = [f for f in as_list(state.get("external_feedback")) if isinstance(f, str) and f.strip()]
    if fb:
        u["_next"] = "run_whetstone"
    else:
        u["whetstone_note"] = "whetstone skipped: no human / third-party review feedback was addressed in this run"
        u["_next"] = "summarize_run"
    return u


def group_closeout(state):
    u = {}
    u["closeout_scope"] = state.get("closeout_scope") or "group"
    u.update({"integration_checked": False, "hygiene_units": []})
    ledger(state, u, f"group: close-out of {state.get('closeout_unit')} (scope {u['closeout_scope']})")
    u["_next"] = "phase_f_gate"
    return u


def reseal_return(state):
    """After a mid-run amend + re-seal + OKAY, go back to where the amendment came from."""
    u = {}
    where = state.get("after_reseal") or ""
    u["after_reseal"] = ""
    if where == "verify":
        u["_next"] = "verify_gate"
    elif where == "phase_f":
        u["_next"] = "hygiene_create"
    elif as_list(state.get("tasks")):
        u["_next"] = "select_tasks"
    else:
        u["_next"] = "autonomy_gate"
    return u


# ====================================================================== entry
GATES = {
    "intake": intake, "resume_route": resume_route, "orient_fault": orient_fault,
    "research_dispatch": research_dispatch, "explore_fault": explore_fault, "archivist_fault": archivist_fault,
    "librarian_fault": librarian_fault, "checklist_fault": checklist_fault, "plan_route": plan_route,
    "open_questions_gate": open_questions_gate, "quality_bar_gate": quality_bar_gate, "record_quality_bar": record_quality_bar,
    "checklist_gate": checklist_gate, "checklist_round": checklist_round,
    "reseal_gate": reseal_gate, "gate1_build": gate1_build, "gate1_route": gate1_route, "leak_route": leak_route,
    "gate2_build": gate2_build, "gate2_route": gate2_route, "autonomy_gate": autonomy_gate, "reseal_return": reseal_return,
    "promote_verify": promote_verify,
    "branch_setup": branch_setup, "select_tasks": select_tasks, "claim_task": claim_task, "compose_delegation": compose_delegation,
    "route_delegate": route_delegate, "task_failed_route": task_failed_route, "verify_gate": verify_gate, "route_verdict": route_verdict,
    "fix_round": fix_round, "post_round": post_round, "fix_done": fix_done, "harvest_report": harvest_report,
    "design_ask_gate": design_ask_gate, "debt_assign": debt_assign, "task_done_verify": task_done_verify, "stack_gate": stack_gate,
    "task_done_route": task_done_route,
    "wave_claim": wave_claim, "wave_verify_build": wave_verify_build, "wave_route": wave_route, "wave_fault": wave_fault,
    "wave_integrate": wave_integrate, "integration_test": integration_test, "wave_cleanup": wave_cleanup,
    "batch_gauntlet_build": batch_gauntlet_build, "batch_fix_route": batch_fix_route, "batch_close": batch_close,
    "escalate_gate": escalate_gate, "ladder_route": ladder_route, "consult_route": consult_route, "ask_dispatch": ask_dispatch,
    "ask_owner_poll": ask_owner_poll, "ask_owner_route": ask_owner_route, "parse_answer": parse_answer, "apply_answer": apply_answer,
    "phase_f_gate": phase_f_gate, "integration_check": integration_check, "integrate_route": integrate_route,
    "hygiene_create": hygiene_create, "stack_gate_final": stack_gate_final, "pr_body": pr_body, "open_pr": open_pr,
    "pr_round_comments": pr_round_comments, "ci_poll": ci_poll, "ci_route": ci_route, "ci_evidence": ci_evidence, "ci_apply": ci_apply,
    "flip_ready": flip_ready, "finish_plan": finish_plan, "final_report": final_report, "whetstone_gate": whetstone_gate,
    "group_closeout": group_closeout,
}


def run(name):
    state = load_state()
    emit(GATES[name](state))
