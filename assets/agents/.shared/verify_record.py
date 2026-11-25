#!/usr/bin/env python3
"""Verification records keyed by the content of the worktree.

A record says which checks ran on exactly one worktree tree and what they
returned. The tree id is the one sisyphus-next, the review gauntlet and the
adversary compute: a temporary copy of the index, `git add -A`, `git
write-tree`, so untracked files that are not ignored count and the real
index is never written. Records live under
`<git-common-dir>/coyote-verification/`, shared by every linked worktree,
because the key is content, not a branch or a HEAD.

A finalize record that decides nothing (a result timed out, the suite did
not run, or the finalize gate could not classify a red suite) reads as
missing, so a caller runs the gate again instead of trusting or failing on
it.

CLI:
  verify_record.py tree [--root DIR]
  verify_record.py write --root DIR --tier round|finalize --writer NAME
                         [--tree ID] [--floor SHA] [--baseline-mode MODE] < results.json
  verify_record.py lookup --root DIR --tier round|finalize [--exclude DIR]...
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

VERSION = 1
TIERS = ("round", "finalize")
TREE_SECS = 40
SCAN_LIMIT = 50
PASSING = ("green", "pre-existing", "not_run")
UNCLASSIFIED_NOTE = "not classified:"


def _git(root, *args, env=None):
    r = subprocess.run(
        ["git", "-C", root, *args],
        capture_output=True,
        encoding="utf-8",
        errors="replace",
        timeout=TREE_SECS,
        env=env,
    )
    if r.returncode != 0:
        raise RuntimeError(f"git {args[0]} failed: {(r.stderr or r.stdout).strip()[:300]}")
    return r.stdout


def toplevel(root):
    return _git(root, "rev-parse", "--show-toplevel").strip()


def rel_dir(root):
    """The directory `root` relative to its worktree root, `.` for the root itself."""
    top = os.path.realpath(toplevel(root))
    rel = os.path.relpath(os.path.realpath(root), top)
    return "." if rel == os.curdir else rel.replace(os.sep, "/")


def common_dir(root):
    path = _git(root, "rev-parse", "--git-common-dir").strip()
    return os.path.realpath(path if os.path.isabs(path) else os.path.join(root, path))


def worktree_tree(root):
    """The tree id of the worktree holding `root`, or "" when git cannot say."""
    try:
        top = toplevel(root)
        index = _git(top, "rev-parse", "--git-path", "index").strip()
    except (OSError, RuntimeError, subprocess.SubprocessError):
        return ""
    index = index if os.path.isabs(index) else os.path.join(top, index)
    fd, tmp = tempfile.mkstemp(prefix="verify-index-")
    os.close(fd)
    try:
        if os.path.isfile(index):
            # copy2 keeps the index's mtime, so git re-hashes exactly the
            # racily clean entries it would with the real index
            shutil.copy2(index, tmp)
        else:
            os.unlink(tmp)
        env = {**os.environ, "GIT_INDEX_FILE": tmp}
        _git(top, "add", "-A", env=env)
        return _git(top, "write-tree", env=env).strip()
    except (OSError, RuntimeError, subprocess.SubprocessError):
        return ""
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def _sha16(text):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:16]


def store_dir(root):
    return os.path.join(common_dir(root), "coyote-verification")


def record_path(root, tree, tier, rel=None):
    rel = rel_dir(root) if rel is None else rel
    return os.path.join(store_dir(root), f"{tree}-{_sha16(rel)}-{tier}.json")


def write(root, tier, results, writer, baseline_mode="", tree=None, floor=""):
    """Stores the record of `results` for the worktree tree (or `tree`) and returns its path.

    `floor` is the commit the writer measured the change from, empty when it had none."""
    if tier not in TIERS:
        raise ValueError(f"unknown tier {tier!r}")
    tree = tree or worktree_tree(root)
    if not tree:
        raise RuntimeError("git could not write a tree of the worktree")
    try:
        head = _git(root, "rev-parse", "--verify", "-q", "HEAD^{commit}").strip()
    except RuntimeError:
        head = ""
    rel = rel_dir(root)
    record = {
        "version": VERSION,
        "tier": tier,
        "tree": tree,
        "head": head,
        "rel_dir": rel,
        "recorded_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "writer": writer,
        "baseline_mode": baseline_mode,
        "floor": floor,
        "results": results,
    }
    path = record_path(root, tree, tier, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".record-", dir=os.path.dirname(path))
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            json.dump(record, f, indent=2)
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)
    return path


def _read(path, tier):
    """The record at `path`, or None when it is unreadable, of another version or tier, or names no tree."""
    try:
        with open(path, encoding="utf-8") as f:
            record = json.load(f)
    except (OSError, ValueError):
        return None
    if not isinstance(record, dict) or record.get("version") != VERSION or record.get("tier") != tier:
        return None
    if not record.get("tree") or not isinstance(record.get("results"), list):
        return None
    return record


def is_green(record, tier):
    if not decides(record, tier):
        return False
    results = record.get("results") or []
    if not all(isinstance(r, dict) and r.get("verdict") in PASSING for r in results):
        return False
    if tier == "finalize":
        return any(r.get("role") == "test" and r.get("verdict") in ("green", "pre-existing") for r in results)
    return bool(results)


def decides(record, tier):
    """False for a finalize record whose results cannot say whether the suite passed."""
    if tier != "finalize":
        return True
    results = [r for r in record.get("results") or [] if isinstance(r, dict)]
    tests = [r for r in results if r.get("role") == "test"]
    if not tests or any(r.get("verdict") == "timed_out" for r in results):
        return False
    for r in tests:
        note = r.get("note") if isinstance(r.get("note"), str) else ""
        if r.get("verdict") == "not_run" or (r.get("verdict") == "red" and note.startswith(UNCLASSIFIED_NOTE)):
            return False
    return True


def _differs_only_under(root, old, new, exclude):
    spec = ["."] + [f":(exclude){d.strip('/')}" for d in exclude if d.strip("/")]
    r = subprocess.run(
        ["git", "-C", toplevel(root), "diff-tree", "--quiet", old, new, "--", *spec],
        capture_output=True,
        timeout=TREE_SECS,
    )
    return r.returncode == 0


def lookup_exact(root, tier, tree, rel=None):
    """(path, record) of the record for exactly `tree`, or (None, None)."""
    path = record_path(root, tree, tier, rel)
    record = _read(path, tier)
    if record is not None and record["tree"] == tree and decides(record, tier):
        return path, record
    return None, None


def lookup(root, tier, exclude=()):
    """(path, record) of the record for the worktree as it stands, or (None, None).

    The exact tree wins. Otherwise, with `exclude` dirs given, the newest of
    the SCAN_LIMIT most recent records for this directory whose tree differs
    from the current one only under those dirs.
    """
    tree = worktree_tree(root)
    if not tree:
        return None, None
    rel = rel_dir(root)
    path, record = lookup_exact(root, tier, tree, rel)
    if record is not None:
        return path, record
    if not exclude:
        return None, None
    suffix = f"-{_sha16(rel)}-{tier}.json"
    try:
        names = [n for n in os.listdir(store_dir(root)) if n.endswith(suffix)]
    except OSError:
        return None, None
    paths = sorted((os.path.join(store_dir(root), n) for n in names), key=os.path.getmtime, reverse=True)
    for candidate in paths[:SCAN_LIMIT]:
        record = _read(candidate, tier)
        if record is None or record.get("rel_dir") != rel or not decides(record, tier):
            continue
        try:
            if _differs_only_under(root, record["tree"], tree, exclude):
                return candidate, record
        except (OSError, subprocess.SubprocessError):
            continue
    return None, None


def main(argv=None):
    parser = argparse.ArgumentParser(prog="verify_record.py")
    sub = parser.add_subparsers(dest="action", required=True)
    p_tree = sub.add_parser("tree")
    p_tree.add_argument("--root", default=".")
    p_write = sub.add_parser("write")
    p_write.add_argument("--root", required=True)
    p_write.add_argument("--tier", required=True, choices=TIERS)
    p_write.add_argument("--writer", required=True)
    p_write.add_argument("--tree", default="")
    p_write.add_argument("--baseline-mode", default="")
    p_write.add_argument("--floor", default="")
    p_lookup = sub.add_parser("lookup")
    p_lookup.add_argument("--root", required=True)
    p_lookup.add_argument("--tier", required=True, choices=TIERS)
    p_lookup.add_argument("--exclude", action="append", default=[])
    args = parser.parse_args(argv)

    if args.action == "tree":
        tree = worktree_tree(args.root)
        if not tree:
            print("git could not write a tree of the worktree", file=sys.stderr)
            return 1
        print(tree)
        return 0
    if args.action == "write":
        results = json.load(sys.stdin)
        print(write(args.root, args.tier, results, args.writer, args.baseline_mode, args.tree or None, args.floor))
        return 0
    path, record = lookup(args.root, args.tier, args.exclude)
    if record is None:
        status = "missing"
    else:
        status = "green" if is_green(record, args.tier) else "red"
    print(json.dumps({"status": status, "path": path, "record": record}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
