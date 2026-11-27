#!/usr/bin/env bash
set -e

# @describe Read the contents of a file at the specified path.
# Use this when you need to examine the contents of an existing file.
# PNG, JPEG, GIF and WebP image files are attached to the result so a vision-capable model can view them.

# @option --path! The path of the file to read

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

main() {
    # shellcheck disable=SC2154
    local path="$argc_path"
    review_ground_path "$path" || return 0
    path="$REVIEW_GROUNDED_PATH"

    # An empty result is shown to the model as the opaque literal "DONE"; emit a note instead.
    if [[ -f "$path" && ! -s "$path" ]]; then
        echo "(empty file: $path)" >> "$LLM_OUTPUT"
        return 0
    fi

    if [[ -f "$path" ]]; then
        case "$path" in
            *.[pP][nN][gG]|*.[jJ][pP][gG]|*.[jJ][pP][eE][gG]|*.[gG][iI][fF]|*.[wW][eE][bB][pP])
                local bytes
                bytes=$(wc -c < "$path" | tr -d ' ')
                # The path goes out exactly as given: under Git Bash a rewritten
                # /c/... path would be unreadable to coyote.
                if [[ -n "${LLM_ATTACHMENTS:-}" ]]; then
                    printf '%s\n' "$path" >> "$LLM_ATTACHMENTS"
                    echo "(image file: $path, $bytes bytes; submitted for attachment, see attached_images or image_notes in this result)" >> "$LLM_OUTPUT"
                else
                    echo "(image file: $path, $bytes bytes; this tool cannot display images in this context)" >> "$LLM_OUTPUT"
                fi
                return 0
                ;;
        esac
    fi

    cat "$path" >> "$LLM_OUTPUT" 2>&1 || echo "No such file or path: $path" >> "$LLM_OUTPUT"
}
