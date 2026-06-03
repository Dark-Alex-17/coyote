#!/usr/bin/env bash
set -e

# @describe Execute the shell command. DO NOT use this to write files — use fs_write (new files) or fs_patch (edits) instead. Shell-based file writes (cat >, echo >, printf >, tee, heredocs, python -c "open(...)") break on multi-line content, special characters, quoted strings, and nested language blocks.
# @option --command! The command to execute.

# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"

main() {
    guard_operation
    local script
    script="$(mktemp)"
    # shellcheck disable=SC2064
    trap "rm -f '$script'" EXIT
    # shellcheck disable=SC2154
    printf '%s\n' "$argc_command" > "$script"
    bash "$script" >> "$LLM_OUTPUT"
}
