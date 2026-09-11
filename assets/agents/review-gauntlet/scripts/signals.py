#!/usr/bin/env python3
"""Deterministic diff signals for the review gauntlet.

Pure git — no LLM judgment. Every signal is computed from the changed-file
list and the added lines of the diff. Failures never sink the gauntlet:
on any error the script reports `signals_error` and leaves the defaults
(false/empty) in place, which biases lane selection toward the caller's
context and forced lanes.
"""

import json
import os
import re
import subprocess

def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


state = load_state()
proj = state.get("project_dir") or "."
spec = (state.get("diff_spec") or "worktree").strip()


def git(*args):
    r = subprocess.run(
        ["git", "-C", proj, *args], capture_output=True, text=True, timeout=90
    )
    if r.returncode != 0:
        raise RuntimeError((r.stderr or r.stdout).strip()[:400])
    return r.stdout


out = {}
try:
    if spec in ("", "worktree"):
        names = git("diff", "--name-only", "HEAD")
        difftext = git("diff", "HEAD")
    else:
        names = git("diff", "--name-only", spec)
        difftext = git("diff", spec)
    files = [f.strip() for f in names.splitlines() if f.strip()]
    added = "\n".join(
        l for l in difftext.splitlines() if l.startswith("+") and not l.startswith("+++")
    )

    def any_path(pattern):
        return any(re.search(pattern, f, re.I) for f in files)

    out.update(
        {
            "changed_files": files[:200],
            "file_count": len(files),
            "touches_proto": any_path(r"\.proto$|buf\.(yaml|gen\.yaml)$"),
            "touches_migrations": any_path(r"migrations?/|\.sql$"),
            # auth(?!or) admits auth/authn/authz/oauth but not "author".
            # Over-firing here is the safe direction: it only ADDS a lane.
            "touches_auth": any_path(
                r"auth(?!or)|login|logout|token|session|permission|rbac|\bacl\b|credential|secret"
            )
            or bool(
                re.search(
                    r"auth(?!or)|password|token|secret|credential|jwt|session",
                    added,
                    re.I,
                )
            ),
            "touches_deps": any_path(
                r"go\.(mod|sum)$|package(-lock)?\.json$|yarn\.lock$|pnpm-lock\.yaml$"
                r"|Cargo\.(toml|lock)$|requirements[^/]*\.txt$|pyproject\.toml$"
                r"|Gemfile(\.lock)?$|pom\.xml$|build\.gradle|composer\.(json|lock)$"
            ),
            "touches_exec": bool(
                re.search(
                    r"exec\.Command|subprocess|os/exec|Runtime\.getRuntime"
                    r"|shell_exec|\beval\(|\bsystem\(|child_process",
                    added,
                )
            ),
            "consumer_surface": any_path(
                r"routes?|handlers?|\bapi\b|endpoints?|\bcmd/|\bcli\b|\.proto$"
                r"|openapi|swagger|controllers?|graphql"
            ),
            "docs_only": bool(files)
            and all(re.search(r"\.(md|rst|txt|adoc)$", f, re.I) for f in files),
            "signals_summary": f"{len(files)} file(s) changed (diff: {spec})",
        }
    )
except Exception as e:  # noqa: BLE001 — degraded signals must not sink the gauntlet
    out["signals_error"] = (
        f"NOTE: diff signals could not be computed ({e}); "
        "lane selection falls back to caller context and forced lanes."
    )
    out["signals_summary"] = "signals unavailable"

print(json.dumps(out))
