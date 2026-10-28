#!/usr/bin/env python3
"""Behavioral smoke battery for the sisyphus-next gates.

Runs every gate in-process against crafted states - both sides of every named
bound, every fail-open / fallback path, the sentinel parsers, the
escalation ladder under both policies, re-entry (A2) and the anomalies harvest
(A10) - inside a scratch git repo and a scratch runs root. Also exercises the
GRAPH_STATE env pattern through the real entry scripts (subprocess) for a
sample of nodes, including the debt_scan shim's fail-open default.

Usage: smoke_test.py   (exit 1 on any failure; prints one line per check)
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.dont_write_bytecode = True
sys.path.insert(0, HERE)

TMP = tempfile.mkdtemp(prefix="sx-smoke-")
os.environ["SISYPHUS_RUNS_ROOT"] = os.path.join(TMP, "runs")
os.environ["HOME"] = os.path.join(TMP, "home")  # no real away file / rulings file
os.makedirs(os.environ["HOME"], exist_ok=True)

import _lib  # noqa: E402
import gates  # noqa: E402
from _lib import (CI_FLAKE_RERUNS, CLARIFY_MAX, REASK_MAX, RERUN_ONCE, STRIKE_LIMIT, SUBTASK_MAX,  # noqa: E402
                  option, escalation)

RESULTS = []


def check(name, cond, detail=""):
    RESULTS.append((name, bool(cond), detail))
    print(("ok   " if cond else "FAIL ") + name + (f"  ({detail})" if detail and not cond else ""))


def git(*a):
    subprocess.run(["git", "-C", REPO, *a], check=True, capture_output=True,
                   env={**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
                        "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"})


REPO = os.path.join(TMP, "repo")
os.makedirs(REPO)
git("init", "-q")
open(os.path.join(REPO, "README.md"), "w").write("# demo\n")
git("add", ".")
git("commit", "-q", "-m", "init")
BASE = subprocess.run(["git", "-C", REPO, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()


def base_state(**over):
    s = {"project_dir": REPO, "git_root": REPO, "base_sha": BASE, "head_sha": BASE, "run_id": "x",
         "run_dir": os.path.join(os.environ["SISYPHUS_RUNS_ROOT"], "x"), "decisions": [], "anomalies": [],
         "anomalies_log": [], "escalation_policy": "interactive", "review_cadence": "task", "parallel": "0",
         "current_task": "add a hello endpoint", "brief": "add a hello endpoint"}
    s.update(over)
    return s


# ---------------------------------------------------------------- run dir resolution
_saved_env = {k: os.environ.get(k) for k in ("SISYPHUS_RUNS_ROOT", "COYOTE_CACHE_DIR", "XDG_CACHE_HOME")}
for _k in _saved_env:
    os.environ.pop(_k, None)
os.environ["COYOTE_CACHE_DIR"] = os.path.join(TMP, "cache")
check("runs_root: COYOTE_CACHE_DIR/sisyphus-next/runs when no override is set",
      _lib.runs_root() == os.path.join(TMP, "cache", "sisyphus-next", "runs"), _lib.runs_root())
os.environ.pop("COYOTE_CACHE_DIR")
os.environ["XDG_CACHE_HOME"] = os.path.join(TMP, "xdg")
check("runs_root: XDG_CACHE_HOME/coyote/sisyphus-next/runs when COYOTE_CACHE_DIR is unset",
      _lib.runs_root() == os.path.join(TMP, "xdg", "coyote", "sisyphus-next", "runs"), _lib.runs_root())
os.environ.pop("XDG_CACHE_HOME")
_platform_root = _lib.runs_root()
check("runs_root: platform cache dir/coyote/sisyphus-next/runs when nothing is set",
      os.path.isabs(_platform_root) and (os.name == "nt" or _platform_root.startswith(os.environ["HOME"]))
      and _platform_root.endswith(os.path.join("coyote", "sisyphus-next", "runs")), _platform_root)
os.environ["SISYPHUS_RUNS_ROOT"] = os.path.join(TMP, "override")
check("runs_root: SISYPHUS_RUNS_ROOT wins over every cache dir", _lib.runs_root() == os.path.join(TMP, "override"))
for _k, _v in _saved_env.items():
    if _v is None:
        os.environ.pop(_k, None)
    else:
        os.environ[_k] = _v
check("runs_root: battery env restored", _lib.runs_root() == os.environ["SISYPHUS_RUNS_ROOT"])


# ---------------------------------------------------------------- intake / re-entry (A2)
st = gates.intake({"initial_prompt": "Implement TASK-007 (hello) in the project. ## SOURCE OF TRUTH plans/x.md\nQuality bar: rigor=prototype, surfaces=http",
                   "project_dir": REPO})
check("intake: base sha + run dir + delegated + quality bar", st["base_sha"] == BASE and os.path.isdir(st["run_dir"])
      and st["delegated"] and st["quality_rigor"] == "prototype" and st["quality_surfaces"] == "http")
check("intake: brief.md + state.json written", os.path.exists(os.path.join(st["run_dir"], "brief.md"))
      and os.path.exists(os.path.join(st["run_dir"], "state.json")))
rid = st["run_id"]
st2 = gates.intake({"initial_prompt": f"Fix these plan-conformance failures: X\nSisyphus run id: {rid}", "project_dir": REPO})
check("intake: re-entry loads the prior run (A2)", st2["resume_mode"] is True and st2["run_id"] == rid and "Re-entry" in st2["resume_note"])
st3 = gates.intake({"initial_prompt": "do it\nSisyphus run id: 20200101T000000-deadbeef", "project_dir": REPO})
check("intake: unknown run id -> fresh run, declared", not st3.get("resume_mode") and "no durable state" in st3["resume_note"])
st4 = gates.intake({"initial_prompt": "typo fix", "project_dir": REPO})
check("intake: standalone brief is top-level", st4["top_level"] and not st4["delegated"])
roster, own = gates._parse_roster("Implement TASK-003 ...\nTeammate roster:\n- TASK-002 agent_x scope: src/auth/, src/api/users.go\n- TASK-003 agent_y scope: src/payments/\n")
check("intake: roster parse", roster.get("TASK-002") == ["src/auth/", "src/api/users.go"] and own == ["src/payments/"])

# ---------------------------------------------------------------- assess_fault / route_intent (B1)
u = gates.assess_fault(base_state(last_script_error="boom"))
check("assess_fault: fail-safe -> ambiguous with options", u["intent"] == "ambiguous" and u["clarifying_questions"])
u = gates.route_intent(base_state(intent="ambiguous", clarifying_questions=[{"question": "which module?", "options": [{"text": "a", "recommended": True, "evidence": "e"}]}], clarify_rounds=0))
check("route_intent: ambiguous -> escalation clarify (round 1)", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "clarify" and u["clarify_rounds"] == 1)
u = gates.route_intent(base_state(intent="ambiguous", clarifying_questions=[{"question": "q"}], clarify_rounds=CLARIFY_MAX))
check("route_intent: B1 clarify bound -> end_blocked", u["_next"] == "end_blocked")
for intent, sizing, exp in [("exploration", "n/a", "research_answer"), ("plan_authoring", "n/a", "end_redirect"), ("plan_execution", "n/a", "run_step"),
                            ("implementation", "plan_shaped", "end_redirect"), ("implementation", "compound", "plan_subtasks"),
                            ("implementation", "simple", "next_subtask"), ("trivial", "simple", "next_subtask")]:
    u = gates.route_intent(base_state(intent=intent, sizing=sizing))
    check(f"route_intent: {intent}/{sizing} -> {exp}", u["_next"] == exp)
u = gates.route_intent(base_state(intent="implementation", sizing="simple", architecture_scale=True))
check("route_intent: architecture-scale -> architecture_review", u["_next"] == "architecture_review")
u = gates.route_intent(base_state(intent="implementation", sizing="simple", review_cadence="batch", batch_plan_defect="criteria not executable"))
check("route_intent: batch plan defect -> end_blocked", u["_next"] == "end_blocked")
u = gates.route_intent(base_state(intent="implementation", resume_mode=True, resume_action="fix"))
check("route_intent: re-entry fix -> compose_fix", u["_next"] == "compose_fix" and u["fix_source"] == "resume")
u = gates.route_intent(base_state(intent="implementation", resume_mode=True, resume_action="git_ops"))
check("route_intent: re-entry git_ops -> git_ops", u["_next"] == "git_ops" and u["git_task"]["return_to"] == "final_report")
u = gates.route_intent(base_state(intent="implementation", sizing="simple", anomalies=["stale plan downstream"]))
check("A10: anomalies harvested into anomalies_log and reset", u["anomalies_log"] == ["assess: stale plan downstream"] and u["anomalies"] == [])

# ---------------------------------------------------------------- subtasks (B2/B3)
subs = [{"title": f"s{i}", "outcome": f"o{i}", "acceptance_criteria": ["c"]} for i in range(SUBTASK_MAX)]
u = gates.record_subtasks(base_state(subtasks=subs))
check("record_subtasks: <=3 recorded", u["_next"] == "next_subtask" and os.path.exists(os.path.join(base_state()["run_dir"], "tasklog.md")))
u = gates.record_subtasks(base_state(subtasks=subs + [{"title": "s4", "outcome": "o", "acceptance_criteria": []}]))
check("record_subtasks: B2 >3 -> end_redirect (plan-shaped)", u["_next"] == "end_redirect")
u = gates.record_subtasks(base_state(subtasks=[], plan_shaped_reason="cross-PR"))
check("record_subtasks: empty -> end_redirect with reason", u["_next"] == "end_redirect" and "cross-PR" in u["redirect_reason"])
u = gates.next_subtask(base_state(subtasks=subs, subtask_index=0, intent="implementation"))
check("next_subtask: first -> plan_research, counters reset", u["_next"] == "plan_research" and u["fix_strikes"] == 0 and u["task_number"] == 1 and "Subtask 1/3" in u["current_task"])
u = gates.next_subtask(base_state(subtasks=subs, subtask_index=SUBTASK_MAX))
check("next_subtask: B3 exhausted -> final_report", u["_next"] == "final_report")
u = gates.next_subtask(base_state(subtasks=subs[:1], subtask_index=0, intent="trivial"))
check("next_subtask: trivial skips research -> compose_brief", u["_next"] == "compose_brief")

# ---------------------------------------------------------------- research
u = gates.research_dispatch(base_state(explore_slices=["a", "b"], team_mode=True))
check("research_dispatch: team mode -> team map", u["_next"] == "explore_fanout_team")
u = gates.research_dispatch(base_state(explore_slices=["a"], team_mode=True))
check("research_dispatch: single slice never a team", u["_next"] == "explore_fanout_solo")
u = gates.research_dispatch(base_state(explore_slices=[], last_script_error="llm died"))
check("research_dispatch: fail-open degraded single slice", u["_next"] == "explore_fanout_solo" and len(u["explore_slices"]) == 1 and u["last_script_error"] == "")
u = gates.research_dispatch(base_state(explore_slices=[], last_node_output="llm node 'plan_research' failed: rate limit"))
check("research_dispatch: llm fallback reason (last_node_output) ledgered + reset", u["last_node_output"] == "" and any("rate limit" in d for d in u["decisions"]))
u = gates.research_dispatch(base_state(explore_slices=["a"], last_node_output='{"explore_slices": ["a"]}'))
check("note_script_error: a successful llm JSON output is not mistaken for a fault", "last_node_output" not in u)
u = gates.explore_fault(base_state(finding="Agent node failed: timeout", slice="s"))
check("explore_fault: PIPELINE-FAULT finding (visible degradation)", u["finding"].startswith("PIPELINE-FAULT"))
u = gates.write_context_pack(base_state(context_pack="", explore_findings=["f1"], librarian_findings=["l1"]))
check("write_context_pack: degraded pack from raw findings, declared", u["context_pack_degraded"] and "DEGRADED" in u["context_pack"] and "f1" in u["context_pack"])
u = gates.write_context_pack(base_state(context_pack="pack"))
check("write_context_pack: persisted", open(os.path.join(base_state()["run_dir"], "context-pack.md")).read().startswith("pack"))
u = gates.oracle_gate(base_state(needs_oracle=True, oracle_consulted=False))
check("oracle_gate: trigger -> consult once", u["_next"] == "consult_oracle" and u["oracle_consulted"])
u = gates.oracle_gate(base_state(needs_oracle=True, oracle_consulted=True))
check("oracle_gate: B4 already consulted -> design_gate", u["_next"] == "design_gate")
u = gates.oracle_gate(base_state(needs_oracle=False, assess_needs_oracle=True, assess_oracle_question="how?"))
check("oracle_gate: assess trigger survives synth overwrite", u["_next"] == "consult_oracle" and u["oracle_question"] == "how?")
u = gates.merge_oracle(base_state(oracle_result="Agent node failed: x"))
check("merge_oracle: failed consult -> escalation oracle_fault (never continue silently)", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "oracle_fault")
u = gates.merge_oracle(base_state(oracle_result="Use pattern X", context_pack="pack"))
check("merge_oracle: appended", u["_next"] == "design_gate" and "Oracle direction" in u["context_pack"])
u = gates.design_gate(base_state(design_options=[{"text": "A", "recommended": True, "evidence": "e"}, {"text": "B"}]))
check("design_gate: options -> design_choice escalation; options consumed", u["_next"] == "escalate_gate" and u["design_options"] == [] and len(u["escalation"]["options"]) == 2)
u = gates.design_gate(base_state(register_question="I see X and Y. Which?"))
check("design_gate: register question -> escalation", u["escalation"]["kind"] == "design_choice")
u = gates.design_gate(base_state(architecture_proposal="proposal"))
check("design_gate: proposal -> adopt? once", u["_next"] == "escalate_gate" and u["proposal_confirmed"])
u = gates.design_gate(base_state(architecture_proposal="proposal", proposal_confirmed=True))
check("design_gate: nothing pending -> compose_brief", u["_next"] == "compose_brief")

# ---------------------------------------------------------------- ladder
esc = escalation("design_choice", "pick", [option("A", True, "repo convention src/x.go:12"), option("B")])
u = gates.escalate_gate(base_state(escalation=esc, escalation_policy="interactive"))
check("escalate_gate: interactive -> tier 4", u["_next"] == "ask_dispatch")
u = gates.escalate_gate(base_state(escalation=esc, escalation_policy="autonomous"))
check("escalate_gate: autonomous -> ladder_judge with rulings text", u["_next"] == "ladder_judge" and u["standing_rulings"])
hard = escalation("design_ask", "Design question: adopt a Strategy?", [], hard_trigger=True)
u = gates.escalate_gate(base_state(escalation=hard, escalation_policy="autonomous"))
check("escalate_gate: hard trigger bypasses the ladder", u["_next"] == "ask_dispatch")
u = gates.escalate_gate(base_state(escalation=esc, escalation_policy="autonomous", autonomous_kinds=["design_choice"]))
check("escalate_gate: a kind already resolved autonomously -> owner (failed-once rule)", u["_next"] == "ask_dispatch")
secret = escalation("yellow_findings", "rotate the API secret?", [option("yes", True)])
check("escalation: secret/money wording auto-flags hard trigger", secret["hard_trigger"])
u = gates.ladder_route(base_state(escalation=esc, ladder_resolution="recommended", ladder_choice="A", ladder_basis="recommended option"))
check("ladder_route: recommended -> digest + apply", u["_next"] == "apply_answer" and u["autonomous_digest"] and u["last_answer"] == "A")
u = gates.ladder_route(base_state(escalation=esc, ladder_resolution="recommended", ladder_choice="Z"))
check("ladder_route: recommended but no matching option -> single recommended option taken", u["_next"] == "apply_answer" and u["last_answer"] == "A")
u = gates.ladder_route(base_state(escalation=esc, ladder_resolution="consult"))
check("ladder_route: consult -> navigator", u["_next"] == "consult_navigator")
u = gates.ladder_route(base_state(escalation=escalation("x", "q", [], tier3_ok=False), ladder_resolution="consult"))
check("ladder_route: consult refused when tier3 not ok -> owner", u["_next"] == "ask_dispatch")
u = gates.ladder_route(base_state(escalation=esc, ladder_resolution="escalate"))
check("ladder_route: escalate -> owner", u["_next"] == "ask_dispatch")
u = gates.consult_route(base_state(escalation=esc, consult_result="...\nCONSULT: ANSWER 2"))
check("consult_route: answer mapped by number", u["_next"] == "apply_answer" and u["last_answer"] == "B")
u = gates.consult_route(base_state(escalation=esc, consult_result="CONSULT: DEFER owner's taste"))
check("consult_route: defer -> owner", u["_next"] == "ask_dispatch")
u = gates.consult_route(base_state(escalation=esc, consult_result="Agent node failed: timeout"))
check("consult_route: A5 timeout -> tier 4", u["_next"] == "ask_dispatch")
u = gates.ask_dispatch(base_state(escalation=esc, top_level=True, owner_away=False))
check("ask_dispatch: terminal prompt + parked", u["_next"] == "ask_user" and u["parked"] == "design_choice")
u = gates.ask_dispatch(base_state(escalation=escalation("review_incomplete", "q"), top_level=True))
check("ask_dispatch: review_incomplete -> approval node", u["_next"] == "ask_review_incomplete")
u = gates.ask_dispatch(base_state(escalation=esc, top_level=True, owner_away=True))
check("ask_dispatch: away + top-level -> ask_owner when the fork provides it", u["_next"] == ("ask_owner_poll" if gates.OWNER_REMOTE_INPUT else "ask_user"))
u = gates.ask_dispatch(base_state(escalation=esc, top_level=False, owner_away=True))
check("ask_dispatch: subagent never pings/polls the owner", u["_next"] == "ask_user")
fake = os.path.join(TMP, "ask_owner")
open(fake, "w").write("#!/bin/sh\necho 'PENDING: waiting'\n")
os.chmod(fake, 0o755)
os.environ["SISYPHUS_ASK_OWNER_BIN"] = fake
u = gates.ask_owner_poll(base_state(escalation=esc, owner_polls=0))
check("ask_owner_poll: invokes the tool, counts the poll", u["owner_polls"] == 1 and "PENDING" in u["owner_poll_result"])
u = gates.ask_owner_route(base_state(escalation=esc, owner_poll_result="PENDING", owner_polls=1))
check("ask_owner_route: PENDING within budget -> poll again", u["_next"] == "ask_owner_poll")
u = gates.ask_owner_route(base_state(escalation=esc, owner_poll_result="PENDING", owner_polls=2))
check("ask_owner_route: B13 budget spent -> terminal prompt", u["_next"] == "ask_user")
u = gates.ask_owner_route(base_state(escalation=esc, owner_poll_result="ANSWERED: 2"))
check("ask_owner_route: ANSWERED -> parse_answer", u["_next"] == "parse_answer" and u["last_answer_text"] == "2")
u = gates.ask_owner_route(base_state(escalation=esc, owner_poll_result="TIMEOUT"))
check("ask_owner_route: TIMEOUT -> terminal prompt (a timeout is never an answer)", u["_next"] == "ask_user")
os.environ["SISYPHUS_ASK_OWNER_BIN"] = "/nonexistent/ask_owner"
u = gates.ask_owner_poll(base_state(escalation=esc, owner_polls=0))
check("ask_owner_poll: missing tool -> NOT_DELIVERED (fail-open)", "NOT_DELIVERED" in u["owner_poll_result"])
u = gates.parse_answer(base_state(escalation=esc, last_answer_text="2"))
check("parse_answer: number -> option, parked cleared", u["last_answer"] == "B" and u["parked"] == "" and u["_next"] == "apply_answer")
u = gates.parse_answer(base_state(escalation=esc, last_answer_text="recommended"))
check("parse_answer: 'recommended' -> the recommended option", u["last_answer"] == "A")
u = gates.parse_answer(base_state(escalation=esc, last_answer_text="garbage", reask_rounds=0))
check("parse_answer: unparseable -> re-ask (round 1)", u["_next"] == "ask_user" and u["reask_rounds"] == 1)
u = gates.parse_answer(base_state(escalation=esc, last_answer_text="garbage", reask_rounds=REASK_MAX))
check("parse_answer: B14 exhausted -> free text stands", u["_next"] == "apply_answer" and u["last_answer"] == "garbage")
u = gates.parse_answer(base_state(escalation=escalation("clarify", "q", []), last_answer_text="the auth module"))
check("parse_answer: no options -> free text", u["last_answer"] == "the auth module")

# ---------------------------------------------------------------- apply_answer continuations
def aa(kind, answer, **over):
    return gates.apply_answer(base_state(escalation=escalation(kind, "q", []), last_answer=answer, **over))

check("apply: clarify -> assess with Q/A", aa("clarify", "auth")["_next"] == "assess")
check("apply: design_choice -> compose_brief", aa("design_choice", "A")["_next"] == "compose_brief")
check("apply: oracle_fault retry (within once) -> consult", aa("oracle_fault", "Retry the consult", oracle_retries=0)["_next"] == "consult_oracle")
check("apply: oracle_fault retry exhausted -> design_gate", aa("oracle_fault", "Retry the consult", oracle_retries=RERUN_ONCE)["_next"] == "design_gate")
check("apply: oracle_fault abort -> end_aborted", aa("oracle_fault", "Abort this task")["_next"] == "end_aborted")
check("apply: coder_rejected -> compose_fix", aa("coder_rejected", "use cookies")["_next"] == "compose_fix")
check("apply: engine_fault retry -> compose_fix", aa("engine_fault", "Retry the coder round")["_next"] == "compose_fix")
u = aa("design_ask", "Adopt the pattern", design_ask_rounds=0)
check("apply: design_ask adopt -> one coder round + design-decision ledgered", u["_next"] == "compose_fix" and u["design_ask_rounds"] == 1 and u["design_decisions"])
check("apply: design_ask adopt B9 exhausted -> review_gate", aa("design_ask", "Adopt the pattern", design_ask_rounds=RERUN_ONCE)["_next"] == "review_gate")
check("apply: design_ask keep -> review_gate", aa("design_ask", "Keep the code as shipped")["_next"] == "review_gate")
check("apply: yellow fix -> compose_fix", aa("yellow_findings", "Fix it")["_next"] == "compose_fix")
check("apply: yellow accept -> observability + follow-up", aa("yellow_findings", "Accept it")["_next"] == "observability_gate")
check("apply: plan_defect stop -> end_blocked", aa("plan_defect", "Stop; the plan needs amending")["_next"] == "end_blocked")
u = aa("plan_defect", "Continue; treat the criterion as amended: X", spec_text="old")
check("apply: plan_defect continue -> amended spec + re-review", u["_next"] == "build_gauntlet_prompt" and "AMENDED" in u["spec_text"])
check("apply: probe_gap supply -> compose_fix", aa("probe_gap", "Supply it and re-run")["_next"] == "compose_fix")
check("apply: probe_gap surface -> observability + follow-up", aa("probe_gap", "Surface the gap and proceed")["follow_ups"])
check("apply: review_incomplete retry -> fresh gauntlet", aa("review_incomplete", "Retry the review again")["_next"] == "build_gauntlet_prompt")
u = aa("review_incomplete", "Accept NEEDS-HUMAN: proceed without the incomplete review", gauntlet_incomplete=["probe"], round_heads=[{}])
check("apply: review_incomplete accept -> skipped review recorded", u["_next"] == "observability_gate" and "probe" in u["skipped_reviews"][0])
check("apply: review_incomplete abort -> end_aborted", aa("review_incomplete", "Abort this task")["_next"] == "end_aborted")
u = aa("strikeout", "Apply the oracle's direction in ONE more coder round", strikeout_rounds=0)
check("apply: strikeout one round -> compose_fix with strikes reset (B6)", u["_next"] == "compose_fix" and u["fix_strikes"] == 0 and u["strikeout_rounds"] == 1)
check("apply: strikeout B6 exhausted -> end_aborted", aa("strikeout", "Apply the oracle's direction", strikeout_rounds=RERUN_ONCE)["_next"] == "end_aborted")
check("apply: lane_stuck accept -> proceed with follow-up", aa("lane_stuck", "Accept the findings unresolved")["_next"] == "observability_gate")
u = aa("scope_breach", "Serialize: finish in-scope work only", coder_brief={"task": "t"}, scope_breach="src/x")
check("apply: scope_breach serialize -> render with constraint", u["_next"] == "render_coder_prompt" and any("Do NOT edit" in l for l in u["coder_brief"]["must_not_do_extra"]))
check("apply: scope_breach abort", aa("scope_breach", "Abort this task")["_next"] == "end_aborted")
check("apply: ci_unsure leave -> final_report + follow-up", aa("ci_unsure", "Leave the PR in draft and report")["_next"] == "final_report")
check("apply: ci_unsure rerun -> ci_poll reset", aa("ci_unsure", "Re-run the failing checks once")["_next"] == "ci_poll")
check("apply: unknown kind -> final_report (never hangs)", aa("weird", "x")["_next"] == "final_report")

# ---------------------------------------------------------------- scope / render / coder routing (B5, B7)
u = gates.scope_check(base_state(parallel_mode=True, own_scope=["src/payments/"], roster_scope={"TASK-002": ["src/auth/"]}, expected_files=["src/payments/a.go", "src/auth/b.go"]))
check("scope_check: breach -> notice + escalation prepared (hard)", u["_next"] == "send_breach_notice" and u["escalation"]["kind"] == "scope_breach" and "TASK-002" in u["scope_breach"])
u = gates.scope_check(base_state(parallel_mode=True, own_scope=["src/payments/"], expected_files=["src/payments/a.go"]))
check("scope_check: in scope -> render", u["_next"] == "render_coder_prompt")
u = gates.scope_check(base_state(parallel_mode=True, own_scope=[], expected_files=["x"]))
check("scope_check: unknown own scope -> skipped, noted", u["_next"] == "render_coder_prompt" and any("skipped" in d for d in u["decisions"]))
u = gates.scope_check(base_state(delegated=False, expected_files=[f"f{i}" for i in range(5)]))
check("scope_check: standalone 5+ files -> confirm once", u["_next"] == "escalate_gate" and u["scope_confirmed"])
u = gates.scope_check(base_state(delegated=True, expected_files=[f"f{i}" for i in range(9)]))
check("scope_check: delegated brief never re-confirms (V7)", u["_next"] == "render_coder_prompt")
u = gates.render_coder_prompt(base_state(coder_brief={"task": "T", "expected_outcome": "O", "context_block": "C", "must_do_extra": ["X"]}, spec_text="AC1", fix_instructions=""))
p = u["coder_prompt"]
check("render: MANDATORY template sections + boilerplate verbatim", all(h in p for h in ("## TASK", "## EXPECTED OUTCOME", "## REQUIRED TOOLS", "## MUST DO", "## MUST NOT DO", "## CONTEXT"))
      and "Load skill `code-review`" in p and "non-ASCII punctuation" in p and "- X" in p and "AC1" in p and "FIX ROUND" not in p)
u = gates.render_coder_prompt(base_state(coder_brief={}, context_pack="PACK", fix_instructions="Fix: boom", fix_round=2, coder_result="CODER_FAILED ..."))
check("render: degraded brief + fix round appended with prior result (E1/A2)", "degraded" in u["coder_prompt"] and "## FIX ROUND 2" in u["coder_prompt"] and "Your previous result" in u["coder_prompt"])
check("render: parallel -> inbox check first", gates.render_coder_prompt(base_state(parallel_mode=True, coder_brief={"task": "t"}))["_next"] == "check_inbox_pre")
check("inbox_route: instruction -> git_ops (return run_coder)", gates.inbox_route(base_state(inbox_instructions="rebase onto main"))["git_task"]["return_to"] == "run_coder")
check("inbox_route: empty -> run_coder", gates.inbox_route(base_state(inbox_instructions=""))["_next"] == "run_coder")
done = "CODER_COMPLETE\nPlan: p\nFiles modified: [a]\nFiles created: []\nBuild: passed\nTests: passed\nDesign: Strategy pattern in src/s.go\nASK -- adopt it at the other call sites?\n"
u = gates.route_coder_result(base_state(coder_result=done, fix_strikes=2))
check("route_coder_result: COMPLETE -> debt_scan, strikes reset, Design block + ASK extracted", u["_next"] == "debt_scan" and u["fix_strikes"] == 0 and "Strategy" in u["design_block"] and u["design_asks"] == ["ASK -- adopt it at the other call sites?"])
u = gates.route_coder_result(base_state(coder_result="CODER_FAILED\nPlan: p\nAttempts: 5/5\n\nVerification gate error:\nx\n\nLast script error:\nnull\n\nLast build output:\nerror E0425", fix_strikes=0))
check("route_coder_result: FAILED strike 1 -> compose_fix", u["_next"] == "compose_fix" and u["fix_strikes"] == 1 and "E0425" in u["last_coder_error"])
u = gates.route_coder_result(base_state(coder_result="CODER_FAILED\nLast script error:\nnull\nLast build output: e", fix_strikes=1))
check("route_coder_result: strike 2 -> diagnose_failure (diagnosing-bugs before strike 2)", u["_next"] == "diagnose_failure" and u["fix_strikes"] == 2)
u = gates.route_coder_result(base_state(coder_result="CODER_FAILED\nLast script error:\nnull\n", fix_strikes=STRIKE_LIMIT - 1))
check("route_coder_result: B5 strike 3 -> strikeout", u["_next"] == "strikeout")
u = gates.route_coder_result(base_state(coder_result="CODER_FAILED\n\nLast script error:\nscript 'verify_build' failed: exit 2\n\nLast build output:\n", fix_strikes=0))
check("route_coder_result: crashed gate -> engine_fault escalation (not a code fix)", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "engine_fault")
u = gates.route_coder_result(base_state(coder_result="Agent node failed: child timed out", fix_strikes=0))
check("route_coder_result: agent-node failure counts as a strike", u["fix_strikes"] == 1 and u["_next"] == "compose_fix")
u = gates.route_coder_result(base_state(coder_result="I did some stuff", fix_strikes=0))
check("route_coder_result: no sentinel -> strike with the reason", u["coder_sentinel"] == "NO_SENTINEL" and "no recognizable sentinel" in u["last_coder_error"])
u = gates.route_coder_result(base_state(coder_result="CODER_REJECTED\nPlan: p", rejected_rounds=0))
check("route_coder_result: REJECTED -> coder_rejected escalation", u["escalation"]["kind"] == "coder_rejected" and u["rejected_rounds"] == 1)
u = gates.route_coder_result(base_state(coder_result="CODER_REJECTED", rejected_rounds=STRIKE_LIMIT))
check("route_coder_result: B7 rejected bound -> end_blocked", u["_next"] == "end_blocked")
u = gates.compose_fix(base_state(fix_source="review", fix_items=["adversary: criterion 2 unmet"], fix_round=1))
check("compose_fix: review items verbatim, round incremented", "criterion 2 unmet" in u["fix_instructions"] and u["fix_round"] == 2 and u["_next"] == "render_coder_prompt")
open(os.path.join(REPO, "dirty.txt"), "w").write("x")
u = gates.strikeout(base_state(last_coder_error="e"))
check("strikeout: dirty tree stashed (recoverable, V8) + documented", "stash" in u["strikeout_stash"].lower() or "Saved" in u["strikeout_stash"])
check("strikeout: tree clean after stash", subprocess.run(["git", "-C", REPO, "status", "--porcelain"], capture_output=True, text=True).stdout.strip() == "")
u = gates.strikeout_route(base_state(oracle_result="I cannot resolve this without more context"))
check("strikeout_route: unresolved -> hard-trigger escalation, abort recommended", u["escalation"]["hard_trigger"] and u["escalation"]["options"][0]["recommended"])
u = gates.strikeout_route(base_state(oracle_result="The root cause is X; change Y."))
check("strikeout_route: resolved -> one-round option recommended", "ONE more" in u["escalation"]["options"][0]["text"] and u["escalation"]["options"][0]["recommended"])

# ---------------------------------------------------------------- bookkeeping (B8/B9)
u = gates.bookkeeping_scan(base_state(debt_hits="src/a.rs:10 -- #[allow(dead_code)] // TASK-009\nsrc/b.rs:3 -- todo!()\n"))
check("bookkeeping_scan: hits parsed with owners", [i["owner"] for i in u["debt_items"]] == ["TASK-009", ""])
u = gates.bookkeeping_route(base_state(debt_items=[{"where": "b:3", "text": "todo!()", "owner": ""}], debt_rounds=0))
check("bookkeeping_route: unowned marker -> one coder round (B8)", u["_next"] == "compose_fix" and u["debt_rounds"] == 1 and u["fix_source"] == "debt")
u = gates.bookkeeping_route(base_state(debt_items=[{"where": "b:3", "text": "todo!()", "owner": ""}], debt_rounds=RERUN_ONCE))
check("bookkeeping_route: B8 exhausted -> Debt left behind with owner none", u["_next"] == "review_gate" and u["debt_left_behind"][0].endswith("-- none"))
u = gates.bookkeeping_route(base_state(debt_items=[], env_gated_tests=["tests/slow.rs under SLOW=1"], debt_rounds=0))
check("bookkeeping_route: env-gated test is unowned debt", u["_next"] == "compose_fix")
u = gates.bookkeeping_route(base_state(debt_items=[], design_asks=["ASK -- adopt?"], design_ask_rounds=0))
check("bookkeeping_route: Design ASK -> hard-trigger 'Design question:' escalation", u["_next"] == "escalate_gate" and u["escalation"]["hard_trigger"] and u["ask_question"].startswith("Design question:"))
u = gates.bookkeeping_route(base_state(debt_items=[], design_block="Strategy in src/s.go"))
check("bookkeeping_route: Design block -> design decision ledgered", u["_next"] == "review_gate" and u["design_decisions"] == ["Strategy in src/s.go"])
u = gates.bookkeeping_route(base_state(debt_items=[], decisions_in_docs=["README.md: decision: we chose X"], debt_rounds=0))
check("bookkeeping_route: decisions in README -> coder round", u["_next"] == "compose_fix")

# ---------------------------------------------------------------- review (B10/B11)
check("review_gate: batch cadence skips the panel", gates.review_gate(base_state(review_cadence="batch"))["_next"] == "whetstone_gate")
check("review_gate: trivial single file no spec -> skip with note", gates.review_gate(base_state(intent="trivial", changed_files=["a"], has_spec=False))["_next"] == "whetstone_gate")
check("review_gate: trivial but spec -> gauntlet", gates.review_gate(base_state(intent="trivial", changed_files=["a"], has_spec=True))["_next"] == "build_gauntlet_prompt")
check("review_gate: default -> gauntlet", gates.review_gate(base_state(intent="implementation"))["_next"] == "build_gauntlet_prompt")
u = gates.build_gauntlet_prompt(base_state(spec_text="AC", quality_rigor="poc", consumer_surface=True))
gp = u["gauntlet_prompt"]
check("build_gauntlet: fields (Diff=base sha, posture from rigor, spec, recipe+house note)", f"Diff: {BASE}" in gp and "Security posture: prototype" in gp and "AC" in gp
      and (gates.PROBE_HOUSE_NOTE[:20] in gp if gates.PROBE_HOUSE_NOTE else "house convention" not in gp)
      and u["round_heads"][0]["round"] == 1)
u = gates.build_gauntlet_prompt(base_state(round_heads=[{"round": 1, "head": "0" * 40, "lanes_with_findings": ["adversary"]}],
                                           gauntlet_lanes={"code-review": "PASS", "adversary": "DIVERGES", "security": "PASS"},
                                           rejected_findings=["rejected-finding: f -- src/x:1"]))
gp = u["gauntlet_prompt"]
check("build_gauntlet: re-review round carries prior decisions, delta, settled lanes minus finding lanes",
      "Prior review decisions" in gp and "Re-review delta: 0000" in gp and "Settled lanes: code-review, security" in gp and "adversary" not in gp.split("Settled lanes:")[1])
passed = "GAUNTLET: PASS\n\n| Lane | Verdict |\n|---|---|\n| code-review | MERGE-READY |\n| adversary | CONFORMS |\n\n## Human attention required\n- check X\n"
u = gates.route_gauntlet(base_state(gauntlet_result=passed, round_heads=[{"round": 1, "head": "h"}]))
check("route_gauntlet: PASS -> observability, lanes parsed, attention carried", u["_next"] == "observability_gate" and u["gauntlet_lanes"]["adversary"] == "CONFORMS" and "check X" in u["gauntlet_attention"])
blocked = "GAUNTLET: BLOCKED\n\n| Lane | Verdict |\n| adversary | DIVERGES |\n\n## Blockers\n- adversary: criterion 2 (tests) not met\n- debris: Implementation debris -- src/a.rs:3 -- non-ASCII\n"
u = gates.route_gauntlet(base_state(gauntlet_result=blocked, round_heads=[{"round": 1, "head": "h"}]))
check("route_gauntlet: BLOCKED with findings -> triage", u["_next"] == "triage_findings" and len(u["gauntlet_blockers"]) >= 2)
inc = "GAUNTLET: BLOCKED\nGAUNTLET_REVIEW_INCOMPLETE: debris\n\n## Blockers\n- debris: PIPELINE-FAULT malformed debris_findings\n"
u = gates.route_gauntlet(base_state(gauntlet_result=inc, debris_reruns=0, round_heads=[{}]))
check("route_gauntlet: debris-only incomplete -> one re-run (B11)", u["_next"] == "build_gauntlet_prompt" and u["debris_reruns"] == 1)
u = gates.route_gauntlet(base_state(gauntlet_result=inc, debris_reruns=RERUN_ONCE, round_heads=[{}]))
check("route_gauntlet: B11 exhausted -> three-option escalation", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "review_incomplete" and len(u["escalation"]["options"]) == 3)
u = gates.route_gauntlet(base_state(gauntlet_result="GAUNTLET: BLOCKED\nGAUNTLET_REVIEW_INCOMPLETE: security, pipeline\n", round_heads=[{}]))
check("route_gauntlet: other lane incomplete -> straight to the options", u["escalation"]["kind"] == "review_incomplete" and u["gauntlet_incomplete"] == ["security", "pipeline"])
u = gates.route_gauntlet(base_state(gauntlet_result="Agent node failed: killed", round_heads=[{}]))
check("route_gauntlet: agent-node failure -> incomplete pipeline (never PASS)", u["gauntlet_verdict"] == "BLOCKED" and u["gauntlet_incomplete"] == ["pipeline"])
tri = [{"finding": "🔴 null deref", "action": "reject", "citation": "src/x.go:1", "lane": "code-review"},
       {"finding": "[convention] naming", "action": "reject", "citation": "", "lane": "code-review"},
       {"finding": "[convention] order", "action": "reject", "citation": "src/y.go:9 does it this way", "lane": "code-review"},
       {"finding": "🟡 [convention] long fn", "action": "defer", "lane": "code-review"}]
u = gates.triage_route(base_state(triage=tri, quality_rigor="production", review_rounds=0))
check("triage_route: critical + uncited rejections stand as fixes; cited rejection ledgered; defer at production -> fix",
      u["_next"] == "compose_fix" and len(u["rejected_findings"]) == 1 and len(u["fix_items"]) == 3 and u["review_rounds"] == 1)
u = gates.triage_route(base_state(triage=tri[3:], quality_rigor="poc", review_rounds=0))
check("triage_route: defer honoured at poc -> deferred, no fix round", u["_next"] == "observability_gate" and u["deferred_findings"] and "(deferred by quality bar)" in u["deferred_findings"][0])
u = gates.triage_route(base_state(triage=[{"finding": "DIVERGES: c1", "action": "fix", "lane": "adversary"}], review_rounds=0, lane_block_streak={"adversary": 1}))
check("triage_route: adversary blocking twice after a fix -> lane_stuck escalation (re-run once rule)", u["escalation"]["kind"] == "lane_stuck")
u = gates.triage_route(base_state(triage=[{"finding": "x", "action": "fix", "lane": "code-review"}], review_rounds=STRIKE_LIMIT))
check("triage_route: B10 review rounds exhausted -> review_exhausted escalation", u["escalation"]["kind"] == "review_exhausted")
u = gates.triage_route(base_state(triage=[{"finding": "USAGE_PROBE: INCONCLUSIVE no recipe", "action": "fix", "lane": "probe"}], review_rounds=0))
check("triage_route: probe INCONCLUSIVE is an environment gap escalation, never a coder fix", u["escalation"]["kind"] == "probe_gap")
u = gates.triage_route(base_state(triage=[{"finding": "criterion 3 is impossible as written", "action": "escalate", "lane": "adversary", "reason": "plan root cause"}]))
check("triage_route: plan-is-root-cause -> plan_defect escalation", u["escalation"]["kind"] == "plan_defect")
u = gates.triage_route(base_state(triage=[{"finding": "🟡 maybe", "action": "escalate", "lane": "code-review"}]))
check("triage_route: unsure yellow -> fix-or-accept escalation", u["escalation"]["kind"] == "yellow_findings")
u = gates.triage_route(base_state(triage=[], gauntlet_blockers=["b1", "b2"], last_script_error="llm died"))
check("triage_route: fail-open when triage died -> every blocker is a fix", u["_next"] == "compose_fix" and u["fix_items"] == ["b1", "b2"])

# ---------------------------------------------------------------- observability / whetstone / publish
open(os.path.join(REPO, "handler.go"), "w").write("func Handler() { http.HandleFunc(\"/x\", f) }\n")
u = gates.observability_gate(base_state(observability_agent=""))
check("observability_gate: endpoint added -> pass; repo-derived note", u["_next"] == "observability_pass" and "repo-derived" in u["obs_inventory_note"])
os.remove(os.path.join(REPO, "handler.go"))
open(os.path.join(REPO, "notes.md"), "w").write("docs\n")
u = gates.observability_gate(base_state())
check("observability_gate: docs-only -> skipped with artifact note", u["_next"] == "whetstone_gate" and "skipped" in u["observability_artifact"])
os.remove(os.path.join(REPO, "notes.md"))
u = gates.observability_route(base_state(observability_artifact="## Observability\nx", obs_coder_brief="add alert", obs_rounds=0, review_rounds=0))
check("observability_route: alert-as-code -> one coder round (B12)", u["_next"] == "compose_fix" and u["obs_rounds"] == 1)
u = gates.observability_route(base_state(observability_artifact="x", obs_coder_brief="add alert", obs_rounds=RERUN_ONCE))
check("observability_route: B12 exhausted -> follow-up, never blocks", u["_next"] == "whetstone_gate" and u["follow_ups"])
u = gates.observability_route(base_state(observability_artifact="", last_script_error="died"))
check("observability_route: dead pass -> declared degraded artifact", "degraded" in u["observability_artifact"])
check("whetstone_gate: feedback -> whetstone", gates.whetstone_gate(base_state(external_feedback=["nit from Bob"]))["_next"] == "run_whetstone")
check("whetstone_gate: none -> skipped note", "skipped" in gates.whetstone_gate(base_state(external_feedback=[]))["whetstone_note"])
check("pr_gate: nothing asked -> final_report (never commit unasked)", gates.pr_gate(base_state(git_actions={}))["_next"] == "final_report")
u = gates.pr_gate(base_state(git_actions={"commit": True, "push": True, "open_pr": True, "base": "feat/stack-1"}))
check("pr_gate: PR asked -> git_ops with draft + base + pre-publish report", u["_next"] == "git_ops" and u["git_task"]["draft"] and u["git_task"]["base"] == "feat/stack-1" and "FOLLOW-UPS" in u["report"])
check("pr_gate: parallel -> inbox first", gates.pr_gate(base_state(git_actions={"push": True}, parallel_mode=True))["_next"] == "check_inbox_final")
check("pr_gate: whetstone fault noted, run finishes", "errored" in gates.pr_gate(base_state(git_actions={}, whetstone_result="Agent node failed: x"))["whetstone_note"])
check("ci_gate: instruction return -> run_coder", gates.ci_gate(base_state(git_task={"kind": "instruction", "return_to": "run_coder"}))["_next"] == "run_coder")
check("ci_gate: parent-owned PR lifecycle -> draft left, final_report", gates.ci_gate(base_state(git_task={"kind": "publish", "return_to": "ci_gate", "open_pr": True}, delegated=True, git_report="https://github.com/o/r/pull/5"))["_next"] == "final_report")
u = gates.ci_gate(base_state(git_task={"kind": "publish", "return_to": "ci_gate", "open_pr": True}, delegated=False, git_report="opened https://github.com/o/r/pull/5"))
check("ci_gate: standalone owns the flip -> ci_poll", u["_next"] == "ci_poll" and u["ci_owner"])
check("ci_gate: flip_ready done -> final_report", gates.ci_gate(base_state(git_task={"kind": "flip_ready"}))["_next"] == "final_report")
os.environ["SISYPHUS_CI_SLEEP"] = "0"
os.environ["SISYPHUS_GH_CHECKS_CMD"] = "echo '[{\"name\":\"build\",\"bucket\":\"pass\"},{\"name\":\"test\",\"bucket\":\"pending\"}]'"
u = gates.ci_poll(base_state(git_report="pull/5", ci_polls=0, ci_started_at=0))
check("ci_poll: pending -> re-enter", u["_next"] == "ci_poll" and u["ci_status"] == "pending")
os.environ["SISYPHUS_GH_CHECKS_CMD"] = "echo '[{\"name\":\"build\",\"bucket\":\"pass\"}]'"
check("ci_poll: all green -> route", gates.ci_poll(base_state(git_report="pull/5", ci_polls=1, ci_started_at=int(__import__('time').time())))["ci_status"] == "green")
os.environ["SISYPHUS_GH_CHECKS_CMD"] = "echo '[{\"name\":\"build\",\"bucket\":\"fail\",\"link\":\"https://github.com/o/r/actions/runs/99/job/1\"}]'"
u = gates.ci_poll(base_state(git_report="pull/5", ci_polls=1, ci_started_at=int(__import__('time').time())))
check("ci_poll: red -> route with failing checks", u["ci_status"] == "red" and u["ci_failing"][0][0] == "build")
os.environ["SISYPHUS_GH_CHECKS_CMD"] = "exit 3"
u = gates.ci_poll(base_state(git_report="pull/5", ci_polls=1, ci_poll_fails=0, ci_started_at=int(__import__('time').time())))
check("ci_poll: poll error 1 -> retry", u["_next"] == "ci_poll" and u["ci_poll_fails"] == 1)
u = gates.ci_poll(base_state(git_report="pull/5", ci_polls=1, ci_poll_fails=2, ci_started_at=int(__import__('time').time())))
check("ci_poll: B15 consecutive poll errors -> route poll_error", u["ci_status"] == "poll_error" and u["_next"] == "ci_route")
u = gates.ci_poll(base_state(git_report="pull/5", ci_polls=1, ci_started_at=1))
check("ci_poll: B15 wall clock exceeded -> timeout", u["ci_status"] == "timeout")
check("ci_route: green -> flip via git_ops", gates.ci_route(base_state(ci_status="green"))["git_task"]["kind"] == "flip_ready")
check("ci_route: poll_error -> ci_exhausted escalation", gates.ci_route(base_state(ci_status="poll_error"))["escalation"]["kind"] == "ci_exhausted")
u = gates.ci_route(base_state(ci_status="red", ci_failing=[["build", "https://x/actions/runs/1"]]))
check("ci_route: red -> evidence pack -> diagnose", u["_next"] == "ci_diagnose" and "### build" in u["ci_evidence"])
u = gates.ci_apply(base_state(ci_verdict="flake", failing_checks=["build"], ci_failing=[["build", ""]], ci_reruns={}))
check("ci_apply: flake -> re-run ledgered, back to poll", u["_next"] == "ci_poll" and u["ci_reruns"]["build"] == 1)
u = gates.ci_apply(base_state(ci_verdict="flake", failing_checks=["build"], ci_failing=[["build", ""]], ci_reruns={"build": CI_FLAKE_RERUNS}, ci_fix_strikes=0))
check("ci_apply: flake budget exhausted -> real (third red is real)", u["_next"] == "compose_fix" and u["fix_source"] == "ci")
check("ci_apply: real strikes exhausted -> escalation", gates.ci_apply(base_state(ci_verdict="real", ci_fix_strikes=STRIKE_LIMIT))["escalation"]["kind"] == "ci_exhausted")
check("ci_apply: infra -> wait-or-leave escalation", gates.ci_apply(base_state(ci_verdict="infra"))["escalation"]["kind"] == "ci_infra")
check("ci_apply: unsure defaults UP (escalation, never re-run)", gates.ci_apply(base_state(ci_verdict="unsure"))["escalation"]["kind"] == "ci_unsure")
check("ci_apply: dead diagnose -> unsure path", gates.ci_apply(base_state(last_script_error="died"))["escalation"]["kind"] == "ci_unsure")

# ---------------------------------------------------------------- final report
u = gates.final_report(base_state(coder_result=done, coder_sentinel="CODER_COMPLETE", gauntlet_verdict="PASS", round_heads=[{"round": 1, "head": "h", "verdict": "PASS"}],
                                  debt_left_behind=["a:1 -- todo!() -- none"], debris_exemptions=["b:2 -- arrow glyph is the point"],
                                  design_decisions=["Strategy"], autonomous_digest=["design_choice: A (basis: recommended option)"],
                                  skipped_reviews=["round 1: probe (NEEDS-HUMAN accepted)"], deferred_findings=["x (deferred by quality bar)"],
                                  anomalies_log=["assess: stale plan"], whetstone_note="whetstone skipped: ..."))
r = u["report"]
check("final_report: every contract section present", all(s in r for s in ("Sisyphus run id:", "Verification evidence", "Decisions taken autonomously", "Skipped reviews",
      "Debt left behind:", "Debris exemptions:", "Design decisions:", "FOLLOW-UPS:", "Anomalies", "Build: passed")))
check("final_report: report.md written", os.path.exists(os.path.join(base_state()["run_dir"], "report.md")))
u = gates.final_report(base_state(intent="plan_execution", report="STEP_COMPLETE ..."))
check("final_report: step-runner result relayed with run id", u["report"].startswith("STEP_COMPLETE") and "Sisyphus run id" in u["report"])

# ---------------------------------------------------------------- entry scripts via GRAPH_STATE (subprocess)
def run_entry(name, state, env=None):
    e = {**os.environ, "GRAPH_STATE": json.dumps(state)}
    e.update(env or {})
    p = subprocess.run([sys.executable, os.path.join(HERE, f"{name}.py")], capture_output=True, text=True, env=e)
    try:
        return p.returncode, json.loads(p.stdout)
    except json.JSONDecodeError:
        return p.returncode, {"_stderr": p.stderr}

code, out = run_entry("route_intent", base_state(intent="implementation", sizing="simple"))
check("entry: route_intent via GRAPH_STATE", code == 0 and out["_next"] == "next_subtask")
big = base_state(intent="implementation", sizing="simple", brief="x" * 40000)
path = os.path.join(TMP, "state.json")
open(path, "w").write(json.dumps(big))
e = {**os.environ, "GRAPH_STATE_FILE": path}
e.pop("GRAPH_STATE", None)
p = subprocess.run([sys.executable, os.path.join(HERE, "route_intent.py")], capture_output=True, text=True, env=e)
check("entry: GRAPH_STATE_FILE (>32 KiB) path", p.returncode == 0 and json.loads(p.stdout)["_next"] == "next_subtask")
code, out = run_entry("debt_scan", {"project_dir": REPO, "debt_base": BASE})
check("entry: debt_scan shim (shared scanner, graph mode)", code == 0 and "debt_hits" in out and ("clean" in out["debt_hits"] or "DEBT_SCAN_FAILED" in out["debt_hits"]))
code, out = run_entry("debt_scan", {"project_dir": os.path.join(TMP, "nope"), "debt_base": "zzz"})
check("entry: debt_scan fail-open (bad dir) -> DEBT_SCAN_FAILED, exit 0", code == 0 and "DEBT_SCAN_FAILED" in out.get("debt_hits", ""))
code, out = run_entry("apply_answer", base_state(escalation={"kind": "weird"}, last_answer="x"))
check("entry: apply_answer unknown kind", code == 0 and out["_next"] == "final_report")
code, out = run_entry("route_coder_result", base_state(coder_result=None))
check("entry: route_coder_result with null result -> strike, no crash", code == 0 and out["fix_strikes"] == 1)
code, out = run_entry("explore_fault", {"slice": "s"})
check("entry: explore_fault branch fallback", code == 0 and out["finding"].startswith("PIPELINE-FAULT"))

# --- A10 conformance: ci_gate harvests (added post-review) ---
u = gates.ci_gate({"git_report": "ok\nANOMALY: commits I did not make\n", "git_task": {"return_to": "final_report"}, "run_dir": TMP, "anomalies_log": []})
check("ci_gate: ANOMALY: lines harvested from git_report", any("commits I did not make" in e for e in u.get("anomalies_log", [])))
u = gates.ci_gate({"git_report": "ok", "anomalies": ["inbox-final observation"], "git_task": {"return_to": "final_report"}, "run_dir": TMP, "anomalies_log": []})
check("ci_gate: inbox-final schema anomalies harvested through git_ops", any("inbox-final observation" in e for e in u.get("anomalies_log", [])))
u = gates.ci_gate({"git_report": "clean exit 0", "git_task": {"kind": "flip_ready"}, "run_dir": TMP, "anomalies_log": []})
check("ci_gate: clean case adds nothing", u.get("anomalies_log", []) == [] and u["_next"] == "final_report")

failed = [n for n, ok, _ in RESULTS if not ok]
print(f"\n{len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed")
shutil.rmtree(TMP, ignore_errors=True)
sys.exit(1 if failed else 0)
