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

# User feedback goes to jq on stdin, never as an argument: it is unbounded
# prose, and on Windows the entire jq command line is capped at 32 KiB.
printf '%s' "$fix_instructions" | jq -Rsc \
  '{
    "fix_instructions": .,
    "_next": "implement"
  }'
