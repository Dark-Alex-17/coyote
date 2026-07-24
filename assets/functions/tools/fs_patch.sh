#!/usr/bin/env bash
set -e

# @describe Apply a unified-diff patch to a file at the specified path. Use this for editing an existing file. It's the
# PREFERRED way to modify a file. Prefer this over fs_write whenever the file already exists: it sends less data,
# preserves unchanged content automatically, and is less prone to accidental data loss from full rewrites.
# Use fs_write only when you are creating a new file or doing a complete rewrite where most of the content changes.
#
# CRITICAL — the patch is matched byte-for-byte. There is no fuzzy matching, no whitespace tolerance, and no context shift:
# - Context lines (prefixed with a single space) and removed lines (prefixed with '-') must equal the file content exactly.
#   If unsure, fs_cat the file first and copy the bytes verbatim into your patch.
# - JSON-escape the contents string ONCE. Each literal backslash in the file becomes \\ in the JSON contents string. So a
#   shell line containing s|\\"|"|g must appear in JSON as s|\\\\\"|\"|g — NOT s|\\\\\\\"|\\\"|g. Over-escaping backslashes
#   is the most common cause of "unable to apply patch" failures, especially in files with sed/jq/regex pipelines or
#   embedded Python with quoted strings.
# - Hunks are applied in order; the first hunk that fails aborts the whole patch — later hunks are NOT attempted.
# - If you've edited this file in earlier tool calls, fs_cat it again before composing the patch. A stale view of the file
#   produces context lines that no longer match.
# - On failure the error message names the failing hunk and shows the expected-vs-actual line. Fix that specific line and
#   retry — do not blindly resend a near-identical patch.
#
# For files with heavy escaping (sed/jq/regex pipelines, shell with embedded heredocs, deeply quoted strings), prefer
# fs_write over chained fs_patch hunks to replace the entire file with the full new contents (i.e. original content +
# your changes).

# @option --path! The path of the file to apply the patch to
# @option --content! The patch to apply to the file

# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"

# shellcheck disable=SC2154
main() {
    argc_contents="$(jq -r '.content' <<< "$LLM_TOOL_RAW_JSON")"
    argc_path="$(jq -r '.path' <<< "$LLM_TOOL_RAW_JSON")"

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
