#!/usr/bin/env python3
"""Facts select_checks picks this lap's tests from.

The changed paths since the base (and which of them are tests), the test
files that mention a changed file's stem, the test files beside each changed
file, the full test command with the base suite's duration, and the previous
lap's selection and result. Every fact is additive: one git cannot produce
becomes a one-line note and the others still print. Each section keeps its
header and is cut to MAX_ITEMS lines of MAX_LINE_CHARS with a count of the
rest. Nothing here knows a language or a test runner: a test is a path
diff_signals.TESTS_RE matches, and a mention is a whole-word match of the
stem.
"""

import json
import os
import re
import subprocess
import sys

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
SHARED = os.path.join(HERE, "..", "..", ".shared")
sys.path.insert(0, SHARED)

from base_suite import bash_program, find_record  # noqa: E402
from diff_signals import TESTS_RE  # noqa: E402

MAX_ITEMS = 20
MAX_LINE_CHARS = 300
GIT_SECS = 30


def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


def gate_facts(state):
    """(directory the gates run in, the full test command as they run it), from gate_lib.

    The state goes through the environment because Git Bash on Windows
    re-parses its command line and mangles the quotes and backslashes of JSON."""
    script = (
        'source "$1"; source "$2"; dir=$(gate_workdir "$IMPACT_FACTS_STATE"); '
        'cmd=$(gate_test_command_for_run "$(gate_test_command "$dir")"); '
        'jq -nc --arg dir "$dir" --arg cmd "$cmd" \'{dir: $dir, cmd: $cmd}\''
    )
    r = subprocess.run(
        [bash_program(), "-c", script, "impact-facts", os.path.join(SHARED, "utils.sh"),
         os.path.join(HERE, "gate_lib.sh")],
        capture_output=True, encoding="utf-8", errors="replace", stdin=subprocess.DEVNULL,
        env={**os.environ, "IMPACT_FACTS_STATE": json.dumps(state)},
    )
    if r.returncode != 0:
        raise RuntimeError(r.stderr.strip()[:300] or f"gate_lib exited {r.returncode}")
    facts = json.loads(r.stdout)
    return facts["dir"], facts["cmd"]


def git(root, *args):
    r = subprocess.run(
        ["git", "-C", root, *args],
        capture_output=True, encoding="utf-8", errors="replace", timeout=GIT_SECS,
        stdin=subprocess.DEVNULL,
    )
    if r.returncode != 0:
        raise RuntimeError(f"git {args[0]} failed: {(r.stderr or r.stdout).strip()[:300]}")
    return [line for line in r.stdout.splitlines() if line]


def is_test(path):
    return re.search(TESTS_RE, path, re.I) is not None


def section(header, items, empty="none"):
    lines = [header]
    if not items:
        lines.append(f"  {empty}")
    for item in items[:MAX_ITEMS]:
        lines.append("  " + item[:MAX_LINE_CHARS])
    if len(items) > MAX_ITEMS:
        lines.append(f"  (+{len(items) - MAX_ITEMS} more)")
    return "\n".join(lines)


def failed(header, error):
    return f"{header}\n  unavailable: {str(error)[:MAX_LINE_CHARS]}"


def changed_paths(root, base):
    """Paths relative to `root` that differ from `base` (tracked) or are new and not ignored."""
    tracked = git(root, "diff", "--name-only", "--relative", "--no-renames", "--no-ext-diff", base, "--")
    untracked = git(root, "ls-files", "--others", "--exclude-standard")
    return sorted(set(tracked) | set(untracked))


def mentions(root, path):
    stem = os.path.splitext(os.path.basename(path))[0]
    if not stem:
        return []
    r = subprocess.run(
        ["git", "-C", root, "grep", "-l", "-F", "-w", "--untracked", "-e", stem, "--"],
        capture_output=True, encoding="utf-8", errors="replace", timeout=GIT_SECS,
        stdin=subprocess.DEVNULL,
    )
    if r.returncode not in (0, 1):
        raise RuntimeError(f"git grep failed: {r.stderr.strip()[:300]}")
    return [f for f in r.stdout.splitlines() if f and f != path and is_test(f)]


def neighbours(root, directory):
    pattern = f":(glob){directory}/*" if directory else ":(glob)*"
    files = git(root, "ls-files", "--cached", "--others", "--exclude-standard", "--", pattern)
    return sorted({f for f in files if is_test(f)})


def baseline_facts(root, state):
    key = state.get("baseline_key") or ""
    mode = state.get("baseline_mode") or "none"
    lines = [f"baseline mode: {mode}"]
    if key:
        record, _ = find_record(root, key)
        if record:
            with open(record, encoding="utf-8") as f:
                stored = json.load(f)
            duration = stored.get("duration_s")
            lines.append(f"base suite {stored.get('status')}, "
                         + (f"took {duration} s" if duration is not None else "duration not recorded"))
        else:
            lines.append("base suite has not finished, duration unknown")
    return lines


def previous_lap(state):
    if not str(state.get("tests_tier") or "").startswith("round"):
        return []
    picked = [f"targeted: {json.dumps(state.get('targeted') or [])}",
              f"impacted: {json.dumps(state.get('impacted') or [])}",
              f"reason: {state.get('reason') or ''}",
              f"result: {state.get('tests_verdict') or ''}"]
    return [" ".join(p.split()) for p in picked]


def main():
    state = load_state()
    parts = []
    try:
        root, cmd = gate_facts(state)
    except (OSError, RuntimeError, ValueError, KeyError) as e:
        root, cmd = state.get("project_dir") or ".", ""
        parts.append(failed("Gate directory and full test command:", e))

    base = state.get("baseline_base") or "HEAD"
    changed = []
    try:
        changed = changed_paths(root, base)
        parts.append(section(f"Changed paths since {base[:12]} (tests marked [test]):",
                             [f"{p} [test]" if is_test(p) else p for p in changed]))
    except (OSError, RuntimeError, subprocess.SubprocessError) as e:
        parts.append(failed("Changed paths:", e))

    sources = [p for p in changed if not is_test(p)]
    for path in sources[:MAX_ITEMS]:
        try:
            parts.append(section(f"Test files mentioning `{os.path.splitext(os.path.basename(path))[0]}` "
                                 f"(stem of {path}):", mentions(root, path)))
        except (OSError, RuntimeError, subprocess.SubprocessError) as e:
            parts.append(failed(f"Test files mentioning the stem of {path}:", e))
    if len(sources) > MAX_ITEMS:
        parts.append(f"Stems not searched: {len(sources) - MAX_ITEMS} more changed files")

    directories = sorted({os.path.dirname(p) for p in changed})
    for directory in directories[:MAX_ITEMS]:
        try:
            parts.append(section(f"Test files in {directory or '.'}/:", neighbours(root, directory)))
        except (OSError, RuntimeError, subprocess.SubprocessError) as e:
            parts.append(failed(f"Test files in {directory or '.'}/:", e))
    if len(directories) > MAX_ITEMS:
        parts.append(f"Directories not listed: {len(directories) - MAX_ITEMS} more")

    try:
        parts.append(section("Full test command and base suite:",
                             [f"command: {cmd or '(none configured or detected)'}", f"runs in: {root}"]
                             + baseline_facts(root, state)))
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as e:
        parts.append(failed("Full test command and base suite:", e))

    parts.append(section("Previous lap's selection and result:", previous_lap(state), empty="none (first lap)"))
    print(json.dumps({"impact_facts": "\n\n".join(parts), "_next": "select_checks"}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
