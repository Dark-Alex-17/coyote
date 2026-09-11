#!/usr/bin/env bash
set -e

# @describe Perform a web search to get up-to-date information or additional context.
# Use this when you need current information or feel a search could provide a better answer.

# @option --query! The search query.

# @meta require-tools coyote

# @env WEB_SEARCH_MODEL=gemini:gemini-2.5-flash The fallback model for web-searching.
#
# Model selection: when WEB_SEARCH_USE_CURRENT_MODEL is true (the default) and
# the running session's model (COYOTE_CURRENT_MODEL, injected by coyote for
# every tool call) natively supports web search, the search runs on the current
# model first. If the current model is unsupported, or if the search fails at
# runtime, the tool falls back to WEB_SEARCH_MODEL. If that also fails, the
# tool fails.
#
# supported coyote models (native web search):
#   - gemini:*
#   - vertexai:gemini-*
#   - perplexity:*
#   - ernie:*
#   - claude:*                       (Anthropic native web_search server tool)
#   - openai:gpt-4o-search-preview   (and -mini-; requires an api-key openai
#                                     client — the codex OAuth path uses the
#                                     Responses API where this parameter
#                                     does not exist)
# @env WEB_SEARCH_USE_CURRENT_MODEL=true Prefer the running session's model when it natively supports web search.
# @env LLM_OUTPUT=/dev/stdout The output path

# Returns 0 when the model natively supports web search via a known mechanism.
supports_native_search() {
    case "$1" in
    gemini:* | vertexai:gemini-* | perplexity:* | ernie:* | claude:*) return 0 ;;
    openai:gpt-4o*search-preview*) return 0 ;;
    *) return 1 ;;
    esac
}

# Exports the request-body patch that enables native web search for the
# given model's client (clearing any patch left over from a prior attempt).
export_search_patch() {
    unset COYOTE_PATCH_GEMINI_CHAT_COMPLETIONS \
        COYOTE_PATCH_VERTEXAI_CHAT_COMPLETIONS \
        COYOTE_PATCH_ERNIE_CHAT_COMPLETIONS \
        COYOTE_PATCH_CLAUDE_CHAT_COMPLETIONS \
        COYOTE_PATCH_OPENAI_CHAT_COMPLETIONS

    case "${1%%:*}" in
    gemini)
        export COYOTE_PATCH_GEMINI_CHAT_COMPLETIONS='{".*":{"body":{"tools":[{"google_search":{}}]}}}'
        ;;
    vertexai)
        export COYOTE_PATCH_VERTEXAI_CHAT_COMPLETIONS='{
    "gemini-1.5-.*":{"body":{"tools":[{"googleSearchRetrieval":{}}]}},
    "gemini-2.0-.*":{"body":{"tools":[{"google_search":{}}]}}
}'
        ;;
    ernie)
        export COYOTE_PATCH_ERNIE_CHAT_COMPLETIONS='{".*":{"body":{"web_search":{"enable":true}}}}'
        ;;
    claude)
        export COYOTE_PATCH_CLAUDE_CHAT_COMPLETIONS='{".*":{"body":{"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":5}]}}}'
        ;;
    openai)
        # Chat Completions native search exists only on the search-preview
        # models; the regex scopes the patch so other OpenAI models run
        # unpatched instead of erroring on an unsupported parameter.
        export COYOTE_PATCH_OPENAI_CHAT_COMPLETIONS='{"gpt-4o.*search-preview.*":{"body":{"web_search_options":{}}}}'
        ;;
    esac
}

# shellcheck disable=SC2154
run_search() {
    export_search_patch "$1"
    coyote -m "$1" "$argc_query" >>"$LLM_OUTPUT"
}

main() {
    local fallback="${WEB_SEARCH_MODEL:-gemini:gemini-2.5-flash}"
    local current="${COYOTE_CURRENT_MODEL:-}"
    local primary=""

    if [[ "${WEB_SEARCH_USE_CURRENT_MODEL:-true}" == "true" && -n "$current" && "$current" != "$fallback" ]] &&
        supports_native_search "$current"; then
        primary="$current"
    fi

    if [[ -n "$primary" ]]; then
        if run_search "$primary"; then
            return 0
        fi
        echo "web_search: search with current model '$primary' failed; falling back to '$fallback'" >&2
    fi

    run_search "$fallback"
}
