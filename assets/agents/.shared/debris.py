#!/usr/bin/env python3
"""Mechanical implementation-debris lint over the ADDED lines of a diff.

Two rules, both decidable without judgment:

  - non-ascii-punctuation: typographic characters (dashes, ellipsis, arrows,
    comparison symbols, curly quotes) on ANY added line of a text file;
  - planning-artefact: plan/task vocabulary (task and plan ids, ruling
    numbers, review-round numbers, lane jargon) on added lines of prose files
    and on comment-shaped added lines of code files.

A line carrying the token `debris-exempt` is exempt from both rules. A task id
on the same line as a debt marker (`#[allow(`, `todo!`, `#[ignore`, ...) is
the one sanctioned place for one and is not reported.

This module is pure when imported: it never reads GRAPH_STATE. The per-agent
wrapper scripts own the state keys and the fail-closed output shape. Run
directly (`debris.py [--exclude <path>]... <project_dir> [diff_spec]`) it
prints the rendered hits and the summary; exit 1 on hits, 0 when clean, 2 on
error.
"""

import os
import re
import subprocess
import sys

# U+FFFD (the replacement character lossy decoding produces) is deliberately
# absent: it marks a decoding problem in the diff fetch, not punctuation the
# author typed.
UNICODE_NEEDLES = {
    "\u2014": ("em dash", "-- or a comma"),
    "\u2013": ("en dash", "- or --"),
    "\u2015": ("horizontal bar", "--"),
    "\u2011": ("non-breaking hyphen", "-"),
    "\u2212": ("minus sign", "-"),
    "\u2026": ("horizontal ellipsis", "..."),
    "\u2190": ("leftwards arrow", "<-"),
    "\u2191": ("upwards arrow", "the word up"),
    "\u2192": ("rightwards arrow", "->"),
    "\u2193": ("downwards arrow", "the word down"),
    "\u2194": ("left right arrow", "<->"),
    "\u21d0": ("leftwards double arrow", "<="),
    "\u21d2": ("rightwards double arrow", "=>"),
    "\u21d4": ("left right double arrow", "<=>"),
    "\u00a7": ("section sign", "section"),
    "\u00d7": ("multiplication sign", "x"),
    "\u2248": ("almost equal to", "~="),
    "\u2264": ("less-than or equal to", "<="),
    "\u2265": ("greater-than or equal to", ">="),
    "\u2260": ("not equal to", "!="),
    "\u201c": ("left double quotation mark", '"'),
    "\u201d": ("right double quotation mark", '"'),
    "\u2018": ("left single quotation mark", "'"),
    "\u2019": ("right single quotation mark", "'"),
}

# Case-insensitive, except the PLAN- prefix: lower-case `plan-<word>` is
# ordinary hyphenated English (plan-based, plan-review) and must not fire.
# Bare `round N` / `phase N` / `step N` are absent on purpose: too many
# legitimate uses.
VOCAB_NEEDLES = [
    re.compile(p, re.I)
    for p in (
        r"(?<![A-Za-z-])TASK-\d+",
        r"\b(?-i:PLAN)-[A-Za-z][\w-]*",
        r"\bruling \d+",
        r"\bT-[a-z]\b",
        r"\b(review|gauntlet|probe) round \d+",
        r"\(r\d+\)",
        r"\bbake-off\b",
        r"\bhand-off\b",
        r"\busage probe:",
        r"\bspec-first probe\b",
        r"\bplan table\b",
        r"\bper the design doc\b",
    )
]
TASK_ID_RE = re.compile(r"(?<![A-Za-z-])TASK-\d+", re.I)
# Attribute-anchored so a `.expect("...")` call or an `allow(` inside prose
# never spares a task id.
DEBT_MARKERS = (
    "#[allow(", "#![allow(", "#[expect(", "#![expect(", "cfg_attr(",
    "todo!", "unimplemented!", "#[ignore",
)
EXEMPT_TOKEN = "debris-exempt"

PROSE_EXTENSIONS = (".md", ".rst", ".txt", ".adoc", ".yaml", ".yml")
SKIPPED_EXTENSIONS = (
    ".lock", ".svg", ".snap", ".pdf", ".png", ".jpg", ".jpeg", ".gif", ".ico",
    ".woff", ".woff2", ".ttf", ".otf",
)
SKIPPED_PATH_RE = re.compile(r"(^|/)(fixtures|vectors|corpus|snapshots|testdata)(/|$)")
COMMENT_LINE_RE = re.compile(r"""^\s*(//|///|//!|#|--|/\*|\*|<!--|\"\"\"|''')""")
CONTROL_CHAR_RE = re.compile(r"[\x00-\x08\x0a-\x1f\x7f-\x9f\u2028\u2029]")

HUNK_HEADER_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")
# git appends a TAB after a name containing a space and C-quotes names with
# non-ASCII bytes, `"` or `\` (core.quotePath); both arms are non-greedy so the
# TAB stays outside the capture.
TARGET_HEADER_RE = re.compile(r'^\+\+\+ (?:"b/(.*?)"|b/(.*?)|(/dev/null))\t?$')
BINARY_MARKER_RE = re.compile(r'^Binary files .* and (?:"b/(.*)"|b/(.*)|/dev/null) differ$')
# triage_binaries rewrites git's marker for a binary it could not diff on its
# own into this line, which parse_diff counts instead of raising. It sits
# outside any hunk, where only git-generated lines occur, so file content can
# never produce it.
SKIPPED_BINARY_MARKER = "Binary file skipped: "
# `git diff` during an unresolved merge prints `* Unmerged path` (cached) or a
# combined diff (unstaged); neither carries the two-way headers parsed below.
MERGE_MARKERS = ("diff --cc ", "diff --combined ", "* Unmerged path ", "@@@ ")
C_ESCAPE_RE = re.compile(r'\\([0-3][0-7]{2}|[abtnvfr"\\])')
C_ESCAPES = {"a": 7, "b": 8, "t": 9, "n": 10, "v": 11, "f": 12, "r": 13, '"': 34, "\\": 92}

RULE_UNICODE = "non-ascii-punctuation"
RULE_VOCAB = "planning-artefact"
VOCAB_FIX = "state the reason in code terms; plan references belong in the commit message"

EXCERPT_CHARS = 120
RENDER_CAP = 25
FINDINGS_CAP = 200
BINARY_NAMES_CAP = 5
MAX_DIFF_BYTES = 20 * 1024 * 1024
GIT_TIMEOUT = 30
GLOB_CHARS = "*?["
# A backslash separates exclude segments only where the host spells paths
# with one; on unix it is an ordinary (if odd) directory character that the
# literal pathspec must keep.
BACKSLASH_IS_SEPARATOR = os.sep == "\\"

# The target-header regex only knows the a/ b/ prefix form; these pin it
# regardless of diff.noprefix / diff.mnemonicPrefix / diff.external in the
# user's config, and parse_diff raises when a hunk arrives without it.
# --no-relative keeps paths root-relative and the scan repository-wide when
# diff.relative is set and project_dir is a subdirectory.
DIFF_FLAGS = (
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--src-prefix=a/",
    "--dst-prefix=b/",
)
USAGE = (
    "usage: debris.py <project_dir> [diff_spec]\n"
    "  --exclude <path>  leave this repo-relative directory out of the diff (repeatable)"
)


def unquote_c(name):
    """Decode git's C-quoted path form (`\\303\\251`, `\\"`, `\\\\`, `\\t`) to the name on disk."""
    out = bytearray()
    pos = 0
    for m in C_ESCAPE_RE.finditer(name):
        out += name[pos : m.start()].encode("utf-8")
        esc = m.group(1)
        out.append(int(esc, 8) if len(esc) == 3 else C_ESCAPES[esc])
        pos = m.end()
    out += name[pos:].encode("utf-8")
    return out.decode("utf-8", errors="replace")


def header_path(quoted, plain):
    return unquote_c(quoted) if quoted is not None else plain


def binary_marker_path(line):
    """The b/ path of a `Binary files ... differ` line, or None when the line
    is not that marker or its target is /dev/null (a deleted binary)."""
    if m := BINARY_MARKER_RE.match(line):
        return header_path(m.group(1), m.group(2))
    return None


def display_path(path):
    """The path with control characters removed, so a name carrying a newline
    or an escape sequence never splits or colours a report row."""
    return CONTROL_CHAR_RE.sub("", path)


def parse_diff(diff_text):
    """Every ADDED line as (path, line_no, text), numbered from the hunk
    headers, plus the changed-file counts the summary reports.

    File headers are only recognised outside a hunk, so an added `++i` line
    (which renders as `+++i`) is never mistaken for a `+++ b/...` header.
    Binary files have no hunks and therefore yield nothing, except that a
    binary marker for a path the lint would scan (a `.gitattributes -diff`
    rule, or `GIT binary patch` under --binary) is an error: those lines exist
    and were not scanned. Only a marker triage_binaries has already cleared
    (SKIPPED_BINARY_MARKER) is counted as a skipped binary instead. A hunk
    whose file never presented a recognisable `+++` header is an error too,
    not a skip, as is any combined-diff marker: the diff is in a shape this
    parser cannot attribute, and silently dropping it would read as clean.
    """
    added = []
    changed_files = 0
    scannable_files = 0
    binaries_skipped = []
    path = None
    header = None
    target_seen = False
    in_hunk = False
    new_line = 0
    for raw in diff_text.split("\n"):
        line = raw[:-1] if raw.endswith("\r") else raw
        if line.startswith(MERGE_MARKERS):
            raise ValueError(
                f"{line[:60]!r} is an unresolved merge in the diff; resolve it before linting"
            )
        if line.startswith("diff --git "):
            path, target_seen, in_hunk = None, False, False
            header = line[len("diff --git ") :]
            changed_files += 1
            if not skipped_path(header.rstrip('"')):
                scannable_files += 1
            continue
        if m := HUNK_HEADER_RE.match(line):
            if not target_seen:
                raise ValueError(
                    f"hunk {line[:60]!r} arrived before any `+++ b/<path>` header; "
                    "the diff is not in the a/ b/ prefix form this lint parses"
                )
            new_line = int(m.group(1))
            in_hunk = True
            continue
        if not in_hunk:
            if m := TARGET_HEADER_RE.match(line):
                target_seen = True
                path = None if m.group(3) else display_path(header_path(m.group(1), m.group(2)))
                continue
            binary = binary_marker_path(line)
            if binary is None and line == "GIT binary patch" and header is not None:
                binary = header.rstrip('"')
            elif binary is None and line.startswith(SKIPPED_BINARY_MARKER):
                binaries_skipped.append(line[len(SKIPPED_BINARY_MARKER) :])
                continue
            if binary is not None and not skipped_path(binary):
                raise ValueError(
                    f"git reports {binary!r} as binary (a `.gitattributes -diff` rule?); "
                    "its added lines were not scanned, so this diff cannot be called clean"
                )
            continue
        if line.startswith("+"):
            if path is not None:
                added.append((path, new_line, line[1:]))
            new_line += 1
        elif line.startswith(" ") or line == "":
            new_line += 1
    return {
        "added": added,
        "changed_files": changed_files,
        "scannable_files": scannable_files,
        "binaries_skipped": binaries_skipped,
    }


def skipped_path(path):
    lower = path.lower()
    return bool(SKIPPED_PATH_RE.search(path)) or lower.endswith(SKIPPED_EXTENSIONS)


def vocab_applies(path, text):
    return path.lower().endswith(PROSE_EXTENSIONS) or bool(COMMENT_LINE_RE.match(text))


def excerpt_of(text):
    text = CONTROL_CHAR_RE.sub("", text).strip()
    if len(text) > EXCERPT_CHARS:
        return text[: EXCERPT_CHARS - 3] + "..."
    return text


def unicode_fix(text):
    seen = []
    for ch in text:
        if ch in UNICODE_NEEDLES and ch not in seen:
            seen.append(ch)
    return "; ".join(
        f"replace U+{ord(ch):04X} ({UNICODE_NEEDLES[ch][0]}) with {UNICODE_NEEDLES[ch][1]}"
        for ch in seen
    )


def vocab_hits(text):
    hits = []
    spared_task_id = any(marker in text for marker in DEBT_MARKERS)
    for needle in VOCAB_NEEDLES:
        for m in needle.finditer(text):
            if spared_task_id and TASK_ID_RE.fullmatch(m.group(0)):
                continue
            hits.append(m.group(0))
    return hits


def scan(diff_text):
    parsed = parse_diff(diff_text)
    findings = []
    total = 0
    exempt = 0
    scanned = 0

    def add(path, line_no, rule, text, fix):
        nonlocal total
        total += 1
        if len(findings) < FINDINGS_CAP:
            findings.append(
                {
                    "path": path,
                    "line": line_no,
                    "rule": rule,
                    "excerpt": excerpt_of(text),
                    "fix": fix,
                }
            )

    for path, line_no, text in parsed["added"]:
        if skipped_path(path):
            continue
        scanned += 1
        if EXEMPT_TOKEN in text:
            exempt += 1
            continue
        if fix := unicode_fix(text):
            add(path, line_no, RULE_UNICODE, text, fix)
        if vocab_applies(path, text) and (hits := vocab_hits(text)):
            fix = f"drop {', '.join(repr(h) for h in hits)}; {VOCAB_FIX}"
            add(path, line_no, RULE_VOCAB, text, fix)
    return {
        "findings": findings,
        "truncated": total - len(findings),
        "exempt": exempt,
        "scanned_lines": scanned,
        "changed_files": parsed["changed_files"],
        "scannable_files": parsed["scannable_files"],
        "binaries_skipped": parsed["binaries_skipped"],
    }


def classify_findings(value):
    """How a recorded `debris_findings` value reads to a verdict script.

    ("empty", 0) when there is nothing to report; ("hits", n) for a list of n
    dict findings; ("malformed", n) for anything else -- a non-list (n = 0) or
    a list carrying n non-dict items. Both verdict scripts block on malformed.
    """
    if not value:
        return "empty", 0
    if not isinstance(value, list):
        return "malformed", 0
    bad = sum(1 for f in value if not isinstance(f, dict))
    return ("malformed", bad) if bad else ("hits", len(value))


def render(findings):
    """One line per finding, capped at RENDER_CAP; the overflow count is folded
    into the last line so every rendered line is a finding."""
    findings = [f for f in (findings or []) if isinstance(f, dict)]
    lines = [
        f"Implementation debris -- {f.get('path', '?')}:{f.get('line', '?')} -- "
        f"{f.get('rule', '?')}: {f.get('excerpt', '')} -- fix: {f.get('fix', '')}"
        for f in findings[:RENDER_CAP]
    ]
    if len(findings) > RENDER_CAP:
        lines[-1] += f" (and {len(findings) - RENDER_CAP} more hit(s) not shown)"
    return lines


def summary(result):
    findings = result.get("findings") or []
    scanned = result.get("scanned_lines", 0)
    exempt = result.get("exempt", 0)
    binaries = result.get("binaries_skipped") or []
    if not binaries:
        binary_note = ""
    elif len(binaries) <= BINARY_NAMES_CAP:
        binary_note = f", binary skipped: {', '.join(binaries)}"
    else:
        binary_note = f", {len(binaries)} binary file(s) skipped"
    if not findings:
        if not scanned and result.get("scannable_files"):
            return (
                "debris: clean (0 added line(s) scanned across "
                f"{result.get('changed_files', 0)} changed file(s){binary_note})"
            )
        extra = f", {exempt} debris-exempt line(s)" if exempt else ""
        return f"debris: clean ({scanned} added line(s) scanned{extra}{binary_note})"
    files = len({f.get("path") for f in findings if isinstance(f, dict)})
    truncated = result.get("truncated", 0)
    kept = f" (first {len(findings)} kept)" if truncated else ""
    return (
        f"debris: {len(findings) + truncated} hit(s){kept} in {files} file(s), "
        f"{exempt} debris-exempt line(s), {scanned} added line(s) scanned{binary_note}"
    )


def git(project_dir, *args, timeout=GIT_TIMEOUT, stdin=None):
    r = subprocess.run(
        ["git", "-C", project_dir, *args], input=stdin, capture_output=True, timeout=timeout
    )
    if r.returncode != 0:
        err = (r.stderr or r.stdout).decode("utf-8", errors="replace")
        raise RuntimeError(" ".join(err.split())[:400])
    if len(r.stdout) > MAX_DIFF_BYTES:
        raise RuntimeError(
            f"git {' '.join(args)} produced {len(r.stdout)} bytes, over the "
            f"{MAX_DIFF_BYTES}-byte cap; narrow the diff spec"
        )
    return r.stdout.decode("utf-8", errors="replace")


def check_excludes(exclude):
    """The excludes as repo-relative literal directories, or ValueError.

    Rejected: pathspec magic, glob characters, absolute paths, a `..` segment
    that could reach outside the project, and anything that normalises to the
    project root -- excluding everything prints the same clean summary an
    unchanged tree does. On Windows `plans\\sub` reads as `plans/sub`, the
    shape os.path.relpath produces there. A leading slash of either kind is
    refused by inspection, since os.path.isabs stops treating a lone leading
    slash as absolute on Windows from Python 3.13.
    """
    normalized = []
    for path in exclude:
        slashed = path.replace("\\", "/") if BACKSLASH_IS_SEPARATOR else path
        parts = [p for p in slashed.split("/") if p not in ("", ".")]
        if (
            not parts
            or ".." in parts
            or path.startswith((":", "/", "\\"))
            or os.path.isabs(slashed)
            or any(c in path for c in GLOB_CHARS)
        ):
            raise ValueError(
                f"exclude path {path!r} is not a plain repo-relative directory inside the project"
            )
        normalized.append("/".join(parts))
    return normalized


def git_diff(project_dir, *revs, timeout=GIT_TIMEOUT, exclude=()):
    """`exclude` directories become top-anchored literal `:(exclude)` pathspecs
    after `--`, with `:/` first so the diff spans the whole repository rather
    than the directory git was started in."""
    pathspecs = [":/", *(f":(top,literal,exclude){p}" for p in exclude)] if exclude else []
    return git(project_dir, "diff", *DIFF_FLAGS, *revs, "--", *pathspecs, timeout=timeout)


def check_attr(top, attr, paths, timeout=GIT_TIMEOUT):
    """{path: value} for one attribute, asked from the repository top level
    `top` because diff paths are root-relative; values are git's `set`,
    `unset`, `unspecified`, or the attribute's string value. Paths travel on
    stdin so a large diff never meets the argument-length limit."""
    stdin = "\0".join(paths).encode("utf-8")
    out = git(top, "check-attr", "-z", "--stdin", attr, timeout=timeout, stdin=stdin)
    fields = out.split("\0")
    return {fields[i]: fields[i + 2] for i in range(0, len(fields) - 2, 3)}


def driver_forces_binary(top, driver, timeout=GIT_TIMEOUT):
    """Whether `diff.<driver>.binary = true` made git print a binary marker
    regardless of content. A driver without that setting leaves the call to
    git's own content check, so its marker means a genuine binary."""
    out = git(
        top, "config", "--type=bool", "--default=false", f"diff.{driver}.binary",
        timeout=timeout,
    )
    return out.strip() == "true"


def triage_binaries(project_dir, diff_text, timeout=GIT_TIMEOUT):
    """Clear the binary markers git printed for files it could not diff on its
    own, so parse_diff counts them as skipped. Only `diff` unspecified or set
    means git decided on content; `unset` (a `.gitattributes -diff` rule) and
    a driver with `diff.<driver>.binary = true` are left for parse_diff to
    reject, since those lines may be text that was hidden. git_diff never
    asks for --binary, so `Binary files ... differ` is the only marker shape
    that reaches here.
    """
    lines = diff_text.split("\n")
    markers = {}
    for i, raw in enumerate(lines):
        line = raw[:-1] if raw.endswith("\r") else raw
        binary = binary_marker_path(line)
        if binary is not None and not skipped_path(binary):
            markers[i] = binary
    if not markers:
        return diff_text
    top = git(project_dir, "rev-parse", "--show-toplevel", timeout=timeout).strip()
    attrs = check_attr(top, "diff", sorted(set(markers.values())), timeout=timeout)
    forced = {}

    def hidden(value):
        if value in ("unspecified", "set"):
            return False
        if value in ("unset", None):
            return True
        if value not in forced:
            forced[value] = driver_forces_binary(top, value, timeout=timeout)
        return forced[value]

    for i, binary in markers.items():
        if not hidden(attrs.get(binary)):
            lines[i] = SKIPPED_BINARY_MARKER + display_path(binary)
    return "\n".join(lines)


def normalize_base(project_dir, spec, timeout):
    """Bare refs diff from their merge-base with HEAD (three-dot semantics), so
    commits that landed on the base after branching never surface as changes.
    Explicit `..` ranges are the caller's choice and pass through untouched."""
    if ".." in spec:
        return spec
    try:
        return git(project_dir, "merge-base", spec, "HEAD", timeout=timeout).strip()
    except Exception:  # noqa: BLE001 -- the plain diff below reports the real error
        return spec


def diff_text_for(project_dir, spec, timeout=GIT_TIMEOUT, exclude=()):
    """The diff text for `spec` in the union dialect of both review agents.

    ""/worktree -> HEAD; staged -> --cached; auto -> staged, else unstaged,
    else HEAD~1; X..Y / X...Y as given; a bare ref from its merge-base.
    Paths under any `exclude` directory are left out of the diff, and binary
    markers are triaged (see triage_binaries). Output is decoded as UTF-8
    with replacement so a locale codec can never mangle the characters the
    unicode rule looks for.
    """
    spec = (spec or "").strip()
    if spec.startswith("-"):
        raise ValueError(
            f"diff spec {spec!r} is option-shaped; pass a revision, a range, or one "
            "of worktree/staged/auto"
        )
    exclude = check_excludes(exclude)

    def diff(*revs):
        return git_diff(project_dir, *revs, timeout=timeout, exclude=exclude)

    if spec in ("", "worktree"):
        text = diff("HEAD")
    elif spec == "staged":
        text = diff("--cached")
    elif spec == "auto":
        text = diff("--cached")
        if not text.strip():
            text = diff()
        if not text.strip():
            text = diff("HEAD~1")
    else:
        text = diff(normalize_base(project_dir, spec, timeout))
    return triage_binaries(project_dir, text, timeout=timeout)


def parse_args(argv):
    """(project_dir, diff_spec, excludes) from argv; `--` ends the options."""
    excludes = []
    positional = []
    args = iter(argv[1:])
    for arg in args:
        if arg == "--":
            positional.extend(args)
        elif arg == "--exclude":
            value = next(args, None)
            if value is None:
                raise ValueError("--exclude needs a path")
            excludes.append(value)
        elif arg.startswith("--exclude="):
            excludes.append(arg[len("--exclude=") :])
        else:
            positional.append(arg)
    if not 1 <= len(positional) <= 2:
        raise ValueError(
            f"expected <project_dir> [diff_spec], got {len(positional)} positional argument(s)"
        )
    spec = positional[1] if len(positional) == 2 else "worktree"
    return positional[0], spec, excludes


def main(argv):
    # The hit lines quote the offending glyphs; an ASCII-only stdout (LC_ALL=C,
    # PYTHONIOENCODING=ascii) must still print them rather than traceback.
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            stream.reconfigure(errors="backslashreplace")
    try:
        project_dir, spec, excludes = parse_args(argv)
    except ValueError as e:
        print(f"debris: error: {e}\n{USAGE}", file=sys.stderr)
        return 2
    try:
        result = scan(diff_text_for(project_dir, spec, exclude=excludes))
    except Exception as e:  # noqa: BLE001 -- any failure is exit 2, never a clean 0
        print(f"debris: error: {e}", file=sys.stderr)
        return 2
    for line in render(result["findings"]):
        print(line)
    print(summary(result))
    return 1 if result["findings"] else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
