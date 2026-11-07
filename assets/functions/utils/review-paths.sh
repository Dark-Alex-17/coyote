#!/usr/bin/env bash

# Path grounding for the read-only fs tools (fs_read, fs_cat, fs_grep,
# fs_glob, fs_ls, ast_grep) when they run inside a review lane.
#
# A review lane declares the `review_grounding` agent variable (default '1'),
# so the tools see LLM_AGENT_VAR_REVIEW_GROUNDING=1 next to
# LLM_AGENT_VAR_PROJECT_DIR, the checkout under review. The guard is inert,
# and every path passes through unchanged, when:
#   - the variable is not '1' (every non-review agent);
#   - project_dir is empty or the literal default `.`: the caller named no
#     checkout, so the working directory is not known to be the one under
#     review;
#   - project_dir is not inside a git checkout;
#   - project_dir is inside the same checkout as the working directory:
#     relative reads already land in it, and an inherited project_dir that
#     merely repeats the working directory says nothing about which checkout
#     is under review.
#
# Otherwise two rules apply, and only these two:
#   1. A relative path resolves against project_dir, never against the
#      process working directory.
#   2. A path inside a DIFFERENT checkout whose `origin` remote is the same
#      repository (a stale local clone, another worktree) is refused, naming
#      the review checkout, when the same file exists in the review checkout
#      or is tracked in that other clone. Untracked files only that clone has
#      (local notes, excluded plans) stay readable.
# Reads anywhere else, other repositories included, are unaffected.
#
# Usage:  review_ground_path "$path" || return 0; path="$REVIEW_GROUNDED_PATH"
# The resolved path lands in REVIEW_GROUNDED_PATH (no subshell, so a refusal
# written to $LLM_OUTPUT=/dev/stdout is never captured as the path). On refusal
# the message is appended to $LLM_OUTPUT and the function returns 1.

# Runs git inside directory $1. The directory is never passed to git as an
# argument: Git Bash rewrites an MSYS path argument (/c/x) for the native
# git.exe only when the argument holds no quote or glob character, so a
# checkout under /c/it's/x would reach git unconverted and the lookup would
# fail. `cd` is a shell builtin and hands git a native working directory
# whatever the path holds.
_review_git() {
    local dir="$1"
    shift
    (cd "$dir" 2>/dev/null && git "$@")
}

# Windows drive paths (C:\x, C:/x) become MSYS paths (/c/x) so every
# comparison below works on one form.
_review_posix() {
    local path="$1"
    if [[ "${OS:-}" == "Windows_NT" && "$path" =~ ^[A-Za-z]:[\\/] ]] && command -v cygpath >/dev/null 2>&1; then
        cygpath -u "$path"
        return 0
    fi
    printf '%s' "$path"
}

_review_expand_home() {
    local path="$1"
    case "$path" in
        \~) path="$HOME" ;;
        \~/*) path="$HOME/${path#\~/}" ;;
    esac
    printf '%s' "$path"
}

# Prints the normalized origin URL of the checkout at $1, or nothing. The
# lookup is cached per checkout root and refreshed when the repository's
# config file is newer than the cached entry.
_review_origin_of() {
    local root="$1" cache_dir key url common config
    cache_dir="${COYOTE_REVIEW_ORIGIN_CACHE:-${TMPDIR:-/tmp}/coyote-review-origins-$(id -u)}"
    key=$(printf '%s' "$root" | git hash-object --stdin 2>/dev/null) || key=""
    [[ -d "$cache_dir" ]] || mkdir -m 700 "$cache_dir" 2>/dev/null || true
    # Only a cache directory this user owns is used.
    [[ -n "$key" && -O "$cache_dir" ]] || cache_dir=""
    common=$(_review_git "$root" rev-parse --git-common-dir 2>/dev/null) || common=""
    [[ -z "$common" || "$common" == /* || "$common" =~ ^[A-Za-z]: ]] || common="$root/$common"
    config="$common/config"
    if [[ -n "$cache_dir" && -f "$cache_dir/$key" && ! "$config" -nt "$cache_dir/$key" ]]; then
        cat "$cache_dir/$key"
        return 0
    fi
    url=$(_review_git "$root" config --get remote.origin.url 2>/dev/null || true)
    url=$(_review_normalize_url "$url")
    if [[ -n "$cache_dir" ]]; then
        printf '%s' "$url" > "$cache_dir/$key.$$" 2>/dev/null &&
            mv -f "$cache_dir/$key.$$" "$cache_dir/$key" 2>/dev/null
    fi
    printf '%s' "$url"
}

# git@host:org/repo.git, ssh://git@host/org/repo, https://host/org/repo/ and
# a local path all reduce to one comparable form.
_review_normalize_url() {
    local url="$1"
    url="${url%/}"
    url="${url%.git}"
    case "$url" in
        *://*)
            url="${url#*://}"
            url="${url#*@}"
            ;;
        *@*:*)
            url="${url#*@}"
            url="${url/://}"
            ;;
    esac
    printf '%s' "$url" | tr '[:upper:]' '[:lower:]'
}

# Physical top-level directory of the git checkout containing $1 (a file or a
# directory; a missing path is looked up from its nearest existing ancestor).
_review_checkout_root() {
    local dir="$1" top
    [[ -d "$dir" ]] || dir=$(dirname "$dir")
    while [[ ! -d "$dir" && "$dir" != "/" && "$dir" != "." ]]; do
        dir=$(dirname "$dir")
    done
    top=$(_review_git "$dir" rev-parse --show-toplevel 2>/dev/null) || return 1
    [[ -n "$top" ]] || return 1
    top=$(_review_posix "$top")
    (cd "$top" 2>/dev/null && pwd -P)
}

# $1 with its nearest existing ancestor replaced by that ancestor's physical
# path, so it compares against `pwd -P` roots (/tmp is /private/tmp on macOS).
_review_physical() {
    local path="$1" rest=""
    while [[ ! -d "$path" && "$path" != "/" && "$path" != "." ]]; do
        rest="/$(basename "$path")$rest"
        path=$(dirname "$path")
    done
    printf '%s%s' "$(cd "$path" 2>/dev/null && pwd -P)" "$rest"
}

# A relative path missing from the checkout often carries a leading directory
# the checkout does not have (`billing/internal/x.go` for `internal/x.go`).
# Name the checkout file it most likely means; the read itself still fails.
_review_missing_hint() {
    local base="$1" rel="$2" tail
    [[ -e "$base/$rel" ]] && return 0
    tail="$rel"
    while [[ "$tail" == */* ]]; do
        tail="${tail#*/}"
        if [[ -e "$base/$tail" ]]; then
            echo "Note: $rel is not in the review checkout $base; did you mean $base/$tail?" >> "$LLM_OUTPUT"
            return 0
        fi
    done
}

# shellcheck disable=SC2034 # REVIEW_GROUNDED_PATH is the result the sourcing tool reads
review_ground_path() {
    local path="$1"
    REVIEW_GROUNDED_PATH="$path"
    local project_dir="${LLM_AGENT_VAR_PROJECT_DIR:-}"
    if [[ "${LLM_AGENT_VAR_REVIEW_GROUNDING:-}" != "1" || -z "$project_dir" || "$project_dir" == "." || "$project_dir" == "./" ]]; then
        return 0
    fi
    project_dir=$(_review_posix "$(_review_expand_home "$project_dir")")
    local base review_root cwd_root
    base=$(cd "$project_dir" 2>/dev/null && pwd -P) || return 0
    review_root=$(_review_checkout_root "$base") || return 0
    cwd_root=$(_review_checkout_root "$(pwd -P)") || cwd_root=""
    [[ "$cwd_root" != "$review_root" ]] || return 0

    path=$(_review_posix "$(_review_expand_home "$path")")
    case "$path" in
        /* | [A-Za-z]:[\\/]*) ;;
        "" | . | ./) path="$base" ;;
        *)
            local rel_in="${path#./}"
            path="$base/$rel_in"
            _review_missing_hint "$base" "$rel_in"
            ;;
    esac
    REVIEW_GROUNDED_PATH="$path"

    local target_root
    target_root=$(_review_checkout_root "$path") || return 0
    [[ "$target_root" != "$review_root" ]] || return 0
    local review_origin
    review_origin=$(_review_origin_of "$review_root")
    [[ -n "$review_origin" && "$(_review_origin_of "$target_root")" == "$review_origin" ]] || return 0

    local rel counterpart
    rel=$(_review_physical "$path")
    rel="${rel#"$target_root"}"
    rel="${rel#/}"
    counterpart="$review_root${rel:+/$rel}"
    if [[ ! -e "$counterpart" ]]; then
        # Not in the review checkout: refuse only what the other clone tracks
        # (stale code), never its untracked local files.
        [[ -n "$rel" && -n "$(_review_git "$target_root" ls-files -- "$rel" 2>/dev/null | head -n 1)" ]] || return 0
        counterpart="$review_root"
    fi
    {
        echo "Refused: $path is in $target_root, another clone of the repository under review ($review_origin)."
        echo "It may not match the code being reviewed. Read the review checkout instead: $counterpart"
    } >> "$LLM_OUTPUT"
    return 1
}
