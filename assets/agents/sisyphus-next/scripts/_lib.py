"""Shared helpers for the sisyphus-next gate scripts.

Every gate is a pure function of the graph state that returns the state updates
(plus `_next`). This module owns: state I/O, the run directory (the A2 durable
artifact set, OUTSIDE the project tree), ledger lines, the anomalies harvest
(A10), the structured escalation record, and the bound constants.

Bound constants are never invented here: each is the protocol's own number, a
knobs-table row cited as the analogue, or the 3-strike / re-run-once family the
protocol applies to every "same problem" loop.
"""

import datetime as _dt
import hashlib
import json
import os
import re
import subprocess
import sys

# ---------------------------------------------------------------- constants
STRIKE_LIMIT = 3          # Phase 7 3-strike rule; also the family bound (B1, B7, B10, B14)
RERUN_ONCE = 1            # adversary/security/probe/debris/debt/obs re-run-once family
SUBTASK_MAX = 3           # A11
CLARIFY_MAX = STRIKE_LIMIT
REASK_MAX = STRIKE_LIMIT
OWNER_POLL_MAX = 2        # ask_owner tool budget: wait 30 / poll 25 minutes
OWNER_WAIT_MINUTES = 30
OWNER_POLL_MINUTES = 25
CI_FLAKE_RERUNS = 2       # section 10.12: max 2 re-runs per check per PR
CI_POLL_SLEEP = 60        # pr-watch/scripts/poll_pr.py SLEEP_SECS
CI_POLL_FAILS = 3         # pr-watch/scripts/poll_pr.py MAX_CONSECUTIVE_FAILS
CI_WALL_SECS = 14400      # coder/graph.yaml settings.timeout (hardware-bound wall clock)
TAIL_CHARS = 6000         # trim_output class: evidence tails handed to llm nodes

# Fork constants (the ONLY script-level fork deltas; design doc section 7).
OWNER_REMOTE_INPUT = False  # upstream: no Slack owner channel; ask_dispatch never routes to ask_owner_poll
PROBE_HOUSE_NOTE = ""  # upstream: no house suite convention; the probe discovers suites

HARD_TRIGGER_RE = re.compile(
    r"secret|credential|password|token|money|payment|refund|charge|destructive|irreversible|"
    r"force-push|drop (table|column|data)|delete .*branch|rm -rf",
    re.I,
)
RESUME_RE = re.compile(r"Sisyphus run id:\s*([0-9]{8}T[0-9]{6}-[0-9a-f]{8})")
DELEGATED_RE = re.compile(r"## SOURCE OF TRUTH|\bTASK-\d{3}\b|Teammate roster", re.I)
QUALITY_RE = re.compile(r"Quality bar:\s*rigor=([A-Za-z]+)(?:[,;]?\s*surfaces=([^\n]*))?")


# ---------------------------------------------------------------- state I/O
def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def emit(updates):
    sys.stdout.write(json.dumps(updates))
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


# ---------------------------------------------------------------- run dir (A2)
def cache_dir():
    """Coyote's cache directory: `COYOTE_CACHE_DIR`, else `$XDG_CACHE_HOME/coyote`,
    the same precedence as the engine for the two environment tiers. The last tier
    approximates the engine's platform tier: the platform cache directory joined
    with `coyote` (`~/Library/Caches` on macOS, `~/.cache` on Linux,
    `%LOCALAPPDATA%` on Windows). Two differences remain: the engine treats a
    set-but-empty variable as set where this treats it as unset, and the engine
    takes the platform directory from the `dirs` crate (the Known Folder API on
    Windows) and falls back to the temp dir."""
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
    """`SISYPHUS_RUNS_ROOT` when set; otherwise `<cache_dir>/sisyphus-next/runs`,
    next to the engine's child-session root and apart from any future `sisyphus` dir."""
    base = os.environ.get("SISYPHUS_RUNS_ROOT")
    if not base:
        base = os.path.join(cache_dir(), "sisyphus-next", "runs")
    return base


def new_run_id(brief):
    stamp = _dt.datetime.now(_dt.timezone.utc).strftime("%Y%m%dT%H%M%S")
    digest = hashlib.sha1((brief or "").encode("utf-8", "replace")).hexdigest()[:8]
    return f"{stamp}-{digest}"


def run_dir_for(run_id):
    return os.path.join(runs_root(), run_id)


def ensure_run_dir(run_id):
    d = run_dir_for(run_id)
    os.makedirs(d, mode=0o700, exist_ok=True)
    return d


def _open_private(path, flags):
    """Open `path` for writing as an owner-only (0600) file, tightening one that
    already exists, since os.open applies the mode only on creation."""
    fd = os.open(path, flags, 0o600)
    try:
        os.chmod(path, 0o600)
    except OSError:
        pass
    return os.fdopen(fd, "a" if flags & os.O_APPEND else "w", encoding="utf-8")


def write_artifact(run_dir, name, text):
    if not run_dir:
        return
    os.makedirs(run_dir, mode=0o700, exist_ok=True)
    with _open_private(os.path.join(run_dir, name), os.O_CREAT | os.O_WRONLY | os.O_TRUNC) as f:
        f.write(text if text.endswith("\n") else text + "\n")


def append_artifact(run_dir, name, line):
    if not run_dir:
        return
    os.makedirs(run_dir, mode=0o700, exist_ok=True)
    with _open_private(os.path.join(run_dir, name), os.O_CREAT | os.O_WRONLY | os.O_APPEND) as f:
        f.write(line.rstrip("\n") + "\n")


def read_artifact(run_dir, name):
    try:
        with open(os.path.join(run_dir, name), encoding="utf-8") as f:
            return f.read()
    except OSError:
        return ""


STATE_KEYS = (
    "run_id", "base_sha", "head_sha", "git_root", "project_dir", "delegated", "top_level",
    "parallel_mode", "intent", "sizing", "subtasks", "subtask_index", "current_task",
    "fix_strikes", "fix_round", "review_rounds", "round_heads", "gauntlet_verdict",
    "gauntlet_lanes", "rejected_findings", "deferred_findings", "skipped_reviews",
    "debt_left_behind", "debris_exemptions", "design_decisions", "autonomous_digest",
    "decisions", "follow_ups", "parked", "escalation", "coder_result", "git_report",
    "ci_status", "anomalies_log",
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


def note_script_error(state, updates, where):
    """Record an engine-reported fallback hop, then reset the key (wiki contract).

    Script fallbacks arrive as `last_script_error`; llm fallbacks arrive through
    the node's `state_updates: {last_node_output: '{{output}}'}` as the
    "...failed: <chain>" string (a successful llm node leaves its JSON there).
    """
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


def escalation(kind, question, options=None, evidence="", hard_trigger=False, tier3_ok=True, data=None):
    return {
        "kind": kind,
        "question": question,
        "options": options or [],
        "evidence": evidence or "",
        "hard_trigger": bool(hard_trigger) or bool(HARD_TRIGGER_RE.search(question or "")),
        "tier3_ok": bool(tier3_ok),
        "data": data or {},
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


# ---------------------------------------------------------------- git
def git(cwd, *args, timeout=60):
    try:
        p = subprocess.run(
            ["git", "-C", cwd, *args], capture_output=True, text=True, timeout=timeout
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        return 1, "", str(e)
    return p.returncode, p.stdout, p.stderr


def git_out(cwd, *args, default=""):
    code, out, _ = git(cwd, *args)
    return out.strip() if code == 0 else default


def changed_files_since(root, base):
    files = []
    if base:
        files += [f for f in git_out(root, "diff", "--name-only", base).splitlines() if f.strip()]
    else:
        files += [f for f in git_out(root, "diff", "--name-only", "HEAD").splitlines() if f.strip()]
    files += [
        f for f in git_out(root, "ls-files", "--others", "--exclude-standard").splitlines() if f.strip()
    ]
    seen, out = set(), []
    for f in files:
        if f not in seen:
            seen.add(f)
            out.append(f)
    return out


def added_lines_since(root, base):
    spec = [base] if base else ["HEAD"]
    code, out, _ = git(root, "diff", "--no-color", "--no-ext-diff", *spec, timeout=120)
    if code != 0:
        return ""
    return "\n".join(l for l in out.splitlines() if l.startswith("+") and not l.startswith("+++"))
