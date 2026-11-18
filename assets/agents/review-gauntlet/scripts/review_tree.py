"""The worktree as a git tree, and the paths that differ between two trees.

signals.py records the tree before the lanes run and verdict_gate.py
compares it with the tree after them, so the files a lane wrote can be
named in the report. The tree is written from a temporary copy of the
index, so the real index is never touched, and it includes untracked files
that are not ignored.

Each public call shares one deadline of TREE_SECS across all its git
commands, so a slow repository costs the calling node at most that long.
"""

import os
import shutil
import subprocess
import tempfile
import time

TREE_SECS = 40


class _Deadline:
    def __init__(self, secs):
        self.end = time.monotonic() + secs

    def git(self, cwd, *args, env=None):
        left = self.end - time.monotonic()
        if left <= 0:
            raise RuntimeError(f"git {args[0]} not started: the {TREE_SECS} s tree budget is spent")
        try:
            r = subprocess.run(
                ["git", "-C", cwd, *args],
                capture_output=True,
                encoding="utf-8",
                errors="replace",
                timeout=left,
                env=env,
            )
        except subprocess.TimeoutExpired:
            raise RuntimeError(f"git {args[0]} ran past the {TREE_SECS} s tree budget") from None
        if r.returncode != 0:
            raise RuntimeError(f"git {args[0]} failed: {(r.stderr or r.stdout).strip()[:300]}")
        return r.stdout


def worktree_tree(proj):
    """The tree id of the worktree holding `proj`; raises when git cannot say."""
    d = _Deadline(TREE_SECS)
    top = d.git(proj, "rev-parse", "--show-toplevel").strip()
    index = d.git(top, "rev-parse", "--git-path", "index").strip()
    index = index if os.path.isabs(index) else os.path.join(top, index)
    fd, tmp = tempfile.mkstemp(prefix="gauntlet-index-")
    os.close(fd)
    try:
        if os.path.isfile(index):
            # copy2 keeps the index's mtime: git then trusts the cached stat
            # data of entries older than the index and re-hashes only the
            # racily clean ones, exactly as it would with the real index
            shutil.copy2(index, tmp)
        else:
            os.unlink(tmp)
        env = {**os.environ, "GIT_INDEX_FILE": tmp}
        d.git(top, "add", "-A", env=env)
        return d.git(top, "write-tree", env=env).strip()
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def tree_changes(proj, before, after, excludes=()):
    """(status letter, repo-relative path) for each path that differs between
    two trees, leaving out paths under the repo-relative `excludes` dirs."""
    out = _Deadline(TREE_SECS).git(
        proj, "diff-tree", "-r", "-z", "--no-renames", "--name-status", before, after
    )
    fields = [f for f in out.split("\0") if f]
    dirs = [d.strip("/") for d in excludes if d.strip("/")]
    return [
        (status, path)
        for status, path in zip(fields[::2], fields[1::2])
        if not any(path == d or path.startswith(d + "/") for d in dirs)
    ]
