#!/usr/bin/env bash
set -uo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

handoff_path=$(echo "$state" | jq -r '.handoff_path // ""')
step_plan_path=$(echo "$state" | jq -r '.step_plan_path // ""')
handoff_attempts=$(echo "$state" | jq -r '.handoff_attempts // 0')

problems=""

if [[ ! -f "$handoff_path" ]]; then
  problems="- handoff file does not exist at $handoff_path"$'\n'
else
  content=$(cat "$handoff_path")
  grep -qE '^result:[[:space:]]*(complete|partial|blocked)' <<< "$content" \
    || problems+="- frontmatter is missing 'result: complete|partial|blocked'"$'\n'
  for section in "Summary" "Completed" "Not completed" "Deviations" "Downstream plan updates" "Edge cases discovered" "Evidence" "Notes for next step"; do
    grep -qE "^##[[:space:]]+${section}" <<< "$content" \
      || problems+="- missing required section: ## ${section}"$'\n'
  done
fi

if [[ -z "$problems" ]]; then
  if [[ -f "$step_plan_path" ]]; then
    tmp=$(mktemp)
    awk 'BEGIN{n=0} /^---[[:space:]]*$/{n++; print; next} n==1 && /^status:/{print "status: complete"; next} {print}' "$step_plan_path" > "$tmp" && mv "$tmp" "$step_plan_path"
  fi
  jq -nc '{"handoff_fix": "", "_next": "gate_user_review"}'
  exit 0
fi

if (( handoff_attempts >= 1 )); then
  jq -nc \
    --arg br "Handoff failed validation twice. Problems:
$problems" \
    '{"blocking_reason": $br, "_next": "end_failure"}'
  exit 0
fi

jq -nc \
  --arg hf "The previous handoff attempt failed validation. Fix exactly these problems:
$problems" \
  '{
    "handoff_attempts": 1,
    "handoff_fix": $hf,
    "_next": "write_handoff"
  }'
