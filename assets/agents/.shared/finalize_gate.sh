#!/usr/bin/env bash
# finalize_gate.sh DIR [WRITER]
#
# The checks a run owes before it reports done, in DIR: the detected fmt
# command, check (BUILD_CMD, else the detected check or build), lint, then the
# full test suite, each through the coder gates' run_gate (so under the
# build_lock prefix) and killed after TEST_TIMEOUT seconds (default 1800, 0
# for none). A red suite is compared with the base suite record for HEAD or
# the nearest commit on its first-parent chain (at most 50 back), so failures
# the base already had are accepted. Writes the finalize verification record
# for the worktree tree and prints {status: green|red|unavailable, tree,
# items}; `unavailable` means nothing new failed but the suite could not be
# shown green: there is no test command, or a red suite had no base to be
# compared with.
set -uo pipefail

shared=$(cd "$(dirname "$0")" && pwd)
# shellcheck disable=SC1091
source "$shared/utils.sh"
# shellcheck disable=SC1091
source "$shared/../coder/scripts/gate_lib.sh"
set +e

if [[ $# -lt 1 || ! -d "$1" ]]; then
  echo "usage: finalize_gate.sh DIR [WRITER]" >&2
  exit 2
fi
dir=$(cd "$1" && pwd -P)
writer=${2:-finalize_gate}
deadline=${TEST_TIMEOUT:-1800}
if [[ ! "$deadline" =~ ^[0-9]+$ ]]; then
  echo "finalize_gate.sh: TEST_TIMEOUT must be a whole number of seconds, got \"$deadline\"" >&2
  exit 2
fi
deadline=$((10#$deadline))
top=$(gate_tree_root "$dir") || {
  echo "finalize_gate.sh: $dir is not inside a git work tree" >&2
  exit 2
}

project_info=$(detect_project "$dir")
fmt_cmd=$(jq -r '.fmt // ""' <<<"$project_info")
if [[ -n "${BUILD_CMD:-}" ]]; then
  check_cmd=$BUILD_CMD
else
  check_cmd=$(jq -r '.check // .build // ""' <<<"$project_info")
fi
lint_cmd=$(jq -r '.lint // ""' <<<"$project_info")
test_cmd=$(gate_test_command_for_run "$(gate_test_command "$dir")")

out_file=$(mktemp)
results_file=$(mktemp)
trap 'rm -f "$out_file" "$results_file"' EXIT

# add_result appends one result: role $1, command $2, exit $3, duration $4,
# verdict $5, note $6, with the tail of the last command's output.
add_result() {
  local role=$1 cmd=$2 exit_code=$3 duration=$4 verdict=$5 tail_text=''
  if [[ "$verdict" != not_run && -s "$out_file" ]]; then
    tail_text=$(tail -c 2000 "$out_file")
  fi
  RESULT_NOTE=$6 RESULT_TAIL=$tail_text \
    jq -nc --arg role "$role" --arg cmd "$cmd" --argjson exit "$exit_code" --argjson duration "$duration" \
    --arg verdict "$verdict" \
    '{role: $role, cmd: $cmd, exit: $exit, duration_s: $duration, verdict: $verdict, note: env.RESULT_NOTE, tail: env.RESULT_TAIL}' \
    >>"$results_file"
}

# run_step runs command $1 and sets STEP_RC, STEP_SECS and STEP_TIMED_OUT.
run_step() {
  local t0=$SECONDS
  : >"$out_file"
  STEP_RC=0
  run_gate_until "$dir" "$1" "$deadline" "$out_file" || STEP_RC=$?
  STEP_SECS=$((SECONDS - t0))
  STEP_TIMED_OUT=$GATE_TIMED_OUT
}

# dirty_paths prints the repository-relative paths git status lists, one per
# line.
dirty_paths() {
  git -C "$top" status --porcelain --no-renames --untracked-files=all -z 2>/dev/null |
    tr '\0' '\n' | cut -c4- | LC_ALL=C sort -u
}

if [[ -z "$fmt_cmd" ]]; then
  add_result fmt "" 0 0 not_run "no fmt command detected"
else
  before=$(dirty_paths)
  run_step "$fmt_cmd"
  # fmt may rewrite files the run never touched; those go back to how they
  # were, and the run's own edits keep whatever fmt made of them.
  restored=()
  while IFS= read -r path; do
    [[ -n "$path" ]] || continue
    if git -C "$top" ls-files --error-unmatch -- "$path" >/dev/null 2>&1 &&
      git -C "$top" checkout -q -- "$path" 2>/dev/null; then
      restored+=("$path")
    fi
  done < <(LC_ALL=C comm -13 <(printf '%s\n' "$before") <(dirty_paths))
  note=''
  if (( ${#restored[@]} > 0 )); then
    note="restored files fmt changed that were clean before it ran: ${restored[*]}"
  fi
  if (( STEP_TIMED_OUT == 1 )); then
    add_result fmt "$fmt_cmd" "$STEP_RC" "$STEP_SECS" timed_out "killed after ${deadline} s${note:+; $note}"
  elif (( STEP_RC == 0 )); then
    add_result fmt "$fmt_cmd" 0 "$STEP_SECS" green "$note"
  else
    add_result fmt "$fmt_cmd" "$STEP_RC" "$STEP_SECS" red "$note"
  fi
fi

tree=$(python3 "$shared/verify_record.py" tree --root "$dir" 2>/dev/null) || tree=''

for role in check lint; do
  if [[ "$role" == check ]]; then
    c=$check_cmd
  else
    c=$lint_cmd
  fi
  if [[ -z "$c" || "$c" == null ]]; then
    add_result "$role" "" 0 0 not_run "no $role command detected"
    continue
  fi
  run_step "$c"
  if (( STEP_TIMED_OUT == 1 )); then
    add_result "$role" "$c" "$STEP_RC" "$STEP_SECS" timed_out "killed after ${deadline} s"
  elif (( STEP_RC == 0 )); then
    add_result "$role" "$c" 0 "$STEP_SECS" green ""
  elif [[ "$role" == lint && "$STEP_RC" == 127 ]]; then
    add_result lint "$c" 127 "$STEP_SECS" not_run "the lint command was not found (exit 127)"
  else
    add_result "$role" "$c" "$STEP_RC" "$STEP_SECS" red ""
  fi
done

# base_record sets BASE_RECORD to the base record a red suite is judged
# against, and BASE_MODE; it fails with BASE_WHY set when there is none.
base_record() {
  local found base key code=0 json until
  BASE_MODE=none
  BASE_RECORD=''
  found=$(python3 "$shared/base_suite.py" status --root "$dir" --cmd "$test_cmd" --chain 50 2>/dev/null) || found='{}'
  if [[ $(jq -r '.found // false' <<<"$found") != true ]]; then
    BASE_WHY="no base suite record or job for HEAD or the 50 commits before it"
    return 1
  fi
  base=$(jq -r '.base' <<<"$found")
  key=$(jq -r '.key' <<<"$found")
  if [[ $(jq -r '.record // ""' <<<"$found") != "" ]]; then
    BASE_MODE=reused
    BASE_RECORD=$(jq -r '.record' <<<"$found")
    return 0
  fi
  until=$(jq -r '.deadline // empty' <<<"$found")
  [[ -n "$until" ]] || until=$(($(date +%s) + (deadline > 0 ? deadline : 3600)))
  json=$(python3 "$shared/base_suite.py" await --root "$dir" --key "$key" --until "$until" 2>/dev/null) || code=$?
  if (( code == 0 )); then
    BASE_MODE=background
    BASE_RECORD=$(jq -r '.record' <<<"$json")
    return 0
  fi
  if (( code != 3 )); then
    BASE_MODE="background (pending)"
    BASE_WHY="the base suite at ${base:0:7} was still running at its deadline"
    return 1
  fi
  BASE_MODE="lazy (background job $(jq -r '(.state // "unknown") + ": " + (.error // "")' <<<"$json" 2>/dev/null))"
  code=0
  json=$(python3 "$shared/base_suite.py" run --root "$dir" --base "$base" --cmd "$test_cmd" \
    --timeout "$deadline" 2>/dev/null) || code=$?
  if (( code != 0 )); then
    BASE_WHY="the full base run at ${base:0:7} failed: $(jq -r '.error // "no record"' <<<"$json" 2>/dev/null)"
    return 1
  fi
  BASE_RECORD=$(jq -r '.record' <<<"$json")
}

unavailable=''
BASE_MODE=none
if [[ -z "$test_cmd" ]]; then
  add_result test "" 0 0 not_run "no test command configured or detected"
  unavailable="no test command configured or detected"
else
  run_step "$test_cmd"
  if (( STEP_TIMED_OUT == 1 )); then
    add_result test "$test_cmd" "$STEP_RC" "$STEP_SECS" timed_out "killed after ${deadline} s (TEST_TIMEOUT)"
  elif (( STEP_RC == 0 )); then
    add_result test "$test_cmd" 0 "$STEP_SECS" green ""
  else
    rc=$STEP_RC
    secs=$STEP_SECS
    BASE_WHY=''
    if base_record; then
      verdict_json=$(gate_red_against_record "$out_file" "$rc" "$BASE_RECORD" "$test_cmd")
      note=$(jq -r "$GATE_JQ_CAP"'
        if .pass then "pre-existing failures: " + (if (.cur | length) > 0 then .cur | gate_cap(", ") else "unidentified, output matches the base" end)
        elif .base_status != "red" then "the base suite was green, so these failures are new"
        elif (.cur | length) == 0 then "no failing test could be identified (\(.reason))"
        else "new failures: " + (.cur - .base | gate_cap(", "))
        end' <<<"$verdict_json")
      verdict=red
      if [[ $(jq -r '.pass' <<<"$verdict_json") == true ]]; then
        verdict=pre-existing
      fi
    else
      verdict=red
      note="not classified: $BASE_WHY"
      unavailable=$BASE_WHY
    fi
    add_result test "$test_cmd" "$rc" "$secs" "$verdict" "$note"
  fi
fi

if [[ -n "$tree" ]]; then
  python3 "$shared/verify_record.py" write --root "$dir" --tier finalize --writer "$writer" \
    --tree "$tree" --baseline-mode "$BASE_MODE" < <(jq -sc '.' "$results_file") >/dev/null 2>&1 ||
    echo "finalize_gate.sh: the finalize record could not be written" >&2
else
  echo "finalize_gate.sh: git could not write a tree of the worktree, so no record was written" >&2
fi

FINALIZE_UNAVAILABLE=$unavailable jq -nc --arg tree "$tree" --slurpfile items "$results_file" '
  (env.FINALIZE_UNAVAILABLE) as $unavailable
  | ($items | map(select(.role != "test")) | all(.verdict == "green" or .verdict == "pre-existing" or .verdict == "not_run")) as $others_ok
  | ($items | map(select(.role == "test")) | .[0]) as $test
  | {
      status: (
        if $others_ok and ($test.verdict == "green" or $test.verdict == "pre-existing") then "green"
        elif $others_ok and $unavailable != "" then "unavailable"
        else "red"
        end),
      tree: $tree,
      items: $items
    }'
