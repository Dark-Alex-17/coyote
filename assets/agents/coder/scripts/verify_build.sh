#!/usr/bin/env bash
set -uo pipefail

# shellcheck disable=SC1091
source "$(dirname "$0")/../../.shared/utils.sh"
# shellcheck disable=SC1091
source "$(dirname "$0")/gate_lib.sh"

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

project_dir=$(gate_workdir "$state")

if [[ -n "${BUILD_CMD:-}" ]]; then
  cmd="$BUILD_CMD"
else
  project_info=$(detect_project "$project_dir")
  cmd=$(echo "$project_info" | jq -r '.check // .build // ""')
fi

if [[ -z "$cmd" || "$cmd" == "null" ]]; then
  jq -nc '{
    "build_ok": true,
    "build_output": "(GATE NOT RUN: no build/check command configured or detected. This is NOT evidence that the build passed — set BUILD_CMD, and never report the build as verified.)",
    "_next": "verify_tests"
  }'
  exit 0
fi

if ! deadline=$(gate_deadline BUILD_TIMEOUT 900); then
  jq -nc '{
    "build_ok": false,
    "build_output": "(GATE NOT RUN: the build_timeout variable is not a whole number of seconds.)",
    "gate_error": ("verify_build did not run: build_timeout must be a whole number of seconds, got \"" + env.LLM_AGENT_VAR_BUILD_TIMEOUT + "\""),
    "_next": "end_failure"
  }'
  exit 0
fi

out_file=$(mktemp)
trap 'rm -f "$out_file"' EXIT
exit_code=0
run_gate_until "$project_dir" "$cmd" "$deadline" "$out_file" || exit_code=$?
output=$(cat "$out_file")
cmd=$(gate_ran "$cmd")
export GATE_DIR="$project_dir" GATE_DEADLINE="$deadline"

# Transcript goes to jq on stdin, not as an argument — a single argv string is
# capped at 128 KiB on Linux (MAX_ARG_STRLEN) and a verbose build exceeds it.
# See verify_tests.sh for the failure mode this prevents, and for why a
# timed-out gate ends the run instead of entering the fix loop.
if (( GATE_TIMED_OUT == 1 )); then
  trim_output "$output" | jq -Rsc \
    --arg cmd "$cmd" \
    '{
      "build_ok": false,
      "build_output": ("Ran: " + $cmd + "\nRan in: " + env.GATE_DIR + "\nTIMED OUT after " + env.GATE_DEADLINE + " s (build_timeout); the build was killed before it finished.\n\n" + .),
      "gate_error": ("verify_build TIMED OUT after " + env.GATE_DEADLINE + " s (build_timeout) before the build finished. This is not a red run: raise build_timeout."),
      "_next": "end_failure"
    }'
elif (( exit_code == 0 )); then
  trim_output "$output" | jq -Rsc \
    --arg cmd "$cmd" \
    '{
      "build_ok": true,
      "build_output": ("Ran: " + $cmd + "\nRan in: " + env.GATE_DIR + "\n\n" + .),
      "_next": "verify_tests"
    }'
else
  trim_output "$output" | jq -Rsc \
    --arg cmd "$cmd" \
    --argjson rc "$exit_code" \
    '{
      "build_ok": false,
      "build_output": ("Ran: " + $cmd + "\nRan in: " + env.GATE_DIR + "\nExit code: " + ($rc | tostring) + "\n\n" + .),
      "_next": "fix_loop_gate"
    }'
fi
