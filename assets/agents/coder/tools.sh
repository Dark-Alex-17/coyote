#!/usr/bin/env bash
set -e

# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"

# @cmd Create a new file at the specified path with the given contents.
# @option --path! The path where the file should be created
# @option --contents! The contents of the file
# shellcheck disable=SC2154
fs_create() {
    guard_path "$argc_path" "Create '$argc_path'?"
    mkdir -p "$(dirname "$argc_path")"
    printf "%s" "$argc_contents" > "$argc_path"
    echo "File created: $argc_path" >> "$LLM_OUTPUT"
}
