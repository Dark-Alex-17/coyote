"""Scratch planning-store fixture for the architect-next test batteries (upstream variant).

The smoke battery and the mode-matrix walk are fork-neutral; everything that knows
how THIS fork stores tasks lives here. Upstream: a plans dir INSIDE the code repo
(`plans/PLAN-<slug>.md` + `plans/tasks/TASK-NNN-<slug>/{index.md,log.md}` per the
task-tracking skill), so planning commits are `chore(plan)` commits on the current
branch of the code repo.
"""

import os
import re
import stat
import subprocess

FORK = "upstream"
OWNER = ""
HAS_OWNER_FILE = False
CLOSE_COMMIT_MARKER = "chore(plan): complete"  # the planning commit subject on task close


def _git(repo, *a, check=True):
    p = subprocess.run(["git", "-C", repo, *a], capture_output=True, text=True)
    if check and p.returncode != 0:
        raise RuntimeError(p.stderr)
    return p.stdout.strip()


def _write(path, text, exe=False):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)
    if exe:
        os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)


class Store:
    """One scratch planning store. `root` is what the fork's planning variable points at."""

    def __init__(self, tmp, code_repo):
        self.code = code_repo
        self.root = os.path.join(code_repo, "plans")
        self.tasks_root = os.path.join(self.root, "tasks")
        os.makedirs(self.tasks_root, exist_ok=True)
        self.plan_path = os.path.join(self.root, "PLAN-widgets.md")   # what author_plan writes
        self.promoted_plan = self.plan_path                            # promotion keeps the same file
        self._n = 0

    def variables(self, design_doc, reference_root, **mode):
        v = {"project_dir": self.code, "plans_dir": "plans", "base_branch": "main", "design_doc": design_doc}
        v.update(mode)
        return v

    def normalized(self):
        return {"plans_root": self.root, "tasks_root": self.tasks_root, "owner_name": "", "base_branch_name": "main"}

    def write_promoted_plan(self, text):
        _write(self.promoted_plan, text)
        return self.promoted_plan

    def new_task(self, gates, slug, files="", blocked="", batch="", protected=False, group="", surfaces="", plan_name="PLAN-widgets.md"):
        nums = [int(m.group(1)) for n in os.listdir(self.tasks_root) if (m := re.match(r"^TASK-(\d+)", n))]
        tid = f"TASK-{(max(nums) + 1) if nums else 1:03d}"
        d = os.path.join(self.tasks_root, f"{tid}-{slug}")
        _write(os.path.join(d, "index.md"), f"---\ntitle: {slug}\nstatus: pending\nplan: {plan_name}\n---\n\n# {slug}\n")
        _write(os.path.join(d, "log.md"), f"# {tid} log\n")
        idx = os.path.join(d, "index.md")
        with open(idx, "a") as f:
            f.write(f"\n## What\n\nDo {slug}.\n\n## Acceptance criteria\n\n- `true` exits 0\n\n### Context pack\n\n```go\npackage x\n```\n")
        gates.set_frontmatter(idx, "files", f"[{files}]")
        if batch:
            gates.set_frontmatter(idx, "batch", batch)
        if group:
            gates.set_frontmatter(idx, "group", group)
        if surfaces:
            gates.set_frontmatter(idx, "surfaces", surfaces)
        if protected:
            gates.set_frontmatter(idx, "protected", "true")
        if blocked:
            gates.set_frontmatter(idx, "blocked_by", f"[{blocked}]")
        return tid, d

    def promote_report(self):
        return f"promoted\nPLAN: {self.promoted_plan}"

    def _status(self, tid):
        for n in os.listdir(self.tasks_root):
            if n.startswith(tid):
                text = open(os.path.join(self.tasks_root, n, "index.md")).read()
                m = re.search(r"(?m)^status:\s*(\S+)", text)
                return m.group(1) if m else ""
        return ""

    def claimed(self, tid):
        return self._status(tid) == "in-progress"

    def closed(self, tid):
        return self._status(tid) == "complete"

    def last_commit(self):
        return _git(self.code, "log", "-1", "--pretty=%s")
