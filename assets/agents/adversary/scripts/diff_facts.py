#!/usr/bin/env python3
"""Deterministic diff resolution for the adversary graph.

Resolves the diff spec (get_diff semantics: staged → unstaged → HEAD~1 for
"auto"), lists changed files, and captures the bounded diff text every
criterion branch verifies against. Failures degrade into diff_note — the
review proceeds fail-closed rather than crashing.

Also records `diff_base`, the commit SHA the diff is taken against, for the
baseline runs in run_checks.py: HEAD for staged, worktree, and an `auto` that
found staged or unstaged changes; HEAD~1 for an `auto` that fell back to it;
A for `A..B`; the merge-base of A and B for `A...B`; and the normalized base
for a revision. It is empty when the base cannot be resolved.
"""

import json
import re
import os
import subprocess

MAX_DIFF_CHARS = 60000


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


state = load_state()

# Zero criteria from a prompt that visibly carries a plan is an extraction
# failure (observed non-deterministically live), not a fail-closed no-plan
# run — re-run the parse ONCE before the verdict fails closed on it.
_crit = state.get("criteria") or []
_prompt = state.get("initial_prompt") or ""
if not _crit and isinstance(_prompt, str) and re.search(
    r"acceptance criteria|PLAN\s*—|Plan / acceptance", _prompt, re.I
):
    try:
        _pr = int(state.get("parse_retries") or 0)
    except (TypeError, ValueError):
        _pr = 0
    if _pr < 1:
        print(json.dumps({"_next": "parse", "parse_retries": _pr + 1}))
        raise SystemExit(0)
proj = os.path.expanduser(
    (state.get("project_dir_in") or "").strip() or state.get("project_dir") or "."
)
spec = (state.get("diff_spec") or "auto").strip()
# Plain text from git's own differ whatever the user's diff config (no
# colour, external driver or textconv), with paths root-relative when
# project_dir is a subdirectory and the `+++ b/` prefix the file list parses
# whatever diff.noprefix or diff.mnemonicPrefix says.
DIFF_FLAGS = (
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
)


def git(*args):
    r = subprocess.run(
        ["git", "-C", proj, *args],
        capture_output=True,
        encoding="utf-8",
        errors="replace",
        timeout=90,
    )
    if r.returncode != 0:
        raise RuntimeError((r.stderr or r.stdout).strip()[:300])
    return r.stdout


def git_diff(*args):
    return git("diff", *DIFF_FLAGS, *args, "--")


def normalize_base(spec):
    """Bare refs diff from their merge-base with HEAD (three-dot semantics), so
    commits that landed on the base after branching never surface as changes.
    Explicit `..` ranges are the caller's choice and pass through untouched, and
    so does a checkpoint commit on top of HEAD, whose merge-base would be HEAD."""
    if ".." in spec:
        return spec, ""
    try:
        base = git("merge-base", spec, "HEAD").strip()
    except Exception as e:  # noqa: BLE001
        return spec, f" (no merge-base for {spec!r}: {e}; diffing against it directly)"
    try:
        head, commit = git("rev-parse", "HEAD", f"{spec}^{{commit}}").split()
        # a commit some ref holds is a real base ahead of HEAD, never a checkpoint
        held = base == head != commit and bool(git("for-each-ref", "--count=1", "--contains", commit).strip())
    except Exception:  # noqa: BLE001 -- the merge-base stands
        head = commit = ""
        held = True
    if base == head != commit and not held:
        return spec, f" (diff spec {spec!r} is a checkpoint on top of HEAD; diffing against it directly)"
    return base, f" (diff spec {spec!r} normalized to merge-base {base[:12]} — three-dot semantics)"


out = {"diff_note": "", "project_dir": proj}
base_rev = ""
try:
    if spec.startswith("-"):
        raise ValueError(
            f"diff spec {spec!r} is option-shaped; pass a revision, a range, or one "
            "of worktree/staged/auto"
        )
    if spec in ("auto", ""):
        base_rev = "HEAD"
        text = git_diff("--cached")
        if not text.strip():
            text = git_diff()
        if not text.strip():
            base_rev = "HEAD~1"
            text = git_diff("HEAD~1")
    elif spec == "staged":
        base_rev = "HEAD"
        text = git_diff("--cached")
    elif spec == "worktree":
        base_rev = "HEAD"
        text = git_diff("HEAD")
    else:
        base, note = normalize_base(spec)
        base_rev = base
        text = git_diff(base)
        out["diff_note"] += note

    files = sorted({
        line[6:].strip()
        for line in text.splitlines()
        if line.startswith("+++ b/")
    })
    out["changed_files"] = files[:300]
    if len(files) > 300:
        out["diff_note"] += (
            f" (file list capped at 300 of {len(files)} changed files — use the fs "
            "tools for the rest)"
        )
    if len(text) > MAX_DIFF_CHARS:
        out["diff_note"] += (
            f" (truncated to {MAX_DIFF_CHARS} chars of {len(text)} — use the fs tools "
            "to inspect files beyond the cut)"
        )
        text = text[:MAX_DIFF_CHARS]
    out["diff_text"] = text if text.strip() else "(empty diff — nothing changed)"
except Exception as e:  # noqa: BLE001
    out["changed_files"] = []
    out["diff_text"] = "(diff unavailable)"
    out["diff_note"] = f" (diff resolution failed for spec {spec!r}: {e})"


def resolve_base(rev):
    if "..." in rev:
        left, right = rev.split("...", 1)
        return git("merge-base", left or "HEAD", right or "HEAD").strip()
    if ".." in rev:
        rev = rev.split("..", 1)[0] or "HEAD"
    return git("rev-parse", "--verify", f"{rev}^{{commit}}").strip()


try:
    out["diff_base"] = resolve_base(base_rev) if base_rev else ""
except Exception:  # noqa: BLE001 -- an unresolvable base means no baseline run
    out["diff_base"] = ""

print(json.dumps(out))
