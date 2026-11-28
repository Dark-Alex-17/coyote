#!/usr/bin/env python3
"""Deterministic execution-evidence recorder for the adversary graph.

Runs ONLY the caller-declared verification commands (verification_commands)
in the project dir and records the outcomes as `exec_results` — a list of
{cmd, exit, duration_s, tail} records. Each run record also carries
`pass_condition` from pass_condition.py: whether the criteria and the request
hold the command to exit 0 or state some other condition for it. Criterion
branches cite this record; they never execute anything themselves.

NO auto-detection: undeclared commands mean a "none declared" marker
(fail-visible, not fail-guessed). Any runner error degrades into an
ENVIRONMENT marker — this script never fails the node.

Trust boundary: `verification_commands` is a DECLARED graph variable (or the
review-gauntlet's structured `inputs:` passthrough), never a field an LLM
extracts from the spawn prompt — that prose also carries plan/diff text pasted
from the repo under review, and whatever is declared here runs with
shell=True. Variables land in state as strings, so a JSON-encoded list is
accepted alongside a real list. A declaration that is not a JSON array of
strings is a caller contract violation, not a runner hiccup: it executes
NOTHING and fails closed as a PIPELINE-FAULT (complaint #1) — it never
degrades to an ENVIRONMENT marker. The engine merges only the keys an
llm node's `output_schema.properties` declares, so a parse LLM coaxed into
emitting an extra `verification_commands` key cannot overwrite the declared
value: the undeclared key is dropped before it reaches state.

Budget: commands run sequentially, so TOTAL runtime is bounded by a deadline
(TOTAL_DEADLINE_SECS = 3120s). Once the review-stack lock is acquired or its
bounded wait (STACK_LOCK_WAIT_SECS = 1200s) gives up, the pre-run tree
capture (below) gets SNAPSHOT_GIT_TIMEOUT_SECS = 120s of its own, and the
deadline starts when it ends, so the capture never takes time from the
declared commands. The run_checks node's 4600s timeout covers 1200 + 120 +
3120 plus margin for the baseline worktree cleanup and for the carried-failure
matching below, which starts a record only while the deadline has time left
and gives each of its two git calls at least GATE_CALL_MIN_TIMEOUT_SECS.
Without the deadline, a
handful of hanging commands (4 x 900s) would blow the NODE timeout and the
executor would kill this script from OUTSIDE -- the
in-script degradation would never fire. Each command gets
min(PER_COMMAND_TIMEOUT_SECS, remaining budget); commands the deadline
leaves no budget for are recorded as {"cmd", "skipped": "deadline"} entries,
so the in-script handling stays authoritative; the verdict reports them as
not run, not as red. The env vars ADVERSARY_RUN_CHECKS_DEADLINE_SECS and
ADVERSARY_STACK_LOCK_WAIT_SECS override the deadline and the lock wait (test
seams), and ADVERSARY_STACK_LOCK_PATH overrides the review-stack lock file
location (isolation seam, see acquire_stack_lock).

Baseline: every red record (nonzero exit, including the -1 timeout) is run
again, inside the same deadline, in one temporary detached git worktree of
the project's repository at `diff_base` (from diff_facts.py), in the same
subdirectory project_dir names, added with core.hooksPath pointed at an
empty directory so the user's git hooks do not run. The worktree is removed
on normal exit. A record that timed out here gets a not-run baseline without
a base run, so it does not spend the shared deadline.
A base run that completed (neither timed out nor killed by a signal) is saved
with its full output and exit code as `adversary-<key>.json` under
`git rev-parse --git-path coyote-baselines`, keyed by the diff base's tree,
the subdirectory, the command, the lock prefix and the per-command timeout.
A later run with the same key compares against the saved output instead of
running the command again, and the worktree is only added when some red
command has no saved run. An unreadable record counts as absent.
The two full outputs are compared with the coder agent's own gate_lib.sh
helpers through bash, by the rule its verify_tests gate applies: the failure
is pre-existing only when the base run is red (and did not time out) with
the same exit code, the current run was not killed by a signal, and either
every currently failing test id already failed on the base, or neither run
named a test id and the failure output hashes alike. The red record gains
`baseline`: {"status": "pre-existing" | "regression" | "not-run" |
"tree-state", "exit", "note"}. not-run means no comparison happened (no diff
base, deadline spent, worktree or helper failure, base run timed out); it
blocks like a regression, so a missing comparison never passes.
Tree state: after the review-stack lock and before the first declared command
runs, `git status --porcelain` is read. When it lists anything, a snapshot
commit of that pre-run tree is built: HEAD plus every tracked and untracked
file that is not ignored, through a temporary GIT_INDEX_FILE, write-tree and
commit-tree, so the real index and refs are never touched. This writes
unreferenced objects (among them the blobs of untracked, non-ignored files)
into the repository's object database, where git's own gc collects them. When
`git status` fails, no snapshot is taken and every red record's note gains
", candidate tree state unknown". When `core.sparseCheckout` is true (a sparse
index cannot be snapshotted whole), a dirty tree takes no snapshot and every
red record's note gains ", candidate tree dirty". After the declared commands run, a dirty
tree is written the same way into a second temporary index and its tree id is
compared with the pre-run one; a clean one only has `git status` read again.
When that second write fails, each note gains ", candidate tree dirty, whether
the declared commands modified it is unknown" and nothing is rerun.
When the tree changed, the declared commands modified it (a regenerate-then-diff
command, say), no red record gets "tree-state", and each note gains ", the
declared commands modified the tree". When HEAD is not the diff base (both
resolved with `git rev-parse --verify <rev>^{commit}`; any failure counts as
not), the uncommitted tree is not the whole change, so nothing is rerun and
each note gains ", candidate tree dirty beside commits since the diff base"
instead. A run that commits some work and leaves the rest uncommitted thus
gets no tree-state release: a false block, never a false pass, so commit
before review or review the worktree spec. Otherwise, on a dirty tree, every red
record held to exit 0 that neither timed out nor was killed by a signal (exit
128 or above) runs once more, inside the same deadline and in
the same subdirectory, in a second detached worktree (hooks disabled the same
way) of the pre-run snapshot commit. A command that exits 0 there gets
baseline status "tree-state", a null exit and no base run: it is red only in
the uncommitted live tree. Any other snapshot outcome (red, timed out, no
snapshot, deadline spent) keeps the live run's classification against the
base, with ", candidate tree dirty" appended to its note; the snapshot's own
output is never compared with the base, since ignored files missing from both
clean checkouts would fake a match. Snapshot runs are never saved. Each
snapshot git call after the declared commands is capped at
SNAPSHOT_GIT_TIMEOUT_SECS and by the deadline. Both worktrees are removed
together.

Process hygiene: commands run through the shell. On POSIX each command gets
its own session (start_new_session) and a timeout SIGKILLs the whole process
group, so grandchildren of a hung command cannot linger. The post-kill
reap/drain is itself bounded by DRAIN_TIMEOUT_SECS: a grandchild that escaped
the session (its own start_new_session) but still holds the stdout pipe
would otherwise block the drain until it exits. On non-POSIX platforms only
the direct shell child is killed — grandchildren of a timed-out command may
survive (documented limitation).

Verification commands run with GRAPH_STATE* (GRAPH_STATE, GRAPH_STATE_FILE)
scrubbed from their environment: the reviewed repo's own tests may exec these very
scripts with their own GRAPH_STATE, and load_state() prefers
GRAPH_STATE_FILE, so an inherited live state file would silently win.

Lock prefix: the `build_lock` graph variable (read from state only, never
from the environment: a harness that itself runs under the lock would
otherwise nest a second one) is a command prefix such as `flock -w 240 /path/to/lock`.
When non-empty each declared command runs as
`<prefix> bash -o pipefail -c '<cmd>'`, the whole command as one unit under
the prefix, and its record gains an `under` field naming the prefix. The
record's `cmd` stays the declared command. pipefail applies only under a
prefix (the bare arm is plain shell, Windows-compatible); the prefixed form
is POSIX-only. A present, non-string value is a
declaration error and fails closed the same way as a malformed
`verification_commands`: PIPELINE-FAULT, nothing executed.

Carried failures: `carried_failures` is a declared variable on the same trust
boundary, a JSON array of {"paths", "owner", "evidence"} items naming
failures the caller declares expected to stay red in this change and owed
elsewhere. A value that is not a JSON array is treated as none and recorded
as `carried_error`, never a PIPELINE-FAULT; an item without a non-empty list
of string paths is dropped and counted there. With a non-empty list, every
red record that ran to a nonzero exit below 128, is held to exit 0 and whose
baseline is neither pre-existing nor tree-state has its full output parsed
by the shared `.shared/failure_locations.py` against the files git lists in
the repository, untracked included. The record gains `carried`: {"all",
"matched", "new", "unlocated"}. `all` is true only when the output was not
trimmed, is under CARRIED_OUTPUT_CAP bytes, locates at least one failure,
has no failure line naming no repo path, and every located failure's paths
exactly equal a carried item's; a new failure inside a carried directory is
new. Matching is per location, so a further failure at a carried location is
not told apart; the owner's own gate still has to see it green. When the
parser cannot be loaded or git cannot list the files, the
record gets a `carried_note` instead and is judged as if nothing were
carried; so does every record the deadline leaves no time to match.

Verification tier: `verification_tier` is '' (the default), `round` or
`final`; any other value executes nothing and records a PIPELINE-FAULT. A
non-empty tier adds `tier_note`, the facts the criterion checks and the
holistic pass are given about it. At `final` the finalize record of the
reviewed tree is read beside the declared commands, which still run (the
caller declares only what the finalize gate does not cover, such as a
run-wide lint), and its runs come first in `exec_results`. With
`review_tree` set, that is the record for exactly that tree
(`verify_record.lookup_exact`) and no `debris_exclude` match applies; with it
empty, the fallback is the worktree tree as it stands (`verify_record.lookup`,
a tree differing only under `debris_exclude` matching too). There is one
run per record result that ran, each with `source:
"finalize record <tree> @ <recorded_at>"`, its pre-existing failures as a
pre-existing baseline and its other reds as a regression (or not-run when
the gate could not classify them). `full_suite_record` gets {status:
green|red|missing, tree, line}; verdict.py forces DIVERGES on its
`FULL-SUITE RECORD MISSING|RED: <detail>` line unless it is green. Carried
failures are matched only against the declared runs at this tier: the
record keeps only output tails.
"""

import hashlib
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from pass_condition import EXIT_0, resolve  # noqa: E402

try:
    import fcntl
except ImportError:  # non-POSIX (Windows): advisory flock unavailable
    fcntl = None

PER_COMMAND_TIMEOUT_SECS = 900
TOTAL_DEADLINE_SECS = float(os.environ.get("ADVERSARY_RUN_CHECKS_DEADLINE_SECS") or 3120)
STACK_LOCK_PATH = os.environ.get("ADVERSARY_STACK_LOCK_PATH") or os.path.expanduser(
    "~/.cache/coyote/review-stack.lock"
)
STACK_LOCK_WAIT_SECS = float(os.environ.get("ADVERSARY_STACK_LOCK_WAIT_SECS") or 1200)
GATE_LIB = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "..", "..", "coder", "scripts", "gate_lib.sh"
)
GIT_CLEANUP_TIMEOUT_SECS = 20
SNAPSHOT_GIT_TIMEOUT_SECS = 120
GATE_CALL_MIN_TIMEOUT_SECS = 10
BASE_RUN_FORMAT = 1
SHARED_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".shared")
CARRIED_OUTPUT_CAP = 4 * 1024 * 1024
CARRIED_DEADLINE_NOTE = "the run's deadline was spent"
TREE_STATE_NOTE = (
    "candidate tree dirty: red only in the live tree; the same content on a clean checkout exits 0"
)
DIRTY_TREE_NOTE = ", candidate tree dirty"
DIRTY_BESIDE_COMMITS_NOTE = ", candidate tree dirty beside commits since the diff base"
UNKNOWN_TREE_NOTE = ", candidate tree state unknown"
UNKNOWN_CHANGE_NOTE = ", candidate tree dirty, whether the declared commands modified it is unknown"
MODIFIED_TREE_NOTE = ", the declared commands modified the tree"
FULL_SUITE_PREFIX = "FULL-SUITE RECORD "
UNCLASSIFIED_NOTE = "not classified:"
TIER_NOTES = {
    "": "",
    "round": (
        "\n\nVerification tier: round. The full test suite and lint run at the final gate, after "
        "this review, so the runs above are the only ones this review has. A criterion that only "
        "the whole suite or lint can prove (the full suite passes, lint is clean) is at best "
        "INSPECTED here, with a `no_test_reason` beginning `DEFERRED: full suite runs at the final "
        "gate`; the deterministic verdict counts it as PARTIAL unless a recorded run is green."
    ),
    "final": (
        "\n\nVerification tier: final. This review is the final gate. The runs above that name a "
        "finalize record in `source` come from the finalize record of the reviewed tree and were "
        "not run again; the others are the declared commands, run now. All are recorded runs like "
        "any other, so a green one proves a criterion it covers, cited as that run. Nothing is "
        "deferred at this tier: a `DEFERRED:` reason counts as PARTIAL."
    ),
}
SNAPSHOT_IDENTITY = {
    "GIT_AUTHOR_NAME": "adversary",
    "GIT_AUTHOR_EMAIL": "adversary@localhost.invalid",
    "GIT_COMMITTER_NAME": "adversary",
    "GIT_COMMITTER_EMAIL": "adversary@localhost.invalid",
}


def acquire_stack_lock():
    """Serialize with the probe lane's shared-stack work (advisory flock on a
    well-known path; the probe is instructed to flock the same file around
    stack boot/teardown). Bounded wait so a hogged lock can never turn into a
    false red verification: on timeout we proceed WITHOUT the lock and note it
    on stderr. The fd is held for the process lifetime (released on exit).
    On non-POSIX platforms (no fcntl) the lock is skipped entirely."""
    if fcntl is None:
        sys.stderr.write("WARN: review-stack lock unsupported on this platform; running unlocked\n")
        return None
    try:
        os.makedirs(os.path.dirname(STACK_LOCK_PATH), exist_ok=True)
        fd = os.open(STACK_LOCK_PATH, os.O_CREAT | os.O_RDWR)
    except OSError as e:
        sys.stderr.write(f"WARN: review-stack lock unavailable ({e}); running unlocked\n")
        return None
    limit = time.monotonic() + STACK_LOCK_WAIT_SECS
    while True:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return fd
        except OSError:
            if time.monotonic() >= limit:
                os.close(fd)
                sys.stderr.write(
                    "WARN: review-stack lock still held after bounded wait — proceeding "
                    "WITHOUT it; shared-stack collisions with the probe lane are possible\n"
                )
                return None
            time.sleep(5)
DRAIN_TIMEOUT_SECS = 10
TAIL_LINES = 50
MAX_TAIL_CHARS = 8000


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


class InvalidDeclaration(ValueError):
    pass


def declared_commands(declared):
    """Normalize the verification_commands declaration to a list of commands.

    Accepts a JSON list or a JSON-encoded string of one; missing/None means
    none declared. Anything else raises InvalidDeclaration.
    """
    if declared is None:
        return []
    if isinstance(declared, str):
        if not declared.strip():
            raise InvalidDeclaration("empty string (declare '[]' for none)")
        try:
            declared = json.loads(declared)
        except json.JSONDecodeError as e:
            raise InvalidDeclaration(f"not valid JSON: {e}") from e
    if not isinstance(declared, list):
        raise InvalidDeclaration(
            f"expected a JSON array of strings, got {type(declared).__name__}"
        )
    bad = [c for c in declared if not isinstance(c, str)]
    if bad:
        raise InvalidDeclaration(f"non-string item(s): {json.dumps(bad)}")
    return [c.strip() for c in declared if c.strip()]


def lock_prefix(declared):
    """Normalize the build_lock declaration to a stripped prefix string.

    Missing/None means no prefix. Anything other than a string raises
    InvalidDeclaration.
    """
    if declared is None:
        return ""
    if not isinstance(declared, str):
        raise InvalidDeclaration(
            f"expected a string, got {type(declared).__name__}"
        )
    return declared.strip()


def declared_carried(declared):
    """(carried items, error) for the carried_failures declaration.

    Missing/None or an empty string means none. A value that is not a JSON
    array is treated as none, and an item without a non-empty list of string
    paths is dropped; either is described in the error, never raised.
    """
    if declared is None or (isinstance(declared, str) and not declared.strip()):
        return [], ""
    if isinstance(declared, str):
        try:
            declared = json.loads(declared)
        except json.JSONDecodeError as e:
            return [], f"carried_failures treated as none: not valid JSON: {e}"
    if not isinstance(declared, list):
        return [], f"carried_failures treated as none: expected a JSON array, got {type(declared).__name__}"
    items = []
    for item in declared:
        paths = item.get("paths") if isinstance(item, dict) else None
        if isinstance(paths, list) and paths and all(isinstance(p, str) and p for p in paths):
            items.append({"paths": paths, "owner": item.get("owner")})
    dropped = len(declared) - len(items)
    if dropped:
        return items, (
            f"carried_failures: {dropped} malformed item(s) dropped (an item needs a non-empty list of string paths)"
        )
    return items, ""


def tail_of(text):
    tail = "\n".join(text.splitlines()[-TAIL_LINES:])
    if len(tail) > MAX_TAIL_CHARS:
        tail = "…" + tail[-MAX_TAIL_CHARS:]
    return tail


def run_one(cmd, proj, budget, prefix="", out_path=None):
    """Run one command, hard-bounded by `budget` seconds; return its record.
    A non-empty `prefix` wraps the command, which keeps its own quoting.
    With `out_path`, the full combined output is written there."""
    start = time.monotonic()
    popen_kwargs = {"start_new_session": True} if os.name == "posix" else {}
    env = {k: v for k, v in os.environ.items() if not k.startswith("GRAPH_STATE")}
    shell_cmd = f"{prefix} bash -o pipefail -c {shlex.quote(cmd)}" if prefix else cmd
    under = {"under": prefix} if prefix else {}
    p = subprocess.Popen(
        shell_cmd,
        shell=True,
        cwd=proj,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="replace",
        env=env,
        **popen_kwargs,
    )
    try:
        out_s, err_s = p.communicate(timeout=budget)
    except subprocess.TimeoutExpired:
        if os.name == "posix":
            try:
                os.killpg(os.getpgid(p.pid), signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        else:
            p.kill()
        try:
            out_s, err_s = p.communicate(timeout=DRAIN_TIMEOUT_SECS)
        except subprocess.TimeoutExpired:
            out_s = err_s = ""
        write_output(out_path, out_s, err_s)
        return {
            "cmd": cmd,
            "exit": -1,
            "duration_s": round(time.monotonic() - start, 1),
            "tail": f"TIMEOUT: no exit within {budget:.0f}s — recorded as a failing run",
            **under,
        }
    combined = write_output(out_path, out_s, err_s)
    return {
        "cmd": cmd,
        "exit": p.returncode,
        "duration_s": round(time.monotonic() - start, 1),
        "tail": tail_of(combined),
        **under,
    }


def write_output(out_path, out_s, err_s):
    combined = (out_s or "") + (("\n" + err_s) if err_s else "")
    if out_path:
        with open(out_path, "w", encoding="utf-8") as f:
            f.write(combined)
    return combined


class GateUnavailable(RuntimeError):
    pass


def gate_call(fn, path, budget):
    """Run one gate_lib.sh helper on output file `path`; return (status, stdout)."""
    if not os.path.isfile(GATE_LIB):
        raise GateUnavailable(f"{os.path.normpath(GATE_LIB)} not found")
    try:
        r = subprocess.run(
            ["bash", "-c", 'source "$1" || exit 90; "$2" "$3"', "gate", GATE_LIB, fn, path],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=max(budget, GATE_CALL_MIN_TIMEOUT_SECS),
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        raise GateUnavailable(f"{fn}: {e}") from e
    if r.returncode == 90:
        raise GateUnavailable(f"sourcing gate_lib.sh failed: {r.stderr.strip()[:200]}")
    return r.returncode, r.stdout


def gate_output(fn, path, budget):
    code, out = gate_call(fn, path, budget)
    if code != 0:
        raise GateUnavailable(f"{fn} exited {code}")
    return out


def gate_ids(path, budget):
    return {line for line in gate_output("gate_failing_tests", path, budget).splitlines() if line}


def gate_killed(path, budget):
    code, _ = gate_call("gate_killed_by_signal", path, budget)
    if code not in (0, 1):
        raise GateUnavailable(f"gate_killed_by_signal exited {code}")
    return code == 0


def listed(ids):
    shown = sorted(ids)[:5]
    more = f" (+{len(ids) - 5} more)" if len(ids) > 5 else ""
    return ", ".join(shown) + more


def classify(cur, cur_path, base, base_path, deadline):
    """Baseline object for red record `cur` against base-run record `base`."""
    base_exit, cur_exit = base["exit"], cur["exit"]

    def verdict(status, note):
        return {"status": status, "exit": base_exit, "note": note}

    if base_exit == 0:
        return verdict("regression", "the command passed on the diff base")
    if cur_exit != base_exit:
        return verdict("regression", f"exit {cur_exit} here but {base_exit} on the diff base")
    if gate_killed(cur_path, left(deadline)):
        return verdict(
            "regression", "a test target was killed by a signal, so this run cannot match the base"
        )
    base_ids, cur_ids = gate_ids(base_path, left(deadline)), gate_ids(cur_path, left(deadline))
    if base_ids:
        if cur_ids and cur_ids <= base_ids:
            return verdict("pre-existing", f"the same tests already failed on the diff base: {listed(cur_ids)}")
        if cur_ids:
            return verdict("regression", f"new failing tests: {listed(cur_ids - base_ids)}")
        reason = gate_output("gate_unidentified_reason", cur_path, left(deadline)).strip()
        return verdict("regression", f"the failures here cannot be identified ({reason}) but the base's could")
    if cur_ids:
        return verdict("regression", f"failing tests not named on the diff base: {listed(cur_ids)}")
    base_hash = gate_output("gate_failure_hash", base_path, left(deadline)).strip()
    if base_hash and gate_output("gate_failure_hash", cur_path, left(deadline)).strip() == base_hash:
        reason = gate_output("gate_unidentified_reason", cur_path, left(deadline)).strip()
        return verdict(
            "pre-existing",
            f"same exit and the same failure output on the diff base (tests unidentified: {reason})",
        )
    return verdict("regression", "the failure output differs from the diff base's")


def not_run(note):
    return {"status": "not-run", "exit": None, "note": note}


def left(deadline):
    return deadline - time.monotonic()


def git_text(args, cwd, timeout, env=None):
    r = subprocess.run(
        ["git", "-C", cwd, *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=max(timeout, 1),
        env=env,
    )
    if r.returncode != 0:
        raise RuntimeError((r.stderr or r.stdout).strip()[:300])
    return r.stdout.strip()


def cleanup_git(top, args):
    try:
        git_text(args, top, GIT_CLEANUP_TIMEOUT_SECS)
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as e:
        sys.stderr.write(f"WARN: baseline worktree cleanup `git {' '.join(args)}` failed: {e}\n")


def remove_worktree(top, path):
    """The prune after the rmtree drops a registration that a failed
    `worktree remove` left behind."""
    cleanup_git(top, ["worktree", "remove", "--force", path])
    cleanup_git(top, ["worktree", "prune"])
    shutil.rmtree(path, ignore_errors=True)
    cleanup_git(top, ["worktree", "prune"])


def add_worktree(top, path, commit, work, deadline):
    """A detached worktree of `commit` at `path`, added without running the user's git hooks."""
    hooks = os.path.join(work, "no-hooks")
    os.makedirs(hooks, exist_ok=True)
    git_text(
        ["-c", f"core.hooksPath={hooks}", "worktree", "add", "--detach", path, commit],
        top,
        left(deadline),
    )


def snapshot_budget(deadline):
    return min(SNAPSHOT_GIT_TIMEOUT_SECS, left(deadline))


def sparse_checkout(top, deadline):
    try:
        return git_text(["config", "--bool", "core.sparseCheckout"], top, snapshot_budget(deadline)) == "true"
    except RuntimeError:
        return False


def head_is_base(top, diff_base, deadline):
    """Whether HEAD is the diff base, so that the uncommitted tree is the whole change."""
    try:
        head = git_text(["rev-parse", "--verify", "HEAD^{commit}"], top, snapshot_budget(deadline))
        base = git_text(["rev-parse", "--verify", f"{diff_base}^{{commit}}"], top, snapshot_budget(deadline))
    except (OSError, RuntimeError, subprocess.TimeoutExpired):
        return False
    return bool(head) and head == base


def snapshot_tree(top, work, index, deadline):
    """The tree id of HEAD plus the live tree's tracked and untracked, non-ignored files,
    written through the temporary index file `index` under `work`."""
    env = dict(os.environ, GIT_INDEX_FILE=os.path.join(work, index))
    git_text(["read-tree", "HEAD"], top, snapshot_budget(deadline), env)
    git_text(["add", "-A"], top, snapshot_budget(deadline), env)
    return git_text(["write-tree"], top, snapshot_budget(deadline), env)


def tree_before(proj, work, deadline):
    """The live tree before any declared command runs: {"top", "dirty", "tree", "commit"}.
    "dirty" is None when the tree state cannot be read; "tree" is None when a dirty tree
    could not be snapshotted."""
    unknown = {"top": None, "dirty": None, "tree": None, "commit": None}
    try:
        top = git_text(["rev-parse", "--show-toplevel"], proj, snapshot_budget(deadline))
        sparse = sparse_checkout(top, deadline)
        status = git_text(["--no-optional-locks", "status", "--porcelain"], top, snapshot_budget(deadline))
    except (OSError, RuntimeError, subprocess.TimeoutExpired):
        return unknown
    if not status:
        return {"top": top, "dirty": False, "tree": None, "commit": None}
    if sparse:
        return {"top": top, "dirty": True, "tree": None, "commit": None}
    try:
        tree = snapshot_tree(top, work, "before-index", deadline)
        commit = git_text(
            ["commit-tree", "--no-gpg-sign", "-p", "HEAD", "-m", "adversary snapshot", tree],
            top,
            snapshot_budget(deadline),
            dict(os.environ, **SNAPSHOT_IDENTITY),
        )
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as e:
        sys.stderr.write(f"WARN: no snapshot of the dirty tree to rerun red commands in: {e}\n")
        return {"top": top, "dirty": True, "tree": None, "commit": None}
    return {"top": top, "dirty": True, "tree": tree, "commit": commit}


def tree_outcome(before, work, deadline):
    """(the pre-run snapshot commit to rerun red commands on, or None; the suffix for the
    note of every red record that does not get a tree-state baseline)."""
    if before["dirty"] is None:
        return None, UNKNOWN_TREE_NOTE
    if not before["dirty"]:
        try:
            status = git_text(["--no-optional-locks", "status", "--porcelain"], before["top"], snapshot_budget(deadline))
        except (OSError, RuntimeError, subprocess.TimeoutExpired):
            return None, UNKNOWN_TREE_NOTE
        return None, MODIFIED_TREE_NOTE if status else ""
    if before["tree"] is None:
        return None, DIRTY_TREE_NOTE
    try:
        after = snapshot_tree(before["top"], work, "after-index", deadline)
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as e:
        sys.stderr.write(f"WARN: the live tree could not be compared with its pre-run state: {e}\n")
        return None, UNKNOWN_CHANGE_NOTE
    if after != before["tree"]:
        return None, MODIFIED_TREE_NOTE + DIRTY_TREE_NOTE
    return before["commit"], DIRTY_TREE_NOTE


def rerun_on_snapshot(results, reds, top, subdir, prefix, deadline, work, commit, worktrees):
    """Run each red command, all held to exit 0, once more on the pre-run snapshot `commit`
    and give the ones that exit 0 there a tree-state baseline."""
    if left(deadline) <= 0:
        return
    tree = os.path.join(work, "snapshot")
    worktrees.append(tree)
    try:
        add_worktree(top, tree, commit, work, deadline)
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as e:
        sys.stderr.write(f"WARN: no snapshot worktree to rerun red commands in: {e}\n")
        return
    snap_proj = os.path.join(tree, subdir)
    for i in reds:
        remaining = left(deadline)
        if remaining <= 0 or not os.path.isdir(snap_proj):
            break
        if run_one(results[i]["cmd"], snap_proj, min(PER_COMMAND_TIMEOUT_SECS, remaining), prefix)["exit"] == 0:
            results[i]["baseline"] = {"status": "tree-state", "exit": None, "note": TREE_STATE_NOTE}


def held_to_exit_0(record):
    """Whether the record's pass condition is exit 0. Under a stated one ("prints nothing",
    say) an exit 0 on the snapshot can itself violate it, so it proves nothing."""
    condition = record.get("pass_condition")
    return isinstance(condition, dict) and condition.get("condition") == EXIT_0


# A saved base run freezes one sample of a flaky base: later rounds compare
# against that sample instead of drawing a new one, the same trade-off the
# coder's stored test baseline makes.
def base_run_store(proj, diff_base, subdir, prefix, deadline):
    """(records dir, base tree, subdir, prefix) for saved diff-base runs, or None when git
    cannot name them."""
    try:
        tree = git_text(["rev-parse", f"{diff_base}^{{tree}}"], proj, left(deadline))
        records = git_text(["rev-parse", "--git-path", "coyote-baselines"], proj, left(deadline))
    except (OSError, RuntimeError, subprocess.TimeoutExpired):
        return None
    if not tree or not records:
        return None
    return os.path.join(proj, records), tree, subdir, prefix


def base_run_path(saved_runs, cmd):
    records, tree, subdir, prefix = saved_runs
    parts = [BASE_RUN_FORMAT, tree, subdir, cmd, prefix, PER_COMMAND_TIMEOUT_SECS]
    key = hashlib.sha256(json.dumps(parts).encode("utf-8")).hexdigest()[:16]
    return os.path.join(records, f"adversary-{key}.json")


def load_base_run(saved_runs, cmd):
    """The saved (exit, output) of `cmd` at the diff base; None when absent or unreadable."""
    if saved_runs is None:
        return None
    try:
        with open(base_run_path(saved_runs, cmd), encoding="utf-8") as f:
            saved = json.load(f)
    except (OSError, ValueError):
        return None
    if not isinstance(saved, dict) or saved.get("version") != BASE_RUN_FORMAT:
        return None
    code, output = saved.get("exit"), saved.get("output")
    if type(code) is not int or not isinstance(output, str):
        return None
    return code, output


def save_base_run(saved_runs, cmd, base, base_path, deadline):
    """Keep a completed base run for later rounds. A timed-out or signal-killed run is
    never kept, and an existing record is never replaced: the first writer wins."""
    code = base["exit"]
    if saved_runs is None or code < 0 or code >= 128:
        return
    try:
        if gate_killed(base_path, left(deadline)):
            return
        with open(base_path, encoding="utf-8", newline="") as f:
            output = f.read()
    except (GateUnavailable, OSError):
        return
    records = saved_runs[0]
    record = {"version": BASE_RUN_FORMAT, "cmd": cmd, "exit": code, "output": output}
    try:
        os.makedirs(records, exist_ok=True)
        fd, tmp = tempfile.mkstemp(dir=records, prefix=".adversary-")
    except OSError:
        return
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(record, f)
        os.link(tmp, base_run_path(saved_runs, cmd))
    except OSError:
        pass
    finally:
        os.unlink(tmp)


def attach_baselines(results, outputs, state, proj, prefix, deadline, work, before):
    """Give every red record a `baseline` from one run at the diff base, reusing a saved
    run of the same command on the same base tree instead of running it again, unless a
    snapshot of the dirty pre-run tree shows the red is tree state."""
    reds = [i for i, r in enumerate(results) if not r.get("skipped") and r.get("exit") != 0]
    for i in reds:
        if results[i]["exit"] == -1:
            results[i]["baseline"] = not_run("the command timed out here, so no comparison applies")
    reds = [i for i in reds if results[i]["exit"] != -1]
    if not reds:
        return
    commit, suffix = tree_outcome(before, work, deadline)
    if classify_reds(results, outputs, reds, state, proj, prefix, deadline, work, commit):
        suffix = DIRTY_BESIDE_COMMITS_NOTE
    for i in reds:
        baseline = results[i]["baseline"]
        if baseline["status"] != "tree-state":
            baseline["note"] += suffix


def classify_reds(results, outputs, reds, state, proj, prefix, deadline, work, commit):
    """Classify each red record against the diff base, first rerunning those held to exit 0
    on the snapshot `commit` when HEAD is the diff base. True when a snapshot was declined
    because HEAD is not the diff base."""
    diff_base = state.get("diff_base")
    diff_base = diff_base.strip() if isinstance(diff_base, str) else ""
    if not diff_base:
        for i in reds:
            results[i]["baseline"] = not_run("no diff base was resolved, so nothing to compare against")
        return False
    if left(deadline) <= 0:
        for i in reds:
            results[i]["baseline"] = not_run("the deadline was spent before the baseline run")
        return False
    try:
        top = git_text(["rev-parse", "--show-toplevel"], proj, left(deadline))
        subdir = git_text(["rev-parse", "--show-prefix"], proj, left(deadline))
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as e:
        for i in reds:
            results[i]["baseline"] = not_run(f"the project's git repository could not be read: {e}")
        return False
    beside_commits = bool(commit) and not head_is_base(top, diff_base, deadline)
    if beside_commits:
        commit = None
    saved_runs = base_run_store(proj, diff_base, subdir, prefix, deadline)
    worktrees = []
    try:
        rerunnable = [i for i in reds if results[i]["exit"] < 128 and held_to_exit_0(results[i])] if commit else []
        if rerunnable:
            rerun_on_snapshot(results, rerunnable, top, subdir, prefix, deadline, work, commit, worktrees)
        reds = [i for i in reds if "baseline" not in results[i]]
        if reds:
            compare_with_base(results, outputs, reds, diff_base, top, subdir, prefix, deadline, work, saved_runs, worktrees)
    finally:
        for tree in worktrees:
            remove_worktree(top, tree)
    return beside_commits


def compare_with_base(results, outputs, reds, diff_base, top, subdir, prefix, deadline, work, saved_runs, worktrees):
    def compare(i, base, base_path):
        try:
            results[i]["baseline"] = classify(results[i], outputs[i], base, base_path, deadline)
        except GateUnavailable as e:
            results[i]["baseline"] = dict(not_run(f"baseline comparison unavailable: {e}"), exit=base["exit"])

    misses = []
    for i in reds:
        saved = load_base_run(saved_runs, results[i]["cmd"])
        if saved is None:
            misses.append(i)
            continue
        if left(deadline) <= 0:
            results[i]["baseline"] = not_run("the deadline was spent before this command's baseline run")
            continue
        base_path = os.path.join(work, f"base-{i}.txt")
        try:
            with open(base_path, "w", encoding="utf-8", newline="") as f:
                f.write(saved[1])
        except OSError as e:
            results[i]["baseline"] = not_run(f"the saved diff-base run could not be written out: {e}")
            continue
        compare(i, {"exit": saved[0]}, base_path)
    if not misses:
        return
    tree = os.path.join(work, "base")
    worktrees.append(tree)
    try:
        add_worktree(top, tree, diff_base, work, deadline)
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as e:
        for i in misses:
            results[i]["baseline"] = not_run(f"the diff-base worktree could not be created: {e}")
        return
    base_proj = os.path.join(tree, subdir)
    for i in misses:
        remaining = left(deadline)
        if remaining <= 0:
            results[i]["baseline"] = not_run("the deadline was spent before this command's baseline run")
            continue
        if not os.path.isdir(base_proj):
            results[i]["baseline"] = not_run(f"the project directory {subdir!r} does not exist at the diff base")
            continue
        base_path = os.path.join(work, f"base-{i}.txt")
        base = run_one(results[i]["cmd"], base_proj, min(PER_COMMAND_TIMEOUT_SECS, remaining), prefix, base_path)
        if base["exit"] == -1:
            results[i]["baseline"] = not_run("the command timed out on the diff base")
            continue
        save_base_run(saved_runs, results[i]["cmd"], base, base_path, deadline)
        compare(i, base, base_path)


def carried_candidate(record):
    """Whether a red record's failures may be matched against the carried ones: it ran to a
    nonzero exit below the signal range, is held to exit 0, and no baseline already releases it."""
    code = record.get("exit")
    baseline = record.get("baseline")
    status = baseline.get("status") if isinstance(baseline, dict) else None
    return (
        not record.get("skipped")
        and type(code) is int
        and 0 < code < 128
        and status not in ("pre-existing", "tree-state")
        and held_to_exit_0(record)
    )


def attach_carried(results, outputs, carried, proj, deadline):
    """Give every candidate red record `carried`, its located failures matched by exact key
    against the `carried` items, or a `carried_note` saying why they could not be matched.
    Matching stops at the deadline; the records it did not reach get a note."""
    candidates = [i for i, r in enumerate(results) if carried_candidate(r)]
    if not candidates:
        return

    def unmatched(why, todo=candidates):
        for i in todo:
            results[i]["carried_note"] = f"not matched: {why}"

    if left(deadline) <= 0:
        unmatched(CARRIED_DEADLINE_NOTE)
        return
    if SHARED_DIR not in sys.path:
        sys.path.insert(0, SHARED_DIR)
    try:
        import failure_locations
    except Exception as e:  # noqa: BLE001 -- a broken locator leaves the records judged as uncarried
        unmatched(f"the failure locator is unavailable ({e})")
        return
    try:
        root = git_text(["rev-parse", "--show-toplevel"], proj, max(left(deadline), GATE_CALL_MIN_TIMEOUT_SECS))
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as e:
        unmatched(f"the project's git repository could not be read: {e}")
        return
    index = failure_locations.repo_index(root, max(left(deadline), GATE_CALL_MIN_TIMEOUT_SECS))
    if index is None:
        unmatched("git could not list the repository's files")
        return
    by_key = {}
    for item in carried:
        by_key.setdefault(failure_locations.failure_key(sorted(item["paths"])), item)
    for n, i in enumerate(candidates):
        if left(deadline) <= 0:
            unmatched(CARRIED_DEADLINE_NOTE, candidates[n:])
            return
        results[i].update(match_carried(failure_locations, root, index, outputs[i], by_key))


def match_carried(locator, root, index, path, by_key):
    """The `carried` field (plus a `carried_note` when nothing could be located) for the
    output file `path`."""
    nothing = {"all": False, "matched": [], "new": [], "unlocated": 0}
    try:
        if os.path.getsize(path) > CARRIED_OUTPUT_CAP:
            return {"carried": nothing, "carried_note": f"not matched: the output exceeds {CARRIED_OUTPUT_CAP} bytes"}
        with open(path, encoding="utf-8", errors="replace") as f:
            text = f.read()
    except OSError as e:
        return {"carried": nothing, "carried_note": f"not matched: the output could not be read: {e}"}
    if locator.TRIMMED_MARK in text:
        return {"carried": nothing, "carried_note": "not matched: the output was trimmed"}
    located = locator.resolve_failure_tokens(root, locator.parse_failure_locations(text), index)
    unlocated = locator.unlocated_failure_lines(root, text, index)
    matched, new = [], []
    for f in located:
        item = by_key.get(locator.failure_key(sorted(f["paths"])))
        if item is None:
            new.append(f["paths"])
        elif all(m["paths"] != item["paths"] for m in matched):
            matched.append({"paths": item["paths"], "owner": item["owner"]})
    carried = {"all": bool(located) and not new and not unlocated, "matched": matched, "new": new,
               "unlocated": unlocated}
    if not located:
        return {"carried": carried, "carried_note": "not matched: no failure in the output resolves to a repo path"}
    return {"carried": carried}


def pipeline_fault_record(state, msg):
    """The fail-closed record for a malformed declaration: nothing runs."""
    prior = state.get("pipeline_faults")
    faults = [f for f in prior if isinstance(f, str)] if isinstance(prior, list) else []
    faults.append(msg)
    return {"exec_results": msg, "pipeline_faults": faults}


def recorded_run(result, source, state):
    """The exec_results record for one result of a finalize record, judged as the finalize
    gate judged it."""
    cmd = result["cmd"]
    verdict = result.get("verdict")
    note = result.get("note") if isinstance(result.get("note"), str) else ""
    code = -1 if verdict == "timed_out" else result.get("exit")
    run = {
        "cmd": cmd,
        "role": result.get("role"),
        "exit": code,
        "duration_s": result.get("duration_s"),
        "tail": tail_of(result.get("tail") if isinstance(result.get("tail"), str) else ""),
        "source": source,
        "pass_condition": resolve(cmd, state.get("criteria"), state.get("initial_prompt")),
    }
    if verdict == "pre-existing":
        run["baseline"] = {"status": "pre-existing", "exit": None, "note": note or "the finalize gate matched it to the base suite"}
    elif code != 0:
        compared = verdict == "red" and not note.startswith(UNCLASSIFIED_NOTE)
        run["baseline"] = {
            "status": "regression" if compared else "not-run",
            "exit": None,
            "note": note or f"the finalize gate recorded {verdict}",
        }
    return run


def failing_results(record, passing):
    """What keeps a finalize record from being green, in one line."""
    parts = []
    for r in record["results"]:
        if not isinstance(r, dict) or r.get("verdict") in passing:
            continue
        note = r.get("note") if isinstance(r.get("note"), str) and r.get("note") else ""
        part = f"{r.get('role', '?')} `{r.get('cmd') or '(none)'}` {r.get('verdict', '?')}"
        parts.append(f"{part} ({note[:300]})" if note else part)
    return "; ".join(parts) or "no green or pre-existing test result"


def finalize_record_runs(state):
    """exec_results and full_suite_record from the finalize record of the reviewed tree, at
    the final tier. Nothing here runs: the finalize gate already ran the full suite on this
    tree.

    The reviewed tree is `review_tree` when the caller recorded one before lanes running
    beside this one could write files, else the worktree tree as it stands now."""
    proj = os.path.expanduser((state.get("project_dir") or "").strip() or ".")
    if SHARED_DIR not in sys.path:
        sys.path.insert(0, SHARED_DIR)
    import verify_record

    try:
        import debris

        excludes = debris.parse_excludes(state.get("debris_exclude"))
    except Exception:  # noqa: BLE001 -- debris_lint reports a bad value; the exact tree still matches
        excludes = []

    def missing(tree, detail):
        line = f"{FULL_SUITE_PREFIX}MISSING: {detail}"
        return {
            "exec_results": f"{line}; the finalize gate writes this record",
            "full_suite_record": {"status": "missing", "tree": tree, "line": line},
        }

    review_tree = state.get("review_tree")
    review_tree = review_tree.strip() if isinstance(review_tree, str) else ""
    if review_tree:
        try:
            _, record = verify_record.lookup_exact(proj, "finalize", review_tree)
        except (OSError, RuntimeError, subprocess.SubprocessError):
            return missing(review_tree, "git could not locate the verification record store")
        if record is None:
            return missing(review_tree, f"no finalize record for the reviewed tree {review_tree}")
    else:
        tree = verify_record.worktree_tree(proj)
        if not tree:
            return missing("", "git could not write a tree of the worktree")
        _, record = verify_record.lookup(proj, "finalize", excludes)
        if record is None:
            return missing(tree, f"no finalize record for the worktree tree {tree}")
    source = f"finalize record {record['tree']} @ {record.get('recorded_at') or '?'}"
    runs = [
        recorded_run(r, source, state)
        for r in record["results"]
        if isinstance(r, dict) and isinstance(r.get("cmd"), str) and r["cmd"] and r.get("verdict") != "not_run"
    ]
    suite = {"status": "green", "tree": record["tree"], "line": ""}
    if not verify_record.is_green(record, "finalize"):
        suite = dict(suite, status="red", line=f"{FULL_SUITE_PREFIX}RED: {source}: {failing_results(record, verify_record.PASSING)}")
    return {"exec_results": runs, "full_suite_record": suite}


def tiered(tier, out):
    """`out` as the node's JSON output, with the tier's note for the criterion checks."""
    if TIER_NOTES[tier]:
        out = dict(out, tier_note=TIER_NOTES[tier])
    return json.dumps(out)


def main():
    state = load_state()
    tier = state.get("verification_tier")
    tier = "" if tier is None else tier
    if not isinstance(tier, str) or tier not in TIER_NOTES:
        msg = f"PIPELINE-FAULT: verification_tier declaration invalid: expected '', round or final, got {tier!r}"
        print(json.dumps(pipeline_fault_record(state, msg)))
        return
    try:
        cmds = declared_commands(state.get("verification_commands"))
    except InvalidDeclaration as e:
        msg = f"PIPELINE-FAULT: verification_commands declaration invalid — {e}"
        print(tiered(tier, pipeline_fault_record(state, msg)))
        return
    try:
        prefix = lock_prefix(state.get("build_lock"))
    except InvalidDeclaration as e:
        print(tiered(tier, pipeline_fault_record(state, f"PIPELINE-FAULT: build_lock declaration invalid: {e}")))
        return
    carried, carried_error = declared_carried(state.get("carried_failures"))
    recorded = finalize_record_runs(state) if tier == "final" else None
    if not cmds:
        if recorded is not None:
            if carried_error:
                recorded["carried_error"] = carried_error
            print(tiered(tier, recorded))
            return
        print(
            tiered(
                tier,
                {
                    "exec_results": (
                        "none declared — the caller supplied no verification_commands; "
                        "execution criteria (build/test/lint) cannot be independently "
                        "confirmed. Declare the exact commands via the "
                        "verification_commands variable to get recorded runs."
                    ),
                    **({"carried_error": carried_error} if carried_error else {}),
                },
            )
        )
        return

    proj = os.path.expanduser(
        (state.get("project_dir") or "").strip() or "."
    )
    results = []
    _stack_lock = acquire_stack_lock() if cmds else None  # noqa: F841 — held until exit
    work = tempfile.mkdtemp(prefix="adversary-run-checks-")
    try:
        before = tree_before(proj, work, time.monotonic() + SNAPSHOT_GIT_TIMEOUT_SECS)
        deadline = time.monotonic() + TOTAL_DEADLINE_SECS
        outputs = {}
        for i, cmd in enumerate(cmds):
            remaining = left(deadline)
            if remaining <= 0:
                results.append({"cmd": cmd, "skipped": "deadline"})
                continue
            outputs[i] = os.path.join(work, f"current-{i}.txt")
            results.append(
                run_one(cmd, proj, min(PER_COMMAND_TIMEOUT_SECS, remaining), prefix, outputs[i])
            )
            results[-1]["pass_condition"] = resolve(cmd, state.get("criteria"), state.get("initial_prompt"))
        try:
            attach_baselines(results, outputs, state, proj, prefix, deadline, work, before)
        except Exception as e:  # noqa: BLE001 -- the recorded runs outlive a baseline failure
            for r in results:
                if not r.get("skipped") and r.get("exit") != 0 and "baseline" not in r:
                    r["baseline"] = not_run(f"baseline comparison unavailable: {e}")
        if carried:
            try:
                attach_carried(results, outputs, carried, proj, deadline)
            except Exception as e:  # noqa: BLE001 -- a matching failure leaves the records judged as uncarried
                for r in results:
                    if carried_candidate(r):
                        r.pop("carried", None)
                        r["carried_note"] = f"not matched: {e}"
    finally:
        shutil.rmtree(work, ignore_errors=True)
    out = {"exec_results": results}
    if recorded is not None:
        runs = recorded["exec_results"] if isinstance(recorded["exec_results"], list) else []
        out = {"exec_results": runs + results, "full_suite_record": recorded["full_suite_record"]}
    if carried_error:
        out["carried_error"] = carried_error
    print(tiered(tier, out))


try:
    main()
except Exception as e:  # noqa: BLE001 — never fail the node; degrade visibly
    print(
        json.dumps(
            {
                "exec_results": (
                    f"ENVIRONMENT: verification runner error: {e} — the declared "
                    "commands could not be executed; execution criteria are unproven "
                    "(no recorded run covers them; the verdict blocks CONFORMS when "
                    "commands were declared)"
                )
            }
        )
    )
