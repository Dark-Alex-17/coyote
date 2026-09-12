#!/usr/bin/env python3
"""Deterministic execution-evidence recorder for the adversary graph.

Runs ONLY the caller-declared verification commands (verification_commands)
in the project dir and records the outcomes as `exec_results` — a list of
{cmd, exit, duration_s, tail} records. Criterion branches cite this record;
they never execute anything themselves.

NO auto-detection: undeclared commands mean a "none declared" marker
(fail-visible, not fail-guessed). Any runner error degrades into an
ENVIRONMENT marker — this script never fails the node.
"""

import json
import os
import subprocess
import time

PER_COMMAND_TIMEOUT_SECS = 900
TAIL_LINES = 50
MAX_TAIL_CHARS = 8000


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def tail_of(text):
    tail = "\n".join(text.splitlines()[-TAIL_LINES:])
    if len(tail) > MAX_TAIL_CHARS:
        tail = "…" + tail[-MAX_TAIL_CHARS:]
    return tail


def main():
    state = load_state()
    declared = state.get("verification_commands")
    cmds = (
        [c.strip() for c in declared if isinstance(c, str) and c.strip()]
        if isinstance(declared, list)
        else []
    )
    if not cmds:
        print(
            json.dumps(
                {
                    "exec_results": (
                        "none declared — the caller supplied no verification_commands; "
                        "execution criteria (build/test/lint) cannot be independently "
                        "confirmed. Declare the exact commands in the spawn prompt to "
                        "get recorded runs."
                    )
                }
            )
        )
        return

    proj = os.path.expanduser(
        (state.get("project_dir") or "").strip() or "."
    )
    results = []
    for cmd in cmds:
        start = time.monotonic()
        try:
            r = subprocess.run(
                cmd,
                shell=True,
                cwd=proj,
                capture_output=True,
                text=True,
                timeout=PER_COMMAND_TIMEOUT_SECS,
            )
            combined = (r.stdout or "") + (("\n" + r.stderr) if r.stderr else "")
            results.append(
                {
                    "cmd": cmd,
                    "exit": r.returncode,
                    "duration_s": round(time.monotonic() - start, 1),
                    "tail": tail_of(combined),
                }
            )
        except subprocess.TimeoutExpired:
            results.append(
                {
                    "cmd": cmd,
                    "exit": -1,
                    "duration_s": round(time.monotonic() - start, 1),
                    "tail": (
                        f"TIMEOUT: no exit within {PER_COMMAND_TIMEOUT_SECS}s — "
                        "recorded as a failing run"
                    ),
                }
            )
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
