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
cmd=$(gate_test_command "$project_dir")

if [[ -z "$cmd" ]]; then
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
ids_file=$(mktemp)
state_copy=$(mktemp)
trap 'rm -f "$out_file" "$ids_file" "$state_copy"' EXIT
exit_code=0
run_gate_until "$project_dir" "$cmd" "$deadline" "$out_file" || exit_code=$?
output=$(cat "$out_file")
cmd=$(gate_ran "$cmd")
export GATE_DIR="$project_dir" GATE_DEADLINE="$deadline" GATE_FAILURE_HASH=''

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
  exit 0
fi

printf '%s' "$state" >"$state_copy"
if (( exit_code != 0 )) && [[ $(printf '%s' "$state" | jq -r '.baseline_status // ""') == "red" ]]; then
  gate_failing_tests "$out_file" >"$ids_file"
  GATE_FAILURE_HASH=$(gate_failure_hash "$out_file")
fi

# A nonzero run passes only against a red baseline with the same exit code,
# and only when every failing id is a baseline id, or, when neither run named
# a test, when the failure output hashes alike. A run whose failures cannot
# be identified never passes against a baseline whose failures could.
trim_output "$output" | jq -Rsc \
  --arg cmd "$cmd" \
  --argjson rc "$exit_code" \
  --slurpfile st "$state_copy" \
  --rawfile ids "$ids_file" \
  '$st[0] as $s
  | (($s.baseline_status // "none") == "red") as $red
  | ($s.baseline_failures // [] | unique) as $base
  | ($ids | split("\n") | map(select(. != "")) | unique) as $cur
  | ($s.baseline_exit_code // 0) as $base_rc
  | (if $rc == 0 then true
     elif ($red | not) or $rc != $base_rc then false
     elif ($base | length) > 0 then ($cur | length) > 0 and ($cur - $base) == []
     else ($cur | length) == 0 and env.GATE_FAILURE_HASH == ($s.baseline_failure_hash // "")
     end) as $pass
  | ("Ran: " + $cmd + "\nRan in: " + env.GATE_DIR) as $ran
  | if $pass then
      (if $red then
         [ (if $rc == 0 then empty
            elif ($cur | length) > 0 then "Pre-existing failures: " + ($cur | join(", "))
            else "Pre-existing failures: unidentified, output matches the baseline (exit \($rc))"
            end),
           (($base - $cur) | if length > 0 then "Fixed pre-existing failures: " + join(", ")
            elif $rc == 0 and ($base | length) == 0 then "Fixed pre-existing failures: unidentified, the suite now passes"
            else empty end)
         ] | join("\n")
       else "" end) as $report
      | {
          "tests_ok": true,
          "tests_output": ((if $report == "" then "" else $report + "\n\n" end) + $ran + (if $rc == 0 then "" else "\nExit code: \($rc)" end) + "\n\n" + .),
          "baseline_report": $report,
          "_next": "self_review"
        }
    else
      (if $red | not then ""
       else "\nBaseline exit code: \($base_rc) (the suite already failed before this run)"
         + (if ($cur | length) > 0 then
              (($cur - $base) | if length > 0 then "\nNew failures: " + join(", ") else "" end)
              + (($cur - ($cur - $base)) | if length > 0 then "\nPre-existing failures: " + join(", ") else "" end)
            else "\nNo failing test could be identified in this output, so none can be matched to the baseline."
            end)
       end) as $block
      | {
          "tests_ok": false,
          "tests_output": ($ran + "\nExit code: " + ($rc | tostring) + $block + "\n\n" + .),
          "_next": "fix_loop_gate"
        }
    end'
