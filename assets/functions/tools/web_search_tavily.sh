#!/usr/bin/env bash
set -e

# @describe Perform a web search using the Tavily API to get up-to-date information or additional context.
# Use this when you need current information or feel a search could provide a better answer.

# @option --query! The search query.

# @env TAVILY_API_KEY! Your Tavile API key
# @env LLM_OUTPUT=/dev/stdout The output path The output path

# shellcheck disable=SC2154
main() {
    curl -fsSL -X POST https://api.tavily.com/search \
        -H "content-type: application/json" \
        -d '
{
    "api_key": "'"$TAVILY_API_KEY"'",
    "query": "'"$argc_query"'",
    "include_answer": true
}' | \
    jq -r '.answer' >> "$LLM_OUTPUT"
}

