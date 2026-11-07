#!/usr/bin/env python3
"""Mechanical anchor check between synthesis and verification.

Every finding must point at code this diff changed. A finding is kept only
when its file is one of the reviewed changed files (relative to the
checkout) and one of its line ranges is anchored: on the new side, inside
the file and inside, or within ADJACENT lines of, a changed hunk; on the old
side, within ADJACENT lines of lines the diff deleted (a citation of removed
code). Anything else (a path outside the diff or outside the checkout, a
generated file, a line past the end of the file, a range far from every
change, no line range at all) is moved to `unanchored` with the reason, and
the report lists it under "Could not anchor" instead of as a finding. A
cross-repo consumer finding therefore anchors at the changed line in this
diff and cites the external caller as evidence.

Paths are normalized before matching: a leading `./`, `a/` or `b/`, an
absolute path under the checkout, a trailing `:line`, Windows separators
and MSYS drive paths, and a unique component-boundary suffix or prefix match
(`admin/x.go` or `billing/internal/admin/x.go` for `internal/admin/x.go`)
for relative paths only; the kept finding carries the normalized path. The
diff is read with the same git settings as facts.py: paths verbatim, renames
as a deletion plus an addition.

Fail-closed: `anchor_checked` is set only by a completed run, so verdict.py
can tell a gate that never ran from one that kept everything; an internal
error keeps every finding and records `anchor_error`, which verdict.py
reports as a pipeline fault.
"""

import json
import os
import re
import subprocess
import sys

# State, diffs and reports carry emoji and non-ASCII paths; never decode them
# with the platform default (cp1252 on Windows).
for _stream in (sys.stdin, sys.stdout, sys.stderr):
    if hasattr(_stream, "reconfigure"):
        _stream.reconfigure(encoding="utf-8", errors="replace")

ADJACENT = 3
GIT_CONFIG = ("-c", "core.quotePath=false", "-c", "diff.renames=false")
DIFF_FLAGS = (
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
)
HUNK_RE = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
# 12 | 12-20 | L12-L20 | 12..20 | 12 to 20 | 12:20, with an en or em dash too;
# several ranges may be listed ("10-12, 40").
LINES_RE = re.compile(r"L?(\d+)(?:\s*(?:-|\u2013|\u2014|\.\.|to|:)\s*L?(\d+))?", re.I)
OUTSIDE = object()


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def git(proj, *args):
    r = subprocess.run(
        ["git", *GIT_CONFIG, "-C", proj, *args],
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


def unquote_path(path):
    """git C-quotes a path holding a quote, backslash or control character."""
    if not (len(path) >= 2 and path.startswith('"') and path.endswith('"')):
        return path
    out, body, i = bytearray(), path[1:-1], 0
    simple = {"n": 10, "t": 9, "r": 13, "a": 7, "b": 8, "f": 12, "v": 11, '"': 34, "\\": 92}
    while i < len(body):
        ch = body[i]
        if ch == "\\" and i + 1 < len(body):
            nxt = body[i + 1]
            if nxt in simple:
                out.append(simple[nxt])
                i += 2
                continue
            if body[i + 1 : i + 4].isdigit():
                out.append(int(body[i + 1 : i + 4], 8))
                i += 4
                continue
        out.extend(ch.encode("utf-8"))
        i += 1
    return out.decode("utf-8", errors="replace")


def header_path(rest, prefix):
    """The path of a `--- a/x` / `+++ b/x` header body, or None for /dev/null.
    git appends a TAB to a name holding a space, and C-quotes odd names."""
    rest = rest.rstrip("\n").rstrip("\r")
    if rest.endswith("\t"):
        rest = rest[:-1]
    rest = unquote_path(rest)
    return rest[len(prefix) :] if rest.startswith(prefix) else None


def parse_hunks(text):
    """path -> [(old_start, old_len, new_start, new_len)] from a -U0 diff."""
    hunks, path, old_path = {}, None, None
    for line in text.splitlines():
        if line.startswith("--- "):
            old_path = header_path(line[4:], "a/")
        elif line.startswith("+++ "):
            path = header_path(line[4:], "b/") or old_path
            hunks.setdefault(path, [])
        elif path and (m := HUNK_RE.match(line)):
            a, b, c, d = m.groups()
            hunks[path].append(
                (int(a), 1 if b is None else int(b), int(c), 1 if d is None else int(d))
            )
    return hunks


def posix(path):
    """Windows separators to `/`, and an MSYS drive path (/c/x) to C:/x on
    Windows so it compares with git's own top-level path."""
    path = path.replace("\\", "/")
    if os.name == "nt" and (m := re.match(r"^/([A-Za-z])/(.*)$", path)):
        path = f"{m.group(1).upper()}:/{m.group(2)}"
    return path


def normalize(raw, reviewed, top):
    path = posix(str(raw or "").strip().strip("`'\"").strip())
    path = re.sub(r":\d+(?:[-:]\d+)?$", "", path)
    if os.path.isabs(path) or re.match(r"^[A-Za-z]:/", path):
        try:
            rel = os.path.relpath(os.path.realpath(path), os.path.realpath(top))
        except ValueError:
            return OUTSIDE
        rel = posix(rel)
        if rel == ".." or rel.startswith("../"):
            return OUTSIDE
        return rel if rel in reviewed else None
    while path.startswith("./"):
        path = path[2:]
    if path in reviewed:
        return path
    for prefix in ("a/", "b/"):
        if path.startswith(prefix) and path[2:] in reviewed:
            return path[2:]
    candidates = [f for f in reviewed if f.endswith("/" + path) or path.endswith("/" + f)]
    return candidates[0] if len(candidates) == 1 else None


def line_count(top, path):
    full = os.path.join(top, path)
    if not os.path.isfile(full):
        return None
    with open(full, "rb") as f:
        data = f.read()
    return data.count(b"\n") + (0 if not data or data.endswith(b"\n") else 1)


def ranges_of(lines):
    out = []
    for m in LINES_RE.finditer(str(lines or "")):
        start = int(m.group(1))
        end = int(m.group(2) or start)
        out.append((min(start, end), max(start, end)))
    return out


def near(start, end, lo, hi):
    return start <= hi + ADJACENT and end >= lo - ADJACENT


def range_anchored(start, end, file_hunks, length):
    """(anchored, past_end) for one cited range."""
    past_end = length is not None and (start < 1 or end > length)
    for old_start, old_len, new_start, new_len in file_hunks:
        if length is not None and not past_end:
            hi = new_start + new_len - 1 if new_len else new_start
            if near(start, end, new_start, hi):
                return True, past_end
        # Deleted lines are cited by their old-side numbers, which the
        # current file length says nothing about.
        if old_len and near(start, end, old_start, old_start + old_len - 1):
            return True, past_end
    return False, past_end


def check(finding, reviewed, generated, hunks, top):
    raw = finding.get("file")
    path = normalize(raw, reviewed, top)
    if path is OUTSIDE:
        return None, f"{raw} cites a file outside the review checkout"
    if path is None:
        if normalize(raw, generated, top) not in (None, OUTSIDE):
            return None, f"{raw} is a generated file, not reviewed"
        return None, f"{raw or '(no file)'} is not a changed file of this diff"
    ranges = ranges_of(finding.get("lines"))
    if not ranges:
        return path, f"no line range to anchor in {path}"
    length = line_count(top, path)
    results = [range_anchored(s, e, hunks.get(path, []), length) for s, e in ranges]
    if any(ok for ok, _ in results):
        return path, None
    cited = ", ".join(f"{s}-{e}" for s, e in ranges)
    if all(past for _, past in results):
        return path, f"lines {cited} are outside {path} ({length} lines)"
    return path, f"lines {cited} are not within {ADJACENT} lines of a change in {path}"


def main():
    state = load_state()
    findings = [f for f in (state.get("findings") or []) if isinstance(f, dict)]
    if not findings:
        print(json.dumps({"findings": [], "unanchored": [], "anchor_checked": True, "anchor_error": ""}))
        return
    proj = os.path.expanduser(state.get("project_dir") or ".")
    reviewed = set(state.get("changed_files") or [])
    generated = set(state.get("generated_files") or [])
    top = posix(git(proj, "rev-parse", "--show-toplevel").strip())
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
