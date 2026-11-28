"""Failure locations in a test command's output.

A failure is identified by the repo paths it resolves to (a directory ends in
"/"), so a gate that records failures as carried and the adversary that
matches a red run against them key on the same evidence. Pure regex over the
output plus one `git ls-files` for the repo index; no LLM.
"""
import os
import re
import subprocess

FAILURE_FILE_RE = re.compile(r"(?<![\w.~@+/-])((?:\.\.?/|/)?[\w.~@+-]+(?:/[\w.~@+-]+)*\.[A-Za-z]\w*):\d+")
FAILURE_SLASH_RE = re.compile(r"[\w.~@+-]*/[\w./~@+-]*")
FAILURE_WORD_RE = re.compile(r"\b(?:fail|failed|failure|error)\b", re.I)
FAILURE_LINE_CAP = 4096
TRIMMED_MARK = "bytes trimmed: output exceeded"
RUNNER_BANNER_RE = re.compile(r"\S*: \*\*\* \[[^\]]*\] Error \d+\s*$")
GIT_SECS = 60


class RepoIndex(tuple):
    """(files, dirs), plus a basename -> files map and a per-token memo built on first use, so resolving many tokens
    against one index scans only the files sharing a token's basename."""

    def lookups(self):
        if not hasattr(self, "by_name"):
            self.by_name, self.memo = {}, {}
            for f in self[0]:
                self.by_name.setdefault(f.rsplit("/", 1)[-1], []).append(f)
        return self.by_name, self.memo


def parse_failure_locations(tests):
    """(token, is_file, line) for every path:LINE token, and every slash token on a line carrying a failure word."""
    out = []
    for line in tests.splitlines():
        line = line[:FAILURE_LINE_CAP]
        if re.match(r"(?:Ran|Ran in|Exit code):", line):
            continue
        out += [(t, True, line) for t in FAILURE_FILE_RE.findall(line)]
        if FAILURE_WORD_RE.search(line):
            out += [(t, False, line) for t in FAILURE_SLASH_RE.findall(line)]
    return out


def repo_index(root, timeout=GIT_SECS):
    """(files, dirs) git knows in the worktree, untracked included, or None when git cannot list them."""
    try:
        p = subprocess.run(["git", "-C", root, "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
                           capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired):
        return None
    if p.returncode != 0:
        return None
    files = {n for n in p.stdout.split("\0") if n.strip()}
    return RepoIndex((files, {f.rsplit("/", i)[0] for f in files for i in range(1, f.count("/") + 1)}))


def resolve_failure_tokens(root, tokens, index=None):
    """[{"paths", "evidence"}] for the tokens that name repo paths (a directory ends in "/"), one per distinct set of
    paths; tokens naming nothing in the repo (toolchain frames, paths outside it) are dropped."""
    index = index or repo_index(root)
    if index is None:
        return []
    index = index if isinstance(index, RepoIndex) else RepoIndex(index)
    files, dirs = index
    by_name, memo = index.lookups()
    prefix = os.path.realpath(root).rstrip("/") + "/"
    found, seen = [], set()
    for token, is_file, line in tokens:
        t = token.removeprefix(prefix).removeprefix(root.rstrip("/") + "/").rstrip("/").removesuffix("/...")
        while t.startswith("./"):
            t = t[2:]
        if (t, is_file) not in memo:
            if is_file:
                same_name = by_name.get(t.rsplit("/", 1)[-1], [])
                memo[t, is_file] = sorted(f for f in same_name if f == t or f.endswith("/" + t))
            else:
                parts = [p for p in t.split("/") if p not in ("", ".")]
                suffixes = ("/".join(parts[i:]) for i in range(len(parts)))
                memo[t, is_file] = next(([s] if s in files else [s + "/"] for s in suffixes
                                         if s in files or s in dirs), [])
        paths = memo[t, is_file]
        if paths and tuple(paths) not in seen:
            seen.add(tuple(paths))
            found.append({"paths": list(paths), "evidence": line.strip()[:200]})
    return found


def unlocated_failure_lines(root, tests, index):
    """How many failure-word lines name no repo path, on the line or on the more-indented lines under it; a line of
    marker words alone (a bare FAIL) or a runner's exit banner (`runner: *** [check] Error 1`) is not counted."""
    lines = [l[:FAILURE_LINE_CAP] for l in tests.splitlines()]
    count = 0
    for i, line in enumerate(lines):
        words = re.findall(r"[A-Za-z]+", line)
        if (re.match(r"(?:Ran|Ran in|Exit code):", line) or not FAILURE_WORD_RE.search(line)
                or all(FAILURE_WORD_RE.fullmatch(w) for w in words) or RUNNER_BANNER_RE.match(line)):
            continue
        depth, group = len(line) - len(line.lstrip()), [line]
        for follow in lines[i + 1:]:
            if not follow.strip() or len(follow) - len(follow.lstrip()) <= depth:
                break
            group.append(follow)
        tokens = [(t, True, l) for l in group for t in FAILURE_FILE_RE.findall(l)]
        tokens += [(t, False, l) for l in group for t in FAILURE_SLASH_RE.findall(l)]
        if not resolve_failure_tokens(root, tokens, index):
            count += 1
    return count


def failure_key(paths):
    return tuple(str(p) for p in (paths if isinstance(paths, list) else []))
