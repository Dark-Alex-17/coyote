#!/usr/bin/env bash
set -e

# @describe Remove the file or directory at the specified path.

# @option --path! The path of the file or directory to remove

# @env LLM_OUTPUT=/dev/stdout The output path

PROMPT_UTILS="${LLM_ROOT_DIR:-$(dirname "${BASH_SOURCE[0]}")/..}/functions/utils/prompt-utils.sh"
# shellcheck disable=SC1090
source "$PROMPT_UTILS"

# shellcheck disable=SC2154
main() {
    if [[ -f "$argc_path" ]]; then
        guard_path "$argc_path" "Remove '$argc_path'?"
        rm -rf "$argc_path"
    fi

    echo "Path removed: $argc_path" >> "$LLM_OUTPUT"
}
