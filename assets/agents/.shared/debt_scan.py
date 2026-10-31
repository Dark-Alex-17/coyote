#!/usr/bin/env python3
"""Deterministic debt-marker scan over a working-tree diff.

Replaces the hand-run pipeline both sisyphus (instructions "Debt and
decisions after a coder returns") and step-runner (edge_case_sweep step 5)
previously asked the MODEL to execute and post-process:

    git ls-files --others --exclude-standard -z | xargs -0 -r git add -N -- \
      && git diff --no-color --no-ext-diff --no-textconv --no-relative <base> -- \
      | grep -nE '^\\+.*(#!?\\[(allow|expect)\\(|cfg_attr\\(|todo!|unimplemented!|#\\[ignore)'

Improvements over the LLM procedure, with identical match semantics:
- real `<file>:<line>` positions come from the diff hunk headers directly -
  no per-hit fs_grep resolution round-trips, no regex-escaping hazards;
- the "bare cfg_attr( without allow|expect is not debt" rule is applied
  mechanically;
- "keep only matches in files the diff touched" is inherent (we parse the
  diff itself).

Judgment stays with the model: owner assignment, the env-gated/opt-in-test
addition, and all routing rules are untouched.

Fail-open contract: on ANY failure this prints a report starting with
`DEBT_SCAN_FAILED:` that quotes the manual pipeline, and exits 0 - callers
(a graph script node, an argc tool) degrade to today's manual behavior
instead of dying.

Modes:
- Graph mode (no command-line arguments and GRAPH_STATE/GRAPH_STATE_FILE
  set): reads `project_dir` and optional `debt_base` from state; prints
  `{"debt_hits": "<report>"}`.
- CLI mode (any command-line argument, or no graph state):
  `debt_scan.py [--dir DIR] [--base REF] [--]`; prints the report. An unknown
  flag, a flag without its value, or an option-shaped base is a scan failure.

The `git add -N` intent-to-add side effect is deliberate and identical to
the sanctioned pipeline: it is what puts the coder's new files into the
diff. (Known, pre-existing consequence: `git stash -u` dislikes
intent-to-add entries.)
"""

import json
import os
import re
import subprocess
import sys

MARKER_RE = re.compile(r"(#!?\[(allow|expect)\(|cfg_attr\(|todo!|unimplemented!|#\[ignore)")
MANUAL_PIPELINE = (
    "git ls-files --others --exclude-standard -z | xargs -0 -r git add -N -- "
    "&& git diff --no-color --no-ext-diff --no-textconv --no-relative <base> -- | grep -nE "
    "'^\\+.*(#!?\\[(allow|expect)\\(|cfg_attr\\(|todo!|unimplemented!|#\\[ignore)'"
)
# Per git call; three calls run back to back, so the sum stays inside the
# graph node's own budget instead of equalling it.
GIT_TIMEOUT = 30
HUNK_HEADER_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")
# The `+++ ` parsing below only knows the a/ b/ prefix form; these pin it
# regardless of diff.noprefix / diff.mnemonicPrefix / diff.external in the
# user's config. --no-relative keeps paths root-relative when diff.relative is
# set and the project dir is a subdirectory.
DIFF_FLAGS = [
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
]
# Only `git diff` with no revision prints these during an unresolved merge;
# their multi-parent hunks do not number lines the way the parser counts them.
COMBINED_MARKERS = ("diff --cc ", "diff --combined ", "@@@")
# Twin of debris.py's helpers (this script runs via runpy from shims, so it
# cannot import debris.py); a test keeps the two copies identical.
C_ESCAPE_RE = re.compile(r'\\([0-3][0-7]{2}|[abtnvfr"\\])')
C_ESCAPES = {"a": 7, "b": 8, "t": 9, "n": 10, "v": 11, "f": 12, "r": 13, '"': 34, "\\": 92}
CONTROL_CHAR_RE = re.compile(r"[\x00-\x08\x0a-\x1f\x7f-\x9f\u2028\u2029]")


def unquote_c(name):
    """Decode git's C-quoted path form (`\\303\\251`, `\\"`, `\\\\`, `\\t`) to the name on disk."""
    out = bytearray()
    pos = 0
    for m in C_ESCAPE_RE.finditer(name):
        out += name[pos : m.start()].encode("utf-8")
        esc = m.group(1)
        out.append(int(esc, 8) if len(esc) == 3 else C_ESCAPES[esc])
        pos = m.end()
    out += name[pos:].encode("utf-8")
    return out.decode("utf-8", errors="replace")


def display_path(path):
    """The path with control characters removed, so a name carrying a newline
    or an escape sequence never splits or colours a report row."""
    return CONTROL_CHAR_RE.sub("", path)


def run_git(args, cwd):
    return subprocess.run(
        ["git"] + args, cwd=cwd, capture_output=True, timeout=GIT_TIMEOUT
    )


def text_of(raw):
    return raw.decode("utf-8", errors="replace")


def is_debt(text):
    # A bare cfg_attr( carrying neither allow nor expect is not debt, but it
    # must not hide a later marker on the same line.
    for m in MARKER_RE.finditer(text):
        if m.group(1) != "cfg_attr(" or re.search(r"(allow|expect)\(", text):
            return True
    return False


def parse_diff(diff_text):
    """The `<file>:<line> -- <text>` hits for the added debt lines of a two-way diff."""
    # File headers are only recognised outside a hunk, so an added line `++ x`
    # (rendered `+++ x`) is read as content, not as a new target path.
    hits = []
    path = None
    target_seen = False
    in_hunk = False
    new_line = 0
    for raw in diff_text.split("\n"):
        line = raw[:-1] if raw.endswith("\r") else raw
        if line.startswith(COMBINED_MARKERS):
            raise RuntimeError(
                f"{line[:60]!r} is a combined (merge) diff; resolve the merge before scanning"
            )
        if line.startswith("diff --git "):
            path, target_seen, in_hunk = None, False, False
            continue
        if m := HUNK_HEADER_RE.match(line):
            if not target_seen:
                raise RuntimeError(
                    f"hunk {line[:60]!r} arrived before any `+++ b/<path>` header; "
                    "the diff is not in the a/ b/ prefix form this scan parses"
                )
            new_line = int(m.group(1))
            in_hunk = True
            continue
        if not in_hunk:
            if line.startswith("+++ "):
                target_seen = True
                target = line[4:].strip()
                if target.startswith('"') and target.endswith('"'):
                    target = unquote_c(target[1:-1])
                path = None if target == "/dev/null" else display_path(re.sub(r"^b/", "", target))
            continue
        if line.startswith("+"):
            text = line[1:]
            if path is not None and is_debt(text):
                hits.append(f"{path}:{new_line} -- {text.strip()}")
            new_line += 1
        elif line.startswith(" ") or line == "":
            new_line += 1
    return hits


def scan(project_dir, base):
    if base.startswith("-"):
        raise ValueError(f"diff base {base!r} is option-shaped; pass a revision or a range")
    # Intent-to-add the untracked files so the diff covers them (same side
    # effect as the sanctioned pipeline; .gitignore keeps artefacts out).
    ls = run_git(["ls-files", "--others", "--exclude-standard", "-z"], project_dir)
    if ls.returncode != 0:
        raise RuntimeError(f"git ls-files failed: {text_of(ls.stderr).strip()}")
    # fsdecode round-trips undecodable path bytes (surrogateescape), so the
    # pathspec handed back to git is the one ls-files printed.
    untracked = [os.fsdecode(p) for p in ls.stdout.split(b"\0") if p]
    if untracked:
        add = run_git(["add", "-N", "--"] + untracked, project_dir)
        if add.returncode != 0:
            raise RuntimeError(f"git add -N failed: {text_of(add.stderr).strip()}")

    # quotePath=false: a path with spaces or non-ASCII bytes comes out literal, not
    # as git's octal-escaped quoted form, so the hit names the real file.
    diff = run_git(["-c", "core.quotePath=false", "diff"] + DIFF_FLAGS + [base, "--"], project_dir)
    if diff.returncode not in (0, 1):
        raise RuntimeError(f"git diff {base} failed: {text_of(diff.stderr).strip()}")

    hits = parse_diff(text_of(diff.stdout))
    header = f"Debt scan (diff base: {base}): "
    if hits:
        return header + f"{len(hits)} hit(s)\n" + "\n".join(hits)
    return header + "no hits (clean)"


def parse_args(args):
    project_dir, base = ".", "HEAD"
    args = list(args)
    while args:
        flag = args.pop(0)
        if flag == "--":
            break
        if flag not in ("--dir", "--project-dir", "--base"):
            raise ValueError(f"unknown argument {flag!r}; usage: debt_scan.py [--dir DIR] [--base REF]")
        if not args:
            raise ValueError(f"{flag} needs a value")
        if flag == "--base":
            base = args.pop(0)
        else:
            project_dir = args.pop(0)
    if args:
        raise ValueError(f"unexpected argument(s) after --: {args!r}")
    return project_dir, base


def main():
    # The report is UTF-8 with LF line ends whatever the host locale (the Rust
    # side decodes it as UTF-8), and an unencodable glyph is escaped rather
    # than a traceback.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="backslashreplace", newline="\n")
    # Any command-line argument means CLI mode, so a tool call inheriting a
    # graph node's state still honours its flags. Without arguments the
    # environment decides, so a state file that fails to load still reports
    # through the graph-shaped JSON envelope.
    state_file = os.environ.get("GRAPH_STATE_FILE")
    state_raw = os.environ.get("GRAPH_STATE")
    graph_mode = not sys.argv[1:] and (
        bool(state_file and os.path.exists(state_file)) or bool(state_raw)
    )
    project_dir, base = ".", "HEAD"
    try:
        if graph_mode:
            if state_file and os.path.exists(state_file):
                with open(state_file, encoding="utf-8") as fh:
                    state = json.load(fh)
            else:
                state = json.loads(state_raw)
            project_dir = state.get("project_dir") or "."
            base = state.get("debt_base") or "HEAD"
        else:
            project_dir, base = parse_args(sys.argv[1:])
        report = scan(project_dir, base)
    except Exception as exc:  # fail-open by contract
        report = (
            f"DEBT_SCAN_FAILED: {exc}\n"
            f"Manual fallback (run it yourself, <base> = {base}):\n"
            f"  {MANUAL_PIPELINE}\n"
            "grep exiting 1 with no lines is the no-debt result; -n numbers the DIFF "
            "output, so resolve each hit to its real <file:line> with the fs_grep tool: "
            "the hit text after the + is its pattern (regex metacharacters escaped) -- "
            "never paste diff text into a shell string -- "
            "and keep only matches in files the diff touched; "
            "a bare cfg_attr( without allow|expect is not debt."
        )

    if graph_mode:
        print(json.dumps({"debt_hits": report}))
    else:
        print(report)


if __name__ == "__main__":
    main()
