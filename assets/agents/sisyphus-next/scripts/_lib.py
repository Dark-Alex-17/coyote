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
SUBTASK_MAX = 6           # A11: in-run subtasks one run takes on; more is the architect's, asked (sizing verdict)
CLARIFY_MAX = STRIKE_LIMIT
REASK_MAX = STRIKE_LIMIT
APPLY_REASK_MAX = 1       # times apply_answer re-asks a reply that reached it naming none of the options
CHOICE_OPTIONS_MIN = 2    # a multiple-choice escalation offers 2-4 options, one recommended (architect-next's bounds)
CHOICE_OPTIONS_MAX = 4
OWNER_POLL_MAX = 2        # ask_owner tool budget: wait 30 / poll 25 minutes
OWNER_WAIT_MINUTES = 30
OWNER_POLL_MINUTES = 25
CI_FLAKE_RERUNS = 2       # section 10.12: max 2 re-runs per check per PR
CI_POLL_SLEEP = 60        # pr-watch/scripts/poll_pr.py SLEEP_SECS
CI_POLL_FAILS = 3         # pr-watch/scripts/poll_pr.py MAX_CONSECUTIVE_FAILS
CI_WALL_SECS = 14400      # coder/graph.yaml settings.timeout (hardware-bound wall clock)
GH_CALL_SECS = 90         # one gh subprocess call; node timeouts budget their sleep plus this per call
CI_LOG_RUNS_MAX = 3       # distinct CI runs whose failed log one ci_route visit fetches, sized with its node timeout
REVIEW_POLL_SLEEP = 600   # review comments arrive on a human timescale, so the watch reads every ten minutes
REVIEW_MAX_POLLS = 80     # per run; stays below settings.max_loop_iterations (100), which caps visits per node
REVIEW_WALL_SECS = REVIEW_MAX_POLLS * REVIEW_POLL_SLEEP  # the poll budget expressed as wall clock
REVIEW_MAX_ROUNDS = STRIKE_LIMIT  # review fix rounds per run, the 3-strike family bound
REVIEW_CI_POLL_BUDGET = 60  # a fix round starts only while ci_polls_run is below this, leaving the next CI watch headroom under the per-node cap
REVIEW_CI_POLL_CEILING = 90  # review mode stops the CI watch here, below the per-node visit cap of 100
RESOLVE_DEADLINE_SECS = 480  # below the pr_review_resolve node timeout of 600 with one GH_CALL_SECS call of margin
REVIEW_PERM_SECS = 120  # per-poll wall-clock budget for uncached permission reads; inside the pr_review_poll node timeout
TAIL_CHARS = 6000         # trim_output class: evidence tails handed to llm nodes
DISCUSSION_PARKS_MAX = 3  # times one escalation may park the run for a discussion (architect-next's bound)
DRAFT_ROUNDS_MAX = STRIKE_LIMIT  # redrafts of an LLM-drafted brief before the run falls back to the message as is

# Fork constants (the ONLY script-level fork deltas; design doc section 7).
OWNER_REMOTE_INPUT = False  # upstream: no Slack owner channel; ask_dispatch never routes to ask_owner_poll
PROBE_HOUSE_NOTE = ""  # upstream: no house suite convention; the probe discovers suites

HARD_TRIGGER_RE = re.compile(
    r"secret|credential|password|token|money|payment|refund|charge|destructive|irreversible|"
    r"force-push|drop (table|column|data)|delete .*branch|rm -rf",
    re.I,
)
RESUME_RE = re.compile(r"Sisyphus run id:\s*([0-9]{8}T[0-9]{6}-[0-9a-f]{8})")
CONTINUE_RE = re.compile(r"(?is)^(?:continue|resume)(?:\s*:\s*(.+)|\W*)$")
DELEGATED_RE = re.compile(r"## SOURCE OF TRUTH|\bTASK-\d{3}\b|Teammate roster", re.I)
QUALITY_RE = re.compile(r"Quality bar:\s*rigor=([A-Za-z]+)(?:[,;]?\s*surfaces=([^\n]*))?")

# A bare go-ahead: a short message made only of these words, at least one of them a go word. Anything else
# (a file, a path, a verb naming the work) is a message that names its own work.
GO_AHEAD_WORDS_MAX = 12
GO_WORDS = frozenset((
    "go", "proceed", "start", "started", "begin", "do", "run", "execute", "implement", "build", "ship", "kick",
    "lgtm", "approved", "send",
))
GO_AHEAD_FILLER = frozenset((
    "ok", "okay", "k", "yes", "yeah", "yep", "yup", "sure", "alright", "right", "great", "cool", "good", "sounds",
    "perfect", "fine", "please", "pls", "and", "then", "now", "so", "get", "it", "that", "this", "let", "lets",
    "let's", "us", "with", "the", "a", "on", "ahead", "off", "away", "thanks", "thank", "you", "work",
    "handoff", "brief", "all", "set", "we", "can", "i", "think", "is", "ready", "for",
))

# The graph variables; a run restored from a discussion park keeps the live values of these.
RUN_VARIABLES = ("project_dir", "observability_agent", "parallel", "auto_confirm", "escalation_policy", "build_lock",
                 "review_cadence", "pr_tracking")


def bare_go_ahead(text):
    """True for an empty message or a short go-ahead that names no work ("okay, go ahead and get started")."""
    text = (text or "").replace("\u2019", "'").replace("\u2018", "'")
    words = re.findall(r"[a-z]+(?:'[a-z]+)?", text.lower())
    if not words:
        return not re.search(r"\S", text)
    return (len(words) <= GO_AHEAD_WORDS_MAX and any(w in GO_WORDS for w in words)
            and all(w in GO_WORDS or w in GO_AHEAD_FILLER for w in words))


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
    already exists, since os.open applies the mode only on creation. The
    descriptor is binary and the file object writes bare newlines, so no layer
    translates line endings on Windows."""
    fd = os.open(path, flags | getattr(os, "O_BINARY", 0), 0o600)
    try:
        os.chmod(path, 0o600)
    except OSError:
        pass
    try:
        return os.fdopen(fd, "a" if flags & os.O_APPEND else "w", encoding="utf-8", newline="\n")
    except BaseException:
        os.close(fd)
        raise


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
    updates["apply_reasks"] = 0
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


_OPTION_LEAD_RE = re.compile(r"(?is)^[\s(\[*_`]*(?:option\s*)?(\d+)(\s*\(recommended\))?([)\]*_`]*)\s*(.*)$")
_OPTION_SEPARATORS = "-:).\u2014\u2013"
# an option number standing alone in the free text: not part of a fraction, a decimal or a word
_OPTION_NUMBER_RE = re.compile(r"(?<![\w/.])(\d+)(?![\w/]|\.\d)")


def _names_another_option(rest, picked, count):
    """Whether the free text after a picked number reads as naming a different option ("no wait, 3", "or 2", "1"):
    an in-range number other than the pick, after "option" or within the first two words."""
    for m in _OPTION_NUMBER_RE.finditer(rest):
        n = int(m.group(1))
        if n == picked or not 1 <= n <= count:
            continue
        before = rest[:m.start()]
        if re.search(r"(?i)\boption\s*$", before) or len(re.findall(r"[^\W\d_]+", before)) <= 2:
            return True
    return False


def split_option_reply(text, options):
    """(option text, free text after it) for a reply led by an in-range option number; (None, text) otherwise.

    The number may be wrapped ("(2)", "**2**", "[2]"), labelled ("Option 2", the rendered "Option 1 (recommended)")
    and followed by a separator ("2: x", "2) x", "2 - x", an em or en dash). Free text that is only punctuation, or
    only the picked option's own text echoed back, counts as none. A reply whose free text names a different option
    number ("3) or 2", "2. 1", "1 - no wait, 3") names no single option, so it is (None, text) too."""
    text = (text or "").strip()
    options = options or []
    m = _OPTION_LEAD_RE.match(text)
    if not m or not 1 <= int(m.group(1)) <= len(options):
        return None, text
    picked, rest = int(m.group(1)), m.group(4)
    if rest[:1] and rest[0] in _OPTION_SEPARATORS:
        rest = rest[1:]
    elif rest and not (m.group(2) or m.group(3)):
        # a bare number running into prose ("2 or 3", "2 because") is not a pick
        return None, text
    rest = rest.strip()
    if _names_another_option(rest, picked, len(options)):
        return None, text
    own = options[picked - 1]["text"]
    if re.sub(r"[\W_]+$", "", rest).lower() == re.sub(r"[\W_]+$", "", own.strip()).lower():
        rest = ""
    return own, rest if re.search(r"\w", rest) else ""


def match_option(text, options):
    """Map a user reply to an option text: a 1-based number (free text after it is dropped), exact, a prefix that
    ends on a word boundary and fits one option only, or 'recommended'."""
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
    prefixed = [o["text"] for o in options if low and _word_prefix(low, o["text"].strip().lower())]
    if len(prefixed) == 1:
        return prefixed[0]
    if low in ("recommended", "the recommended option", "go with the recommendation"):
        for o in options:
            if o.get("recommended"):
                return o["text"]
    return None


def _word_prefix(low, option_low):
    """Whether `low` starts `option_low` and stops at a word boundary ("abort" for "abort the run", never "a")."""
    if not option_low.startswith(low):
        return False
    nxt = option_low[len(low):len(low) + 1]
    return not nxt or not (nxt.isalnum() and low[-1].isalnum())


# ---------------------------------------------------------------- handoff frontmatter
def read_file(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return ""


def frontmatter(text):
    """Flat `key: value` pairs from a leading --- block, values unquoted, and the body after it."""
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


def set_frontmatter(path, pairs):
    """Set or insert flat frontmatter keys in one atomic rewrite; False (nothing written) when the read fails,
    the file is empty or it has no frontmatter block closed by a whole `---` line. Line endings are kept."""
    try:
        with open(path, encoding="utf-8", newline="") as f:
            text = f.read()
    except (OSError, UnicodeDecodeError):
        return False
    lines = text.splitlines(keepends=True)
    if not lines or lines[0].rstrip("\r\n") != "---":
        return False
    close = next((i for i in range(1, len(lines)) if lines[i].rstrip("\r\n") == "---"), None)
    if close is None:
        return False
    eol = "\r\n" if lines[0].endswith("\r\n") else "\n"
    head = lines[1:close]
    for key, value in pairs.items():
        row = f"{key}: {value}{eol}"
        at = next((i for i, l in enumerate(head) if re.match(rf"{re.escape(key)}:", l)), None)
        if at is None:
            head.append(row)
        else:
            head[at] = row
    out = "".join([lines[0], *head, *lines[close:]])
    tmp = f"{path}.tmp-{os.getpid()}"
    try:
        with open(tmp, "w", encoding="utf-8", newline="") as f:
            f.write(out)
        os.replace(tmp, path)
    except OSError:
        try:
            os.remove(tmp)
        except OSError:
            pass
        return False
    return True


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
