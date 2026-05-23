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

if [[ -n "${BUILD_CMD:-}" ]]; then
  cmd="$BUILD_CMD"
else
  project_info=$(detect_project "$project_dir")
  cmd=$(echo "$project_info" | jq -r '.check // .build // ""')
fi

if [[ -z "$cmd" || "$cmd" == "null" ]]; then
  jq -nc '{
    "build_ok": true,
    "build_output": "(no build/check command available for this project type)",
    "_next": "verify_tests"
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
      "build_ok": true,
      "build_output": ("Ran: " + $cmd + "\n\n" + $out),
      "_next": "verify_tests"
    }'
else
  jq -nc \
    --arg out "$output" \
    --arg cmd "$cmd" \
    --argjson rc "$exit_code" \
    '{
      "build_ok": false,
      "build_output": ("Ran: " + $cmd + "\nExit code: " + ($rc | tostring) + "\n\n" + $out),
      "_next": "fix_loop_gate"
    }'
fi
