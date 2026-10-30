#!/usr/bin/env python3
"""Behavioral smoke battery for the architect-next gates.

Runs every gate in-process against crafted states - both sides of every named
bound, every fail-open / fallback path, the sentinel parsers, the
escalation ladder under both policies, re-entry (A2), the anomalies harvest
(A10), the A1 amendment cap, the per-check-name CI budget keying (a fix push
must NOT re-arm a flake budget) - inside a scratch code repo (with a bare
origin), the fork's scratch planning store (_fixture.py) and a scratch runs root,
with the REAL prose-architect tools.sh wired through a scratch LLM_ROOT_DIR. Also exercises the
GRAPH_STATE env pattern through the real entry scripts (subprocess).

Usage: smoke_test.py   (exit 1 on any failure; prints one line per check)
"""

import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.dont_write_bytecode = True
sys.path.insert(0, HERE)

TMP = tempfile.mkdtemp(prefix="ax-smoke-")
os.environ["ARCHITECT_RUNS_ROOT"] = os.path.join(TMP, "runs")
os.environ["HOME"] = os.path.join(TMP, "home")  # no real away file / rulings file
os.makedirs(os.environ["HOME"], exist_ok=True)
os.environ["ARCHITECT_CI_SLEEP"] = "0"
for _k in [k for k in os.environ if k.startswith("LLM_AGENT_VAR_")]:
    # the battery must not inherit the agent variables of whoever runs it
    os.environ.pop(_k)
for _k in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR"):
    # a caller's repository variables (a git hook exports them) would point
    # every scratch-repo git call at the caller's own repository
    os.environ.pop(_k, None)
for _k in [k for k in os.environ if k == "ARCHITECT_CORE_TOOLS" or (k.startswith("ARCHITECT_GH_") and k.endswith("_CMD"))]:
    # a caller's tool overrides would replace the scripts' own tools.sh and gh calls
    os.environ.pop(_k)

import gates  # noqa: E402
import _lib  # noqa: E402
from _lib import (AMEND_CYCLES_MAX, CI_FLAKE_RERUNS, CI_MAX_ROUNDS, CI_POLL_FAILS, CONFLICT_CYCLES_MAX, CORE_TOOLS_REL,  # noqa: E402
                  DESIGN_ROUNDS_MAX, FORK, MULTI_IS_GROUPS, RERUN_ONCE, STRIKE_LIMIT, WAVE_CAP, escalation, option, read_file)

RESULTS = []


def check(name, cond, detail=""):
    RESULTS.append((name, bool(cond), detail))
    print(("ok   " if cond else "FAIL ") + name + (f"  ({detail})" if detail and not cond else ""))


GIT_ENV = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"}


def git(repo, *a, check_=True):
    p = subprocess.run(["git", "-C", repo, *a], capture_output=True, text=True, env=GIT_ENV)
    if check_ and p.returncode != 0:
        raise RuntimeError(p.stderr)
    return p.stdout.strip()


def write(path, text, exe=False):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)
    if exe:
        os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)


# ---------------------------------------------------------------- scratch repos
def make_repo(path, files):
    os.makedirs(path)
    git(path, "init", "-q", "-b", "main")
    for rel, text in files.items():
        write(os.path.join(path, rel), text)
    git(path, "add", ".")
    git(path, "commit", "-q", "-m", "init")
    bare = path + ".git"
    git(path, "init", "-q", "--bare", bare)
    git(path, "remote", "add", "origin", bare)
    git(path, "push", "-q", "-u", "origin", "main")
    return git(path, "rev-parse", "HEAD")


CODE = os.path.join(TMP, "code")
CODE_BASE = make_repo(CODE, {"README.md": "# demo\n", "CLAUDE.md": "Run `go test ./...` before committing.\n",
                             "src/a.go": "package a\n", "src/b.go": "package b\n", "api/h.go": "package api\n"})

from _fixture import CLOSE_COMMIT_MARKER, HAS_OWNER_FILE, OWNER, Store  # noqa: E402

STORE = Store(TMP, CODE)
PLANS = STORE.root

# The REAL prose-architect tools.sh through a scratch LLM_ROOT_DIR (copies only; never symlinks).
ROOT = os.path.join(TMP, "coyote")
core_src = os.path.join(HERE, "..", "..", CORE_TOOLS_REL)
shutil.copytree(core_src, os.path.join(ROOT, "agents", CORE_TOOLS_REL))
utils_src = os.path.join(HERE, "..", "..", ".shared", "utils.sh")
if os.path.isfile(utils_src):
    write(os.path.join(ROOT, "agents", ".shared", "utils.sh"), read_file(utils_src))
else:
    write(os.path.join(ROOT, "agents", ".shared", "utils.sh"), "# stub utils\n")
check("utils.sh: the repo asset is copied, not a stub", "set -euo pipefail" in read_file(os.path.join(ROOT, "agents", ".shared", "utils.sh")))
write(os.path.join(ROOT, "agents", ".shared", "debris.py"), "import sys\nsys.exit(0)\n")
os.environ["LLM_ROOT_DIR"] = ROOT
# Fake gh for the tools / scripts that shell out (never the real GitHub).
GHBIN = os.path.join(TMP, "bin")
write(os.path.join(GHBIN, "gh"), "#!/usr/bin/env bash\necho 'gh stub: $*' >&2\nexit 1\n", exe=True)
os.environ["PATH"] = GHBIN + os.pathsep + os.environ["PATH"]

DESIGN = os.path.join(TMP, "design.md")
write(DESIGN, "# Widget service\n\nAdd a widget endpoint and a worker.\n")

PLAN = STORE.plan_path
PLAN_TEXT = """---
title: Widgets
slug: widgets
status: open
---

# Widgets

## Problem

Widgets are missing.

## Approach

Add them.

## Quality bar

rigor: production

## Task breakdown

| Task | Title | Points | Blocked by | Files |
|---|---|---|---|---|
| 1 | Widget model | 1 | | src/a.go |
| 2 | Widget endpoint | 1 | 1 | api/h.go |
| 3 | Widget worker | 1 | | src/b.go |

## Local-run recipe

`make run`
"""
write(PLAN, PLAN_TEXT)


def base_state(**over):
    s = STORE.variables(DESIGN, TMP, autonomy="full", escalation_policy="interactive", parallel_tasks="0", pr_strategy="single",
                        task_granularity="standard", auto_confirm="1")
    s.update(STORE.normalized())
    s.update({"code_repo_path": CODE, "git_root": CODE, "reference_root_path": TMP, "design_doc_path": DESIGN,
              "run_id": "x", "run_dir": os.path.join(os.environ["ARCHITECT_RUNS_ROOT"], "x"), "decisions": [], "anomalies": [],
              "anomalies_log": [], "plan_path": PLAN, "plan_slug": "widgets", "mode_line": "m", "tasks": [], "task_rounds": {},
              "follow_ups": [], "pending_fixes": [], "round_log": {}, "verification_commands": '["true"]'})
    s.update(over)
    return s


os.makedirs(base_state()["run_dir"], exist_ok=True)
os.makedirs(os.path.join(base_state()["run_dir"], "reports"), exist_ok=True)
os.makedirs(os.path.join(base_state()["run_dir"], "rounds"), exist_ok=True)

# ---------------------------------------------------------------- run dir resolution
_saved_env = {k: os.environ.get(k) for k in ("ARCHITECT_RUNS_ROOT", "COYOTE_CACHE_DIR", "XDG_CACHE_HOME")}
for _k in _saved_env:
    os.environ.pop(_k, None)
os.environ["COYOTE_CACHE_DIR"] = os.path.join(TMP, "cache")
check("runs_root: COYOTE_CACHE_DIR/architect-next/runs when no override is set",
      _lib.runs_root() == os.path.join(TMP, "cache", "architect-next", "runs"), _lib.runs_root())
os.environ.pop("COYOTE_CACHE_DIR")
os.environ["XDG_CACHE_HOME"] = os.path.join(TMP, "xdg")
check("runs_root: XDG_CACHE_HOME/coyote/architect-next/runs when COYOTE_CACHE_DIR is unset",
      _lib.runs_root() == os.path.join(TMP, "xdg", "coyote", "architect-next", "runs"), _lib.runs_root())
os.environ.pop("XDG_CACHE_HOME")
_platform_root = _lib.runs_root()
check("runs_root: platform cache dir/coyote/architect-next/runs when nothing is set",
      os.path.isabs(_platform_root) and (os.name == "nt" or _platform_root.startswith(os.environ["HOME"]))
      and _platform_root.endswith(os.path.join("coyote", "architect-next", "runs")), _platform_root)
os.environ["COYOTE_CACHE_DIR"] = os.path.join(TMP, "cache")
os.environ["XDG_CACHE_HOME"] = os.path.join(TMP, "xdg")
os.environ["ARCHITECT_RUNS_ROOT"] = os.path.join(TMP, "override")
check("runs_root: ARCHITECT_RUNS_ROOT wins over every cache dir", _lib.runs_root() == os.path.join(TMP, "override"))
for _k, _v in _saved_env.items():
    if _v is None:
        os.environ.pop(_k, None)
    else:
        os.environ[_k] = _v
check("runs_root: battery env restored", _lib.runs_root() == os.environ["ARCHITECT_RUNS_ROOT"])


# ---------------------------------------------------------------- intake / re-entry (A2)
st = gates.intake({"initial_prompt": f"Implement the design at {DESIGN}", **STORE.variables(DESIGN, TMP, owner="someone", pr_strategy="single")})
check("intake: run dir + brief + mode line + normalized paths", st["_next"] == "orient_read" and os.path.isdir(st["run_dir"])
      and st["code_repo_path"] == CODE and st["tasks_root"] == STORE.tasks_root and "pr_strategy=single" in st["mode_line"])
check("intake: design doc resolved from the prompt", st["design_doc_path"] == DESIGN)
check("intake: owner identity file wins over the owner variable (ledgered)", (not HAS_OWNER_FILE) or (st["owner_name"] == OWNER and any("whoami" in d for d in st["decisions"])))
check("intake: brief.md + state.json written", os.path.exists(os.path.join(st["run_dir"], "brief.md")) and os.path.exists(os.path.join(st["run_dir"], "state.json")))
rid = st["run_id"]
st_bad = gates.intake({"initial_prompt": f"go {DESIGN}", **STORE.variables(DESIGN, TMP, pr_strategy="triple")})
check("intake: unknown mode value -> escalation mode_invalid (fail-safe, never a default)", st_bad["_next"] == "escalate_gate" and st_bad["escalation"]["kind"] == "mode_invalid")
st_nd = gates.intake({"initial_prompt": "implement the thing", **STORE.variables("", TMP)})
check("intake: no design doc -> escalation need_design_doc", st_nd["_next"] == "escalate_gate" and st_nd["escalation"]["kind"] == "need_design_doc")
write(os.path.join(st["run_dir"], "tasks.json"), json.dumps([{"id": "TASK-101", "status": "complete"}]))
gates.snapshot(st, {"sealed": True, "gate2_verdict": "OKAY", "plan_path": PLAN}, "task TASK-101 done")
st2 = gates.intake({"initial_prompt": f"Fix these CI failures: X\nArchitect run id: {rid}", **STORE.variables(DESIGN, TMP)})
check("intake: re-entry loads the prior run state + tasks.json (A2)", st2["_next"] == "resume_route" and st2["run_id"] == rid and st2["resume_mode"] is True
      and st2["tasks"] and st2["tasks"][0]["id"] == "TASK-101" and st2["sealed"] is True)
st3 = gates.intake({"initial_prompt": f"go {DESIGN}\nArchitect run id: 20200101T000000-deadbeef", **STORE.variables(DESIGN, TMP)})
check("intake: unknown run id -> fresh run, declared", not st3.get("resume_mode") and "no durable state" in st3["resume_note"])
for phase, parked, exp in [("task TASK-101 done", "", "select_tasks"), ("phase_f", "", "phase_f_gate"), ("sealed", "", "reseal_gate"), ("x", "design_round", "escalate_gate")]:
    u = gates.resume_route(base_state(resume_phase=phase, parked=parked, sealed=False, tasks=[]))
    check(f"resume_route: phase {phase!r} parked={parked!r} -> {exp}", u["_next"] == exp)
u = gates.orient_fault(base_state(last_script_error="boom"))
check("orient_fault: fail-safe -> escalation clarify", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "clarify")

# ---------------------------------------------------------------- research
u = gates.research_dispatch(base_state(explore_slices=["a", "b"], team_mode=True, archivist_questions=["what was decided?"]))
check("research_dispatch: team mode -> team map", u["_next"] == "explore_fanout_team")
check("research_dispatch: archivist lane per fork constant", (u["archivist_questions"] == ["what was decided?"]) == gates.ARCHIVIST_AVAILABLE)
u = gates.research_dispatch(base_state(explore_slices=[], last_node_output="llm node 'plan_research' failed: x"))
check("research_dispatch: fail-open degraded grounding slice + reason ledgered", u["_next"] == "explore_fanout_solo" and len(u["explore_slices"]) == 1 and any("failed" in d for d in u["decisions"]))
u = gates.research_dispatch(base_state(explore_slices=["a"], team_mode=True))
check("research_dispatch: single slice never a team", u["_next"] == "explore_fanout_solo")
check("explore_fault: PIPELINE-FAULT finding", gates.explore_fault({"slice": "s", "finding": "Agent node failed"})["finding"].startswith("PIPELINE-FAULT"))
check("archivist/librarian/checklist faults declared", all(f({"question": "q", "label": "l"})["finding"].startswith("PIPELINE-FAULT") for f in (gates.archivist_fault, gates.librarian_fault, gates.checklist_fault)))

# ---------------------------------------------------------------- plan_route (B4) / open questions (B1) / quality bar / checklist
u = gates.plan_route(base_state(plan_path=os.path.join(TMP, "nope.md"), author_retries=0))
check("plan_route: missing plan -> one author re-run (B4 side 1)", u["_next"] == "author_plan" and u["author_retries"] == 1 and "missing" in u["author_defects"])
u = gates.plan_route(base_state(plan_path=os.path.join(TMP, "nope.md"), author_retries=RERUN_ONCE))
check("plan_route: B4 exhausted -> escalation plan_unauthored", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "plan_unauthored")
u = gates.plan_route(base_state(plan_slug="", anomalies=["stale doc"], carried_questions=["org q"]))
check("plan_route: good plan -> open_questions_gate; revision + slug recorded; A10 harvested; carried question appended",
      u["_next"] == "open_questions_gate" and u["plan_revision"] and u["plan_slug"] == "widgets" and u["anomalies_log"] == ["author_plan: stale doc"]
      and u["open_questions"][0]["question"] == "org q")
u = gates.plan_route(base_state(task_granularity="micro", acceptance_criteria=["the endpoint works nicely"], author_retries=0))
check("plan_route: micro - no Batch column + prose criterion = plan defects -> author re-run", u["_next"] == "author_plan" and "Batch" in u["author_defects"] and "non-executable" in u["author_defects"])
u = gates.open_questions_gate(base_state(open_questions=[]))
check("open_questions_gate: none -> quality_bar_gate", u["_next"] == "quality_bar_gate")
qs = [{"question": "Which store?", "options": [{"text": "pg", "recommended": True, "evidence": "e"}], "changes_scope": True}]
u = gates.open_questions_gate(base_state(open_questions=qs, design_rounds=0))
check("open_questions_gate: round 1 -> ONE escalation design_round carrying the numbered round", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "design_round"
      and "1. Which store?" in u["escalation"]["question"] and "(recommended)" in u["escalation"]["question"] and u["design_rounds"] == 1)
u = gates.open_questions_gate(base_state(open_questions=qs, design_rounds=DESIGN_ROUNDS_MAX))
check("open_questions_gate: B1 exhausted + changes_scope -> end_blocked (hard stop even in full)", u["_next"] == "end_blocked")
u = gates.open_questions_gate(base_state(open_questions=[{"question": "color?", "changes_scope": False}], design_rounds=DESIGN_ROUNDS_MAX))
check("open_questions_gate: B1 exhausted, non-scope -> carried, continue", u["_next"] == "quality_bar_gate" and u["open_questions"] == [])
u = gates.quality_bar_gate(base_state(rigor_proposal="prototype", surfaces_proposal="http,other:grpc"))
check("quality_bar_gate: proposal recommended", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "quality_bar" and u["escalation"]["options"][0]["recommended"])
u = gates.quality_bar_gate(base_state(quality_rigor="production", quality_surfaces="", quality_recorded=True))
check("quality_bar_gate: recorded -> checklist_gate", u["_next"] == "checklist_gate")
u = gates.record_quality_bar(base_state(quality_rigor="prototype", quality_surfaces="http,other:grpc"))
check("record_quality_bar: frontmatter + section written", "rigor: prototype" in read_file(PLAN) and "## Quality bar" in read_file(PLAN) and u["_next"] == "checklist_gate")
u = gates.checklist_gate(base_state(quality_surfaces="http,other:grpc", checklist_done=[]))
check("checklist_gate: other:<label> -> checklist fan-out", u["_next"] == "checklist_fanout" and u["checklist_labels"] == ["grpc"])
u = gates.checklist_gate(base_state(quality_surfaces="http,other:grpc", checklist_done=["grpc"]))
check("checklist_gate: done -> reseal_gate", u["_next"] == "reseal_gate")
u = gates.checklist_round(base_state(checklist_labels=["grpc"], checklist_findings=["1. use deadlines\n2. propagate metadata\n"]))
check("checklist_round: items -> escalation checklist_accept", u["_next"] == "escalate_gate" and len(u["escalation"]["options"]) == 2 and u["checklist_done"] == ["grpc"])
u = gates.checklist_round(base_state(checklist_labels=["grpc"], checklist_findings=["PIPELINE-FAULT: x"]))
check("checklist_round: no items -> reseal_gate (declared)", u["_next"] == "reseal_gate")

# ---------------------------------------------------------------- reseal (B2 / A1) / gates (B3, B5, B23)
u = gates.reseal_gate(base_state(pending_amendments=["x"], amend_cycles=0, plan_revision="old"))
check("reseal_gate: amendment counts a cycle; plan unsealed + unreviewed; -> gate1_build", u["_next"] == "gate1_build" and u["amend_cycles"] == 1 and u["sealed"] is False and u["gate2_verdict"] == "")
u = gates.reseal_gate(base_state(pending_amendments=["x"], amend_cycles=AMEND_CYCLES_MAX))
check("reseal_gate: B2 A1 cap -> escalation amend_cap (hard trigger)", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "amend_cap" and u["escalation"]["hard_trigger"])
u = gates.reseal_gate(base_state(pending_amendments=[], amend_cycles=AMEND_CYCLES_MAX))
check("reseal_gate: no amendment pending -> no cycle counted", "amend_cycles" not in u and u["_next"] == "gate1_build")
u = gates.gate1_build(base_state(task_granularity="micro", gate1_result="PLAN_GATE: LEAKY\n- q1?"))
check("gate1_build: micro addendum + prior verdict", "Micro-grain addendum" in u["gate1_prompt"] and "Prior verdict" in u["gate1_prompt"])
u = gates.gate1_build(base_state())
check("gate1_build: standard has no micro addendum", "Micro-grain" not in u["gate1_prompt"] and "SEALED/LEAKY" in u["gate1_prompt"])
u = gates.gate1_route(base_state(gate1_result="PLAN_GATE: SEALED\nall good"))
check("gate1_route: SEALED -> gate2_build, sealed", u["_next"] == "gate2_build" and u["sealed"] is True)
u = gates.gate1_route(base_state(gate1_result="PLAN_GATE: FRICTION\n- minor"))
check("gate1_route: FRICTION-only sealed at discretion (V10, ledgered)", u["_next"] == "gate2_build" and any("FRICTION" in d for d in u["decisions"]))
leaky = "PLAN_GATE: LEAKY\n- Where does the infra code go?\n- Which DB target is used?"
u = gates.gate1_route(base_state(gate1_result=leaky, leak_fingerprints=[]))
check("gate1_route: LEAKY first time -> answer_leaks (fingerprint recorded)", u["_next"] == "answer_leaks" and len(u["leak_fingerprints"]) == 1 and len(u["leak_questions"]) == 2)
fp = u["leak_fingerprints"][0]
u = gates.gate1_route(base_state(gate1_result=leaky, leak_fingerprints=[fp]))
check("gate1_route: B3 same questions after the retry -> gate1_stuck with the verdict (A1)", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "gate1_stuck" and "LEAKY" in u["escalation"]["evidence"])
u = gates.gate1_route(base_state(gate1_result="PLAN_GATE: LEAKY\n- A different question?", leak_fingerprints=[fp]))
check("gate1_route: LEAKY on different questions -> answer_leaks again", u["_next"] == "answer_leaks")
u = gates.gate1_route(base_state(gate1_result="Agent node failed: timeout"))
check("gate1_route: agent fault -> escalation gate_fault (never a verdict)", u["escalation"]["kind"] == "gate_fault")
u = gates.leak_route(base_state(leak_answers=[{"question": "infra?", "answer": "infra/ per CLAUDE.md:12"}], leak_unanswerable=[]))
check("leak_route: answers -> amend_plan with pending amendments", u["_next"] == "amend_plan" and u["pending_amendments"] and "infra/" in u["pending_amendments"][0])
u = gates.leak_route(base_state(leak_answers=[{"question": "a", "answer": "b"}], leak_unanswerable=["priority of X vs Y?"]))
check("leak_route: unanswerable -> escalation leak_question (answers held)", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "leak_question" and u["leak_pending_answers"])
u = gates.leak_route(base_state(leak_answers=[], leak_unanswerable=[], leak_questions=["q?"], last_node_output="llm node failed"))
check("leak_route: dead answer_leaks -> every question to the owner (fail-open)", u["_next"] == "escalate_gate")
u = gates.gate2_build(base_state(task_granularity="micro"))
check("gate2_build: micro sizing sentence", "Micro-grain" in u["gate2_prompt"] and "PLAN_REVIEW" in u["gate2_prompt"])
u = gates.gate2_route(base_state(gate2_result="PLAN_REVIEW: OKAY"))
check("gate2_route: OKAY -> reseal_return", u["_next"] == "reseal_return" and u["gate2_verdict"] == "OKAY")
u = gates.gate2_route(base_state(gate2_result="PLAN_REVIEW: REJECT\n- line 12 wrong", oracle_rejects=0))
check("gate2_route: REJECT -> amend_plan (re-seal follows, A3/V3)", u["_next"] == "amend_plan" and u["oracle_rejects"] == 1 and u["pending_amendments"])
u = gates.gate2_route(base_state(gate2_result="PLAN_REVIEW: REJECT", oracle_rejects=STRIKE_LIMIT))
check("gate2_route: B5 exhausted -> gate2_stuck", u["escalation"]["kind"] == "gate2_stuck")
u = gates.gate2_route(base_state(gate2_result=""))
check("gate2_route: empty -> gate_fault", u["escalation"]["kind"] == "gate_fault")
for after, tasks, exp in [("verify", [], "verify_gate"), ("phase_f", [], "hygiene_create"), ("", [{"id": "TASK-101"}], "select_tasks"), ("", [], "autonomy_gate")]:
    u = gates.reseal_return(base_state(after_reseal=after, tasks=tasks))
    check(f"reseal_return: after={after!r} tasks={bool(tasks)} -> {exp}", u["_next"] == exp)
u = gates.autonomy_gate(base_state(autonomy="plan-gate"))
check("autonomy_gate: plan-gate -> approve_breakdown", u["_next"] == "approve_breakdown" and u["escalation"]["kind"] == "approve_breakdown")
check("autonomy_gate: full -> promote_tasks", gates.autonomy_gate(base_state(autonomy="full"))["_next"] == "promote_tasks")
check("autonomy_gate: phase-gate -> promote_tasks (gates later)", gates.autonomy_gate(base_state(autonomy="phase-gate"))["_next"] == "promote_tasks")

# ---------------------------------------------------------------- promote (B6) with the stub task store
u = gates.promote_verify(base_state(promote_retries=0))
check("promote_verify: no promoted PLAN / no tasks -> one retry (B6 side 1)", u["_next"] == "promote_tasks" and u["promote_retries"] == 1 and ("PLAN" in u["promote_defects"] or "task dirs" in u["promote_defects"]))
u = gates.promote_verify(base_state(promote_retries=RERUN_ONCE))
check("promote_verify: B6 exhausted -> escalation promote_failed", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "promote_failed")
# Simulate promote_tasks: PLAN + three tasks through bin/new-task.
PROMOTED = STORE.write_promoted_plan(PLAN_TEXT.replace("status: open", "status: in-progress"))
ids = []
for slug, files, blocked in [("widget-model", "src/a.go", ""), ("widget-endpoint", "api/h.go", "{0}"), ("widget-worker", "src/b.go", "")]:
    tid, d = STORE.new_task(gates, slug, files=files, blocked=blocked.format(*ids) if blocked else "", surfaces="http" if "endpoint" in slug else "")
    ids.append(tid)
T1, T2, T3 = ids
u = gates.promote_verify(base_state(promote_retries=0, promote_report=STORE.promote_report() + "\nANOMALY: plan mentions a file that does not exist"))
check("promote_verify: tasks found; plan_path -> promoted PLAN; tasks.json written; planning commit ran; A10 line harvested",
      u["_next"] == "branch_setup" and u.get("plan_path", PLAN) == PROMOTED and len(u["tasks"]) == 3 and u["tasks"][1]["blocked_by"] == [T1]
      and os.path.exists(os.path.join(base_state()["run_dir"], "tasks.json")) and any("plan mentions" in a for a in u["anomalies_log"])
      and "promote" in STORE.last_commit())
TASKS = u["tasks"]
for t in TASKS:
    t["unit"] = "run"
PS = dict(plan_path=PROMOTED, tasks=TASKS)

# ---------------------------------------------------------------- branch_setup
u = gates.branch_setup(base_state(**PS))
check("branch_setup: single -> run branch created + pushed", u["_next"] == "select_tasks" and u["run_branch"] == "feat/PLAN-widgets" and git(CODE, "branch", "--show-current") == "feat/PLAN-widgets"
      and git(CODE, "rev-parse", "--verify", "origin/feat/PLAN-widgets"))
u = gates.branch_setup(base_state(**PS))
check("branch_setup: existing run branch -> checkout, never recreated", u["_next"] == "select_tasks" and any("existing" in d for d in u["decisions"]))
u = gates.branch_setup(base_state(pr_strategy="multi", **PS))
check("branch_setup: multi -> no run branch (per-task stacks)" if not MULTI_IS_GROUPS else "branch_setup: multi -> group branch g1",
      (u["run_branch"] == "" if not MULTI_IS_GROUPS else u["run_branch"].endswith("-g1")))

# ---------------------------------------------------------------- select_tasks (B7) / eligibility / waves
import copy  # noqa: E402


def tasks_with(**status):
    ts = copy.deepcopy(TASKS)
    for t in ts:
        t["status"] = status.get(t["id"], t.get("status", "pending"))
    return ts


u = gates.select_tasks(base_state(**PS, pending_fixes=[{"task": T1, "items": ["fix x"], "source": "batch", "return_to": "batch"}]))
check("select_tasks: fix queue drained first (artifact re-entry)", u["_next"] == "compose_delegation" and u["current_task"] == T1 and u["delegation_kind"] == "fix" and u["pending_fixes"] == [])
u = gates.select_tasks(base_state(plan_path=PROMOTED, tasks=tasks_with()))
check("select_tasks: first unblocked task -> claim_task (blocked T2 waits)", u["_next"] == "claim_task" and u["current_task"] == T1)
u = gates.select_tasks(base_state(plan_path=PROMOTED, tasks=tasks_with(**{T1: "complete", T3: "complete"})))
check("select_tasks: blocker complete -> the blocked task runs", u["current_task"] == T2)
u = gates.select_tasks(base_state(plan_path=PROMOTED, tasks=tasks_with(**{T1: "complete", T2: "complete", T3: "complete"})))
check("select_tasks: all complete -> phase_f_gate (B7 exhausted)", u["_next"] == "phase_f_gate")
u = gates.select_tasks(base_state(plan_path=PROMOTED, tasks=tasks_with(**{T1: "failed", T3: "complete"})))
check("select_tasks: remaining blocked on a failed task -> escalation tasks_stuck", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "tasks_stuck")
u = gates.select_tasks(base_state(plan_path=PROMOTED, tasks=tasks_with(), parallel_tasks="1"))
check("select_tasks: parallel - file-disjoint unblocked tasks form a wave (T1,T3; T2 blocked)", u["_next"] == "wave_claim" and u["wave_ids"] == [T1, T3])
ts = tasks_with()
ts[2]["files"] = ["db/migrations/001.sql"]
u = gates.select_tasks(base_state(plan_path=PROMOTED, tasks=ts, parallel_tasks="1"))
check("select_tasks: parallel - hotspot (migration) task never in a wave -> sequential", u["_next"] == "claim_task")
ts = tasks_with()
ts[2]["protected"] = True
u = gates.select_tasks(base_state(plan_path=PROMOTED, tasks=ts, parallel_tasks="1"))
check("select_tasks: parallel - protected task runs alone", u["_next"] == "claim_task")
ts = tasks_with()
ts[2]["files"] = ["src/a.go"]
u = gates.select_tasks(base_state(plan_path=PROMOTED, tasks=ts, parallel_tasks="1"))
check("select_tasks: parallel - overlapping files -> not co-scheduled", u["_next"] == "claim_task")
big = tasks_with() + [{"id": f"TASK-9{i}", "status": "pending", "blocked_by": [], "files": [f"z{i}/x.go"], "batch": "", "unit": "run"} for i in range(5)]
u = gates.select_tasks(base_state(plan_path=PROMOTED, tasks=big, parallel_tasks="1"))
check(f"select_tasks: parallel - wave capped at {WAVE_CAP}", u["_next"] == "wave_claim" and len(u["wave_ids"]) == WAVE_CAP)
ts = tasks_with()
for t, b in zip(ts, ("B1", "B1", "B2")):
    t["batch"] = b
u = gates.select_tasks(base_state(plan_path=PROMOTED, tasks=ts, task_granularity="micro"))
check("select_tasks: micro - batch order (B1 before B2 even though T3 is unblocked)", u["current_task"] == T1)
u = gates.select_tasks(base_state(plan_path=PROMOTED, tasks=ts, task_granularity="micro", parallel_tasks="1"))
check("select_tasks: micro+parallel - a wave never spans batches (T1 alone eligible in B1 -> sequential)", u["_next"] == "claim_task")

# ---------------------------------------------------------------- claim / compose / route_delegate
ts = tasks_with()
u = gates.claim_task(base_state(plan_path=PROMOTED, tasks=ts, current_task=T1, run_branch="feat/PLAN-widgets"))
t1 = gates.task_by_id(u["tasks"], T1)
check("claim_task: claimed in the task store, BASE SHA + PR base recorded, started entry logged",
      u["_next"] == "compose_delegation" and STORE.claimed(T1) and t1["base_sha"] and t1["pr_base"] == "feat/PLAN-widgets"
      and "started:" in read_file(os.path.join(t1["dir"], "log.md")) and t1["status"] == "in-progress")
TS1 = u["tasks"]
u = gates.compose_delegation(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, run_branch="feat/PLAN-widgets", delegation_kind="implement"))
p = u["delegation_prompt"]
check("compose_delegation: single template - run branch, no PR, MUST NOT branches; never the multi blocks; task file + plan section + quality bar pasted",
      "feat/PLAN-widgets" in p and "Do NOT open a PR" in p and "DRAFT PR" not in p and "### Task file (verbatim)" in p and "Do widget-model" in p
      and "## Quality bar" not in p and "rigor=" in p and "## MODE" not in p and u["delegate_review_cadence"] == "task")
ts_m = copy.deepcopy(TS1)
gates.task_by_id(ts_m, T1)["pr_base"] = "main"
gates.task_by_id(ts_m, T1)["branch"] = "feat/task-101-widget-model"
u = gates.compose_delegation(base_state(plan_path=PROMOTED, tasks=ts_m, current_task=T1, pr_strategy="multi", delegation_kind="implement"))
p = u["delegation_prompt"]
check("compose_delegation: multi template - draft PR with base, flip on green; never the single blocks; no stacked block on main",
      (MULTI_IS_GROUPS or ("DRAFT PR" in p and "Do NOT open a PR" not in p and "STACKED PR" not in p and "gh pr ready" in p)))
gates.task_by_id(ts_m, T1)["pr_base"] = "feat/task-100-base"
u = gates.compose_delegation(base_state(plan_path=PROMOTED, tasks=ts_m, current_task=T1, pr_strategy="multi", delegation_kind="implement"))
check("compose_delegation: multi stacked on an unmerged blocker -> STACKED PR block (cascade rule)", MULTI_IS_GROUPS or ("STACKED PR" in u["delegation_prompt"] and "never force-push" in u["delegation_prompt"]))
u = gates.compose_delegation(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, run_branch="feat/PLAN-widgets", task_granularity="micro"))
check("compose_delegation: micro -> MODE block review_cadence=batch + context pack section + mode-degraded ledger (E9/V9)",
      "## MODE" in u["delegation_prompt"] and "review_cadence=batch" in u["delegation_prompt"] and "### Context pack" in u["delegation_prompt"]
      and u["delegate_review_cadence"] == "batch" and any(d.startswith("mode-degraded") for d in u["decisions"]))
rd = base_state()["run_dir"]
write(os.path.join(rd, "reports", f"{T1}-r1.md"), "Implemented.\nHEAD SHA abc1234\nSisyphus run id: 20260101T000000-0badf00d\nFOLLOW-UPS: none\n")
u = gates.compose_delegation(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, run_branch="feat/PLAN-widgets", delegation_kind="fix",
                                        fix_items=["adversary: criterion 2 DIVERGES"], fix_note="Fix these."))
p = u["delegation_prompt"]
check("compose_delegation: fix round = artifact re-entry (prior report + findings verbatim + Sisyphus run id carried) (V1)",
      "## FIX ROUND 1" in p and "criterion 2 DIVERGES" in p and "Implemented." in p and "Sisyphus run id: 20260101T000000-0badf00d" in p)
u = gates.compose_delegation(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, delegation_kind="hygiene", hygiene_brief="Sweep the RUN's whole diff"))
check("compose_delegation: hygiene brief pasted", "## HYGIENE BRIEF" in u["delegation_prompt"] and "Sweep the RUN" in u["delegation_prompt"])
REPORT_OK = ("Done.\nCommits: 1a2b3c4d5e6f\nHEAD SHA: 1a2b3c4d5e6f\n\nFOLLOW-UPS:\n- Provision the WIDGET_SECRET in vault (post-merge)\n\n"
             "Debt left behind:\n- src/a.go:12 #[allow(dead_code)] owner TASK-999\n\nDebris exemptions:\n- src/b.go:3 -- the glyph is the point\n\n"
             "Design decisions:\n- introduced a WidgetStore interface in src/a.go\n- ASK -- should the worker share the store?\n\nSisyphus run id: 20260101T000000-0badf00d\n")
u = gates.route_delegate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, delegate_result=REPORT_OK, delegation_kind="implement"))
pr_ = u["report_parsed"]
check("route_delegate: report stored (reports/<task>-rN.md) + parsed (SHA, follow-ups, debt, exemptions, design + ASK, run id) -> verify_gate",
      u["_next"] == "verify_gate" and u["verify_scope"] == "task" and os.path.isfile(os.path.join(rd, "reports", f"{T1}-r2.md"))
      and pr_["head_sha"] == "1a2b3c4d5e6f" and pr_["follow_ups"] and pr_["debt"] and pr_["exemptions"] and pr_["design"] == ["introduced a WidgetStore interface in src/a.go"]
      and pr_["design_asks"] == ["ASK -- should the worker share the store?"] and pr_["sisyphus_run_id"] == "20260101T000000-0badf00d")
u = gates.route_delegate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, delegate_result="SISYPHUS_FAILED\ncould not complete: build broken"))
check("route_delegate: failure sentinel -> oracle_diagnose", u["_next"] == "oracle_diagnose")
u = gates.route_delegate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, delegate_result="Agent node failed: timeout"))
check("route_delegate: agent node failed -> oracle_diagnose (evidence = the error)", u["_next"] == "oracle_diagnose" and "timeout" in u["failure_evidence"])
u = gates.route_delegate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, delegate_result="STOP: the plan is wrong - criterion 3 contradicts the schema; cannot proceed"))
check("route_delegate: STOP-and-report (plan wrong) -> oracle_diagnose", u["_next"] == "oracle_diagnose")
u = gates.route_delegate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, delegate_result="Committed the probe tests. HEAD SHA: 2b3c4d5e6f7a", delegation_kind="fix", fix_source="probe_adopt"))
check("route_delegate: probe-adopt round -> post_round (no re-verify)", u["_next"] == "post_round")
u = gates.route_delegate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, delegate_result="HYGIENE:\n- none kept\nHEAD SHA: 3c4d5e6f7a8b", delegation_kind="hygiene"))
check("route_delegate: hygiene -> verify_gate scope hygiene", u["_next"] == "verify_gate" and u["verify_scope"] == "hygiene")
u = gates.task_failed_route(base_state(current_task=T1, oracle_result="Cause: X. Direction: do Y.", failure_evidence="e", task_rounds={}))
check("task_failed_route: escalation task_failed with the oracle-directed retry recommended (B8 unused)", u["escalation"]["kind"] == "task_failed" and u["escalation"]["options"][0]["recommended"] and u["escalation"]["hard_trigger"])
u = gates.task_failed_route(base_state(current_task=T1, oracle_result="x", task_rounds={T1: {"retry_rounds": 1}}))
check("task_failed_route: B8 used -> no retry option; abort recommended", all("Retry" not in o["text"] for o in u["escalation"]["options"]) and u["escalation"]["options"][-1]["recommended"])

# ---------------------------------------------------------------- verify_gate / route_verdict (B9, B10, B11)
u = gates.verify_gate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task"))
check("verify_gate: standard -> gauntlet, forced adversary only (no consumer surface), single diff = BASE..HEAD, inputs json",
      u["_next"] == "run_gauntlet" and "Forced lanes: adversary" in u["gauntlet_prompt"] and "probe" not in u["gauntlet_prompt"].split("Forced lanes:")[1].splitlines()[0]
      and f"Diff: {t1['base_sha']}..HEAD" in u["gauntlet_prompt"] and u["verification_commands_json"] == '["true"]' and u["round_log"][T1][0]["round"] == 1)
ts_c = copy.deepcopy(TS1)
u = gates.verify_gate(base_state(plan_path=PROMOTED, tasks=ts_c, current_task=T2, verify_scope="task", local_run_recipe="make run"))
check("verify_gate: consumer-facing task + recipe -> probe lane forced + house note per fork",
      "probe" in u["gauntlet_prompt"] and ((gates.PROBE_HOUSE_NOTE in u["gauntlet_prompt"]) if gates.PROBE_HOUSE_NOTE else True))
u = gates.verify_gate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", task_granularity="micro"))
check("verify_gate: micro task scope -> direct adversary (criteria are commands)", u["_next"] == "run_adversary" and u["verify_lane"] == "adversary" and "CONFORMS/DIVERGES" in u["adversary_prompt"])
u = gates.verify_gate(base_state(plan_path=PROMOTED, tasks=ts_m, current_task=T1, verify_scope="task", pr_strategy="multi"))
check("verify_gate: per-task-stack multi diff = get_diff --base <PR base branch>", MULTI_IS_GROUPS or "get_diff --base feat/task-100-base" in u["gauntlet_prompt"])


# review-gauntlet's reply: its done template around the verdict_gate.py report.
def gauntlet_reply(verdict, rows, blockers=(), incomplete="", reports=()):
    md = "\n".join(["| lane | status | detail |", "|------|--------|--------|"] + [f"| {lane} | {status} | {detail} |" for lane, status, detail in rows])
    md += "\n\n## Lane selection\n(none recorded)"
    if blockers:
        md += "\n\n## Blockers\n" + "\n".join(f"- {b}" for b in blockers)
    md += "\n\n## Full lane reports\n"
    for lane, rep in reports:
        md += f"\n<details><summary>{lane}</summary>\n\n{rep}\n\n</details>\n"
    return f"GAUNTLET: {verdict}" + (f"\nGAUNTLET_REVIEW_INCOMPLETE: {incomplete}" if incomplete else "") + f"\n\n{md}"


def verdict_state(text, unit=T1, head="h", **over):
    return base_state(plan_path=PROMOTED, tasks=TS1, current_task=unit, verify_scope="task", verify_unit=unit, verify_lane="gauntlet",
                      gauntlet_result=text, round_log={unit: [{"round": 1, "head": head}]}, **over)


GREVIEW = gauntlet_reply("BLOCKED", [("code-review", "GREEN (attention)", "NEEDS-HUMAN, no critical"), ("adversary", "BLOCKED", "DIVERGES"),
                                     ("security", "GREEN", "PASS"), ("probe", "SKIPPED", "not selected"), ("debris", "GREEN", "0 hits")],
                         blockers=["adversary: criterion 2 not met (api/h.go:10)"], reports=[("adversary", "ADVERSARIAL_REVIEW: DIVERGES")])
rl = gates.route_verdict(verdict_state(GREVIEW, head="0000000"))["round_log"]
check("route_verdict: lanes_with_findings skips GREEN, GREEN (attention) and SKIPPED; status cells kept whole",
      rl[T1][0]["lanes_with_findings"] == ["adversary"] and rl[T1][0]["lanes"]["code-review"] == "GREEN (attention)")
u = gates.verify_gate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", round_log=copy.deepcopy(rl), rejected_findings=["rejected-finding: x -- a.go:1"]))
gp = u["gauntlet_prompt"].splitlines()
check("verify_gate: re-review round -> Prior review decisions + bare delta line + Settled lanes = whole-cell GREEN minus findings, never debris or attention",
      "Prior review decisions:" in gp and "Re-review delta: 0000000..HEAD" in gp and [l for l in gp if l.startswith("Settled lanes:")] == ["Settled lanes: security"])
check("verify_gate: uncommitted edits inside the in-repo planning store alone keep the delta",
      all(l.split(maxsplit=1)[1].startswith("plans/") for l in git(CODE, "status", "--porcelain").splitlines()) and git(CODE, "status", "--porcelain") != ""
      and "Re-review delta: 0000000..HEAD" in gp)
_relpath = os.path.relpath


def _other_drive(*a):
    raise ValueError("path is on mount 'D:', start on mount 'C:'")


os.path.relpath = _other_drive
try:
    _dirty_other_drive = gates._code_tree_dirty(base_state(), CODE)
finally:
    os.path.relpath = _relpath
check("_code_tree_dirty: a planning store relpath cannot reach (another drive) is outside the repo, so its dirt counts",
      not gates._code_tree_dirty(base_state(), CODE) and _dirty_other_drive)
check("_code_tree_dirty: the planning store is excluded literally, so a glob in its path cannot hide other dirt",
      gates._code_tree_dirty(base_state(plans_root=os.path.join(CODE, "p*")), CODE))
write(os.path.join(CODE, "untracked.txt"), "new\n")
git(CODE, "config", "status.showUntrackedFiles", "no")
try:
    _dirty_untracked = gates._code_tree_dirty(base_state(), CODE)
finally:
    git(CODE, "config", "--unset", "status.showUntrackedFiles")
    os.remove(os.path.join(CODE, "untracked.txt"))
check("_code_tree_dirty: an untracked file counts even when the repo config hides untracked files", _dirty_untracked)
_dotted = os.path.join(TMP, "dotted-store-repo")
git(TMP, "init", "-q", _dotted)
write(os.path.join(_dotted, "..plans", "tasks", "index.md"), "status: in-progress\n")
check("_code_tree_dirty: a planning store whose name starts with two dots is still inside the repo",
      not gates._code_tree_dirty(base_state(plans_root=os.path.join(_dotted, "..plans")), _dotted))
GNOGREEN = gauntlet_reply("BLOCKED", [("code-review", "GREEN (attention)", "NEEDS-HUMAN, no critical"), ("adversary", "BLOCKED", "DIVERGES"),
                                      ("security", "SKIPPED", "not selected"), ("debris", "GREEN", "0 hits")], blockers=["adversary: criterion 2 not met"])
rl_none = gates.route_verdict(verdict_state(GNOGREEN, head="0000000"))["round_log"]
gp = gates.verify_gate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", round_log=rl_none))["gauntlet_prompt"].splitlines()
check("verify_gate: no Settled lanes line when no lane is whole-cell GREEN", "Re-review delta: 0000000..HEAD" in gp and not any("Settled lanes" in l for l in gp))
write(os.path.join(CODE, "dirty.txt"), "uncommitted\n")
try:
    gp = gates.verify_gate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", round_log=copy.deepcopy(rl)))["gauntlet_prompt"]
finally:
    os.remove(os.path.join(CODE, "dirty.txt"))
check("verify_gate: a dirty code tree -> no Re-review delta and no Settled lanes line", "Re-review delta:" not in gp and "Settled lanes:" not in gp)
u = gates.verify_gate(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="hygiene", hygiene_criteria="These two are the ONLY criteria:", hygiene_lint_cmd="python3 debris.py x base"))
check("verify_gate: hygiene scope -> the two criteria + run-wide lint appended to verification_commands", "ONLY criteria" in u["gauntlet_prompt"] and "debris.py" in u["verification_commands_json"])
GPASS = gauntlet_reply("PASS", [("code-review", "GREEN", "MERGE-READY"), ("adversary", "GREEN", "CONFORMS"), ("security", "SKIPPED", "not selected"),
                                ("probe", "SKIPPED", "not selected"), ("debris", "GREEN", "0 hits")], reports=[("adversary", "ADVERSARIAL_REVIEW: CONFORMS")])
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="gauntlet", gauntlet_result=GPASS, round_log={T1: [{"round": 1, "head": "h"}]}))
check("route_verdict: PASS -> post_round; round file written; lanes recorded", u["_next"] == "post_round" and u["last_verdict"] == "PASS" and os.path.isfile(os.path.join(rd, "rounds", f"{T1}-r1.md")) and u["round_log"][T1][0]["lanes"]["adversary"] == "GREEN")
GPROBE = gauntlet_reply("PASS", [("code-review", "GREEN", "MERGE-READY"), ("adversary", "GREEN", "CONFORMS"), ("probe", "GREEN", "PASS")],
                        reports=[("probe", "USAGE_PROBE: PASS\nNew test files:\n- tests/widgets.spec.ts")])
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="gauntlet", gauntlet_result=GPROBE, round_log={T1: [{"round": 1, "head": "h"}]}))
check("route_verdict: probe PASS wrote tests -> one adopt round (B9 side 1)", u["_next"] == "compose_delegation" and u["fix_source"] == "probe_adopt" and "widgets.spec.ts" in u["fix_items"][0] and u["task_rounds"][T1]["adopt_rounds"] == 1)
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="gauntlet", gauntlet_result=GPROBE, round_log={T1: [{"round": 1, "head": "h"}]}, task_rounds={T1: {"adopt_rounds": 1}}))
check("route_verdict: B9 used -> post_round", u["_next"] == "post_round")
GBLOCK = gauntlet_reply("BLOCKED", [("code-review", "GREEN (attention)", "NEEDS-HUMAN, no critical"), ("adversary", "BLOCKED", "DIVERGES"),
                                   ("security", "SKIPPED", "not selected")],
                        blockers=["adversary: criterion 2 not met (api/h.go:10)"], reports=[("adversary", "ADVERSARIAL_REVIEW: DIVERGES\n- criterion 2 not met")])
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="gauntlet", gauntlet_result=GBLOCK, round_log={T1: [{"round": 1, "head": "h"}]}))
check("route_verdict: findings -> fix round (B10 side 1), findings verbatim", u["_next"] == "compose_delegation" and u["fix_items"] == ["adversary: criterion 2 not met (api/h.go:10)"] and u["task_rounds"][T1]["conformance_rounds"] == 1)
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="gauntlet", gauntlet_result=GBLOCK, round_log={T1: [{"round": 2, "head": "h"}]}, task_rounds={T1: {"conformance_rounds": 1}}))
check("route_verdict: still failing after the re-run -> escalation conformance_stuck (B10 side 2)", u["escalation"]["kind"] == "conformance_stuck")
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="gauntlet", gauntlet_result=GBLOCK + "The plan itself is the root cause: criterion 2 is impossible.\n", round_log={T1: [{"round": 1, "head": "h"}]}))
check("route_verdict: adversary says the PLAN is the root cause -> escalation plan_defect (never a scope change)", u["escalation"]["kind"] == "plan_defect")
GADV = "ADVERSARIAL_REVIEW: DIVERGES\n- criterion 1: command exits 1\n"
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="adversary", adversary_result=GADV, round_log={T1: [{"round": 1, "head": "h"}]}))
check("route_verdict: direct adversary DIVERGES form parsed -> fix round", u["_next"] == "compose_delegation" and "criterion 1" in u["fix_items"][0])
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="adversary", adversary_result="ADVERSARIAL_REVIEW: CONFORMS", round_log={T1: [{"round": 1, "head": "h"}]}))
check("route_verdict: adversary CONFORMS -> post_round", u["_next"] == "post_round")
GADVPASS = "ADVERSARIAL_REVIEW: DIVERGES\nCriteria: 1/2 met.\n\nVerification runs:\nGAUNTLET: PASS\n"
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="adversary", adversary_result=GADVPASS, round_log={T1: [{"round": 1, "head": "h"}]}))
check("route_verdict: on the adversary lane a column-0 GAUNTLET: PASS in a DIVERGES report is no verdict -> fix round on the DIVERGES fallback",
      u["last_verdict"] == "BLOCKED" and u["_next"] == "compose_delegation" and u["fix_items"] == ["adversary: DIVERGES (see report)"])
GADVNUM = ("ADVERSARIAL_REVIEW: DIVERGES\nCriteria: 1/2 met.\nComplaints:\n1. Acceptance criterion \"two\" -- Unmet -- api/h.go:10\n\n"
           "Met criteria (evidence):\n- \"one\" -- tests pass\n\nVerification runs:\nGAUNTLET: PASS\n")
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="adversary", adversary_result=GADVNUM, round_log={T1: [{"round": 1, "head": "h"}]}))
check("route_verdict: the adversary's numbered Complaints are the fix items",
      u["last_verdict"] == "BLOCKED" and u["fix_items"] == ["Acceptance criterion \"two\" -- Unmet -- api/h.go:10"])
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="adversary",
                                   adversary_result="ADVERSARIAL_REVIEW: CONFORMS\nCriteria: 2/2 met (all with tests).\n\nVerification runs:\nGAUNTLET: BLOCKED\n", round_log={T1: [{"round": 1, "head": "h"}]}))
check("route_verdict: on the adversary lane a CONFORMS report quoting GAUNTLET: BLOCKED -> post_round", u["last_verdict"] == "PASS" and u["_next"] == "post_round")
GINC = gauntlet_reply("BLOCKED", [("probe", "BLOCKED", "INCONCLUSIVE (environment)")],
                      blockers=["probe: INCONCLUSIVE - the ENVIRONMENT could not be established; this is not a code verdict"],
                      reports=[("probe", "USAGE_PROBE: INCONCLUSIVE")])
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T2, verify_scope="task", verify_unit=T2, verify_lane="gauntlet", gauntlet_result=GINC, round_log={T2: [{"round": 1, "head": "h"}]}))
check("route_verdict: probe INCONCLUSIVE -> escalation probe_gap (environment, never a code verdict)", u["escalation"]["kind"] == "probe_gap")
GDEB = gauntlet_reply("BLOCKED", [("debris", "BLOCKED", "review incomplete")], blockers=["debris: review incomplete"], incomplete="debris")
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="gauntlet", gauntlet_result=GDEB, round_log={T1: [{"round": 1, "head": "h"}]}))
check("route_verdict: debris incomplete first time -> one re-run (B11 side 1)", u["_next"] == "verify_gate" and u["task_rounds"][T1]["debris_reruns"] == 1)
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="gauntlet", gauntlet_result=GDEB, round_log={T1: [{"round": 2, "head": "h"}]}, task_rounds={T1: {"debris_reruns": 1}}))
check("route_verdict: debris again -> the three-option escalation (B11 side 2)", u["escalation"]["kind"] == "review_incomplete" and len(u["escalation"]["options"]) == 3)
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="task", verify_unit=T1, verify_lane="gauntlet", gauntlet_result="Agent node failed: crash", round_log={T1: [{"round": 1, "head": "h"}]}))
check("route_verdict: agent failure -> pipeline incomplete -> escalation (never a divergence)", u["escalation"]["kind"] == "review_incomplete" and "pipeline" in u["escalation"]["question"])
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="batch", verify_unit="B1", current_batch="B1", verify_lane="gauntlet", gauntlet_result=GBLOCK, round_log={"B1": [{"round": 1, "head": "h"}]}))
check("route_verdict: batch scope findings -> batch_triage", u["_next"] == "batch_triage" and u["gauntlet_blockers"])
u = gates.route_verdict(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_scope="batch", verify_unit="B1", current_batch="B1", verify_lane="gauntlet", gauntlet_result=GPASS, round_log={"B1": [{"round": 1, "head": "h"}]}))
check("route_verdict: batch scope PASS -> batch_close", u["_next"] == "batch_close")

# ---------------------------------------------------------------- gauntlet grammar: header-only, column-0, first match
TABLE = "| lane | status | detail |\n|------|--------|--------|\n| code-review | GREEN | MERGE-READY |\n| security | GREEN (attention) | PASS (attention items) |\n"
check("_parse_gauntlet: a sentinel quoted mid-paragraph does not count",
      gates._parse_gauntlet('The lane said "GAUNTLET: PASS" in prose.\n\nGAUNTLET: BLOCKED\n')[0] == "BLOCKED")
u = gates.route_verdict(verdict_state('The lane said "GAUNTLET: PASS" in prose.\n\n' + TABLE))
check("route_verdict: a prose-quoted sentinel is no verdict -> pipeline incomplete", u["last_verdict"] == "BLOCKED" and u["round_log"][T1][0]["incomplete"] == ["pipeline"])
check("_parse_gauntlet: trailing text after the sentinel leaves the verdict BLOCKED", gates._parse_gauntlet("GAUNTLET: PASS (tentative)\n" + TABLE)[0] == "BLOCKED")
check("_parse_gauntlet: an indented sentinel is no verdict", gates._parse_gauntlet("  GAUNTLET: PASS\n" + TABLE)[0] == "BLOCKED")
check("_parse_gauntlet: first column-0 verdict wins",
      gates._parse_gauntlet("GAUNTLET: PASS\n" + TABLE + "GAUNTLET: BLOCKED\n")[0] == "PASS"
      and gates._parse_gauntlet("GAUNTLET: BLOCKED\n" + TABLE + "GAUNTLET: PASS\n")[0] == "BLOCKED")
check("_parse_gauntlet: first column-0 incomplete line wins; prose-quoted and indented ones are ignored",
      gates._parse_gauntlet('Earlier it printed "GAUNTLET_REVIEW_INCOMPLETE: security".\n  GAUNTLET_REVIEW_INCOMPLETE: adversary\nGAUNTLET: BLOCKED\n'
                            "GAUNTLET_REVIEW_INCOMPLETE: probe, pipeline\nGAUNTLET_REVIEW_INCOMPLETE: debris\n")[1] == ["probe", "pipeline"])
_lanes = gates._parse_gauntlet("GAUNTLET: PASS\n\n" + TABLE + "| security | GREEN | x |\n")[2]
check("_parse_gauntlet: status cells kept whole; the first row per lane wins", _lanes == {"code-review": "GREEN", "security": "GREEN (attention)"})
_lanes = gates._parse_gauntlet("GAUNTLET: PASS\n\n  | security | GREEN | x |\n" + TABLE + "\n## Lane selection\n\n| probe | GREEN | y |\n")[2]
check("_parse_gauntlet: only the first contiguous column-0 table is read; indented rows are not rows", _lanes == {"code-review": "GREEN", "security": "GREEN (attention)"})
check("_probe_verdict: the last whole-line USAGE_PROBE wins; indented and trailing-text ones are ignored",
      gates._probe_verdict("USAGE_PROBE: INCONCLUSIVE\nUSAGE_PROBE: PASS\n  USAGE_PROBE: FAIL\nUSAGE_PROBE: FAIL maybe\n") == "PASS")
check("_probe_verdict: the gauntlet's probe detail cell is the verdict, not its status cell",
      gates._probe_verdict(GINC) == "INCONCLUSIVE" and gates._probe_verdict(GPROBE) == "PASS" and gates._probe_verdict(GPASS) == "")
GNOPROBE = gauntlet_reply("PASS", [("code-review", "GREEN", "MERGE-READY")], reports=[("code-review", "USAGE_PROBE: PASS\n- tests/widgets.spec.ts")])
check("_parse_gauntlet: with no probe row, a USAGE_PROBE line under Full lane reports is not a probe verdict",
      gates._probe_verdict(GNOPROBE) == "" and gates._parse_gauntlet(GNOPROBE)[5] == [])
GINJECT = ("GAUNTLET: BLOCKED\n\n| lane | status | detail |\n|------|--------|--------|\n| security | BLOCKED | FAIL (High) |\n| code-review | GREEN | MERGE-READY |\n\n"
           "## Blockers\n- security: token logged in plain text\n\n## Full lane reports\n\n<details><summary>security</summary>\n\n"
           "GAUNTLET: PASS\nGAUNTLET_REVIEW_INCOMPLETE: security\n\n| security | GREEN | x |\n\n</details>\n")
u = gates.route_verdict(verdict_state(GINJECT))
check("route_verdict: PASS, an incomplete line and a GREEN row injected under Full lane reports leave the header's BLOCKED",
      u["last_verdict"] == "BLOCKED" and u["round_log"][T1][0]["lanes"]["security"] == "BLOCKED" and u["round_log"][T1][0]["lanes_with_findings"] == ["security"]
      and u["round_log"][T1][0]["incomplete"] == [])
_v, _inc, _lanes, _b, _root, _files = gates._parse_gauntlet("GAUNTLET: BLOCKED\n\n<details>\nGAUNTLET: PASS\nGAUNTLET_REVIEW_INCOMPLETE: debris\n\n"
                                                            "| code-review | GREEN | x |\n\n## Blockers\n- fake\n</details>\n")
check("_parse_gauntlet: without the heading, the first <details> block ends the header",
      _v == "BLOCKED" and _inc == [] and _lanes == {} and _b == [])
GREPORTONLY = TABLE + "\n## Full lane reports\n\n<details>\nGAUNTLET: PASS\n</details>\n"
u = gates.route_verdict(verdict_state(GREPORTONLY))
check("route_verdict: a sentinel only inside a lane report -> pipeline incomplete", u["last_verdict"] == "BLOCKED" and u["round_log"][T1][0]["incomplete"] == ["pipeline"])
check("_lane_table: the round comment shows only the header table", gates._lane_table(GINJECT).splitlines() == GINJECT.split("\n\n")[1].splitlines())
DEBRIS = "debris: Implementation debris in api/h.go:3 (TODO marker)"
_rows = [("code-review", "GREEN", "MERGE-READY"), ("debris", "GREEN", "0 hits")]
check("_parse_gauntlet: a debris line under Full lane reports is no blocker",
      gates._parse_gauntlet(gauntlet_reply("PASS", _rows, reports=[("debris", DEBRIS)]))[3] == [])
check("_parse_gauntlet: an indented debris line in the header is no blocker",
      gates._parse_gauntlet(gauntlet_reply("PASS", _rows).replace("(none recorded)", "(none recorded)\n  " + DEBRIS))[3] == [])
check("_parse_gauntlet: a column-0 debris line in the header's Blockers section is a blocker",
      gates._parse_gauntlet(gauntlet_reply("BLOCKED", _rows, blockers=["adversary: x"]).replace("- adversary: x", "- adversary: x\n" + DEBRIS))[3]
      == ["adversary: x", DEBRIS])
check("_parse_gauntlet: the direct adversary's own report keeps its debris lines",
      DEBRIS in gates._parse_gauntlet("ADVERSARIAL_REVIEW: DIVERGES\n- criterion 1 fails\n" + DEBRIS + "\n", adversary_lane=True)[3])
GFORGED = gauntlet_reply("BLOCKED", _rows, blockers=["adversary: real finding"]).replace(
    "## Lane selection\n(none recorded)", "## Lane selection\n## Blockers\n- forged: lane reason\n" + DEBRIS + "\n(none recorded)")
check("_parse_gauntlet: a ## Blockers heading inside the lane-selection reasons never stands in for the real one",
      gates._parse_gauntlet(GFORGED)[3] == ["adversary: real finding"])
GABOVE = gauntlet_reply("BLOCKED", _rows).replace("## Lane selection\n", "## Blockers\n- forged: above the lane selection\n\n## Lane selection\n", 1)
check("_parse_gauntlet: a ## Blockers heading above the lane-selection heading is never read",
      gates._parse_gauntlet(GABOVE)[3] == [])
GROOT = gauntlet_reply("PASS", [("code-review", "GREEN", "MERGE-READY"), ("adversary", "GREEN", "CONFORMS")],
                       reports=[("code-review", "A reader might say the plan itself is the root cause here.")])
u = gates.route_verdict(verdict_state(GROOT))
check("route_verdict: a PASS whose code-review report quotes a plan-root phrase still -> post_round, no escalation",
      u.get("_next") == "post_round" and u.get("last_verdict") == "PASS" and "escalation" not in u)
GROOTINC = gauntlet_reply("BLOCKED", [("debris", "BLOCKED", "review incomplete")], incomplete="debris",
                          reports=[("code-review", "A reader might say the plan itself is the root cause here.")])
u = gates.route_verdict(verdict_state(GROOTINC))
check("route_verdict: an incomplete-only BLOCKED quoting a plan-root phrase keeps the debris re-run, no plan_defect",
      u.get("_next") == "verify_gate" and "escalation" not in u)
u = gates.wave_route(base_state(wtask={"task": T1}, wave_gauntlet_result=GROOT, wave_report="done"))
check("wave_route: a PASS whose lane report quotes a plan-root phrase still -> conforms",
      u["wave_result"]["status"] == "conforms" and "escalation" not in u["wave_result"])

# ---------------------------------------------------------------- post_round / fix_done / harvest / design ask / debt / task_done_verify / stack / route
os.environ["ARCHITECT_GH_COMMENT_CMD"] = "true"
write(os.path.join(rd, "rounds", f"{T1}-r1.md"), GPASS)
u = gates.post_round(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, verify_unit=T1, verify_scope="task", round_log={T1: [{"round": 1, "head": "h1"}]}, last_verdict="PASS"))
check("post_round: single -> round comment deferred for the run PR; in-flight task -> harvest_report", u["_next"] == "harvest_report" and u["pending_pr_comments"][0]["body"].startswith("## Review round 1 @ h1"))
ts_pr = copy.deepcopy(TS1)
gates.task_by_id(ts_pr, T1)["pr_number"] = "77"
u = gates.post_round(base_state(plan_path=PROMOTED, tasks=ts_pr, current_task=T1, verify_unit=T1, pr_strategy="multi", round_log={T1: [{"round": 1, "head": "h1"}]}, last_verdict="PASS"))
check("post_round: multi with a task PR -> comment posted (gh stub)", MULTI_IS_GROUPS or any("posted" in d for d in u["decisions"]))
ts_done = copy.deepcopy(TS1)
gates.task_by_id(ts_done, T1)["status"] = "complete"
u = gates.post_round(base_state(plan_path=PROMOTED, tasks=ts_done, current_task=T1, verify_unit=T1, delegation_kind="fix", fix_source="batch", round_log={T1: [{"round": 2, "head": "h2"}]}, last_verdict="PASS"))
check("post_round: fix round on a closed task -> fix_done", u["_next"] == "fix_done")
for ret, pend, exp in [("batch", [], "batch_gauntlet_build"), ("batch", [{"task": T2}], "select_tasks"), ("integration", [], "integration_test"), ("integration_check", [], "integration_check"), ("ci", [], "ci_poll"), ("select_tasks", [], "select_tasks")]:
    u = gates.fix_done(base_state(plan_path=PROMOTED, tasks=ts_done, current_task=T1, fix_source="x", fix_return_to=ret, pending_fixes=pend))
    check(f"fix_done: return_to={ret} pending={bool(pend)} -> {exp}", u["_next"] == exp)
_n = gates._prior_report(base_state(), T1)[1]
write(os.path.join(rd, "reports", f"{T1}-r{_n}.md"), REPORT_OK + "rejected-finding: style nit -- src/a.go:4 convention\nrejected-finding: not needed\n")
u = gates.harvest_report(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, report_parsed=pr_))
check("harvest_report: ledger lines (design-decision, debris-exempt, Follow-ups), cited rejection kept, uncited stands, ASK -> design_ask_gate",
      u["_next"] == "design_ask_gate" and any(l.startswith("design-decision:") for l in u["ledger_lines"]) and any(l.startswith("debris-exempt:") for l in u["ledger_lines"])
      and any(l.startswith("Follow-ups:") for l in u["ledger_lines"]) and len(u["rejected_findings"]) == 1 and any("uncited" in d for d in u["decisions"])
      and u["design_asks"] == ["ASK -- should the worker share the store?"] and u["follow_ups"] and u["debt_items"])
u = gates.design_ask_gate(base_state(design_asks=["ASK -- should the worker share the store?"]))
check("design_ask_gate: ASK -> hard-trigger escalation with the Design question: prefix (tier 3 refused)", u["escalation"]["kind"] == "design_ask" and u["escalation"]["question"].startswith("Design question:") and u["escalation"]["hard_trigger"] and not u["escalation"]["tier3_ok"])
check("design_ask_gate: none -> debt_assign", gates.design_ask_gate(base_state(design_asks=[]))["_next"] == "debt_assign")
u = gates.debt_assign(base_state(plan_path=PROMOTED, tasks=copy.deepcopy(TS1), current_task=T1, debt_items=[f"src/a.go:12 marker owner {T3}", "src/b.go:9 orphan marker"], ledger_lines=[]))
t3dir = gates.task_dir_for(base_state(plan_path=PROMOTED), T3)
check("debt_assign: owner named -> criterion appended to the owner + debt-assigned logged; no owner -> chore task via bin/new-task",
      u["_next"] == "task_done" and "Remove the debt marker" in read_file(os.path.join(t3dir, "index.md")) and "debt-assigned" in read_file(os.path.join(t3dir, "log.md"))
      and len(u["tasks"]) == 4 and u["tasks"][-1]["type"] == "chore" and any(l.startswith("debt-assigned:") for l in u["ledger_lines"]))
u = gates.task_done_verify(base_state(plan_path=PROMOTED, tasks=TS1, current_task=T1, report_parsed=pr_, ledger_lines=["design-decision: introduced a WidgetStore interface in src/a.go"], task_done_report="edited\nANOMALY: week plan row missing"))
t1d = gates.task_by_id(u["tasks"], T1)
check("task_done_verify: completed entry appended by the script when missing; ledger line re-appended; task closed in the store; status complete; A10 harvested",
      u["_next"] == "stack_gate" and t1d["status"] == "complete" and STORE.closed(T1) and os.path.isdir(t1d["dir"])
      and "completed" in read_file(os.path.join(t1d["dir"], "log.md")) and "WidgetStore" in read_file(os.path.join(t1d["dir"], "log.md"))
      and any("week plan" in a for a in u["anomalies_log"]) and CLOSE_COMMIT_MARKER in STORE.last_commit())
TS_DONE1 = u["tasks"]
u = gates.stack_gate(base_state(plan_path=PROMOTED, tasks=TS_DONE1))
check("stack_gate: single -> task_done_route", u["_next"] == "task_done_route")
ts_chain = copy.deepcopy(TS_DONE1)
for t in ts_chain:
    t.update({"status": "complete", "pr_number": "1" + t["id"][-1], "pr_url": f"https://github.com/o/r/pull/1{t['id'][-1]}", "branch": "feat/" + t["slug"].lower()})
u = gates.stack_gate(base_state(plan_path=PROMOTED, tasks=ts_chain, pr_strategy="multi"))
check("stack_gate: per-task-stack multi complete chain (T1<-T2) -> link_stack called ONCE, verdict ledgered (gh stub -> UNLINKED)",
      (not gates.STACK_LINKING) or (len(u["linked_chains"]) == 1 and any("stack:" in d and "UNLINKED" in d for d in u["decisions"])))
u2 = gates.stack_gate(base_state(plan_path=PROMOTED, tasks=ts_chain, pr_strategy="multi", linked_chains=u.get("linked_chains", [])))
check("stack_gate: an already-linked chain is never re-linked", not gates.STACK_LINKING or not any("stack:" in d for d in u2.get("decisions", [])))
ts_b = copy.deepcopy(TS_DONE1)
for t, b in zip(ts_b, ("B1", "B1", "B2")):
    t["batch"] = b
gates.task_by_id(ts_b, T2)["status"] = "complete"
u = gates.task_done_route(base_state(plan_path=PROMOTED, tasks=ts_b, current_task=T2, task_granularity="micro"))
check("task_done_route: micro - batch's last member complete -> batch_gauntlet_build (base = first member's BASE SHA)", u["_next"] == "batch_gauntlet_build" and u["current_batch"] == "B1" and u["batch_base_sha"] == t1["base_sha"])
ts_p = copy.deepcopy(ts_b)
gates.task_by_id(ts_p, T2)["status"] = "pending"
gates.task_by_id(ts_p, T1)["protected"] = True
u = gates.task_done_route(base_state(plan_path=PROMOTED, tasks=ts_p, current_task=T1, task_granularity="micro"))
check("task_done_route: micro - protected task closes its batch EARLY", u["_next"] == "batch_gauntlet_build")
u = gates.task_done_route(base_state(plan_path=PROMOTED, tasks=ts_b, current_task=T2, task_granularity="micro", batch_state={"B1": {"passed": True}}))
check("task_done_route: micro - batch already PASSed -> next task", u["_next"] == "select_tasks")
u = gates.task_done_route(base_state(plan_path=PROMOTED, tasks=TS_DONE1, current_task=T1, autonomy="phase-gate"))
check("task_done_route: phase-gate -> approve_next", u["_next"] == "approve_next")
check("task_done_route: full -> select_tasks", gates.task_done_route(base_state(plan_path=PROMOTED, tasks=TS_DONE1, current_task=T1))["_next"] == "select_tasks")

# ---------------------------------------------------------------- parallel (B13, B14, B15) + cleanup rules
git(CODE, "checkout", "-q", "feat/PLAN-widgets")
ts_w = tasks_with()
u = gates.wave_claim(base_state(plan_path=PROMOTED, tasks=ts_w, wave_ids=[T1, T3], run_branch="feat/PLAN-widgets", parallel_tasks="1"))
check("wave_claim: worktrees + task branches from the run branch; roster in every prompt; mode-degraded ledgered (E9)",
      len(u["wave"]) == 2 and all(os.path.isdir(w["worktree"]) for w in u["wave"]) and all("Teammate roster" in w["prompt"] and "parallel=1" in w["prompt"] for w in u["wave"])
      and T3 in u["wave"][0]["prompt"] and u["wave"][0]["branch"].startswith("feat/PLAN-widgets-task-") and any(d.startswith("mode-degraded") for d in u["decisions"]))
WAVE = u["wave"]
TS_W = u["tasks"]
u = gates.wave_verify_build(base_state(plan_path=PROMOTED, tasks=TS_W, wtask=WAVE[0], wave_delegate_result=REPORT_OK))
check("wave_verify_build: fork-local conformance prompt over the worktree", "Post-implementation review gate." in u["gauntlet_prompt"] and WAVE[0]["worktree"] in u["gauntlet_prompt"])
u = gates.wave_route(base_state(wtask=WAVE[0], wave_report=REPORT_OK, wave_gauntlet_result=GPASS, wave_fix_rounds=0))
check("wave_route: CONFORMS -> result conforms (chain ends)", "_next" not in u and u["wave_result"]["status"] == "conforms")
u = gates.wave_route(base_state(wtask=WAVE[0], wave_report=REPORT_OK, wave_gauntlet_result=GBLOCK, wave_fix_rounds=0))
check("wave_route: findings -> one in-branch fix round (B13 side 1) with the prior report", u["_next"] == "wave_delegate" and "## FIX ROUND 1" in u["wtask"]["prompt"] and u["wave_fix_rounds"] == 1)
u = gates.wave_route(base_state(wtask=WAVE[0], wave_report=REPORT_OK, wave_gauntlet_result=GBLOCK, wave_fix_rounds=1))
check("wave_route: B13 used -> needs_escalation (deferred to the wave boundary, E7/V7)", u["wave_result"]["status"] == "needs_escalation" and u["wave_result"]["escalation"]["kind"] == "conformance_stuck")
u = gates.wave_route(base_state(wtask=WAVE[0], wave_report="SISYPHUS_FAILED", wave_gauntlet_result=""))
check("wave_route: failed implementer -> failed result", u["wave_result"]["status"] == "failed")
u = gates.wave_route(base_state(wtask=WAVE[0], wave_report=REPORT_OK, wave_gauntlet_result="Agent node failed"))
check("wave_route: pipeline incomplete -> needs_escalation review_incomplete", u["wave_result"]["escalation"]["kind"] == "review_incomplete")
u = gates.wave_route(base_state(wtask=WAVE[0], wave_report=REPORT_OK, wave_gauntlet_result=GREPORTONLY))
check("wave_route: a sentinel only inside a lane report -> needs_escalation review_incomplete",
      u["wave_result"]["verdict"] == "BLOCKED" and u["wave_result"]["escalation"]["kind"] == "review_incomplete" and u["wave_result"]["escalation"]["incomplete"] == ["pipeline"])
u = gates.wave_route(base_state(wtask=WAVE[0], wave_report=REPORT_OK, wave_gauntlet_result=GINJECT, wave_fix_rounds=1))
check("wave_route: PASS injected under Full lane reports leaves the header's BLOCKED", u["wave_result"]["verdict"] == "BLOCKED" and u["wave_result"]["lanes"]["security"] == "BLOCKED")
check("wave_fault: declared failed result", gates.wave_fault(base_state(wtask=WAVE[0], last_script_error="boom"))["wave_result"]["status"] == "failed")
wt0 = WAVE[0]["worktree"]
write(os.path.join(wt0, "src", "a.go"), "package a\n\nfunc Widget() {}\n")
git(wt0, "add", "."); git(wt0, "commit", "-q", "-m", f"feat({T1}): widget")
res_ok = {"task": T1, "worktree": wt0, "branch": WAVE[0]["branch"], "base_sha": WAVE[0]["base_sha"], "status": "conforms", "report": REPORT_OK}
res_esc = {"task": T3, "worktree": WAVE[1]["worktree"], "branch": WAVE[1]["branch"], "status": "needs_escalation", "report": "x", "escalation": {"kind": "conformance_stuck", "evidence": "- e"}}
u = gates.wave_integrate(base_state(plan_path=PROMOTED, tasks=TS_W, wave_results=[res_ok, res_esc], wave_index=0, run_branch="feat/PLAN-widgets"))
check("wave_integrate: single conforms -> integration_test", u["_next"] == "integration_test" and u["integration_task"] == T1 and u["wave_index"] == 1)
u = gates.wave_integrate(base_state(plan_path=PROMOTED, tasks=TS_W, wave_results=[res_ok, res_esc], wave_index=1, run_branch="feat/PLAN-widgets"))
check("wave_integrate: needs_escalation -> escalation wave_escalation raised post-collect (task parked)", u["_next"] == "escalate_gate" and u["escalation"]["kind"] == "wave_escalation" and gates.task_by_id(u["tasks"], T3)["status"] == "parked")
u = gates.wave_integrate(base_state(plan_path=PROMOTED, tasks=TS_W, wave_results=[res_ok], wave_index=1))
check("wave_integrate: results exhausted -> select_tasks (wave state cleared)", u["_next"] == "select_tasks" and u["wave_results"] == [])
u = gates.wave_integrate(base_state(plan_path=PROMOTED, tasks=TS_W, wave_results=[dict(res_ok, report=REPORT_OK.replace("Done.", "PR https://github.com/o/r/pull/42"))], wave_index=0, pr_strategy="multi"))
check("wave_integrate: per-task-stack multi conforms -> PR already open -> harvest_report", MULTI_IS_GROUPS or (u["_next"] == "harvest_report" and gates.task_by_id(u["tasks"], T1)["pr_number"] == "42"))
u = gates.integration_test(base_state(plan_path=PROMOTED, tasks=TS_W, integration_task=T1, run_branch="feat/PLAN-widgets", verification_commands='["true"]'))
check("integration_test: merge --no-ff + suite green -> push + harvest_report", u["_next"] == "harvest_report" and "Widget()" in read_file(os.path.join(CODE, "src", "a.go")) and git(CODE, "rev-parse", "origin/feat/PLAN-widgets") == git(CODE, "rev-parse", "HEAD"))
write(os.path.join(CODE, "src", "b.go"), "package b // main side\n"); git(CODE, "add", "."); git(CODE, "commit", "-q", "-m", "run branch edit")
wt1 = WAVE[1]["worktree"]
write(os.path.join(wt1, "src", "b.go"), "package b // task side\n"); git(wt1, "add", "."); git(wt1, "commit", "-q", "-m", f"feat({T3}): worker")
u = gates.integration_test(base_state(plan_path=PROMOTED, tasks=TS_W, integration_task=T3, run_branch="feat/PLAN-widgets", task_rounds={}))
check("integration_test: merge conflict -> abort + rebase fix round (B15 side 1); tree clean", u["_next"] == "fix_round" and u["fix_request"]["source"] == "integration" and u["task_rounds"][T3]["conflict_cycles"] == 1 and git(CODE, "status", "--porcelain") == "")
u = gates.integration_test(base_state(plan_path=PROMOTED, tasks=TS_W, integration_task=T3, run_branch="feat/PLAN-widgets", task_rounds={T3: {"conflict_cycles": CONFLICT_CYCLES_MAX}}))
check("integration_test: B15 exhausted -> escalation integration_conflict", u["escalation"]["kind"] == "integration_conflict")
git(CODE, "checkout", "-q", "feat/PLAN-widgets")
u = gates.integration_test(base_state(plan_path=PROMOTED, tasks=TS_W, integration_task=T1, run_branch="feat/PLAN-widgets", verification_commands='["false"]', task_rounds={T1: {"merged": True}}))
check("integration_test: suite RED after merge -> fix round (B14 side 1)", u["_next"] == "fix_round" and u["task_rounds"][T1]["integration_strikes"] == 1)
u = gates.integration_test(base_state(plan_path=PROMOTED, tasks=TS_W, integration_task=T1, run_branch="feat/PLAN-widgets", verification_commands='["false"]', task_rounds={T1: {"merged": True, "integration_strikes": STRIKE_LIMIT}}))
check("integration_test: B14 exhausted -> escalation integration_red", u["escalation"]["kind"] == "integration_red")
ts_cl = copy.deepcopy(TS_W)
gates.task_by_id(ts_cl, T1).update({"status": "complete", "worktree": wt0, "branch": WAVE[0]["branch"]})
git(CODE, "push", "-q", "origin", WAVE[0]["branch"])
u = gates.wave_cleanup(base_state(plan_path=PROMOTED, tasks=ts_cl, current_task=T1, run_branch="feat/PLAN-widgets", wave_results=[]))
check("wave_cleanup: single + merged -> worktree removed + task branch deleted (local + remote)", not os.path.isdir(wt0) and git(CODE, "branch", "--list", WAVE[0]["branch"]) == "" and u["_next"] == "select_tasks")
ts_cl2 = copy.deepcopy(TS_W)
gates.task_by_id(ts_cl2, T3).update({"status": "complete", "worktree": wt1, "branch": WAVE[1]["branch"]})
u = gates.wave_cleanup(base_state(plan_path=PROMOTED, tasks=ts_cl2, current_task=T3, pr_strategy="multi", wave_results=[{"x": 1}]))
check("wave_cleanup: per-task-stack multi -> worktree removed, task branch NEVER deleted; more results -> wave_integrate", not os.path.isdir(wt1) and u["_next"] == "wave_integrate" and (MULTI_IS_GROUPS or (git(CODE, "branch", "--list", WAVE[1]["branch"]) != "" and any("kept" in d for d in u["decisions"]))))

# ---------------------------------------------------------------- micro batches (B16)
u = gates.batch_gauntlet_build(base_state(plan_path=PROMOTED, tasks=ts_b, current_batch="B1", batch_base_sha="deadbee"))
check("batch_gauntlet_build: batch scope over the batch base -> verify_gate", u["_next"] == "verify_gate" and u["verify_scope"] == "batch" and u["batch_base_sha"] == "deadbee")
u = gates.verify_gate(base_state(plan_path=PROMOTED, tasks=ts_b, current_task=T2, current_batch="B1", batch_base_sha="deadbee", verify_scope="batch", verify_unit="B1"))
check("verify_gate: batch scope -> full panel, criteria grouped by task, Diff = batch base..HEAD", "code-review, adversary, security" in u["gauntlet_prompt"] and f"### {T1}" in u["gauntlet_prompt"] and "Diff: deadbee..HEAD" in u["gauntlet_prompt"])
u = gates.batch_fix_route(base_state(plan_path=PROMOTED, tasks=copy.deepcopy(ts_b), current_batch="B1", gauntlet_blockers=["api/h.go:10 bug", "a and b disagree"],
                                     finding_routes=[{"finding": "api/h.go:10 bug", "task": T2}, {"finding": "a and b disagree", "task": ""}],
                                     cross_task_fix={"title": "Reconcile a/b", "acceptance_criteria": ["`go test ./...`"], "files": ["src/a.go"]}, batch_state={}))
check("batch_fix_route: owned finding -> fix round queued (re-entry); cross-task -> new fix micro-task in the batch; strike 1",
      u["_next"] == "select_tasks" and u["pending_fixes"][0]["task"] == T2 and u["pending_fixes"][0]["return_to"] == "batch" and len(u["tasks"]) == 4
      and u["tasks"][-1]["batch"] == "B1" and u["tasks"][-1]["type"] == "chore" and u["batch_state"]["B1"]["strikes"] == 1)
u = gates.batch_fix_route(base_state(plan_path=PROMOTED, tasks=copy.deepcopy(ts_b), current_batch="B1", gauntlet_blockers=["x"], finding_routes=[], batch_state={"B1": {"strikes": STRIKE_LIMIT}}))
check("batch_fix_route: B16 3 strikes per batch -> escalation batch_strikeout", u["escalation"]["kind"] == "batch_strikeout")
u = gates.batch_fix_route(base_state(plan_path=PROMOTED, tasks=copy.deepcopy(ts_b), current_batch="B1", gauntlet_blockers=["x"], finding_routes=[], batch_state={}, last_node_output="llm node failed"))
check("batch_fix_route: dead triage -> everything routed to the batch's last task (fail-open)", u["pending_fixes"][0]["task"] == T2)
u = gates.batch_close(base_state(plan_path=PROMOTED, tasks=ts_b, current_batch="B1", current_task=T2, verify_unit="B1", round_log={"B1": [{"round": 1, "head": "h"}]}, last_verdict="PASS", batch_state={}))
check("batch_close: PASS -> batch-closed @ sha logged on members; batch round comment deferred", u["batch_state"]["B1"]["passed"] and "batch-closed @" in read_file(os.path.join(gates.task_by_id(ts_b, T2)["dir"], "log.md")) and u["pending_pr_comments"][0]["body"].startswith("## Batch review round"))

# ---------------------------------------------------------------- escalation ladder
E = escalation("conformance_stuck", "still failing", [option("Stop and report", True, "protocol"), option("One more round")])
u = gates.escalate_gate(base_state(escalation=E, escalation_policy="interactive"))
check("escalate_gate: interactive -> ask_dispatch", u["_next"] == "ask_dispatch")
u = gates.escalate_gate(base_state(escalation=E, escalation_policy="autonomous"))
check("escalate_gate: autonomous -> ladder_judge (rulings file absent noted)", u["_next"] == "ladder_judge" and "no standing rulings" in u["standing_rulings"])
u = gates.escalate_gate(base_state(escalation=escalation("design_ask", "Design question: share the store?", hard_trigger=True), escalation_policy="autonomous"))
check("escalate_gate: hard trigger bypasses the ladder", u["_next"] == "ask_dispatch")
u = gates.escalate_gate(base_state(escalation=E, escalation_policy="autonomous", autonomous_kinds=["conformance_stuck"]))
check("escalate_gate: a kind already resolved autonomously once -> owner", u["_next"] == "ask_dispatch")
u = gates.ladder_route(base_state(escalation=E, ladder_resolution="recommended", ladder_choice="", anomalies=["branch smells"]))
check("ladder_route: recommended -> the single recommended option, digest + ledger + apply; A10 harvested", u["_next"] == "apply_answer" and u["last_answer"] == "Stop and report" and u["autonomous_digest"] and u["anomalies_log"])
u = gates.ladder_route(base_state(escalation=E, ladder_resolution="ruling", ladder_choice="2", ladder_basis="ruling: R1"))
check("ladder_route: ruling by option number", u["last_answer"] == "One more round")
check("ladder_route: consult -> navigator", gates.ladder_route(base_state(escalation=E, ladder_resolution="consult"))["_next"] == "consult_navigator")
check("ladder_route: consult refused when tier3_ok false", gates.ladder_route(base_state(escalation=escalation("design_ask", "q", tier3_ok=False), ladder_resolution="consult"))["_next"] == "ask_dispatch")
check("ladder_route: escalate -> owner", gates.ladder_route(base_state(escalation=E, ladder_resolution="escalate"))["_next"] == "ask_dispatch")
u = gates.consult_route(base_state(escalation=E, consult_result="...\nCONSULT: ANSWER 2"))
check("consult_route: answer -> apply", u["_next"] == "apply_answer" and u["last_answer"] == "One more round")
check("consult_route: defer -> owner", gates.consult_route(base_state(escalation=E, consult_result="CONSULT: DEFER owner's call"))["_next"] == "ask_dispatch")
check("consult_route: fault/timeout -> owner", gates.consult_route(base_state(escalation=E, consult_result="Agent node failed"))["_next"] == "ask_dispatch")
os.environ["ARCHITECT_SLACK_PING_BIN"] = os.path.join(TMP, "nope-slack")
u = gates.ask_dispatch(base_state(escalation=E, top_level=True, owner_away=False))
check("ask_dispatch: terminal prompt; parked recorded (A14)", u["_next"] == "ask_user" and u["parked"] == "conformance_stuck")
check("ask_dispatch: review_incomplete -> the approval node", gates.ask_dispatch(base_state(escalation=escalation("review_incomplete", "q")))["_next"] == "ask_review_incomplete")
u = gates.ask_dispatch(base_state(escalation=E, top_level=True, owner_away=True))
check("ask_dispatch: away + top-level -> ask_owner_poll (fork constant)", (u["_next"] == "ask_owner_poll") == gates.OWNER_REMOTE_INPUT)
FAKE = os.path.join(TMP, "fake_ask_owner")
write(FAKE, "#!/usr/bin/env bash\necho \"${FAKE_ASK_OWNER_OUT:-PENDING}\"\n", exe=True)
os.environ["ARCHITECT_ASK_OWNER_BIN"] = FAKE
os.environ["FAKE_ASK_OWNER_OUT"] = "PENDING"
u = gates.ask_owner_poll(base_state(escalation=E, owner_polls=0))
check("ask_owner_poll: tool called with question+options; PENDING", u["owner_polls"] == 1 and "PENDING" in u["owner_poll_result"] and u["_next"] == "ask_owner_route")
check("ask_owner_route: PENDING within budget -> poll again (B21 side 1)", gates.ask_owner_route(base_state(escalation=E, owner_polls=1, owner_poll_result="PENDING"))["_next"] == "ask_owner_poll")
check("ask_owner_route: PENDING at budget -> terminal (B21 side 2)", gates.ask_owner_route(base_state(escalation=E, owner_polls=2, owner_poll_result="PENDING"))["_next"] == "ask_user")
check("ask_owner_route: ANSWERED -> parse_answer", gates.ask_owner_route(base_state(escalation=E, owner_polls=1, owner_poll_result="ANSWERED: 1"))["_next"] == "parse_answer")
os.environ["ARCHITECT_ASK_OWNER_BIN"] = os.path.join(TMP, "missing-bin")
check("ask_owner_poll: missing tool -> NOT_DELIVERED", "NOT_DELIVERED" in gates.ask_owner_poll(base_state(escalation=E))["owner_poll_result"])
u = gates.parse_answer(base_state(escalation=E, last_answer_text="2"))
check("parse_answer: number -> option; parked cleared; decision ledgered", u["_next"] == "apply_answer" and u["last_answer"] == "One more round" and u["parked"] == "" and any(d.startswith("decision:") for d in u["decisions"]))
u = gates.parse_answer(base_state(escalation=E, last_answer_text="banana", reask_rounds=0))
check("parse_answer: unparseable -> re-ask (B22 side 1)", u["_next"] == "ask_user" and u["reask_rounds"] == 1)
u = gates.parse_answer(base_state(escalation=E, last_answer_text="banana", reask_rounds=STRIKE_LIMIT))
check("parse_answer: B22 exhausted -> free text stands", u["_next"] == "apply_answer" and u["last_answer"] == "banana")
u = gates.parse_answer(base_state(escalation=escalation("design_round", "q"), last_answer_text="1: pg; 2: no"))
check("parse_answer: free-text kinds never re-ask", u["_next"] == "apply_answer")

# ---------------------------------------------------------------- apply_answer (every kind)
def AA(kind, answer, **over):
    esc = over.pop("esc", None) or escalation(kind, "q", [])
    return gates.apply_answer(base_state(escalation=esc, last_answer=answer, **over))


check("apply_answer: mode_invalid -> value applied -> intake/orient", AA("mode_invalid", "pr_strategy = single", design_doc_path="")["_next"] == "intake")
check("apply_answer: mode_invalid abort -> end_aborted", AA("mode_invalid", "Abort the run")["_next"] == "end_aborted")
check("apply_answer: need_design_doc with a real path -> orient_read", AA("need_design_doc", DESIGN)["design_doc_path"] == DESIGN)
check("apply_answer: need_design_doc bad path -> end_blocked", AA("need_design_doc", "/nope.md")["_next"] == "end_blocked")
u = AA("design_round", "1: pg", esc=escalation("design_round", "q", data={"questions": qs}))
check("apply_answer: design_round -> amendments -> amend_plan; open questions cleared", u["_next"] == "amend_plan" and "1: pg" in u["pending_amendments"][0] and u["open_questions"] == [])
u = AA("quality_bar", "rigor=prototype, surfaces=http,other:grpc", esc=escalation("quality_bar", "q", data={"rigor": "production", "surfaces": "x"}))
check("apply_answer: quality_bar -> record_quality_bar with the chosen values", u["_next"] == "record_quality_bar" and u["quality_rigor"] == "prototype" and u["quality_surfaces"] == "http,other:grpc")
u = AA("quality_bar", "rigor=poc, surfaces=(as proposed)", esc=escalation("quality_bar", "q", data={"rigor": "production", "surfaces": "http"}))
check("apply_answer: quality_bar '(as proposed)' keeps the proposal's surfaces", u["quality_surfaces"] == "http" and u["quality_rigor"] == "poc")
u = AA("checklist_accept", "1,3", esc=escalation("checklist_accept", "q", data={"items": ["a", "b", "c"]}))
check("apply_answer: checklist_accept numbers -> amend_plan with the accepted items", u["_next"] == "amend_plan" and "- a" in u["pending_amendments"][0] and "- c" in u["pending_amendments"][0] and "- b" not in u["pending_amendments"][0])
check("apply_answer: checklist_accept none -> checklist_gate", AA("checklist_accept", "none", esc=escalation("checklist_accept", "q", data={"items": ["a"]}))["_next"] == "checklist_gate")
check("apply_answer: amend_cap stop -> end_blocked", AA("amend_cap", "Stop and report")["_next"] == "end_blocked")
u = AA("amend_cap", "Allow one more amendment cycle")
check("apply_answer: amend_cap allow -> one more cycle then gate1", u["_next"] == "gate1_build" and u["amend_cycles"] == AMEND_CYCLES_MAX - 1)
check("apply_answer: gate1_stuck stop -> end_blocked", AA("gate1_stuck", "Stop and report")["_next"] == "end_blocked")
u = AA("gate1_stuck", "infra goes in infra/; DB is pg")
check("apply_answer: gate1_stuck answers -> amend_plan (fingerprints reset)", u["_next"] == "amend_plan" and u["leak_fingerprints"] == [])
u = AA("leak_question", "priority: X first", leak_pending_answers=[{"question": "a", "answer": "b"}])
check("apply_answer: leak_question -> held answers + owner's answers amended", u["_next"] == "amend_plan" and len(u["pending_amendments"]) == 2)
check("apply_answer: gate2_stuck stop -> end_blocked", AA("gate2_stuck", "Stop and report")["_next"] == "end_blocked")
check("apply_answer: gate2_stuck override -> reseal_return (recorded)", AA("gate2_stuck", "Override: promote the plan as reviewed (recorded)")["_next"] == "reseal_return")
u = AA("gate_fault", "Retry the gatekeeper", esc=escalation("gate_fault", "q", data={"gate": "gate1"}), gate_fault_retries=0)
check("apply_answer: gate_fault retry -> gate1_build (B23 side 1)", u["_next"] == "gate1_build" and u["gate_fault_retries"] == 1)
check("apply_answer: gate_fault abort -> end_aborted", AA("gate_fault", "Abort the run", esc=escalation("gate_fault", "q", data={"gate": "gate2"}))["_next"] == "end_aborted")
check("apply_answer: approve_breakdown approve -> promote_tasks", AA("approve_breakdown", "Approve the breakdown")["_next"] == "promote_tasks")
check("apply_answer: approve_breakdown revise -> amend_plan", AA("approve_breakdown", "Revise the breakdown (describe the change)")["_next"] == "amend_plan")
check("apply_answer: approve_breakdown abort -> end_aborted", AA("approve_breakdown", "Abort the run")["_next"] == "end_aborted")
check("apply_answer: promote_failed retry -> promote_verify", AA("promote_failed", "Retry the planning commit")["_next"] == "promote_verify")
check("apply_answer: promote_failed retry the close -> task_done_verify", AA("promote_failed", "Retry the close")["_next"] == "task_done_verify")
check("apply_answer: promote_failed continue -> branch_setup", AA("promote_failed", "Continue with the tasks as promoted")["_next"] == "branch_setup")
check("apply_answer: git_setup_failed retry -> branch_setup", AA("git_setup_failed", "Retry the branch setup")["_next"] == "branch_setup")
u = AA("tasks_stuck", "Retry the failed blocker", tasks=tasks_with(**{T1: "failed"}))
check("apply_answer: tasks_stuck retry -> failed tasks pending again", u["_next"] == "select_tasks" and gates.task_by_id(u["tasks"], T1)["status"] == "pending")
u = AA("tasks_stuck", "Skip the blocked tasks and finish (follow-ups)", tasks=tasks_with(**{T1: "failed", T3: "complete"}))
check("apply_answer: tasks_stuck skip -> follow-ups + phase_f_gate", u["_next"] == "phase_f_gate" and len(u["follow_ups"]) == 2)
check("apply_answer: tasks_stuck stop -> end_blocked", AA("tasks_stuck", "Stop and report", tasks=tasks_with())["_next"] == "end_blocked")
u = AA("task_failed", "Retry with the oracle's direction (one round)", current_task=T1, tasks=tasks_with(), oracle_result="do Y", task_rounds={})
check("apply_answer: task_failed retry -> oracle-directed fix round (B8 side 1)", u["_next"] == "compose_delegation" and u["fix_source"] == "oracle" and u["task_rounds"][T1]["retry_rounds"] == 1)
u = AA("task_failed", "Retry with the oracle's direction (one round)", current_task=T1, tasks=tasks_with(), task_rounds={T1: {"retry_rounds": 1}})
check("apply_answer: task_failed retry at B8 -> abort path (task failed, end_aborted)", u["_next"] == "end_aborted")
u = AA("task_failed", "Re-scope: amend the plan/task", current_task=T1, tasks=tasks_with())
check("apply_answer: task_failed re-scope -> amend_plan (task pending again)", u["_next"] == "amend_plan" and gates.task_by_id(u["tasks"], T1)["status"] == "pending")
u = AA("conformance_stuck", "One more fix round anyway", esc=escalation("conformance_stuck", "q", evidence="- finding A"), current_task=T1, tasks=tasks_with())
check("apply_answer: conformance_stuck one more round -> fix with the findings", u["_next"] == "compose_delegation" and u["fix_items"] == ["finding A"])
check("apply_answer: conformance_stuck accept -> post_round + follow-up", AA("conformance_stuck", "Accept the findings unresolved and close the task (recorded)", current_task=T1, tasks=tasks_with())["_next"] == "post_round")
check("apply_answer: conformance_stuck stop -> end_blocked", AA("conformance_stuck", "Stop and report", current_task=T1, tasks=tasks_with())["_next"] == "end_blocked")
u = AA("plan_defect", "Amend the plan: criterion 2 means X", current_task=T1, tasks=tasks_with())
check("apply_answer: plan_defect amend -> amend_plan, returning to verify after re-seal", u["_next"] == "amend_plan" and u["after_reseal"] == "verify")
check("apply_answer: plan_defect stop -> end_blocked", AA("plan_defect", "Stop and report", current_task=T1, tasks=tasks_with())["_next"] == "end_blocked")
u = AA("probe_gap", "Supply the recipe: make run", current_task=T2, tasks=tasks_with())
check("apply_answer: probe_gap supply -> amend_plan (recipe) -> verify after re-seal", u["_next"] == "amend_plan" and u["after_reseal"] == "verify")
u = AA("probe_gap", "Surface the gap and proceed", current_task=T2, tasks=tasks_with())
check("apply_answer: probe_gap surface -> follow-up + post_round (never PASS/FAIL)", u["_next"] == "post_round" and u["follow_ups"])
check("apply_answer: review_incomplete retry -> verify_gate", AA("review_incomplete", "Retry the review again", current_task=T1, tasks=tasks_with())["_next"] == "verify_gate")
u = AA("review_incomplete", "Accept NEEDS-HUMAN: proceed without the incomplete review (recorded in the report / PR body)", current_task=T1, tasks=TS_W, verify_unit=T1, round_log={T1: [{"round": 1, "incomplete": ["security"]}]})
check("apply_answer: review_incomplete accept -> skipped review recorded (lane names) -> post_round", u["_next"] == "post_round" and "security" in u["skipped_reviews"][0])
check("apply_answer: review_incomplete abort -> end_aborted", AA("review_incomplete", "Abort this task", current_task=T1, tasks=TS_W)["_next"] == "end_aborted")
u = AA("design_ask", "Adopt the pattern (work: one fix round follows)", esc=escalation("design_ask", "Design question: share the store?"), current_task=T1, tasks=tasks_with(), task_rounds={}, design_asks=[])
check("apply_answer: design_ask adopt -> design-decision ledgered + adopting fix round queued (B12 side 1) -> next ask", u["_next"] == "design_ask_gate" and u["design_decisions"][0].startswith("share the store? ->") and u["pending_fixes"][0]["source"] == "design_ask" and any(l.startswith("design-decision:") for l in u["ledger_lines"]))
u = AA("design_ask", "Adopt the pattern (work: one fix round follows)", esc=escalation("design_ask", "Design question: q2"), current_task=T1, tasks=tasks_with(), task_rounds={T1: {"design_ask_rounds": 1}})
check("apply_answer: design_ask adopt at B12 -> recorded only, no second round", "pending_fixes" not in u)
check("apply_answer: design_ask record -> no fix round", "pending_fixes" not in AA("design_ask", "Record the decision without adopting", esc=escalation("design_ask", "Design question: q"), current_task=T1, tasks=tasks_with()))
u = AA("batch_strikeout", "One more batch round anyway", current_batch="B1", batch_state={"B1": {"strikes": 4}})
check("apply_answer: batch_strikeout one more -> batch_fix_route with strikes reset below the cap", u["_next"] == "batch_fix_route" and u["batch_state"]["B1"]["strikes"] == STRIKE_LIMIT - 1)
check("apply_answer: batch_strikeout accept -> batch passed (recorded) -> select_tasks", AA("batch_strikeout", "Accept the batch unresolved (recorded)", current_batch="B1", batch_state={})["batch_state"]["B1"]["passed"])
check("apply_answer: batch_strikeout stop -> end_blocked", AA("batch_strikeout", "Stop and report", current_batch="B1")["_next"] == "end_blocked")
u = AA("wave_escalation", "Retry the task sequentially (one fix round with the evidence)", esc=escalation("wave_escalation", "q", evidence="- e1"), current_task=T3, tasks=TS_W)
check("apply_answer: wave_escalation retry -> sequential fix round in the worktree", u["_next"] == "compose_delegation" and u["fix_items"] == ["e1"])
check("apply_answer: wave_escalation accept -> integration_test (single)", AA("wave_escalation", "Accept as-is and close the task (recorded)", current_task=T3, tasks=TS_W)["_next"] == "integration_test")
check("apply_answer: wave_escalation abort -> wave_cleanup (task failed, branch kept)", AA("wave_escalation", "Abort this task (worktree removed, branch kept)", current_task=T3, tasks=TS_W)["_next"] == "wave_cleanup")
u = AA("integration_conflict", "One more rebase cycle", current_task=T3, tasks=TS_W, integration_task=T3, task_rounds={T3: {"conflict_cycles": 3}})
check("apply_answer: integration_conflict one more -> integration_test (cycle decremented)", u["_next"] == "integration_test" and u["task_rounds"][T3]["conflict_cycles"] == 2)
check("apply_answer: integration_red one more at Phase F -> integration_check", AA("integration_red", "One more fix round anyway", current_task=T1, tasks=TS_W, task_rounds={T1: {"integration_strikes": 4}})["_next"] == "integration_check")
check("apply_answer: integration_* stop -> end_blocked", AA("integration_red", "Stop and report", current_task=T1, tasks=TS_W)["_next"] == "end_blocked")
check("apply_answer: approve_next continue -> select_tasks", AA("approve_next", "Continue")["_next"] == "select_tasks")
u = AA("approve_next", "Stop here (park the run)")
check("apply_answer: approve_next park -> end_blocked with parked marker (resumable)", u["_next"] == "end_blocked" and u["parked"] == "approve_next")
check("apply_answer: approve_next abort -> end_aborted", AA("approve_next", "Abort the run")["_next"] == "end_aborted")
check("apply_answer: hygiene_base_missing stop -> end_blocked", AA("hygiene_base_missing", "Stop and report")["_next"] == "end_blocked")
u = AA("hygiene_base_missing", "Skip the hygiene pass (recorded as a follow-up)")
check("apply_answer: hygiene_base_missing skip -> follow-up + next unit (hygiene_create)", u["_next"] == "hygiene_create" and u["follow_ups"])
check("apply_answer: ci_unsure re-run -> ci_poll (watch reset)", AA("ci_unsure", "Re-run the failing checks once")["_next"] == "ci_poll")
u = AA("ci_exhausted", "Resume with one more coder fix round", tasks=TS_W, ci_owning_task=T1)
check("apply_answer: ci_* fix -> fix_round to the owning task", u["_next"] == "fix_round" and u["fix_request"]["task"] == T1)
u = AA("ci_infra", "Leave the PR in draft and report", pr_number="5")
check("apply_answer: ci_* leave -> follow-up + finish_plan", u["_next"] == "finish_plan" and "draft" in u["follow_ups"][0])
check("apply_answer: scope_surprise -> end_blocked", AA("scope_surprise", "x")["_next"] == "end_blocked")
check("apply_answer: unknown kind -> end_failure (never silent)", AA("bogus", "x")["_next"] == "end_failure")

# ---------------------------------------------------------------- Phase F
ts_f = tasks_with(**{T1: "complete", T2: "complete", T3: "complete"})
for t, b in zip(ts_f, ("B1", "B1", "B2")):
    t["batch"] = b
u = gates.phase_f_gate(base_state(plan_path=PROMOTED, tasks=ts_f, task_granularity="micro", batch_state={"B1": {"passed": True, "base_sha": "a"}}))
check("phase_f_gate: micro - a batch never PASSed (B2) -> its gauntlet first (anti-pattern guard)", u["_next"] == "batch_gauntlet_build" and u["current_batch"] == "B2")
u = gates.phase_f_gate(base_state(plan_path=PROMOTED, tasks=ts_f, integration_checked=False))
check("phase_f_gate: single -> integration_check first; hygiene unit = run", u["_next"] == "integration_check" and u["hygiene_units"][0]["id"] == "run" and len(u["hygiene_units"][0]["members"]) == 3)
u = gates.phase_f_gate(base_state(plan_path=PROMOTED, tasks=ts_chain, pr_strategy="multi"))
check("phase_f_gate: per-task-stack multi -> integrate_findings; hygiene units = stacked chains with >=2 PRs (T1<-T2), standalone T3 none",
      MULTI_IS_GROUPS or (u["_next"] == "integrate_findings" and len(u["hygiene_units"]) == 1 and sorted(u["hygiene_units"][0]["members"]) == sorted([T1, T2])))
u = gates.integration_check(base_state(plan_path=PROMOTED, tasks=ts_f, verification_commands='["true"]'))
check("integration_check: green -> integrate_findings", u["_next"] == "integrate_findings" and u["integration_checked"])
u = gates.integration_check(base_state(plan_path=PROMOTED, tasks=ts_f, verification_commands='["false"]', final_integration_strikes=0))
check("integration_check: RED -> fix round (B14 side 1, return_to integration_check)", u["_next"] == "fix_round" and u["fix_request"]["return_to"] == "integration_check")
u = gates.integration_check(base_state(plan_path=PROMOTED, tasks=ts_f, verification_commands='["false"]', final_integration_strikes=STRIKE_LIMIT))
check("integration_check: B14 exhausted -> integration_red", u["escalation"]["kind"] == "integration_red")
u = gates.integrate_route(base_state(integration_amendments=["TASK-3 criterion stale"], integration_follow_ups=["rotate key"], integration_amended=False, anomalies=["x"]))
check("integrate_route: amendments -> amend_plan (once per Phase F), follow-ups ledgered, A10 consumed", u["_next"] == "amend_plan" and u["integration_amended"] and u["follow_ups"] == ["(integration) rotate key"] and u["after_reseal"] == "phase_f" and u["anomalies_log"])
check("integrate_route: already amended -> hygiene_create", gates.integrate_route(base_state(integration_amendments=["x"], integration_amended=True))["_next"] == "hygiene_create")
check("integrate_route: nothing -> hygiene_create", gates.integrate_route(base_state())["_next"] == "hygiene_create")
u = gates.hygiene_create(base_state(plan_path=PROMOTED, tasks=copy.deepcopy(ts_f), hygiene_units=[{"id": "run", "members": [T1, T2, T3]}], unit_base_sha=CODE_BASE, run_branch="feat/PLAN-widgets"))
check("hygiene_create: single - chore task created + claimed, base literal in the brief + criteria, lint cmd, starting ledger via collect_ledger_lines, -> compose_delegation",
      u["_next"] == "compose_delegation" and u["delegation_kind"] == "hygiene" and u["hygiene_base"] == CODE_BASE and f"{CODE_BASE}..HEAD" in u["hygiene_brief"]
      and "debris.py" in u["hygiene_lint_cmd"] and "These two are the ONLY criteria" in u["hygiene_criteria"] and "Starting ledger" in u["hygiene_brief"]
      and u["tasks"][-1]["type"] == "hygiene" and u["tasks"][-1]["blocked_by"] == [T1, T2, T3] and u["hygiene_done"] == ["run"] and "NO separate hygiene PR" in u["hygiene_brief"])
check("hygiene_create: no units -> pr_summary (single)", gates.hygiene_create(base_state(plan_path=PROMOTED, tasks=ts_f, hygiene_units=[]))["_next"] == "pr_summary")
check("hygiene_create: no units -> stack_gate_final (multi)", MULTI_IS_GROUPS or gates.hygiene_create(base_state(plan_path=PROMOTED, tasks=ts_f, hygiene_units=[], pr_strategy="multi"))["_next"] == "stack_gate_final")
u = gates.hygiene_create(base_state(plan_path=PROMOTED, tasks=copy.deepcopy(ts_chain), pr_strategy="multi", hygiene_units=[{"id": "chain-" + T1, "members": [T1, T2], "top_branch": "feat/nonexistent"}]))
check("hygiene_create: chain base unresolvable -> escalation hygiene_base_missing (STOP, never a vacuous clean)", MULTI_IS_GROUPS or u["escalation"]["kind"] == "hygiene_base_missing")
u = gates.pr_body(base_state(plan_path=PROMOTED, tasks=ts_f, pr_summary_text="Widgets were missing; we added them.", body_fix_rounds=0))
body = read_file(u["pr_body_path"])
check("pr_body: assemble_pr_body (core tool) -> body with summary + task checklist + hygiene section; NOTE lines -> one fix round (B17 side 1)",
      "Widgets were missing" in body and "## Tasks" in body and "## Hygiene pass" in body and u["_next"] == "pr_body_fix" and u["body_fix_rounds"] == 1)
u = gates.pr_body(base_state(plan_path=PROMOTED, tasks=ts_f, pr_summary_text="s", body_fix_rounds=RERUN_ONCE))
check("pr_body: B17 used -> open_pr (NOTEs stay visible)", u["_next"] == "open_pr")
u = gates.pr_body(base_state(plan_path=PROMOTED, tasks=ts_f, pr_summary_text="", body_fix_rounds=RERUN_ONCE, last_node_output="llm node failed"))
check("pr_body: dead summary -> Problem/Approach pasted verbatim (declared)", "Widgets are missing" in read_file(u["pr_body_path"]) and any("verbatim" in d for d in u["decisions"]))
os.environ["ARCHITECT_GH_CREATE_CMD"] = "echo https://github.com/o/r/pull/99"
u = gates.open_pr(base_state(plan_path=PROMOTED, tasks=ts_f, run_branch="feat/PLAN-widgets", pr_body_path=os.path.join(rd, "pr-body.md"), pr_body_fix_report="fixed\nANOMALY: body had stale sha"))
check("open_pr: draft PR opened -> number recorded, CI watch armed, A10 line harvested", u["_next"] == "pr_round_comments" and u["pr_number"] == "99" and u["ci_rounds"] == 0 and any("stale sha" in a for a in u["anomalies_log"]))
os.environ["ARCHITECT_GH_CREATE_CMD"] = "false"
u = gates.open_pr(base_state(plan_path=PROMOTED, tasks=ts_f, run_branch="feat/PLAN-widgets", pr_body_path="x"))
check("open_pr: gh failure -> follow-up, finish_plan (never silent)", u["_next"] == "finish_plan" and u["follow_ups"])
check("open_pr: PR already exists -> pr_round_comments", gates.open_pr(base_state(pr_number="99"))["_next"] == "pr_round_comments")
u = gates.pr_round_comments(base_state(pr_number="99", pending_pr_comments=[{"unit": T1, "body": "## Review round 1 @ h"}]))
check("pr_round_comments: deferred comments posted -> ci_poll", u["_next"] == "ci_poll" and u["pending_pr_comments"] == [] and any("1/1" in d for d in u["decisions"]))
check("pr_round_comments: no PR -> finish_plan", gates.pr_round_comments(base_state(pr_number=""))["_next"] == "finish_plan")

# ---------------------------------------------------------------- CI watch (B18, B19, B20) with per-check-name per-PR keying
def checks(*rows):
    os.environ["ARCHITECT_GH_CHECKS_CMD"] = "echo '" + json.dumps([{"name": n, "bucket": b, "link": f"https://github.com/o/r/actions/runs/{i + 1}"} for i, (n, b) in enumerate(rows)]) + "'"


checks(("build", "pass"), ("test", "pending"))
u = gates.ci_poll(base_state(pr_number="99", ci_polls=0, ci_rounds=0, ci_started_at=0))
check("ci_poll: pending -> poll again", u["_next"] == "ci_poll" and u["ci_status"] == "pending" and u["ci_rounds"] == 1)
checks(("build", "pass"), ("test", "pass"))
check("ci_poll: green -> ci_route", gates.ci_poll(base_state(pr_number="99", ci_polls=1))["ci_status"] == "green")
checks(("build", "pass"), ("test", "fail"))
u = gates.ci_poll(base_state(pr_number="99", ci_polls=1))
check("ci_poll: red -> ci_route with failing checks", u["ci_status"] == "red" and u["ci_failing"][0][0] == "test")
os.environ["ARCHITECT_GH_CHECKS_CMD"] = "false"
check("ci_poll: poll error below the cap -> poll again (B20 side 1)", gates.ci_poll(base_state(pr_number="99", ci_polls=1, ci_poll_fails=0))["_next"] == "ci_poll")
check("ci_poll: consecutive poll errors at the cap -> ci_route poll_error (B20 side 2)", gates.ci_poll(base_state(pr_number="99", ci_polls=1, ci_poll_fails=CI_POLL_FAILS - 1))["ci_status"] == "poll_error")
check("ci_poll: rounds cap (A6) -> timeout", gates.ci_poll(base_state(pr_number="99", ci_polls=1, ci_rounds=CI_MAX_ROUNDS))["ci_status"] == "timeout")
check("ci_poll: wall clock -> timeout", gates.ci_poll(base_state(pr_number="99", ci_polls=1, ci_started_at=1))["ci_status"] == "timeout")
check("ci_route: green -> flip_ready", gates.ci_route(base_state(ci_status="green"))["_next"] == "flip_ready")
check("ci_route: red -> ci_evidence", gates.ci_route(base_state(ci_status="red"))["_next"] == "ci_evidence")
check("ci_route: timeout -> escalation ci_exhausted with evidence", gates.ci_route(base_state(ci_status="timeout"))["escalation"]["kind"] == "ci_exhausted")
os.environ["ARCHITECT_GH_LOG_CMD"] = "printf 'ok\\n--- FAIL: TestWidget (0.1s)\\n    widget_test.go:12: expected 2\\nThe runner has lost communication with the server\\n'"
u = gates.ci_evidence(base_state(ci_failing=[("test", "https://github.com/o/r/actions/runs/7")]))
check("ci_evidence: evidence pack + failure signatures + infra pre-classification hint", "TestWidget" in u["ci_evidence"] and any("FAIL" in s for s in u["ci_signatures"]) and u["ci_infra_hint"] == ["test"])
reruns0 = {}
u = gates.ci_apply(base_state(pr_number="99", ci_verdict="flake", failing_checks=["test"], ci_failing=[("test", "https://github.com/o/r/actions/runs/7")], ci_reruns=reruns0))
check("ci_apply: flake -> re-run ledgered (flake-rerun:) keyed per PR per check-name, watch reset (B19 side 1)", u["_next"] == "ci_poll" and u["ci_reruns"]["99"]["test"] == 1 and any(d.startswith("flake-rerun:") for d in u["decisions"]))
R1 = u["ci_reruns"]
u = gates.ci_apply(base_state(pr_number="99", ci_verdict="real", failing_checks=["test"], ci_failing=[("test", "l")], ci_reruns=R1, ci_fix_strikes={}, tasks=ts_f, ci_diagnosis="widget_test.go broke; src/a.go"))
check("ci_apply: real -> fix round to the owning task (declared files match) with strikes keyed per PR per check", u["_next"] == "fix_round" and u["fix_request"]["task"] == T1 and u["ci_fix_strikes"]["99"]["test"] == 1 and u["fix_request"]["return_to"] == "ci")
check("ci_apply: a fix push did NOT re-arm the flake budget (ci_reruns untouched by the real path)", "ci_reruns" not in u and R1["99"]["test"] == 1)
u = gates.ci_apply(base_state(pr_number="99", ci_verdict="flake", failing_checks=["test"], ci_failing=[("test", "l")], ci_reruns={"99": {"test": CI_FLAKE_RERUNS}}, ci_fix_strikes={}, tasks=ts_f))
check("ci_apply: flake budget exhausted -> treated as REAL (third red is real by definition) (B19 side 2)", u["_next"] == "fix_round" and any("third red" in d for d in u["decisions"]))
u = gates.ci_apply(base_state(pr_number="99", ci_verdict="flake", failing_checks=["lint"], ci_failing=[("lint", "l")], ci_reruns={"99": {"test": CI_FLAKE_RERUNS}, "98": {"lint": CI_FLAKE_RERUNS}}, tasks=ts_f))
check("ci_apply: budgets are per check-name per PR (lint on #99 fresh although test is exhausted and #98 lint is)", u["_next"] == "ci_poll" and u["ci_reruns"]["99"]["lint"] == 1)
u = gates.ci_apply(base_state(pr_number="99", ci_verdict="real", failing_checks=["test"], ci_failing=[("test", "l")], ci_fix_strikes={"99": {"test": STRIKE_LIMIT}}, tasks=ts_f))
check("ci_apply: B18 fix strikes exhausted per check per PR -> ci_exhausted", u["escalation"]["kind"] == "ci_exhausted")
check("ci_apply: infra -> escalation ci_infra (wait-or-escalate)", gates.ci_apply(base_state(pr_number="99", ci_verdict="infra", failing_checks=["test"]))["escalation"]["kind"] == "ci_infra")
check("ci_apply: unsure defaults UP (never a re-run)", gates.ci_apply(base_state(pr_number="99", ci_verdict="unsure", failing_checks=["test"]))["escalation"]["kind"] == "ci_unsure")
check("ci_apply: dead diagnose = unsure", gates.ci_apply(base_state(pr_number="99", ci_verdict="", last_node_output="llm node failed", failing_checks=["test"]))["escalation"]["kind"] == "ci_unsure")
os.environ["ARCHITECT_GH_READY_CMD"] = "true"
u = gates.flip_ready(base_state(pr_number="99", tasks=ts_f))
check("flip_ready: single green -> gh pr ready -> finish_plan", u["flipped_ready"] and u["_next"] == "finish_plan")
if MULTI_IS_GROUPS:
    u = gates.flip_ready(base_state(pr_number="99", tasks=ts_f, pr_strategy="multi", closeout_scope="group", closeout_unit="g1"))
    check("flip_ready: group close-out -> next group", u["_next"] == "select_tasks" and u["closeout_done_for"] == "g1")
u = gates.finish_plan(base_state(plan_path=PROMOTED, tasks=ts_f, pr_url="https://github.com/o/r/pull/99", follow_ups=["Provision WIDGET_SECRET"]))
check("finish_plan: validate run; PLAN status Implemented + PR + follow-ups; final planning commit", u["_next"] == "final_report" and "implemented" in read_file(PROMOTED).lower() and "pull/99" in read_file(PROMOTED)
      and "**Follow-ups:**" in read_file(PROMOTED) and "implemented" in STORE.last_commit())
u = gates.final_report(base_state(plan_path=PROMOTED, tasks=ts_f, pr_url="https://github.com/o/r/pull/99", flipped_ready=True, ci_status="green", escalation_policy="autonomous",
                                  autonomous_digest=["x: y (basis: recommended)"], skipped_reviews=["T1: security"], anomalies_log=["a: b"], follow_ups=["f"], quality_rigor="prototype",
                                  mode_line="m", task_granularity="micro", batch_state={"B1": {"passed": True, "closed_at": "h", "strikes": 1}}))
r = u["report"]
check("final_report: grep-complete rollups (collect_ledger_lines), batch grouping, digest, skipped reviews, anomalies, Quality bar line, run id; report.md written",
      all(s in r for s in ("## Review decisions", "## Hygiene pass", "## Design decisions", "## Batch gauntlets", "## Decisions taken autonomously", "## Skipped reviews", "## Anomalies",
                           "Quality bar: prototype", "Architect run id:", "record-ruling")) and os.path.isfile(os.path.join(rd, "report.md")) and u["_next"] == "whetstone_gate")
check("whetstone_gate: feedback -> run_whetstone", gates.whetstone_gate(base_state(external_feedback=["human said x"]))["_next"] == "run_whetstone")
check("whetstone_gate: none -> summarize_run (noted)", gates.whetstone_gate(base_state())["_next"] == "summarize_run")
check("group_closeout -> phase_f_gate", gates.group_closeout(base_state(closeout_unit="g1"))["_next"] == "phase_f_gate")

# ---------------------------------------------------------------- rollup scope: --plan always, --tasks for a PR group only
# A recording tools.sh (ARCHITECT_CORE_TOOLS) logs the --tasks / --plan each core call receives.
CORE_CALLS = os.path.join(TMP, "core-calls.txt")
RECORDING_TOOLS = os.path.join(TMP, "recording-tools", "tools.sh")
write(RECORDING_TOOLS, "record() { printf '%s tasks=%s plan=%s\\n' \"$1\" \"${argc_tasks-unset}\" \"${argc_plan-unset}\" >> \"" + CORE_CALLS + "\"; }\n"
                       "assemble_pr_body() { record assemble_pr_body; echo body > \"$argc_out\"; }\n"
                       "collect_ledger_lines() { record collect_ledger_lines; echo '(none)'; }\n")


def core_calls(run, name, tools=RECORDING_TOOLS):
    write(CORE_CALLS, "")
    prior = os.environ.get("ARCHITECT_CORE_TOOLS")
    os.environ["ARCHITECT_CORE_TOOLS"] = tools
    try:
        run()
    finally:
        if prior is None:
            os.environ.pop("ARCHITECT_CORE_TOOLS")
        else:
            os.environ["ARCHITECT_CORE_TOOLS"] = prior
    return [l.split(" ", 1)[1] for l in read_file(CORE_CALLS).splitlines() if l.startswith(name + " ")]


ts_g = copy.deepcopy(ts_f)
for t, g in zip(ts_g, ("g1", "g1", "g2")):
    t["unit"] = g
GROUP_TASKS = f"{T1},{T2}" if MULTI_IS_GROUPS else "unset"
calls = core_calls(lambda: gates.pr_body(base_state(plan_path=PROMOTED, tasks=ts_g, pr_strategy="multi", closeout_unit="g1", pr_summary_text="s", body_fix_rounds=RERUN_ONCE)), "assemble_pr_body")
check("pr_body: PR-group mode -> assemble_pr_body --tasks <the closing group's task ids, comma-separated>", calls == [f"tasks={GROUP_TASKS} plan={PROMOTED}"], str(calls))
calls = core_calls(lambda: gates.pr_body(base_state(plan_path=PROMOTED, tasks=ts_g, closeout_unit="g1", pr_summary_text="s", body_fix_rounds=RERUN_ONCE)), "assemble_pr_body")
check("pr_body: single PR -> assemble_pr_body without --tasks", calls == [f"tasks=unset plan={PROMOTED}"], str(calls))
ts_skip = copy.deepcopy(ts_g)
for t in ts_skip:
    t["status"] = "skipped" if t["id"] == T2 else "complete"
calls = core_calls(lambda: gates.pr_body(base_state(plan_path=PROMOTED, tasks=ts_skip, pr_strategy="multi", closeout_unit="g1", pr_summary_text="s", body_fix_rounds=RERUN_ONCE)),
                   "assemble_pr_body")
check("pr_body: PR-group mode with a skipped member -> assemble_pr_body --tasks lists only the group's complete tasks",
      calls == [f"tasks={T1 if MULTI_IS_GROUPS else 'unset'} plan={PROMOTED}"], str(calls))
FAILING_TOOLS = os.path.join(TMP, "failing-tools", "tools.sh")
write(FAILING_TOOLS, "record() { printf '%s tasks=%s plan=%s\\n' \"$1\" \"${argc_tasks-unset}\" \"${argc_plan-unset}\" >> \"" + CORE_CALLS + "\"; }\n"
                     "assemble_pr_body() { record assemble_pr_body; echo 'assemble failed' >&2; return 1; }\n")
ts_fail = copy.deepcopy(ts_g)
for t in ts_fail:
    t["status"] = "complete"
calls = core_calls(lambda: gates.pr_body(base_state(plan_path=PROMOTED, tasks=ts_fail, pr_strategy="multi", closeout_unit="g1", pr_summary_text="s",
                                                    body_fix_rounds=RERUN_ONCE)), "assemble_pr_body", FAILING_TOOLS)
_listed = [l.split(" ")[2] for l in read_file(os.path.join(base_state()["run_dir"], "pr-body.md")).splitlines() if l.startswith("- [x] ")]
check("pr_body: assemble_pr_body fails in PR-group mode -> the fallback task list holds only the closing group's tasks",
      calls == [f"tasks={GROUP_TASKS} plan={PROMOTED}"] and _listed == ([T1, T2] if MULTI_IS_GROUPS else [t["id"] for t in ts_fail]), f"{calls} {_listed}")
calls = core_calls(lambda: gates.hygiene_create(base_state(plan_path=PROMOTED, tasks=copy.deepcopy(ts_g), pr_strategy="multi", closeout_unit="g1", unit_base_sha=CODE_BASE,
                                                           run_branch="feat/PLAN-widgets", hygiene_units=[{"id": "g1", "members": [T1, T2]}])), "collect_ledger_lines")
check("hygiene_create: PR-group mode -> collect_ledger_lines --plan <plan> --tasks <the group's members>", calls == [f"tasks={GROUP_TASKS} plan={PROMOTED}"], str(calls))
calls = core_calls(lambda: gates.final_report(base_state(plan_path=PROMOTED, tasks=ts_g, pr_strategy="multi", closeout_unit="g2")), "collect_ledger_lines")
check("final_report: every collect_ledger_lines call gets a non-empty --plan and no --tasks (the run report spans all groups)",
      len(calls) == 4 and all(c == f"tasks=unset plan={PROMOTED}" for c in calls), str(calls))

# The REAL tools.sh collect_ledger_lines over a scratch plan and task log: the lines arrive, not "(collect_ledger_lines unavailable: ...)".
LEDGER = os.path.join(TMP, "ledger")
LEDGER_PLAN = os.path.join(LEDGER, "PLAN-ledger.md")
LEDGER_TASKS = os.path.join(LEDGER, "tasks")
write(LEDGER_PLAN, "---\ntitle: Ledger\nrigor: production\n---\n\n# Ledger\n")
ledger_task = os.path.join(LEDGER_TASKS, "completed" if gates.TASKSTORE == "dirstate" else "", "TASK-901-done")
write(os.path.join(ledger_task, "index.md"), "---\ntitle: Done\nstatus: complete\nplan: PLAN-ledger\n---\n")
write(os.path.join(ledger_task, "log.md"), "# TASK-901 log\n\n## 2026-01-01 - completed\n\n"
                                           "debris-exempt: scripts/gen.py keeps its marker (generated upstream)\nFollow-ups:\n- rotate the widget key before merge\n")
u = gates.hygiene_create(base_state(plan_path=LEDGER_PLAN, tasks_root=LEDGER_TASKS, tasks=[], unit_base_sha=CODE_BASE, run_branch="feat/PLAN-widgets",
                                    hygiene_units=[{"id": "run", "members": ["TASK-901"]}]))
check("hygiene_create: the real collect_ledger_lines collects the plan's debris-exempt line into the starting ledger",
      "TASK-901-done (log.md:5): debris-exempt: scripts/gen.py keeps its marker" in u.get("hygiene_brief", "") and "unavailable" not in u.get("hygiene_brief", ""), u.get("hygiene_brief", "")[-400:])
rows = gates._collect(base_state(plan_path=LEDGER_PLAN, tasks_root=LEDGER_TASKS), "Follow-ups:")
check("final_report rollup: the real collect_ledger_lines collects a Follow-ups: line with its bullet",
      rows == ["TASK-901-done (log.md:6): Follow-ups:", "  - rotate the widget key before merge"], str(rows))

# ---------------------------------------------------------------- entry scripts through GRAPH_STATE / GRAPH_STATE_FILE
env = {**os.environ, "GRAPH_STATE": json.dumps(base_state(escalation=E, escalation_policy="interactive"))}
p = subprocess.run([sys.executable, os.path.join(HERE, "escalate_gate.py")], capture_output=True, text=True, env=env)
check("entry script: GRAPH_STATE env -> JSON with _next", p.returncode == 0 and json.loads(p.stdout.strip().splitlines()[-1])["_next"] == "ask_dispatch", p.stderr[-300:])
big_state = base_state(escalation=E, escalation_policy="interactive", pad="x" * 40000)
sf = os.path.join(TMP, "state.json")
write(sf, json.dumps(big_state))
env = {**os.environ, "GRAPH_STATE_FILE": sf}
env.pop("GRAPH_STATE", None)
p = subprocess.run([sys.executable, os.path.join(HERE, "escalate_gate.py")], capture_output=True, text=True, env=env)
check("entry script: GRAPH_STATE_FILE path (>32 KiB) -> JSON with _next", p.returncode == 0 and json.loads(p.stdout.strip().splitlines()[-1])["_next"] == "ask_dispatch", p.stderr[-300:])
env = {**os.environ, "GRAPH_STATE": json.dumps({"initial_prompt": f"go {DESIGN}", **STORE.variables(DESIGN, TMP)})}
p = subprocess.run([sys.executable, os.path.join(HERE, "intake.py")], capture_output=True, text=True, env=env)
check("entry script: intake through GRAPH_STATE", p.returncode == 0 and json.loads(p.stdout.strip().splitlines()[-1])["_next"] == "orient_read", p.stderr[-300:])

# ---------------------------------------------------------------- summary
fails = [n for n, ok, _ in RESULTS if not ok]
print(f"\n{len(RESULTS) - len(fails)}/{len(RESULTS)} checks passed")
shutil.rmtree(TMP, ignore_errors=True)
sys.exit(1 if fails else 0)
