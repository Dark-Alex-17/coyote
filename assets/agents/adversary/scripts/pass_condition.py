"""The pass condition of a recorded verification command, shared by run_checks.py and verdict.py.

A line of the request (state `initial_prompt`) or of a parsed criterion's text
names a command only through a backticked span whose content equals the
command, compared with runs of whitespace collapsed and quote characters
treated as one character. A line that mentions the command without backticks,
or inside a longer span (`x --all | grep y`), does not name it. A naming line
states exit 0 when its prose (the line without any of its backticked spans)
matches CRITERION_PASS_RE and not CRITERION_FAIL_RE. A naming span states
another condition when its own clause matches STATED_RE (no output, a file
absent, exit 1); the clause runs from the end of the span to the next
backticked span, a '.' or ';' followed by whitespace, or the end of the line,
so a second command on the same line does not lend its condition. The
condition is "stated" only when no naming line states exit 0 and at least one
naming span states another condition; it is "exit 0" otherwise, so a command
named in passing ("run `x` after each edit") or named nowhere (the caller's
build/test/lint contract) is held to exit 0. The decision is made per wording,
never per tool or flag.
"""

import re

CRITERION_PASS_RE = re.compile(r"\b(pass(es|ing)?|succeed(s|ed)?|green|exits?\s+(with\s+)?(code\s+|status\s+)?0)\b", re.I)
CRITERION_FAIL_RE = re.compile(r"\b(fail(s|ed)?|exits?\s+(with\s+)?(code\s+|status\s+)?[1-9]|non-?zero|rejected|refuse[sd]?)\b", re.I)
# Matched against normalised prose, where an apostrophe has become a double quote.
# Words that ordinary instructions use in passing ("if it fails", "the missing
# case") are left out: a request line carrying them must not release a red run.
# "<verb> no <word>" takes only nouns for output, so "reports no warnings" stays
# a claim about the command succeeding.
STATED_RE = re.compile(
    r"\b((prints?|outputs?|shows?|returns?|reports?|lists?|finds?)\s+"
    r"(nothing|no\s+(output|match(es)?|lines?|results?|hits?|files?|changes?|differences?|diff))"
    r"|no\s+(output|match(es)?|lines|results)|is\s+empty|empty\s+output"
    r"|absent|does\s+not\s+exist|doesn\"t\s+exist|not\s+present"
    r"|exits?\s+(with\s+)?(code\s+|status\s+)?[1-9])\b",
    re.I,
)
EXIT_0 = "exit 0"
STATED = "stated"
QUOTES = str.maketrans({c: '"' for c in "'\u2018\u2019\u201c\u201d"})
SPAN_RE = re.compile(r"`([^`]*)`")
CLAUSE_END_RE = re.compile(r"`|[.;]\s")
MAX_LINE_CHARS = 200


def normalise(text):
    return " ".join(text.translate(QUOTES).split())


def states_exit_0(prose):
    return bool(CRITERION_PASS_RE.search(prose)) and not CRITERION_FAIL_RE.search(prose)


def clause_after(line, end):
    rest = line[end:]
    stop = CLAUSE_END_RE.search(rest)
    return normalise(rest[: stop.start()] if stop else rest)


def bounded(line):
    line = line.strip()
    if len(line) > MAX_LINE_CHARS:
        line = line[: MAX_LINE_CHARS - 3] + "..."
    return line


def resolve(cmd, criteria, request):
    """{"condition": EXIT_0 | STATED, "criteria": ids of the parsed criteria naming
    `cmd`, "line": the first naming line, or "" when nothing names it}.

    `criteria` is the parsed list of {id, text}; malformed items are ignored.
    """
    needle = normalise(cmd) if isinstance(cmd, str) else ""
    sources = []
    for c in criteria if isinstance(criteria, list) else []:
        if isinstance(c, dict) and isinstance(c.get("text"), str):
            sources.append((c.get("id") if isinstance(c.get("id"), str) else "", c["text"]))
    if isinstance(request, str):
        sources.append((None, request))
    ids, first, passes, other = [], "", False, False
    for cid, text in sources if needle else []:
        for line in text.splitlines():
            naming = [m for m in SPAN_RE.finditer(line) if normalise(m.group(1)) == needle]
            if not naming:
                continue
            first = first or bounded(line)
            passes = passes or states_exit_0(normalise(SPAN_RE.sub(" ", line)))
            other = other or any(STATED_RE.search(clause_after(line, m.end())) for m in naming)
            if cid is not None and cid not in ids:
                ids.append(cid)
    condition = STATED if other and not passes else EXIT_0
    return {"condition": condition, "criteria": ids, "line": first}
