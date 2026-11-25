#!/usr/bin/env python3
"""The test suite run once at a base commit, in the background.

The base suite runs in a temporary detached worktree of the base commit, so
the live tree is never touched and an edited tree does not stop it. A
separate worktree directory means a separate in-tree build output; a build
directory shared through the environment is what build_lock serialises, and
the job runs its command under that prefix like every other gate.

A job is claimed with an atomic mkdir of `jobs/<key>/` under
`<git-common-dir>/coyote-baselines/`; whoever loses the mkdir attaches to
the job instead of starting a second one, and a job found dead is never
restarted: its callers re-run what they need at the base themselves (lazy).
The record `tests-<key>.json` keeps the format baseline_tests stores, so a
record taken synchronously at the same commit, directory and command is
reused, and the per-worktree `--git-path coyote-baselines` store is still
read (never written). An existing record is never replaced.

CLI (every action prints one JSON object):
  start  --root DIR --cmd CMD [--floor SHA] [--timeout SECS]
  status --root DIR (--key KEY | --cmd CMD [--chain N])
  await  --root DIR --key KEY --until EPOCH   exit 0 record, 3 no record and
                                              no live job, 4 deadline
  sweep  --root DIR
  run    --root DIR --base SHA --cmd CMD [--timeout SECS] [--job] [--mode M]
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
CLAIM_GRACE_SECS = 60
AWAIT_POLL_SECS = 2
LIVE = ("claimed", "running")
KILLED_ERROR = "the job process is gone and left no record"


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
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


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
    return pid_alive(status.get("pid"))


def kill_group(pgid, sig=signal.SIGKILL):
    if isinstance(pgid, int) and pgid > 0:
        try:
            os.killpg(pgid, sig)
        except (ProcessLookupError, PermissionError):
            pass


def remove_worktree(top, path):
    if not path:
        return
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
    kill_group(status.get("cmd_pgid"))
    remove_worktree(toplevel(root), status.get("worktree"))
    status.update(state="killed", finished_at=now_iso(), error=status.get("error") or error)
    write_status(root, key, status)
    return status


def sweep(root):
    """Marks every live-looking job whose process is gone as killed and removes its worktree."""
    swept = []
    jobs = os.path.join(store(root), "jobs")
    try:
        keys = sorted(os.listdir(jobs))
    except OSError:
        keys = []
    top = toplevel(root)
    for key in keys:
        status = read_status(root, key)
        if status is None:
            continue
        if status.get("state") in LIVE and not job_alive(status):
            mark_killed(root, key, status)
            swept.append(key)
        elif status.get("state") not in LIVE and status.get("worktree") and os.path.exists(status["worktree"]):
            remove_worktree(top, status["worktree"])
    try:
        git(top, "worktree", "prune")
    except (OSError, RuntimeError, subprocess.SubprocessError):
        pass
    return {"swept": swept}


def resolve_base(root, floor):
    head = git(root, "rev-parse", "--verify", "-q", "HEAD^{commit}")
    if floor:
        try:
            floor_sha = git(root, "rev-parse", "--verify", "-q", f"{floor}^{{commit}}")
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


def start(root, cmd, floor="", timeout=0):
    top = toplevel(root)
    rel = rel_dir(root)
    base = resolve_base(root, floor)
    key = record_key(base, rel, cmd)
    out = {"key": key, "base": base}
    sweep(root)
    record, corrupt = find_record(root, key)
    if record:
        return {**out, "mode": "reused", "record": record}
    if corrupt:
        return {**out, "mode": "none", "error": f"the stored baseline {corrupt} cannot be read"}
    directory = job_dir(root, key)
    os.makedirs(os.path.dirname(directory), exist_ok=True)
    try:
        os.mkdir(directory)
    except FileExistsError:
        return attach(root, key, base)
    lock_prefix = os.environ.get("LLM_AGENT_VAR_BUILD_LOCK", "")
    status = {
        "version": 1,
        "key": key,
        "base": base,
        "rel_dir": rel,
        "cmd": cmd,
        "timeout": timeout,
        "pid": None,
        "pgid": None,
        "cmd_pgid": None,
        "state": "claimed",
        "started_at": now_iso(),
        "heartbeat_at": now_iso(),
        "finished_at": None,
        "worktree": None,
        "error": "",
        "lock_prefix": lock_prefix,
    }
    write_status(root, key, status)
    argv = [sys.executable, os.path.abspath(__file__), "run", "--job", "--root", os.path.realpath(root),
            "--base", base, "--cmd", cmd, "--timeout", str(timeout)]
    # A new session keeps the job alive past the node that started it;
    # close_fds keeps it from inheriting a lock descriptor its parent holds.
    with open(os.path.join(directory, "job.log"), "ab") as log:
        proc = subprocess.Popen(
            argv,
            cwd=top,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            close_fds=True,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
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
    r = subprocess.run(["bash", "-c", script, "classify", GATE_LIB, output_file],
                       capture_output=True, encoding="utf-8", errors="replace", stdin=subprocess.DEVNULL)
    if r.returncode != 0:
        raise RuntimeError(f"the failures could not be read: {r.stderr.strip()[:300]}")
    return json.loads(r.stdout)


def run_command(cwd, cmd, timeout, output_file, on_spawn=None, on_tick=None):
    """(exit code, timed out) of `cmd` run through gate_lib's run_gate in its own process group."""
    env = {**os.environ, "GATE_BASE_CMD": cmd}
    with open(output_file, "wb") as out:
        proc = subprocess.Popen(
            ["bash", "-c", 'source "$1"; source "$2"; (cd "$3" && run_gate "$GATE_BASE_CMD")',
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


def run_at_base(root, base, cmd, timeout, mode, work, on_spawn=None, on_tick=None, on_worktree=None):
    """Runs `cmd` at `base` in a temporary detached worktree and stores its record.

    Returns {"record": path} or {"error": reason, "state": failed|timed_out}.
    """
    top = toplevel(root)
    rel = rel_dir(root)
    key = record_key(base, rel, cmd)
    worktree = os.path.join(tempfile.gettempdir(), f"coyote-base-{key}")
    hooks = os.path.join(work, "no-hooks")
    os.makedirs(hooks, exist_ok=True)
    if os.path.exists(worktree):
        remove_worktree(top, worktree)
    if on_worktree:
        on_worktree(worktree)
    began = time.monotonic()
    try:
        git(top, "-c", f"core.hooksPath={hooks}", "worktree", "add", "--detach", worktree, base)
        cwd = worktree if rel == "." else os.path.join(worktree, rel)
        output = os.path.join(work, "output.txt")
        rc, timed_out = run_command(cwd, cmd, timeout, output, on_spawn, on_tick)
        if timed_out:
            return {"state": "timed_out", "error": f"the base suite was killed after {timeout} s (test_timeout)"}
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
            "lock_prefix": os.environ.get("LLM_AGENT_VAR_BUILD_LOCK", ""),
        }
        if rc != 0:
            parsed = classify(output)
            record.update(failures=sorted(set(parsed["ids"])), failure_hash=parsed["hash"],
                          unidentified_reason=parsed["reason"])
        return {"record": store_record(root, key, record)}
    except (OSError, RuntimeError, subprocess.SubprocessError, ValueError) as e:
        return {"state": "failed", "error": str(e)[:300]}
    finally:
        remove_worktree(top, worktree)


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
    status.update(pid=os.getpid(), pgid=os.getpgid(0), state="running", heartbeat_at=now_iso())
    write_status(root, key, status)

    def update(**fields):
        status.update(fields)
        write_status(root, key, status)

    result = run_at_base(
        root, base, cmd, timeout, "background", directory,
        on_spawn=lambda pgid: update(cmd_pgid=pgid),
        on_tick=lambda: update(heartbeat_at=now_iso()),
        on_worktree=lambda path: update(worktree=path),
    )
    if "record" in result:
        update(state="done", finished_at=now_iso(), heartbeat_at=now_iso())
    else:
        update(state=result["state"], finished_at=now_iso(), error=result["error"])
    return result


def run_foreground(root, base, cmd, timeout, mode):
    work = tempfile.mkdtemp(prefix="coyote-base-run-")
    try:
        return run_at_base(root, base, cmd, timeout, mode, work)
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


def nearest(root, cmd, chain):
    """The status of the nearest commit on HEAD's first-parent chain with a readable record or a job."""
    rel = rel_dir(root)
    shas = git(root, "rev-list", "--first-parent", f"--max-count={chain}", "HEAD").split()
    for sha in shas:
        key = record_key(sha, rel, cmd)
        found = status_of(root, key)
        if found["record"] or found["job"] is not None:
            return {**found, "base": sha, "found": True}
    return {"found": False}


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
    p.add_argument("--timeout", type=int, default=0)
    p = sub.add_parser("status")
    p.add_argument("--root", required=True)
    p.add_argument("--key", default="")
    p.add_argument("--cmd", default="")
    p.add_argument("--chain", type=int, default=50)
    p = sub.add_parser("await")
    p.add_argument("--root", required=True)
    p.add_argument("--key", required=True)
    p.add_argument("--until", type=float, required=True)
    p = sub.add_parser("sweep")
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
        out = start(args.root, args.cmd, args.floor, args.timeout)
    elif args.action == "status":
        if args.key:
            out = status_of(args.root, args.key)
        elif args.cmd:
            out = nearest(args.root, args.cmd, args.chain)
        else:
            parser.error("status needs --key or --cmd")
    elif args.action == "await":
        code, out = await_record(args.root, args.key, args.until)
    elif args.action == "sweep":
        out = sweep(args.root)
    elif args.job:
        out = run_job(args.root, args.base, args.cmd, args.timeout)
    else:
        out = run_foreground(args.root, args.base, args.cmd, args.timeout, args.mode)
        code = 0 if "record" in out else 3
    print(json.dumps(out))
    return code


if __name__ == "__main__":
    sys.exit(main())
