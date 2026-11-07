#!/usr/bin/env python3
"""Mechanical anchor check between synthesis and verification.

Every finding must point at code this diff changed. A finding is kept only
when its file is one of the reviewed changed files (relative to the
checkout) and its line range lies inside the file and inside, or within
ADJACENT lines of, a changed hunk. Anything else (a path outside the diff, a
generated file, a line past the end of the file, a range far from every
change, no line range at all) is moved to `unanchored` with the reason, and
the report lists it under "Could not anchor" instead of as a finding. A
cross-repo consumer finding therefore anchors at the changed line in this
diff and cites the external caller as evidence.

Paths are normalized before matching: a leading `./`, `a/` or `b/`, an
absolute path under the checkout, a trailing `:line`, and a unique
component-boundary suffix or prefix match (`admin/x.go` or
`billing/internal/admin/x.go` for `internal/admin/x.go`); the kept finding
carries the normalized path.

Fail-closed: `anchor_checked` is set only by a completed run, so verdict.py
can tell a gate that never ran from one that kept everything; an internal
error keeps every finding and records `anchor_error`, which verdict.py
reports as a pipeline fault.
"""

import json
import os
import re
import subprocess

ADJACENT = 3
DIFF_FLAGS = (
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
)
HUNK_RE = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
# 12 | 12-20 | L12-L20 | 12..20 | 12 to 20 | 12:20, with an en or em dash too.
LINES_RE = re.compile(r"L?(\d+)(?:\s*(?:-|\u2013|\u2014|\.\.|to|:)\s*L?(\d+))?", re.I)


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def git(proj, *args):
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


def diff_args(spec, resolved):
    """The same diff facts.py resolved (see its diff_text)."""
    if not resolved or resolved.startswith("-"):
        raise ValueError(f"unusable diff spec {resolved!r}")
    if resolved == "staged":
        return ["--cached"]
    if resolved == "worktree":
        return ["HEAD"] if spec == "worktree" else []
    return [resolved]


def parse_hunks(text):
    """path -> [(old_start, old_len, new_start, new_len)] from a -U0 diff."""
    hunks, path, old_path = {}, None, None
    for line in text.splitlines():
        if line.startswith("--- "):
            old_path = line[6:] if line.startswith("--- a/") else None
        elif line.startswith("+++ "):
            path = line[6:] if line.startswith("+++ b/") else old_path
            hunks.setdefault(path, [])
        elif path and (m := HUNK_RE.match(line)):
            a, b, c, d = m.groups()
            hunks[path].append(
                (int(a), 1 if b is None else int(b), int(c), 1 if d is None else int(d))
            )
    return hunks


def normalize(raw, reviewed, top):
    path = str(raw or "").strip().strip("`'\"").strip()
    path = re.sub(r":\d+(?:[-:]\d+)?$", "", path)
    if top and os.path.isabs(path):
        try:
            rel = os.path.relpath(os.path.realpath(path), os.path.realpath(top))
            if not rel.startswith(".."):
                path = rel
        except ValueError:
            pass
    while path.startswith("./"):
        path = path[2:]
    if path in reviewed:
        return path
    for prefix in ("a/", "b/"):
        if path.startswith(prefix) and path[2:] in reviewed:
            return path[2:]
    candidates = [
        f for f in reviewed if f.endswith("/" + path) or (path.endswith("/" + f))
    ]
    return candidates[0] if len(candidates) == 1 else None


def line_count(top, path):
    full = os.path.join(top, path)
    if not os.path.isfile(full):
        return None
    with open(full, "rb") as f:
        data = f.read()
    return data.count(b"\n") + (0 if not data or data.endswith(b"\n") else 1)


def anchor_reason(path, start, end, hunks, length):
    """None when anchored, else why the range is not."""
    if start < 1 or (length is not None and end > length):
        return f"lines {start}-{end} are outside {path} ({length} lines)"
    for old_start, old_len, new_start, new_len in hunks.get(path, []):
        if length is not None:
            lo = new_start
            hi = new_start + new_len - 1 if new_len else new_start
            if start <= hi + ADJACENT and end >= lo - ADJACENT:
                return None
        if old_len:
            lo, hi = old_start, old_start + old_len - 1
            if start <= hi + ADJACENT and end >= lo - ADJACENT:
                return None
    return f"lines {start}-{end} are not within {ADJACENT} lines of a change in {path}"


def check(finding, reviewed, generated, hunks, top):
    raw = finding.get("file")
    path = normalize(raw, reviewed, top)
    if path is None:
        if normalize(raw, generated, top) is not None:
            return None, f"{raw} is a generated file, not reviewed"
        return None, f"{raw or '(no file)'} is not a changed file of this diff"
    m = LINES_RE.search(str(finding.get("lines") or ""))
    if not m:
        return path, f"no line range to anchor in {path}"
    start = int(m.group(1))
    end = int(m.group(2) or start)
    if end < start:
        start, end = end, start
    return path, anchor_reason(path, start, end, hunks, line_count(top, path))


def main():
    state = load_state()
    findings = [f for f in (state.get("findings") or []) if isinstance(f, dict)]
    proj = os.path.expanduser(state.get("project_dir") or ".")
    reviewed = set(state.get("changed_files") or [])
    generated = set(state.get("generated_files") or [])
    top = git(proj, "rev-parse", "--show-toplevel").strip()
    args = diff_args(
        (state.get("diff_spec") or "auto").strip(), state.get("resolved_diff_spec") or ""
    )
    hunks = parse_hunks(git(proj, "diff", *DIFF_FLAGS, "-U0", *args, "--"))

    kept, unanchored = [], []
    for f in findings:
        path, reason = check(f, reviewed, generated, hunks, top)
        if reason is None:
            kept.append({**f, "file": path})
        else:
            unanchored.append(
                {
                    "id": f.get("id", ""),
                    "severity": f.get("severity", ""),
                    "title": f.get("title", ""),
                    "file": f.get("file", ""),
                    "lines": f.get("lines", ""),
                    "reason": reason,
                }
            )
    print(
        json.dumps(
            {"findings": kept, "unanchored": unanchored, "anchor_checked": True, "anchor_error": ""}
        )
    )


try:
    main()
except Exception as e:  # noqa: BLE001 - a crash must keep the findings and say so
    print(
        json.dumps(
            {
                "anchor_checked": True,
                "anchor_error": f"PIPELINE-FAULT: anchor check failed, findings are unanchored: {e}",
            }
        )
    )
