#!/usr/bin/env bash
set -e

# @describe List all files and directories at the specified path.

# @option --path! The path of the directory to list

# @env LLM_OUTPUT=/dev/stdout The output path

main() {
    # shellcheck disable=SC2154
    local path="$argc_path"
    local output

    if ! output=$(ls -1 "$path" 2>&1); then
        echo "$output" >> "$LLM_OUTPUT"
        return 0
    fi

    # An empty result is shown to the model as the opaque literal "DONE"; emit a note instead.
    if [[ -z "$output" ]]; then
        echo "(empty directory: $path)" >> "$LLM_OUTPUT"
    else
        echo "$output" >> "$LLM_OUTPUT"
    fi
}
