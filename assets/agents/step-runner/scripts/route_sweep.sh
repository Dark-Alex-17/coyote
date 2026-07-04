#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

needs_review=$(echo "$state" | jq -r '.needs_independent_review // false')

if [[ "${STEP_SKIP_REVIEW:-0}" == "1" ]]; then
  jq -nc '{"_next": "write_handoff"}'
  exit 0
fi

if [[ "$needs_review" == "true" ]]; then
  jq -nc '{"_next": "independent_review"}'
else
  jq -nc '{"_next": "write_handoff"}'
fi
