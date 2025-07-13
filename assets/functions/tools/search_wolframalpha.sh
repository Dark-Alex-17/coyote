#!/usr/bin/env bash
set -e

# @describe Get an answer to a question using Wolfram Alpha. The input query should be in English.
# Use it to answer user questions that require computation, detailed facts, data analysis, or complex queries.

# @option --query! The search/computation query to pass to Wolfram Alpha

# @env WOLFRAM_API_ID! Your Wolfram Alpha API ID
# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC2154
main() {
    encoded_query="$(jq -nr --arg q "$argc_query" '$q|@uri')"
    url="https://api.wolframalpha.com/v2/query?appid=${WOLFRAM_API_ID}&input=$encoded_query&output=json&format=plaintext"

    curl -fsSL "$url" | jq '[.queryresult | .pods[] | {title:.title, values:[.subpods[].plaintext | select(. != "")]}]' \
    >> "$LLM_OUTPUT"
}
