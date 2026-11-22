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
import shlex
import subprocess
import sys

# ---------------------------------------------------------------- constants
STRIKE_LIMIT = 3          # Phase 7 3-strike rule; also the family bound (B1, B7, B10, B14)
RERUN_ONCE = 1            # adversary/security/probe/debris/debt/obs re-run-once family
SUBTASK_MAX = 6           # A11: in-run subtasks one run takes on; more is the architect's, asked (sizing verdict)
CLARIFY_MAX = STRIKE_LIMIT
REASK_MAX = STRIKE_LIMIT
NAVIGATOR_REASK_MAX = RERUN_ONCE  # navigator words the reply interpreter cannot read go back to the navigator once
CHOICE_OPTIONS_MIN = 2    # a multiple-choice escalation offers 2-4 options, one recommended (architect-next's bounds)
CHOICE_OPTIONS_MAX = 4
OWNER_POLL_MAX = 2        # ask_owner tool budget: wait 30 / poll 25 minutes
OWNER_WAIT_MINUTES = 30
OWNER_POLL_MINUTES = 25
CI_FLAKE_RERUNS = 2       # section 10.12: max 2 re-runs per check per PR
CI_POLL_SLEEP = 60        # pr-watch/scripts/poll_pr.py SLEEP_SECS
CI_POLL_BACKOFF = (10, 20, 40)  # sleeps before the first re-polls of a watch; later re-polls sleep CI_POLL_SLEEP
CI_POLL_FAILS = 3         # pr-watch/scripts/poll_pr.py MAX_CONSECUTIVE_FAILS
CI_WALL_SECS = 14400      # coder/graph.yaml settings.timeout (hardware-bound wall clock)
CI_MAX_ROUNDS = CI_WALL_SECS // CI_POLL_SLEEP  # ci_poll visits per watch (240), a bound beside the elapsed-time limit
CI_NO_CHECKS_SECS = 300   # an empty check list must last this long into a watch before it reads no_checks
# One initial watch, a re-watch after each of the STRIKE_LIMIT CI fix rounds and one watch of headroom (1200);
# counted over every ci_poll visit of the run and kept below ENGINE_VISIT_CAP so the watch stops by name.
POLL_VISIT_BUDGET = (1 + STRIKE_LIMIT + 1) * CI_MAX_ROUNDS
ENGINE_VISIT_CAP = 1500   # mirrors graph.yaml settings.max_loop_iterations, the engine's per-node visit cap
CI_PASS = ("pass", "success")
CI_FAIL = ("fail", "failure", "error", "timed_out", "startup_failure", "action_required")
CI_PENDING = ("pending", "queued", "in_progress", "expected", "waiting", "requested")
CI_CANCELLED = ("cancel", "cancelled")
CI_QUIET = ("skipping", "skipped", "neutral")
GH_CALL_SECS = 90         # one gh subprocess call; node timeouts budget their sleep plus this per call
BASE_FETCH_SECS = 60      # the one fetch of the PR's base branch before the run reports success
BASE_GIT_SECS = 10        # each read-only local git call of the base check; the node budget allows 15 of them
BASE_SYNC_MAX = 2         # base integrations per run the base_moved question recommends before it asks for a reason
CI_LOG_RUNS_MAX = 3       # distinct CI runs whose failed log one ci_route visit fetches, sized with its node timeout
REVIEW_POLL_SLEEP = 600   # review comments arrive on a human timescale, so the watch reads every ten minutes
REVIEW_MAX_POLLS = 80     # per run; stays below ENGINE_VISIT_CAP, which caps visits per node
REVIEW_WALL_SECS = REVIEW_MAX_POLLS * REVIEW_POLL_SLEEP  # the poll budget expressed as wall clock
REVIEW_MAX_ROUNDS = STRIKE_LIMIT  # review fix rounds per run, the 3-strike family bound
FIX_ROUND_CEILING = STRIKE_LIMIT  # extra fix rounds per subtask the stuck / exhausted / strikeout questions recommend
REVIEW_CI_POLL_BUDGET = 60  # a review fix round starts only while ci_polls_run is below this, leaving the next CI watch headroom
REVIEW_CI_POLL_CEILING = 90  # once the review watch has started, ci_poll stops here instead of at POLL_VISIT_BUDGET
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
    "parallel_mode", "intent", "sizing", "subtasks", "subtask_index", "task_number", "final_review", "current_task",
    "fix_strikes", "fix_round", "review_rounds", "round_heads", "gauntlet_verdict",
    "gauntlet_lanes", "rejected_findings", "deferred_findings", "skipped_reviews",
    "debt_left_behind", "debt_scan_failures", "debris_exemptions", "design_decisions", "autonomous_digest",
    "decisions", "follow_ups", "parked", "escalation", "coder_result", "git_report",
    "ci_status", "ci_rerun_ids", "anomalies_log",
    "pipeline_size", "pipeline_size_reason", "size_escalations", "full_gauntlet_due", "probe_tests", "probe_adoption",
    "base_syncs", "base_integrated", "base_checked_head",
    "attention_notes", "autonomous_decisions", "judge_rulings", "answered_decisions",
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


def _own_text(question, quoted):
    """The question with the data it quotes blanked out; longest first so a quoted string inside another goes too."""
    text = question or ""
    for q in sorted({str(q) for q in quoted if q not in (None, "")}, key=len, reverse=True):
        text = text.replace(q, " ")
    return text


def escalation(kind, question, options=None, evidence="", hard_trigger=False, tier3_ok=True, data=None, quoted=()):
    return {
        "kind": kind,
        "question": question,
        "options": options or [],
        "evidence": evidence or "",
        "hard_trigger": bool(hard_trigger) or bool(HARD_TRIGGER_RE.search(_own_text(question, quoted))),
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
    updates["reply_source"] = ""
    updates["answer_source"] = ""
    updates["navigator_reasks"] = 0
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


# The reply reader from _OPTION_LEAD_RE through split_option_reply is architect-next's too, byte for byte; a test in
# tests/candidate_architect_next.rs fails when the two copies differ.
_OPTION_LEAD_RE = re.compile(r"(?is)^[\s(\[*_`#]*(?:option\s*)?(\d{1,9})(\s*\(recommended\))?([)\]*_`]*)\s*(.*)$")
_OPTION_SEPARATORS = "-:).\u2014\u2013"
# an option number standing alone in the free text: not part of a fraction, a decimal or a word. Both patterns read
# at most nine digits, so int() never meets a number past Python's digit limit.
_OPTION_NUMBER_RE = re.compile(r"(?<![\w/.])(\d{1,9})(?![\w/]|\.\d)")
# a number that is the operand of a comparison ("seats < 1", "count >= 2") is a value, never an option reference;
# an arrow ("1 -> 3", "=> 3") compares nothing
_COMPARED_RE = re.compile(r"(?:(?<![-=])[<>]|[=\u2264\u2265\u2260])\s*$")


def _names_another_option(rest, picked, count, strict=False):
    """Whether the free text after a picked number reads as naming a different option ("no wait, 3", "or 2", "1"):
    an in-range number other than the pick, after "option" or within the first two words (anywhere when `strict`).
    A number compared against ("seats < 1") names no option."""
    for m in _OPTION_NUMBER_RE.finditer(rest):
        n = int(m.group(1))
        if n == picked or not 1 <= n <= count:
            continue
        before = rest[:m.start()]
        if _COMPARED_RE.search(before):
            continue
        if strict or re.search(r"(?i)\boption\s*$", before) or len(re.findall(r"[^\W\d_]+", before)) <= 2:
            return True
    return False


def _echoes_option(rest, own):
    """Whether free text only repeats the picked option's own text, as rendered ("<text> (recommended)" too)."""
    def bare(s):
        return re.sub(r"[\W_]+$", "", re.sub(r"(?i)\s*\(recommended\)", "", s.strip())).lower()
    return bool(rest) and bare(rest) == bare(own)


def _unwrap_gloss(rest):
    """The text of a parenthesised gloss leading `rest` ("(keep it as planned) and log it" -> "keep it as planned and
    log it"), or None when `rest` opens no balanced parenthesis."""
    if not rest.startswith("("):
        return None
    depth = 0
    for i, ch in enumerate(rest):
        depth += {"(": 1, ")": -1}.get(ch, 0)
        if depth == 0:
            return " ".join(p for p in (rest[1:i].strip(), rest[i + 1:].strip()) if p)
    return None


def split_option_reply(text, options, strict=False):
    """(option text, free text after it) for a reply led by an in-range option number; (None, text) otherwise.

    The number may be wrapped ("(2)", "**2**", "[2]", "#2"), labelled ("Option 2", the rendered
    "Option 1 (recommended)") and followed by a separator ("2: x", "2) x", "2 - x", "Option 2, but x", an em or en
    dash) or a parenthesised gloss ("1 (as planned; log it)"). Free text that is only punctuation, or only the picked
    option's own text echoed back, counts as none. A reply whose free text names a different option number
    ("3) or 2", "2. 1", "1 - no wait, 3") names no single option, so it is (None, text) too; `strict` counts a
    different option number anywhere in the free text, fractions, decimals and compared values aside."""
    text = (text or "").strip()
    options = options or []
    m = _OPTION_LEAD_RE.match(text)
    if not m or not 1 <= int(m.group(1)) <= len(options):
        return None, text
    picked, rest = int(m.group(1)), m.group(4)
    if rest[:2] in ("->", "=>"):
        rest = rest[2:]
    elif rest[:1] and (rest[0] in _OPTION_SEPARATORS or (rest[0] == "," and not rest[1:2].isdigit())):
        rest = rest[1:]
    elif (gloss := _unwrap_gloss(rest)) is not None:
        rest = gloss
    elif rest and not (m.group(2) or m.group(3)):
        # a bare number running into prose ("2 or 3", "2 because") is not a pick
        return None, text
    rest = rest.strip()
    own = options[picked - 1]["text"]
    if _echoes_option(rest, own):
        return own, ""
    if _names_another_option(rest, picked, len(options), strict):
        return None, text
    return own, rest if re.search(r"\w", rest) else ""


_LED_SEPARATORS = ":;,.-\u2014\u2013"


def _led_option(text, options):
    """(1-based pick, option text, free text) when exactly one option's own text, its trailing parenthesised prompt
    ("(say how)") aside, opens `text` and is followed by a separator or the end; None otherwise."""
    low = text.lower()
    led = []
    for i, o in enumerate(options, 1):
        own = (o.get("text") or "").strip()
        core = re.sub(r"\s*\([^()]*\)\s*$", "", re.sub(r"(?i)\s*\(recommended\)\s*$", "", own)).rstrip(_LED_SEPARATORS + " ")
        nxt = low[len(core):len(core) + 1]
        if core and low.startswith(core.lower()) and (not nxt or nxt in _LED_SEPARATORS or nxt == "("):
            led.append((i, own, text[len(core):]))
    if len(led) != 1:
        return None
    picked, own, rest = led[0]
    rest = (_unwrap_gloss(rest.strip()) if rest.strip().startswith("(") else None) or rest.lstrip(_LED_SEPARATORS + " ")
    return picked, own, rest.strip()


def split_option_led(text, options, strict=False):
    """(option text, free text after it) for a reply that opens with one option's own text ("Narrow the scope first:
    only the 4 code files"); (None, text) when no option, or more than one, leads it, or the free text names another
    option number (see split_option_reply for `strict`)."""
    text = (text or "").strip()
    options = options or []
    found = _led_option(text, options)
    if not found:
        return None, text
    picked, own, rest = found
    if _echoes_option(rest, own) or not re.search(r"\w", rest):
        return own, ""
    if _names_another_option(rest, picked, len(options), strict):
        return None, text
    return own, rest


def names_several_options(text, options):
    """Whether a reply opening with one option, by number or by its own text, names a different option number
    anywhere after it ("1 - no wait, 3", "2: then option 3"); an echo of the picked option's text names no other."""
    text = (text or "").strip()
    options = options or []
    m = _OPTION_LEAD_RE.match(text)
    if m and 1 <= int(m.group(1)) <= len(options) and not split_option_reply(text, options, True)[0]:
        return _names_another_option(m.group(4), int(m.group(1)), len(options), True)
    found = _led_option(text, options)
    return bool(found) and not split_option_led(text, options, True)[0]


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
def git(cwd, *args, timeout=60, env=None):
    try:
        p = subprocess.run(
            ["git", "-C", cwd, *args], capture_output=True, text=True, timeout=timeout,
            env={**os.environ, **env} if env else None,
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


# ---------------------------------------------------------------- criterion commands
GIT_READ_ONLY = ("diff", "status", "log", "show", "rev-parse", "ls-files", "merge-base", "cat-file")
GIT_UNSAFE_ARGS = ("-O", "--open-files-in-pager", "--ext-diff", "--textconv", "--output")
SHELL_META_RE = re.compile(r"[;|&$<>(){}`*?\n'\"~!]")
ENV_ASSIGN_RE = re.compile(r"^[A-Za-z_]\w*=")
CRITERION_PASS_RE = re.compile(r"\b(pass(es|ing)?|succeed(s|ed)?|green|exits?\s+(with\s+)?(code\s+|status\s+)?0)\b", re.I)
CRITERION_FAIL_RE = re.compile(r"\b(fail(s|ed)?|exits?\s+(with\s+)?(code\s+|status\s+)?[1-9]|non-?zero|rejected|refuse[sd]?)\b", re.I)
EXIT_CODE_FLAGS = ("--exit-code", "--quiet")


def _command_args(span):
    try:
        toks = shlex.split(span)
    except ValueError:
        return []
    return toks[next((i for i, t in enumerate(toks) if not ENV_ASSIGN_RE.match(t)), len(toks)):]


def _in_tree_executable(prog, cwd):
    if not cwd:
        return False
    root = os.path.realpath(cwd)
    path = os.path.realpath(os.path.join(root, prog))
    return os.path.commonpath([root, path]) == root and os.path.isfile(path) and os.access(path, os.X_OK)


def criterion_commands(criteria_text, cwd, declared, cap=8):
    """(kept, dropped): the backticked spans of acceptance criteria whose exit status is the criterion's verdict.

    A span counts when it has a program and at least one argument, no leading NAME=value assignment, no shell
    metacharacters, quotes, ~ or !, and a program that is either the program of a declared verification command,
    a path holding a slash that resolves to an executable file inside cwd, or git with a read-only subcommand
    (diff, status, log, show, rev-parse, ls-files, merge-base, cat-file) and no -O, --open-files-in-pager,
    --ext-diff, --textconv or --output argument. Any git span must meet the git rule, even when git is declared.
    The span must also carry --exit-code or --quiet, or sit on a criterion line that states a passing exit and no
    failing one, because a span whose exit does not decide the criterion would report a false green or red.
    Assignments are refused because the span runs through a shell, where GIT_EXTERNAL_DIFF, GIT_CONFIG_*, PATH or
    LD_PRELOAD would turn an allowed program into an arbitrary one.
    kept is deduplicated and at most cap long; dropped counts the eligible spans the cap left out.
    """
    programs = {a[0] for a in map(_command_args, declared or []) if a}
    out, seen, dropped = [], set(), 0
    for line in (criteria_text or "").splitlines():
        prose = re.sub(r"`[^`]*`", " ", line)
        line_passes = bool(CRITERION_PASS_RE.search(prose)) and not CRITERION_FAIL_RE.search(prose)
        for span in re.findall(r"`([^`]+)`", line):
            span = span.strip()
            if span in seen or SHELL_META_RE.search(span):
                continue
            args = _command_args(span)
            if ENV_ASSIGN_RE.match(span) or len(args) < 2 or not (line_passes or any(a in EXIT_CODE_FLAGS for a in args)):
                continue
            prog = args[0]
            if prog == "git":
                if args[1] not in GIT_READ_ONLY or any(a.startswith(GIT_UNSAFE_ARGS) for a in args):
                    continue
            elif prog not in programs and not ("/" in prog and _in_tree_executable(prog, cwd)):
                continue
            seen.add(span)
            if len(out) >= cap:
                dropped += 1
                continue
            out.append(span)
    return out, dropped
