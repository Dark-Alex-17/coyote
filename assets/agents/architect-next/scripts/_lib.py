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
import time

# ---------------------------------------------------------------- constants
STRIKE_LIMIT = 3          # 3-strike family (B1, B5, B14, B16, B18, B22)
RERUN_ONCE = 1            # re-run-once family (B3 retry, B4, B6, B9-B13, B17, B23)
DESIGN_ROUNDS_MAX = STRIKE_LIMIT
# A1: amendment cycles per run, budgeted per source so that one source never spends another's chances
AMEND_DESIGN_MAX = DESIGN_ROUNDS_MAX + 1  # each design round's answers, plus the checklist round's accepted items
AMEND_GATE1_MAX = 2 * (STRIKE_LIMIT + 1)  # the prose architect's 2 leak amend cycles per sealing: the first, and one per oracle REJECT
AMEND_GATE2_MAX = STRIKE_LIMIT            # one amendment per oracle REJECT (B5)
AMEND_REWORK_MAX = STRIKE_LIMIT           # owner revisions, re-scopes, plan defects, recipes, integration findings
# the hard overall ceiling: the sum of the source budgets, so no source spends another's chances through it
AMEND_CYCLES_MAX = AMEND_DESIGN_MAX + AMEND_GATE1_MAX + AMEND_GATE2_MAX + AMEND_REWORK_MAX
CHOICE_OPTIONS_MIN = 2    # a multiple-choice question offers 2-4 options, one recommended
CHOICE_OPTIONS_MAX = 4
DISCUSSION_PARKS_MAX = 3  # times one multiple-choice question may park the run for a discussion
DRAFT_ROUNDS_MAX = STRIKE_LIMIT  # redrafts of an LLM-drafted brief before asking for a design doc
CONFLICT_CYCLES_MAX = 2   # Phase E parallel integration: "two failed conflict cycles -> STOP"
WAVE_CAP = 3              # Phase E parallel eligibility: "cap concurrent code tasks at 3"
OWNER_POLL_MAX = 2        # owner-remote-input tool budget: wait 30 / poll 25 minutes
OWNER_WAIT_MINUTES = 30
OWNER_POLL_MINUTES = 25
CI_FLAKE_RERUNS = 2       # section 10.12 / A6: 2 re-runs per check-name per PR
CI_POLL_SLEEP = 60        # pr-watch/scripts/poll_pr.py SLEEP_SECS
CI_POLL_FAILS = 3         # pr-watch/scripts/poll_pr.py MAX_CONSECUTIVE_FAILS
GH_CALL_SECS = 90         # one gh call or seam run; node timeouts cover the sleep plus this
GH_LOOP_SECS = 180        # time a gate spends issuing gh calls in a loop before it stops; each such node's timeout covers this plus one GH_CALL_SECS call plus margin
CI_WALL_SECS = 14400      # coder/graph.yaml settings.timeout (hardware-bound wall clock)
CI_MAX_ROUNDS = CI_WALL_SECS // CI_POLL_SLEEP  # A6 rounds cap, derived (240)
REVIEW_POLL_SLEEP = 600   # review watch: one poll every 10 minutes
REVIEW_WALL_SECS = 86400  # review watch: one day without approval escalates
REVIEW_MAX_POLLS = REVIEW_WALL_SECS // REVIEW_POLL_SLEEP  # review polls cap, derived (144)
REVIEW_MAX_ROUNDS = STRIKE_LIMIT  # review fix rounds per PR
REVIEW_UPDATE_BUDGET = 600  # review_update: seconds of thread replies before the rest wait for the next round
PERM_LOOKUPS_PER_POLL = 2  # review_poll: uncached permission reads per poll, so the sleep plus every gh call fits the node timeout
POLL_VISIT_BUDGET = 4 * CI_MAX_ROUNDS + CI_MAX_ROUNDS  # four group PRs each watched to the CI wall cap, plus one watch of headroom; below settings.max_loop_iterations
PR_TRACKING_ENUM = ("ci", "review")
VERIFY_CMD_SECS = 900     # gauntlet: each verification command <= 15 min
VERIFY_SET_SECS = 3300    # gauntlet: the whole set <= 55 min
TAIL_CHARS = 6000         # trim_output class: evidence tails handed to llm nodes
GATE2_AMEND_CHARS = 4 * TAIL_CHARS  # an oracle rejection reaches amend_plan whole up to this, then per item from its end

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
# A settled answer only follows a colon; a bare word with trailing punctuation re-asks, and any other prose is not a continue.
CONTINUE_RE = re.compile(r"(?is)^(?:continue|resume)(?:\s*:\s*(.+)|\W*)$")
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


ITEM_START_RE = re.compile(r"^(?:\d+[.)]|[-*+]|#{1,6})\s")
ITEM_HEAD_MIN = 160    # each item keeps this much of its start while the bound has room for it
ITEM_HEAD_FLOOR = 40   # each item keeps at least this much of its start, past the bound if it must
ITEM_TRIMMED = " ... [trimmed]"
BLOCK_TRIMMED = "\n... [trimmed] ...\n"


def trim_items(text, n=TAIL_CHARS):
    """Bound a verdict or report to n chars, keeping the start of every item.

    The text splits into a preamble and the items that start at column 0 (a numbered or
    bulleted line, or a heading). Each block keeps its start and loses only its end, the
    shortest kept whole first, so item 1's start survives however long the rest is. A text
    with no items keeps its start and its end. No item is ever dropped: each keeps at least
    ITEM_HEAD_FLOOR characters, so a header line up to that long survives whole, and a text
    with more items than the bound has room for comes out longer than n.
    """
    text = text or ""
    if len(text) <= n:
        return text
    blocks = []
    for line in text.splitlines(keepends=True):
        if not blocks or ITEM_START_RE.match(line):
            blocks.append(line)
        else:
            blocks[-1] += line
    if len(blocks) == 1:
        room = max(n - len(BLOCK_TRIMMED), 0)
        head = min(len(text.splitlines()[0]), ITEM_HEAD_FLOOR) if ITEM_START_RE.match(text) else 0
        start = max(room // 2, head)
        end = max(room - start, 0)
        if start + end >= len(text):
            return text
        return text[:start] + BLOCK_TRIMMED + (text[-end:] if end else "")
    floor = min(ITEM_HEAD_MIN, max(n // len(blocks) - len(ITEM_TRIMMED), ITEM_HEAD_FLOOR))
    budget = max(n - len(ITEM_TRIMMED) * len(blocks), 0)
    alloc = [0] * len(blocks)
    order = sorted(range(len(blocks)), key=lambda i: len(blocks[i]))
    for rank, i in enumerate(order):
        share = budget // (len(blocks) - rank)
        alloc[i] = min(len(blocks[i]), max(share, floor))
        budget = max(budget - alloc[i], 0)
    out = []
    for block, keep in zip(blocks, alloc):
        if keep >= len(block):
            out.append(block)
            continue
        # the newline the trimmed block ends with comes out of its own share
        newline = "\n" if block.endswith("\n") else ""
        out.append(block[:keep - len(newline)].rstrip() + ITEM_TRIMMED + newline)
    return "".join(out)


def now_iso():
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def today():
    return _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%d")


# Read as _lib._now() so a test can move the clock for every caller at once.
_now = time.time


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
    """Coyote's cache directory, in the engine's precedence order:
    `COYOTE_CACHE_DIR`, else `$XDG_CACHE_HOME/coyote`, else the platform cache
    directory joined with `coyote` (`~/Library/Caches` on macOS, `~/.cache` on
    Linux, `%LOCALAPPDATA%` on Windows). One difference: the engine takes
    `COYOTE_CACHE_DIR` or `XDG_CACHE_HOME` whenever it is set, even to the
    empty string, while this treats an empty value as unset and falls through
    to the next tier."""
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
    # replies and model text may carry lone surrogates; they are written as escapes, never dropped
    with open(os.path.join(run_dir, name), "w", encoding="utf-8", errors="backslashreplace") as f:
        f.write(text if text.endswith("\n") else text + "\n")


def append_artifact(run_dir, name, line):
    if not run_dir:
        return
    os.makedirs(run_dir, exist_ok=True)
    with open(os.path.join(run_dir, name), "a", encoding="utf-8", errors="backslashreplace") as f:
        f.write(line.rstrip("\n") + "\n")


def read_artifact(run_dir, name):
    try:
        with open(os.path.join(run_dir, name), encoding="utf-8") as f:
            return f.read()
    except OSError:
        return ""


STATE_KEYS = (
    "run_id", "run_dir", "phase", "mode_vector", "code_repo_path", "tasks_root", "plans_root", "base_branch_name",
    "owner_name", "reference_root_path", "design_doc_path", "seed_path", "draft_path", "draft_rounds", "draft_feedback",
    "draft_text", "plan_path", "plan_slug", "plan_revision",
    "amend_cycles", "amend_counts", "amend_source", "leak_fingerprints", "oracle_rejects", "gate1_verdict",
    "gate2_verdict", "sealed",
    "current_task", "current_unit", "run_branch", "group_index", "pending_fixes", "task_rounds",
    "batch_state", "wave_results", "pr_numbers", "ci_reruns", "ci_fix_strikes", "parked", "escalation",
    "autonomous_digest", "decisions", "follow_ups", "rejected_findings", "deferred_findings", "skipped_reviews",
    "design_decisions", "hygiene_items", "anomalies_log", "closeout_scope", "verification_commands",
    "quality_rigor", "quality_surfaces", "round_log", "open_questions", "carried_questions",
    "rigor_proposal", "surfaces_proposal", "quality_evidence", "quality_recorded",
    "checklist_labels", "checklist_findings", "checklist_items", "checklist_done",
    "leak_questions", "leak_unanswerable", "leak_pending_answers",
    "design_rounds", "choice_kind", "choice_queue", "choice_index", "choice_answers", "choice_reframed", "choice_parks",
    "choice_forced", "run_branch_override", "run_branch_override_group", "base_branch_override", "code_dir_override",
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


def read_snapshot(run_dir):
    """The run's last snapshot (<run_dir>/state.json); {} when there is none or it is not UTF-8 JSON."""
    if not (isinstance(run_dir, str) and run_dir):
        return {}
    try:
        return as_dict(read_artifact(run_dir, "state.json"))
    except ValueError:
        return {}


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
    if opts and as_dict(esc.get("data")).get("multiple_choice"):
        lines.append("")
        for i, o in enumerate(opts):
            lines.append(f"{choice_label(i)}: {o.get('text', '')}")
            if o.get("evidence"):
                lines.append(f"    Evidence: {o['evidence']}")
        lines += ["", 'Pick an option, or choose "Other (custom)" to type your own answer or to start a discussion '
                      "about this question."]
    elif opts:
        lines += ["", "Options:"]
        for i, o in enumerate(opts, 1):
            mark = " (recommended)" if o.get("recommended") else ""
            ev = f" -- {o['evidence']}" if o.get("evidence") else ""
            lines.append(f"  {i}) {o.get('text', '')}{mark}{ev}")
        lines += ["", "Reply with a number or free text."]
    return "\n".join(lines).strip()


def choice_label(index):
    """The static approval-node label of a multiple-choice option; the recommended one is always first."""
    return "Option 1 (recommended)" if index == 0 else f"Option {index + 1}"


def raise_escalation(state, updates, esc):
    updates["escalation"] = esc
    updates["ask_question"] = render_question(esc)
    updates["last_answer"] = ""
    updates["last_answer_text"] = ""
    updates["reask_rounds"] = 0
    updates["reply_interpreted"] = False
    updates["_next"] = "escalate_gate"
    return updates


# The engine's answer to a free-text question when no human is available (HEADLESS_GUIDANCE in
# src/function/user_interaction.rs; a test there pins this copy to it).
HEADLESS_GUIDANCE = "No human is available to answer. Go with the recommended option; if none is marked, use your best judgment."


def headless_recommended(text, options):
    """The recommended option's text when the reply is exactly the headless guidance; None otherwise or with none marked."""
    if (text or "").strip() != HEADLESS_GUIDANCE:
        return None
    return next((o["text"] for o in options or [] if truthy(o.get("recommended"))), None)


def review_fault(blocker):
    """A gauntlet blocker that records a review fault, not a finding; an adversary complaint quoting the words is a finding."""
    if blocker.startswith("adversary: DIVERGES"):
        return False
    return "PIPELINE-FAULT" in blocker or "review incomplete" in blocker.lower()


# a reply led by an option number: "2", "2.", "2)", "(2)", "#2", "Option 2", then optionally a separator and free text
OPTION_NUMBER_RE = re.compile(r"(?is)^(?:option\s*)?[(#]?\s*(\d{1,9})\s*(?:$|[-:.)\u2014\u2013]\s*(.*)$)")


def split_option_reply(text, options):
    """(option text, free text after it) for a reply led by an in-range option number ("2", "(2)", "#2", "Option 2",
    "2 - x", "2: x", "2) x", or an em or en dash); (None, text) otherwise. A reply whose free text names another
    option ("1 - no wait, 3") picks none. Free text that is only punctuation counts as none."""
    text = (text or "").strip()
    m = OPTION_NUMBER_RE.match(text)
    count = len(options or [])
    if not m or not 1 <= int(m.group(1)) <= count:
        return None, text
    picked = int(m.group(1))
    rest = (m.group(2) or "").strip()
    if any(len(d) <= 9 and int(d) != picked and 1 <= int(d) <= count for d in re.findall(r"\d+", rest)):
        return None, text
    return options[picked - 1]["text"], rest if re.search(r"\w", rest) else ""


def match_option(text, options):
    """Map a user reply to an option text: a 1-based number (free text after it is dropped), exact, prefix, or
    'recommended'. A reply naming two options maps to none."""
    text = (text or "").strip()
    if not options:
        return text or None
    picked, _ = split_option_reply(text, options)
    if picked:
        return picked
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
    with open(path, "w", encoding="utf-8", errors="backslashreplace") as f:
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


# ---------------------------------------------------------------- state heal
HEAL_PATH_KEYS = ("run_dir", "code_repo_path", "plans_root", "tasks_root", "plan_path", "design_doc_path", "reference_root_path",
                  "seed_path", "draft_path")
HEAL_PATH_VARS = {"code_repo_path": CODE_VAR, "plans_root": PLANNING_VAR, "design_doc_path": "design_doc"}


def _task_list(v):
    """v as a list of task dicts, or None when it is not one."""
    if not isinstance(v, (list, str)):
        return None
    tasks = as_list(v)
    return tasks if all(isinstance(t, dict) for t in tasks) else None


def _path_sources(state, snap, key):
    if key == "run_dir":
        run_id = state.get("run_id")
        if isinstance(run_id, str) and run_id and os.path.isdir(run_dir_for(run_id)):
            yield "run_id", run_dir_for(run_id)
    if key == "tasks_root" and isinstance(state.get("plans_root"), str) and state["plans_root"]:
        yield "plans_root", os.path.join(state["plans_root"], "tasks")
    yield "state.json", snap.get(key)
    name = HEAL_PATH_VARS.get(key, key)
    v = var(state, name)
    if isinstance(v, str) and os.path.isabs(os.path.expanduser(v)):
        yield f"variable {name}", os.path.expanduser(v)


def _heal_path(healed, notes, snap, key):
    if key in healed and isinstance(healed[key], str):
        return
    found = next(((src, v) for src, v in _path_sources(healed, snap, key) if isinstance(v, str) and v), None)
    if not found and key in healed:
        found = ("empty", "")
    if found:
        healed[key] = found[1]
        notes.append((key, found[0]))


def heal_state(state):
    """Repair the state keys a gate cannot run without: (healed state, [(key, source)]).

    A key is repaired only when it is wrong: a path key that is missing or not a string, `tasks` that is missing or
    not a list of task dicts, `current_task` that is missing or not a string. A valid value is never replaced. A
    present but wrong value with no source left becomes the initial-state value; a missing one stays missing. A
    `current_task` with no saved value is reported with source None and left for the gate to handle. A state with
    no run yet (no run id and no run dir) is returned unchanged.
    """
    if not any(isinstance(state.get(k), str) and state.get(k) for k in ("run_id", "run_dir")):
        return state, []
    healed, notes = dict(state), []
    _heal_path(healed, notes, {}, "run_dir")
    snap = read_snapshot(healed.get("run_dir"))
    for key in HEAL_PATH_KEYS[1:]:
        _heal_path(healed, notes, snap, key)
    if _task_list(healed.get("tasks")) is None:
        run_dir = healed.get("run_dir")
        try:
            raw = read_artifact(run_dir, "tasks.json") if isinstance(run_dir, str) and run_dir else ""
            saved = _task_list(json.loads(raw)) if raw else None
        except ValueError:
            saved = None
        if saved is not None:
            healed["tasks"], src = saved, "tasks.json"
        elif stored := load_tasks(healed):
            healed["tasks"], src = stored, "the task files"
        elif "tasks" in healed:
            healed["tasks"], src = [], "empty"
        else:
            src = None
        if src:
            notes.append(("tasks", src))
    if not isinstance(healed.get("current_task"), str):
        saved = snap.get("current_task")
        if isinstance(saved, str) and saved:
            healed["current_task"] = saved
            notes.append(("current_task", "state.json"))
        else:
            notes.append(("current_task", None))
    return healed, notes


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
