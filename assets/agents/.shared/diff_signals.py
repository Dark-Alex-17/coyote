"""Shared lane-selection signals over a diff - the ONE copy of the rules.

Consumers: review-gauntlet/scripts/signals.py (primary + re-review delta
signals) and any downstream watch-round delta classifier. All consumers must
answer "would this diff alone have selected the lane" on the same evidence
basis, so the surface patterns live here and nowhere else. Pure regex over the
changed-file list and the ADDED lines; no git, no LLM.
"""
import re

# `.txt` is prose only under a docs dir or as a README/CHANGELOG-style name,
# so build inputs such as CMakeLists.txt and requirements.txt are not docs.
DOCS_RE = (
    r"\.(md|rst|adoc)$|(^|/)docs?/.*\.txt$"
    r"|(^|/)(readme|changelog|changes|history|license|licence|copying|notice|authors|contributing)[^/]*\.txt$"
)
# `spec/` is a test root only at the top level or inside a test dir; a spec/
# elsewhere (an OpenAPI or design spec) is code or docs.
TESTS_RE = (
    r"(^|/)tests?/|(^|/)testdata/|(^|/)__tests__/|^spec/"
    r"|_test\.|\.test\.|(^|/)test_|_spec\.|\.spec\."
)
# auth(?!or(?!i[sz])) admits auth/authn/authz/oauth/authoriz(e|ation) but not
# "author". Deliberately wider than code-reviewer's touches_auth (token,
# session, secret; tests and docs included).
# Over-firing here is the safe direction: it only ADDS a lane.
AUTH_PATH_RE = (
    r"auth(?!or(?!i[sz]))|login|logout|token|session|permission|rbac|\bacl\b|credential|secret"
)
# Superset of code-reviewer facts.py's content vocabulary plus token/session.
AUTH_ADDED_RE = (
    r"auth(?!or(?!i[sz]))|password|token|secret|credential|jwt|session|bearer"
    r"|permission|rbac|\bacl\b|csrf|saml|oidc|\bsso\b|\bmfa\b|\botp\b"
)
# Manifest and lockfile basenames, as anchored regex fragments. Any `*.lock`,
# `*-lock.*` or `*.lockfile` counts too, so a lockfile missing from the list
# still selects the lane.
DEPS_BASENAMES = (
    r"[^/]*\.lock",
    r"[^/]*-lock\.[^/]*",
    r"[^/]*\.lockfile",
    r"go\.(mod|sum|work|work\.sum)",
    r"package\.json",
    r"npm-shrinkwrap\.json",
    r"Cargo\.toml",
    r"requirements[^/]*\.(txt|in)",
    r"constraints[^/]*\.txt",
    r"pyproject\.toml",
    r"Pipfile",
    r"setup\.(py|cfg)",
    r"environment\.ya?ml",
    r"Gemfile",
    r"[^/]*\.gemspec",
    r"pom\.xml",
    r"build\.gradle(\.kts)?",
    r"settings\.gradle(\.kts)?",
    r"gradle\.properties",
    r"libs\.versions\.toml",
    r"build\.sbt",
    r"composer\.json",
    r"[^/]*\.(csproj|fsproj|vbproj)",
    r"packages\.config",
    r"Directory\.Packages\.props",
    r"[^/]*\.nuspec",
    r"Package\.swift",
    r"Podfile",
    r"Cartfile",
    r"pubspec\.yaml",
    r"mix\.exs",
    r"rebar\.config",
    r"[^/]*\.cabal",
    r"stack\.yaml",
    r"dune-project",
    r"[^/]*\.opam",
    r"deps\.edn",
    r"project\.clj",
    r"DESCRIPTION",
    r"cpanfile",
    r"vcpkg\.json",
    r"conanfile\.(txt|py)",
    r"flake\.nix",
    r"WORKSPACE(\.bazel)?",
    r"MODULE\.bazel",
)
DEPS_RE = r"(^|/)(" + "|".join(DEPS_BASENAMES) + r")$"
EXEC_RE = (
    r"exec\.Command|subprocess|os/exec|Runtime\.getRuntime"
    r"|shell_exec|\beval\(|\bsystem\(|child_process|Command::new|\bpopen\("
    r"|ProcessBuilder|Process\.Start|\bspawn\(|\bexec[lv]p?e?\("
)
CONSUMER_RE = (
    r"routes?|handlers?|\bapi\b|endpoints?|\bcmd/|\bcli\b|\.proto$"
    r"|openapi|swagger|controllers?|graphql"
    r"|(^|/)(servers?|views|urls|app|wsgi|asgi)(\.[^/]*)?(/|$)"
)
PROTO_RE = r"\.proto$|buf\.(yaml|gen\.yaml)$"
MIGRATIONS_RE = r"migrations?/|\.sql$"
# Tenancy / ownership-shaped added lines. Multi-tenant services often enforce
# authorization by data ownership (org_id / account_id / customer_id checks,
# "does this row belong to the caller") rather than by auth-named paths, so a
# delta can re-order or weaken an ownership check without any AUTH_* hit. A
# replay over three historical PR engagements showed exactly that shape. It is
# deliberately NOT part of classify() so the gauntlet's signal set is
# unchanged, and nothing calls tenancy_signal yet: it is kept here, shared-only,
# ahead of the watch-round narrowing classifiers it is meant for, so they
# import one definition instead of growing their own copy.
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
