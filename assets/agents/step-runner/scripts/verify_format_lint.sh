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
project_type=$(detect_project "$project_dir" | jq -r '.type // "unknown"')

format_cmd="${FORMAT_CMD:-}"
if [[ -z "$format_cmd" ]]; then
  case "$project_type" in
    rust) format_cmd="cargo fmt" ;;
    go) format_cmd="gofmt -w ." ;;
    python) command -v ruff &>/dev/null && format_cmd="ruff format ." ;;
  esac
fi

if [[ -z "$format_cmd" ]]; then
  format_output="(no format command configured for project type '$project_type'; skipped. Set FORMAT_CMD to enable.)"
else
  fmt_rc=0
  fmt_out=$(cd "$project_dir" && eval "$format_cmd" 2>&1) || fmt_rc=$?
  format_output="Ran: $format_cmd
Exit code: $fmt_rc

$fmt_out"
fi

lint_cmd="${LINT_CMD:-}"
if [[ -z "$lint_cmd" ]]; then
  jq -nc \
    --arg fo "$format_output" \
    '{
      "format_output": $fo,
      "lint_ok": true,
      "lint_output": "(no LINT_CMD configured; linting is covered by the build/check command)",
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
