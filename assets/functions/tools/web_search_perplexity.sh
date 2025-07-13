#!/usr/bin/env bash
set -e

# @describe Perform a web search using the Perplexity API to get up-to-date information or additional context.
# Use this when you need current information or feel a search could provide a better answer.

# @option --query! The search query.

# @env PERPLEXITY_API_KEY! Your Perplexity API key
# @env PERPLEXITY_WEB_SEARCH_MODEL=llama-3.1-sonar-small-128k-online The LLM model to use for the search
# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC2154
main() {
    curl -fsS -X POST https://api.perplexity.ai/chat/completions \
     -H "authorization: Bearer $PERPLEXITY_API_KEY" \
     -H "accept: application/json" \
     -H "content-type: application/json" \
     --data '
{
  "model": "'"$PERPLEXITY_WEB_SEARCH_MODEL"'",
  "messages": [
    {
      "role": "user",
      "content": "'"$argc_query"'"
    }
  ]
}
'  | \
        jq -r '.choices[0].message.content' \
        >> "$LLM_OUTPUT"
}
