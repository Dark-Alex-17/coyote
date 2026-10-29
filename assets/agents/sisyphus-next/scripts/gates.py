"""The sisyphus-next gate functions - one per script node.

Each function takes the graph state and returns the updates to merge, including
`_next` when the node routes dynamically. Entry scripts call `run("<name>")`.
Gates on the critical path raise on internal errors (the engine then takes the
node's declared fallback and records last_script_error); advisory gates catch
and declare their degradation instead.
"""

import json
import os
import re
import subprocess
import sys
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from _lib import (  # noqa: E402
    CI_FLAKE_RERUNS, CI_POLL_FAILS, CI_POLL_SLEEP, CI_WALL_SECS, CLARIFY_MAX, DELEGATED_RE,
    OWNER_POLL_MAX, OWNER_POLL_MINUTES, OWNER_REMOTE_INPUT, OWNER_WAIT_MINUTES, PROBE_HOUSE_NOTE,
    QUALITY_RE, REASK_MAX, RERUN_ONCE, RESUME_RE, STRIKE_LIMIT, SUBTASK_MAX, added_lines_since,
    append_artifact, as_dict, as_int, as_list, changed_files_since, emit, ensure_run_dir,
    escalation, git, git_out, harvest_anomalies, ledger, load_state, match_option,
    note_script_error, now_iso, option, raise_escalation, read_artifact, run_dir_for, snapshot,
    tail, truthy, var, write_artifact,
)

SHARED_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".shared")

GAUNTLET_LANES = ("code-review", "adversary", "security", "probe", "debris")
RERUN_ONCE_LANES = ("adversary", "security", "probe")
OPERATIONAL_RE = re.compile(
    r"queue|consumer|producer|cron|scheduler|worker|metric|prometheus|job\b|alert|"
    r"endpoint|handler|route|webhook|retry|timeout|error",
    re.I,
)
DEBT_HITS_STALE = ("DEBT_SCAN_FAILED: the debt_scan node did not run this round and the previous round's report "
                   "was discarded; run the manual fallback named in the graph's initial_state default for debt_hits.")


# ====================================================================== 2.1
def intake(state):
    brief = state.get("initial_prompt") or ""
    project = var(state, "project_dir", ".")
    project_abs = os.path.abspath(os.path.expanduser(project))
    root = git_out(project_abs, "rev-parse", "--show-toplevel", default="")
    head = git_out(root or project_abs, "rev-parse", "HEAD", default="") if root else ""
    u = {
        "project_dir": project_abs,
        "git_root": root,
        "base_sha": head,
        "head_sha": head,
        "debt_base": head,
        "brief": brief,
        "parallel_mode": str(var(state, "parallel", "0")).strip() == "1",
    }
    u["delegated"] = bool(DELEGATED_RE.search(brief)) or u["parallel_mode"]
    u["top_level"] = not u["delegated"]
    if m := QUALITY_RE.search(brief):
        u["quality_rigor"] = m.group(1).strip().lower()
        u["quality_surfaces"] = (m.group(2) or "").strip()
    plans = os.path.join(project_abs, "plans")
    u["plans_detected"] = os.path.isdir(os.path.join(plans, "steps")) and os.path.isdir(
        os.path.join(plans, "handoffs")
    )
    u["debris_exclude"] = '["plans"]' if os.path.isdir(plans) else ""
    away = os.path.join(os.path.expanduser("~"), ".config", "coyote", "share", "away")
    u["owner_away"] = OWNER_REMOTE_INPUT and os.path.isfile(away)
    u["roster_scope"], u["own_scope"] = _parse_roster(brief)

    # Re-entry (A2): a prompt that names a prior run loads its durable state.
    m = RESUME_RE.search(brief)
    prior = {}
    if m:
        prior_dir = run_dir_for(m.group(1))
        raw = read_artifact(prior_dir, "state.json")
        if raw:
            try:
                prior = json.loads(raw)
            except json.JSONDecodeError:
                prior = {}
    if prior:
        run_id = m.group(1)
        u["run_id"] = run_id
        u["run_dir"] = ensure_run_dir(run_id)
        u["resume_mode"] = True
        for k in ("base_sha", "debt_base", "subtasks", "subtask_index", "current_task", "fix_round",
                  "review_rounds", "round_heads", "rejected_findings", "deferred_findings",
                  "skipped_reviews", "debt_left_behind", "debris_exemptions", "design_decisions",
                  "autonomous_digest", "decisions", "follow_ups", "parked", "escalation",
                  "coder_result", "anomalies_log", "gauntlet_lanes"):
            if prior.get(k) is not None:
                u[k] = prior[k]
        u["resume_note"] = (
            f"Re-entry into run {run_id} (phase at exit: {prior.get('phase')}, base {prior.get('base_sha')}, "
            f"head {prior.get('head_sha')}, fix rounds {prior.get('fix_round')}, review rounds "
            f"{prior.get('review_rounds')}). Prior task:\n{prior.get('current_task') or ''}\n\n"
            f"Prior coder result (tail):\n{tail(prior.get('coder_result') or '', 2000)}"
        )
        append_artifact(u["run_dir"], "brief.md", f"\n\n## Re-entry {now_iso()}\n\n{brief}")
    else:
        if m:
            u["resume_note"] = (
                f"The prompt names run {m.group(1)} but no durable state exists for it; "
                "treated as a fresh run (never merged into another run's state)."
            )
        run_id = state.get("run_id") or __import__("_lib").new_run_id(brief)
        u["run_id"] = run_id
        u["run_dir"] = ensure_run_dir(run_id)
        write_artifact(u["run_dir"], "brief.md", brief)
    ledger(state, u, f"run: {run_id} project={project_abs} base={head or '(no git)'} "
                     f"delegated={u['delegated']} parallel={u['parallel_mode']} resume={u['resume_mode'] if 'resume_mode' in u else False}")
    snapshot(state, u, "intake")
    return u


def _parse_roster(brief):
    """Teammate roster lines -> {task_or_agent: [scope paths]}; own scope = the TASK this brief implements."""
    roster, own = {}, []
    if "Teammate roster" not in brief:
        return roster, own
    own_task = None
    if m := re.search(r"Implement (TASK-\d{3})", brief):
        own_task = m.group(1)
    section = brief.split("Teammate roster", 1)[1]
    for line in section.splitlines()[:40]:
        if not line.strip():
            continue
        tasks = re.findall(r"TASK-\d{3}", line)
        if not tasks:
            continue
        paths = re.findall(r"(?<![\w/])((?:[\w.-]+/)+[\w.*-]*|[\w-]+\.[A-Za-z]{1,6})", line)
        roster[tasks[0]] = paths
        if own_task and tasks[0] == own_task:
            own = paths
    return roster, own


def assess_fault(state):
    u = {}
    note_script_error(state, u, "assess")
    err = state.get("last_script_error") or "assess produced no verdict"
    u.update({
        "intent": "ambiguous",
        "sizing": "n/a",
        "intent_statement": "intake classification failed; asking instead of guessing",
        "clarifying_questions": [{
            "question": f"The intake classifier failed ({err[:200]}). How should this message be handled?",
            "options": [
                option("Treat it as an implementation task (one coherent change)"),
                option("Treat it as a question (research answer, no edits)"),
                option("Abort"),
            ],
        }],
    })
    return u


def route_intent(state):
    u = {}
    harvest_anomalies(state, u, "assess")
    intent = state.get("intent") or "ambiguous"
    sizing = state.get("sizing") or "simple"
    cadence = var(state, "review_cadence", "task")
    u["assess_needs_oracle"] = truthy(state.get("needs_oracle"))
    u["assess_oracle_question"] = state.get("oracle_question") or ""

    if truthy(state.get("resume_mode")):
        action = state.get("resume_action") or "fix"
        if action == "git_ops":
            u["git_task"] = {"kind": "instruction", "text": state.get("brief", ""), "return_to": "final_report"}
            u["_next"] = "git_ops"
        elif action == "answer":
            u["_next"] = "research_answer"
        else:
            u["fix_source"] = "resume"
            u["_next"] = "compose_fix"
        return u

    if cadence == "batch" and (state.get("batch_plan_defect") or "").strip():
        u["failure_reason"] = f"plan defect (batch cadence): {state['batch_plan_defect']}"
        ledger(state, u, u["failure_reason"])
        u["_next"] = "end_blocked"
        return u

    questions = as_list(state.get("clarifying_questions"))
    if intent == "ambiguous" or (questions and intent not in ("exploration", "design_question")):
        rounds = as_int(state.get("clarify_rounds"))
        if rounds >= CLARIFY_MAX:
            u["failure_reason"] = f"still ambiguous after {CLARIFY_MAX} clarification rounds"
            u["_next"] = "end_blocked"
            return u
        q = questions[0] if questions else {"question": "The request is ambiguous. What exactly should be done, and where?"}
        opts = [option(o.get("text", ""), o.get("recommended", False), o.get("evidence", ""))
                for o in as_list(q.get("options")) if isinstance(o, dict)]
        u["clarify_rounds"] = rounds + 1
        return raise_escalation(state, u, escalation("clarify", q.get("question", ""), opts))

    if intent in ("exploration", "design_question"):
        u["_next"] = "research_answer"
    elif intent == "plan_authoring":
        u["redirect_reason"] = ("Plan authoring is a design conversation: use the chat lane (engineer) or run the "
                                "architect; the run branch does not author plans.")
        u["_next"] = "end_redirect"
    elif intent == "plan_execution":
        u["_next"] = "run_step"
    elif sizing == "plan_shaped":
        u["redirect_reason"] = ("Plan-shaped work (more than 3 subtasks, cross-PR sequencing, or batch/protected "
                                "semantics): this wants an architect run. The chat lane can draft the design-doc skeleton.")
        u["_next"] = "end_redirect"
    elif sizing == "compound":
        u["_next"] = "plan_subtasks"
    elif truthy(state.get("architecture_scale")):
        u["subtasks"] = [{"title": "task", "outcome": state.get("brief", ""), "constraints": "", "acceptance_criteria": []}]
        u["subtask_index"] = 0
        u["_next"] = "architecture_review"
    else:
        u["subtasks"] = [{"title": "task", "outcome": state.get("brief", ""), "constraints": "", "acceptance_criteria": []}]
        u["subtask_index"] = 0
        u["_next"] = "next_subtask"
    snapshot(state, u, "route_intent")
    return u


def record_subtasks(state):
    u = {}
    harvest_anomalies(state, u, "plan_subtasks")
    subtasks = [s for s in as_list(state.get("subtasks")) if isinstance(s, dict)]
    if not subtasks or len(subtasks) > SUBTASK_MAX:
        why = state.get("plan_shaped_reason") or f"{len(subtasks)} subtasks (bound: {SUBTASK_MAX})"
        u["redirect_reason"] = f"Plan-shaped by A11: {why}. This wants an architect run."
        u["_next"] = "end_redirect"
        return u
    lines = ["# In-run subtasks (A11)", ""]
    for i, s in enumerate(subtasks, 1):
        lines += [f"## {i}. {s.get('title', '')}", "", f"Outcome: {s.get('outcome', '')}", ""]
        if s.get("constraints"):
            lines += [f"Constraints: {s['constraints']}", ""]
        lines += ["Acceptance criteria:"] + [f"- {c}" for c in as_list(s.get("acceptance_criteria"))] + [""]
    write_artifact(state.get("run_dir", ""), "tasklog.md", "\n".join(lines))
    u["subtasks"] = subtasks
    u["subtask_index"] = 0
    ledger(state, u, f"subtasks: {len(subtasks)} sealed in-run subtasks recorded")
    u["_next"] = "next_subtask"
    return u


def next_subtask(state):
    u = {}
    subtasks = as_list(state.get("subtasks"))
    idx = as_int(state.get("subtask_index"))
    if idx >= len(subtasks):
        u["_next"] = "final_report"
        return u
    s = subtasks[idx] if isinstance(subtasks[idx], dict) else {"title": "task", "outcome": str(subtasks[idx])}
    text = s.get("outcome", "")
    if len(subtasks) > 1:
        text = f"Subtask {idx + 1}/{len(subtasks)}: {s.get('title', '')}\n\n{text}"
        if s.get("constraints"):
            text += f"\n\nConstraints: {s['constraints']}"
        ac = as_list(s.get("acceptance_criteria"))
        if ac:
            text += "\n\nAcceptance criteria:\n" + "\n".join(f"- {c}" for c in ac)
    if idx == 0 and (state.get("architecture_proposal") or "").strip():
        text += "\n\nArchitecture-reviewer proposal (adopt only if confirmed below):\n" + tail(state["architecture_proposal"], 4000)
    u.update({
        "current_task": text, "task_number": idx + 1, "subtask_index": idx + 1,
        "fix_strikes": 0, "fix_round": 0, "review_rounds": 0, "debt_rounds": 0, "obs_rounds": 0,
        "lane_block_streak": {}, "debris_reruns": 0, "round_heads": [], "oracle_consulted": False,
        "design_ask_rounds": 0, "rejected_rounds": 0, "strikeout_rounds": 0, "context_pack": "",
        "context_pack_degraded": False, "explore_findings": [], "librarian_findings": [],
        "explore_slices": [], "librarian_questions": [], "design_options": [], "register_question": "",
        "coder_brief": {}, "expected_files": [], "fix_instructions": "", "gauntlet_lanes": {},
        "scope_confirmed": False,
    })
    if idx > 0:
        # Later subtasks diff against the tree the previous one left behind.
        head = git_out(state.get("git_root") or state.get("project_dir") or ".", "rev-parse", "HEAD", default="")
        if head:
            u["base_sha"], u["debt_base"] = head, head
    u["_next"] = "compose_brief" if state.get("intent") == "trivial" else "plan_research"
    snapshot(state, u, f"subtask {idx + 1}")
    return u


# ====================================================================== 2.2
def research_dispatch(state):
    u = {}
    harvest_anomalies(state, u, "plan_research")
    note_script_error(state, u, "plan_research")
    slices = [s for s in as_list(state.get("explore_slices")) if isinstance(s, str) and s.strip()]
    if not slices:
        slices = [f"Find the files, patterns, conventions and call sites relevant to: {tail(state.get('current_task', ''), 600)}"]
        u["team_mode"] = False
        u["librarian_questions"] = []
        ledger(state, u, "research: plan_research produced no slices; degraded to one slice")
    u["explore_slices"] = slices
    u["librarian_questions"] = [q for q in as_list(u.get("librarian_questions", state.get("librarian_questions")))
                               if isinstance(q, str) and q.strip()]
    u["_next"] = "explore_fanout_team" if truthy(state.get("team_mode")) and len(slices) > 1 else "explore_fanout_solo"
    return u


def explore_fault(state):
    prior = state.get("finding") or state.get("last_script_error") or "agent node failed"
    return {"finding": f"PIPELINE-FAULT: explore slice failed ({tail(str(prior), 500)}); slice: {state.get('slice', '')}"}


def librarian_fault(state):
    prior = state.get("finding") or state.get("last_script_error") or "agent node failed"
    return {"finding": f"PIPELINE-FAULT: librarian lane failed ({tail(str(prior), 500)}); question: {state.get('question', '')}"}


def write_context_pack(state):
    u = {}
    harvest_anomalies(state, u, "synthesize_context")
    note_script_error(state, u, "synthesize_context")
    pack = (state.get("context_pack") or "").strip()
    if not pack:
        parts = ["# Context pack (DEGRADED: synthesis unavailable; raw research reports follow)", ""]
        for i, f in enumerate(as_list(state.get("explore_findings")), 1):
            parts += [f"## Explore finding {i}", "", str(f), ""]
        for i, f in enumerate(as_list(state.get("librarian_findings")), 1):
            parts += [f"## Librarian finding {i}", "", str(f), ""]
        pack = "\n".join(parts)
        u["context_pack_degraded"] = True
        u["context_pack"] = pack
        ledger(state, u, "context pack: degraded (synthesis unavailable) - raw findings handed to the coder, declared")
    write_artifact(state.get("run_dir", ""), "context-pack.md", pack)
    snapshot(state, u, "research")
    return u


def oracle_gate(state):
    u = {}
    needs = truthy(state.get("needs_oracle")) or truthy(state.get("assess_needs_oracle"))
    if needs and not truthy(state.get("oracle_consulted")):
        u["oracle_consulted"] = True
        u["oracle_question"] = (state.get("oracle_question") or state.get("assess_oracle_question")
                                or "Review the approach for this task and recommend the design.")
        u["_next"] = "consult_oracle"
    else:
        u["_next"] = "design_gate"
    return u


def merge_oracle(state):
    u = {}
    note_script_error(state, u, "consult_oracle")
    result = (state.get("oracle_result") or "").strip()
    if not result or result.startswith("Agent node failed"):
        return raise_escalation(state, u, escalation(
            "oracle_fault",
            "The oracle consult failed and the task depends on its direction. Never 'time out and continue'. How to proceed?",
            [option("Retry the consult", recommended=as_int(state.get("oracle_retries")) < RERUN_ONCE,
                    evidence="one retry is within the re-run-once bound"),
             option("Proceed without the oracle (the context pack alone directs the coder)"),
             option("Abort this task")],
            evidence=tail(result, 800)))
    pack = (state.get("context_pack") or "") + "\n\n## Oracle direction\n\n" + result
    u["context_pack"] = pack
    write_artifact(state.get("run_dir", ""), "context-pack.md", pack)
    ledger(state, u, "oracle: consulted; direction appended to the context pack")
    u["_next"] = "design_gate"
    return u


def design_gate(state):
    u = {}
    note_script_error(state, u, "architecture_review")
    if (state.get("register_question") or "").strip():
        q = state["register_question"].strip()
        u["register_question"] = ""
        return raise_escalation(state, u, escalation("design_choice", q, [], evidence="codebase register: "
                                                     + str(state.get("codebase_register") or "")))
    opts = [o for o in as_list(state.get("design_options")) if isinstance(o, dict) and o.get("text")]
    if opts:
        u["design_options"] = []
        return raise_escalation(state, u, escalation(
            "design_choice", "Which design approach should the implementation follow?",
            [option(o["text"], o.get("recommended", False), o.get("evidence", "")) for o in opts]))
    proposal = (state.get("architecture_proposal") or "").strip()
    if proposal and not truthy(state.get("proposal_confirmed")):
        u["proposal_confirmed"] = True
        return raise_escalation(state, u, escalation(
            "design_choice", "Adopt the architecture-reviewer's refined proposal as the implementation plan?",
            [option("Adopt the proposal", recommended=True, evidence="refined into an implementation-ready interface"),
             option("Skip it; implement the request as stated")],
            evidence=tail(proposal, 1500)))
    u["_next"] = "compose_brief"
    return u


# ====================================================================== 2.3
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
            digest = list(as_list(state.get("autonomous_digest"))) + [line]
            kinds = list(as_list(state.get("autonomous_kinds"))) + [esc.get("kind")]
            u.update({"autonomous_digest": digest, "autonomous_kinds": kinds,
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
    m = re.search(r"CONSULT:\s*ANSWER\s+(.+)", text)
    if m:
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


def ask_dispatch(state):
    u = {}
    esc = as_dict(state.get("escalation"))
    u["parked"] = esc.get("kind", "")
    snapshot(state, u, "parked")
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
    binary = os.environ.get("SISYPHUS_ASK_OWNER_BIN", "ask_owner")
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
    choice = match_option(text, options)
    if choice is None:
        rounds = as_int(state.get("reask_rounds"))
        if rounds < REASK_MAX:
            u["reask_rounds"] = rounds + 1
            u["ask_question"] = (f"Could not match '{text}' to an option (reply with a number or the option text; "
                                 f"free text stands after {REASK_MAX} tries).\n\n" + state.get("ask_question", ""))
            u["_next"] = _terminal_prompt(esc)
            return u
        choice = text
    u.update({"last_answer": choice, "last_answer_text": text, "parked": ""})
    ledger(state, u, f"decision: {esc.get('kind')}: {choice}")
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

    if kind == "clarify":
        u["clarifications"] = (state.get("clarifications") or "") + f"\nQ: {esc.get('question', '')}\nA: {answer}\n"
        u["_next"] = "assess"
    elif kind == "design_choice":
        u["_next"] = "compose_brief"
    elif kind == "oracle_fault":
        a = _answered(state, {"retry": ["retry"], "abort": ["abort"], "proceed": ["proceed"]}, "proceed")
        if a == "retry" and as_int(state.get("oracle_retries")) < RERUN_ONCE:
            u["oracle_retries"] = as_int(state.get("oracle_retries")) + 1
            u["_next"] = "consult_oracle"
        elif a == "abort":
            u["failure_reason"] = "aborted at the user's request after an oracle consult failure"
            u["_next"] = "end_aborted"
        else:
            u["_next"] = "design_gate"
    elif kind == "coder_rejected":
        u["fix_source"] = "rejected"
        u["fix_note"] = answer
        u["_next"] = "compose_fix"
    elif kind == "engine_fault":
        a = _answered(state, {"abort": ["abort"], "retry": ["retry"]}, "retry")
        if a == "abort":
            u["failure_reason"] = "aborted: a verification gate crashed (engine fault)"
            u["_next"] = "end_aborted"
        else:
            u["fix_source"] = "coder_failed"
            u["_next"] = "compose_fix"
    elif kind == "design_ask":
        q = esc.get("question", "")
        dd = list(as_list(state.get("design_decisions"))) + [f"{q} -> {answer}"]
        u["design_decisions"] = dd
        ledger(state, u, f"design-decision: {q} -> {answer}")
        adopt = _answered(state, {"adopt": ["adopt", "yes", "introduce"]}, None) == "adopt"
        if adopt and as_int(state.get("design_ask_rounds")) < RERUN_ONCE:
            u["design_ask_rounds"] = as_int(state.get("design_ask_rounds")) + 1
            u["design_asks"] = []
            u["fix_source"] = "design_ask"
            u["fix_note"] = f"Adopt the pattern the user chose: {answer}"
            u["_next"] = "compose_fix"
        else:
            u["design_asks"] = []
            u["_next"] = "review_gate"
    elif kind == "yellow_findings":
        a = _answered(state, {"fix": ["fix"], "accept": ["accept"]}, "fix")
        if a == "fix":
            u["fix_source"] = "review"
            u["_next"] = "compose_fix"
        else:
            u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [
                f"Accepted without fix at the user's say-so: {tail(esc.get('evidence', ''), 300)}"]
            u["_next"] = "observability_gate"
    elif kind == "plan_defect":
        a = _answered(state, {"stop": ["stop", "amend the plan", "abort"], "continue": ["continue", "treat"]}, "stop")
        if a == "continue":
            u["spec_text"] = (state.get("spec_text") or "") + f"\n\nAMENDED by the user: {answer}"
            ledger(state, u, f"plan amendment (user): {answer}")
            u["_next"] = "build_gauntlet_prompt"
        else:
            u["failure_reason"] = f"the plan itself is the root cause of a conformance failure; the user chose to stop: {answer}"
            u["_next"] = "end_blocked"
    elif kind == "probe_gap":
        a = _answered(state, {"supply": ["supply", "provide"], "surface": ["surface", "proceed"]}, "surface")
        if a == "supply":
            u["fix_source"] = "probe_gap"
            u["fix_note"] = answer
            u["_next"] = "compose_fix"
        else:
            u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [
                "Consumer-facing change could not be exercised locally (probe INCONCLUSIVE): "
                "provide the local-run recipe / fixture and re-probe"]
            u["_next"] = "observability_gate"
    elif kind == "review_incomplete":
        a = _answered(state, {"retry": ["retry"], "accept": ["accept"], "abort": ["abort"]}, "retry")
        if a == "retry":
            u["_next"] = "build_gauntlet_prompt"
        elif a == "accept":
            lanes = as_list(state.get("gauntlet_incomplete")) or ["pipeline"]
            u["skipped_reviews"] = list(as_list(state.get("skipped_reviews"))) + [
                f"round {len(as_list(state.get('round_heads')))}: {', '.join(lanes)} (NEEDS-HUMAN accepted)"]
            u["_next"] = "observability_gate"
        else:
            u["failure_reason"] = "aborted at the user's request: the review could not be completed"
            u["_next"] = "end_aborted"
    elif kind in ("strikeout", "lane_stuck", "review_exhausted"):
        a = _answered(state, {"abort": ["abort"], "round": ["apply", "one more", "round"], "accept": ["accept"]}, "abort")
        if a == "round" and as_int(state.get("strikeout_rounds")) < RERUN_ONCE:
            u["strikeout_rounds"] = as_int(state.get("strikeout_rounds")) + 1
            u["fix_strikes"] = 0
            u["fix_source"] = "strikeout" if kind == "strikeout" else "review"
            u["_next"] = "compose_fix"
        elif a == "accept" and kind != "strikeout":
            u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [
                f"Review blockers accepted unresolved at the user's say-so ({kind}); see the gauntlet report"]
            u["_next"] = "observability_gate"
        else:
            u["failure_reason"] = f"stopped after repeated failures ({kind}); the code was left in a working state"
            u["_next"] = "end_aborted"
    elif kind == "scope_breach":
        a = _answered(state, {"proceed": ["proceed", "re-scope", "rescope"], "serialize": ["serialize", "in-scope"],
                              "abort": ["abort"]}, "serialize")
        if a == "abort":
            u["failure_reason"] = "aborted: scope breach could not be resolved"
            u["_next"] = "end_aborted"
        else:
            brief = as_dict(state.get("coder_brief"))
            extra = list(as_list(brief.get("must_not_do_extra")))
            if a == "serialize":
                extra.append(f"Do NOT edit files outside this task's declared scope ({state.get('scope_breach', '')}); "
                             "report the remaining edit as a follow-up for the parent to serialize.")
            else:
                extra.append(f"Scope re-declared by the parent to include: {answer}")
            brief["must_not_do_extra"] = extra
            u["coder_brief"] = brief
            u["scope_breach"] = ""
            u["_next"] = "render_coder_prompt"
    elif kind in ("ci_unsure", "ci_exhausted", "ci_infra"):
        a = _answered(state, {"rerun": ["re-run", "rerun", "keep waiting", "wait"], "fix": ["resume", "coder", "fix"],
                              "leave": ["leave", "draft", "report"]}, "leave")
        if a == "rerun":
            u.update({"ci_polls": 0, "ci_poll_fails": 0, "ci_started_at": int(time.time())})
            u["_next"] = "ci_poll"
        elif a == "fix":
            u["fix_source"] = "ci"
            u["_next"] = "compose_fix"
        else:
            u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [
                f"PR left in draft with CI {state.get('ci_status') or 'unresolved'}: {tail(state.get('ci_diagnosis', ''), 300)}"]
            u["_next"] = "final_report"
    else:
        ledger(state, u, f"apply_answer: unknown escalation kind {kind!r}; continuing to the report")
        u["_next"] = "final_report"
    snapshot(state, u, f"answered {kind}")
    return u


# ====================================================================== 2.4
def scope_check(state):
    u = {}
    harvest_anomalies(state, u, "compose_brief")
    note_script_error(state, u, "compose_brief")
    expected = [f for f in as_list(state.get("expected_files")) if isinstance(f, str)]
    if truthy(state.get("parallel_mode")):
        own = as_list(state.get("own_scope"))
        roster = as_dict(state.get("roster_scope"))
        if own and expected:
            outside = [f for f in expected if not any(_in_scope(f, s) for s in own)]
            if outside:
                owners = {f: [t for t, paths in roster.items() if any(_in_scope(f, p) for p in paths)] for f in outside}
                breach = "; ".join(f"{f} (owned by {', '.join(o) or 'nobody declared'})" for f, o in owners.items())
                u["scope_breach"] = breach
                u["escalation"] = escalation(
                    "scope_breach",
                    f"Scope breach: correct implementation needs files outside this task's declared scope: {breach}. "
                    "Nothing was edited. How should the parent resolve it?",
                    [option("Serialize: finish in-scope work only; the breaching edit waits for the owner", recommended=True,
                            evidence="the batch was declared file-disjoint"),
                     option("Proceed: scope re-declared to include these files"),
                     option("Abort this task")],
                    hard_trigger=True)
                u["ask_question"] = __import__("_lib").render_question(u["escalation"])
                u["_next"] = "send_breach_notice"
                return u
        elif not own:
            ledger(state, u, "scope check: no declared own scope in the roster; breach check skipped")
    elif not truthy(state.get("delegated")) and len(expected) >= 5 and not truthy(state.get("scope_confirmed")):
        u["scope_confirmed"] = True
        return raise_escalation(state, u, escalation(
            "design_choice",
            f"This change is expected to touch {len(expected)} files. Proceed?",
            [option("Proceed", recommended=True, evidence="the context pack grounds the approach"),
             option("Narrow the scope first (say how)")],
            evidence="\n".join(expected[:20])))
    u["_next"] = "render_coder_prompt"
    return u


def _in_scope(path, scope):
    scope = scope.rstrip("/")
    if scope.endswith("*"):
        return path.startswith(scope[:-1])
    return path == scope or path.startswith(scope + "/")


STANDARD_MUST_DO = [
    "Follow the patterns in the reference files below; match naming, import and error-handling conventions shown",
    "Load skill `code-review` after editing to self-review",
    "Leave a same-line owning-task comment on every debt marker (`#[allow(...)]`, `#[expect(...)]` (outer or inner "
    "`#![...]` form), any `cfg_attr(...)` carrying an allow or expect, `todo!()`, `unimplemented!()`, `#[ignore]`, "
    "env-gated or opt-in tests); for an env- or cfg-gated test, a gate the lint cannot recognise, the comment goes on "
    "the gate's code line (the `if` of the env check, the `fn` line under the `#[cfg(...)]`)",
]
STANDARD_MUST_NOT = [
    "Do not modify files outside the scope named above",
    "Do not introduce new dependencies",
    "Do not suppress errors (as any, @ts-ignore, #[allow(...)] on unfamiliar lints)",
    "Do not write non-ASCII punctuation or planning-artefact references (task ids, rulings, phases, rounds) into "
    "comments, docs or strings (the one carve-out: a task id on the same line as a debt marker)",
    "Do not leave any of the debt markers named in the code-review skill without a same-line owning-task comment",
    "Do not record decisions or evidence ledgers in CONTRIBUTING/README/code comments; they go to the plan or task log",
    "Do not commit or push unless the task block says so",
]


def render_coder_prompt(state):
    u = {}
    brief = as_dict(state.get("coder_brief"))
    task_text = brief.get("task") or state.get("current_task") or state.get("brief") or ""
    degraded = not brief.get("task")
    parts = ["## TASK", task_text, ""]
    parts += ["## EXPECTED OUTCOME", brief.get("expected_outcome") or "Done when the task above is implemented, "
              "build and tests pass, and the acceptance criteria (if any) are met.", ""]
    parts += ["## REQUIRED TOOLS", "fs_cat, fs_write, fs_patch, execute_command", ""]
    parts += ["## MUST DO"] + [f"- {l}" for l in STANDARD_MUST_DO + as_list(brief.get("must_do_extra"))] + [""]
    parts += ["## MUST NOT DO"] + [f"- {l}" for l in STANDARD_MUST_NOT + as_list(brief.get("must_not_do_extra"))] + [""]
    parts += ["## CONTEXT"]
    if degraded:
        parts += ["(brief composition degraded: the research context pack follows verbatim)", "",
                  tail(state.get("context_pack") or "", 20000)]
    else:
        parts += [brief.get("context_block") or ""]
    spec = (state.get("spec_text") or "").strip()
    if spec and spec not in (brief.get("context_block") or ""):
        parts += ["", "Acceptance criteria (verbatim):", spec]
    if state.get("quality_rigor"):
        parts += ["", f"Quality bar: rigor={state.get('quality_rigor')}, surfaces={state.get('quality_surfaces') or ''}"]
    for n in as_list(brief.get("skill_nudges")):
        parts += ["", f"Skill nudge: {n}"]
    fix = (state.get("fix_instructions") or "").strip()
    if fix:
        n = as_int(state.get("fix_round"))
        parts += ["", f"## FIX ROUND {n}",
                  "This continues the SAME task - do not restart from scratch; the files you created or modified are "
                  "listed in your previous result below. Address every item verbatim.", "",
                  fix, "", "### Your previous result", tail(state.get("coder_result") or "", 4000)]
    prompt = "\n".join(parts)
    u["coder_prompt"] = prompt
    write_artifact(state.get("run_dir", ""), f"coder-prompt-{as_int(state.get('fix_round'))}.md", prompt)
    u["_next"] = "check_inbox_pre" if truthy(state.get("parallel_mode")) else "run_coder"
    return u


def inbox_route(state):
    u = {}
    harvest_anomalies(state, u, "check_inbox")
    instr = (state.get("inbox_instructions") or "").strip()
    if instr:
        ledger(state, u, f"inbox: parent instruction before implementation: {tail(instr, 200)}")
        u["git_task"] = {"kind": "instruction", "text": instr, "return_to": "run_coder"}
        u["_next"] = "git_ops"
    else:
        u["_next"] = "run_coder"
    return u


def _coder_error_tail(result):
    m = re.search(r"(Verification gate error:.*)", result, re.S)
    body = m.group(1) if m else result
    return tail(body, 4000)


def route_coder_result(state):
    u = {}
    note_script_error(state, u, "run_coder")
    result = state.get("coder_result") or ""
    if "CODER_COMPLETE" in result:
        u.update({"coder_sentinel": "CODER_COMPLETE", "fix_strikes": 0, "fix_instructions": "", "last_coder_error": ""})
        block, asks = _design_block(result)
        u["design_block"], u["design_asks"] = block, asks
        u["debt_base"] = state.get("base_sha") or ""
        ledger(state, u, f"coder: CODER_COMPLETE (fix round {as_int(state.get('fix_round'))})")
        u["_next"] = "debt_scan"
    elif "CODER_REJECTED" in result:
        rounds = as_int(state.get("rejected_rounds")) + 1
        u["rejected_rounds"] = rounds
        u["coder_sentinel"] = "CODER_REJECTED"
        if rounds > STRIKE_LIMIT:
            u["failure_reason"] = f"the coder's plan was rejected {rounds} times"
            u["_next"] = "end_blocked"
            return u
        return raise_escalation(state, u, escalation(
            "coder_rejected",
            "The coder's plan was rejected at the approval gate. What should change before it plans again?",
            [], evidence=tail(result, 1500), tier3_ok=False))
    else:
        strikes = as_int(state.get("fix_strikes")) + 1
        u["fix_strikes"] = strikes
        u["coder_sentinel"] = "CODER_FAILED" if "CODER_FAILED" in result else "NO_SENTINEL"
        err = _coder_error_tail(result) if result else "coder returned no output"
        if not result or ("CODER_FAILED" not in result and "Agent node failed" not in result):
            err = "coder returned no recognizable sentinel (expected CODER_COMPLETE / CODER_REJECTED / CODER_FAILED)\n" + err
        u["last_coder_error"] = err
        ledger(state, u, f"coder: {u['coder_sentinel']} strike {strikes}/{STRIKE_LIMIT}")
        m = re.search(r"Last script error:\s*\n(.*?)(?:\n\s*\n|\nLast |\Z)", result, re.S)
        if m and m.group(1).strip() and m.group(1).strip() != "null":
            return raise_escalation(state, u, escalation(
                "engine_fault",
                f"The coder's verification gate crashed (an engine-side fault, not a code fix): {m.group(1).strip()[:300]}. Retry the round or abort?",
                [option("Retry the coder round", recommended=True, evidence="a crashed gate is transient more often than not"),
                 option("Abort this task")], evidence=err))
        if strikes >= STRIKE_LIMIT:
            u["_next"] = "strikeout"
        elif strikes == 2:
            u["_next"] = "diagnose_failure"
        else:
            u["fix_source"] = "coder_failed"
            u["_next"] = "compose_fix"
    snapshot(state, u, "coder")
    return u


def _design_block(result):
    m = re.search(r"^Design:\s*(.*?)(?=^\S[^\n]*:|\Z)", result, re.S | re.M)
    block = m.group(1).strip() if m else ""
    if block.lower() in ("", "none", "null", "n/a"):
        block = ""
    asks = [l.strip() for l in block.splitlines() if l.strip().startswith("ASK --")]
    return block, asks


def compose_fix(state):
    u = {}
    harvest_anomalies(state, u, "diagnose_failure")
    src = state.get("fix_source") or "coder_failed"
    note = state.get("fix_note") or ""
    lines = []
    if src == "coder_failed":
        lines += ["Fix: the previous round failed verification.", "", state.get("last_coder_error") or ""]
        if (state.get("diagnosis") or "").strip():
            lines += ["", "Diagnosis brief:", state["diagnosis"]]
    elif src == "review":
        for item in as_list(state.get("fix_items")):
            lines.append(f"- {item}")
        lines.insert(0, "Fix these review findings (verbatim; critical findings and plan-conformance / security / "
                        "behavioral failures block completion):")
    elif src == "debt":
        lines += ["Name the owning task on the same line as each debt marker below, or remove the marker; move any "
                  "decision or evidence ledger out of README/CONTRIBUTING/code comments into the task log:"]
        lines += [f"- {d}" for d in as_list(state.get("fix_items"))]
    elif src == "design_ask":
        lines += [note]
    elif src == "rejected":
        lines += ["The user rejected the previous plan. Change the approach as follows before planning again:", note]
    elif src == "strikeout":
        lines += ["Three fix attempts failed; the broken attempt was stashed. Apply the oracle's direction below in ONE round:",
                  "", state.get("oracle_result") or ""]
    elif src == "observability":
        lines += ["Observability pass: make these alert-as-code / instrumentation changes following the repo's existing "
                  "rule conventions (ordinary code changes; the usual review gates apply):", state.get("obs_coder_brief") or ""]
    elif src == "probe_gap":
        lines += ["Provide the local-run recipe / fixture the usage probe needs, as the user described:", note]
    elif src == "ci":
        lines += ["CI is red on the PR. Fix the failure below (evidence pack verbatim):", state.get("ci_evidence") or ""]
        if (state.get("ci_diagnosis") or "").strip():
            lines += ["", "Diagnosis:", state["ci_diagnosis"]]
    elif src == "resume":
        lines += ["Resume request from the orchestrator (address every item):", state.get("brief") or ""]
    else:
        lines += [note or state.get("last_coder_error") or ""]
    u["fix_instructions"] = "\n".join(lines).strip()
    u["fix_round"] = as_int(state.get("fix_round")) + 1
    u["fix_note"] = ""
    u["diagnosis"] = state.get("diagnosis") or ""
    u["_next"] = "render_coder_prompt"
    return u


def strikeout(state):
    u = {}
    root = state.get("git_root") or state.get("project_dir") or "."
    dirty = git_out(root, "status", "--porcelain")
    stash = ""
    if dirty:
        code, out, err = git(root, "stash", "push", "-u", "-m", f"sisyphus-next strikeout {state.get('run_id', '')}")
        stash = out.strip() or err.strip()
    attempts = f"{STRIKE_LIMIT} consecutive failed fix attempts. Last error:\n{state.get('last_coder_error') or ''}"
    u["diagnosis"] = ((state.get("diagnosis") or "") + "\n\n" + attempts).strip()
    ledger(state, u, f"strikeout: stopped after {STRIKE_LIMIT} strikes; "
                     + (f"broken attempt stashed ({stash[:80]})" if stash else "worktree was clean"))
    u["strikeout_stash"] = stash
    snapshot(state, u, "strikeout")
    return u


def strikeout_route(state):
    u = {}
    note_script_error(state, u, "oracle_strikeout")
    result = (state.get("oracle_result") or "").strip()
    unresolved = (not result or result.startswith("Agent node failed")
                  or re.search(r"cannot (be )?resolve|unable to resolve|no resolution|cannot determine", result, re.I))
    if unresolved:
        opts = [option("Abort and report (code left in a working state; the failed attempt is stashed)", recommended=True,
                       evidence="the oracle could not resolve it"),
                option("Apply the oracle's notes in one more coder round anyway")]
    else:
        opts = [option("Apply the oracle's direction in ONE more coder round", recommended=True,
                       evidence="the oracle proposed a concrete resolution"),
                option("Abort and report")]
    return raise_escalation(state, u, escalation(
        "strikeout", f"Three fix attempts failed on the same problem. Oracle verdict: "
                     f"{'unresolved' if unresolved else 'resolution proposed'}. How to proceed?",
        opts, evidence=tail(result, 2500), hard_trigger=bool(unresolved)))


# ====================================================================== 2.5
def bookkeeping_scan(state):
    u = {}
    crashed = bool(state.get("last_script_error"))
    note_script_error(state, u, "debt_scan")
    root = state.get("git_root") or state.get("project_dir") or "."
    base = state.get("base_sha") or ""
    hits = state.get("debt_hits") or ""
    items = []
    if crashed:
        # The engine leaves debt_hits untouched on a crash, so it holds the initial_state
        # default (first pass) or a previous round's report; neither describes this worktree.
        if not hits.startswith("DEBT_SCAN_FAILED"):
            u["debt_hits"] = DEBT_HITS_STALE
            ledger(state, u, "debt_scan: the node did not run this round; the previous round's report was discarded")
    else:
        for line in hits.splitlines():
            m = re.match(r"^(\S+?:\d+)\s+--\s+(.*)$", line.strip())
            if not m:
                continue
            owner = re.search(r"\b[A-Z][A-Z0-9]+-\d{2,}\b", m.group(2))
            items.append({"where": m.group(1), "text": m.group(2).strip(), "owner": owner.group(0) if owner else ""})
    u["debt_items"] = items
    u["changed_files"] = changed_files_since(root, base)
    u["diff_stat"] = tail(git_out(root, "diff", "--stat", base) if base else git_out(root, "diff", "--stat", "HEAD"), 3000)
    u["head_sha"] = git_out(root, "rev-parse", "HEAD", default=state.get("head_sha", ""))
    added = added_lines_since(root, base)
    docs = []
    cur = None
    code, out, _ = git(root, "diff", "--no-color", "--no-ext-diff", base or "HEAD", "--", "README*", "CONTRIBUTING*", timeout=60)
    for l in (out if code == 0 else "").splitlines():
        if l.startswith("+++ b/"):
            cur = l[6:]
        elif l.startswith("+") and not l.startswith("+++") and re.search(
                r"(?i)\b(decision|rejected-finding|ruling|ledger|we chose|verified against)\b", l):
            docs.append(f"{cur}: {l[1:].strip()[:160]}")
    u["decisions_in_docs"] = docs
    u["added_lines_cache"] = tail(added, 30000)
    return u


def bookkeeping_route(state):
    u = {}
    harvest_anomalies(state, u, "bookkeeping_judge")
    note_script_error(state, u, "bookkeeping_judge")
    items = [i for i in as_list(state.get("debt_items")) if isinstance(i, dict)]
    env_tests = [t for t in as_list(state.get("env_gated_tests")) if isinstance(t, str) and t.strip()]
    docs = as_list(state.get("decisions_in_docs"))
    unowned = [f"{i['where']} -- {i['text']}" for i in items if not i.get("owner")] + \
              [f"env-gated/opt-in test -- {t}" for t in env_tests]
    if (unowned or docs) and as_int(state.get("debt_rounds")) < RERUN_ONCE:
        u["debt_rounds"] = as_int(state.get("debt_rounds")) + 1
        u["fix_items"] = unowned + [f"decision written into docs: {d}" for d in docs]
        u["fix_source"] = "debt"
        ledger(state, u, f"debt: {len(unowned)} unowned marker(s), {len(docs)} decision(s) in docs -> one coder round")
        u["_next"] = "compose_fix"
        return u
    left = [f"{i['where']} -- {i['text']} -- {i.get('owner') or 'none'}" for i in items] + \
           [f"env-gated/opt-in test -- {t} -- none" for t in env_tests]
    u["debt_left_behind"] = sorted(set(as_list(state.get("debt_left_behind")) + left))
    if docs:
        u["follow_ups"] = list(as_list(state.get("follow_ups"))) + [f"Decision text still in docs after one round: {d}" for d in docs]
    asks = [a for a in as_list(state.get("design_asks")) if isinstance(a, str) and a.strip()]
    if asks and as_int(state.get("design_ask_rounds")) < RERUN_ONCE + 1 and not truthy(state.get("design_ask_pending_done")):
        ask = asks[0]
        u["design_ask_pending_done"] = True
        return raise_escalation(state, u, escalation(
            "design_ask", f"Design question: {ask}",
            [option("Adopt the pattern (one more coder round introduces it)"),
             option("Keep the code as shipped (answer recorded, no change)")],
            hard_trigger=True, tier3_ok=False))
    block = (state.get("design_block") or "").strip()
    if block:
        dd = list(as_list(state.get("design_decisions")))
        if block not in dd:
            dd.append(block)
            ledger(state, u, f"design-decision: {tail(block, 300)}")
        u["design_decisions"] = dd
    u["design_ask_pending_done"] = False
    u["_next"] = "review_gate"
    snapshot(state, u, "bookkeeping")
    return u


# ====================================================================== 2.6
def review_gate(state):
    u = {}
    cadence = var(state, "review_cadence", "task")
    if cadence == "batch":
        u["review_skipped_note"] = ("review cadence batch: no gauntlet / reviewer / observability pass on this task; "
                                    "the parent orchestrator runs the full panel over the batch")
        u["_next"] = "whetstone_gate"
    elif state.get("intent") == "trivial" and len(as_list(state.get("changed_files"))) <= 1 and not truthy(state.get("has_spec")):
        u["review_skipped_note"] = "review skipped: trivial single-file change with no spec (coder self_review is sufficient)"
        u["_next"] = "whetstone_gate"
    else:
        u["_next"] = "build_gauntlet_prompt"
    return u


def _posture(state):
    posture = (state.get("security_posture") or "standard").strip().lower()
    rigor = (state.get("quality_rigor") or "production").strip().lower()
    if posture == "hardened":
        return "hardened"
    return {"poc": "prototype", "prototype": "standard", "production": "standard"}.get(rigor, posture or "standard")


def build_gauntlet_prompt(state):
    u = {}
    root = state.get("git_root") or state.get("project_dir") or "."
    base = state.get("base_sha") or ""
    head = git_out(root, "rev-parse", "HEAD", default=state.get("head_sha", ""))
    rounds = as_list(state.get("round_heads"))
    n = len(rounds) + 1
    diff = base if base else "worktree"
    lines = ["Post-implementation review gate.",
             f"Project: {state.get('project_dir')}",
             f"Diff: {diff}",
             f"Rigor: {state.get('quality_rigor') or 'production'}   Security posture: {_posture(state)}"]
    spec = (state.get("spec_text") or "").strip()
    if spec:
        lines += ["Plan / acceptance criteria:", spec]
    if rounds:
        prior = as_list(state.get("rejected_findings"))
        if prior:
            lines += ["Prior review decisions:"] + [f"- {p}" for p in prior]
        last = rounds[-1] if isinstance(rounds[-1], dict) else {}
        dirty = bool(git_out(root, "status", "--porcelain"))
        if last.get("head") and last["head"] != head and not dirty:
            lines += [f"Re-review delta: {last['head']}..HEAD"]
            green = [l for l, v in as_dict(state.get("gauntlet_lanes")).items()
                     if l != "debris" and str(v).strip() == "GREEN" and l not in as_list(last.get("lanes_with_findings"))]
            if green:
                lines += [f"Settled lanes: {', '.join(green)}"]
    recipe = (state.get("local_run_recipe") or "").strip()
    if recipe or truthy(state.get("consumer_surface")):
        note = recipe or "discover them"
        if PROBE_HOUSE_NOTE and truthy(state.get("consumer_surface")):
            note += f"\n{PROBE_HOUSE_NOTE}"
        lines += ["Local-run recipe / usage suites:", note]
    u["gauntlet_prompt"] = "\n".join(lines)
    u["round_heads"] = rounds + [{"round": n, "head": head, "started": now_iso()}]
    u["head_sha"] = head
    u["prior_review_decisions"] = "\n".join(as_list(state.get("rejected_findings")))
    ledger(state, u, f"gauntlet: round {n} spawned @ {head or '(no git)'}")
    snapshot(state, u, f"gauntlet round {n}")
    return u


def _parse_gauntlet(text):
    verdict = "BLOCKED"
    if found := re.findall(r"^GAUNTLET:\s*(PASS|BLOCKED)\s*$", text, re.M):
        verdict = found[-1]
    incomplete = []
    if found := re.findall(r"^GAUNTLET_REVIEW_INCOMPLETE:\s*([^\n]+?)\s*$", text, re.M):
        incomplete = [x.strip() for x in found[-1].split(",") if x.strip()]
    lanes = {}
    details = {}
    for line in text.splitlines():
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) >= 2 and cells[0].lower() in GAUNTLET_LANES:
            lanes[cells[0].lower()] = cells[1]
            if len(cells) >= 3 and cells[2]:
                details[cells[0].lower()] = cells[2]
    blockers = []
    sec = re.search(r"(?ims)^#+\s*Blockers?\s*\n(.*?)(?=^#+\s|\Z)", text)
    if sec:
        blockers = [l.strip()[1:].strip() for l in sec.group(1).splitlines() if l.strip().startswith(("-", "*"))]
    blockers += [l.strip() for l in text.splitlines() if l.strip().startswith("debris: Implementation debris")]
    attention = ""
    if m := re.search(r"(?ims)^#+\s*Human attention required\s*\n(.*?)(?=^#+\s|\Z)", text):
        attention = m.group(1).strip()
    return verdict, incomplete, lanes, details, blockers, attention


def route_gauntlet(state):
    u = {}
    note_script_error(state, u, "run_gauntlet")
    text = state.get("gauntlet_result") or ""
    if not text or text.startswith("Agent node failed") or "GAUNTLET:" not in text:
        verdict, incomplete, lanes, details, blockers, attention = "BLOCKED", ["pipeline"], {}, {}, [], ""
    else:
        verdict, incomplete, lanes, details, blockers, attention = _parse_gauntlet(text)
    rounds = as_list(state.get("round_heads"))
    if rounds and isinstance(rounds[-1], dict):
        rounds[-1].update({"verdict": verdict, "lanes": lanes, "lane_details": details, "incomplete": incomplete,
                           "lanes_with_findings": [l for l, v in lanes.items() if v not in ("GREEN", "GREEN (attention)", "SKIPPED", "")]})
    merged_lanes = dict(as_dict(state.get("gauntlet_lanes")))
    merged_lanes.update(lanes)
    u.update({"gauntlet_verdict": verdict, "gauntlet_incomplete": incomplete, "gauntlet_lanes": merged_lanes,
              "gauntlet_blockers": blockers, "gauntlet_attention": attention, "round_heads": rounds})
    ledger(state, u, f"gauntlet: round {len(rounds)} -> {verdict}" + (f" incomplete={incomplete}" if incomplete else ""))
    if verdict == "PASS":
        u["_next"] = "observability_gate"
    else:
        real = [b for b in blockers if "PIPELINE-FAULT" not in b and "review incomplete" not in b.lower()]
        if real:
            u["gauntlet_blockers"] = real
            u["_next"] = "triage_findings"
        elif incomplete == ["debris"] and as_int(state.get("debris_reruns")) < RERUN_ONCE:
            u["debris_reruns"] = as_int(state.get("debris_reruns")) + 1
            ledger(state, u, "gauntlet: debris lint produced no verdict; one re-run (transient assumed)")
            u["_next"] = "build_gauntlet_prompt"
        else:
            lanes_txt = ", ".join(incomplete or ["pipeline"])
            return raise_escalation(state, u, escalation(
                "review_incomplete",
                f"The review could not be completed: {lanes_txt} still faulted (not a code finding). Retry, accept "
                f"NEEDS-HUMAN (proceed without the {lanes_txt} review, recorded in the report / PR body), or abort?",
                [option("Retry the review again"),
                 option("Accept NEEDS-HUMAN: proceed without the incomplete review (recorded in the report / PR body)"),
                 option("Abort this task")], evidence=tail(text, 1500)))
    snapshot(state, u, "gauntlet")
    return u


def _lane_of(item, finding):
    lane = (item.get("lane") or "").lower()
    if lane in GAUNTLET_LANES:
        return lane
    f = finding.lower()
    if "adversar" in f or "diverges" in f or "criterion" in f:
        return "adversary"
    if "security" in f:
        return "security"
    if "probe" in f or "usage_probe" in f:
        return "probe"
    if f.startswith("debris"):
        return "debris"
    return "code-review"


def triage_route(state):
    u = {}
    harvest_anomalies(state, u, "triage_findings")
    note_script_error(state, u, "triage_findings")
    blockers = as_list(state.get("gauntlet_blockers"))
    triage = [t for t in as_list(state.get("triage")) if isinstance(t, dict) and t.get("finding")]
    if not triage:
        triage = [{"finding": b, "action": "fix"} for b in blockers]
    rigor = (state.get("quality_rigor") or "production").lower()
    fixes, escalations = [], []
    rejected = list(as_list(state.get("rejected_findings")))
    deferred = list(as_list(state.get("deferred_findings")))
    exemptions = list(as_list(state.get("debris_exemptions"))) + [e for e in as_list(state.get("debris_exemptions_new", [])) if e]
    for t in triage:
        finding = str(t.get("finding", ""))
        action = t.get("action") or "fix"
        sev = (str(t.get("severity") or "") + " " + finding).lower()
        critical = "🔴" in sev or "critical" in sev or "[correctness]" in sev
        citation = str(t.get("citation") or "")
        cited = bool(re.search(r"[\w./-]+:\d+", citation)) or "quality bar" in citation.lower() or "dropped" in citation.lower()
        if action == "reject" and (critical or not cited):
            action = "fix"
            ledger(state, u, f"triage: rejection without valid citation (or non-rejectable class) stands as a finding: {tail(finding, 120)}")
        if action == "defer" and rigor not in ("poc", "prototype"):
            action = "fix"
        if action == "reject":
            line = f"rejected-finding: {finding} -- {citation}"
            if line not in rejected:
                rejected.append(line)
                ledger(state, u, line)
        elif action == "defer":
            deferred.append(f"{finding} (deferred by quality bar)")
        elif action == "escalate":
            escalations.append(t)
        else:
            fixes.append((_lane_of(t, finding), finding))
    for e in as_list(state.get("debris_exemptions")):
        if e not in exemptions:
            exemptions.append(e)
    u.update({"rejected_findings": rejected, "deferred_findings": deferred, "debris_exemptions": exemptions, "debris_exemptions_new": [], "triage": []})

    streak = dict(as_dict(state.get("lane_block_streak")))
    lanes_now = {lane for lane, _ in fixes}
    for lane in list(streak):
        if lane not in lanes_now:
            streak[lane] = 0
    probe_gap = any("inconclusive" in f.lower() for _, f in fixes if _ == "probe")
    if probe_gap:
        fixes = [(l, f) for l, f in fixes if not (l == "probe" and "inconclusive" in f.lower())]
        escalations.append({"finding": "probe INCONCLUSIVE", "action": "escalate", "reason": "environment gap", "kind": "probe_gap"})

    if fixes:
        rounds = as_int(state.get("review_rounds"))
        stuck = []
        for lane in lanes_now:
            streak[lane] = streak.get(lane, 0) + 1
            if lane in RERUN_ONCE_LANES and streak[lane] > RERUN_ONCE:
                stuck.append(lane)
        u["lane_block_streak"] = streak
        if stuck:
            return raise_escalation(state, u, escalation(
                "lane_stuck", f"The {', '.join(stuck)} lane still blocks after one fix cycle on the same class of findings "
                              "(the plan or the approach may be wrong). How to proceed?",
                [option("Abort and report", recommended=True, evidence="protocol: re-run once, then stop and escalate"),
                 option("One more fix round anyway"), option("Accept the findings unresolved and proceed")],
                evidence="\n".join(f"- {f}" for _, f in fixes)[:2500]))
        if rounds >= STRIKE_LIMIT:
            return raise_escalation(state, u, escalation(
                "review_exhausted", f"Still blocked after {STRIKE_LIMIT} review fix rounds. How to proceed?",
                [option("Abort and report", recommended=True), option("One more fix round anyway"),
                 option("Accept the findings unresolved and proceed")],
                evidence="\n".join(f"- {f}" for _, f in fixes)[:2500]))
        u["review_rounds"] = rounds + 1
        u["fix_items"] = [f for _, f in fixes]
        u["fix_source"] = "review"
        ledger(state, u, f"review: round {rounds + 1} fixes {len(fixes)} finding(s) in lanes {sorted(lanes_now)}")
        u["_next"] = "compose_fix"
        return u
    u["lane_block_streak"] = streak
    if escalations:
        e = escalations[0]
        kind = e.get("kind") or ("plan_defect" if re.search(r"plan|criterion", str(e.get("reason", "")) + str(e.get("finding", "")), re.I)
                                 and _lane_of(e, str(e.get("finding", ""))) == "adversary" else "yellow_findings")
        if kind == "probe_gap":
            esc = escalation("probe_gap", "The usage probe is INCONCLUSIVE: the environment, not the code, blocks it. "
                                          "Supply the missing recipe/fixture/mock and re-run, or surface the gap?",
                             [option("Supply it and re-run the probe (say what to provide)"),
                              option("Surface the gap and proceed", recommended=True,
                                     evidence="a consumer-facing change that cannot be exercised locally is itself a finding")],
                             evidence=str(e.get("finding", "")))
        elif kind == "plan_defect":
            esc = escalation("plan_defect", "The adversary reports the PLAN itself as the root cause (impossible or "
                                            "contradictory criterion). Never fixed by changing scope: amend the plan, or stop?",
                             [option("Stop; the plan needs amending (report it)", recommended=True),
                              option("Continue; treat the criterion as amended (state the amendment)")],
                             evidence=str(e.get("finding", "")))
        else:
            esc = escalation("yellow_findings", "A review warning at production rigor: fix it, or accept it as scoped-out?",
                             [option("Fix it", recommended=True), option("Accept it (recorded)")],
                             evidence="\n".join(str(x.get("finding", "")) for x in escalations)[:2500])
        return raise_escalation(state, u, esc)
    ledger(state, u, "review: every blocker rejected with citation or deferred by the quality bar; proceeding")
    u["_next"] = "observability_gate"
    return u


def observability_gate(state):
    u = {}
    note_script_error(state, u, "observability")
    root = state.get("git_root") or state.get("project_dir") or "."
    base = state.get("base_sha") or ""
    try:
        sys.path.insert(0, SHARED_DIR)
        import diff_signals  # noqa: WPS433
        files = changed_files_since(root, base)
        added = added_lines_since(root, base)
        sig = diff_signals.classify(files, added)
        surface = bool(sig.get("consumer_surface") or sig.get("touches_deps")) or bool(OPERATIONAL_RE.search(added))
        u["changed_files"] = files
    except Exception as e:  # advisory lane: degrade, never block
        surface = True
        ledger(state, u, f"observability: signals unavailable ({e}); running the pass to be safe")
    agent = var(state, "observability_agent", "")
    u["obs_inventory_note"] = (f"Spawn `{agent}` for a read-only live inventory of existing alerts/thresholds."
                               if agent else "No observability agent is configured - note in the artifact that the inventory is repo-derived.")
    u["operational_surface"] = surface
    if surface:
        u["_next"] = "observability_pass"
    else:
        u["observability_artifact"] = "## Observability\n\nNo operational surface added by this change (no endpoints, error paths, jobs, queues, deps or metrics); pass skipped."
        u["_next"] = "whetstone_gate"
    return u


def observability_route(state):
    u = {}
    harvest_anomalies(state, u, "observability_pass")
    note_script_error(state, u, "observability_pass")
    brief = (state.get("obs_coder_brief") or "").strip()
    if not (state.get("observability_artifact") or "").strip():
        u["observability_artifact"] = "## Observability\n\n(pass unavailable this run - declared degraded)"
    if brief and as_int(state.get("obs_rounds")) < RERUN_ONCE and as_int(state.get("review_rounds")) < STRIKE_LIMIT:
        u["obs_rounds"] = as_int(state.get("obs_rounds")) + 1
        u["fix_source"] = "observability"
        ledger(state, u, "observability: alert-as-code changes warranted -> one coder round (review gates re-run)")
        u["_next"] = "compose_fix"
    else:
        if brief:
            u["follow_ups"] = list(as_list(state.get("follow_ups"))) + ["Observability: alert-as-code changes recommended but not applied this run: " + tail(brief, 300)]
        u["_next"] = "whetstone_gate"
    return u


def whetstone_gate(state):
    u = {}
    note_script_error(state, u, "whetstone")
    fb = [f for f in as_list(state.get("external_feedback")) if isinstance(f, str) and f.strip()]
    if fb:
        u["_next"] = "run_whetstone"
    else:
        u["whetstone_note"] = "whetstone skipped: no human / third-party review feedback was addressed in this run"
        u["_next"] = "pr_gate"
    return u


# ====================================================================== 2.7
def pr_gate(state):
    u = {}
    note_script_error(state, u, "run_whetstone")
    if (state.get("whetstone_result") or "").startswith("Agent node failed"):
        u["whetstone_note"] = "whetstone errored; noted and finished without it"
    actions = as_dict(state.get("git_actions"))
    wanted = any(truthy(actions.get(k)) for k in ("commit", "push", "open_pr"))
    if not wanted:
        u["_next"] = "final_report"
        return u
    u["report"] = _assemble_report(state, u, pre_publish=True)
    task = {"kind": "publish", "return_to": "ci_gate",
            "commit": truthy(actions.get("commit")) or truthy(actions.get("push")) or truthy(actions.get("open_pr")),
            "push": truthy(actions.get("push")) or truthy(actions.get("open_pr")),
            "open_pr": truthy(actions.get("open_pr")), "base": actions.get("base") or "",
            "draft": True, "flip_ready": False,
            "text": "Commit the task's work (task id in the message when the brief names one)"
                    + (", push the CURRENT branch" if truthy(actions.get("push")) or truthy(actions.get("open_pr")) else "")
                    + (f", open a DRAFT PR" + (f" with --base {actions.get('base')}" if actions.get("base") else "") if truthy(actions.get("open_pr")) else "")
                    + ". Then post one '## Review round N @ <sha>' comment per round listed."}
    u["git_task"] = task
    u["_next"] = "check_inbox_final" if truthy(state.get("parallel_mode")) else "git_ops"
    return u


def ci_gate(state):
    u = {}
    note_script_error(state, u, "git_ops")
    # A10: check_inbox_final's schema `anomalies` survives in state through the
    # schema-less git_ops node; harvest it here (first gate downstream).
    harvest_anomalies(state, u, "check_inbox_final")
    # A10 line convention for the free-text git-master island: harvest
    # "ANOMALY: ..." lines from its report (see the node's instructions).
    report = state.get("git_report") or ""
    found = [m.strip() for m in re.findall(r"^ANOMALY:\s*(.+)$", report, re.M) if m.strip()]
    if found:
        u["anomalies"] = found
        harvest_anomalies({**state, "anomalies": found}, u, "git_ops")
        u["anomalies"] = []
    task = as_dict(state.get("git_task"))
    ret = task.get("return_to") or "final_report"
    if task.get("kind") == "flip_ready":
        u["_next"] = "final_report"
        return u
    if ret == "run_coder":
        u["git_task"] = {}
        u["_next"] = "run_coder"
        return u
    if ret != "ci_gate":
        u["_next"] = "final_report"
        return u
    owns = task.get("open_pr") and not truthy(state.get("delegated"))
    u["ci_owner"] = bool(owns)
    if owns and re.search(r"github\.com/[^\s]+/pull/\d+|#\d+", state.get("git_report") or ""):
        u.update({"ci_polls": 0, "ci_poll_fails": 0, "ci_started_at": int(time.time())})
        u["_next"] = "ci_poll"
    else:
        if task.get("open_pr") and not owns:
            ledger(state, u, "ci: PR lifecycle owned by the parent orchestrator; left in draft")
        u["_next"] = "final_report"
    return u


def _pr_ref(state):
    rep = state.get("git_report") or ""
    if m := re.search(r"https://github\.com/[^\s)]+/pull/(\d+)", rep):
        return m.group(1)
    if m := re.search(r"#(\d+)", rep):
        return m.group(1)
    return ""


def ci_poll(state):
    u = {}
    polls = as_int(state.get("ci_polls"))
    started = as_int(state.get("ci_started_at")) or int(time.time())
    if polls > 0:
        time.sleep(int(os.environ.get("SISYPHUS_CI_SLEEP", CI_POLL_SLEEP)))
    u["ci_polls"] = polls + 1
    if time.time() - started > CI_WALL_SECS:
        u["ci_status"] = "timeout"
        u["_next"] = "ci_route"
        return u
    pr = _pr_ref(state)
    root = state.get("git_root") or state.get("project_dir") or "."
    cmd = os.environ.get("SISYPHUS_GH_CHECKS_CMD")
    try:
        if cmd:
            p = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=90)
        else:
            p = subprocess.run(["gh", "pr", "checks", pr, "--json", "name,state,bucket,link"], cwd=root,
                               capture_output=True, text=True, timeout=90)
        data = json.loads(p.stdout) if p.returncode == 0 and p.stdout.strip() else None
    except Exception:  # noqa: BLE001
        data = None
    if data is None:
        fails = as_int(state.get("ci_poll_fails")) + 1
        u["ci_poll_fails"] = fails
        if fails >= CI_POLL_FAILS:
            u["ci_status"] = "poll_error"
            u["_next"] = "ci_route"
        else:
            u["_next"] = "ci_poll"
        return u
    u["ci_poll_fails"] = 0
    buckets = [(c.get("name", ""), (c.get("bucket") or c.get("state") or "").lower(), c.get("link", "")) for c in data]
    failing = [(n, l) for n, b, l in buckets if b in ("fail", "failure", "cancelled")]
    pending = [n for n, b, _ in buckets if b in ("pending", "queued", "in_progress", "")]
    u["ci_checks"] = buckets
    if failing:
        u["ci_status"] = "red"
        u["ci_failing"] = failing
        u["_next"] = "ci_route"
    elif pending:
        u["ci_status"] = "pending"
        u["_next"] = "ci_poll"
    else:
        u["ci_status"] = "green"
        u["_next"] = "ci_route"
    return u


def ci_route(state):
    u = {}
    note_script_error(state, u, "ci_poll")
    status = state.get("ci_status") or "poll_error"
    if status == "green":
        u["git_task"] = {"kind": "flip_ready", "return_to": "final_report", "flip_ready": True,
                         "text": "CI is green and the verification loop is complete: flip the PR to ready (gh pr ready)."}
        u["flip_ready"] = True
        ledger(state, u, "ci: green -> flip to ready")
        u["_next"] = "git_ops"
    elif status == "red":
        root = state.get("git_root") or state.get("project_dir") or "."
        pack = []
        for name, link in as_list(state.get("ci_failing")):
            pack.append(f"### {name}\n{link}")
            if m := re.search(r"/actions/runs/(\d+)", link or ""):
                try:
                    p = subprocess.run(["gh", "run", "view", m.group(1), "--log-failed"], cwd=root,
                                       capture_output=True, text=True, timeout=120)
                    pack.append(tail(p.stdout or p.stderr, 4000))
                except Exception as e:  # noqa: BLE001
                    pack.append(f"(log unavailable: {e})")
        u["ci_evidence"] = tail("\n".join(pack), 12000)
        u["_next"] = "ci_diagnose"
    else:
        return raise_escalation(state, u, escalation(
            "ci_exhausted", f"CI watch ended without a verdict ({status}). Keep waiting, or leave the PR in draft and report?",
            [option("Keep waiting (re-run the watch)"), option("Leave the PR in draft and report", recommended=True,
                                                               evidence="a draft with a named blocker beats a ready PR on red/pending")]))
    return u


def ci_apply(state):
    u = {}
    harvest_anomalies(state, u, "ci_diagnose")
    note_script_error(state, u, "ci_diagnose")
    verdict = state.get("ci_verdict") or "unsure"
    checks = [c for c in as_list(state.get("failing_checks")) if isinstance(c, str)] or [n for n, _ in as_list(state.get("ci_failing"))]
    reruns = dict(as_dict(state.get("ci_reruns")))
    if verdict == "flake":
        exhausted = [c for c in checks if as_int(reruns.get(c)) >= CI_FLAKE_RERUNS]
        if exhausted:
            verdict = "real"
            ledger(state, u, f"ci: flake re-run budget exhausted for {exhausted}; a third red is real by definition")
        else:
            root = state.get("git_root") or state.get("project_dir") or "."
            for name, link in as_list(state.get("ci_failing")):
                reruns[name] = as_int(reruns.get(name)) + 1
                if m := re.search(r"/actions/runs/(\d+)", link or ""):
                    subprocess.run(["gh", "run", "rerun", m.group(1), "--failed"], cwd=root, capture_output=True, text=True, timeout=60)
                ledger(state, u, f"flake-rerun: {name} (re-run {reruns[name]}/{CI_FLAKE_RERUNS})")
            u["ci_reruns"] = reruns
            u.update({"ci_polls": 0, "ci_poll_fails": 0})
            u["_next"] = "ci_poll"
            return u
    if verdict == "real":
        strikes = as_int(state.get("ci_fix_strikes")) + 1
        u["ci_fix_strikes"] = strikes
        if strikes > STRIKE_LIMIT:
            return raise_escalation(state, u, escalation(
                "ci_exhausted", f"CI still red after {STRIKE_LIMIT} fix rounds. How to proceed?",
                [option("Resume with one more coder fix round"), option("Leave the PR in draft and report", recommended=True)],
                evidence=tail(state.get("ci_evidence", ""), 2000)))
        u["fix_source"] = "ci"
        u["_next"] = "compose_fix"
        return u
    if verdict == "infra":
        return raise_escalation(state, u, escalation(
            "ci_infra", "CI failed for infrastructure reasons (runner lost / timeout / 429 / cancelled). Wait and re-watch, or leave in draft?",
            [option("Keep waiting (re-run the watch)", recommended=True, evidence=tail(state.get("ci_diagnosis", ""), 300)),
             option("Leave the PR in draft and report")]))
    return raise_escalation(state, u, escalation(
        "ci_unsure", "CI diagnosis is unsure (unsure defaults up, never to a re-run). How to proceed?",
        [option("Re-run the failing checks once"), option("Resume with a coder fix round"),
         option("Leave the PR in draft and report", recommended=True)],
        evidence=tail(state.get("ci_evidence", ""), 2000)))


def _assemble_report(state, u, pre_publish=False):
    g = lambda k, d="": (u.get(k) if k in u else state.get(k, d)) or d  # noqa: E731
    root = state.get("git_root") or state.get("project_dir") or "."
    head = git_out(root, "rev-parse", "HEAD", default=g("head_sha"))
    u["head_sha"] = head
    lines = [f"# Sisyphus run report", "",
             f"Sisyphus run id: {g('run_id')}", f"Run directory: {g('run_dir')}",
             f"Project: {g('project_dir')}", f"Base SHA: {g('base_sha')}   HEAD SHA: {head}",
             f"Intent: {g('intent')}  sizing: {g('sizing')}  review cadence: {var(state, 'review_cadence', 'task')}"
             f"  escalation policy: {var(state, 'escalation_policy', 'interactive')}", ""]
    subs = as_list(g("subtasks", []))
    if len(subs) > 1:
        lines += ["## Subtasks (A11)"] + [f"- {i}. {s.get('title', '') if isinstance(s, dict) else s}" for i, s in enumerate(subs, 1)] + [""]
    lines += ["## Verification evidence"]
    cr = g("coder_result")
    if cr:
        sent = g("coder_sentinel") or "?"
        lines += [f"- coder: {sent}" + (f" (fix rounds: {as_int(g('fix_round', 0))})" if as_int(g("fix_round", 0)) else "")]
        for l in cr.splitlines():
            if l.startswith(("Build:", "Tests:", "Files modified:", "Files created:")):
                lines.append(f"  - {l.strip()}")
    if g("review_skipped_note"):
        lines.append(f"- review: {g('review_skipped_note')}")
    verdict = g("gauntlet_verdict")
    if verdict:
        lanes = as_dict(g("gauntlet_lanes", {}))
        lines.append(f"- review gauntlet: {verdict} after {len(as_list(g('round_heads', [])))} round(s)"
                     + (f"; lanes: " + ", ".join(f"{k}={v}" for k, v in lanes.items()) if lanes else ""))
        for r in as_list(g("round_heads", [])):
            if isinstance(r, dict):
                details = as_dict(r.get("lane_details"))
                lines.append(f"  - round {r.get('round')} @ {r.get('head')}: {r.get('verdict', '(running)')}"
                             + (" [" + "; ".join(f"{k}: {v}" for k, v in details.items()) + "]" if details else ""))
    if g("ci_status"):
        lines.append(f"- CI: {g('ci_status')}" + (" (flipped to ready)" if truthy(g("flip_ready", False)) else ""))
    lines.append("")
    digest = as_list(g("autonomous_digest", []))
    if digest:
        lines += ["## Decisions taken autonomously"] + [f"- {d}" for d in digest] + [""]
    skipped = as_list(g("skipped_reviews", []))
    if skipped:
        lines += ["## Skipped reviews (NEEDS-HUMAN)"] + [f"- {s}" for s in skipped] + [""]
    if g("gauntlet_attention"):
        lines += ["## Human attention required", g("gauntlet_attention"), ""]
    rej = as_list(g("rejected_findings", []))
    if rej:
        lines += ["## Review decisions"] + [f"- {r}" for r in rej] + [""]
    debt = as_list(g("debt_left_behind", []))
    lines += ["Debt left behind:"] + ([f"- {d}" for d in debt] or ["- none"]) + [""]
    ex = as_list(g("debris_exemptions", []))
    lines += ["Debris exemptions:"] + ([f"- {e}" for e in ex] or ["- none"]) + [""]
    dd = as_list(g("design_decisions", []))
    lines += ["Design decisions:"] + ([f"- {d}" for d in dd] or ["- none"]) + [""]
    if g("observability_artifact"):
        lines += [g("observability_artifact"), ""]
    fus = list(as_list(g("follow_ups", []))) + list(as_list(g("deferred_findings", [])))
    if g("whetstone_note"):
        fus.append(g("whetstone_note"))
    lines += ["FOLLOW-UPS:"] + ([f"- {f}" for f in fus] or ["none"]) + [""]
    anomalies = as_list(g("anomalies_log", []))
    if anomalies:
        lines += ["## Anomalies (observed outside this task's scope)"] + [f"- {a}" for a in anomalies] + [""]
    if g("whetstone_result") and not str(g("whetstone_result")).startswith("Agent node failed"):
        lines += ["## Whetstone", tail(g("whetstone_result"), 2000), ""]
    if not pre_publish and g("git_report"):
        lines += ["## Git / PR", tail(g("git_report"), 3000), ""]
    if g("failure_reason"):
        lines += ["## Stop reason", g("failure_reason"), ""]
    return "\n".join(lines)


def final_report(state):
    u = {}
    harvest_anomalies(state, u, "final")
    note_script_error(state, u, "final")
    if state.get("intent") == "plan_execution" and (state.get("report") or "").strip():
        step = state["report"]
        u["report"] = step + f"\n\nSisyphus run id: {state.get('run_id')}"
        write_artifact(state.get("run_dir", ""), "report.md", u["report"])
        return u
    u["report"] = _assemble_report(state, u)
    write_artifact(state.get("run_dir", ""), "report.md", u["report"])
    snapshot(state, u, "done")
    if OWNER_REMOTE_INPUT and truthy(state.get("top_level")) and truthy(state.get("owner_away")):
        binary = os.environ.get("SISYPHUS_SLACK_PING_BIN", "slack_ping")
        try:
            subprocess.run([binary, "--text", f"sisyphus run finished: {state.get('intent')} {state.get('gauntlet_verdict') or ''} "
                                              f"(run {state.get('run_id')})"], capture_output=True, text=True, timeout=60)
        except Exception:  # noqa: BLE001
            pass
    return u


# ====================================================================== entry
GATES = {
    "intake": intake, "assess_fault": assess_fault, "route_intent": route_intent,
    "record_subtasks": record_subtasks, "next_subtask": next_subtask,
    "research_dispatch": research_dispatch, "explore_fault": explore_fault, "librarian_fault": librarian_fault,
    "write_context_pack": write_context_pack, "oracle_gate": oracle_gate, "merge_oracle": merge_oracle,
    "design_gate": design_gate, "escalate_gate": escalate_gate, "ladder_route": ladder_route,
    "consult_route": consult_route, "ask_dispatch": ask_dispatch, "ask_owner_poll": ask_owner_poll,
    "ask_owner_route": ask_owner_route, "parse_answer": parse_answer, "apply_answer": apply_answer,
    "scope_check": scope_check, "render_coder_prompt": render_coder_prompt, "inbox_route": inbox_route,
    "route_coder_result": route_coder_result, "compose_fix": compose_fix, "strikeout": strikeout,
    "strikeout_route": strikeout_route, "bookkeeping_scan": bookkeeping_scan,
    "bookkeeping_route": bookkeeping_route, "review_gate": review_gate,
    "build_gauntlet_prompt": build_gauntlet_prompt, "route_gauntlet": route_gauntlet,
    "triage_route": triage_route, "observability_gate": observability_gate,
    "observability_route": observability_route, "whetstone_gate": whetstone_gate, "pr_gate": pr_gate,
    "ci_gate": ci_gate, "ci_poll": ci_poll, "ci_route": ci_route, "ci_apply": ci_apply,
    "final_report": final_report,
}


def run(name):
    state = load_state()
    emit(GATES[name](state))
