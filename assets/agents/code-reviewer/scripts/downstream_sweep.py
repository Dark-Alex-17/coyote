#!/usr/bin/env python3
"""Downstream-consumers sweep for the code-reviewer graph.

Runs only when consumer-facing contract surface changed. Extracts the most
specific changed contract tokens from the diff deterministically (route path
literals, proto message/service/rpc names, exported symbols), greps sibling
checkouts under `consumers_root`, and runs a bounded org code search via gh.
Hits are reported for the synthesis to raise as 🔴 findings naming the
consuming repo + file:line. Degrades silently when unconfigured/offline;
public-library caveats are noted, not guessed at.
"""

import json
import os
import re
import shutil
import subprocess

def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


state = load_state()
proj = os.path.abspath(state.get("project_dir") or ".")
spec = state.get("resolved_diff_spec") or "worktree"
consumers_root = os.path.expanduser((state.get("consumers_root") or "").strip())
github_org = (state.get("github_org") or "").strip()

if not state.get("consumer_surface"):
    print(json.dumps({"downstream_notes": "(skipped — no consumer-facing contract surface in the diff)"}))
    raise SystemExit(0)


def diff_text():
    args = {"staged": ["--cached"], "worktree": ["HEAD"], "HEAD~1": ["HEAD~1"]}.get(spec, [spec])
    r = subprocess.run(["git", "-C", proj, "diff", *args], capture_output=True, text=True, timeout=90)
    return r.stdout if r.returncode == 0 else ""


try:
    changed = [
        l[1:] for l in diff_text().splitlines()
        if (l.startswith("+") or l.startswith("-")) and not l.startswith(("+++", "---"))
    ]
    text = "\n".join(changed)
    tokens = set()
    tokens.update(re.findall(r'["\'](/[a-z0-9][a-z0-9/_{}.:-]{5,})["\']', text))          # route paths
    tokens.update(re.findall(r"^\s*(?:message|service|rpc)\s+([A-Z][A-Za-z0-9_]{5,})", text, re.M))  # proto names
    tokens.update(re.findall(r"^\s*(?:pub\s+fn|pub\s+struct|func|export\s+(?:function|const|class))\s+([A-Z][A-Za-z0-9_]{5,})", text, re.M))
    tokens = sorted(tokens, key=len, reverse=True)[:5]

    notes = []
    if not tokens:
        notes.append("(no specific contract tokens extractable from the diff — sweep skipped)")
    else:
        notes.append(f"contract tokens probed: {tokens}")
        if consumers_root and os.path.isdir(consumers_root):
            self_name = os.path.basename(proj)
            for tok in tokens:
                r = subprocess.run(
                    ["grep", "-rIl", "--exclude-dir=.git", "--exclude-dir=node_modules",
                     "--exclude-dir=target", "--exclude-dir=vendor", tok, consumers_root],
                    capture_output=True, text=True, timeout=60,
                )
                hits = [
                    h for h in r.stdout.splitlines()
                    if h.strip() and f"/{self_name}/" not in h and not h.startswith(proj)
                ][:4]
                for h in hits:
                    notes.append(f"CONSUMER HIT (sibling checkout): {h} references '{tok}' — "
                                 "a changed/removed contract element with a live consumer is a 🔴 finding")
        elif consumers_root:
            notes.append(f"(consumers_root '{consumers_root}' not found — local probe skipped)")
        else:
            notes.append("(consumers_root unset — local probe skipped)")

        if github_org and shutil.which("gh"):
            env = dict(os.environ)
            env.pop("CLICOLOR_FORCE", None)
            env.pop("FORCE_COLOR", None)
            env["GH_NO_UPDATE_NOTIFIER"] = "1"
            for tok in tokens:
                r = subprocess.run(
                    ["gh", "api", "search/code", "-f", f'q=org:{github_org} "{tok}"',
                     "--jq", r'.items[:4][] | "\(.repository.full_name): \(.path)"'],
                    capture_output=True, text=True, timeout=30, env=env,
                )
                if r.returncode == 0 and r.stdout.strip():
                    for line in r.stdout.strip().splitlines():
                        notes.append(f"CONSUMER HIT (org search): {line} references '{tok}'")
        elif github_org:
            notes.append("(gh unavailable — org code search skipped)")
        else:
            notes.append("(github_org unset — org code search skipped)")
        notes.append("Public-library caveat: consumers of a PUBLIC library are unenumerable — "
                     "for exported-API changes the check degrades to semver/changelog discipline.")
    print(json.dumps({"downstream_notes": "\n".join(notes)}))
except Exception as e:  # noqa: BLE001
    print(json.dumps({"downstream_notes": f"(sweep degraded: {e})"}))
