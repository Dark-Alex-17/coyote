#!/usr/bin/env bash
set -e

# @describe Read a TRUNCATED view of a file with line numbers, offset, and limit. For directories, lists entries.
# IMPORTANT: This tool truncates output — lines over 2000 chars are cut off, and output is capped at 2000 lines by default.
# If you need the FULL, untruncated contents of a file, use fs_cat instead.
# Use this tool when you want line numbers, want to read a specific section via --offset/--limit, or are scanning a large file.
# Use the grep tool to find specific content before reading, then read with offset to target the relevant section.
# PNG, JPEG, GIF and WebP image files are attached to the result so a vision-capable model can view them.

# @option --path! The absolute path to the file or directory to read
# @option --offset <INT> The line number to start reading from (1-indexed, default: 1)
# @option --limit <INT> The maximum number of lines to read (default: 2000)

# @env LLM_OUTPUT=/dev/stdout The output path
# @env LLM_ATTACHMENTS The file to append image paths to (one per line) to attach them to the result

# Review lanes ground relative paths in the checkout under review and refuse
# reads in another clone of the same repository (utils/review-paths.sh).
if [[ -n "${LLM_PROMPT_UTILS_FILE:-}" ]]; then
    _review_lib="$(dirname "$LLM_PROMPT_UTILS_FILE")/review-paths.sh"
else
    _review_lib="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/../utils/review-paths.sh"
fi
if [[ -f "$_review_lib" ]]; then
    # shellcheck source=/dev/null
    source "$_review_lib"
else
    review_ground_path() {
        if [[ "${LLM_AGENT_VAR_REVIEW_GROUNDING:-}" == "1" ]]; then
            echo "Refused: the review path guard ($_review_lib) is not installed; reinstall the built-in functions." >> "$LLM_OUTPUT"
            return 1
        fi
        REVIEW_GROUNDED_PATH="$1"
    }
fi

MAX_LINE_LENGTH=2000
MAX_BYTES=51200

main() {
    # shellcheck disable=SC2154
    local target="$argc_path"
    local offset="${argc_offset:-1}"
    local limit="${argc_limit:-2000}"
    review_ground_path "$target" || return 0
    target="$REVIEW_GROUNDED_PATH"

    if [[ ! -e "$target" ]]; then
        echo "Error: path not found: $target" >&2
        exit 1
    fi

    if [[ -d "$target" ]]; then
        ls -1 "$target" >> "$LLM_OUTPUT" 2>&1
        return 0
    fi

    if [[ -f "$target" && -s "$target" ]]; then
        case "$target" in
            *.[pP][nN][gG]|*.[jJ][pP][gG]|*.[jJ][pP][eE][gG]|*.[gG][iI][fF]|*.[wW][eE][bB][pP])
                local bytes
                bytes=$(wc -c < "$target" | tr -d ' ')
                # The path goes out exactly as given: under Git Bash a rewritten
                # /c/... path would be unreadable to coyote.
                if [[ -n "${LLM_ATTACHMENTS:-}" ]]; then
                    printf '%s\n' "$target" >> "$LLM_ATTACHMENTS"
                    echo "(image file: $target, $bytes bytes; submitted for attachment, see attached_images or image_notes in this result)" >> "$LLM_OUTPUT"
                else
                    echo "(image file: $target, $bytes bytes; this tool cannot display images in this context)" >> "$LLM_OUTPUT"
                fi
                return 0
                ;;
        esac
    fi

    local total_lines file_bytes
    # awk counts a final line that lacks a trailing newline; wc -l would undercount it by one.
    total_lines=$(awk 'END { print NR }' "$target" 2>/dev/null || echo 0)
    file_bytes=$(wc -c < "$target" 2>/dev/null || echo 0)

    if [[ "$total_lines" -eq 0 ]]; then
        echo "(file is empty: $target)" >> "$LLM_OUTPUT"
        return 0
    fi

    if [[ "$offset" -gt "$total_lines" ]]; then
        echo "(offset $offset is past the end of the file, which has $total_lines lines)" >> "$LLM_OUTPUT"
        return 0
    fi

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
        # `|| [[ -n "$line" ]]` keeps the final line when the file has no trailing newline.
        while IFS= read -r line || [[ -n "$line" ]]; do
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
