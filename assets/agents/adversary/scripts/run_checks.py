#!/usr/bin/env python3
"""Deterministic execution-evidence recorder for the adversary graph.

Runs ONLY the caller-declared verification commands (verification_commands)
in the project dir and records the outcomes as `exec_results` — a list of
{cmd, exit, duration_s, tail} records. Criterion branches cite this record;
they never execute anything themselves.

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
(TOTAL_DEADLINE_SECS = 3300s). The deadline starts once the review-stack lock
is acquired or its bounded wait (STACK_LOCK_WAIT_SECS = 1200s) gives up, so
the run_checks node's 4600s timeout covers 1200 + 3300 plus margin for the
baseline worktree cleanup. Without it, a handful of hanging commands
(4 x 900s) would blow the
NODE timeout and the executor would kill this script from OUTSIDE — the
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
The two full outputs are compared with the coder agent's own gate_lib.sh
helpers through bash, by the rule its verify_tests gate applies: the failure
is pre-existing only when the base run is red (and did not time out) with
the same exit code, the current run was not killed by a signal, and either
every currently failing test id already failed on the base, or neither run
named a test id and the failure output hashes alike. The red record gains
`baseline`: {"status": "pre-existing" | "regression" | "not-run", "exit",
"note"}. not-run means no comparison happened (no diff base, deadline spent,
worktree or helper failure, base run timed out); it blocks like a
regression, so a missing comparison never passes.

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
"""

import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

try:
    import fcntl
except ImportError:  # non-POSIX (Windows): advisory flock unavailable
    fcntl = None

PER_COMMAND_TIMEOUT_SECS = 900
TOTAL_DEADLINE_SECS = float(os.environ.get("ADVERSARY_RUN_CHECKS_DEADLINE_SECS") or 3300)
STACK_LOCK_PATH = os.environ.get("ADVERSARY_STACK_LOCK_PATH") or os.path.expanduser(
    "~/.cache/coyote/review-stack.lock"
)
STACK_LOCK_WAIT_SECS = float(os.environ.get("ADVERSARY_STACK_LOCK_WAIT_SECS") or 1200)
GATE_LIB = os.path.join(
    os.path.dirname(os.path.abspath(__file__)), "..", "..", "coder", "scripts", "gate_lib.sh"
)
GIT_CLEANUP_TIMEOUT_SECS = 20
GATE_CALL_MIN_TIMEOUT_SECS = 10


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


def git_text(args, cwd, timeout):
    r = subprocess.run(
        ["git", "-C", cwd, *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=max(timeout, 1),
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


def attach_baselines(results, outputs, state, proj, prefix, deadline, work):
    """Give every red record a `baseline` from one run at the diff base."""
    reds = [i for i, r in enumerate(results) if not r.get("skipped") and r.get("exit") != 0]
    for i in reds:
        if results[i]["exit"] == -1:
            results[i]["baseline"] = not_run("the command timed out here, so no comparison applies")
    reds = [i for i in reds if results[i]["exit"] != -1]
    if not reds:
        return
    diff_base = state.get("diff_base")
    diff_base = diff_base.strip() if isinstance(diff_base, str) else ""
    if not diff_base:
        for i in reds:
            results[i]["baseline"] = not_run("no diff base was resolved, so nothing to compare against")
        return
    if left(deadline) <= 0:
        for i in reds:
            results[i]["baseline"] = not_run("the deadline was spent before the baseline run")
        return
    try:
        top = git_text(["rev-parse", "--show-toplevel"], proj, left(deadline))
        subdir = git_text(["rev-parse", "--show-prefix"], proj, left(deadline))
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as e:
        for i in reds:
            results[i]["baseline"] = not_run(f"the project's git repository could not be read: {e}")
        return
    tree = os.path.join(work, "base")
    hooks = os.path.join(work, "no-hooks")
    try:
        try:
            os.mkdir(hooks)
            git_text(
                ["-c", f"core.hooksPath={hooks}", "worktree", "add", "--detach", tree, diff_base],
                top,
                left(deadline),
            )
        except (OSError, RuntimeError, subprocess.TimeoutExpired) as e:
            for i in reds:
                results[i]["baseline"] = not_run(f"the diff-base worktree could not be created: {e}")
            return
        base_proj = os.path.join(tree, subdir)
        for i in reds:
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
            try:
                results[i]["baseline"] = classify(results[i], outputs[i], base, base_path, deadline)
            except GateUnavailable as e:
                results[i]["baseline"] = dict(
                    not_run(f"baseline comparison unavailable: {e}"), exit=base["exit"]
                )
    finally:
        remove_worktree(top, tree)


def emit_pipeline_fault(state, msg):
    """Print the fail-closed record for a malformed declaration: nothing runs."""
    prior = state.get("pipeline_faults")
    faults = [f for f in prior if isinstance(f, str)] if isinstance(prior, list) else []
    faults.append(msg)
    print(json.dumps({"exec_results": msg, "pipeline_faults": faults}))


def main():
    state = load_state()
    try:
        cmds = declared_commands(state.get("verification_commands"))
    except InvalidDeclaration as e:
        msg = f"PIPELINE-FAULT: verification_commands declaration invalid — {e}"
        emit_pipeline_fault(state, msg)
        return
    try:
        prefix = lock_prefix(state.get("build_lock"))
    except InvalidDeclaration as e:
        emit_pipeline_fault(state, f"PIPELINE-FAULT: build_lock declaration invalid: {e}")
        return
    if not cmds:
        print(
            json.dumps(
                {
                    "exec_results": (
                        "none declared — the caller supplied no verification_commands; "
                        "execution criteria (build/test/lint) cannot be independently "
                        "confirmed. Declare the exact commands via the "
                        "verification_commands variable to get recorded runs."
                    )
                }
            )
        )
        return

    proj = os.path.expanduser(
        (state.get("project_dir") or "").strip() or "."
    )
    results = []
    _stack_lock = acquire_stack_lock() if cmds else None  # noqa: F841 — held until exit
    deadline = time.monotonic() + TOTAL_DEADLINE_SECS
    work = tempfile.mkdtemp(prefix="adversary-run-checks-")
    try:
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
        try:
            attach_baselines(results, outputs, state, proj, prefix, deadline, work)
        except Exception as e:  # noqa: BLE001 -- the recorded runs outlive a baseline failure
            for r in results:
                if not r.get("skipped") and r.get("exit") != 0 and "baseline" not in r:
                    r["baseline"] = not_run(f"baseline comparison unavailable: {e}")
    finally:
        shutil.rmtree(work, ignore_errors=True)
    print(json.dumps({"exec_results": results}))


try:
    main()
except Exception as e:  # noqa: BLE001 — never fail the node; degrade visibly
    print(
        json.dumps(
            {
                "exec_results": (
                    f"ENVIRONMENT: verification runner error: {e} — the declared "
                    "commands could not be executed; execution criteria are unproven "
                    "(PARTIAL at the criterion level; the verdict blocks CONFORMS when "
                    "commands were declared)"
                )
            }
        )
    )
