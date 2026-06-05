#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

findings=$(echo "$state" | jq -r '.findings // ""')

trimmed=$(echo "$findings" | awk '/^##+ [Ff]indings/{found=1} found{print}')

if [[ -z "$trimmed" ]]; then
  trimmed="$findings"
fi

jq -nc \
  --arg f "$trimmed" \
  '{
    "findings": $f,
    "_next": "end_success"
  }'
