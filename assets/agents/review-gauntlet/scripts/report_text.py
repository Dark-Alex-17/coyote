"""One-line rendering for text the gauntlet writes into its report header.

Everything above `## Full lane reports` is read line by line with
`str.splitlines()`, so a value that is not a gate literal (model-written
reasons, fault text, names taken from state) must never carry a line
boundary there: one forged heading or `<details>` line would end the header
early or stand in for a gate section.
"""

import re

# Every character str.splitlines() treats as a line boundary, not just \r\n.
_LINE_BREAKS = re.compile(r"[\n\r\x0b\x0c\x1c\x1d\x1e\x85\u2028\u2029]+")


def one_line(s):
    return _LINE_BREAKS.sub(" ", str(s)).strip()


def table_cell(s):
    """one_line for a derived lane-table cell; `|` becomes `/` so the text
    cannot open another column or row. Literal cells do not need it."""
    return one_line(s).replace("|", "/")
