#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

fix_attempts=$(echo "$state" | jq -r '.fix_attempts // 0')
max_fix_attempts=$(echo "$state" | jq -r '.max_fix_attempts // 3')
build_ok=$(echo "$state" | jq -r '.build_ok | if . == null then "true" else (. | tostring) end')
tests_ok=$(echo "$state" | jq -r '.tests_ok | if . == null then "true" else (. | tostring) end')
build_output=$(echo "$state" | jq -r '.build_output // ""')
tests_output=$(echo "$state" | jq -r '.tests_output // ""')

if (( fix_attempts >= max_fix_attempts )); then
  jq -nc \
    --argjson n "$fix_attempts" \
    '{
      "fix_attempts": $n,
      "_next": "end_failure"
    }'
  exit 0
fi

next_attempts=$((fix_attempts + 1))

if [[ "$build_ok" != "true" ]]; then
  fix_instructions=$(printf '## Fix loop status (attempt %d of %d)\n\nThe previous attempt failed the build.\n\nBuild output:\n```\n%s\n```\n\nIdentify the minimal fix and apply it. Do not refactor.' \
    "$next_attempts" "$max_fix_attempts" "$build_output")
elif [[ "$tests_ok" != "true" ]]; then
  fix_instructions=$(printf '## Fix loop status (attempt %d of %d)\n\nBuild passed but tests failed.\n\nTest output:\n```\n%s\n```\n\nIdentify the minimal fix and apply it. Do not refactor.' \
    "$next_attempts" "$max_fix_attempts" "$tests_output")
else
  fix_instructions=$(printf '## Fix loop status (attempt %d of %d)\n\nfix_loop_gate was reached but no failure was detected in state. Re-run the verification step.' \
    "$next_attempts" "$max_fix_attempts")
fi

jq -nc \
  --argjson n "$next_attempts" \
  --arg fi "$fix_instructions" \
  '{
    "fix_attempts": $n,
    "fix_instructions": $fi,
    "_next": "implement"
  }'
