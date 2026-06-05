#!/usr/bin/env bash
set -e

# @describe Write the FULL file contents to a file at the specified path. Use this for NEW files or COMPLETE rewrites
# only. For editing an existing file, prefer fs_patch. It's a surgical edit that preserves unchanged content, requires
# sending less data, and is less prone to accidental data loss.

# @option --path! The path of the file to write to
# @option --contents! The full contents to write to the file

# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"

# shellcheck disable=SC2154
main() {
    argc_contents="$(jq -r '.contents' <<< "$LLM_TOOL_RAW_JSON")"
    argc_path="$(jq -r '.path' <<< "$LLM_TOOL_RAW_JSON")"

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
