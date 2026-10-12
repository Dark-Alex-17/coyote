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
project_dir=$(resolve_gate_dir "$project_dir")

if [[ -n "${TEST_CMD:-}" ]]; then
  cmd="$TEST_CMD"
else
  project_info=$(detect_project "$project_dir")
  cmd=$(echo "$project_info" | jq -r '.test // ""')
fi

if [[ -z "$cmd" || "$cmd" == "null" ]]; then
  jq -nc '{
    "tests_ok": true,
    "tests_output": "(GATE NOT RUN: no test command configured or detected. This is NOT evidence that tests passed — set TEST_CMD, and never report the suite as green.)",
    "_next": "self_review"
  }'
  exit 0
fi

exit_code=0
output=$(cd "$project_dir" && eval "$cmd" 2>&1) || exit_code=$?

# The transcript reaches jq on stdin, never as an argument: Linux caps a single
# argv string at 128 KiB (MAX_ARG_STRLEN), and a green `cargo test --all` run
# on a large crate exceeds that. With `--arg out "$output"` the exec failed
# E2BIG (exit 126), the node fell back to fix_loop_gate with tests_ok still
# true, and the loop burned every fix attempt on a suite that had passed.
if (( exit_code == 0 )); then
  trim_output "$output" | jq -Rsc \
    --arg cmd "$cmd" \
    '{
      "tests_ok": true,
      "tests_output": ("Ran: " + $cmd + "\n\n" + .),
      "_next": "self_review"
    }'
else
  trim_output "$output" | jq -Rsc \
    --arg cmd "$cmd" \
    --argjson rc "$exit_code" \
    '{
      "tests_ok": false,
      "tests_output": ("Ran: " + $cmd + "\nExit code: " + ($rc | tostring) + "\n\n" + .),
      "_next": "fix_loop_gate"
    }'
fi
