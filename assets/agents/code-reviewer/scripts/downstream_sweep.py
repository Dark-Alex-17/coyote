#!/usr/bin/env python3
"""Downstream-consumers sweep for the code-reviewer graph.

Runs only when consumer-facing contract surface changed. Extracts the most
specific changed contract tokens from the diff deterministically (route path
literals, proto message/service/rpc names, top-level definitions, and the
import paths of the changed files), greps sibling checkouts under
`consumers_root`, and runs a bounded org code search via gh.
Hits are reported for the synthesis to raise as 🔴 findings naming the
consuming repo + file:line. Degrades silently when unconfigured/offline;
public-library caveats are noted, not guessed at.
"""

import json
import os
import re
import shutil
import subprocess
import sys

# State, diffs and reports carry emoji and non-ASCII paths; never decode them
# with the platform default (cp1252 on Windows).
for _stream in (sys.stdin, sys.stdout, sys.stderr):
    if hasattr(_stream, "reconfigure"):
        _stream.reconfigure(encoding="utf-8", errors="replace")

def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path, encoding="utf-8") as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


state = load_state()
proj = os.path.abspath(state.get("project_dir") or ".")
spec = state.get("resolved_diff_spec") or "worktree"
consumers_root = os.path.expanduser((state.get("consumers_root") or "").strip())
github_org = (state.get("github_org") or "").strip()
# Plain text from git's own differ whatever the user's diff config (no
# colour, external driver or textconv), with paths root-relative when
# project_dir is a subdirectory and a fixed a/ b/ prefix pair.
DIFF_FLAGS = (
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
)

ROUTE_RE = re.compile(r'["\'](/[a-z0-9][a-z0-9/_{}.:-]{5,})["\']')
SCHEMA_RE = re.compile(r"^\s*(?:message|service|rpc)\s+([A-Z][A-Za-z0-9_]{5,})", re.M)
# Top-level definition keywords and the modifiers that may precede them,
# across common languages. A `func` may carry a method receiver `(...)`.
DEFINITION_KEYWORDS = (
    "def", "class", "fn", "func", "function", "type", "interface", "struct", "enum",
    "trait", "module", "const", "let", "var", "val", "object", "record", "protocol",
)
DEFINITION_MODIFIERS = (
    r"pub(?:\([^)]*\))?", "export", "default", "public", "static", "async", "abstract",
    "final", "open", "internal", "sealed", "data",
)
DEFINITION_RE = re.compile(
    r"^\s*(?:(?:" + "|".join(DEFINITION_MODIFIERS) + r")\s+)*"
    r"(?:func\s*\([^)]*\)\s*|(?:" + "|".join(DEFINITION_KEYWORDS) + r")\s+)"
    r"([A-Za-z][A-Za-z0-9_]{5,})",
    re.M,
)
# A consumer line that imports a module: the keyword table covers
# import/from/require/use/include/using/load forms.
IMPORT_LINE_ERE = "(^|[^A-Za-z0-9_])(import|from|require|use|include|using|load)([^A-Za-z0-9_]|$)"
TOKEN_CAP = 5
MIN_TOKEN = 6


def ere_escape(text):
    """`text` as a literal inside a POSIX extended regex."""
    return re.sub(r"([.\[\]()*+?{}|^$\\])", r"\\\1", text)


def import_tokens(paths):
    """Module tokens a consumer would import a changed file by: the path
    without its extension in slashed and dotted form, and the bare stem."""
    found = set()
    for p in paths:
        stem_path = os.path.splitext(p)[0]
        stem = os.path.basename(stem_path)
        if stem.startswith("_") or not stem:
            continue
        for tok in (stem_path, stem_path.replace("/", "."), stem):
            if len(tok) >= MIN_TOKEN:
                found.add(tok)
    return sorted(found, key=lambda t: (-len(t), t))[:TOKEN_CAP]

if not state.get("consumer_surface"):
    print(json.dumps({"downstream_notes": "(skipped — no consumer-facing contract surface in the diff)"}))
    raise SystemExit(0)


def normalize_base(spec):
    """Bare refs diff from their merge-base with HEAD (three-dot semantics), so
    commits that landed on the base after branching never surface as changes.
    Explicit `..` ranges are the caller's choice and pass through untouched, and
    so does a checkpoint commit on top of HEAD, whose merge-base would be HEAD."""
    if ".." in spec:
        return spec
    r = subprocess.run(
        ["git", "-C", proj, "merge-base", spec, "HEAD"],
        capture_output=True,
        encoding="utf-8",
        errors="replace",
        timeout=30,
    )
    if r.returncode != 0 or not r.stdout.strip():
        return spec
    base = r.stdout.strip()
    p = subprocess.run(
        ["git", "-C", proj, "rev-parse", "HEAD", f"{spec}^{{commit}}"],
        capture_output=True,
        encoding="utf-8",
        errors="replace",
        timeout=30,
    )
    head, commit = (p.stdout.split() + ["", ""])[:2] if p.returncode == 0 else ("", "")
    if base != head or head == commit:
        return base
    # a commit some ref holds is a real base ahead of HEAD, never a checkpoint
    held = subprocess.run(
        ["git", "-C", proj, "for-each-ref", "--count=1", "--contains", commit],
        capture_output=True,
        encoding="utf-8",
        errors="replace",
        timeout=30,
    )
    return spec if held.returncode == 0 and not held.stdout.strip() else base


def diff_text():
    if spec.startswith("-"):
        raise ValueError(
            f"diff spec {spec!r} is option-shaped; pass a revision, a range, or one "
            "of worktree/staged/HEAD~1"
        )
    fixed = {"staged": ["--cached"], "worktree": ["HEAD"], "HEAD~1": ["HEAD~1"]}
    args = fixed.get(spec) or [normalize_base(spec)]
    r = subprocess.run(
        ["git", "-C", proj, "diff", *DIFF_FLAGS, *args, "--"],
        capture_output=True, encoding="utf-8", errors="replace", timeout=90,
    )
    return r.stdout if r.returncode == 0 else ""


try:
    lines = diff_text().splitlines()
    changed = [
        l[1:] for l in lines
        if (l.startswith("+") or l.startswith("-")) and not l.startswith(("+++", "---"))
    ]
    paths = sorted({
        l[6:] for l in lines
        if l.startswith(("+++ b/", "--- a/"))
    })
    text = "\n".join(changed)
    families = {
        "route literals": set(ROUTE_RE.findall(text)),
        "schema names": set(SCHEMA_RE.findall(text)),
        "definitions": set(DEFINITION_RE.findall(text)),
    }
    tokens = sorted(set().union(*families.values()), key=lambda t: (-len(t), t))[:TOKEN_CAP]
    imports = import_tokens(paths)
    families["import paths"] = set(imports)

    notes = [
        "token families extracted: "
        + ", ".join(f"{name} {len(found)}" for name, found in families.items())
        + " (import paths are matched only on import-shaped lines of sibling checkouts; "
        "a family with 0 tokens was not searched)"
    ]
    if not tokens and not imports:
        notes.append("(no specific contract tokens extractable from the diff — sweep skipped)")
    else:
        if tokens:
            notes.append(f"contract tokens probed: {tokens}")
        if imports:
            notes.append(f"import paths probed: {imports}")
        if consumers_root and os.path.isdir(consumers_root):
            self_name = os.path.basename(proj)
            probes = [(tok, ["-wF", "-e", tok]) for tok in tokens] + [
                (tok, ["-E", "-e", f"{IMPORT_LINE_ERE}.*{ere_escape(tok)}"]) for tok in imports
            ]
            for tok, pattern in probes:
                r = subprocess.run(
                    ["grep", "-rIl", "--exclude-dir=.git", "--exclude-dir=node_modules",
                     "--exclude-dir=target", "--exclude-dir=vendor", *pattern, consumers_root],
                    capture_output=True, encoding="utf-8", errors="replace", timeout=60,
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
                    capture_output=True, encoding="utf-8", errors="replace", timeout=30, env=env,
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
