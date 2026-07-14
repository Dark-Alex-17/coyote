#!/usr/bin/env bash
set -e

# @describe Read the contents of a file at the specified path.
# Use this when you need to examine the contents of an existing file.

# @option --path! The path of the file to read

# @env LLM_OUTPUT=/dev/stdout The output path

main() {
    # shellcheck disable=SC2154
    local path="$argc_path"

    # An empty result is shown to the model as the opaque literal "DONE"; emit a note instead.
    if [[ -f "$path" && ! -s "$path" ]]; then
        echo "(empty file: $path)" >> "$LLM_OUTPUT"
        return 0
    fi

    cat "$path" >> "$LLM_OUTPUT" 2>&1 || echo "No such file or path: $path" >> "$LLM_OUTPUT"
}
