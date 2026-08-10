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
if [[ -z "$lint_cmd" || "$lint_cmd" == "null" ]]; then
  jq -nc \
    --arg fo "$format_output" \
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
  jq -nc \
    --arg fo "$format_output" \
    --arg lo "Ran: $lint_cmd

$lint_out" \
    '{
      "format_output": $fo,
      "lint_ok": true,
      "lint_output": $lo,
      "_next": "verify_build"
    }'
else
  jq -nc \
    --arg fo "$format_output" \
    --arg lo "Ran: $lint_cmd
Exit code: $lint_rc

$lint_out" \
    '{
      "format_output": $fo,
      "lint_ok": false,
      "lint_output": $lo,
      "_next": "fix_loop_gate"
    }'
fi
