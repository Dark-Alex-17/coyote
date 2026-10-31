#!/usr/bin/env python3
"""The plan's task directories: one rule, shared by assemble_pr_body.py and the
`collect_ledger_lines` tool.

A task dir under --tasks-dir qualifies when its name opens with a task id
(`TASK-<n>` with an optional one-letter suffix, `TASK-265a`), its index.md
frontmatter `plan:` names the plan (basename compare with a trailing `.md`
stripped on both sides, so `PLAN-x` and `PLAN-x.md` agree while a dotted slug
keeps its dots), its `status:` is `complete`, and - when `--tasks TASK-a,TASK-b`
is given - its id is one of those (a PR group's members). A task whose index.md
has no `plan:` or no `status:` field is kept and named in a NOTE by both consumers
(this CLI on stderr, assemble_pr_body in its NOTE lines), so the leniency is
visible; an unreadable index.md is skipped with a NOTE the same way. Order: task number, then suffix.

CLI: `plan_tasks.py --plan PLAN-<slug>.md --tasks-dir DIR [--tasks IDS]` prints
one qualifying dir per NUL-terminated record (a dir name may hold a newline),
as `<tasks-dir>/<rel>` with the dir spelled as given; NOTE lines go to stderr,
one per line. A missing plan file or tasks dir is an error, never an empty listing.
A `--tasks` value that names no id (blank or commas only) is a usage error.
"""
import argparse
import os
import re
import sys

TASK_DIR_RE = re.compile(r"^(TASK-\d+[a-z]?)(?=-|$)")
TASK_ID_RE = re.compile(r"^TASK-(\d+)([a-z]?)$")
CONTROL_ESCAPES = {"\n": "\\n", "\r": "\\r", "\t": "\\t"}


def read(path):
    with open(path, encoding="utf-8", errors="replace") as fh:
        return fh.read()


def frontmatter(text):
    """Flat `key: value` pairs from a leading --- block; values unquoted."""
    fm = {}
    if not text.startswith("---"):
        return fm
    end = text.find("\n---", 3)
    if end < 0:
        return fm
    for line in text[3:end].splitlines():
        m = re.match(r"^([A-Za-z_][\w-]*):\s*(.*?)\s*$", line)
        if m:
            fm[m.group(1)] = m.group(2).strip().strip('"').strip("'")
    return fm


def plan_identity(path):
    """The plan a path or `plan:` value names: its basename, a trailing `.md` dropped.
    Only that suffix goes, so `PLAN-a.b.md` and `PLAN-a.b` agree and `PLAN-a` does not."""
    name = os.path.basename(path)
    return name[:-3] if name.endswith(".md") else name


def task_sort_key(task_id):
    m = TASK_ID_RE.match(task_id)
    return (int(m.group(1)), m.group(2)) if m else (10**9, task_id)


def parse_task_ids(text):
    """`--tasks` value -> set of ids, or None when the list is empty."""
    return {t.strip() for t in text.split(",") if t.strip()} or None


def walk_tasks(tasks_dir, plan_id, only=None):
    """Yield (task_id, frontmatter, dir) for every task dir of the plan (any status,
    `only` applied), in task order; `missing` collects listed ids never seen. An
    unreadable index.md (frontmatter None) is yielded but never counts as seen,
    since its plan is unknown."""
    rows, seen = [], set()
    for root, dirs, files in os.walk(tasks_dir):
        dirs.sort()
        base = os.path.basename(root)
        m = TASK_DIR_RE.match(base)
        if "index.md" not in files or not m:
            continue
        try:
            fm = frontmatter(read(os.path.join(root, "index.md")))
        except OSError:
            fm = None
        task_id = m.group(1)
        if fm is not None:
            plan = fm.get("plan", "")
            if plan and plan_identity(plan) != plan_id:
                continue
        if only is not None and task_id not in only:
            continue
        if fm is not None:
            seen.add(task_id)
        rows.append((task_id, fm, root))
    rows.sort(key=lambda r: task_sort_key(r[0]))
    missing = sorted(only - seen, key=task_sort_key) if only is not None else []
    return rows, missing


def is_complete(fm):
    status = fm.get("status", "")
    return not status or status == "complete"


def absent_fields(fm):
    """The selection fields index.md lacks (`plan:`, `status:`), in that order; a task
    missing one is kept, and both consumers name it with `fieldless_note`."""
    return [f"{key}:" for key in ("plan", "status") if not fm.get(key)]


def escape(text):
    """`text` on one line with every non-printable character escaped."""
    out = []
    for ch in text:
        if ch in CONTROL_ESCAPES:
            out.append(CONTROL_ESCAPES[ch])
        elif ch.isprintable():
            out.append(ch)
        elif ord(ch) <= 0xFF:
            out.append(f"\\x{ord(ch):02x}")
        elif ord(ch) <= 0xFFFF:
            out.append(f"\\u{ord(ch):04x}")
        else:
            out.append(f"\\U{ord(ch):08x}")
    return "".join(out)


def label(root):
    """A task dir name for a NOTE, escaped to one line."""
    return escape(os.path.basename(root))


def unreadable_note(base):
    return f"{base}: index.md unreadable (plan unknown); skipped"


def fieldless_note(base, absent):
    return f"{base}: index.md has no {'/'.join(absent)}; kept"


def not_complete_note(base, status):
    return f"{base}: listed in --tasks but status: {status}; skipped"


def missing_note(missing):
    return "listed in --tasks but not found under the plan: " + ", ".join(escape(t) for t in missing)


def task_filter(ap, tasks):
    """The `--tasks` id set, None when the option is absent; a value naming no id
    is a usage error (exit 2), never a silent "no filter"."""
    if tasks is None:
        return None
    only = parse_task_ids(tasks)
    if only is None:
        ap.error("--tasks names no task ids")
    return only


def main(argv):
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(encoding="utf-8", errors="backslashreplace", newline="\n")
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--plan", required=True, help="PLAN-<slug>.md; its name, `.md` stripped, selects the tasks")
    ap.add_argument("--tasks-dir", required=True, help="tree searched recursively for TASK-NNN-*/index.md")
    ap.add_argument("--tasks", help="comma-separated task ids; keep only those")
    args = ap.parse_args(argv)
    only = task_filter(ap, args.tasks)
    if not os.path.isdir(args.tasks_dir):
        sys.exit(f"plan_tasks: tasks dir not found: {args.tasks_dir}")
    if not os.path.isfile(args.plan):
        sys.exit(f"plan_tasks: plan not found: {args.plan}")
    rows, missing = walk_tasks(args.tasks_dir, plan_identity(args.plan), only)
    for _, fm, root in rows:
        if fm is None:
            print(f"plan_tasks: NOTE {unreadable_note(label(root))}", file=sys.stderr)
            continue
        absent = absent_fields(fm)
        if absent:
            print(f"plan_tasks: NOTE {fieldless_note(label(root), absent)}", file=sys.stderr)
        if is_complete(fm):
            print(root, end="\0")
        elif only is not None:
            print(f"plan_tasks: NOTE {not_complete_note(label(root), fm['status'])}", file=sys.stderr)
    if missing:
        print(f"plan_tasks: NOTE {missing_note(missing)}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
