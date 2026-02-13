#!/usr/bin/env bash
set -e

# @describe Read a file with line numbers, offset, and limit. For directories, lists entries.
# Prefer this over fs_cat for controlled reading. Use offset/limit to read specific sections.
# Use the grep tool to find specific content before reading, then read with offset to target the relevant section.

# @option --path! The absolute path to the file or directory to read
# @option --offset The line number to start reading from (1-indexed, default: 1)
# @option --limit The maximum number of lines to read (default: 2000)

# @env LLM_OUTPUT=/dev/stdout The output path

MAX_LINE_LENGTH=2000
MAX_BYTES=51200

main() {
    # shellcheck disable=SC2154
    local target="$argc_path"
    local offset="${argc_offset:-1}"
    local limit="${argc_limit:-2000}"

    if [[ ! -e "$target" ]]; then
        echo "Error: path not found: $target" >> "$LLM_OUTPUT"
        return 1
    fi

    if [[ -d "$target" ]]; then
        ls -1 "$target" >> "$LLM_OUTPUT" 2>&1
        return 0
    fi

    local total_lines file_bytes
    total_lines=$(wc -l < "$target" 2>/dev/null || echo 0)
    file_bytes=$(wc -c < "$target" 2>/dev/null || echo 0)

    if [[ "$file_bytes" -gt "$MAX_BYTES" ]] && [[ "$offset" -eq 1 ]] && [[ "$limit" -ge 2000 ]]; then
        {
            echo "Warning: Large file (${file_bytes} bytes, ${total_lines} lines). Showing first ${limit} lines."
            echo "Use --offset and --limit to read specific sections, or use the grep tool to find relevant lines first."
            echo ""
        } >> "$LLM_OUTPUT"
    fi

    local end_line=$((offset + limit - 1))

    sed -n "${offset},${end_line}p" "$target" 2>/dev/null | {
        local line_num=$offset
        while IFS= read -r line; do
            if [[ ${#line} -gt $MAX_LINE_LENGTH ]]; then
                line="${line:0:$MAX_LINE_LENGTH}... (truncated)"
            fi
            printf "%d: %s\n" "$line_num" "$line"
            line_num=$((line_num + 1))
        done
    } >> "$LLM_OUTPUT"

    if [[ "$end_line" -lt "$total_lines" ]]; then
        echo "" >> "$LLM_OUTPUT"
        echo "(${total_lines} total lines. Use --offset $((end_line + 1)) to read more.)" >> "$LLM_OUTPUT"
    fi
}
