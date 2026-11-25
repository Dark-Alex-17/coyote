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
cmd=$(gate_test_command_for_run "$(gate_test_command "$project_dir")")
tier=$(gate_verify_tier)

if [[ -z "$cmd" && "$tier" == "full" ]]; then
  jq -nc '{
    "tests_ok": true,
    "tests_output": "(GATE NOT RUN: no test command configured or detected. This is NOT evidence that tests passed; set the test_command variable or TEST_CMD, and never report the suite as green.)",
    "tests_verdict": "NOT RUN: no test command configured or detected",
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

# Round tier: the commands select_checks (or select_fault) chose run in
# order, targeted then impacted, under one shared deadline. A red command is
# judged against the base suite (reused, awaited, or re-run at the base on
# demand) and the run stops at the first red that is not pre-existing. The
# full suite and lint are left to the finalize gate. A selected command
# holding a carriage return or line feed is dropped: one could forge a line
# of the report.
if [[ "$tier" == "round" ]]; then
  shared="$(dirname "$0")/../../.shared"
  started=$SECONDS
  floor=${LLM_AGENT_VAR_BASELINE_FLOOR:-}
  pinned_base=$(printf '%s' "$state" | jq -r '.baseline_base // "" | strings')
  budget=$deadline
  (( budget > 0 )) || budget=3300
  selection=$(printf '%s' "$state" | jq -c '
    def picked: if type == "array" then map(strings | select(test("\\S")) | select(test("[\\r\\n]") | not)) else [] end;
    {
      targeted: (.targeted | picked),
      impacted: (.impacted | picked),
      reason: ((.reason // "") | tostring | gsub("\\s+"; " ") | ltrimstr(" ") | rtrimstr(" ")
        | if . == "" then "no reason given" else . end)
    }')
  ROUND_REASON=$(jq -r '.reason' <<<"$selection")
  export ROUND_REASON
  count=$(jq '(.targeted + .impacted) | length' <<<"$selection")
  ntargeted=$(jq '.targeted | length' <<<"$selection")
  if (( count == 0 )); then
    printf '%s' "$state" | jq -c '{
      "tests_ok": true,
      "tests_output": ("(GATE NOT RUN: no targeted or impacted tests were selected (" + env.ROUND_REASON + "). This is NOT evidence that tests passed, and never report the tests as green.)"),
      "tests_verdict": ("NOT RUN: no targeted or impacted tests selected: " + env.ROUND_REASON),
      "tests_tier": "round: no targeted or impacted tests selected; full suite and lint deferred to the finalize gate",
      "baseline_report": ("\nBaseline mode: " + (.baseline_mode | if type == "string" and . != "" then . else "none" end)),
      "_next": "self_review"
    }'
    exit 0
  fi

  base_mode=$(printf '%s' "$state" | jq -r '.baseline_mode | if type == "string" and . != "" then . else "none" end')
  mode_now=$base_mode
  deferred=0
  if [[ "$base_mode" == *"(deferred"* ]]; then
    deferred=1
  fi
  job_started=0
  if ! command -v python3 >/dev/null 2>&1; then
    no_base_why="python3 is not available"
  elif [[ -z "$cmd" ]]; then
    no_base_why="no full test command was configured or detected"
  else
    no_base_why=''
  fi

  # base_start starts the base suite job or attaches to it, printing the
  # start JSON; it fails when there is nothing to start. Callers set
  # job_started themselves: this runs in a command substitution. The base
  # baseline_tests fixed wins over one derived again from HEAD now.
  base_start() {
    [[ -z "$no_base_why" ]] || return 1
    if [[ -n "$pinned_base" ]]; then
      python3 "$shared/base_suite.py" start --root "$project_dir" --cmd "$cmd" \
        --base "$pinned_base" --timeout "$deadline" 2>/dev/null
    else
      python3 "$shared/base_suite.py" start --root "$project_dir" --cmd "$cmd" \
        --floor "$floor" --timeout "$deadline" 2>/dev/null
    fi
  }

  # round_left prints the whole seconds left of the shared budget less a
  # margin of $1 seconds, at least 1.
  round_left() {
    local left=$(( budget - (SECONDS - started) - $1 ))
    (( left >= 1 )) || left=1
    printf '%s' "$left"
  }

  # round_classify judges red command $1 (exit $2, output file $3) against
  # the base suite. It sets CLASS_VERDICT (pre-existing or red), CLASS_NOTE
  # and mode_now. A baseline that is still running, or that could not be
  # taken, leaves the failures unclassified, which is red.
  round_classify() {
    local c=$1 rc=$2 out=$3 start_json mode key base json code=0 record='' lazy_why verdict_json
    CLASS_VERDICT=red
    CLASS_NOTE=''
    job_started=1
    if ! start_json=$(base_start); then
      mode_now=none
      CLASS_NOTE="No baseline to compare with: ${no_base_why:-the base suite could not be started}."
      return 0
    fi
    mode=$(jq -r '.mode // "none"' <<<"$start_json")
    key=$(jq -r '.key // ""' <<<"$start_json")
    base=$(jq -r '.base // ""' <<<"$start_json")
    lazy_why="background job $(jq -r '(.state // "unknown") + ": " + (.error // "")' <<<"$start_json")"
    case "$mode" in
      reused)
        record=$(jq -r '.record' <<<"$start_json")
        mode_now=reused
        ;;
      background)
        json=$(python3 "$shared/base_suite.py" await --root "$project_dir" --key "$key" \
          --until "$(( $(date +%s) + $(round_left 30) ))" 2>/dev/null) || code=$?
        if (( code == 0 )); then
          record=$(jq -r '.record' <<<"$json")
          mode_now=background
        elif (( code == 3 )); then
          lazy_why="background job $(jq -r '(.state // "unknown") + ": " + (.error // "")' <<<"$json")"
        else
          mode_now="background (pending)"
          CLASS_NOTE="Baseline still running; failures not classified: the base suite at ${base:0:7} did not finish within this run's deadline."
          return 0
        fi
        ;;
      lazy) ;;
      *)
        mode_now=none
        CLASS_NOTE="No baseline to compare with: $(jq -r '.error // "the base suite could not be started"' <<<"$start_json")."
        return 0
        ;;
    esac
    if [[ -z "$record" ]]; then
      mode_now="lazy ($lazy_why)"
      code=0
      json=$(python3 "$shared/base_suite.py" run --root "$project_dir" --base "$base" --cmd "$c" \
        --timeout "$(round_left 0)" 2>/dev/null) || code=$?
      if (( code != 0 )); then
        CLASS_NOTE="Failures not classified: the on-demand base run failed ($(jq -r '.error // "no record"' <<<"$json" 2>/dev/null))."
        return 0
      fi
      record=$(jq -r '.record' <<<"$json")
    fi
    verdict_json=$(gate_red_against_record "$out" "$rc" "$record" "$c")
    if [[ $(jq -r '.pass' <<<"$verdict_json") == "true" ]]; then
      CLASS_VERDICT=pre-existing
    fi
    CLASS_NOTE=$(jq -r --argjson rc "$rc" --arg base "${base:0:7}" "$GATE_JQ_CAP"'
      if .pass then
        if (.cur | length) > 0 then "Pre-existing failures: " + (.cur | gate_cap(", "))
        else "Pre-existing failures: unidentified (\(.reason)), output matches the baseline (exit \($rc))"
        end
      elif .base_status != "red" then "The base suite at \($base) was green, so these failures are new."
      elif (.cur | length) == 0 then "No failing test could be identified in this output (\(.reason)), so none can be matched to the baseline."
      elif (.cur - .base | length) > 0 then "New failures: " + (.cur - .base | gate_cap(", "))
      else "This run does not match the baseline at \($base) (its exit code or failure output differs)."
      end' <<<"$verdict_json")
  }

  tree=''
  if command -v python3 >/dev/null 2>&1; then
    tree=$(python3 "$shared/verify_record.py" tree --root "$project_dir" 2>/dev/null) || tree=''
  fi
  out_file=$(mktemp)
  sections_file=$(mktemp)
  results_file=$(mktemp)
  trap 'rm -f "$out_file" "$sections_file" "$results_file"' EXIT
  outcome=ok
  fail_rc=0
  accepted_rc=''
  timed_cmd=''
  targeted_labels=()
  impacted_labels=()
  report_lines=()
  accepted=()
  export GATE_DIR="$project_dir" GATE_DEADLINE="$deadline"

  for ((i = 0; i < count; i++)); do
    raw=$(jq -r --argjson i "$i" '(.targeted + .impacted)[$i]' <<<"$selection")
    c=$(gate_test_command_for_run "$raw")
    role=impacted
    if (( i < ntargeted )); then
      role=targeted
    fi
    label="\`$raw\`"
    rc=0
    duration=0
    verdict=''
    note=''
    ran=0
    if [[ "$outcome" != ok ]]; then
      verdict=not_run
      note="an earlier command failed"
      label+=" (not run)"
    else
      left=0
      if (( deadline > 0 )); then
        left=$(( deadline - (SECONDS - started) ))
      fi
      if (( deadline > 0 && left < 1 )); then
        outcome=timed_out
        timed_cmd=$raw
        verdict=timed_out
        note="the ${deadline} s deadline (test_timeout) was spent"
        label+=" (not run)"
      else
        ran=1
        t0=$SECONDS
        run_gate_until "$project_dir" "$c" "$left" "$out_file" || rc=$?
        duration=$(( SECONDS - t0 ))
        if (( GATE_TIMED_OUT == 1 )); then
          outcome=timed_out
          timed_cmd=$raw
          verdict=timed_out
          note="TIMED OUT: killed when the ${deadline} s deadline (test_timeout) ran out"
        elif (( rc == 0 )); then
          verdict=green
        else
          round_classify "$c" "$rc" "$out_file"
          verdict=$CLASS_VERDICT
          note=$CLASS_NOTE
          if [[ "$verdict" == pre-existing ]]; then
            accepted+=("${note#Pre-existing failures: }")
            report_lines+=("$note")
            [[ -n "$accepted_rc" ]] || accepted_rc=$rc
          else
            outcome=failed
            fail_rc=$rc
          fi
        fi
        {
          printf 'Ran: %s (%s)\nRan in: %s\n' "$(gate_ran "$c")" "$role" "$project_dir"
          if [[ "$verdict" != green ]]; then
            printf 'Exit code: %s\n' "$rc"
          fi
          if [[ -n "$note" ]]; then
            printf '%s\n' "$note"
          fi
          printf '\n%s\n\n' "$(trim_output "$(cat "$out_file")")"
        } >>"$sections_file"
      fi
    fi
    if (( ran == 0 )); then
      printf 'Not run: %s (%s; %s)\n\n' "$raw" "$role" "$note" >>"$sections_file"
    fi
    if [[ "$role" == targeted ]]; then
      targeted_labels+=("$label")
    else
      impacted_labels+=("$label")
    fi
    tail_text=''
    if (( ran == 1 )) && [[ -s "$out_file" ]]; then
      tail_text=$(tail -c 2000 "$out_file")
    fi
    RESULT_NOTE=$note RESULT_TAIL=$tail_text \
      jq -nc --arg role "$role" --arg cmd "$c" --argjson exit "$rc" --argjson duration "$duration" \
      --arg verdict "$verdict" \
      '{role: $role, cmd: $cmd, exit: $exit, duration_s: $duration, verdict: $verdict, note: env.RESULT_NOTE, tail: env.RESULT_TAIL}' \
      >>"$results_file"
    : >"$out_file"
  done

  if [[ "$mode_now" == "background (pending)" && -z "$no_base_why" ]]; then
    key=$(printf '%s' "$state" | jq -r '.baseline_key // "" | strings')
    if [[ -n "$key" ]] && python3 "$shared/base_suite.py" status --root "$project_dir" --key "$key" 2>/dev/null |
      jq -e '.record | strings' >/dev/null 2>&1; then
      mode_now=background
    fi
  fi
  if (( deferred == 1 && job_started == 0 )) && [[ "$outcome" != timed_out ]]; then
    job_started=1
    base_start >/dev/null || true
  fi
  if (( deferred == 1 && job_started == 1 )); then
    if [[ "$mode_now" == "$base_mode" ]]; then
      mode_now="background (deferred: build_lock)"
    fi
    report_lines+=("Base suite runs under build_lock \`$(gate_prefix)\`; it started after this lap's targeted/impacted runs, and later gate runs wait while it holds the lock.")
  fi

  record_line=''
  if [[ -n "$tree" ]]; then
    if record_path=$(jq -s '.' "$results_file" | python3 "$shared/verify_record.py" write --root "$project_dir" \
      --tier round --writer coder --tree "$tree" --baseline-mode "$mode_now" 2>/dev/null); then
      record_line="Round record: $record_path"
    else
      record_line="No round record was written: verify_record.py failed."
    fi
  else
    record_line="No round record was written: git could not write a tree of the worktree, or python3 is not available."
  fi
  report_lines+=("Baseline mode: $mode_now")

  join_labels() {
    local out='' item
    for item in "$@"; do
      out+="${out:+, }$item"
    done
    printf '%s' "${out:-none}"
  }
  ROUND_TIER="round: targeted $(join_labels ${targeted_labels[@]+"${targeted_labels[@]}"}); impacted $(join_labels ${impacted_labels[@]+"${impacted_labels[@]}"}); full suite and lint deferred to the finalize gate; selection: $ROUND_REASON"
  ROUND_REPORT=$(printf '\n%s' "${report_lines[@]}")
  ROUND_ACCEPTED=$(printf '%s\n' ${accepted[@]+"${accepted[@]}"})
  export ROUND_TIER ROUND_REPORT ROUND_ACCEPTED ROUND_RECORD="$record_line" ROUND_TIMED_CMD="$timed_cmd" ROUND_MODE="$mode_now"
  printf '%s\n' "$record_line" >>"$sections_file"
  jq -nc \
    --rawfile sections "$sections_file" \
    --arg outcome "$outcome" \
    --argjson fail_rc "$fail_rc" \
    --arg accepted_rc "$accepted_rc" \
    '(env.ROUND_ACCEPTED | split("\n") | map(select(. != ""))) as $accepted
    | {
        "tests_ok": ($outcome == "ok"),
        "tests_output": $sections,
        "tests_verdict": (
          if $outcome == "timed_out" then "TIMED OUT after " + env.GATE_DEADLINE + " s"
          elif $outcome == "failed" then "failed (exit \($fail_rc))"
          elif ($accepted | length) > 0 then
            "red, matches the pre-run baseline (exit \($accepted_rc)); accepted pre-existing failures: " + ($accepted | join(", "))
          else "passed"
          end),
        "tests_tier": env.ROUND_TIER,
        "baseline_report": env.ROUND_REPORT,
        "baseline_mode": env.ROUND_MODE,
        "_next": (if $outcome == "timed_out" then "end_failure" elif $outcome == "failed" then "fix_loop_gate" else "self_review" end)
      }
    + (if $outcome == "timed_out" then
        {"gate_error": ("verify_tests TIMED OUT after " + env.GATE_DEADLINE + " s (test_timeout) before `" + env.ROUND_TIMED_CMD + "` finished. This is not a red run: raise test_timeout or select fewer tests.")}
      else {} end)'
  exit 0
fi

out_file=$(mktemp)
ids_file=$(mktemp)
passing_file=$(mktemp)
state_copy=$(mktemp)
trap 'rm -f "$out_file" "$ids_file" "$passing_file" "$state_copy"' EXIT
exit_code=0
run_gate_until "$project_dir" "$cmd" "$deadline" "$out_file" || exit_code=$?
output=$(cat "$out_file")
cmd=$(gate_ran "$cmd")
export GATE_DIR="$project_dir" GATE_DEADLINE="$deadline" GATE_FAILURE_HASH='' GATE_REASON='' GATE_SIGNAL=0

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
      "tests_verdict": ("TIMED OUT after " + env.GATE_DEADLINE + " s"),
      "gate_error": ("verify_tests TIMED OUT after " + env.GATE_DEADLINE + " s (test_timeout) before the suite finished. This is not a red run: raise test_timeout or pass a narrower test_command."),
      "_next": "end_failure"
    }'
  exit 0
fi

printf '%s' "$state" >"$state_copy"
if (( exit_code != 0 )) && gate_killed_by_signal "$out_file"; then
  GATE_SIGNAL=1
fi
if (( exit_code != 0 )) && [[ $(printf '%s' "$state" | jq -r '.baseline_status // ""') == "red" ]]; then
  gate_failing_tests "$out_file" >"$ids_file"
  gate_passing_tests "$out_file" >"$passing_file"
  GATE_FAILURE_HASH=$(gate_failure_hash "$out_file")
  GATE_REASON=$(gate_unidentified_reason "$out_file")
fi

# The run and the baseline used the same command, so gate_test_verdict holds
# it to the baseline's exit code and failure hash. A run whose failures
# cannot be identified never passes against a baseline whose failures could.
# A baseline id missing from a red run is reported fixed only when the run
# shows it passing.
trim_output "$output" | jq -Rsc \
  --arg cmd "$cmd" \
  --argjson rc "$exit_code" \
  --slurpfile st "$state_copy" \
  --rawfile ids "$ids_file" \
  --rawfile passing "$passing_file" \
  "$GATE_JQ_CAP$GATE_JQ_VERDICT"'
  $st[0] as $s
  | (($s.baseline_status // "none") == "red") as $red
  | ($s.baseline_failures // [] | unique) as $base
  | ($ids | split("\n") | map(select(. != "")) | unique) as $cur
  | ($passing | split("\n") | map(select(. != "")) | unique) as $shown_passing
  | ($s.baseline_exit_code // 0) as $base_rc
  | ($s.baseline_failure_hash // "") as $base_hash
  | (env.GATE_SIGNAL == "1") as $signal
  | ($s.baseline_reused_from // "") as $reused
  | (if $reused == "" then ""
     else "Baseline reused from " + $reused + ": recorded before this run'"'"'s later commits; the suite was not run again on HEAD."
     end) as $reuse_line
  | ({rc: $rc, signal: $signal, red: $red, same_command: true, cur: $cur, base: $base,
      base_rc: $base_rc, hash: env.GATE_FAILURE_HASH, base_hash: $base_hash}
     | gate_test_verdict) as $pass
  | ("Ran: " + $cmd + "\nRan in: " + env.GATE_DIR) as $ran
  | if $pass then
      ($base - $cur) as $gone
      | (if $rc == 0 then $gone else $gone - ($gone - $shown_passing) end) as $fixed
      | (if $red then
         [ (if $rc == 0 then empty
            elif ($cur | length) > 0 then "Pre-existing failures: " + ($cur | gate_cap(", "))
            else "Pre-existing failures: unidentified (\(env.GATE_REASON)), output matches the baseline (exit \($rc))"
            end),
           ($fixed | if length > 0 then "Fixed pre-existing failures: " + gate_cap(", ")
            elif $rc == 0 and ($base | length) == 0 then "Fixed pre-existing failures: unidentified, the suite now passes"
            else empty end),
           (($gone - $fixed) | if length > 0 then "No longer reported failing: " + gate_cap(", ") else empty end)
         ] | join("\n")
       else "" end) as $report
      | {
          "tests_ok": true,
          "tests_output": ((if $report == "" then "" else $report + "\n\n" end) + $ran + (if $rc == 0 then "" else "\nExit code: \($rc)" end) + "\n\n" + .),
          "tests_verdict": (
            if $rc == 0 then "passed"
            else "red, matches the pre-run baseline (exit \($rc)); accepted pre-existing failures: "
              + (if ($cur | length) > 0 then $cur | gate_cap(", ") else "unidentified (\(env.GATE_REASON))" end)
            end),
          "baseline_report": ((if $report == "" then "" else "\n" + $report end)
            + (if $reuse_line == "" then "" else "\n" + $reuse_line end)),
          "_next": "self_review"
        }
    else
      (if $signal and $red then "\nA test target was killed by a signal, so its remaining tests never ran; this run cannot pass against the baseline." else "" end) as $crash
      | (if $red | not then ""
       else "\nBaseline exit code: \($base_rc) (the suite already failed before this run)"
         + (if ($cur | length) > 0 and ($base | length) == 0 then
              "\nFailing tests (the baseline'"'"'s failures could not be identified, so none can be compared): " + ($cur | gate_cap(", "))
            elif ($cur | length) > 0 then
              (($cur - $base) | if length > 0 then "\nNew failures: " + gate_cap(", ") else "" end)
              + (($cur - ($cur - $base)) | if length > 0 then "\nPre-existing failures: " + gate_cap(", ") else "" end)
            else "\nNo failing test could be identified in this output (\(env.GATE_REASON)), so none can be matched to the baseline."
            end)
       end) as $block
      | {
          "tests_ok": false,
          "tests_output": ($ran + "\nExit code: " + ($rc | tostring) + $crash + $block + "\n\n" + .),
          "tests_verdict": ("failed (exit " + ($rc | tostring) + ")"),
          "_next": "fix_loop_gate"
        }
        + (if $reuse_line == "" then {} else {"baseline_report": ("\n" + $reuse_line)} end)
    end'
