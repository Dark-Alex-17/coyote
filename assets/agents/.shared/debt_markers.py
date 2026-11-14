"""Debt-marker table shared by debt_scan.py and debris.py.

debt_scan.py reports every added line that carries one of these markers;
debris.py spares a task id on such a line, because a debt marker is the one
sanctioned place for one. Both import this module, so the two can never
disagree about what a debt marker is.

Every pattern is written in the regex subset Python `re` and POSIX `grep -E`
read the same way (no `\\b`, `\\s`, `\\d`, lookarounds or non-capturing
groups, and a plain space rather than an escaped tab), so debt_scan.py can
generate its manual fallback grep (`GREP_PATTERN`, run over the `+` lines of
a diff) from the same table. Markers are anchored to a directive shape (an
attribute, a comment leader, a decorator, a test-runner call), so a
`.expect("...")` call or an `allow(` inside prose is not one.
"""

import re

# Openers of a line or block comment across common languages.
COMMENT_LEADERS = ("//", "#", "--", ";", r"/\*", r"\*", "<!--", "%", "'", "REM ")
LEADER = "(" + "|".join(COMMENT_LEADERS) + ") *"
NON_WORD = "[^A-Za-z0-9_.]"
WORD_END = "([^A-Za-z0-9_]|$)"

# (family, markers found anywhere on a line, markers that must start the line
# or follow a non-word character, markers that must follow a comment leader).
MARKER_FAMILIES = (
    (
        "lint/type suppression",
        (
            r"#!?\[(allow|expect)\(",
            r"cfg_attr\(",
            r"(@|\[)Suppress",
        ),
        (),
        (
            "@?no(lint|qa|sec|check|inspection)",
            "@?[A-Za-z_]+[-:] ?(disable|ignore|nocheck|expect-error)",
            "[A-Za-z_]+ disable=",
            "pragma[: ]+(warning +disable|no ?cover)",
        ),
    ),
    (
        "test skip/ignore",
        (
            r"#\[ignore",
            r"@([A-Za-z_]+\.)*(skip|skipIf|skipUnless|xfail|Skip|Ignore|Disabled)" + WORD_END,
            r"\[Ignore",
            'Skip *= *"',
        ),
        (
            r"(t|b|it|test|describe|context|suite|self|[a-z]*test)\."
            r"(skip|Skip|Skipf|SkipNow|skipTest|xfail|todo)\(",
            r"x(it|describe|test|context)\(",
        ),
        (),
    ),
    (
        "unimplemented/todo stubs",
        (
            r"(todo|unimplemented)!",
            "NotImplemented",
            r'panic\("(TODO|not implemented|unimplemented)',
        ),
        (r"TODO\(",),
        (),
    ),
    (
        "TODO/FIXME comments",
        (),
        (),
        ("(TODO|FIXME|XXX|HACK)" + WORD_END,),
    ),
)

FAMILY_NAMES = tuple(family[0] for family in MARKER_FAMILIES)
# One leader group for every comment-anchored marker keeps the generated grep
# short enough to read.
FREE = (
    "|".join(p for family in MARKER_FAMILIES for p in family[1])
    + "|"
    + LEADER
    + "("
    + "|".join(p for family in MARKER_FAMILIES for p in family[3])
    + ")"
)
BOUNDED = "|".join(p for family in MARKER_FAMILIES for p in family[2])
MARKER_ALTERNATION = "(" + FREE + "|(^|" + NON_WORD + ")(" + BOUNDED + "))"
# The grep reads the diff line with its `+` still on, so a line-start marker
# is the one right after that `+`.
GREP_PATTERN = "^\\+(.*(" + FREE + ")|(.*" + NON_WORD + ")?(" + BOUNDED + "))"
MARKER_RE = re.compile(MARKER_ALTERNATION)
