#!/usr/bin/env bash
set -e

# @describe Apply a patch to a file at the specified path.
# This can be used to edit a file without having to rewrite the whole file.

# @option --path! The path of the file to apply the patch to
# @option --contents! The patch to apply to the file

# @env LLM_OUTPUT=/dev/stdout The output path

PROMPT_UTILS="${LLM_ROOT_DIR:-$(dirname "${BASH_SOURCE[0]}")/..}/utils/prompt-utils.sh"
# shellcheck disable=SC1090
source "$PROMPT_UTILS"

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
