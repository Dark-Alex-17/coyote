#!/usr/bin/env python3
"""Deterministic domain grouping of changed files for the code-reviewer graph.

Each file lands in the FIRST matching domain (patterns ordered most-specific
first); unmatched files land in `general`. When the grouping is very wide,
single-file groups merge into `general` so the fan-out stays sane. The LLM
refinement node may rebalance this proposal; the cover gate guarantees no
file is lost either way. Crash-safe: an error emits one catch-all proposal
rather than tolerant-failing into an empty grouping.
"""

import json
import os
import re

def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


DOMAINS = [
    ("db", r"migrations?/|\.sql$|schema\.(rb|prisma|sql)|models?/"),
    ("iac", r"\.tf$|helm|charts?/|(^|/)Dockerfile|docker-compose|k8s/|manifests?/"),
    ("ci-cd", r"\.github/workflows/|\.gitlab-ci|Jenkinsfile|\.circleci/"),
    ("workers", r"workers?/|jobs?/|consumers?/|cron|tasks?/"),
    ("api", r"routes?|handlers?|controllers?|endpoints?|\bapi\b|\.proto$|openapi|swagger|graphql|resolvers?"),
    ("cli", r"\bcmd/|\bcli\b"),
    ("deps", r"go\.(mod|sum)$|package(-lock)?\.json$|yarn\.lock$|Cargo\.(toml|lock)$|requirements[^/]*\.txt$|pyproject\.toml$|Gemfile|pom\.xml$|composer\.json$"),
    ("tests", r"(^|/)tests?/|_test\.|\.test\.|spec\."),
    ("docs", r"\.(md|rst|adoc|txt)$"),
]


def main():
    state = load_state()
    files = state.get("changed_files") or []

    groups = {}
    for f in files:
        domain = "general"
        for name, pat in DOMAINS:
            if re.search(pat, f, re.I):
                domain = name
                break
        groups.setdefault(domain, []).append(f)

    # Wide fan-out: merge single-file groups into general.
    if len(groups) > 6:
        for name in list(groups):
            if name != "general" and len(groups[name]) == 1:
                groups.setdefault("general", []).extend(groups.pop(name))

    proposed = [{"domain": k, "files": v} for k, v in sorted(groups.items())]
    print(json.dumps({"proposed_groups": proposed}))


try:
    main()
except Exception as e:  # noqa: BLE001 — never tolerant-fail into an empty grouping
    try:
        files = load_state().get("changed_files") or []
    except Exception:  # noqa: BLE001
        files = []
    print(json.dumps({"proposed_groups": [{"domain": "all-changes", "files": files}]}))
