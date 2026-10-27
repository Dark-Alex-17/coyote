"""Shared lane-selection signals over a diff - the ONE copy of the rules.

Consumers: review-gauntlet/scripts/signals.py (primary + re-review delta
signals) and any downstream watch-round delta classifier. All consumers must
answer "would this diff alone have selected the lane" on the same evidence
basis, so the surface patterns live here and nowhere else. Pure regex over the
changed-file list and the ADDED lines; no git, no LLM.
"""
import re

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
# Tenancy / ownership-shaped added lines. Multi-tenant services often enforce
# authorization by data ownership (org_id / account_id / customer_id checks,
# "does this row belong to the caller") rather than by auth-named paths, so a
# delta can re-order or weaken an ownership check without any AUTH_* hit. This
# signal exists for watch-round narrowing classifiers (tenancy_signal); a
# replay over three historical PR engagements showed exactly that shape; it
# is deliberately NOT part of classify() so the gauntlet's signal set is unchanged.
TENANCY_ADDED_RE = (
    r"owner|ownership|tenant|org[_-]?id|account[_-]?id|customer[_-]?id"
    r"|wrong[_ -]org|cross[_ -]tenant|authoriz|belongs"
)


def added_lines(difftext):
    """The '+' lines of a unified diff, minus the '+++' file headers."""
    return "\n".join(
        l for l in difftext.splitlines() if l.startswith("+") and not l.startswith("+++")
    )


def tenancy_signal(added):
    """True when the added lines carry tenancy/ownership-shaped text."""
    return bool(re.search(TENANCY_ADDED_RE, added, re.I))


def classify(files, added):
    """Lane-selection signals for one diff: `files` is the changed-path list,
    `added` the added-lines text (see added_lines)."""

    def any_path(pattern):
        return any(re.search(pattern, f, re.I) for f in files)

    return {
        "files": list(files),
        "touches_proto": any_path(PROTO_RE),
        "touches_migrations": any_path(MIGRATIONS_RE),
        "touches_auth": any_path(AUTH_PATH_RE) or bool(re.search(AUTH_ADDED_RE, added, re.I)),
        "touches_deps": any_path(DEPS_RE),
        "touches_exec": bool(re.search(EXEC_RE, added)),
        "consumer_surface": any_path(CONSUMER_RE),
        "docs_only": bool(files) and all(re.search(DOCS_RE, f, re.I) for f in files),
        "tests_docs_only": bool(files)
        and all(re.search(DOCS_RE, f, re.I) or re.search(TESTS_RE, f, re.I) for f in files),
    }
