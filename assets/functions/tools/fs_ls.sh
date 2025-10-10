#!/usr/bin/env bash
set -e

# @describe List all files and directories at the specified path.

# @option --path! The path of the directory to list

# @env LLM_OUTPUT=/dev/stdout The output path

main() {
    # shellcheck disable=SC2154
    ls -1 "$argc_path" >> "$LLM_OUTPUT"
}