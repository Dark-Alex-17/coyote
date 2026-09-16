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
#   - openai:gpt-4o*/gpt-4.1*/gpt-5*/o3*/o4*
#     OpenAI serves web search through two different mechanisms depending on
#     the wire API the client resolves (the script cannot pick the wire):
#     Chat Completions serves it via `web_search_options` on the chat-only
#     *-search-preview models, while the Responses API serves it via its
#     native `web_search` tool on regular models. Both request patches are
#     exported (request patching is wire-aware, so the resolved wire applies
#     exactly one) and openai searches run as an attempt chain: the
#     chat-completions mechanism first, then the Responses mechanism, and
#     the search fails only when both attempts fail.
# @env WEB_SEARCH_USE_CURRENT_MODEL=true Prefer the running session's model when it natively supports web search.
# @env LLM_OUTPUT=/dev/stdout The output path

# Returns 0 when the model natively supports web search via a known mechanism.
supports_native_search() {
    case "$1" in
    gemini:* | vertexai:gemini-* | perplexity:* | ernie:* | claude:*) return 0 ;;
    openai:gpt-4o* | openai:gpt-4.1* | openai:gpt-5* | openai:o3* | openai:o4*) return 0 ;;
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
        COYOTE_PATCH_OPENAI_CHAT_COMPLETIONS \
        COYOTE_PATCH_OPENAI_RESPONSES

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
        # Both patches are exported; request patching is wire-aware, so the
        # client's resolved wire applies exactly one of them. Each regex
        # scopes its patch to the models that actually support that
        # mechanism, so other OpenAI models run unpatched instead of
        # erroring on an unsupported parameter:
        #   - Chat Completions native search exists only on the chat-only
        #     *-search-preview models (`web_search_options`).
        #   - The Responses API serves search through its native
        #     `web_search` tool, which only some model families support.
        export COYOTE_PATCH_OPENAI_CHAT_COMPLETIONS='{"gpt-4o.*search-preview.*":{"body":{"web_search_options":{}}}}'
        export COYOTE_PATCH_OPENAI_RESPONSES='{"(gpt-4o|gpt-4\\.1|gpt-5|o3|o4).*":{"body":{"tools":[{"type":"web_search"}]}}}'
        ;;
    esac
}

# Maps an openai model to the one used for the chat-completions attempt:
# Chat Completions web search is served only by the *-search-preview models.
openai_chat_search_model() {
    case "${1#openai:}" in
    gpt-4o*search-preview*) echo "$1" ;;
    gpt-4o-mini*) echo "openai:gpt-4o-mini-search-preview" ;;
    *) echo "openai:gpt-4o-search-preview" ;;
    esac
}

# Maps an openai model to the one used for the Responses attempt: the
# *-search-preview models are chat-only and do not exist on /v1/responses,
# so they map back to their base models.
openai_responses_search_model() {
    case "${1#openai:}" in
    gpt-4o-mini-search-preview*) echo "openai:gpt-4o-mini" ;;
    gpt-4o*search-preview*) echo "openai:gpt-4o" ;;
    *) echo "$1" ;;
    esac
}

# OpenAI attempt chain: try the chat-completions mechanism first (works when
# the client's wire is chat); if that fails — e.g. the client resolved the
# responses wire, where the search-preview models do not exist — retry with
# the Responses mechanism on a responses-capable model. Fails when both
# attempts fail.
# shellcheck disable=SC2154
run_openai_search() {
    if coyote -m "$(openai_chat_search_model "$1")" "$argc_query" >>"$LLM_OUTPUT"; then
        return 0
    fi
    echo "web_search: openai chat-completions attempt failed; retrying via the Responses API" >&2
    coyote -m "$(openai_responses_search_model "$1")" "$argc_query" >>"$LLM_OUTPUT"
}

# shellcheck disable=SC2154
run_search() {
    export_search_patch "$1"
    case "$1" in
    openai:*)
        run_openai_search "$1"
        ;;
    *)
        coyote -m "$1" "$argc_query" >>"$LLM_OUTPUT"
        ;;
    esac
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
