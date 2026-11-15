#!/usr/bin/env python3
"""Black-box usage probe for TASK-556 (sisyphus-next CI status, report truth, fault reasons).

Runs the graph's real entry scripts (assets/agents/sisyphus-next/scripts/<node>.py) the way the
engine does: state in GRAPH_STATE, one JSON update object on stdout. gh is replaced only through
the operator seams (SISYPHUS_GH_*_CMD). Expectations were written from the TASK-556 acceptance
criteria before reading gates.py. Run: python3 tests/probes/sisyphus_next_task556_usage.py
"""
import json
import os
import re
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
S = os.path.join(HERE, "..", "..", "assets", "agents", "sisyphus-next", "scripts")
BASE = tempfile.mkdtemp(prefix="probe-sis556-")
FAILS = []
PR = "https://github.com/o/r/pull/9"


def run(node, state, env=None):
    e = {k: v for k, v in os.environ.items()
         if not k.startswith(("LLM_AGENT_VAR_", "SISYPHUS_", "COYOTE_", "GIT_"))}
    e.update({"COYOTE_CACHE_DIR": os.path.join(BASE, "cache"), "SISYPHUS_CI_SLEEP": "0",
              "SISYPHUS_REVIEW_SLEEP": "0", "GRAPH_STATE": json.dumps(state)})
    e.update(env or {})
    p = subprocess.run([sys.executable, os.path.join(S, node + ".py")], env=e,
                       capture_output=True, text=True, timeout=120)
    assert p.returncode == 0, f"{node} rc {p.returncode}: {p.stderr[-800:]}"
    return json.loads(p.stdout.strip().splitlines()[-1])


def rd(name):
    d = os.path.join(BASE, re.sub(r"\W+", "-", name))
    os.makedirs(d, exist_ok=True)
    return d


def merge(st, u):
    return {**st, **{k: v for k, v in u.items() if not k.startswith("_")}}


def check(name, ok, detail=""):
    print(("ok   " if ok else "FAIL ") + name + ("" if ok else f"  -- {detail}"))
    if not ok:
        FAILS.append(name)


def checks_env(checks):
    return {"SISYPHUS_GH_CHECKS_CMD": "echo '%s'" % json.dumps(checks)}


def report_lines(st):
    r = run("final_report", st)["report"]
    return [l for l in r.splitlines() if l.startswith("- CI:")], r


READY_OK = {"SISYPHUS_GH_PR_READY_CMD": "exit 0", "SISYPHUS_GH_PR_VIEW_CMD": "echo '{\"isDraft\":false}'"}

# ---- AC1: only success is green; everything else finished is inconclusive; empty -> no_checks
now = int(time.time())
for label, checks, want in [
    ("all pass", [{"name": "a", "bucket": "pass"}], "green"),
    ("SUCCESS state", [{"name": "a", "state": "SUCCESS"}], "green"),
    ("pass + cancel", [{"name": "a", "bucket": "pass"}, {"name": "b", "bucket": "cancel"}], "inconclusive"),
    ("pass + skipping", [{"name": "a", "bucket": "pass"}, {"name": "b", "bucket": "skipping"}], "inconclusive"),
    ("neutral + no bucket", [{"name": "a", "state": "NEUTRAL"}, {"name": "b"}], "inconclusive"),
    ("fail + cancel", [{"name": "a", "bucket": "fail"}, {"name": "b", "bucket": "cancel"}], "red"),
]:
    u = run("ci_poll", {"run_dir": rd("p" + label), "pr_ref": PR, "ci_started_at": now}, checks_env(checks))
    check(f"AC1 ci_poll {label} -> {want}", u.get("ci_status") == want, u)
u = run("ci_poll", {"run_dir": rd("empty-early"), "pr_ref": PR, "ci_started_at": now - 10}, checks_env([]))
check("AC1 ci_poll empty list early in the watch keeps polling", u.get("ci_status") == "pending" and u["_next"] == "ci_poll", u)
u = run("ci_poll", {"run_dir": rd("empty-late"), "pr_ref": PR, "ci_started_at": now - 400},
        {"SISYPHUS_GH_CHECKS_CMD": "echo 'no checks reported on the x branch' >&2; exit 1"})
check("AC1 ci_poll 'no checks reported' (rc 1) late in the watch -> no_checks", u.get("ci_status") == "no_checks", u)

st = {"run_dir": rd("route-inc"), "pr_ref": PR, "ci_status": "inconclusive",
      "ci_checks": [["build", "pass", ""], ["lint", "cancel", ""], ["docs", "skipping", ""], ["x", "unknown", ""]]}
e = run("ci_route", st).get("escalation") or {}
check("AC1 inconclusive escalates with every check and its bucket as evidence",
      e.get("kind") == "ci_inconclusive" and all(f"{n}: {b}" in e.get("evidence", "") for n, b, _ in st["ci_checks"]), e)
st = {"run_dir": rd("route-none"), "pr_ref": PR, "ci_status": "no_checks", "ci_checks": []}
r = run("ci_route", st)
e = r.get("escalation") or {}
check("AC1 no_checks escalates", e.get("kind") == "ci_no_checks" and e.get("evidence"), e)
a = run("apply_answer", {**merge(st, r), "last_answer": e["options"][0]["text"]})
check("AC1 a repo with no CI is flipped (recommended answer -> flip_ready git task)",
      a.get("_next") == "git_ops" and (a.get("git_task") or {}).get("kind") == "flip_ready", a)

# ---- AC2: flip reported only after rc 0 AND re-read isDraft false
for label, env, want in [
    ("rc0 + isDraft false", READY_OK, True),
    ("rc0 + isDraft true", {**READY_OK, "SISYPHUS_GH_PR_VIEW_CMD": "echo '{\"isDraft\":true}'"}, False),
    ("rc1 + isDraft false", {**READY_OK, "SISYPHUS_GH_PR_READY_CMD": "exit 1"}, False),
    ("rc0 + view fails", {**READY_OK, "SISYPHUS_GH_PR_VIEW_CMD": "exit 1"}, False),
    ("rc0 + view garbage", {**READY_OK, "SISYPHUS_GH_PR_VIEW_CMD": "echo nope"}, False),
]:
    st = {"run_dir": rd("flip" + label), "pr_ref": PR, "ci_status": "green", "git_report": "flipped",
          "git_task": {"kind": "flip_ready", "return_to": "final_report", "flip_ready": True}, "flip_ready": True}
    g = run("ci_gate", st, env)
    lines, _ = report_lines(merge(st, g))
    said = any("(flipped to ready)" in l for l in lines)
    check(f"AC2 flip {label}: report says flipped={want}", said == want and g.get("pr_ready_confirmed") is want, (lines, g))
marker = os.path.join(BASE, "ready-ran")
st = {"run_dir": rd("flip-noref"), "ci_status": "green", "git_report": "flipped #12",
      "git_task": {"kind": "flip_ready", "return_to": "final_report", "flip_ready": True}, "flip_ready": True}
g = run("ci_gate", st, {"SISYPHUS_GH_PR_READY_CMD": f"touch {marker}", "SISYPHUS_GH_PR_VIEW_CMD": "echo '{\"isDraft\":false}'"})
check("AC2 no saved pr_ref: gh pr ready not run and flip not confirmed",
      not os.path.exists(marker) and g.get("pr_ready_confirmed") is False, g)
for label, task, report, want in [
    ("no PR link", {"kind": "publish", "open_pr": True, "return_to": "ci_gate"}, "gh pr create failed", "no PR link"),
    ("no PR opened", {"kind": "publish", "open_pr": False, "return_to": "ci_gate"}, "committed", "opened no PR"),
]:
    st = {"run_dir": rd("skip" + label), "git_task": task, "git_report": report}
    lines, _ = report_lines(merge(st, run("ci_gate", st)))
    check(f"AC2 skipped CI watch named in the report ({label})", any("not watched" in l and want in l for l in lines), lines)
st = {"run_dir": rd("skip-delegated"), "delegated": True, "git_report": f"opened {PR}",
      "git_task": {"kind": "publish", "open_pr": True, "return_to": "ci_gate"}}
lines, _ = report_lines(merge(st, run("ci_gate", st)))
check("AC2 skipped CI watch named in the report (delegated)", any("parent orchestrator" in l for l in lines), lines)
for label, extra in [("crashed node", {"last_script_error": "debt_scan: killed", "debt_hits": "clean\n"}),
                     ("DEBT_SCAN_FAILED", {"debt_hits": "DEBT_SCAN_FAILED: rg exited 2\n"})]:
    st = {"run_dir": rd("debt" + label), "subtasks": ["a", "b"], "task_number": 1, **extra}
    st2 = merge(st, run("bookkeeping_scan", st))
    st2.pop("last_script_error", None)
    r = run("final_report", st2)["report"]
    debt = r.split("Debt left behind:")[1].split("\n\n")[0]
    check(f"AC2 failed debt scan ({label}) shows as a scan failure, not none",
          "FAILED" in debt and "- none" not in debt, debt)

# ---- AC3: engine cap sized from loop bounds; CI pending bounded by the watch, with an escalation
graph = open(os.path.join(S, "..", "graph.yaml")).read()
cap = int(re.search(r"^\s*max_loop_iterations:\s*(\d+)", graph, re.M).group(1))
check("AC3 settings.max_loop_iterations is raised above the CI watch bound", cap >= 1500, cap)
PEND = checks_env([{"name": "a", "bucket": "pending"}])
u = run("ci_poll", {"run_dir": rd("w240"), "pr_ref": PR, "ci_started_at": now, "ci_polls": 240, "ci_polls_run": 240}, PEND)
r = run("ci_route", merge({"run_dir": rd("w240"), "pr_ref": PR}, u))
check("AC3 a watch that stays pending ends by name (timeout) and escalates",
      u.get("ci_status") == "timeout" and r.get("_next") == "escalate_gate", (u, r.get("_next")))
polls = 0
st = {"run_dir": rd("budget"), "pr_ref": PR, "ci_started_at": now, "ci_polls": 0, "ci_polls_run": 1200}
u = run("ci_poll", st, PEND)
r = run("ci_route", merge(st, u))
e = r.get("escalation") or {}
check("AC3 run-wide CI poll budget escalates ci_poll_cap below the engine cap",
      u.get("ci_status") == "poll_cap" and e.get("kind") == "ci_poll_cap" and u["ci_polls_run"] < cap, (u, e.get("kind")))
st = {"run_dir": rd("budget2"), "pr_ref": PR, "ci_started_at": now, "ci_polls": 0, "ci_polls_run": 1440, "ci_poll_budget": 1440}
u = run("ci_poll", st, PEND)
e = run("ci_route", merge(st, u)).get("escalation") or {}
check("AC3 keep waiting is not offered when one more watch would pass the engine cap",
      not any(o["text"].lower().startswith("keep waiting") for o in e.get("options", [])), e.get("options"))


# ---- AC4: next_subtask never silently skips a review
def T(t):
    return {"title": t, "outcome": t + " done", "acceptance_criteria": [t + " works"]}


d = rd("ns")
rec = run("record_subtasks", {"run_dir": d, "delegated": True, "subtasks": [T("A"), T("B"), T("A"), T("C")]})
check("AC4 record_subtasks drops the exact duplicate and seals subtasks.json",
      [s["title"] for s in rec.get("subtasks", [])] == ["A", "B", "C"] and os.path.exists(os.path.join(d, "subtasks.json")), rec)
st1 = merge({"run_dir": d, "delegated": True}, rec)
st1 = merge(st1, run("next_subtask", st1))
sealed = st1["subtasks"]
for label, st in {
    "duplicate": {**st1, "subtasks": [sealed[0], sealed[0], sealed[1], sealed[2]]},
    "missing list": {k: v for k, v in st1.items() if k != "subtasks"},
    "str list": {**st1, "subtasks": "A, B, C"},
    "truncated list": {**st1, "subtasks": sealed[:1]},
}.items():
    u = run("next_subtask", st)
    check(f"AC4 next_subtask {label} heals from the snapshot and reviews subtask 2",
          u.get("task_number") == 2 and [s["title"] for s in u.get("subtasks", [])] == ["A", "B", "C"], u)
for label, st in {
    "missing index": {k: v for k, v in st1.items() if k != "subtask_index"},
    "str index": {**st1, "subtask_index": "1"},
    "stale index": {**st1, "subtask_index": 2},
}.items():
    u = run("next_subtask", st)
    check(f"AC4 next_subtask {label} -> end_blocked with a reason",
          u.get("_next") == "end_blocked" and u.get("failure_reason"), u)
for f in ("subtasks.json", "tasklog.md"):
    p = os.path.join(d, f)
    if os.path.exists(p):
        os.remove(p)
u = run("next_subtask", {**st1, "subtasks": [sealed[0], sealed[0], sealed[1]]})
check("AC4 duplicate with no snapshot -> end_blocked with a reason",
      u.get("_next") == "end_blocked" and "duplicate" in u.get("failure_reason", ""), u)

# ---- AC5: no fault route without a reason; never ask_user with no question
for node in ("ask_dispatch", "parse_answer"):
    for label, v in [("missing", None), ("list", [{"kind": "ci_exhausted", "question": "q?"}]),
                     ("no kind", {"question": "q?"}), ("blank question", {"kind": "ci_exhausted", "question": " "})]:
        st = {"run_dir": rd(node + label), "last_answer": "Option 1"}
        if v is not None:
            st["escalation"] = v
        u = run(node, st)
        check(f"AC5 {node} escalation {label} -> end_blocked with a reason",
              u.get("_next") == "end_blocked" and u.get("failure_reason"), u)
for label, v in [("missing", None), ("str", "A then B")]:
    st = {"run_dir": rd("rec" + label), "delegated": True}
    if v is not None:
        st["subtasks"] = v
    u = run("record_subtasks", st)
    check(f"AC5 record_subtasks {label} subtasks -> a fault with a reason, not a reasonless end_redirect",
          (u.get("_next") == "end_blocked" and u.get("failure_reason"))
          or (u.get("_next") == "end_redirect" and u.get("redirect_reason")), u)

# ---- AC6: past STRIKE_LIMIT rejections, escalate with evidence and a continue option
d = rd("rej")
u = run("route_coder_result", {"run_dir": d, "coder_result": "CODER_REJECTED\nPlan: forbidden X", "rejected_rounds": 3})
e = u.get("escalation") or {}
opts = [o["text"] for o in e.get("options", [])]
check("AC6 rejected past STRIKE_LIMIT -> coder_rejected_cap escalation with evidence and a continue option",
      u.get("_next") == "escalate_gate" and e.get("kind") == "coder_rejected_cap" and "forbidden X" in e.get("evidence", "")
      and any("again" in o.lower() for o in opts), e)
st = merge({"run_dir": d}, u)
a = run("apply_answer", {**st, "last_answer": next(o for o in opts if "again" in o.lower()) + ": allow Y instead"})
check("AC6 continue re-plans with a fresh budget", a.get("_next") == "compose_fix" and a.get("rejected_rounds") == 0, a)
a = run("apply_answer", {**st, "last_answer": next(o for o in opts if o.lower().startswith("stop"))})
check("AC6 stop ends blocked with a reason", a.get("_next") == "end_blocked" and a.get("failure_reason"), a)

print(f"\n{'FAILED: ' + str(len(FAILS)) if FAILS else 'all checks passed'}")
sys.exit(1 if FAILS else 0)
