#!/usr/bin/env bash
set -e

# @describe Search Wikipedia using the given search query.
# Use it to get detailed information about a public figure, interpretation of a complex scientific concept or in-depth connectivity of a significant historical event, etc.

# @option --query! The search query.

# @env LLM_OUTPUT=/dev/stdout The output path

# shellcheck disable=SC2154
main() {
    encoded_query="$(jq -nr --arg q "$argc_query" '$q|@uri')"
    base_url="https://en.wikipedia.org/w/api.php"
    url="$base_url?action=query&list=search&srprop=&srlimit=1&limit=1&srsearch=$encoded_query&srinfo=suggestion&format=json"
    json="$(curl -fsSL "$url")"
    # suggestion="$(echo "$json" | jq -r '.query.searchinfo.suggestion // empty')"
    title="$(echo "$json" | jq -r '.query.search[0].title // empty')"
    pageid="$(echo "$json" | jq -r '.query.search[0].pageid // empty')"

    if [[ -z "$title" || -z "$pageid" ]]; then
        echo "error: no results for '$argc_query'" >&2
        exit 1
    fi

    title="$(echo "$title" | tr ' ' '_')"
    url="$base_url?action=query&prop=extracts&explaintext=&titles=$title&exintro=&format=json"
    curl -fsSL "$url" | jq -r '.query.pages["'"$pageid"'"].extract' >> "$LLM_OUTPUT"
}
