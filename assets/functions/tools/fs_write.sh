#!/usr/bin/env bash
set -e

# @describe Write the full file contents to a file at the specified path.

# @option --path! The path of the file to write to
# @option --contents! The full contents to write to the file

# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"

# shellcheck disable=SC2154
main() {
    if [[ -f "$argc_path" ]]; then
        printf "%s" "$argc_contents" | git diff --no-index "$argc_path" - || true
        guard_operation "Apply changes?"
    else
        guard_path "$argc_path" "Write '$argc_path'?"
        mkdir -p "$(dirname "$argc_path")"
    fi

    printf "%s" "$argc_contents" > "$argc_path"
    echo "The File contents were written to: $argc_path" >> "$LLM_OUTPUT"
}
