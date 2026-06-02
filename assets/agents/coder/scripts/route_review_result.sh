#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

review_clean=$(echo "$state" | jq -r '.review_clean // true')
review_attempts=$(echo "$state" | jq -r '.review_attempts // 0')
max_review_attempts=$(echo "$state" | jq -r '.max_review_attempts // 1')
review_notes=$(echo "$state" | jq -r '.review_notes // ""')

if [[ "$review_clean" == "true" ]]; then
  jq -nc '{"_next": "end_success"}'
  exit 0
fi

if (( review_attempts >= max_review_attempts )); then
  jq -nc \
    --arg n "$review_notes" \
    '{
      "_next": "end_success",
      "review_notes_unresolved": ("Shipped with unresolved review notes (budget exhausted):\n" + $n)
    }'
  exit 0
fi

next_review=$((review_attempts + 1))
fix_instr=$(printf '## Self-review feedback (attempt %d of %d)\n\nThe code review found concrete issues. Address them with minimal edits. Do not refactor unrelated code.\n\n%s' \
  "$next_review" "$max_review_attempts" "$review_notes")

jq -nc \
  --argjson n "$next_review" \
  --arg fi "$fix_instr" \
  '{
    "review_attempts": $n,
    "fix_instructions": $fi,
    "_next": "implement"
  }'
