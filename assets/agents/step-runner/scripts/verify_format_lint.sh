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
project_info=$(detect_project "$project_dir")
project_type=$(echo "$project_info" | jq -r '.type // "unknown"')

format_cmd="${FORMAT_CMD:-}"
if [[ -z "$format_cmd" ]]; then
  format_cmd=$(echo "$project_info" | jq -r '.fmt // ""')
fi
if [[ "$format_cmd" == "null" ]]; then format_cmd=""; fi

if [[ -z "$format_cmd" ]]; then
  format_output="(GATE NOT RUN: no format command configured or detected for project type '$project_type'. This is NOT evidence that formatting is clean. Set FORMAT_CMD to enable.)"
else
  fmt_rc=0
  fmt_out=$(cd "$project_dir" && eval "$format_cmd" 2>&1) || fmt_rc=$?
  fmt_out=$(trim_output "$fmt_out")
  format_output="Ran: $format_cmd
Exit code: $fmt_rc

$fmt_out"
fi

lint_cmd="${LINT_CMD:-}"
if [[ -z "$lint_cmd" ]]; then
  lint_cmd=$(echo "$project_info" | jq -r '.lint // ""')
fi
# The skip message must read as a WARNING, never a reassurance: the previous
# wording ("linting is covered by the build/check command") was quoted
# verbatim by workers as false evidence that linting passed
#
# Neither transcript rides argv: a single argv string is capped at 128 KiB on
# Linux (MAX_ARG_STRLEN) and COYOTE_GATE_OUTPUT_MAX_BYTES may be raised past
# it. format_output goes in via --rawfile on a process-substitution fd.
if [[ -z "$lint_cmd" || "$lint_cmd" == "null" ]]; then
  jq -nc \
    --rawfile fo <(printf '%s' "$format_output") \
    '{
      "format_output": $fo,
      "lint_ok": true,
      "lint_output": "(GATE NOT RUN: no lint command configured or detected. This is NOT evidence that linting passed — set LINT_CMD or add a Taskfile lint target, and never report linting as covered.)",
      "_next": "verify_build"
    }'
  exit 0
fi

lint_rc=0
lint_out=$(cd "$project_dir" && eval "$lint_cmd" 2>&1) || lint_rc=$?

if (( lint_rc == 0 )); then
  trim_output "$lint_out" | jq -Rsc \
    --rawfile fo <(printf '%s' "$format_output") \
    --arg cmd "$lint_cmd" \
    '{
      "format_output": $fo,
      "lint_ok": true,
      "lint_output": ("Ran: " + $cmd + "\n\n" + .),
      "_next": "verify_build"
    }'
else
  trim_output "$lint_out" | jq -Rsc \
    --rawfile fo <(printf '%s' "$format_output") \
    --arg cmd "$lint_cmd" \
    --argjson rc "$lint_rc" \
    '{
      "format_output": $fo,
      "lint_ok": false,
      "lint_output": ("Ran: " + $cmd + "\nExit code: " + ($rc | tostring) + "\n\n" + .),
      "_next": "fix_loop_gate"
    }'
fi
