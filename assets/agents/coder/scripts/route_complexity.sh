#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

complexity=$(echo "$state" | jq -r '.complexity_score // 0')

if [[ "${CODER_AUTOAPPROVE:-0}" == "1" ]]; then
  jq -nc '{"_next": "implement"}'
  exit 0
fi

if (( complexity >= 7 )); then
  jq -nc '{"_next": "gate_approval"}'
else
  jq -nc '{"_next": "implement"}'
fi
