"""The pass condition of a recorded verification command, shared by run_checks.py and verdict.py.

A line of the request (state `initial_prompt`) or of a parsed criterion's text
names a command when it contains the command, compared with runs of whitespace
collapsed and backticks and quote characters treated as one character. A naming
line states exit 0 when its prose (the line without its backticked spans and
without the command) matches CRITERION_PASS_RE and not CRITERION_FAIL_RE. The
condition is "exit 0" when no line names the command (the caller's
build/test/lint contract) or any naming
line states exit 0, and "stated" otherwise (the criterion states some other
condition: no output, a file absent, exit 1). The decision is made per criterion
line, never per tool or flag.
"""

import re

CRITERION_PASS_RE = re.compile(r"\b(pass(es|ing)?|succeed(s|ed)?|green|exits?\s+(with\s+)?(code\s+|status\s+)?0)\b", re.I)
CRITERION_FAIL_RE = re.compile(r"\b(fail(s|ed)?|exits?\s+(with\s+)?(code\s+|status\s+)?[1-9]|non-?zero|rejected|refuse[sd]?)\b", re.I)
EXIT_0 = "exit 0"
STATED = "stated"
QUOTES = str.maketrans({c: '"' for c in "`'\u2018\u2019\u201c\u201d"})
MAX_LINE_CHARS = 200


def normalise(text):
    return " ".join(text.translate(QUOTES).split())


def states_exit_0(line, needle):
    prose = normalise(re.sub(r"`[^`]*`", " ", line)).replace(needle, " ")
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
    ids, first, passes = [], "", False
    for cid, text in sources if needle else []:
        for line in text.splitlines():
            if needle not in normalise(line):
                continue
            first = first or bounded(line)
            passes = passes or states_exit_0(line, needle)
            if cid is not None and cid not in ids:
                ids.append(cid)
    condition = STATED if first and not passes else EXIT_0
    return {"condition": condition, "criteria": ids, "line": first}
