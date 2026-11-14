#!/usr/bin/env python3
"""Black-box bad-state sweep for the architect-next script nodes.

Every script node whose graph fallback is `end_failure` is run through its REAL entry
script (subprocess, GRAPH_STATE env) once per bad-state trigger:
`tasks` a plain string, `code_repo_path` / `run_dir` / `plan_path` null, `current_task` /
`tasks` / `tasks_root` missing, and `current_task` null. Each trigger runs twice: with a
saved run on disk (state.json + tasks.json to heal from) and with an empty run dir
(nothing to heal from).

The promise under test: never a bare crash. Each run must exit 0, print a JSON update
whose `_next` names a node of graph.yaml, and never route the first fault to end_failure.
A recoverable case heals and the gate runs; an unrecoverable one routes to the fault
escalation (escalate_gate) with a reason.

Usage: state_sweep_test.py   (exit 1 on any failure; prints one line per failing case)
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

TMP = tempfile.mkdtemp(prefix="ax-sweep-")
os.environ["ARCHITECT_RUNS_ROOT"] = os.path.join(TMP, "runs")
os.environ["HOME"] = os.path.join(TMP, "home")
os.makedirs(os.environ["HOME"], exist_ok=True)
os.environ["ARCHITECT_CI_SLEEP"] = "0"
os.environ["ARCHITECT_REVIEW_SLEEP"] = "0"
for _k in list(os.environ):
    if (_k.startswith(("LLM_AGENT_VAR_", "GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_")) or _k.startswith("ARCHITECT_GH_")
            or _k in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR",
                      "GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT", "ARCHITECT_CORE_TOOLS", "GRAPH_STATE", "GRAPH_STATE_FILE")):
        os.environ.pop(_k)
os.environ.update({"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t",
                   "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"})

BIN = os.path.join(TMP, "bin")
os.makedirs(BIN)
for name in ("gh", "ask_owner", "slack_ping"):
    p = os.path.join(BIN, name)
    with open(p, "w") as f:
        f.write("#!/usr/bin/env bash\necho \"stub: $0 $*\" >&2\nexit 1\n")
    os.chmod(p, os.stat(p).st_mode | stat.S_IEXEC)
os.environ["PATH"] = BIN + os.pathsep + os.environ["PATH"]
os.environ["ARCHITECT_ASK_OWNER_BIN"] = os.path.join(BIN, "ask_owner")
os.environ["ARCHITECT_SLACK_PING_BIN"] = os.path.join(BIN, "slack_ping")
os.environ["LLM_ROOT_DIR"] = os.path.join(TMP, "coyote")  # no real core tools reachable

import gates  # noqa: E402
from _fixture import Store  # noqa: E402


def git(repo, *a):
    return subprocess.run(["git", "-C", repo, *a], capture_output=True, text=True, check=True).stdout.strip()


CODE = os.path.join(TMP, "code")
os.makedirs(CODE)
git(CODE, "init", "-q", "-b", "main")
with open(os.path.join(CODE, "README.md"), "w") as f:
    f.write("# demo\n")
git(CODE, "add", ".")
git(CODE, "commit", "-q", "-m", "init")
git(CODE, "init", "-q", "--bare", CODE + ".git")
git(CODE, "remote", "add", "origin", CODE + ".git")
git(CODE, "push", "-q", "-u", "origin", "main")

STORE = Store(TMP, CODE)
PLAN = STORE.write_promoted_plan("---\ntitle: Widgets\nslug: widgets\nstatus: in-progress\n---\n\n# Widgets\n\n"
                                 "## Quality bar\n\nrigor: production\n\n## Task breakdown\n\n"
                                 "| Task | Title | Points | Blocked by | Files |\n|---|---|---|---|---|\n| 1 | Widget model | 1 | | README.md |\n")
T1, _ = STORE.new_task(gates, "widget-model", files="README.md")
git(CODE, "add", ".")
git(CODE, "commit", "-q", "-m", "chore(plan): tasks")
DESIGN = os.path.join(TMP, "design.md")
with open(DESIGN, "w") as f:
    f.write("# Widgets\n")

RUN_ID = "sweep"
RUN_DIR = os.path.join(os.environ["ARCHITECT_RUNS_ROOT"], RUN_ID)


def base_state():
    s = STORE.variables(DESIGN, TMP, autonomy="full", escalation_policy="interactive", parallel_tasks="0", pr_strategy="single",
                        task_granularity="standard", auto_confirm="1")
    s.update(STORE.normalized())
    s.update({"code_repo_path": CODE, "git_root": CODE, "reference_root_path": TMP, "design_doc_path": DESIGN,
              "run_id": RUN_ID, "run_dir": RUN_DIR, "decisions": [], "anomalies": [], "anomalies_log": [],
              "plan_path": PLAN, "plan_slug": "widgets", "mode_line": "m", "task_rounds": {}, "follow_ups": [],
              "pending_fixes": [], "round_log": {}, "verification_commands": '["true"]', "current_task": T1})
    s["tasks"] = gates.load_tasks(s)
    return s


TASKS = base_state()["tasks"]
assert TASKS and isinstance(TASKS, list), f"fixture produced no tasks: {TASKS!r}"


def reset(saved):
    git(CODE, "checkout", "-q", "-f", "main")
    git(CODE, "clean", "-fdq")
    shutil.rmtree(RUN_DIR, ignore_errors=True)
    os.makedirs(os.path.join(RUN_DIR, "reports"))
    os.makedirs(os.path.join(RUN_DIR, "rounds"))
    if saved:
        with open(os.path.join(RUN_DIR, "tasks.json"), "w") as f:
            json.dump(TASKS, f)
        with open(os.path.join(RUN_DIR, "state.json"), "w") as f:
            json.dump({"run_id": RUN_ID, "run_dir": RUN_DIR, "code_repo_path": CODE, "plan_path": PLAN,
                       "tasks_root": STORE.tasks_root, "current_task": T1, "phase": "task saved"}, f)


def trig_set(key, value):
    def f(s):
        s[key] = value
        return s
    return f


def trig_drop(key):
    def f(s):
        s.pop(key, None)
        return s
    return f


TRIGGERS = {
    "tasks=str": trig_set("tasks", "garbage"),
    "code_repo_path=null": trig_set("code_repo_path", None),
    "run_dir=null": trig_set("run_dir", None),
    "plan_path=null": trig_set("plan_path", None),
    "current_task missing": trig_drop("current_task"),
    "tasks missing": trig_drop("tasks"),
    "tasks_root missing": trig_drop("tasks_root"),
    "current_task=null": trig_set("current_task", None),
}


def graph_nodes():
    p = subprocess.run(["yq", "-o=json", ".nodes", os.path.join(HERE, "..", "graph.yaml")], capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"yq could not read graph.yaml: {p.stderr}")
    return json.loads(p.stdout)


NODES = graph_nodes()
SCRIPT_GATES = sorted(k for k, v in NODES.items()
                      if v.get("fallback") == "end_failure" and v.get("type") == "script" and k in gates.GATES)
ENDS = {"end_failure", "end_aborted", "end_success", "end"}
# A gate that only runs with an owner question open gets one, so the trigger is the only defect in its state
# (apply_answer with no escalation at all stops on "unknown escalation kind" by design, outside this sweep).
GATE_STATE = {"apply_answer": {"escalation": gates.escalation("plan_unauthored", "q", []), "last_answer": "go"}}


def run_entry(gate, state):
    env = {**os.environ, "GRAPH_STATE": json.dumps(state)}
    try:
        p = subprocess.run([sys.executable, os.path.join(HERE, f"{gate}.py")], capture_output=True, text=True, env=env,
                           timeout=120, cwd=TMP)
    except subprocess.TimeoutExpired:
        return None, {}, "timeout"
    lines = p.stdout.strip().splitlines()
    try:
        out = json.loads(lines[-1]) if lines else {}
    except json.JSONDecodeError:
        out = {}
    return p.returncode, out, p.stderr.strip()[-400:]


fails, total = [], 0
outcomes = {}
for gate in SCRIPT_GATES:
    for tname, trig in TRIGGERS.items():
        for saved in (True, False):
            total += 1
            reset(saved)
            rc, out, err = run_entry(gate, trig({**base_state(), **GATE_STATE.get(gate, {})}))
            nxt = out.get("_next")
            case = f"{gate} [{tname}, {'saved run' if saved else 'nothing saved'}]"
            outcomes.setdefault(nxt if isinstance(nxt, str) else repr(nxt), 0)
            outcomes[nxt if isinstance(nxt, str) else repr(nxt)] += 1
            esc = out.get("escalation") or {}
            why = ""
            if rc != 0:
                why = f"exit {rc}: {err}"
            elif (not isinstance(nxt, str) or not nxt) and not NODES[gate].get("next"):
                why = f"no _next in the update: {str(out)[:200]}"
            elif not nxt:
                pass  # the node's static `next` edge routes it
            elif nxt == "end_failure":
                why = f"first fault ended the run: {out.get('failure_reason', '')[:200]}"
            elif nxt not in NODES and nxt not in ENDS:
                why = f"_next {nxt!r} is not a graph node"
            elif esc.get("kind") == "state_fault" and not esc.get("question"):
                why = "state_fault with no reason"
            elif esc.get("kind") == "state_fault" and len(esc.get("options") or []) < 2:
                why = f"state_fault offers no continue/retry: {esc.get('options')}"
            if why:
                fails.append(f"FAIL {case}: {why}")

for line in fails:
    print(line)
print(f"\n{len(SCRIPT_GATES)} end_failure script gates x {len(TRIGGERS)} triggers x 2 disk states")
print("routes:", json.dumps(dict(sorted(outcomes.items(), key=lambda kv: -kv[1]))))
print(f"{total - len(fails)}/{total} cases passed")
shutil.rmtree(TMP, ignore_errors=True)
sys.exit(1 if fails else 0)
