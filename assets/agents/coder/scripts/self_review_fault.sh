#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

# self_review died (iteration cap, API failure) and the engine took its
# fallback edge, so design_notes is still the empty seed. The implementer's
# `Design: ASK -- <question>` lines live in design_trail; carry them, label
# stripped, so end_success still renders `Design: ASK -- <question>`.
printf '%s' "$state" | jq -c '
  def trimmed: sub("^\\s+"; "") | sub("\\s+$"; "");
  {
    "design_notes": (
      ((.design_trail // "") | tostring)
      | split("\n")
      | map(trimmed | select(startswith("Design: ASK -- ")) | ltrimstr("Design: "))
      | reduce .[] as $line ([]; if any(.[]; . == $line) then . else . + [$line] end)
      | join("\n")
    ),
    "review_notes": ("self-review did not run; " + ((.review_notes // "") | tostring)),
    "_next": "end_success"
  }'
