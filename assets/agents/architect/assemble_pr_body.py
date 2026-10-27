#!/usr/bin/env python3
"""Assemble the single-PR body from the plan, the tasks tree, and a summary file.

Grep-complete where the ground truth is on disk; verbatim where it is judgment:

  summary          --summary-file, pasted verbatim (the Problem/Approach prose)
  Quality bar      plan frontmatter `rigor:`; the line is emitted only below `production`
  task checklist   every task dir under --tasks-dir whose index.md belongs to the plan
                   (`plan:` basename match when the field exists; `status: complete` when
                   that field exists), title from frontmatter, commit SHAs from the
                   log.md `completed` entry
  Review decisions `rejected-finding:` ledger lines, one entry per tag occurrence
  Hygiene pass     --hygiene-file verbatim, else `HYGIENE:` ledger lines, else "clean"
  Design decisions `design-decision` ledger lines
  Follow-ups       `Follow-ups:` ledger lines as checkboxes (only with --followups)

Ledger lines in log.md are prose, often several tags on one line: an entry runs from its
tag to the next ledger tag of any kind on the line, or the end of the line, so an entry
can carry a trailing clause - the body is a draft the caller reviews before `gh pr
create`, never posted blind. Anything that cannot be derived is reported as a NOTE and left
visible in the body (never silently omitted) so a hand fix is a local edit, not a rewrite.
"""
import argparse
import os
import re
import sys

TASK_DIR_RE = re.compile(r"^(TASK-\d+)")
SHA_RE = re.compile(r"\b(?=[0-9a-f]*[a-f])[0-9a-f]{7,40}\b")
HEX_WORDS = {"deadbeef", "cafebabe", "decade", "facade", "accede"}
# Every tag the ledger vocabulary knows; an entry for one tag stops at the next of any.
LEDGER_TAGS = ("rejected-finding:", "design-decision", "Follow-ups:", "HYGIENE:", "debris-exempt", "debt-assigned", "(deferred by quality bar)")


def note(msg):
    print(f"assemble_pr_body: NOTE {msg}", file=sys.stderr)


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


def task_number(name):
    m = re.match(r"^TASK-(\d+)", name)
    return int(m.group(1)) if m else 10**9


def find_tasks(tasks_dir, plan_base):
    """[(task_id, title, dir)] for the plan's tasks, in TASK number order."""
    found, skipped = [], []
    for root, dirs, files in os.walk(tasks_dir):
        dirs.sort()
        base = os.path.basename(root)
        if "index.md" not in files or not TASK_DIR_RE.match(base):
            continue
        try:
            fm = frontmatter(read(os.path.join(root, "index.md")))
        except OSError as exc:
            note(f"{base}: index.md unreadable ({exc}); skipped")
            continue
        plan = fm.get("plan", "")
        if plan and os.path.basename(plan) != plan_base:
            continue
        status = fm.get("status", "")
        if status and status != "complete":
            skipped.append(f"{base} (status: {status})")
            continue
        title = fm.get("title", "")
        if not title:
            m = re.search(r"^#\s+(.+?)\s*$", read(os.path.join(root, "index.md")), re.M)
            title = m.group(1) if m else "(untitled - index.md has no title)"
            if not fm.get("title"):
                note(f"{base}: no `title:` frontmatter; used {'the H1' if m else 'a placeholder'}")
        found.append((TASK_DIR_RE.match(base).group(1), title, root))
    if skipped:
        note("excluded from the checklist (not `status: complete`): " + ", ".join(skipped))
    found.sort(key=lambda t: task_number(t[0]))
    return found


def completed_shas(log_text):
    """Hex SHAs from the `completed` entry: the `## ... completed` section when the log
    uses H2 entries, else lines that open with `completed`."""
    sections, cur, in_completed = [], [], False
    for line in log_text.splitlines():
        if line.startswith("## "):
            if in_completed:
                sections.append("\n".join(cur))
            in_completed = "completed" in line.lower()
            cur = []
        elif in_completed:
            cur.append(line)
    if in_completed:
        sections.append("\n".join(cur))
    if not sections:
        sections = [l for l in log_text.splitlines() if l.lower().startswith("completed")]
    seen, out = set(), []
    for s in sections:
        for sha in SHA_RE.findall(s):
            if sha in HEX_WORDS or sha in seen:
                continue
            seen.add(sha)
            out.append(sha)
    return out


def ledger_entries(tasks, tag):
    """[(task_id, text)] - one entry per tag occurrence across the tasks' log.md files."""
    others = [t for t in LEDGER_TAGS if t != tag]
    entries = []
    for task_id, _title, root in tasks:
        path = os.path.join(root, "log.md")
        if not os.path.isfile(path):
            continue
        for line in read(path).splitlines():
            if tag not in line:
                continue
            parts = line.split(tag)[1:]
            for part in parts:
                cuts = [part.find(o) for o in others if part.find(o) >= 0]
                if cuts:
                    part = part[: min(cuts)]
                text = re.sub(r"^[\s:\u2014\u2013-]+", "", part).strip()
                if text:
                    entries.append((task_id, text))
    return entries


def bullets(entries, box=""):
    return "\n".join(f"- {box}({t}) {text}" for t, text in entries)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--plan", required=True, help="PLAN-<slug>.md (rigor from frontmatter; basename filters tasks)")
    ap.add_argument("--tasks-dir", required=True, help="tree searched recursively for TASK-NNN-*/index.md")
    ap.add_argument("--summary-file", required=True, help="file holding the Problem/Approach summary, pasted verbatim")
    ap.add_argument("--hygiene-file", help="file holding the kept HYGIENE items, pasted verbatim (else derived from `HYGIENE:` ledger lines)")
    ap.add_argument("--quality-note", default="", help="text appended to the Quality bar line (e.g. deferred-hardening task ids)")
    ap.add_argument("--followups", action="store_true", help="append a `## Follow-up / manual actions` section from `Follow-ups:` ledger lines")
    ap.add_argument("--out", help="also write the body to this file")
    args = ap.parse_args()

    for p in (args.plan, args.summary_file) + ((args.hygiene_file,) if args.hygiene_file else ()):
        if not os.path.isfile(p):
            sys.exit(f"assemble_pr_body: file not found: {p}")
    if not os.path.isdir(args.tasks_dir):
        sys.exit(f"assemble_pr_body: tasks dir not found: {args.tasks_dir}")

    plan_base = os.path.basename(args.plan)
    plan_fm = frontmatter(read(args.plan))
    tasks = find_tasks(args.tasks_dir, plan_base)
    if not tasks:
        note(f"no tasks under {args.tasks_dir} belong to {plan_base} - checklist left empty")

    out = [read(args.summary_file).strip(), ""]

    rigor = plan_fm.get("rigor", "")
    if not rigor:
        note("plan frontmatter has no `rigor:`; Quality bar line omitted - add it by hand if rigor is below production")
    elif rigor != "production":
        line = f"**Quality bar:** {rigor}"
        if args.quality_note:
            line += f" - {args.quality_note}"
        out += [line, ""]

    out.append("## Tasks")
    for task_id, title, root in tasks:
        log_path = os.path.join(root, "log.md")
        shas = completed_shas(read(log_path)) if os.path.isfile(log_path) else []
        if not shas:
            note(f"{task_id}: no commit SHAs in the log.md `completed` entry")
            sha_text = "commit SHAs: none recorded in log.md"
        else:
            sha_text = ", ".join(f"`{s}`" for s in shas)
        out.append(f"- [x] {task_id} - {title} ({sha_text})")
    out.append("")

    rejected = ledger_entries(tasks, "rejected-finding:")
    if rejected:
        out += ["## Review decisions", bullets(rejected), ""]

    out.append("## Hygiene pass")
    if args.hygiene_file:
        out.append(read(args.hygiene_file).strip())
    else:
        hyg = ledger_entries(tasks, "HYGIENE:")
        out.append(bullets(hyg) if hyg else "clean")
    out.append("")

    design = ledger_entries(tasks, "design-decision")
    if design:
        out += ["## Design decisions", bullets(design), ""]

    if args.followups:
        fu = ledger_entries(tasks, "Follow-ups:")
        out += ["## Follow-up / manual actions", bullets(fu, box="[ ] ") if fu else "None.", ""]

    body = "\n".join(out).rstrip() + "\n"
    if args.out:
        with open(args.out, "w", encoding="utf-8") as fh:
            fh.write(body)
    sys.stdout.write(body)


if __name__ == "__main__":
    main()
