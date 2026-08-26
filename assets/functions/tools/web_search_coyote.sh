#!/usr/bin/env bash
set -e

# @describe Perform a web search to get up-to-date information or additional context.
# Use this when you need current information or feel a search could provide a better answer.

# @option --query! The search query.

# @meta require-tools coyote

# @env WEB_SEARCH_MODEL=gemini:gemini-2.5-flash The model for web-searching.
#
# supported coyote models:
#   - gemini:gemini-2.0-*
#   - vertexai:gemini-*
#   - perplexity:*
#   - ernie:*
#   - claude:*                       (Anthropic native web_search server tool)
#   - openai:gpt-4o-search-preview   (and -mini-; requires an api-key openai
#                                     client — the codex OAuth path uses the
#                                     Responses API where this parameter
#                                     does not exist)
# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC2154
main() {
    client="${WEB_SEARCH_MODEL%%:*}"

    if [[ "$client" == "gemini" ]]; then
        export COYOTE_PATCH_GEMINI_CHAT_COMPLETIONS='{".*":{"body":{"tools":[{"google_search":{}}]}}}'
    elif [[ "$client" == "vertexai" ]]; then
        export COYOTE_PATCH_VERTEXAI_CHAT_COMPLETIONS='{
    "gemini-1.5-.*":{"body":{"tools":[{"googleSearchRetrieval":{}}]}},
    "gemini-2.0-.*":{"body":{"tools":[{"google_search":{}}]}}
}'
    elif [[ "$client" == "ernie" ]]; then
        export COYOTE_PATCH_ERNIE_CHAT_COMPLETIONS='{".*":{"body":{"web_search":{"enable":true}}}}'
    elif [[ "$client" == "claude" ]]; then
        export COYOTE_PATCH_CLAUDE_CHAT_COMPLETIONS='{".*":{"body":{"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":5}]}}}'
    elif [[ "$client" == "openai" ]]; then
        # Chat Completions native search exists only on the search-preview
        # models; the regex scopes the patch so other OpenAI models run
        # unpatched instead of erroring on an unsupported parameter.
        export COYOTE_PATCH_OPENAI_CHAT_COMPLETIONS='{"gpt-4o.*search-preview.*":{"body":{"web_search_options":{}}}}'
    fi

    coyote -m "$WEB_SEARCH_MODEL" "$argc_query" >> "$LLM_OUTPUT"
}