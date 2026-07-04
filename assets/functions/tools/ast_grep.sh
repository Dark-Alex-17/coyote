#!/usr/bin/env bash
set -e

# @describe Structural code search using AST patterns (ast-grep). Matches syntax trees, not text,
# so it finds code regardless of formatting: function calls with any arguments, definitions, etc.
# Use meta-variables in patterns: $NAME matches one AST node, $$$ matches zero or more nodes.
# Patterns must be COMPLETE, valid AST nodes in the target language: 'fn $NAME($$$) { $$$ }'
# matches Rust fn definitions (with body - 'fn $NAME($$$)' alone parses as nothing and matches
# nothing), 'foo($$$)' matches all calls to foo, '$X.unwrap()' matches all unwrap calls.
# Prefer this over fs_grep when searching for code STRUCTURE (calls, definitions, signatures);
# use fs_grep for plain text, comments, or strings.

# @option --pattern! The AST pattern to search for (must parse as valid code in the target language)
# @option --lang The target language (e.g. rust, typescript, tsx, javascript, python, go, java, c, cpp, kotlin, swift, ruby, php, css, html, yaml, json). Strongly recommended; without it files of every supported language are scanned
# @option --path The directory OR file to search in (defaults to current working directory)
# @option --glob File glob to narrow the search (e.g. "src/**/*.rs", "!**/tests/**")

# @env LLM_OUTPUT=/dev/stdout The output path

MAX_RESULTS=100
MAX_OUTPUT_BYTES=32768

resolve_binary() {
    if command -v ast-grep &>/dev/null; then
        echo "ast-grep"
        return 0
    fi
    if command -v sg &>/dev/null && sg --version 2>/dev/null | grep -qi 'ast-grep'; then
        echo "sg"
        return 0
    fi
    return 1
}

main() {
    # shellcheck disable=SC2154
    local pattern="$argc_pattern"
    local lang="${argc_lang:-}"
    local search_path="${argc_path:-.}"
    local glob="${argc_glob:-}"

    local bin
    if ! bin=$(resolve_binary); then
        printf 'ast-grep is not installed. Fall back to fs_grep for this search.\nTo enable structural search, install ast-grep:\n  cargo install ast-grep --locked\n  brew install ast-grep\n  npm i -g @ast-grep/cli\n' >> "$LLM_OUTPUT"
        return 0
    fi

    if [[ ! -e "$search_path" ]]; then
        echo "Error: path not found: $search_path" >> "$LLM_OUTPUT"
        return 1
    fi

    local args=(run --pattern "$pattern" --color never --heading never)
    [[ -n "$lang" ]] && args+=(--lang "$lang")
    [[ -n "$glob" ]] && args+=(--globs "$glob")
    args+=("$search_path")

    local output exit_code=0
    output=$("$bin" "${args[@]}" 2>&1) || exit_code=$?

    if [[ -z "$output" ]]; then
        echo "No structural matches found for: $pattern" >> "$LLM_OUTPUT"
        return 0
    fi

    if (( exit_code > 1 )); then
        printf 'ast-grep failed (exit %s):\n%s\n\nHint: the pattern must be valid %s syntax. Meta-variables: $NAME (one node), $$$ (zero or more).\n' \
            "$exit_code" "$output" "${lang:-source}" >> "$LLM_OUTPUT"
        return 0
    fi

    local total
    total=$(wc -l <<< "$output")
    output=$(head -n "$MAX_RESULTS" <<< "$output" | head -c "$MAX_OUTPUT_BYTES")

    echo "$output" >> "$LLM_OUTPUT"
    if (( total > MAX_RESULTS )); then
        printf '\n(Showing %s of %s matching lines. Narrow with --glob, --lang, or a more specific pattern.)\n' \
            "$MAX_RESULTS" "$total" >> "$LLM_OUTPUT"
    fi
}
