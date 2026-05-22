#!/usr/bin/env bash
set -uo pipefail

# shellcheck disable=SC1091
source "$(dirname "$0")/../../.shared/utils.sh"

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

project_dir=$(echo "$state" | jq -r '.project_dir // "."')

if [[ -n "${TEST_CMD:-}" ]]; then
  cmd="$TEST_CMD"
else
  project_info=$(detect_project "$project_dir")
  cmd=$(echo "$project_info" | jq -r '.test // ""')
fi

if [[ -z "$cmd" || "$cmd" == "null" ]]; then
  jq -nc '{
    "tests_ok": true,
    "tests_output": "(no test command available for this project type)",
    "_next": "end_success"
  }'
  exit 0
fi

exit_code=0
output=$(cd "$project_dir" && eval "$cmd" 2>&1) || exit_code=$?

if (( exit_code == 0 )); then
  jq -nc \
    --arg out "$output" \
    --arg cmd "$cmd" \
    '{
      "tests_ok": true,
      "tests_output": ("Ran: " + $cmd + "\n\n" + $out),
      "_next": "end_success"
    }'
else
  jq -nc \
    --arg out "$output" \
    --arg cmd "$cmd" \
    --argjson rc "$exit_code" \
    '{
      "tests_ok": false,
      "tests_output": ("Ran: " + $cmd + "\nExit code: " + ($rc | tostring) + "\n\n" + $out),
      "_next": "fix_loop_gate"
    }'
fi
