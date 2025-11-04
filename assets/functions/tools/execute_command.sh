#!/usr/bin/env bash
set -e

# @describe Execute the shell command.
# @option --command! The command to execute.

# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"

main() {
    guard_operation
    # shellcheck disable=SC2154
    eval "$argc_command" >> "$LLM_OUTPUT"
}
