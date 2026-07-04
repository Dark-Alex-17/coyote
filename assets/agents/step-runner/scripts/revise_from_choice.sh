#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

feedback=$(echo "$state" | jq -r '.user_feedback // ""')

if [[ -z "$feedback" ]]; then
  jq -nc '{"_next": "get_revision"}'
  exit 0
fi

fix_instructions=$(printf '## Revision requested by the user at the step approval gate\n\nAddress these comments with minimal edits, then the step re-verifies and the handoff is rewritten:\n\n%s' \
  "$feedback")

jq -nc \
  --arg 'fi' "$fix_instructions" \
  '{
    "fix_instructions": $fi,
    "_next": "implement"
  }'
