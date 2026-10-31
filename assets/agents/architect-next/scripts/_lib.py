"""Shared helpers for the architect-next gate scripts.

Every gate is a pure function of the graph state that returns the state updates
(plus `_next`). This module owns: state I/O, the run directory (the A2 durable
artifact set, OUTSIDE both repos), ledger lines, the anomalies harvest (A10), the
structured escalation record, the bound constants, the fork constants, the task
store (a directory-state planning repo here, a plans dir upstream) and the caller for the
prose architect's own tools.sh commands (collect_ledger_lines, assemble_pr_body, link_stack,
the store claim/close/push) -
called, never reimplemented.

Bound constants are never invented here: each is the protocol's own number, an
amendment's number (A1, A6), a knobs-table row cited as the analogue, or the
3-strike / re-run-once family the protocol applies to every "same problem" loop.
"""

import datetime as _dt
import hashlib
import json
import os
import re
import subprocess
import sys

# ---------------------------------------------------------------- constants
STRIKE_LIMIT = 3          # 3-strike family (B1, B5, B14, B16, B18, B22)
RERUN_ONCE = 1            # re-run-once family (B3 retry, B4, B6, B9-B13, B17, B23)
AMEND_CYCLES_MAX = 3      # A1: amendment cycles per run
DESIGN_ROUNDS_MAX = STRIKE_LIMIT
CONFLICT_CYCLES_MAX = 2   # Phase E parallel integration: "two failed conflict cycles -> STOP"
WAVE_CAP = 3              # Phase E parallel eligibility: "cap concurrent code tasks at 3"
OWNER_POLL_MAX = 2        # owner-remote-input tool budget: wait 30 / poll 25 minutes
OWNER_WAIT_MINUTES = 30
OWNER_POLL_MINUTES = 25
CI_FLAKE_RERUNS = 2       # section 10.12 / A6: 2 re-runs per check-name per PR
CI_POLL_SLEEP = 60        # pr-watch/scripts/poll_pr.py SLEEP_SECS
CI_POLL_FAILS = 3         # pr-watch/scripts/poll_pr.py MAX_CONSECUTIVE_FAILS
CI_WALL_SECS = 14400      # coder/graph.yaml settings.timeout (hardware-bound wall clock)
CI_MAX_ROUNDS = CI_WALL_SECS // CI_POLL_SLEEP  # A6 rounds cap, derived (240)
VERIFY_CMD_SECS = 900     # gauntlet: each verification command <= 15 min
VERIFY_SET_SECS = 3300    # gauntlet: the whole set <= 55 min
TAIL_CHARS = 6000         # trim_output class: evidence tails handed to llm nodes

# Fork constants (the ONLY script-level fork deltas; design doc section 7, D2/D5/D8).
# Everything below this line down to the hard-trigger regex is fork text; the upstream
# variant substitutes this block wholesale.
FORK = "upstream"
IS_UPSTREAM = FORK == "upstream"
TASKSTORE = "frontmatter" if IS_UPSTREAM else "dirstate"   # dirstate: the directory IS the status (claim/close move it)
MULTI_IS_GROUPS = IS_UPSTREAM             # upstream multi = PR groups
ARCHIVIST_AVAILABLE = not IS_UPSTREAM
STACK_LINKING = not IS_UPSTREAM
OWNER_REMOTE_INPUT = not IS_UPSTREAM      # no Slack owner channel upstream
CORE_TOOLS_REL = "architect"
STORE_TOOL_PREFIX = "store_"              # no store claim/close/push tools upstream (dirstate branches are unreachable)
PLANNING_VAR = "plans_dir"                # the fork's planning-location variable
CODE_VAR = "project_dir"                  # the fork's code-repo variable
STORE_LABEL = "plans"                     # how the planning store is named in ledger lines
PROBE_HOUSE_NOTE = ""                     # no house suite convention; the probe discovers suites
IDIOM_LINE = "Match the project's existing patterns and conventions."
REPO_HINT = "the project at"
SIBLING_HINT = " other local repos/directories"
CONVENTIONS_LINE = "the project's CLAUDE.md / AGENTS.md / CONTRIBUTING.md in {code} - READ BEFORE CODING."
LOG_EVENT_LINE = ""
NO_COMMIT_MAIN_LINE = ""
GATE1_HOUSE_LINE = ""
TEST_FILE_RE = r"(?im)^\s*[-*]?\s*((?:[\w.-]+/)*[\w.-]+\.(?:spec\.\w+|test\.\w+|_test\.go|http))\b"
CONVENTION_FILES = (("code", ("CLAUDE.md", "AGENTS.md", "CONTRIBUTING.md")),)

HARD_TRIGGER_RE = re.compile(
    r"secret|credential|password|token|money|payment|refund|charge|destructive|irreversible|"
    r"force-push|drop (table|column|data)|delete .*branch|rm -rf|Design question:",
    re.I,
)
RESUME_RE = re.compile(r"Architect run id:\s*([0-9]{8}T[0-9]{6}-[0-9a-f]{8}(?:-[0-9]+)?)")
SISYPHUS_RUN_RE = re.compile(r"Sisyphus run id:\s*([0-9]{8}T[0-9]{6}-[0-9a-f]{8})")
TASK_ID_RE = re.compile(r"\bTASK-\d{3,}\b")
SHA_RE = re.compile(r"\b(?=[0-9a-f]*[a-f])[0-9a-f]{7,40}\b")
PR_URL_RE = re.compile(r"https://github\.com/([^\s/]+/[^\s/]+)/pull/(\d+)")
MODE_ENUMS = {
    "pr_strategy": ("multi", "single"),
    "parallel_tasks": ("0", "1"),
    "task_granularity": ("standard", "micro"),
    "autonomy": ("full", "plan-gate", "phase-gate"),
    "escalation_policy": ("interactive", "autonomous"),
}


# ---------------------------------------------------------------- state I/O
def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def emit(updates):
    sys.stdout.write(json.dumps(updates, default=str))
    sys.stdout.write("\n")


def var(state, name, default=""):
    """A graph variable: the seeded state key first, then the LLM_AGENT_VAR_ env."""
    v = state.get(name)
    if v is None or v == "":
        v = os.environ.get(f"LLM_AGENT_VAR_{name.upper()}", default)
    return v


def as_list(v):
    if v is None or v == "":
        return []
    if isinstance(v, list):
        return v
    if isinstance(v, str):
        try:
            parsed = json.loads(v)
            return parsed if isinstance(parsed, list) else [v]
        except json.JSONDecodeError:
            return [v]
    return [v]


def as_dict(v):
    if isinstance(v, dict):
        return v
    if isinstance(v, str) and v.strip():
        try:
            parsed = json.loads(v)
            return parsed if isinstance(parsed, dict) else {}
        except json.JSONDecodeError:
            return {}
    return {}


def as_int(v, default=0):
    try:
        return int(v)
    except (TypeError, ValueError):
        return default


def truthy(v):
    return v is True or str(v).strip().lower() in ("1", "true", "yes")


def tail(text, n=TAIL_CHARS):
    text = text or ""
    return text if len(text) <= n else "... [trimmed] ...\n" + text[-n:]


def now_iso():
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def today():
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%d")


def mode(state):
    """The mode vector as strings (always the five keys)."""
    return {k: str(var(state, k, MODE_ENUMS[k][0] if k != "pr_strategy" else ("single" if IS_UPSTREAM else "multi"))).strip()
            for k in MODE_ENUMS}


def is_single(state):
    return mode(state)["pr_strategy"] == "single"


def is_micro(state):
    return mode(state)["task_granularity"] == "micro"


def is_parallel(state):
    return mode(state)["parallel_tasks"] == "1"


# ---------------------------------------------------------------- run dir (A2)
def cache_dir():
    """Coyote's cache directory, resolved the way the engine resolves it:
    `COYOTE_CACHE_DIR`, else `$XDG_CACHE_HOME/coyote`, else the platform cache
    directory joined with `coyote` (`~/Library/Caches` on macOS, `~/.cache` on
    Linux, `%LOCALAPPDATA%` on Windows)."""
    explicit = os.environ.get("COYOTE_CACHE_DIR")
    if explicit:
        return explicit
    xdg = os.environ.get("XDG_CACHE_HOME")
    if xdg:
        return os.path.join(xdg, "coyote")
    home = os.path.expanduser("~")
    if sys.platform == "darwin":
        base = os.path.join(home, "Library", "Caches")
    elif os.name == "nt":
        base = os.environ.get("LOCALAPPDATA") or os.path.join(home, "AppData", "Local")
    else:
        base = os.path.join(home, ".cache")
    return os.path.join(base, "coyote")


def runs_root():
    """`ARCHITECT_RUNS_ROOT` when set; otherwise `<cache_dir>/architect-next/runs`,
    next to the engine's child-session root and apart from any future `architect` dir."""
    base = os.environ.get("ARCHITECT_RUNS_ROOT")
    if not base:
        base = os.path.join(cache_dir(), "architect-next", "runs")
    return base


def new_run_id(brief):
    """Claims a fresh run dir with `os.mkdir`, so two runs of one brief in the same
    second get `<stamp>-<digest>` and `<stamp>-<digest>-2` instead of sharing a dir."""
    stamp = _dt.datetime.now(_dt.timezone.utc).strftime("%Y%m%dT%H%M%S")
    digest = hashlib.sha1((brief or "").encode("utf-8", "replace")).hexdigest()[:8]
    base = f"{stamp}-{digest}"
    os.makedirs(runs_root(), exist_ok=True)
    n = 1
    while True:
        run_id = base if n == 1 else f"{base}-{n}"
        try:
            os.mkdir(run_dir_for(run_id))
            return run_id
        except FileExistsError:
            n += 1


def run_dir_for(run_id):
    return os.path.join(runs_root(), run_id)


def ensure_run_dir(run_id):
    d = run_dir_for(run_id)
    os.makedirs(os.path.join(d, "reports"), exist_ok=True)
    os.makedirs(os.path.join(d, "rounds"), exist_ok=True)
    return d


def write_artifact(run_dir, name, text):
    if not run_dir:
        return
    os.makedirs(os.path.dirname(os.path.join(run_dir, name)) or run_dir, exist_ok=True)
    with open(os.path.join(run_dir, name), "w", encoding="utf-8") as f:
        f.write(text if text.endswith("\n") else text + "\n")


def append_artifact(run_dir, name, line):
    if not run_dir:
        return
    os.makedirs(run_dir, exist_ok=True)
    with open(os.path.join(run_dir, name), "a", encoding="utf-8") as f:
        f.write(line.rstrip("\n") + "\n")


def read_artifact(run_dir, name):
    try:
        with open(os.path.join(run_dir, name), encoding="utf-8") as f:
            return f.read()
    except OSError:
        return ""


STATE_KEYS = (
    "run_id", "run_dir", "phase", "mode_vector", "code_repo_path", "tasks_root", "plans_root", "base_branch_name",
    "owner_name", "reference_root_path", "design_doc_path", "plan_path", "plan_slug", "plan_revision",
    "amend_cycles", "leak_fingerprints", "oracle_rejects", "gate1_verdict", "gate2_verdict", "sealed",
    "current_task", "current_unit", "run_branch", "group_index", "pending_fixes", "task_rounds",
    "batch_state", "wave_results", "pr_numbers", "ci_reruns", "ci_fix_strikes", "parked", "escalation",
    "autonomous_digest", "decisions", "follow_ups", "rejected_findings", "deferred_findings", "skipped_reviews",
    "design_decisions", "hygiene_items", "anomalies_log", "closeout_scope", "verification_commands",
    "quality_rigor", "quality_surfaces", "round_log",
)


def snapshot(state, updates, phase):
    """Persist the run's durable state (A2) after a gate; `updates` win over state."""
    merged = dict(state)
    merged.update(updates)
    run_dir = merged.get("run_dir")
    if not run_dir:
        return
    snap = {k: merged.get(k) for k in STATE_KEYS}
    snap["phase"] = phase
    snap["updated_at"] = now_iso()
    write_artifact(run_dir, "state.json", json.dumps(snap, indent=1, default=str))


def ledger(state, updates, line):
    """One durable decision/ledger line: decisions.md on disk + the `decisions` list."""
    run_dir = (updates.get("run_dir") or state.get("run_dir") or "")
    stamped = f"- {now_iso()} {line}"
    append_artifact(run_dir, "decisions.md", stamped)
    decisions = list(as_list(updates.get("decisions", state.get("decisions"))))
    decisions.append(line)
    updates["decisions"] = decisions
    return updates


def harvest_anomalies(state, updates, node=""):
    """A10: move a node's optional `anomalies` output into the run-wide log.

    Consecutive llm nodes overwrite the `anomalies` key, so every gate harvests
    it; the list is also appended to anomalies.md so nothing is dropped.
    """
    fresh = [a for a in as_list(state.get("anomalies")) if isinstance(a, str) and a.strip()]
    log = list(as_list(updates.get("anomalies_log", state.get("anomalies_log"))))
    run_dir = updates.get("run_dir") or state.get("run_dir") or ""
    for a in fresh:
        entry = f"{node}: {a.strip()}" if node else a.strip()
        if entry not in log:
            log.append(entry)
            append_artifact(run_dir, "anomalies.md", f"- {now_iso()} {entry}")
    updates["anomalies_log"] = log
    updates["anomalies"] = []
    return updates


def harvest_anomaly_lines(state, updates, text, node):
    """A10 line convention for free-text llm nodes: `ANOMALY: ...` lines."""
    found = [m.strip() for m in re.findall(r"^ANOMALY:\s*(.+)$", text or "", re.M) if m.strip()]
    if found:
        harvest_anomalies({**state, "anomalies": found}, updates, node)
    return updates


def note_script_error(state, updates, where):
    """Record an engine-reported fallback hop, then reset the key (wiki contract)."""
    err = state.get("last_script_error") or ""
    if err:
        ledger(state, updates, f"fallback: {where}: {err[:300]}")
        updates["last_script_error"] = ""
    out = state.get("last_node_output")
    if isinstance(out, str) and out and not out.lstrip().startswith("{") and "failed" in out.lower():
        ledger(state, updates, f"fallback: {where}: {out[:300]}")
        updates["last_node_output"] = ""
    return updates


# ---------------------------------------------------------------- escalations
def option(text, recommended=False, evidence=""):
    return {"text": text, "recommended": bool(recommended), "evidence": evidence or ""}


def escalation(kind, question, options=None, evidence="", hard_trigger=False, tier3_ok=True, data=None, return_to=""):
    return {
        "kind": kind,
        "question": question,
        "options": options or [],
        "evidence": evidence or "",
        "hard_trigger": bool(hard_trigger) or bool(HARD_TRIGGER_RE.search(question or "")),
        "tier3_ok": bool(tier3_ok),
        "data": data or {},
        "return_to": return_to,
        "raised_at": now_iso(),
    }


def render_question(esc):
    """The structured escalation contract (section 10.11.3) as terminal text."""
    lines = [esc.get("question", "").strip()]
    if esc.get("evidence"):
        lines += ["", "Evidence:", esc["evidence"].strip()]
    opts = esc.get("options") or []
    if opts:
        lines += ["", "Options:"]
        for i, o in enumerate(opts, 1):
            mark = " (recommended)" if o.get("recommended") else ""
            ev = f" -- {o['evidence']}" if o.get("evidence") else ""
            lines.append(f"  {i}) {o.get('text', '')}{mark}{ev}")
        lines += ["", "Reply with a number or free text."]
    return "\n".join(lines).strip()


def raise_escalation(state, updates, esc):
    updates["escalation"] = esc
    updates["ask_question"] = render_question(esc)
    updates["last_answer"] = ""
    updates["last_answer_text"] = ""
    updates["reask_rounds"] = 0
    updates["_next"] = "escalate_gate"
    return updates


def match_option(text, options):
    """Map a user reply to an option text: a 1-based number, exact, prefix, or 'recommended'."""
    text = (text or "").strip()
    if not options:
        return text or None
    m = re.match(r"^\s*(\d+)\s*[).:]?\s*$", text)
    if m:
        i = int(m.group(1))
        if 1 <= i <= len(options):
            return options[i - 1]["text"]
    low = text.lower()
    for o in options:
        if o["text"].strip().lower() == low:
            return o["text"]
    for o in options:
        if low and o["text"].strip().lower().startswith(low):
            return o["text"]
    if low in ("recommended", "the recommended option", "go with the recommendation"):
        for o in options:
            if o.get("recommended"):
                return o["text"]
    return None


# ---------------------------------------------------------------- git / shell
def git(cwd, *args, timeout=120):
    try:
        p = subprocess.run(["git", "-C", cwd, *args], capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired) as e:
        return 1, "", str(e)
    return p.returncode, p.stdout, p.stderr


def git_out(cwd, *args, default=""):
    code, out, _ = git(cwd, *args)
    return out.strip() if code == 0 else default


def sh(cmd, cwd=None, timeout=300, env=None):
    """Run a command list; (code, stdout+stderr). Never raises."""
    try:
        p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout, env=env)
        return p.returncode, (p.stdout or "") + (p.stderr or "")
    except FileNotFoundError as e:
        return 127, f"not found: {e}"
    except subprocess.TimeoutExpired:
        return 124, f"timeout after {timeout}s: {' '.join(str(c) for c in cmd)}"
    except OSError as e:
        return 1, str(e)


def core_tools_path():
    """The prose architect's tools.sh (installed copy first, then the sibling agent dir)."""
    if p := os.environ.get("ARCHITECT_CORE_TOOLS"):
        return p
    root = os.environ.get("LLM_ROOT_DIR", "")
    cands = [os.path.join(root, "agents", CORE_TOOLS_REL, "tools.sh") if root else "",
             os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", CORE_TOOLS_REL, "tools.sh")]
    for c in cands:
        if c and os.path.isfile(c):
            return os.path.abspath(c)
    return ""


def core_tool(name, timeout=600, **argc):
    """Call one tools.sh command (argc convention: options arrive as argc_<name> env vars).

    Returns (ok, output). A missing tools.sh is reported, never faked: the caller
    decides whether the step is advisory (note it) or load-bearing (escalate).
    """
    path = core_tools_path()
    if not path:
        return False, f"{name}: architect tools.sh not found (LLM_ROOT_DIR unset and no sibling {CORE_TOOLS_REL}/)"
    # an inherited argc_* (a stale argc_tasks) would act as an option this call never passed
    env = {k: v for k, v in os.environ.items() if not k.startswith("argc_")}
    env["LLM_OUTPUT"] = "/dev/stdout"
    env.setdefault("LLM_ROOT_DIR", os.path.abspath(os.path.join(os.path.dirname(path), "..", "..")))
    env["LLM_AGENT_ROOT_DIR"] = os.path.dirname(path)
    for k, v in argc.items():
        if v is None or v is False:
            continue
        env[f"argc_{k}"] = "1" if v is True else str(v)
    code, out = sh(["bash", "-c", f'set -eo pipefail; source "{path}"; {name}'], timeout=timeout, env=env)
    return code == 0, out


# ---------------------------------------------------------------- task files
def frontmatter(text):
    """Flat `key: value` pairs from a leading --- block; values unquoted; lists as JSON-ish."""
    fm = {}
    if not text.startswith("---"):
        return fm, text
    end = text.find("\n---", 3)
    if end < 0:
        return fm, text
    for line in text[3:end].splitlines():
        m = re.match(r"^([A-Za-z_][\w-]*):\s*(.*?)\s*$", line)
        if m:
            fm[m.group(1)] = m.group(2).strip().strip('"').strip("'")
    return fm, text[end + 4:].lstrip("\n")


def fm_list(value):
    v = (value or "").strip()
    if not v:
        return []
    if v.startswith("["):
        return [x.strip().strip('"').strip("'") for x in v.strip("[]").split(",") if x.strip()]
    return [x.strip() for x in v.split(",") if x.strip()]


def read_file(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return ""


def write_file(path, text):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


def set_frontmatter(path, key, value):
    """Set/insert a flat frontmatter key in place (deterministic edit)."""
    text = read_file(path)
    if not text.startswith("---"):
        text = f"---\n{key}: {value}\n---\n\n" + text
        write_file(path, text)
        return
    end = text.find("\n---", 3)
    head, rest = text[3:end], text[end:]
    if re.search(rf"(?m)^{re.escape(key)}:", head):
        head = re.sub(rf"(?m)^{re.escape(key)}:.*$", f"{key}: {value}", head)
    else:
        head = head.rstrip("\n") + f"\n{key}: {value}\n"
    write_file(path, "---" + head + rest)


def plan_section(plan_text, heading_re):
    """The body of the first `## <heading>` matching heading_re (until the next ## )."""
    m = re.search(rf"(?ims)^##\s+{heading_re}\s*\n(.*?)(?=^##\s|\Z)", plan_text)
    return m.group(1).strip() if m else ""


def task_section(plan_text, task_id, title=""):
    """The plan text that concerns one task: its breakdown row + any heading naming it."""
    out = []
    for line in plan_text.splitlines():
        if task_id in line or (title and title.lower() in line.lower() and line.lstrip().startswith("|")):
            out.append(line)
    sec = re.search(rf"(?ims)^#+\s+[^\n]*{re.escape(task_id)}[^\n]*\n(.*?)(?=^#+\s|\Z)", plan_text)
    if sec:
        out += ["", sec.group(0).strip()]
    if title:
        sec2 = re.search(rf"(?ims)^#+\s+[^\n]*{re.escape(title)}[^\n]*\n(.*?)(?=^#+\s|\Z)", plan_text)
        if sec2 and sec2.group(0).strip() not in "\n".join(out):
            out += ["", sec2.group(0).strip()]
    return "\n".join(out).strip()


def context_pack(text, task_id, title=""):
    """A task's `Context pack` subsection from the plan or its index.md (micro)."""
    for key in (task_id, title):
        if not key:
            continue
        m = re.search(rf"(?ims)^#+\s+[^\n]*{re.escape(key)}[^\n]*\n(.*?)(?=^#+\s+(?!#)|\Z)", text)
        if m and (cp := re.search(r"(?ims)^#+\s+Context pack\s*\n(.*?)(?=^#+\s|\Z)", m.group(1))):
            return cp.group(1).strip()
    m = re.search(r"(?ims)^#+\s+Context pack\s*\n(.*?)(?=^#+\s|\Z)", text)
    return m.group(1).strip() if m else ""


EXECUTABLE_RE = re.compile(
    r"`[^`]+`|\b(go|cargo|npm|pnpm|yarn|make|pytest|python3?|bash|sh|grep|rg|curl|task|just|mvn|gradle|dotnet|bun)\b\s",
    re.I,
)


def looks_executable(criterion):
    """Micro mode: a criterion must name a command (backticked or a known runner)."""
    return bool(EXECUTABLE_RE.search(criterion or ""))


# ---------------------------------------------------------------- task store
def tasks_tree(state):
    return state.get("tasks_root") or ""


def task_dir_for(state, task):
    """The task's current directory (dirstate: active/<owner> or completed/; frontmatter: tasks/<id>-slug)."""
    if isinstance(task, dict) and task.get("dir") and os.path.isdir(task["dir"]):
        return task["dir"]
    tid = task["id"] if isinstance(task, dict) else str(task)
    root = tasks_tree(state)
    cands = []
    if TASKSTORE == "dirstate":
        for sub in (os.path.join("active", state.get("owner_name", "")), "completed", "unassigned", ""):
            d = os.path.join(root, sub) if sub else root
            if os.path.isdir(d):
                cands += [os.path.join(d, n) for n in sorted(os.listdir(d)) if n.startswith(tid)]
    else:
        if os.path.isdir(root):
            cands += [os.path.join(root, n) for n in sorted(os.listdir(root)) if n.startswith(tid)]
    return cands[0] if cands else ""


def load_tasks(state):
    """[(task dict)] from the task store for THIS plan (tasks.json is derived from this)."""
    root = tasks_tree(state)
    plan_base = os.path.basename(state.get("plan_path") or "")
    found = []
    if not root or not os.path.isdir(root):
        return found
    for dp, dirs, files in os.walk(root):
        dirs.sort()
        base = os.path.basename(dp)
        if "index.md" not in files or not re.match(r"^TASK-\d+", base):
            continue
        text = read_file(os.path.join(dp, "index.md"))
        fm, body = frontmatter(text)
        plan = fm.get("plan", "")
        if plan_base and plan and os.path.basename(plan) != plan_base:
            continue
        tid = re.match(r"^(TASK-\d+)", base).group(1)
        status = fm.get("status", "")
        if TASKSTORE == "dirstate":
            rel = os.path.relpath(dp, root)
            status = "complete" if rel.startswith("completed") else ("in-progress" if rel.startswith("active") else "pending")
        found.append({
            "id": tid, "slug": base, "dir": dp, "title": fm.get("title") or _h1(body) or tid,
            "status": status or "pending",
            "blocked_by": [b for b in fm_list(fm.get("blocked_by") or fm.get("depends_on")) if b.startswith("TASK")],
            "batch": fm.get("batch", ""), "protected": truthy(fm.get("protected", "")),
            "surfaces": fm.get("surfaces", ""), "type": fm.get("type", "feature"),
            "files": fm_list(fm.get("files")), "group": fm.get("group", ""), "points": fm.get("points", ""),
        })
    found.sort(key=lambda t: int(re.search(r"\d+", t["id"]).group()))
    return found


def _h1(text):
    m = re.search(r"^#\s+(.+?)\s*$", text or "", re.M)
    return m.group(1) if m else ""


def save_tasks(state, updates, tasks):
    updates["tasks"] = tasks
    write_artifact(updates.get("run_dir") or state.get("run_dir", ""), "tasks.json", json.dumps(tasks, indent=1))
    return updates


def task_by_id(tasks, tid):
    for t in tasks:
        if t.get("id") == tid:
            return t
    return None


def log_event(state, task, text):
    """Append a dated entry to the task's log.md (dirstate: bin/log-event; frontmatter: direct append)."""
    tdir = task_dir_for(state, task)
    if not tdir:
        return False, f"no task dir for {task.get('id') if isinstance(task, dict) else task}"
    if TASKSTORE == "dirstate":
        store = state.get("plans_root") or ""
        tool = os.path.join(store, "bin", "log-event")
        if os.path.isfile(tool):
            code, out = sh([tool, tdir, text], cwd=store, timeout=120)
            if code == 0:
                return True, out
    append_artifact(tdir, "log.md", f"\n## {today()} - {text.splitlines()[0][:60]}\n\n{text}")
    return True, "appended"


def set_task_status(state, task, status):
    """frontmatter store: the status field; dirstate: the directory IS the status (claim/close move it)."""
    if TASKSTORE != "dirstate":
        tdir = task_dir_for(state, task)
        if tdir:
            set_frontmatter(os.path.join(tdir, "index.md"), "status", status)
    return True


def new_task(state, slug, title, body, blocked_by=(), task_type="chore", batch="", plan_name="", points="1"):
    """Create a task through the race-safe path (bin/new-task) or the plans tree (max+1)."""
    root = tasks_tree(state)
    if TASKSTORE == "dirstate":
        store = state.get("plans_root") or ""
        tool = os.path.join(store, "bin", "new-task")
        if not os.path.isfile(tool):
            return "", f"bin/new-task not found under {store}"
        cmd = [tool, state.get("owner_name", ""), slug, "--lifecycle", "active"]
        if plan_name:
            cmd += ["--plan", plan_name]
        cmd += ["--points", str(points)]
        code, out = sh(cmd, cwd=store, timeout=300)
        if code != 0:
            return "", out
        m = re.search(r"(TASK-\d+)", out)
        if not m:
            return "", f"bin/new-task printed no task id: {out[-300:]}"
        tid = m.group(1)
        tdir = task_dir_for(state, tid)
    else:
        os.makedirs(root, exist_ok=True)
        nums = [int(re.match(r"^TASK-(\d+)", n).group(1)) for n in os.listdir(root) if re.match(r"^TASK-\d+", n)]
        tid = f"TASK-{(max(nums) + 1) if nums else 1:03d}"
        tdir = os.path.join(root, f"{tid}-{slug}")
        os.makedirs(tdir, exist_ok=True)
        write_file(os.path.join(tdir, "index.md"),
                   f"---\ntitle: {title}\nstatus: pending\ntype: {task_type}\nplan: {plan_name}\n"
                   f"blocked_by: [{', '.join(blocked_by)}]\n" + (f"batch: {batch}\n" if batch else "") + "---\n\n")
        write_file(os.path.join(tdir, "log.md"), f"# {tid} log\n\n## {today()} - created\n\ncreated by architect-next\n")
    if tdir:
        idx = os.path.join(tdir, "index.md")
        text = read_file(idx)
        fm, _ = frontmatter(text)
        if TASKSTORE == "dirstate":
            if blocked_by:
                set_frontmatter(idx, "blocked_by", "[" + ", ".join(blocked_by) + "]")
            if task_type and task_type != "feature":
                set_frontmatter(idx, "type", task_type)
            if batch:
                set_frontmatter(idx, "batch", batch)
            if title and not fm.get("title"):
                set_frontmatter(idx, "title", title)
        with open(idx, "a", encoding="utf-8") as f:
            f.write(("\n" if not read_file(idx).endswith("\n") else "") + body.rstrip("\n") + "\n")
    return tid, tdir


def commit_planning(state, message):
    """dirstate: the store push tool (board, validate gate, push-race retry); frontmatter: chore(plan) commit when inside the repo."""
    if TASKSTORE == "dirstate":
        ok, out = core_tool(STORE_TOOL_PREFIX + "push", **{PLANNING_VAR: state.get("plans_root", "")}, owner=state.get("owner_name", ""), message=message)
        return ok, out
    plans = state.get("plans_root") or ""
    repo = state.get("code_repo_path") or ""
    inside = plans.startswith(repo.rstrip("/") + "/") if plans and repo else False
    if not inside:
        return True, "plans dir outside the repo: nothing to commit"
    rel = os.path.relpath(plans, repo)
    code, out, err = git(repo, "add", "-A", "--", rel)
    if code != 0:
        return False, out + err
    code, out, err = git(repo, "commit", "-q", "-m", message)
    if code != 0 and "nothing to commit" in (out + err):
        return True, "nothing to commit"
    return code == 0, out + err
