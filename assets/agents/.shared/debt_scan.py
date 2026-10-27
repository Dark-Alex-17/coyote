#!/usr/bin/env python3
"""Deterministic debt-marker scan over a working-tree diff.

Replaces the hand-run pipeline both sisyphus (instructions "Debt and
decisions after a coder returns") and step-runner (edge_case_sweep step 5)
previously asked the MODEL to execute and post-process:

    git ls-files --others --exclude-standard -z | xargs -0 -r git add -N -- \
      && git diff --no-color --no-ext-diff <base> \
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
- Graph mode (GRAPH_STATE/GRAPH_STATE_FILE set): reads `project_dir` and
  optional `debt_base` from state; prints `{"debt_hits": "<report>"}`.
- CLI mode: `debt_scan.py [--dir DIR] [--base REF]`; prints the report.

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
    "&& git diff --no-color --no-ext-diff <base> | grep -nE "
    "'^\\+.*(#!?\\[(allow|expect)\\(|cfg_attr\\(|todo!|unimplemented!|#\\[ignore)'"
)
# Per git call; three calls run back to back, so the sum stays inside the
# graph node's own budget instead of equalling it.
GIT_TIMEOUT = 30
HUNK_HEADER_RE = re.compile(r"^@@ -\S+ \+(\d+)(?:,\d+)? @@")


def run_git(args, cwd):
    return subprocess.run(
        ["git"] + args, cwd=cwd, capture_output=True, timeout=GIT_TIMEOUT
    )


def text_of(raw):
    return raw.decode("utf-8", errors="replace")


def scan(project_dir, base):
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
    diff = run_git(["-c", "core.quotePath=false", "diff", "--no-color", "--no-ext-diff", base], project_dir)
    if diff.returncode not in (0, 1):
        raise RuntimeError(f"git diff {base} failed: {text_of(diff.stderr).strip()}")

    # File headers are only recognised outside a hunk, so an added `++x` line
    # (rendered `+++x`) never rewrites the path and a removed `--y` line
    # (rendered `---y`) never shifts the numbering of the lines after it.
    hits = []
    path = None
    in_hunk = False
    new_line = 0
    for raw in text_of(diff.stdout).split("\n"):
        line = raw[:-1] if raw.endswith("\r") else raw
        if line.startswith("diff --git "):
            path, in_hunk = None, False
            continue
        if m := HUNK_HEADER_RE.match(line):
            new_line = int(m.group(1))
            in_hunk = True
            continue
        if not in_hunk:
            if line.startswith("+++ "):
                target = line[4:].strip()
                if target.startswith('"') and target.endswith('"'):
                    target = target[1:-1]
                path = None if target == "/dev/null" else re.sub(r"^b/", "", target)
            continue
        if line.startswith("+"):
            text = line[1:]
            m = MARKER_RE.search(text) if path is not None else None
            if m:
                # Bare cfg_attr( carrying neither allow nor expect is not debt.
                if m.group(1) == "cfg_attr(" and not re.search(r"(allow|expect)\(", text):
                    pass
                else:
                    hits.append(f"{path}:{new_line} -- {text.strip()}")
            new_line += 1
        elif line.startswith(" ") or line == "":
            new_line += 1

    header = f"Debt scan (diff base: {base}): "
    if hits:
        return header + f"{len(hits)} hit(s)\n" + "\n".join(hits)
    return header + "no hits (clean)"


def main():
    # The report is UTF-8 with LF line ends whatever the host locale (the Rust
    # side decodes it as UTF-8), and an unencodable glyph is escaped rather
    # than a traceback.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="backslashreplace", newline="\n")
    # Graph mode is decided from the environment alone, so a state file that
    # fails to load still reports through the graph-shaped JSON envelope.
    state_file = os.environ.get("GRAPH_STATE_FILE")
    state_raw = os.environ.get("GRAPH_STATE")
    graph_mode = bool(state_file and os.path.exists(state_file)) or bool(state_raw)
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
            args = sys.argv[1:]
            while args:
                flag = args.pop(0)
                if flag in ("--dir", "--project-dir") and args:
                    project_dir = args.pop(0)
                elif flag == "--base" and args:
                    base = args.pop(0)
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
