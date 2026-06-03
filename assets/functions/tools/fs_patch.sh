#!/usr/bin/env bash
set -e

# @describe Apply a unified-diff patch to a file at the specified path. Use this for editing an existing file. It's the
# PREFERRED way to modify a file. Prefer this over fs_write whenever the file already exists: it sends less data,
# preserves unchanged content automatically, and is less prone to accidental data loss from full rewrites.
# Use fs_write only when you are creating a new file or doing a complete rewrite where most of the content changes.

# @option --path! The path of the file to apply the patch to
# @option --contents! The patch to apply to the file

# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"

# shellcheck disable=SC2154
main() {
    if [[ ! -f "$argc_path" ]]; then
        error "Unable to find the specified file: $argc_path"
        exit 1
    fi

    new_contents="$(patch_file "$argc_path" <(printf "%s" "$argc_contents"))"
    printf "%s" "$new_contents" | git diff --no-index "$argc_path" - || true

    guard_operation "Apply changes?"

    printf "%s" "$new_contents" > "$argc_path"

    info "Applied the patch to: $argc_path" >> "$LLM_OUTPUT"
}
