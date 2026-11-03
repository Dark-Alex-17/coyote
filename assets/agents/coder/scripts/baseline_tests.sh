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

# emit_baseline prints the node's state update from the BASELINE_* variables,
# reading the failing test ids on stdin. Every outcome goes on to implement:
# with no red baseline, verify_tests keeps its plain exit-code verdict.
emit_baseline() {
  jq -Rsc '
    (split("\n") | map(select(. != "")) | unique) as $ids
    | (env.BASELINE_EXIT | tonumber) as $rc
    | {
        "baseline_status": env.BASELINE_STATUS,
        "baseline_exit_code": $rc,
        "baseline_failures": $ids,
        "baseline_failure_hash": env.BASELINE_HASH,
        "baseline_note": (
          if env.BASELINE_STATUS != "red" then env.BASELINE_NOTE
          elif ($ids | length) > 0 then
            "These tests already failed before you changed anything (exit \($rc)):\n"
            + ($ids | join("\n"))
            + "\n\nDo not fix them unless the request asks you to, and do not write new tests that encode the behaviour a known-failing test rejects."
          else
            "The test suite already failed before you changed anything (exit \($rc)), and its failures could not be identified. Do not fix them unless the request asks you to."
          end),
        "_next": "implement"
      }'
}

export BASELINE_STATUS=none BASELINE_EXIT=0 BASELINE_HASH='' BASELINE_NOTE=''

project_dir=$(gate_workdir "$state")
cmd=$(gate_test_command "$project_dir")

if [[ -z "$cmd" ]]; then
  BASELINE_NOTE="No test command was configured or detected, so no baseline was recorded."
  printf '' | emit_baseline
  exit 0
fi

if ! deadline=$(gate_deadline TEST_TIMEOUT 1800); then
  BASELINE_NOTE="No baseline was recorded: the test_timeout variable is not a whole number of seconds."
  printf '' | emit_baseline
  exit 0
fi

out_file=$(mktemp)
trap 'rm -f "$out_file"' EXIT
exit_code=0
run_gate_until "$project_dir" "$cmd" "$deadline" "$out_file" || exit_code=$?

if (( GATE_TIMED_OUT == 1 )); then
  BASELINE_STATUS=timed_out
  BASELINE_NOTE="The baseline test run timed out after ${deadline} s (test_timeout), so no baseline was recorded."
  printf '' | emit_baseline
elif (( exit_code == 0 )); then
  BASELINE_STATUS=green
  BASELINE_NOTE="All tests passed before you changed anything."
  printf '' | emit_baseline
else
  BASELINE_STATUS=red
  BASELINE_EXIT=$exit_code
  BASELINE_HASH=$(gate_failure_hash "$out_file")
  gate_failing_tests "$out_file" | emit_baseline
fi
