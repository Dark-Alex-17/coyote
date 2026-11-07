#!/usr/bin/env bash
set -e

# @describe List all files and directories at the specified path.

# @option --path! The path of the directory to list

# @env LLM_OUTPUT=/dev/stdout The output path

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
    local output
    review_ground_path "$path" || return 0
    path="$REVIEW_GROUNDED_PATH"

    if ! output=$(ls -1 "$path" 2>&1); then
        echo "$output" >> "$LLM_OUTPUT"
        return 0
    fi

    # An empty result is shown to the model as the opaque literal "DONE"; emit a note instead.
    if [[ -z "$output" ]]; then
        echo "(empty directory: $path)" >> "$LLM_OUTPUT"
    else
        echo "$output" >> "$LLM_OUTPUT"
    fi
}
