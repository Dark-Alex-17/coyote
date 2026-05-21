#!/usr/bin/env bash

set -e

# @env LLM_OUTPUT=/dev/stdout The output path

# @cmd Classify the credibility tier of a web source from its URL.
# A deterministic check based on the host and top-level domain. Use it
# to weigh how much trust to place in a source before relying on it.
# @option --url!  The full source URL to classify
classify_source() {
    # shellcheck disable=SC2154
    local url="$argc_url"
    local host="${url#*://}"
    host="${host%%/*}"
    host="${host##*@}"
    host="${host%%:*}"
    host="$(printf '%s' "$host" | tr '[:upper:]' '[:lower:]')"

    local tier
    case "$host" in
        '')
            tier="UNKNOWN - no host could be parsed from the URL" ;;
        *.gov | *.gov.* | *.mil)
            tier="HIGH - government source" ;;
        *.edu | *.edu.* | *.ac.*)
            tier="HIGH - academic institution" ;;
        arxiv.org | *.arxiv.org | biorxiv.org | *.biorxiv.org | medrxiv.org | *.medrxiv.org | ssrn.com | *.ssrn.com)
            tier="PREPRINT - not yet peer reviewed, corroborate before citing" ;;
        wikipedia.org | *.wikipedia.org)
            tier="TERTIARY - encyclopedia, good for orientation not citation" ;;
        *.org | *.org.*)
            tier="MEDIUM - organization site, check for institutional bias" ;;
        *)
            tier="UNVERIFIED - general web source, corroborate before citing" ;;
    esac

    printf '%s: %s\n' "${host:-<none>}" "$tier" >> "$LLM_OUTPUT"
}
