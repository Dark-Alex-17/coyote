#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

review_report=$(echo "$state" | jq -r '.review_report // ""')
review_attempts=$(echo "$state" | jq -r '.review_attempts // 0')
max_review_attempts=$(echo "$state" | jq -r '.max_review_attempts // 1')

if ! grep -qF "🔴" <<< "$review_report"; then
  jq -nc '{"_next": "write_handoff"}'
  exit 0
fi

if (( review_attempts >= max_review_attempts )); then
  jq -nc '{"_next": "write_handoff"}'
  exit 0
fi

next_review=$((review_attempts + 1))
fix_instructions=$(printf '## Independent review findings (attempt %d of %d)\n\nAn independent reviewer flagged CRITICAL (🔴) findings. Address ONLY the 🔴 findings with minimal edits. Do not refactor unrelated code.\n\n%s' \
  "$next_review" "$max_review_attempts" "$review_report")

jq -nc \
  --argjson n "$next_review" \
  --arg 'fi' "$fix_instructions" \
  '{
    "review_attempts": $n,
    "fix_instructions": $fi,
    "needs_independent_review": false,
    "_next": "implement"
  }'
