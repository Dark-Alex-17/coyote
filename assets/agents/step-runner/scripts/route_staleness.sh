#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

has_major=$(echo "$state" | jq -r '.has_major_deviation // false')

if [[ "${STEP_AUTOAPPROVE:-0}" == "1" ]]; then
  jq -nc '{"_next": "implement"}'
  exit 0
fi

if [[ "$has_major" == "true" ]]; then
  jq -nc '{"_next": "gate_deviation"}'
else
  jq -nc '{"_next": "implement"}'
fi
