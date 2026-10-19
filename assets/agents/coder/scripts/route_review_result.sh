#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

# `//` treats false as absent; null-check keeps the boolean so findings actually route to implement.
review_clean=$(echo "$state" | jq -r '.review_clean | if . == null then "true" else (. | tostring) end')
review_attempts=$(echo "$state" | jq -r '.review_attempts // 0')
max_review_attempts=$(echo "$state" | jq -r '.max_review_attempts // 1')
review_notes=$(echo "$state" | jq -r '.review_notes // ""')

if [[ "$review_clean" != "true" && "$review_clean" != "false" ]]; then
  echo "ERROR: review_clean must be boolean ('true'/'false'); got: $review_clean" >&2
  exit 1
fi

if ! [[ "$review_attempts" =~ ^[0-9]+$ ]]; then
  echo "ERROR: review_attempts must be a non-negative integer; got: $review_attempts" >&2
  exit 1
fi

if ! [[ "$max_review_attempts" =~ ^[0-9]+$ ]]; then
  echo "ERROR: max_review_attempts must be a non-negative integer; got: $max_review_attempts" >&2
  exit 1
fi

# end_success renders `Design: {{design_notes}}`; a null from the self-review
# would print as the literal word, so both end routes re-emit it as a string.
if [[ "$review_clean" == "true" ]]; then
  printf '%s' "$state" | jq -c '{"_next": "end_success", "design_notes": ((.design_notes // "") | tostring)}'
  exit 0
fi

if (( review_attempts >= max_review_attempts )); then
  printf '%s' "$state" | jq -c '{
    "_next": "end_success",
    "review_notes_unresolved": ("Shipped with unresolved review notes (budget exhausted):\n" + ((.review_notes // "") | tostring)),
    "design_notes": ((.design_notes // "") | tostring)
  }'
  exit 0
fi

next_review=$((review_attempts + 1))
fix_instr=$(printf '## Self-review feedback (attempt %d of %d)\n\nThe code review found concrete issues. Address them with minimal edits. Do not refactor unrelated code.\n\n%s' \
  "$next_review" "$max_review_attempts" "$review_notes")

# Review text goes to jq on stdin, never as an argument: on Windows the entire
# jq command line is capped at 32 KiB, and Linux's 128 KiB per-argument cap is
# the looser bound. Looping back to implement starts a fresh verify cycle, so
# the gate-crash bookkeeping is reset, and the next self_review refills
# design_notes from scratch.
printf '%s' "$fix_instr" | jq -Rsc \
  --argjson n "$next_review" \
  '{
    "review_attempts": $n,
    "fix_instructions": .,
    "design_notes": "",
    "gate_retries": 0,
    "gate_error": "",
    "last_script_error": "",
    "_next": "implement"
  }'
