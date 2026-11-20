"""The pass condition of a recorded verification command, shared by run_checks.py and verdict.py.

A line of the request (state `initial_prompt`) or of a parsed criterion's text
names a command when it contains the command, compared with runs of whitespace
collapsed and backticks and quote characters treated as one character. A naming
line states exit 0 when its prose (the line without its backticked spans and
without the command) matches CRITERION_PASS_RE and not CRITERION_FAIL_RE, and
states another condition when that prose matches STATED_RE (no output, a file
absent, exit 1). The condition is "stated" only when no naming line states exit
0 and at least one states another condition; it is "exit 0" otherwise, so a
command named in passing ("run `x` after each edit") or named nowhere (the
caller's build/test/lint contract) is held to exit 0. The decision is made per
wording, never per tool or flag.
"""

import re

CRITERION_PASS_RE = re.compile(r"\b(pass(es|ing)?|succeed(s|ed)?|green|exits?\s+(with\s+)?(code\s+|status\s+)?0)\b", re.I)
CRITERION_FAIL_RE = re.compile(r"\b(fail(s|ed)?|exits?\s+(with\s+)?(code\s+|status\s+)?[1-9]|non-?zero|rejected|refuse[sd]?)\b", re.I)
# Matched against normalised prose, where an apostrophe has become a double quote.
# Words that ordinary instructions use in passing ("if it fails", "the missing
# case") are left out: a request line carrying them must not release a red run.
STATED_RE = re.compile(
    r"\b((prints?|outputs?|shows?|returns?|reports?|lists?|finds?)\s+(nothing|no\s+\w+)"
    r"|no\s+(output|match(es)?|lines|results)|is\s+empty|empty\s+output"
    r"|absent|does\s+not\s+exist|doesn\"t\s+exist|not\s+present"
    r"|exits?\s+(with\s+)?(code\s+|status\s+)?[1-9])\b",
    re.I,
)
EXIT_0 = "exit 0"
STATED = "stated"
QUOTES = str.maketrans({c: '"' for c in "`'\u2018\u2019\u201c\u201d"})
MAX_LINE_CHARS = 200


def normalise(text):
    return " ".join(text.translate(QUOTES).split())


def prose_of(line, needle):
    return normalise(re.sub(r"`[^`]*`", " ", line)).replace(needle, " ")


def states_exit_0(prose):
    return bool(CRITERION_PASS_RE.search(prose)) and not CRITERION_FAIL_RE.search(prose)


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
            if needle not in normalise(line):
                continue
            first = first or bounded(line)
            prose = prose_of(line, needle)
            passes = passes or states_exit_0(prose)
            other = other or bool(STATED_RE.search(prose))
            if cid is not None and cid not in ids:
                ids.append(cid)
    condition = STATED if other and not passes else EXIT_0
    return {"condition": condition, "criteria": ids, "line": first}
