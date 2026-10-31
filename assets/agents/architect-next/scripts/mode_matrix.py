#!/usr/bin/env python3
"""Mode-matrix coverage proof for architect-next.

Two halves (the transplanted render-combo discipline):

1. MARKER MAP. Every `<!-- when:... -->` region of the prose architect's
   instructions.md must be mapped in marker_map.json to a graph mechanism
   (R routing / I interpolation / N per-mode node / V engine replacement /
   - prose). The map is checked against the LIVE marker list (ordinal,
   condition, first content line) so a new or moved region fails the proof
   until it is mapped. `--regen` rewrites the ordinal/line/first-line fields
   from the live file, keeping the mechanism/home columns.

2. COMBINATION WALK. For every combination of the five mode variables
   (pr_strategy x parallel_tasks x task_granularity x autonomy x
   escalation_policy = 48) the REAL script gates are executed on a synthetic
   five-task plan (one dependency edge, one consumer-facing task, two batches, one protected task) inside scratch repos, with every llm / agent / map /
   approval / input node replaced by a canned island output. The walk must
   reach `end_complete` within a step cap, every edge it takes must name a
   real node, and the mode-specific assertions of section 3 must hold.

Usage: mode_matrix.py [--regen] [--instructions <path>]   (exit 1 on any failure)
"""

import copy
import json
import os
import re
import shutil
import stat
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.dont_write_bytecode = True
sys.path.insert(0, HERE)
for _k in [k for k in os.environ if k.startswith("LLM_AGENT_VAR_")]:
    # the walk must not inherit the agent variables of whoever runs it
    os.environ.pop(_k)
for _k in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR"):
    # a caller's repository variables (a git hook exports them) would point
    # every scratch-repo git call at the caller's own repository
    os.environ.pop(_k, None)
for _k in [k for k in os.environ if k in ("ARCHITECT_CORE_TOOLS", "ARCHITECT_ASK_OWNER_BIN", "ARCHITECT_SLACK_PING_BIN") or (k.startswith("ARCHITECT_GH_") and k.endswith("_CMD"))]:
    # a caller's tool overrides would replace the scripts' own tools.sh and gh calls
    os.environ.pop(_k)
for _k in [k for k in os.environ if k in ("GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT") or k.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))]:
    # a caller's command-line git config would reach every scratch-repo git call
    os.environ.pop(_k)
# HOME becomes a scratch dir with no git identity. git refuses to guess one when
# user.useConfigOnly is set or when the host name has no dot (it cannot build an
# email), so the walk sets useConfigOnly on every host and every gate that
# commits inherits this fixed identity instead
os.environ.update({"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"})
os.environ.update({"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "user.useConfigOnly", "GIT_CONFIG_VALUE_0": "true"})
from _fixture import Store  # noqa: E402

MAP_PATH = os.path.join(HERE, "marker_map.json")
MECHANISMS = {"R", "I", "N", "V", "-"}
AXES = {
    "pr_strategy": ["multi", "single"], "parallel_tasks": ["0", "1"], "task_granularity": ["standard", "micro"],
    "autonomy": ["full", "plan-gate", "phase-gate"], "escalation_policy": ["interactive", "autonomous"],
}
RESULTS = []


def check(name, cond, detail=""):
    RESULTS.append((name, bool(cond)))
    print(("ok   " if cond else "FAIL ") + name + (f"  ({detail})" if detail and not cond else ""))


# ---------------------------------------------------------------- 1. marker map
def live_markers(path):
    lines = open(path, encoding="utf-8").read().splitlines()
    out, ordinal = [], 0
    for i, l in enumerate(lines, 1):
        m = re.match(r"^\s*<!-- when:(\S+) -->\s*$", l)
        if not m:
            continue
        ordinal += 1
        first = ""
        for nxt in lines[i:]:
            if nxt.strip() and not re.match(r"^\s*<!-- (when|end)", nxt):
                first = nxt.strip()[:60]
                break
        out.append({"ordinal": ordinal, "line": i, "condition": m.group(1), "first": first})
    return out


def check_marker_map(instr_path, regen):
    live = live_markers(instr_path)
    mapped = json.load(open(MAP_PATH)) if os.path.exists(MAP_PATH) else []
    if regen:
        by_ord = {m["ordinal"]: m for m in mapped}
        new = []
        for lm in live:
            old = by_ord.get(lm["ordinal"], {})
            new.append({**lm, "mechanism": old.get("mechanism", "?"), "home": old.get("home", "?")})
        json.dump(new, open(MAP_PATH, "w"), indent=1)
        print(f"regenerated {MAP_PATH}: {len(new)} regions")
        mapped = new
    check(f"marker map: {len(live)} live regions == {len(mapped)} mapped", len(live) == len(mapped))
    for lm, mm in zip(live, mapped):
        same = lm["ordinal"] == mm.get("ordinal") and lm["condition"] == mm.get("condition") and lm["first"] == mm.get("first")
        if not same:
            check(f"marker map: region {lm['ordinal']} ({lm['condition']} @ {lm['line']}) matches the map", False, f"map has {mm.get('condition')!r} / {mm.get('first')!r}")
    check("marker map: every region matches (ordinal, condition, first content line)",
          all(lm["ordinal"] == mm.get("ordinal") and lm["condition"] == mm.get("condition") and lm["first"] == mm.get("first") for lm, mm in zip(live, mapped)))
    bad = [m for m in mapped if m.get("mechanism") not in MECHANISMS or not m.get("home") or m.get("home") == "?"]
    check("marker map: every region names a mechanism (R/I/N/V/-) and a graph home", not bad, str([b.get("ordinal") for b in bad]))
    conds = sorted({m["condition"] for m in live})
    axes_seen = {c.lstrip("!").split("!=")[0].split("=")[0] for c in conds}
    check("marker map: the marker conditions span exactly the five mode axes", axes_seen == set(AXES), str(axes_seen))
    return live


# ---------------------------------------------------------------- 2. combination walk
def git(repo, *a, check_=True):
    p = subprocess.run(["git", "-C", repo, *a], capture_output=True, text=True)
    if check_ and p.returncode != 0:
        raise RuntimeError(p.stderr)
    return p.stdout.strip()


def write(path, text, exe=False):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)
    if exe:
        os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)


PLAN_TEXT = """---
title: Widgets
slug: widgets
status: open
rigor: production
---

# Widgets

## Problem

Widgets are missing.

## Approach

Add them.

## Quality bar

rigor: production

## Task breakdown

| Task | Title | Points | Blocked by | Files | Batch | Protected |
|---|---|---|---|---|---|---|
| 1 | Widget model | 1 | | src/a.go | B1 | |
| 2 | Widget endpoint | 1 | 1 | api/h.go | B1 | |
| 3 | Widget worker | 1 | | src/b.go | B2 | |
| 4 | Widget audit | 1 | | src/c.go | B2 | yes |
| 5 | Widget metrics | 1 | | src/d.go | B2 | |

## PR groups

| Group | Title | Members |
|---|---|---|
| g1 | Core | 1, 2 |
| g2 | Worker | 3, 4, 5 |

## Local-run recipe

`make run`
"""


class World:
    """Scratch repos for one combination: code repo + bare origin, and the fork's planning store (_fixture.py)."""

    TASKS = [  # slug, files, blocked (format index), batch, protected, group, surfaces
        ("widget-model", "src/a.go", "", "B1", False, "g1", ""), ("widget-endpoint", "api/h.go", "{0}", "B1", False, "g1", "http"),
        ("widget-worker", "src/b.go", "", "B2", False, "g2", ""), ("widget-audit", "src/c.go", "", "B2", True, "g2", ""),
        ("widget-metrics", "src/d.go", "", "B2", False, "g2", ""),
    ]

    def __init__(self, fork):
        self.tmp = tempfile.mkdtemp(prefix="ax-matrix-")
        self.fork = fork
        self.code = os.path.join(self.tmp, "code")
        os.makedirs(self.code)
        git(self.code, "init", "-q", "-b", "main")
        for rel, text in {"README.md": "# demo\n", "CLAUDE.md": "Run `true`.\n", "src/a.go": "package a\n", "src/b.go": "package b\n",
                          "src/c.go": "package c\n", "src/d.go": "package d\n", "api/h.go": "package api\n"}.items():
            write(os.path.join(self.code, rel), text)
        git(self.code, "add", ".")
        git(self.code, "commit", "-q", "-m", "init")
        git(self.code, "init", "-q", "--bare", self.code + ".git")
        git(self.code, "remote", "add", "origin", self.code + ".git")
        git(self.code, "push", "-q", "-u", "origin", "main")
        self.design = os.path.join(self.tmp, "design.md")
        write(self.design, "# Widget service\n\nAdd widgets.\n")
        self.store = Store(self.tmp, self.code)
        self.plan_path = self.store.plan_path

    def variables(self, combo):
        return self.store.variables(self.design, self.tmp, auto_confirm="1", **combo)

    def promote(self, gates, state):
        """Canned promote_tasks: the promoted PLAN + five tasks through the fork's task path."""
        self.store.write_promoted_plan(PLAN_TEXT.replace("status: open", "status: in-progress"))
        ids = []
        for slug, files, blocked, batch, prot, group, surf in self.TASKS:
            tid, _ = self.store.new_task(gates, slug, files=files, blocked=blocked.format(*ids) if blocked else "", batch=batch,
                                         protected=prot, group=group, surfaces=surf)
            ids.append(tid)
        return self.store.promote_report()

    def delegate_report(self, gates, state, task_id, project_dir=None):
        """Canned sisyphus: commit a line on the task's branch/worktree; report with a PR URL in multi."""
        tasks = gates.as_list(state.get("tasks"))
        task = gates.task_by_id(tasks, task_id) or {}
        single = gates.is_single(state) or gates.MULTI_IS_GROUPS
        cwd = project_dir or task.get("worktree") or self.code
        if not single and not task.get("worktree"):
            # Sisyphus cuts its own branch from the PR base.
            base = task.get("pr_base") or "main"
            git(self.code, "fetch", "-q", "origin")
            if git(self.code, "rev-parse", "--verify", "--quiet", f"refs/heads/{task.get('branch')}", check_=False) == "":
                git(self.code, "checkout", "-q", "-b", task.get("branch"), f"origin/{base}" if git(self.code, "rev-parse", "--verify", "--quiet", f"refs/remotes/origin/{base}", check_=False) else base)
            else:
                git(self.code, "checkout", "-q", task.get("branch"))
        fname = (task.get("files") or [f"work/{task_id}.txt"])[0]
        path = os.path.join(cwd, fname)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "a") as f:
            f.write(f"// {task_id} {state.get('delegation_kind')}\n")
        git(cwd, "add", "-A")
        git(cwd, "commit", "-q", "-m", f"feat({task_id}): work", check_=False)
        branch = git(cwd, "branch", "--show-current")
        git(cwd, "push", "-q", "-u", "origin", branch, check_=False)
        head = git(cwd, "rev-parse", "HEAD")
        if not single and not task.get("worktree"):
            git(self.code, "checkout", "-q", "main")
        pr = f"\nDraft PR opened: https://github.com/o/r/pull/{abs(hash(task_id)) % 900 + 100}\n" if not single else ""
        return (f"Implemented {task_id}.{pr}HEAD SHA: {head}\n\nFOLLOW-UPS: none\n\nDebt left behind:\n- none\n\nDebris exemptions:\n- none\n\n"
                f"Design decisions:\n- none\n")


def load_graph():
    p = subprocess.run(["yq", "-o=json", ".", os.path.join(HERE, "..", "graph.yaml")], capture_output=True, text=True)
    if p.returncode != 0:
        sys.exit(f"yq failed to read graph.yaml (exit {p.returncode}): {p.stderr.strip()}")
    return json.loads(p.stdout)


def walk(combo, gates, graph, world, log):
    nodes = graph["nodes"]
    state = dict(graph.get("initial_state", {}))
    state.update(world.variables(combo))
    for k, v in world.variables(combo).items():
        os.environ[f"LLM_AGENT_VAR_{k.upper()}"] = str(v)
    state["initial_prompt"] = f"Implement the design doc at {world.design}"
    cur, steps, visited = graph["start"], 0, []
    asserts = {}

    def canned(nid, st):
        """Island outputs per node id (state updates applied directly)."""
        t = nodes[nid]["type"]
        if nid == "orient_read":
            return {"design_title": "Widget service", "conventions_summary": "c", "verification_commands": '["true"]', "local_run_recipe": "`make run`",
                    "consumer_surfaces": ["/widgets"], "archivist_questions": ["what was decided?"], "convention_discrepancies": [], "anomalies": ["design doc dates a stale PRD"]}
        if nid == "plan_research":
            return {"explore_slices": ["a", "b"], "librarian_questions": ["lib?"], "team_mode": True, "anomalies": []}
        if nid == "author_plan":
            write(world.plan_path, PLAN_TEXT)
            return {"plan_path": world.plan_path, "plan_slug": "widgets", "breakdown_rows": [], "acceptance_criteria": ["`true` exits 0"], "open_questions": [],
                    "rigor_proposal": "production", "surfaces_proposal": "http", "quality_evidence": "e", "anomalies": []}
        if nid == "amend_plan":
            return {"amended": True, "amend_summary": "applied", "anomalies": []}
        if nid == "answer_leaks":
            return {"leak_answers": [{"question": "q", "answer": "a"}], "leak_unanswerable": [], "anomalies": []}
        if nid == "gate1":
            return {"gate1_result": "PLAN_GATE: SEALED\nfine"}
        if nid == "gate2":
            return {"gate2_result": "PLAN_REVIEW: OKAY"}
        if nid == "promote_tasks":
            return {"promote_report": world.promote(gates, st)}
        if nid == "delegate":
            return {"delegate_result": world.delegate_report(gates, st, st.get("current_task"))}
        if nid in ("run_gauntlet", "wave_gauntlet"):
            key = "gauntlet_result" if nid == "run_gauntlet" else "wave_gauntlet_result"
            return {key: "GAUNTLET: PASS\n\n| lane | status | detail |\n|------|--------|--------|\n"
                         "| code-review | GREEN (attention) | NEEDS-HUMAN, no critical |\n| adversary | GREEN | CONFORMS |\n"
                         "| security | SKIPPED | not selected |\n| probe | GREEN | PASS |\n| debris | GREEN | 0 hits |\n\n"
                         "## Lane selection\n(none recorded)\n\n## Full lane reports\n\n"
                         "<details><summary>probe</summary>\n\nUSAGE_PROBE: PASS\n\n</details>\n"}
        if nid == "run_adversary":
            return {"adversary_result": "ADVERSARIAL_REVIEW: CONFORMS"}
        if nid == "task_done":
            return {"task_done_report": "edited index.md and log.md"}
        if nid == "oracle_diagnose":
            return {"oracle_result": "direction"}
        if nid == "batch_triage":
            return {"finding_routes": [], "anomalies": []}
        if nid == "integrate_findings":
            return {"integration_amendments": [], "integration_follow_ups": ["rotate the widget key (post-merge)"], "integration_notes": [], "anomalies": []}
        if nid == "pr_summary":
            return {"pr_summary_text": "Widgets were missing; we added them.", "anomalies": []}
        if nid == "pr_body_fix":
            return {"pr_body_fix_report": "fixed"}
        if nid == "ci_diagnose":
            return {"ci_diagnosis": "x", "ci_verdict": "unsure", "failing_checks": [], "anomalies": []}
        if nid == "ladder_judge":
            return {"ladder_resolution": "recommended", "ladder_choice": "", "ladder_basis": "recommended option", "anomalies": []}
        if nid == "consult_navigator":
            return {"consult_result": "CONSULT: ANSWER 1"}
        if nid == "summarize_run":
            return {"summary": "done"}
        if nid == "run_whetstone":
            return {"whetstone_result": "ok"}
        if nid == "wave_delegate":
            w = gates.as_dict(st.get("wtask"))
            return {"wave_delegate_result": world.delegate_report(gates, st, w.get("task"), project_dir=w.get("worktree"))}
        if nid in ("explore_slice_team", "explore_slice_solo", "archivist_question", "librarian_question", "checklist_librarian"):
            return {"finding": f"finding for {st.get('slice') or st.get('question') or st.get('label')}"}
        raise RuntimeError(f"no canned output for {t} node {nid}")

    def run_chain(entry, st):
        """A map branch chain on a fork of the state; returns the fork."""
        fork, nid, hops = dict(st), entry, 0
        while nid and hops < 40:
            hops += 1
            n = nodes[nid]
            if n["type"] == "script":
                upd = gates.GATES[gates.gate_name(n["script"])](fork)
                fork.update(upd)
                nid = upd.pop("_next", None) or n.get("next")
            else:
                fork.update(canned(nid, fork))
                nid = n.get("next")
        return fork

    try:
        return _walk_loop(nodes, state, cur, visited, asserts, log, gates, canned, run_chain)
    except Exception as e:  # noqa: BLE001
        log.append(f"exception at {visited[-1] if visited else cur}: {type(e).__name__}: {e}")
        return False, state, asserts, visited


def _walk_loop(nodes, state, cur, visited, asserts, log, gates, canned, run_chain):
    steps = 0
    while steps < 600:
        steps += 1
        if cur not in nodes:
            log.append(f"UNDEFINED EDGE -> {cur!r} from {visited[-1] if visited else '?'}")
            return False, state, asserts, visited
        visited.append(cur)
        n = nodes[cur]
        t = n["type"]
        if t == "end":
            return cur == "end_complete", state, asserts, visited
        if t == "script":
            upd = gates.GATES[gates.gate_name(n["script"])](state)
            # Section 3 assertions, recorded as the gates run.
            if cur == "autonomy_gate":
                asserts["plan_gate"] = upd["_next"]
            if cur == "task_done_route":
                asserts.setdefault("task_done_route", set()).add(upd["_next"])
            if cur == "escalate_gate":
                asserts.setdefault("escalate", set()).add((state.get("escalation", {}).get("hard_trigger"), upd["_next"]))
            if cur == "branch_setup":
                asserts["run_branch"] = upd.get("run_branch", "")
            if cur == "select_tasks":
                asserts.setdefault("select", set()).add(upd["_next"])
            if cur == "verify_gate":
                asserts.setdefault("verify", set()).add((state.get("verify_scope") or "task", upd["_next"], "get_diff" in upd["gauntlet_prompt"], "probe" in upd["gauntlet_prompt"].split("Forced lanes:")[1].splitlines()[0] if "Forced lanes:" in upd["gauntlet_prompt"] else False))
            if cur == "compose_delegation":
                p = upd["delegation_prompt"]
                asserts.setdefault("delegation", set()).add(("Do NOT open a PR" in p, "DRAFT PR" in p, "## MODE" in p, "### Context pack (verbatim)" in p))
            if cur == "wave_cleanup":
                asserts.setdefault("cleanup", set()).add(tuple(d for d in upd.get("decisions", []) if d.startswith("cleanup:") and "branch" in d)[-1:])
            if cur == "phase_f_gate":
                asserts["hygiene_units"] = [u["id"] for u in upd.get("hygiene_units", [])]
            if cur == "final_report":
                asserts["report"] = upd["report"]
            state.update(upd)
            nxt = upd.get("_next") or n.get("next")
            state.pop("_next", None)
        elif t == "map":
            items = state.get(n["over"].strip("{}"), [])
            results = []
            for it in items:
                fork = dict(state)
                fork.pop(n["output_key"], None)
                fork[n["as"]] = it
                fork = run_chain(n["branch"], fork)
                results.append(fork.get(n["output_key"]))
            state[n["collect_into"]] = results
            nxt = n["next"]
        elif t in ("llm", "agent"):
            state.update(canned(cur, state))
            nxt = n.get("next")
        elif t == "approval":
            opts = n["options"]
            choice = opts[0]
            state.update({"last_answer": choice, "last_answer_text": choice})
            nxt = n["routes"][choice]
            asserts.setdefault("approvals", set()).add(cur)
        elif t == "input":
            state["last_answer_text"] = "1"
            asserts.setdefault("inputs", set()).add(state.get("escalation", {}).get("kind"))
            nxt = n["next"]
        else:
            raise RuntimeError(t)
        cur = nxt
    log.append("step cap exceeded")
    return False, state, asserts, visited


def main():
    regen = "--regen" in sys.argv
    instr = None
    if "--instructions" in sys.argv:
        instr = sys.argv[sys.argv.index("--instructions") + 1]
    import gates  # noqa: WPS433
    from _lib import CORE_TOOLS_REL  # noqa: WPS433
    gates.gate_name = lambda script: re.search(r'run\("([a-z0-9_]+)"\)', open(os.path.join(HERE, os.path.basename(script))).read()).group(1)
    if instr is None:
        instr = os.path.join(HERE, "..", "..", CORE_TOOLS_REL, "instructions.md")
    if os.path.isfile(instr):
        check_marker_map(instr, regen)
    else:
        check("marker map: instructions.md located", False, instr)

    tmp_home = tempfile.mkdtemp(prefix="ax-matrix-home-")
    try:
        walk_combinations(tmp_home)
    finally:
        shutil.rmtree(tmp_home, ignore_errors=True)
    fails = [n for n, ok in RESULTS if not ok]
    print(f"\n{len(RESULTS) - len(fails)}/{len(RESULTS)} checks passed")
    sys.exit(1 if fails else 0)


def walk_combinations(tmp_home):
    import gates  # noqa: WPS433
    from _lib import CORE_TOOLS_REL, FORK, MULTI_IS_GROUPS, STACK_LINKING  # noqa: WPS433
    os.environ["HOME"] = tmp_home
    os.environ["ARCHITECT_RUNS_ROOT"] = os.path.join(tmp_home, "runs")
    os.environ["ARCHITECT_CI_SLEEP"] = "0"
    os.environ["ARCHITECT_GH_COMMENT_CMD"] = "true"
    os.environ["ARCHITECT_GH_CREATE_CMD"] = "echo https://github.com/o/r/pull/99"
    os.environ["ARCHITECT_GH_READY_CMD"] = "true"
    os.environ["ARCHITECT_GH_CHECKS_CMD"] = "echo '[{\"name\":\"build\",\"bucket\":\"pass\",\"link\":\"\"}]'"
    os.environ["ARCHITECT_SLACK_PING_BIN"] = os.path.join(tmp_home, "nope")
    # The REAL tools.sh through a scratch LLM_ROOT_DIR (copies only).
    root = os.path.join(tmp_home, "coyote")
    core = CORE_TOOLS_REL
    shutil.copytree(os.path.join(HERE, "..", "..", core), os.path.join(root, "agents", core))
    utils_src = os.path.join(HERE, "..", "..", ".shared", "utils.sh")
    if not os.path.isfile(utils_src) or "set -euo pipefail" not in open(utils_src).read():
        sys.exit(f"utils.sh: the repo asset is missing or is not the real file ({utils_src}); the walk needs it, not a stub")
    write(os.path.join(root, "agents", ".shared", "utils.sh"), open(utils_src).read())
    print("utils.sh: real asset")
    write(os.path.join(root, "agents", ".shared", "debris.py"), "import sys\nsys.exit(0)\n")
    os.environ["LLM_ROOT_DIR"] = root
    ghbin = os.path.join(tmp_home, "bin")
    write(os.path.join(ghbin, "gh"), "#!/usr/bin/env bash\nexit 1\n", exe=True)
    os.environ["PATH"] = ghbin + os.pathsep + os.environ["PATH"]

    graph = load_graph()
    combos = [dict(zip(AXES, vals)) for vals in __import__("itertools").product(*AXES.values())]
    check(f"combination walk: {len(combos)} combinations enumerated (2x2x2x3x2)", len(combos) == 48)
    for combo in combos:
        label = " ".join(f"{k}={v}" for k, v in combo.items())
        world = World(FORK)
        log = []
        try:
            ok, st, A, visited = walk(combo, gates, graph, world, log)
        except Exception as e:  # noqa: BLE001
            ok, st, A, visited = False, {}, {}, []
            log.append(f"exception: {type(e).__name__}: {e}")
        single = combo["pr_strategy"] == "single" or MULTI_IS_GROUPS
        micro = combo["task_granularity"] == "micro"
        par = combo["parallel_tasks"] == "1"
        problems = []
        if not ok:
            problems.append("did not reach end_complete: " + "; ".join(log[-2:]) + f" (last nodes {visited[-6:]})")
        if A.get("plan_gate") != ("approve_breakdown" if combo["autonomy"] == "plan-gate" else "promote_tasks"):
            problems.append(f"autonomy_gate -> {A.get('plan_gate')}")
        if combo["autonomy"] == "phase-gate" and "approve_next" not in A.get("approvals", set()):
            problems.append("phase-gate never approved a task")
        if combo["autonomy"] != "phase-gate" and "approve_next" in A.get("approvals", set()):
            problems.append("approve_next reached outside phase-gate")
        if combo["autonomy"] != "plan-gate" and "approve_breakdown" in A.get("approvals", set()):
            problems.append("approve_breakdown reached outside plan-gate")
        esc = A.get("escalate", set())
        if combo["escalation_policy"] == "autonomous":
            if any(nxt != "ladder_judge" for hard, nxt in esc if not hard):
                problems.append("autonomous: a non-hard escalation bypassed the ladder")
        else:
            if any(nxt != "ask_dispatch" for _, nxt in esc):
                problems.append("interactive: an escalation entered the ladder")
        if bool(A.get("run_branch")) != single:
            problems.append(f"branch_setup run_branch={A.get('run_branch')!r}")
        sel = A.get("select", set())
        if par and "wave_claim" not in sel:
            problems.append("parallel never formed a wave")
        if not par and "wave_claim" in sel:
            problems.append("sequential formed a wave")
        ver = A.get("verify", set())
        task_lanes = {v[1] for v in ver if v[0] == "task"}
        if micro and task_lanes != {"run_adversary"}:
            problems.append(f"micro task scope lanes {task_lanes}")
        if not micro and "run_adversary" in task_lanes:
            problems.append("standard used the adversary at task scope")
        if micro and not any(v[0] == "batch" for v in ver):
            problems.append("micro never ran a batch gauntlet")
        if any(v[2] for v in ver) != (not single):
            problems.append(f"diff spec get_diff={any(v[2] for v in ver)} for single={single}")
        if not micro and not any(v[3] for v in ver):
            problems.append("consumer-facing task never forced the probe lane")
        dele = A.get("delegation", set())
        if any(d[0] and d[1] for d in dele):
            problems.append("a delegation rendered BOTH the single and the multi blocks")
        if any(d[0] != single for d in dele):
            problems.append("delegation single/multi block mismatch")
        if any(d[2] != micro for d in dele) and not par:
            problems.append("MODE block iff micro violated (sequential)")
        if any(d[3] != micro for d in dele):
            problems.append("Context pack pasted iff micro violated")
        tdr = A.get("task_done_route", set())
        if micro and "batch_gauntlet_build" not in tdr:
            problems.append("micro batch boundary never reached batch_gauntlet_build")
        hu = A.get("hygiene_units", None)
        if hu is not None:
            if single and hu != ["run"] and hu != []:
                problems.append(f"single hygiene units {hu}")
            if not single and STACK_LINKING and not all(h.startswith("chain-") for h in hu):
                problems.append(f"multi hygiene units {hu}")
        rep = A.get("report", "")
        if micro and "## Batch gauntlets" not in rep:
            problems.append("micro report lacks the batch rollup")
        if combo["escalation_policy"] == "autonomous" and "## Decisions taken autonomously" not in rep:
            problems.append("autonomous report lacks the digest")
        if not single and STACK_LINKING and "## Stacks" not in rep:
            problems.append("multi report lacks the stacks section")
        check(f"combo [{label}]", not problems, " | ".join(problems))
        shutil.rmtree(world.tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
