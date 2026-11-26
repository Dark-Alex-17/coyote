#!/usr/bin/env python3
"""The test suite run once at a base commit, in the background.

The base suite runs in a temporary detached worktree of the base commit, so
the live tree is never touched and an edited tree does not stop it. A
separate worktree directory means a separate in-tree build output; a build
directory shared through the environment is what build_lock serialises, and
the job runs its command under that prefix like every other gate. The
command never sees the caller's LLM_AGENT_VAR_* variables.

A job is claimed with an atomic mkdir of `jobs/<key>/` under
`<git-common-dir>/coyote-baselines/`; whoever loses the mkdir attaches to
the job instead of starting a second one, and a job found dead is never
restarted: its callers re-run what they need at the base themselves (lazy).
A lazy run takes the same claim, so two callers never run the same command
at the same base at once; one that loses to a dead job runs unclaimed. A job
is alive while its pid exists and its heartbeat is under STALE_SECS old, so
a reused pid does not keep a dead job alive.
Where the platform has no process groups (Windows) no job is started: start
reports the lazy mode and why, and callers re-run what they need at the base.
Each run gets its own 0700 worktree directory (mkdtemp) under
`coyote-baselines/worktrees/`; nothing outside that directory is ever
removed, and a run whose worktree vanished stores no record.
The record `tests-<key>.json` keeps the format baseline_tests stores, so a
record taken synchronously at the same commit, directory and command is
reused, and the per-worktree `--git-path coyote-baselines` store is still
read (never written). An existing record is never replaced.

CLI (every action prints one JSON object):
  start   --root DIR --cmd CMD [--floor SHA | --base SHA] [--timeout SECS]
  resolve --root DIR --cmd CMD [--floor SHA]      the base and key start uses
  status  --root DIR (--key KEY | --cmd CMD [--chain N] [--floor SHA])
  await   --root DIR --key KEY --until EPOCH   exit 0 record, 3 no record and
                                               no live job, 4 deadline
  sweep   --root DIR
  cancel  --root DIR                           kills every live job this
                                               worktree started
  run     --root DIR --base SHA --cmd CMD [--timeout SECS] [--job] [--mode M]
"""

import argparse
import calendar
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time

SHARED = os.path.dirname(os.path.abspath(__file__))
UTILS = os.path.join(SHARED, "utils.sh")
GATE_LIB = os.path.join(SHARED, "..", "coder", "scripts", "gate_lib.sh")
GIT_SECS = 60
HEARTBEAT_SECS = 10
STALE_SECS = 300
CLAIM_GRACE_SECS = 60
AWAIT_POLL_SECS = 2
LIVE = ("claimed", "running")
KILLED_ERROR = "the job process is gone and left no record"
NO_GROUPS_ERROR = "this platform has no process groups, so the base suite cannot run as a background job"
AGENT_VAR_PREFIX = "LLM_AGENT_VAR_"


def bash_program():
    """The first bash on PATH outside System32.

    Windows resolves a bare `bash` in System32 before PATH, and that bash is
    the WSL launcher, not the Git Bash the gate scripts run under."""
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        found = shutil.which("bash", path=directory) if directory else None
        if found and "system32" not in found.replace("\\", "/").lower().split("/"):
            return found
    return "bash"


def git(root, *args, timeout=GIT_SECS):
    r = subprocess.run(
        ["git", "-C", root, *args],
        capture_output=True,
        encoding="utf-8",
        errors="replace",
        timeout=timeout,
        stdin=subprocess.DEVNULL,
    )
    if r.returncode != 0:
        raise RuntimeError(f"git {args[0]} failed: {(r.stderr or r.stdout).strip()[:300]}")
    return r.stdout.strip()


def now_iso():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def record_key(base, rel, cmd):
    return hashlib.sha256(f"{base}\n{rel}\n{cmd}".encode("utf-8")).hexdigest()[:16]


def toplevel(root):
    return os.path.realpath(git(root, "rev-parse", "--show-toplevel"))


def rel_dir(root):
    rel = os.path.relpath(os.path.realpath(root), toplevel(root))
    return "." if rel == os.curdir else rel.replace(os.sep, "/")


def _absolute(root, path):
    return os.path.realpath(path if os.path.isabs(path) else os.path.join(root, path))


def store(root):
    return os.path.join(_absolute(root, git(root, "rev-parse", "--git-common-dir")), "coyote-baselines")


def worktrees_dir(root):
    return os.path.join(store(root), "worktrees")


def stores(root):
    """The store records are written to, then the per-worktree one read as a fallback."""
    found = [store(root)]
    try:
        old = _absolute(root, git(root, "rev-parse", "--git-path", "coyote-baselines"))
    except RuntimeError:
        return found
    if old not in found:
        found.append(old)
    return found


def agent_free_env(**extra):
    """os.environ without the caller's LLM_AGENT_VAR_* variables, plus `extra`."""
    env = {k: v for k, v in os.environ.items() if not k.startswith(AGENT_VAR_PREFIX)}
    env.update(extra)
    return env


def valid_record(path):
    try:
        with open(path, encoding="utf-8") as f:
            record = json.load(f)
    except (OSError, ValueError):
        return False
    return isinstance(record, dict) and record.get("version") == 1 and record.get("status") in ("green", "red")


def find_record(root, key):
    """(path of a readable record for `key`, path of an unreadable one); either may be None."""
    corrupt = None
    for directory in stores(root):
        path = os.path.join(directory, f"tests-{key}.json")
        if os.path.exists(path):
            if valid_record(path):
                return path, None
            corrupt = corrupt or path
    return None, corrupt


def job_dir(root, key):
    return os.path.join(store(root), "jobs", key)


def read_status(root, key):
    try:
        with open(os.path.join(job_dir(root, key), "status.json"), encoding="utf-8") as f:
            status = json.load(f)
    except (OSError, ValueError):
        return None
    return status if isinstance(status, dict) and status.get("version") == 1 else None


def write_status(root, key, status):
    directory = job_dir(root, key)
    fd, tmp = tempfile.mkstemp(prefix=".status-", dir=directory)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump(status, f, indent=2)
    os.replace(tmp, os.path.join(directory, "status.json"))


def pid_alive(pid):
    if not isinstance(pid, int) or pid <= 0:
        return False
    if os.name == "nt":
        return _windows_pid_alive(pid)
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    except OSError:
        return False
    return True


def _windows_pid_alive(pid):
    """Asks Windows whether `pid` is running; os.kill there would terminate it."""
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.OpenProcess.argtypes = (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)
    kernel32.OpenProcess.restype = wintypes.HANDLE
    kernel32.GetExitCodeProcess.argtypes = (wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD))
    kernel32.GetExitCodeProcess.restype = wintypes.BOOL
    kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
    process_query_limited_information = 0x1000
    error_access_denied = 5
    still_active = 259
    handle = kernel32.OpenProcess(process_query_limited_information, False, pid)
    if not handle:
        return ctypes.get_last_error() == error_access_denied
    try:
        code = wintypes.DWORD()
        if not kernel32.GetExitCodeProcess(handle, ctypes.byref(code)):
            return False
        return code.value == still_active
    finally:
        kernel32.CloseHandle(handle)


def has_process_groups():
    return hasattr(os, "killpg") and hasattr(os, "getpgid")


def _age(stamp):
    try:
        return time.time() - calendar.timegm(time.strptime(stamp, "%Y-%m-%dT%H:%M:%SZ"))
    except (TypeError, ValueError):
        return float("inf")


def job_alive(status):
    if status.get("state") not in LIVE:
        return False
    if status.get("pid") is None:
        return _age(status.get("started_at")) < CLAIM_GRACE_SECS
    return pid_alive(status.get("pid")) and _age(status.get("heartbeat_at")) < STALE_SECS


def own_group():
    """This process's group id, or None where the platform has no process groups."""
    return os.getpgid(0) if hasattr(os, "getpgid") else None


def kill_group(pgid, sig=None):
    if not isinstance(pgid, int) or pgid <= 0:
        return
    if sig is None:
        sig = getattr(signal, "SIGKILL", signal.SIGTERM)
    try:
        if hasattr(os, "killpg"):
            os.killpg(pgid, sig)
        else:
            os.kill(pgid, sig)
    except OSError:
        pass


def inside(path, directory):
    try:
        return os.path.commonpath([os.path.realpath(path), os.path.realpath(directory)]) == os.path.realpath(directory)
    except ValueError:
        return False


def remove_worktree(root, path):
    """Removes base worktree `path`; a path outside the worktrees directory is left alone."""
    if not path or not inside(path, worktrees_dir(root)) or os.path.realpath(path) == os.path.realpath(worktrees_dir(root)):
        return
    top = toplevel(root)
    for args in (["worktree", "remove", "--force", path], ["worktree", "prune"]):
        try:
            git(top, *args)
        except (OSError, RuntimeError, subprocess.SubprocessError):
            pass
    shutil.rmtree(path, ignore_errors=True)
    try:
        git(top, "worktree", "prune")
    except (OSError, RuntimeError, subprocess.SubprocessError):
        pass


def mark_killed(root, key, status, error=KILLED_ERROR):
    # A dead or stale job's group id may already belong to another process.
    if job_alive(status):
        kill_group(status.get("cmd_pgid"))
    remove_worktree(root, status.get("worktree"))
    status.update(state="killed", finished_at=now_iso(), error=status.get("error") or error)
    write_status(root, key, status)
    return status


def sweep(root):
    """Marks every live-looking job whose process is gone as killed and removes its worktree.

    A worktree is removed only for a job that has ended or is dead by job_alive's rule."""
    swept = []
    jobs = os.path.join(store(root), "jobs")
    try:
        keys = sorted(os.listdir(jobs))
    except OSError:
        keys = []
    for key in keys:
        status = read_status(root, key)
        if status is None:
            continue
        if status.get("state") in LIVE:
            if not job_alive(status):
                mark_killed(root, key, status)
                swept.append(key)
        elif status.get("worktree") and os.path.exists(status["worktree"]):
            remove_worktree(root, status["worktree"])
    try:
        git(toplevel(root), "worktree", "prune")
    except (OSError, RuntimeError, subprocess.SubprocessError):
        pass
    return {"swept": swept}


def cancel(root):
    """Kills every live job started from this worktree, removes its worktree and marks it cancelled."""
    top = toplevel(root)
    cancelled = []
    jobs = os.path.join(store(root), "jobs")
    try:
        keys = sorted(os.listdir(jobs))
    except OSError:
        keys = []
    for key in keys:
        status = read_status(root, key)
        if status is None or status.get("state") not in LIVE or status.get("root") != top:
            continue
        # The job first, so it cannot store a record from a command killed
        # under it; a job that is not alive is never signalled, since its
        # group ids may already belong to other processes.
        if job_alive(status):
            kill_group(status.get("pgid"))
            kill_group(status.get("cmd_pgid"))
        remove_worktree(root, status.get("worktree"))
        status.update(state="cancelled", finished_at=now_iso(), error="cancelled by a later gate in this worktree")
        write_status(root, key, status)
        cancelled.append(key)
    return {"cancelled": cancelled}


def commit_of(root, rev):
    return git(root, "rev-parse", "--verify", "-q", f"{rev}^{{commit}}")


def resolve_base(root, floor):
    head = commit_of(root, "HEAD")
    if floor:
        try:
            floor_sha = commit_of(root, floor)
            git(root, "merge-base", "--is-ancestor", floor_sha, head)
            return floor_sha
        except RuntimeError:
            pass
    return head


def attach(root, key, base):
    out = {"key": key, "base": base}
    status = read_status(root, key)
    if status is None:
        return {**out, "mode": "lazy", "state": "unreadable", "error": "the job status cannot be read"}
    state = status.get("state")
    out["state"] = state
    if state == "done" and find_record(root, key)[0]:
        return {**out, "mode": "background"}
    if state in LIVE:
        if job_alive(status):
            return {**out, "mode": "background"}
        status = mark_killed(root, key, status)
        return {**out, "mode": "lazy", "state": "killed", "error": status["error"]}
    return {**out, "mode": "lazy", "error": status.get("error") or f"the job ended {state} without a record"}


def new_status(root, key, base, cmd, timeout, state):
    return {
        "version": 1,
        "key": key,
        "base": base,
        "root": toplevel(root),
        "rel_dir": rel_dir(root),
        "cmd": cmd,
        "timeout": timeout,
        "pid": None,
        "pgid": None,
        "cmd_pgid": None,
        "state": state,
        "started_at": now_iso(),
        "heartbeat_at": now_iso(),
        "finished_at": None,
        "worktree": None,
        "error": "",
        "lock_prefix": os.environ.get("LLM_AGENT_VAR_BUILD_LOCK", ""),
    }


def start(root, cmd, floor="", timeout=0, base=""):
    top = toplevel(root)
    rel = rel_dir(root)
    base = commit_of(root, base) if base else resolve_base(root, floor)
    key = record_key(base, rel, cmd)
    out = {"key": key, "base": base}
    sweep(root)
    record, corrupt = find_record(root, key)
    if record:
        return {**out, "mode": "reused", "record": record}
    if corrupt:
        return {**out, "mode": "none", "error": f"the stored baseline {corrupt} cannot be read"}
    if not has_process_groups():
        return {**out, "mode": "lazy", "state": "unsupported", "error": NO_GROUPS_ERROR}
    directory = job_dir(root, key)
    os.makedirs(os.path.dirname(directory), exist_ok=True)
    try:
        os.mkdir(directory)
    except FileExistsError:
        return attach(root, key, base)
    status = new_status(root, key, base, cmd, timeout, "claimed")
    write_status(root, key, status)
    argv = [sys.executable, os.path.abspath(__file__), "run", "--job", "--root", os.path.realpath(root),
            "--base", base, "--cmd", cmd, "--timeout", str(timeout)]
    # A new session keeps the job alive past the node that started it;
    # close_fds keeps it from inheriting a lock descriptor its parent holds.
    # The job reads its lock prefix from status.json.
    with open(os.path.join(directory, "job.log"), "ab") as log:
        proc = subprocess.Popen(
            argv,
            cwd=top,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            close_fds=True,
            env=agent_free_env(PYTHONDONTWRITEBYTECODE="1"),
        )
    status.update(pid=proc.pid, pgid=proc.pid)
    write_status(root, key, status)
    return {**out, "mode": "background", "state": "running"}


def classify(output_file):
    """{ids, hash, reason} of a red transcript, read by gate_lib's parsers."""
    script = (
        'source "$1"; hash=$(gate_failure_hash "$2"); reason=$(gate_unidentified_reason "$2"); '
        'gate_failing_tests "$2" | jq -Rnc --arg h "$hash" --arg r "$reason" '
        "'{ids: [inputs | select(. != \"\")], hash: $h, reason: $r}'"
    )
    r = subprocess.run([bash_program(), "-c", script, "classify", GATE_LIB, output_file],
                       capture_output=True, encoding="utf-8", errors="replace", stdin=subprocess.DEVNULL)
    if r.returncode != 0:
        raise RuntimeError(f"the failures could not be read: {r.stderr.strip()[:300]}")
    return json.loads(r.stdout)


def run_command(cwd, cmd, timeout, output_file, lock_prefix, on_spawn=None, on_tick=None):
    """(exit code, timed out) of `cmd` run through gate_lib's run_gate in its own process group.

    run_gate reads the lock prefix from a shell variable that is not exported,
    so the command itself sees no LLM_AGENT_VAR_* variable."""
    env = agent_free_env(GATE_BASE_CMD=cmd, GATE_BASE_LOCK=lock_prefix)
    with open(output_file, "wb") as out:
        proc = subprocess.Popen(
            [bash_program(), "-c",
             'source "$1"; source "$2"; LLM_AGENT_VAR_BUILD_LOCK=$GATE_BASE_LOCK; unset GATE_BASE_LOCK; '
             '(cd "$3" && run_gate "$GATE_BASE_CMD")',
             "base-suite", UTILS, GATE_LIB, cwd],
            stdin=subprocess.DEVNULL,
            stdout=out,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            close_fds=True,
            env=env,
        )
    if on_spawn:
        on_spawn(proc.pid)
    began = time.monotonic()
    last_tick = began
    while proc.poll() is None:
        if timeout and time.monotonic() - began >= timeout:
            kill_group(proc.pid, signal.SIGTERM)
            try:
                proc.wait(2)
            except subprocess.TimeoutExpired:
                pass
            kill_group(proc.pid)
            proc.wait()
            return 124, True
        if on_tick and time.monotonic() - last_tick >= HEARTBEAT_SECS:
            on_tick()
            last_tick = time.monotonic()
        time.sleep(0.2)
    return proc.returncode, False


def store_record(root, key, record):
    """Writes `record` as tests-<key>.json unless one exists; returns the path of the record that won."""
    directory = store(root)
    os.makedirs(directory, exist_ok=True)
    path = os.path.join(directory, f"tests-{key}.json")
    fd, tmp = tempfile.mkstemp(prefix=f".tests-{key}.", dir=directory)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(record, f, indent=2)
        try:
            os.link(tmp, path)
        except FileExistsError:
            pass
    finally:
        os.unlink(tmp)
    return path


def new_worktree_dir(root, key):
    """A fresh 0700 directory for the base worktree of `key`; refuses a symlinked parent.

    mkdtemp creates the directory with an exclusive mkdir, so a path that
    already exists, symlink or not, is never reused."""
    parent = worktrees_dir(root)
    if os.path.islink(parent) or os.path.islink(os.path.dirname(parent)):
        raise RuntimeError(f"{parent} or its parent is a symlink, so no base worktree is made there")
    os.makedirs(parent, mode=0o700, exist_ok=True)
    return tempfile.mkdtemp(prefix=f"coyote-base-{key}-", dir=parent)


def run_at_base(root, base, cmd, timeout, mode, work, lock_prefix, on_spawn=None, on_tick=None,
                on_worktree=None, still_ours=None):
    """Runs `cmd` at `base` in a temporary detached worktree and stores its record.

    Returns {"record": path} or {"error": reason, "state": failed|timed_out|cancelled}.
    `still_ours`, when given, is asked before the record is stored; False
    means another process took the run over and nothing is stored.
    """
    top = toplevel(root)
    rel = rel_dir(root)
    key = record_key(base, rel, cmd)
    hooks = os.path.join(work, "no-hooks")
    worktree = None
    began = time.monotonic()
    try:
        os.makedirs(hooks, exist_ok=True)
        worktree = new_worktree_dir(root, key)
        if on_worktree:
            on_worktree(worktree)
        git(top, "-c", f"core.hooksPath={hooks}", "worktree", "add", "--detach", worktree, base)
        if on_tick:
            on_tick()
        cwd = worktree if rel == "." else os.path.join(worktree, rel)
        output = os.path.join(work, "output.txt")
        rc, timed_out = run_command(cwd, cmd, timeout, output, lock_prefix, on_spawn, on_tick)
        if timed_out:
            return {"state": "timed_out", "error": f"the base suite was killed after {timeout} s (test_timeout)"}
        if not os.path.isdir(cwd) or not os.path.exists(os.path.join(worktree, ".git")):
            return {"state": "failed", "error": f"the base worktree {worktree} vanished during the run, so no record was stored"}
        if still_ours and not still_ours():
            return {"state": "cancelled", "error": "the run was cancelled before it stored a record"}
        record = {
            "version": 1,
            "head": base,
            "workdir": os.path.realpath(root),
            "command": cmd,
            "recorded_at": now_iso(),
            "status": "green" if rc == 0 else "red",
            "exit_code": rc,
            "failures": [],
            "failure_hash": "",
            "unidentified_reason": "",
            "mode": mode,
            "duration_s": round(time.monotonic() - began, 1),
            "ran_in": f"detached worktree at {base[:7]}",
            "lock_prefix": lock_prefix,
        }
        if rc != 0:
            parsed = classify(output)
            record.update(failures=sorted(set(parsed["ids"])), failure_hash=parsed["hash"],
                          unidentified_reason=parsed["reason"])
        return {"record": store_record(root, key, record)}
    except (OSError, RuntimeError, subprocess.SubprocessError, ValueError) as e:
        return {"state": "failed", "error": str(e)[:300]}
    finally:
        remove_worktree(root, worktree)


def run_claimed(root, key, status, base, cmd, timeout, mode, work):
    """Runs `cmd` at `base` as the owner of job `key`, keeping its status.json current."""

    def update(**fields):
        status.update(fields)
        write_status(root, key, status)

    def still_ours():
        current = read_status(root, key)
        return current is not None and current.get("state") == "running" and current.get("pid") == os.getpid()

    result = run_at_base(
        root, base, cmd, timeout, mode, work, status.get("lock_prefix") or "",
        on_spawn=lambda pgid: update(cmd_pgid=pgid),
        on_tick=lambda: update(heartbeat_at=now_iso()),
        on_worktree=lambda path: update(worktree=path),
        still_ours=still_ours,
    )
    if not still_ours():
        return result
    if "record" in result:
        update(state="done", finished_at=now_iso(), heartbeat_at=now_iso())
    else:
        update(state=result["state"], finished_at=now_iso(), error=result["error"])
    return result


def run_job(root, base, cmd, timeout):
    key = record_key(base, rel_dir(root), cmd)
    directory = job_dir(root, key)
    deadline = time.monotonic() + 5
    status = read_status(root, key)
    while (status is None or status.get("pid") != os.getpid()) and time.monotonic() < deadline:
        time.sleep(0.05)
        status = read_status(root, key)
    if status is None:
        return {"error": "the job status cannot be read"}
    status.update(pid=os.getpid(), pgid=own_group(), state="running", heartbeat_at=now_iso())
    write_status(root, key, status)
    return run_claimed(root, key, status, base, cmd, timeout, "background", directory)


def run_foreground(root, base, cmd, timeout, mode):
    """A lazy run at `base`: the record when one exists, else a run under the job claim of its key.

    A live job holding the claim is awaited until `timeout` instead; a dead
    one leaves the run unclaimed, in its own worktree."""
    key = record_key(base, rel_dir(root), cmd)
    record, _ = find_record(root, key)
    if record:
        return {"record": record}
    directory = job_dir(root, key)
    os.makedirs(os.path.dirname(directory), exist_ok=True)
    claimed = True
    try:
        os.mkdir(directory)
    except FileExistsError:
        claimed = False
        status = read_status(root, key)
        # The winner writes its status just after its mkdir.
        settle = time.monotonic() + 5
        while status is None and time.monotonic() < settle:
            time.sleep(0.05)
            status = read_status(root, key)
        if status is not None and job_alive(status):
            code, out = await_record(root, key, time.time() + (timeout or 3600))
            if code == 0:
                return {"record": out["record"]}
            if code == 4:
                return {"state": "timed_out", "error": "another run of this command at the base was still running"}
    work = tempfile.mkdtemp(prefix="coyote-base-run-")
    try:
        if not claimed:
            return run_at_base(root, base, cmd, timeout, mode, work, os.environ.get("LLM_AGENT_VAR_BUILD_LOCK", ""))
        status = new_status(root, key, base, cmd, timeout, "running")
        # The caller's process group is not this run's to kill, so pgid stays unset.
        status.update(pid=os.getpid())
        write_status(root, key, status)
        return run_claimed(root, key, status, base, cmd, timeout, mode, work)
    finally:
        shutil.rmtree(work, ignore_errors=True)


def status_of(root, key):
    record, corrupt = find_record(root, key)
    job = read_status(root, key)
    return {"key": key, "record": record, "corrupt": corrupt, "job": job, "deadline": job_deadline(job)}


def job_deadline(status):
    """Epoch seconds at which the job's own timeout runs out, or None when it has none."""
    if not status or not isinstance(status.get("timeout"), int) or status["timeout"] <= 0:
        return None
    age = _age(status.get("started_at"))
    if age == float("inf"):
        return None
    return int(time.time() - age) + status["timeout"]


def _epoch(stamp):
    try:
        return calendar.timegm(time.strptime(stamp, "%Y-%m-%dT%H:%M:%SZ"))
    except (TypeError, ValueError):
        return None


def chain_gap(root, sha, found):
    """Why the base at `sha` cannot stand for HEAD's, or "" when it can.

    It can when no merge commit lies between it and HEAD and every commit in
    between was made at or after the base record (or job) was taken, the
    same rule baseline_tests applies to a record reused down the chain."""
    if sha == commit_of(root, "HEAD"):
        return ""
    if git(root, "rev-list", "--merges", f"{sha}..HEAD"):
        return f"a merge commit lies between the base record at {sha[:7]} and HEAD"
    taken = None
    if found["record"]:
        try:
            with open(found["record"], encoding="utf-8") as f:
                taken = _epoch(json.load(f).get("recorded_at"))
        except (OSError, ValueError, AttributeError):
            taken = None
    elif found["job"]:
        taken = _epoch(found["job"].get("started_at"))
    if taken is None:
        return f"the base record at {sha[:7]} has no readable time it was taken"
    dates = [int(d) for d in git(root, "log", "--format=%ct", f"{sha}..HEAD").split()]
    if any(d < taken for d in dates):
        return f"a commit between {sha[:7]} and HEAD is older than the base record there, so the record may not be this run's base"
    return ""


def nearest(root, cmd, chain, floor=""):
    """The status of the base a suite at HEAD is judged against.

    With `floor`, exactly the floor commit, which must be an ancestor of
    HEAD. Without, the nearest commit on HEAD's first-parent chain with a
    readable record or a job, when chain_gap allows it."""
    rel = rel_dir(root)
    if floor:
        try:
            sha = commit_of(root, floor)
            git(root, "merge-base", "--is-ancestor", sha, "HEAD")
        except RuntimeError:
            return {"found": False, "error": f"baseline_floor {floor} is not a commit HEAD descends from"}
        return {**status_of(root, record_key(sha, rel, cmd)), "base": sha, "found": True, "floor": True}
    shas = git(root, "rev-list", "--first-parent", f"--max-count={chain}", "HEAD").split()
    for sha in shas:
        key = record_key(sha, rel, cmd)
        found = status_of(root, key)
        if found["record"] or found["job"] is not None:
            gap = chain_gap(root, sha, found)
            if gap:
                return {"found": False, "base": sha, "error": gap}
            return {**found, "base": sha, "found": True}
    return {"found": False, "error": f"no base suite record or job for HEAD or the {chain} commits before it"}


def await_record(root, key, until):
    while True:
        record, _ = find_record(root, key)
        if record:
            return 0, {"key": key, "record": record}
        status = read_status(root, key)
        if status is None or not job_alive(status):
            if status is not None and status.get("state") in LIVE:
                status = mark_killed(root, key, status)
            state = status.get("state") if status else "missing"
            error = (status or {}).get("error") or f"no job and no record for {key}"
            return 3, {"key": key, "state": state, "error": error}
        left = until - time.time()
        if left <= 0:
            return 4, {"key": key, "state": status.get("state"), "error": "the base suite is still running"}
        time.sleep(min(AWAIT_POLL_SECS, left))


def main(argv=None):
    parser = argparse.ArgumentParser(prog="base_suite.py")
    sub = parser.add_subparsers(dest="action", required=True)
    p = sub.add_parser("start")
    p.add_argument("--root", required=True)
    p.add_argument("--cmd", required=True)
    p.add_argument("--floor", default="")
    p.add_argument("--base", default="")
    p.add_argument("--timeout", type=int, default=0)
    p = sub.add_parser("resolve")
    p.add_argument("--root", required=True)
    p.add_argument("--cmd", required=True)
    p.add_argument("--floor", default="")
    p = sub.add_parser("status")
    p.add_argument("--root", required=True)
    p.add_argument("--key", default="")
    p.add_argument("--cmd", default="")
    p.add_argument("--chain", type=int, default=50)
    p.add_argument("--floor", default="")
    p = sub.add_parser("await")
    p.add_argument("--root", required=True)
    p.add_argument("--key", required=True)
    p.add_argument("--until", type=float, required=True)
    p = sub.add_parser("sweep")
    p.add_argument("--root", required=True)
    p = sub.add_parser("cancel")
    p.add_argument("--root", required=True)
    p = sub.add_parser("run")
    p.add_argument("--root", required=True)
    p.add_argument("--base", required=True)
    p.add_argument("--cmd", required=True)
    p.add_argument("--timeout", type=int, default=0)
    p.add_argument("--job", action="store_true")
    p.add_argument("--mode", default="lazy")
    args = parser.parse_args(argv)

    code = 0
    if args.action == "start":
        out = start(args.root, args.cmd, args.floor, args.timeout, args.base)
    elif args.action == "resolve":
        base = resolve_base(args.root, args.floor)
        out = {"base": base, "key": record_key(base, rel_dir(args.root), args.cmd)}
    elif args.action == "status":
        if args.key:
            out = status_of(args.root, args.key)
        elif args.cmd:
            out = nearest(args.root, args.cmd, args.chain, args.floor)
        else:
            parser.error("status needs --key or --cmd")
    elif args.action == "await":
        code, out = await_record(args.root, args.key, args.until)
    elif args.action == "sweep":
        out = sweep(args.root)
    elif args.action == "cancel":
        out = cancel(args.root)
    elif args.job:
        out = run_job(args.root, args.base, args.cmd, args.timeout)
    else:
        out = run_foreground(args.root, args.base, args.cmd, args.timeout, args.mode)
        code = 0 if "record" in out else 3
    print(json.dumps(out))
    return code


if __name__ == "__main__":
    sys.exit(main())
