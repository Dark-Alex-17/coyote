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

if [[ -n "${LLM_AGENT_VAR_TEST_COMMAND:-}" ]]; then
  cmd="$LLM_AGENT_VAR_TEST_COMMAND"
elif [[ -n "${TEST_CMD:-}" ]]; then
  cmd="$TEST_CMD"
else
  project_info=$(detect_project "$project_dir")
  cmd=$(echo "$project_info" | jq -r '.test // ""')
fi

if [[ -z "$cmd" || "$cmd" == "null" ]]; then
  jq -nc '{
    "tests_ok": true,
    "tests_output": "(GATE NOT RUN: no test command configured or detected. This is NOT evidence that tests passed; set the test_command variable or TEST_CMD, and never report the suite as green.)",
    "_next": "self_review"
  }'
  exit 0
fi

if ! deadline=$(gate_deadline TEST_TIMEOUT 1800); then
  jq -nc '{
    "tests_ok": false,
    "tests_output": "(GATE NOT RUN: the test_timeout variable is not a whole number of seconds.)",
    "gate_error": ("verify_tests did not run: test_timeout must be a whole number of seconds, got \"" + env.LLM_AGENT_VAR_TEST_TIMEOUT + "\""),
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

# The transcript reaches jq on stdin, never as an argument: Linux caps a single
# argv string at 128 KiB (MAX_ARG_STRLEN), and a green `cargo test --all` run
# on a large crate exceeds that. With `--arg out "$output"` the exec failed
# E2BIG (exit 126), the node fell back to fix_loop_gate with tests_ok still
# true, and the loop burned every fix attempt on a suite that had passed.
# A suite killed at its deadline did not fail: it ends the run with a
# timed-out verdict instead of sending the implementer to fix green code.
if (( GATE_TIMED_OUT == 1 )); then
  trim_output "$output" | jq -Rsc \
    --arg cmd "$cmd" \
    '{
      "tests_ok": false,
      "tests_output": ("Ran: " + $cmd + "\nRan in: " + env.GATE_DIR + "\nTIMED OUT after " + env.GATE_DEADLINE + " s (test_timeout); the suite was killed before it finished.\n\n" + .),
      "gate_error": ("verify_tests TIMED OUT after " + env.GATE_DEADLINE + " s (test_timeout) before the suite finished. This is not a red run: raise test_timeout or pass a narrower test_command."),
      "_next": "end_failure"
    }'
elif (( exit_code == 0 )); then
  trim_output "$output" | jq -Rsc \
    --arg cmd "$cmd" \
    '{
      "tests_ok": true,
      "tests_output": ("Ran: " + $cmd + "\nRan in: " + env.GATE_DIR + "\n\n" + .),
      "_next": "self_review"
    }'
else
  trim_output "$output" | jq -Rsc \
    --arg cmd "$cmd" \
    --argjson rc "$exit_code" \
    '{
      "tests_ok": false,
      "tests_output": ("Ran: " + $cmd + "\nRan in: " + env.GATE_DIR + "\nExit code: " + ($rc | tostring) + "\n\n" + .),
      "_next": "fix_loop_gate"
    }'
fi
