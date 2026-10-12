#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

coder_result=$(echo "$state" | jq -r '.coder_result // ""')

case "$coder_result" in
  *CODER_COMPLETE*)
    # Every implement → verify cycle passes through here, so this is where the
    # gate-crash bookkeeping from the previous cycle is reset.
    jq -nc '{"gate_retries": 0, "gate_error": "", "last_script_error": "", "_next": "verify_format_lint"}'
    ;;
  *CODER_REJECTED*)
    jq -nc '{"_next": "end_rejected"}'
    ;;
  *CODER_FAILED*)
    jq -nc '{"blocking_reason": "coder fix-loop exhausted; see coder result", "gate_error": "", "last_script_error": "", "_next": "end_failure"}'
    ;;
  *)
    jq -nc '{"blocking_reason": "coder returned no recognizable sentinel (expected CODER_COMPLETE / CODER_REJECTED / CODER_FAILED)", "gate_error": "", "last_script_error": "", "_next": "end_failure"}'
    ;;
esac
