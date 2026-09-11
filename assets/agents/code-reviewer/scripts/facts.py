#!/usr/bin/env python3
"""Deterministic review facts for the code-reviewer graph.

Resolves the diff, the changed-file list, the quality bar (caller-passed →
plan frontmatter → detected default), the conventions pointer, review-miss
ledger matches, the repo-history sweep (fix/revert clusters, bounded gh
PR-comment pull), and the deterministic verdict-trigger signals. Pure
git/gh/filesystem — no LLM. Failures degrade into `facts_note`; they never
sink the review.
"""

import glob
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
spec = (state.get("diff_spec") or "auto").strip()
notes = []
out = {}


def git(*args, timeout=60):
    r = subprocess.run(["git", "-C", proj, *args], capture_output=True, text=True, timeout=timeout)
    if r.returncode != 0:
        raise RuntimeError((r.stderr or r.stdout).strip()[:300])
    return r.stdout


def scrubbed_env():
    env = dict(os.environ)
    env.pop("CLICOLOR_FORCE", None)
    env.pop("FORCE_COLOR", None)
    env["GH_NO_UPDATE_NOTIFIER"] = "1"
    env["NO_COLOR"] = "1"
    return env


# --- diff resolution (get_diff semantics) -----------------------------------
resolved = spec
files = []
try:
    if spec in ("auto", ""):
        names = git("diff", "--cached", "--numstat")
        resolved = "staged"
        if not names.strip():
            names = git("diff", "--numstat")
            resolved = "worktree"
        if not names.strip():
            names = git("diff", "HEAD~1", "--numstat")
            resolved = "HEAD~1"
    elif spec == "staged":
        names = git("diff", "--cached", "--numstat")
    elif spec == "worktree":
        names = git("diff", "HEAD", "--numstat")
    else:
        names = git("diff", spec, "--numstat")
    for line in names.splitlines():
        parts = line.split("\t")
        if len(parts) == 3:
            added, deleted, path = parts
            if added == "-" and deleted == "-":
                notes.append(f"binary file skipped: {path}")
                continue
            files.append(path.strip())
except Exception as e:  # noqa: BLE001
    notes.append(f"diff resolution failed for spec {spec!r}: {e}")

out["resolved_diff_spec"] = resolved
out["changed_files"] = files[:300]
out["file_count"] = len(files)


def diff_text():
    try:
        if resolved == "staged":
            return git("diff", "--cached", timeout=90)
        if resolved == "worktree":
            return git("diff", "HEAD", timeout=90) if spec == "worktree" else git("diff", timeout=90)
        if resolved == "HEAD~1":
            return git("diff", "HEAD~1", timeout=90)
        return git("diff", resolved, timeout=90)
    except Exception:  # noqa: BLE001
        return ""


dtext = diff_text()
added_lines = "\n".join(
    l for l in dtext.splitlines() if l.startswith("+") and not l.startswith("+++")
)

# --- quality bar resolution --------------------------------------------------
rigor_in = (state.get("rigor_in") or state.get("rigor") or "").strip()
surfaces_in = (state.get("surfaces_in") or state.get("surfaces") or "").strip()
resolved_rigor, resolved_surfaces, provenance = "production", "", "detected-default"
if rigor_in or surfaces_in:
    resolved_rigor = rigor_in or "production"
    resolved_surfaces = surfaces_in
    provenance = "passed"
else:
    for plan in sorted(glob.glob(os.path.join(proj, "plans", "PLAN-*.md"))):
        try:
            with open(plan) as f:
                head = f.read(4000)
            m = re.match(r"\A---\n(.*?)\n---", head, re.S)
            if m and re.search(r"^status:\s*active\s*$", m.group(1), re.M):
                rig = re.search(r"^rigor:\s*(\S+)", m.group(1), re.M)
                sur = re.search(r"^surfaces:\s*(.+)$", m.group(1), re.M)
                if rig:
                    resolved_rigor = rig.group(1).strip()
                if sur:
                    resolved_surfaces = sur.group(1).strip("[] \n")
                provenance = "plan"
                break
        except OSError:
            continue

SURFACE_PATTERNS = [
    ("db-migration", r"migrations?/|\.sql$|schema\.(rb|prisma|sql)"),
    ("iac", r"\.tf$|helm|charts?/|Dockerfile|docker-compose|k8s/|manifests?/"),
    ("ci-cd", r"\.github/workflows/|\.gitlab-ci|Jenkinsfile|\.circleci/"),
    ("worker", r"workers?/|jobs?/|consumers?/|cron|celery|sidekiq|river"),
    ("rest-api", r"routes?|handlers?|controllers?|endpoints?|\bapi\b|\.proto$|openapi|swagger|graphql|resolvers?"),
    ("cli", r"\bcmd/|\bcli\b"),
    ("library", r"go\.mod$|package\.json$|Cargo\.toml$|pyproject\.toml$|setup\.py$|__init__\.py$"),
]
if provenance == "detected-default" and files:
    detected = []
    for surface, pat in SURFACE_PATTERNS:
        if any(re.search(pat, f, re.I) for f in files):
            detected.append(surface)
    resolved_surfaces = ", ".join(detected)

out["resolved_rigor"] = resolved_rigor
out["resolved_surfaces"] = resolved_surfaces
out["bar_provenance"] = provenance

# --- conventions pointer ------------------------------------------------------
conv = []
for pat in ("CLAUDE.md", "COYOTE.md", "CONTRIBUTING.md", ".golangci*", ".eslintrc*",
            "rustfmt.toml", ".rubocop.yml", "ruff.toml", ".editorconfig", "Makefile"):
    conv.extend(sorted(glob.glob(os.path.join(proj, pat)))[:2])
out["conventions"] = ", ".join(os.path.relpath(p, proj) for p in conv) or "none found"

# --- review-miss ledger -------------------------------------------------------
ledger_text = ""
for cand in (
    os.path.join(os.path.expanduser("~"), ".config", "coyote", "share", "review-misses.md"),
    os.path.join(os.path.expanduser("~"), ".config", "coyote", "review-misses.md"),
):
    if os.path.isfile(cand):
        try:
            with open(cand) as f:
                ledger_text = f.read()
        except OSError:
            pass
        break

matches = []
if ledger_text:
    try:
        repo_name = os.path.basename(git("rev-parse", "--show-toplevel").strip())
    except Exception:  # noqa: BLE001
        repo_name = os.path.basename(os.path.abspath(proj))
    keywords = {repo_name.lower()}
    keywords.update(s.strip().lower() for s in resolved_surfaces.split(",") if s.strip())
    for f in files:
        keywords.update(p.lower() for p in re.split(r"[/_.-]", f) if len(p) >= 4)
    entries = re.split(r"(?=^## MISS-)", ledger_text, flags=re.M)
    for entry in entries:
        if not entry.startswith("## MISS-"):
            continue
        body = entry.lower()
        if any(k in body for k in keywords):
            lines = entry.strip().splitlines()
            matches.append("\n".join(lines[:8]))
        if len(matches) >= 8:
            break
out["ledger_matches"] = "\n\n".join(matches) if matches else "(no matching ledger entries)"

# --- repo-history sweep -------------------------------------------------------
history = []
fragile = []
try:
    if files:
        log = git("log", "--oneline", "-15", "--", *files[:100])
        history.append(log.strip()[:2000])
        per_file = {}
        for f in files[:50]:
            try:
                fl = git("log", "--oneline", "-10", "--", f)
            except Exception:  # noqa: BLE001
                continue
            hits = [l for l in fl.splitlines() if re.search(r"\b(fix|revert|hotfix)\b", l, re.I)]
            if len(hits) >= 2:
                fragile.append(f)
                per_file[f] = hits[:2]
        for f, hits in per_file.items():
            history.append(f"FRAGILE: {f} — {len(hits)}+ fix/revert commits recently — raise scrutiny")
except Exception as e:  # noqa: BLE001
    notes.append(f"history sweep failed: {e}")

# gh one-level-deeper: PR review comments for fragile files (bounded, silent skip)
try:
    if fragile:
        remote = git("remote", "get-url", "origin").strip()
        m = re.search(r"github\.com[:/]([^/]+/[^/.]+)", remote)
        if m:
            repo_slug = m.group(1)
            env = scrubbed_env()
            shas = []
            for f in fragile[:2]:
                lg = git("log", "--format=%H", "-2", "--", f)
                shas.extend(lg.split())
            pr_nums = []
            for sha in shas[:3]:
                r = subprocess.run(
                    ["gh", "api", f"repos/{repo_slug}/commits/{sha}/pulls", "--jq", ".[].number"],
                    capture_output=True, text=True, timeout=30, env=env,
                )
                if r.returncode == 0:
                    pr_nums.extend(n for n in r.stdout.split() if n.isdigit())
            seen = []
            for n in pr_nums:
                if n not in seen:
                    seen.append(n)
            comments = []
            for n in seen[:3]:
                r = subprocess.run(
                    ["gh", "api", f"repos/{repo_slug}/pulls/{n}/comments",
                      "--jq", r'.[] | "\(.path): \(.body)"'],
                    capture_output=True, text=True, timeout=30, env=env,
                )
                if r.returncode == 0:
                    for line in r.stdout.splitlines():
                        path = line.split(":", 1)[0]
                        if any(path in f or f in path for f in files):
                            comments.append(line[:200])
                if len(comments) >= 6:
                    break
            if comments:
                history.append("Prior PR review comments on fragile paths:\n" + "\n".join(comments[:6]))
except Exception:  # noqa: BLE001
    pass  # offline / not GitHub / no gh — silent by design

out["history_notes"] = "\n".join(h for h in history if h) or "(no notable history)"

# --- deterministic verdict-trigger signals -----------------------------------
def any_path(pattern):
    return any(re.search(pattern, f, re.I) for f in files)


migration_files = [f for f in files if re.search(r"migrations?/|\.sql$", f, re.I)]
mig_added = "\n".join(
    l for l in dtext.splitlines()
    if l.startswith("+") and not l.startswith("+++")
) if migration_files else ""
out["destructive_migration"] = bool(
    migration_files
    and re.search(r"(?i)drop\s+(table|column)|rename\s+(table|column|to)|alter\s+\S+\s+drop", mig_added)
)
out["touches_auth"] = bool(
    any_path(r"auth(?!or)|login|logout|token|session|permission|rbac|\bacl\b|credential")
    or re.search(r"auth(?!or)|password|jwt|session|credential", added_lines, re.I)
)
out["consumer_surface"] = any_path(
    r"routes?|handlers?|\bapi\b|endpoints?|\bcmd/|\bcli\b|\.proto$|openapi|swagger|controllers?|graphql"
)

if notes:
    out["facts_note"] = "; ".join(notes)[:800]
print(json.dumps(out))
