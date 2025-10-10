#!/usr/bin/env bash
set -e

# @describe Execute the shell command.
# @option --command! The command to execute.

# @env LLM_OUTPUT=/dev/stdout The output path

PROMPT_UTILS="${LLM_ROOT_DIR:-$(dirname "${BASH_SOURCE[0]}")/..}/utils/prompt-utils.sh"
# shellcheck disable=SC1090
source "$PROMPT_UTILS"

main() {
    guard_operation
    # shellcheck disable=SC2154
    eval "$argc_command" >> "$LLM_OUTPUT"
}