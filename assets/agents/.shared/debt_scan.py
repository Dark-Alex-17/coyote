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


def run_git(args, cwd):
    return subprocess.run(
        ["git"] + args, cwd=cwd, capture_output=True, text=True, timeout=120
    )


def scan(project_dir, base):
    # Intent-to-add the untracked files so the diff covers them (same side
    # effect as the sanctioned pipeline; .gitignore keeps artefacts out).
    ls = run_git(["ls-files", "--others", "--exclude-standard", "-z"], project_dir)
    if ls.returncode != 0:
        raise RuntimeError(f"git ls-files failed: {ls.stderr.strip()}")
    untracked = [p for p in ls.stdout.split("\0") if p]
    if untracked:
        add = run_git(["add", "-N", "--"] + untracked, project_dir)
        if add.returncode != 0:
            raise RuntimeError(f"git add -N failed: {add.stderr.strip()}")

    # quotePath=false: a path with spaces or non-ASCII bytes comes out literal, not
    # as git's octal-escaped quoted form, so the hit names the real file.
    diff = run_git(["-c", "core.quotePath=false", "diff", "--no-color", "--no-ext-diff", base], project_dir)
    if diff.returncode not in (0, 1):
        raise RuntimeError(f"git diff {base} failed: {diff.stderr.strip()}")

    hits = []
    path = None
    new_line = 0
    for raw in diff.stdout.splitlines():
        if raw.startswith("+++ "):
            target = raw[4:].strip()
            if target.startswith('"') and target.endswith('"'):
                target = target[1:-1]
            path = None if target == "/dev/null" else re.sub(r"^b/", "", target)
            continue
        if raw.startswith("@@"):
            m = re.match(r"^@@ -\S+ \+(\d+)(?:,\d+)? @@", raw)
            if m:
                new_line = int(m.group(1))
            continue
        if path is None:
            continue
        if raw.startswith("+") and not raw.startswith("+++"):
            text = raw[1:]
            m = MARKER_RE.search(text)
            if m:
                # Bare cfg_attr( carrying neither allow nor expect is not debt.
                if m.group(1) == "cfg_attr(" and not re.search(r"(allow|expect)\(", text):
                    pass
                else:
                    hits.append(f"{path}:{new_line} -- {text.strip()}")
            new_line += 1
        elif raw.startswith("-") and not raw.startswith("---"):
            continue
        elif raw.startswith("\\"):  # "\ No newline at end of file"
            continue
        else:
            new_line += 1

    header = f"Debt scan (diff base: {base}): "
    if hits:
        return header + f"{len(hits)} hit(s)\n" + "\n".join(hits)
    return header + "no hits (clean)"


def main():
    state = {}
    graph_mode = False
    state_file = os.environ.get("GRAPH_STATE_FILE")
    state_raw = os.environ.get("GRAPH_STATE")
    if state_file and os.path.exists(state_file):
        graph_mode = True
        with open(state_file, encoding="utf-8") as fh:
            state = json.load(fh)
    elif state_raw:
        graph_mode = True
        state = json.loads(state_raw)

    if graph_mode:
        project_dir = state.get("project_dir") or "."
        base = state.get("debt_base") or "HEAD"
    else:
        project_dir, base = ".", "HEAD"
        args = sys.argv[1:]
        while args:
            flag = args.pop(0)
            if flag in ("--dir", "--project-dir") and args:
                project_dir = args.pop(0)
            elif flag == "--base" and args:
                base = args.pop(0)

    try:
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
