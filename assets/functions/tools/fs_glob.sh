#!/usr/bin/env bash
set -e

# @describe Find files by glob pattern. Returns matching file paths sorted by modification time.
# Use this to discover files before reading them.

# @option --pattern! The glob pattern to match files against (e.g. "**/*.rs", "src/**/*.ts", "*.yaml")
# @option --path The directory to search in (defaults to current working directory)

# @env LLM_OUTPUT=/dev/stdout The output path

MAX_RESULTS=100

main() {
    # shellcheck disable=SC2154
    local glob_pattern="$argc_pattern"
    local search_path="${argc_path:-.}"

    if [[ ! -d "$search_path" ]]; then
        echo "Error: directory not found: $search_path" >> "$LLM_OUTPUT"
        return 1
    fi

    local results
    if command -v fd &>/dev/null; then
        results=$(fd --type f --glob "$glob_pattern" "$search_path" \
            --exclude '.git' \
            --exclude 'node_modules' \
            --exclude 'target' \
            --exclude 'dist' \
            --exclude '__pycache__' \
            --exclude 'vendor' \
            --exclude '.build' \
            2>/dev/null | head -n "$MAX_RESULTS") || true
    else
        results=$(find "$search_path" -type f -name "$glob_pattern" \
            -not -path '*/.git/*' \
            -not -path '*/node_modules/*' \
            -not -path '*/target/*' \
            -not -path '*/dist/*' \
            -not -path '*/__pycache__/*' \
            -not -path '*/vendor/*' \
            -not -path '*/.build/*' \
            2>/dev/null | head -n "$MAX_RESULTS") || true
    fi

    if [[ -z "$results" ]]; then
        echo "No files found matching: $glob_pattern" >> "$LLM_OUTPUT"
        return 0
    fi

    echo "$results" >> "$LLM_OUTPUT"

    local count
    count=$(echo "$results" | wc -l)
    if [[ "$count" -ge "$MAX_RESULTS" ]]; then
        printf "\n(Results limited to %s files. Use a more specific pattern.)\n" "$MAX_RESULTS" >> "$LLM_OUTPUT"
    fi
}
