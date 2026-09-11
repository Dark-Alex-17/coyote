#!/usr/bin/env python3
"""Domain linter pass for the code-reviewer graph.

Runs whichever mechanized checkers exist on PATH against the matching changed
files: tflint (Terraform), hadolint (Dockerfiles), actionlint (workflow
files), and `buf breaking` when buf-configured protos changed. Read-only,
never installs anything, and each tool degrades silently to a skip note.
Output is routed per-file where possible so the domain leaders can extract
their slice's lines.
"""

import json
import os
import re
import shutil
import subprocess

def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


state = load_state()
proj = state.get("project_dir") or "."
files = state.get("changed_files") or []
spec = state.get("resolved_diff_spec") or "worktree"
notes = []


def run(cmd, timeout=60):
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout, cwd=proj)
    return (r.stdout + r.stderr).strip()


def capped(text, n=40):
    lines = text.splitlines()
    return "\n".join(lines[:n]) + (f"\n… ({len(lines) - n} more lines)" if len(lines) > n else "")


try:
    tf = [f for f in files if f.endswith(".tf")]
    if tf and shutil.which("tflint"):
        notes.append("[tflint]\n" + capped(run(["tflint", "--no-color"], 120) or "clean"))
    elif tf:
        notes.append("[tflint] skipped — not on PATH")

    dockers = [f for f in files if re.search(r"(^|/)Dockerfile", f)]
    if dockers and shutil.which("hadolint"):
        notes.append("[hadolint]\n" + capped(run(["hadolint", "--no-color", *dockers[:10]], 60) or "clean"))
    elif dockers:
        notes.append("[hadolint] skipped — not on PATH")

    workflows = [f for f in files if ".github/workflows/" in f]
    if workflows and shutil.which("actionlint"):
        notes.append("[actionlint]\n" + capped(run(["actionlint", "-no-color", *workflows[:10]], 60) or "clean"))
    elif workflows:
        notes.append("[actionlint] skipped — not on PATH")

    protos = [f for f in files if f.endswith(".proto")]
    if protos and os.path.isfile(os.path.join(proj, "buf.yaml")) and shutil.which("buf"):
        base = None
        if "..." in spec:
            base = spec.split("...")[0]
        elif ".." in spec:
            base = spec.split("..")[0]
        elif spec not in ("staged", "worktree", "HEAD~1"):
            base = spec
        else:
            try:
                r = subprocess.run(["git", "-C", proj, "rev-parse", "HEAD"],
                                   capture_output=True, text=True, timeout=15)
                base = r.stdout.strip() if r.returncode == 0 else None
            except Exception:  # noqa: BLE001
                base = None
        if base:
            out = run(["buf", "breaking", "--against", f".git#ref={base}"], 120)
            notes.append("[buf breaking] (wire-compat breaks are 🔴 by default)\n" + capped(out or "clean"))
        else:
            notes.append("[buf breaking] skipped — no base ref resolvable")
    elif protos:
        notes.append("[buf breaking] skipped — no buf.yaml or buf not on PATH")
except Exception as e:  # noqa: BLE001
    notes.append(f"[linters] pass degraded: {e}")

print(json.dumps({"linter_notes": "\n\n".join(notes) if notes else "(no applicable linters)"}))
