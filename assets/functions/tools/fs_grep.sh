#!/usr/bin/env bash
set -e

# @describe Search file contents using regular expressions. Returns matching file paths and lines.
# Use this to find relevant code before reading files. Much faster than reading files to search.
# --path accepts either a directory (recursive search with exclude rules applied) or a single file.

# @option --pattern! The regex pattern to search for in file contents
# @option --path The directory OR file to search in (defaults to current working directory)
# @option --include File pattern to filter by (e.g. "*.rs", "*.{ts,tsx}", "*.py"). Ignored when --path is a single file.

# @env LLM_OUTPUT=/dev/stdout The output path

MAX_RESULTS=50
MAX_LINE_LENGTH=2000

main() {
    # shellcheck disable=SC2154
    local search_pattern="$argc_pattern"
    local search_path="${argc_path:-.}"
    local include_filter="${argc_include:-}"

    if [[ ! -e "$search_path" ]]; then
        echo "Error: path not found: $search_path" >&2
        exit 1
    fi

    local grep_args=(-nH --color=never)

    if [[ -d "$search_path" ]]; then
        # Use -r (not -R) so symlinks to directories are NOT followed - this avoids
        # infinite loops on pathological symlink cycles (e.g. `ln -s . loop`).
        grep_args+=(-r)
        grep_args+=(
            --exclude-dir='.git'
            --exclude-dir='node_modules'
            --exclude-dir='target'
            --exclude-dir='dist'
            --exclude-dir='build'
            --exclude-dir='__pycache__'
            --exclude-dir='vendor'
            --exclude-dir='.build'
            --exclude-dir='.next'
            --exclude='*.min.js'
            --exclude='*.min.css'
            --exclude='*.map'
            --exclude='*.lock'
            --exclude='package-lock.json'
        )
        if [[ -n "$include_filter" ]]; then
            grep_args+=("--include=$include_filter")
        fi
    fi
    # If --path is a single file, --include and the exclude rules are ignored
    # (they only matter when recursing into a directory tree).

    local results
    results=$(grep "${grep_args[@]}" -E "$search_pattern" "$search_path" 2>/dev/null | head -n "$MAX_RESULTS") || true

    if [[ -z "$results" ]]; then
        echo "No matches found for: $search_pattern" >> "$LLM_OUTPUT"
        return 0
    fi

    echo "$results" | while IFS= read -r line; do
        if [[ ${#line} -gt $MAX_LINE_LENGTH ]]; then
            line="${line:0:$MAX_LINE_LENGTH}... (truncated)"
        fi

        echo "$line"
    done >> "$LLM_OUTPUT"

    local count
    count=$(echo "$results" | wc -l)
    if [[ "$count" -ge "$MAX_RESULTS" ]]; then
        printf "\n(Results limited to %s matches. Narrow your search with --include or a more specific pattern.)\n" "$MAX_RESULTS" >> "$LLM_OUTPUT"
    fi
}
