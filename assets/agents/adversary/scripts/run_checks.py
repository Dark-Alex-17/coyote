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
NOTHING and records a PIPELINE-FAULT (fail-closed into DIVERGES) instead of
degrading to the soft ENVIRONMENT marker. The engine merges only the keys an
llm node's `output_schema.properties` declares, so a parse LLM coaxed into
emitting an extra `verification_commands` key cannot overwrite the declared
value: the undeclared key is dropped before it reaches state.

Budget: commands run sequentially, so TOTAL runtime is bounded by a deadline
(TOTAL_DEADLINE_SECS = 3300s, the run_checks node's 3600s timeout minus
margin). Without it, a handful of hanging commands (4 x 900s) would blow the
NODE timeout and the executor would kill this script from OUTSIDE — the
in-script degradation would never fire. Each command gets
min(PER_COMMAND_TIMEOUT_SECS, remaining budget); commands the deadline
leaves no budget for are recorded as {"cmd", "skipped": "deadline"} entries,
so the in-script handling stays authoritative. The env var
ADVERSARY_RUN_CHECKS_DEADLINE_SECS overrides the deadline (test seam).

Process hygiene: commands run through the shell. On POSIX each command gets
its own session (start_new_session) and a timeout SIGKILLs the whole process
group, so grandchildren of a hung command cannot linger. On non-POSIX
platforms only the direct shell child is killed — grandchildren of a
timed-out command may survive (documented limitation).

Verification commands run with GRAPH_STATE* (GRAPH_STATE, GRAPH_STATE_FILE)
scrubbed from their environment: the reviewed repo's own tests may exec these very
scripts with their own GRAPH_STATE, and load_state() prefers
GRAPH_STATE_FILE, so an inherited live state file would silently win.
"""

import json
import os
import signal
import subprocess
import time

PER_COMMAND_TIMEOUT_SECS = 900
TOTAL_DEADLINE_SECS = float(os.environ.get("ADVERSARY_RUN_CHECKS_DEADLINE_SECS") or 3300)
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


def tail_of(text):
    tail = "\n".join(text.splitlines()[-TAIL_LINES:])
    if len(tail) > MAX_TAIL_CHARS:
        tail = "…" + tail[-MAX_TAIL_CHARS:]
    return tail


def run_one(cmd, proj, budget):
    """Run one command, hard-bounded by `budget` seconds; return its record."""
    start = time.monotonic()
    popen_kwargs = {"start_new_session": True} if os.name == "posix" else {}
    env = {k: v for k, v in os.environ.items() if not k.startswith("GRAPH_STATE")}
    p = subprocess.Popen(
        cmd,
        shell=True,
        cwd=proj,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
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
        p.communicate()  # reap the child and drain the pipes
        return {
            "cmd": cmd,
            "exit": -1,
            "duration_s": round(time.monotonic() - start, 1),
            "tail": f"TIMEOUT: no exit within {budget:.0f}s — recorded as a failing run",
        }
    combined = (out_s or "") + (("\n" + err_s) if err_s else "")
    return {
        "cmd": cmd,
        "exit": p.returncode,
        "duration_s": round(time.monotonic() - start, 1),
        "tail": tail_of(combined),
    }


def main():
    state = load_state()
    try:
        cmds = declared_commands(state.get("verification_commands"))
    except InvalidDeclaration as e:
        msg = f"PIPELINE-FAULT: verification_commands declaration invalid — {e}"
        prior = state.get("pipeline_faults")
        faults = [f for f in prior if isinstance(f, str)] if isinstance(prior, list) else []
        faults.append(msg)
        print(json.dumps({"exec_results": msg, "pipeline_faults": faults}))
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
    deadline = time.monotonic() + TOTAL_DEADLINE_SECS
    results = []
    for cmd in cmds:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            results.append({"cmd": cmd, "skipped": "deadline"})
            continue
        results.append(run_one(cmd, proj, min(PER_COMMAND_TIMEOUT_SECS, remaining)))
    print(json.dumps({"exec_results": results}))


try:
    main()
except Exception as e:  # noqa: BLE001 — never fail the node; degrade visibly
    print(
        json.dumps(
            {
                "exec_results": (
                    f"ENVIRONMENT: verification runner error: {e} — the declared "
                    "commands could not be executed; treat execution criteria as "
                    "PARTIAL (unproven), not as failed"
                )
            }
        )
    )
