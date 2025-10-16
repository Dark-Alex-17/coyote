#!/usr/bin/env bash
set -e

# @describe Execute sql code.
# @option --code! The code to execute.

# @meta require-tools usql

# @env USQL_DSN! The database connection url. e.g. pgsql://user:pass@host:port
# @env LLM_OUTPUT=/dev/stdout The output path

PROMPT_UTILS="${LLM_ROOT_DIR:-$(dirname "${BASH_SOURCE[0]}")/..}/functions/utils/prompt-utils.sh"
# shellcheck disable=SC1090
source "$PROMPT_UTILS"

# shellcheck disable=SC2154
main() {
    if ! grep -qi '^select' <<<"$argc_code"; then
        guard_operation ""
    fi
    usql -c "$argc_code" "$USQL_DSN" >> "$LLM_OUTPUT"
}
