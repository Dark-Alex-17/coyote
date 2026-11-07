#!/usr/bin/env bash
set -e

# @describe Read the contents of a file at the specified path.
# Use this when you need to examine the contents of an existing file.

# @option --path! The path of the file to read

# @env LLM_OUTPUT=/dev/stdout The output path

# Review lanes ground relative paths in the checkout under review and refuse
# reads in another clone of the same repository (utils/review-paths.sh).
_review_lib="$(dirname "${LLM_PROMPT_UTILS_FILE:-${BASH_SOURCE[0]%/*}/../utils/x}")/review-paths.sh"
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

    cat "$path" >> "$LLM_OUTPUT" 2>&1 || echo "No such file or path: $path" >> "$LLM_OUTPUT"
}
