#!/usr/bin/env python3
"""Deterministic diff resolution for the adversary graph.

Resolves the diff spec (get_diff semantics: staged → unstaged → HEAD~1 for
"auto"), lists changed files, and captures the bounded diff text every
criterion branch verifies against. Failures degrade into diff_note — the
review proceeds fail-closed rather than crashing.
"""

import json
import os
import subprocess

MAX_DIFF_CHARS = 60000


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


state = load_state()
proj = os.path.expanduser(
    (state.get("project_dir_in") or "").strip() or state.get("project_dir") or "."
)
spec = (state.get("diff_spec") or "auto").strip()


def git(*args):
    r = subprocess.run(["git", "-C", proj, *args], capture_output=True, text=True, timeout=90)
    if r.returncode != 0:
        raise RuntimeError((r.stderr or r.stdout).strip()[:300])
    return r.stdout


out = {"diff_note": "", "project_dir": proj}
try:
    if spec in ("auto", ""):
        text = git("diff", "--cached")
        if not text.strip():
            text = git("diff")
        if not text.strip():
            text = git("diff", "HEAD~1")
    elif spec == "staged":
        text = git("diff", "--cached")
    elif spec == "worktree":
        text = git("diff", "HEAD")
    else:
        text = git("diff", spec)

    files = sorted({
        line[6:].strip()
        for line in text.splitlines()
        if line.startswith("+++ b/")
    })
    out["changed_files"] = files[:300]
    if len(text) > MAX_DIFF_CHARS:
        out["diff_note"] = (
            f" (truncated to {MAX_DIFF_CHARS} chars of {len(text)} — use the fs tools "
            "to inspect files beyond the cut)"
        )
        text = text[:MAX_DIFF_CHARS]
    out["diff_text"] = text if text.strip() else "(empty diff — nothing changed)"
except Exception as e:  # noqa: BLE001
    out["changed_files"] = []
    out["diff_text"] = "(diff unavailable)"
    out["diff_note"] = f" (diff resolution failed for spec {spec!r}: {e})"

print(json.dumps(out))
