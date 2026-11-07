#!/usr/bin/env bash

# Path grounding for the read-only fs tools (fs_read, fs_cat, fs_grep,
# fs_glob, fs_ls) when they run inside a review lane.
#
# A review lane declares the `review_grounding` agent variable (default '1'),
# so the tools see LLM_AGENT_VAR_REVIEW_GROUNDING=1 next to
# LLM_AGENT_VAR_PROJECT_DIR, the checkout under review. Outside a review lane,
# or when project_dir does not name a git checkout, every path passes through
# unchanged.
#
# Inside a review lane two rules apply, and only these two:
#   1. A relative path resolves against the review checkout's project dir,
#      never against the process working directory.
#   2. A path inside a DIFFERENT checkout whose `origin` remote is the same
#      repository as the review checkout (a stale local clone, another
#      worktree) is refused with a message naming the review checkout.
# Reads anywhere else, other repositories included, are unaffected.
#
# Usage:  review_ground_path "$path" || return 0; path="$REVIEW_GROUNDED_PATH"
# The resolved path lands in REVIEW_GROUNDED_PATH (no subshell, so a refusal
# written to $LLM_OUTPUT=/dev/stdout is never captured as the path). On refusal
# the message is appended to $LLM_OUTPUT and the function returns 1.

# Prints the normalized origin URL of the checkout at $1, or nothing. The
# lookup is cached per checkout root, so repeated reads stay cheap.
_review_origin_of() {
    local root="$1" cache_dir key url
    cache_dir="${COYOTE_REVIEW_ORIGIN_CACHE:-${TMPDIR:-/tmp}/coyote-review-origins-$(id -u)}"
    key="${root//\//%}"
    [[ -d "$cache_dir" ]] || mkdir -m 700 "$cache_dir" 2>/dev/null || true
    # A cache directory someone else owns could forge an origin match.
    [[ -O "$cache_dir" ]] || cache_dir=""
    if [[ -n "$cache_dir" && -f "$cache_dir/$key" ]]; then
        cat "$cache_dir/$key"
        return 0
    fi
    url=$(git -C "$root" config --get remote.origin.url 2>/dev/null || true)
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
    top=$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null) || return 1
    [[ -n "$top" ]] || return 1
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
    if [[ "${LLM_AGENT_VAR_REVIEW_GROUNDING:-}" != "1" || -z "$project_dir" ]]; then
        REVIEW_GROUNDED_PATH="$path"
        return 0
    fi
    local base review_root
    base=$(cd "$project_dir" 2>/dev/null && pwd -P) || {
        REVIEW_GROUNDED_PATH="$path"
        return 0
    }
    review_root=$(_review_checkout_root "$base") || {
        REVIEW_GROUNDED_PATH="$path"
        return 0
    }

    case "$path" in
        /* | "~"*) ;;
        "" | .) path="$base" ;;
        *)
            local rel_in="${path#./}"
            path="$base/$rel_in"
            _review_missing_hint "$base" "$rel_in"
            ;;
    esac

    local target_root
    target_root=$(_review_checkout_root "$path") || {
        REVIEW_GROUNDED_PATH="$path"
        return 0
    }
    if [[ "$target_root" != "$review_root" ]]; then
        local review_origin
        review_origin=$(_review_origin_of "$review_root")
        if [[ -n "$review_origin" && "$(_review_origin_of "$target_root")" == "$review_origin" ]]; then
            local rel
            rel=$(_review_physical "$path")
            rel="${rel#"$target_root"}"
            rel="${rel#/}"
            {
                echo "Refused: $path is in $target_root, another clone of the repository under review ($review_origin)."
                echo "It may not match the code being reviewed. Read the review checkout instead: $review_root${rel:+/$rel}"
            } >> "$LLM_OUTPUT"
            return 1
        fi
    fi
    REVIEW_GROUNDED_PATH="$path"
}
