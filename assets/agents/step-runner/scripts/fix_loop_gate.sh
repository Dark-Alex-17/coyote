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
max_fix_attempts=$(echo "$state" | jq -r '.max_fix_attempts // 2')
lint_ok=$(echo "$state" | jq -r '.lint_ok | if . == null then "true" else (. | tostring) end')
build_ok=$(echo "$state" | jq -r '.build_ok | if . == null then "true" else (. | tostring) end')
tests_ok=$(echo "$state" | jq -r '.tests_ok | if . == null then "true" else (. | tostring) end')
lint_output=$(echo "$state" | jq -r '.lint_output // ""')
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

if [[ "$lint_ok" != "true" ]]; then
  stage="lint"
  output="$lint_output"
elif [[ "$build_ok" != "true" ]]; then
  stage="build"
  output="$build_output"
elif [[ "$tests_ok" != "true" ]]; then
  stage="full test suite"
  output="$tests_output"
else
  stage="verification"
  output="fix_loop_gate was reached but no failing stage was recorded. Re-run verification."
fi

fix_instructions=$(printf '## Fix loop status (step-level attempt %d of %d)\n\nThe implementation passed the coder'"'"'s internal checks but failed step-level verification at the %s stage.\n\nOutput:\n```\n%s\n```\n\nIdentify the minimal fix and apply it. Do not refactor. Regressions in untouched code caused by this change are in scope.' \
  "$next_attempts" "$max_fix_attempts" "$stage" "$output")

jq -nc \
  --argjson n "$next_attempts" \
  --arg 'fi' "$fix_instructions" \
  '{
    "fix_attempts": $n,
    "fix_instructions": $fi,
    "lint_ok": true,
    "build_ok": true,
    "tests_ok": true,
    "_next": "implement"
  }'
