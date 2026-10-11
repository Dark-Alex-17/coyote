#!/usr/bin/env python3
"""Deterministic diff signals for the review gauntlet.

Pure git — no LLM judgment. Every signal is computed from the changed-file
list and the added lines of the diff. Failures never sink the gauntlet:
on any error the script reports `signals_error` and marks the signals
unavailable — downstream (build_items / default_lanes) treats that as
UNKNOWN surface and degrades WIDER (security on; probe when a recipe
exists), on top of caller context and forced lanes.

Re-review narrowing inputs: when the caller's prompt carries labeled
`Re-review delta:` / `Settled lanes:` lines (re-review rounds after fixes),
this script extracts them by REGEX (never trusting the parse LLM with a
structured field) and computes the SAME signal set over the delta range into
`delta_*` keys. build_items uses them to skip settled lanes the delta
provably cannot affect. Fail-open: a failed or missing delta computation
sets a `delta signals unavailable` sentinel and narrowing is disabled —
a broken delta never narrows the selection.
"""

import json
import os
import re
import subprocess
import time

def load_state():
    if path := os.environ.get("GRAPH_STATE_FILE"):
        with open(path) as f:
            return json.load(f)
    return json.loads(os.environ.get("GRAPH_STATE", "{}"))


state = load_state()
proj = os.path.expanduser(
    (state.get("project_dir_in") or "").strip() or state.get("project_dir") or "."
)
spec = (state.get("diff_spec") or "worktree").strip()
spec_raw = spec
# The parse stage should emit a bare revision token, but callers write prose
# and extraction drifts ("run get_diff --base origin/main" once reached
# `git diff` verbatim, killing the signals — and with them the security and
# probe lanes). Deterministically recover a usable spec before giving up.
_TOKEN = r"[\w./~^@{}-]+"
if spec not in ("", "worktree") and not re.fullmatch(
    rf"{_TOKEN}(\.\.\.?{_TOKEN})?", spec
):
    if m := re.search(rf"({_TOKEN}\.\.\.?{_TOKEN})", spec):
        spec = m.group(1)  # a revision range buried in prose
    elif m := re.search(rf"--base[= ]({_TOKEN})", spec):
        spec = f"{m.group(1)}...HEAD"  # the get_diff idiom


def git(*args):
    r = subprocess.run(
        ["git", "-C", proj, *args], capture_output=True, text=True, timeout=90
    )
    if r.returncode != 0:
        raise RuntimeError((r.stderr or r.stdout).strip()[:400])
    return r.stdout


def normalize_base(spec):
    """Bare refs diff from their merge-base with HEAD (three-dot semantics), so
    commits that landed on the base after branching never surface as changes.
    Explicit `..` ranges are the caller's choice and pass through untouched."""
    if ".." in spec:
        return spec
    try:
        return git("merge-base", spec, "HEAD").strip()
    except Exception:  # noqa: BLE001 — the plain diff below reports the real error
        return spec


# Shared surface patterns: the primary diff and the re-review delta are
# classified with the SAME rules, so "would the delta alone have selected
# this lane" (build_items' narrowing test) is answered on the same evidence
# basis the original selection used.
DOCS_RE = r"\.(md|rst|txt|adoc)$"
TESTS_RE = (
    r"(^|/)tests?/|(^|/)testdata/|(^|/)__tests__/|(^|/)spec/"
    r"|_test\.|\.test\.|(^|/)test_|_spec\.|\.spec\."
)
# auth(?!or) admits auth/authn/authz/oauth but not "author".
# Over-firing here is the safe direction: it only ADDS a lane.
AUTH_PATH_RE = (
    r"auth(?!or)|login|logout|token|session|permission|rbac|\bacl\b|credential|secret"
)
AUTH_ADDED_RE = r"auth(?!or)|password|token|secret|credential|jwt|session"
DEPS_RE = (
    r"go\.(mod|sum)$|package(-lock)?\.json$|yarn\.lock$|pnpm-lock\.yaml$"
    r"|Cargo\.(toml|lock)$|requirements[^/]*\.txt$|pyproject\.toml$"
    r"|Gemfile(\.lock)?$|pom\.xml$|build\.gradle|composer\.(json|lock)$"
)
EXEC_RE = (
    r"exec\.Command|subprocess|os/exec|Runtime\.getRuntime"
    r"|shell_exec|\beval\(|\bsystem\(|child_process"
)
CONSUMER_RE = (
    r"routes?|handlers?|\bapi\b|endpoints?|\bcmd/|\bcli\b|\.proto$"
    r"|openapi|swagger|controllers?|graphql"
)
PROTO_RE = r"\.proto$|buf\.(yaml|gen\.yaml)$"
MIGRATIONS_RE = r"migrations?/|\.sql$"


def diff_signals(spec_):
    """Changed files + lane-selection signals for one diff spec."""
    if spec_ in ("", "worktree"):
        names = git("diff", "--name-only", "HEAD")
        difftext = git("diff", "HEAD")
    else:
        base = normalize_base(spec_)
        names = git("diff", "--name-only", base)
        difftext = git("diff", base)
    files = [f.strip() for f in names.splitlines() if f.strip()]
    added = "\n".join(
        l for l in difftext.splitlines() if l.startswith("+") and not l.startswith("+++")
    )

    def any_path(pattern):
        return any(re.search(pattern, f, re.I) for f in files)

    return {
        "files": files,
        "touches_proto": any_path(PROTO_RE),
        "touches_migrations": any_path(MIGRATIONS_RE),
        "touches_auth": any_path(AUTH_PATH_RE)
        or bool(re.search(AUTH_ADDED_RE, added, re.I)),
        "touches_deps": any_path(DEPS_RE),
        "touches_exec": bool(re.search(EXEC_RE, added)),
        "consumer_surface": any_path(CONSUMER_RE),
        "docs_only": bool(files)
        and all(re.search(DOCS_RE, f, re.I) for f in files),
        "tests_docs_only": bool(files)
        and all(
            re.search(DOCS_RE, f, re.I) or re.search(TESTS_RE, f, re.I) for f in files
        ),
    }


out = {"project_dir": proj}

# Deterministic plan_context recovery: the parse LLM proved a lossy channel
# for verbatim spec text (observed replacing ~4 KB of pasted criteria with
# file paths + a summary sentence). When the raw prompt carries the callers'
# labeled plan block, cut it out verbatim and override the extraction — but
# only when the recovered block is LONGER, so a good extraction is never
# degraded. The adversary lane fails closed on whatever goes missing here.
_prompt = state.get("initial_prompt")
if isinstance(_prompt, str) and _prompt:
    _m = re.search(
        r"Plan / acceptance criteria:\s*(.+?)"
        r"(?=\n\s*(?:Local-run recipe|Forced lanes|Rigor:|Security posture:|Extra context"
        r"|Re-review delta|Settled lanes|Prior review decisions)\b|\Z)",
        _prompt,
        re.S | re.I,
    )
    if _m:
        _block = _m.group(1).strip()
        if len(_block) > len((state.get("plan_context") or "").strip()):
            out["plan_context"] = _block

# Re-review narrowing inputs — regex-extracted from the labeled prompt lines
# (structured fields; the LLM parse never touches them). A token that is not
# a usable revision spec is dropped rather than guessed.
_rr_spec = ""
_settled = []
if isinstance(_prompt, str) and _prompt:
    if m := re.search(
        r"^[ \t]*Re-review delta:[ \t]*(\S+)([^\n]*)$", _prompt, re.M | re.I
    ):
        tok = m.group(1).strip().rstrip(".,;:")
        rest = m.group(2).strip()
        if (
            tok.lower() not in ("none", "-", "(none)", "worktree")
            and re.fullmatch(rf"{_TOKEN}(\.\.\.?{_TOKEN})?", tok)
            # The line must be the bare token, optionally followed by a
            # parenthetical/em-dash remark — a prose sentence whose first
            # word happens to be a resolvable ref must never become the
            # narrowing basis.
            and (not rest or rest.startswith(("(", "—", "–", "--")))
        ):
            _rr_spec = tok
    if m := re.search(r"^[ \t]*Settled lanes:[ \t]*(.+)$", _prompt, re.M | re.I):
        _settled = [
            t
            for t in re.split(r"[,\s]+", m.group(1).strip())
            if t and t.lower() not in ("none", "-", "(none)")
        ]
out["rereview_spec"] = _rr_spec
out["settled_lanes"] = _settled

# retry_gate measures its wall-clock budget from this stamp; it is set
# before the git work so a failed diff still starts the clock.
if not state.get("gauntlet_started_at"):
    out["gauntlet_started_at"] = time.time()
try:
    sig = diff_signals(spec)
    files = sig.pop("files")
    out.update(
        {
            "changed_files": files[:200],
            "file_count": len(files),
            **sig,
            "signals_summary": f"{len(files)} file(s) changed (diff: {spec})"
            + (f" — spec normalized from {spec_raw!r}" if spec != spec_raw else ""),
        }
    )
except Exception as e:  # noqa: BLE001 — degraded signals must not sink the gauntlet
    what = f"diff spec {spec!r}" + (
        f", normalized from {spec_raw!r}" if spec != spec_raw else ""
    )
    out["signals_error"] = (
        f"NOTE: diff signals could not be computed ({what}: {e}); surface UNKNOWN — "
        "lane selection degrades WIDER (security lane added; probe eligible when a "
        "local-run recipe exists), plus caller context and forced lanes."
    )
    out["signals_summary"] = "signals unavailable"

# Delta signals for narrowing. Computed only when the primary signals
# succeeded (unavailable primary signals already force the widest selection,
# which narrowing must never undercut). A delta failure is fail-open: the
# sentinel below disables narrowing and the full selection runs.
if _rr_spec and not out.get("signals_error"):
    try:
        d = diff_signals(_rr_spec)
        out.update(
            {
                "delta_file_count": len(d["files"]),
                "delta_touches_auth": d["touches_auth"],
                "delta_touches_deps": d["touches_deps"],
                "delta_touches_exec": d["touches_exec"],
                "delta_consumer_surface": d["consumer_surface"],
                "delta_docs_only": d["docs_only"],
                "delta_tests_docs_only": d["tests_docs_only"],
                "delta_summary": f"{len(d['files'])} file(s) in re-review delta "
                f"(diff: {_rr_spec})",
            }
        )
    except Exception as e:  # noqa: BLE001 — a broken delta must never narrow
        out["delta_summary"] = f"delta signals unavailable ({str(e)[:200]})"

print(json.dumps(out))
