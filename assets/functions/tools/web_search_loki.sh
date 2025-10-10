#!/usr/bin/env bash
set -e

# @describe Perform a web search to get up-to-date information or additional context.
# Use this when you need current information or feel a search could provide a better answer.

# @option --query! The search query.

# @meta require-tools loki

# @env WEB_SEARCH_MODEL=gemini:gemini-2.5-flash The model for web-searching.
#
# supported loki models:
#   - gemini:gemini-2.0-*
#   - vertexai:gemini-*
#   - perplexity:*
#   - ernie:*
# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC2154
main() {
    client="${WEB_SEARCH_MODEL%%:*}"

    if [[ "$client" == "gemini" ]]; then
        export LOKI_PATCH_GEMINI_CHAT_COMPLETIONS='{".*":{"body":{"tools":[{"google_search":{}}]}}}'
    elif [[ "$client" == "vertexai" ]]; then
        export LOKI_PATCH_VERTEXAI_CHAT_COMPLETIONS='{
    "gemini-1.5-.*":{"body":{"tools":[{"googleSearchRetrieval":{}}]}},
    "gemini-2.0-.*":{"body":{"tools":[{"google_search":{}}]}}
}'
    elif [[ "$client" == "ernie" ]]; then
        export LOKI_PATCH_ERNIE_CHAT_COMPLETIONS='{".*":{"body":{"web_search":{"enable":true}}}}'
    fi

    loki -m "$WEB_SEARCH_MODEL" "$argc_query" >> "$LLM_OUTPUT"
}