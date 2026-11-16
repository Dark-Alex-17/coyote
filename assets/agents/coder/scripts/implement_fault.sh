#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

fix_attempts=$(echo "$state" | jq -r '.fix_attempts // 0')
max_fix_attempts=$(echo "$state" | jq -r '.max_fix_attempts // 5')

# The quoted error goes on one line and is capped, so a multi-line provider
# error cannot start a line that reads as a report field. last_node_output
# itself is left as the engine wrote it.
quoted_error='((.last_node_output // "") | tostring | gsub("\\s*\\n\\s*"; " ") | .[0:400]
  | if . == "" then "no error recorded in state" else . end) as $err'

# implement died (iteration cap, timeout, API failure) and the engine took its
# fallback edge, leaving its error in last_node_output. The retry spends the
# same budget as fix_loop_gate. It never routes to verify_build: a pass that
# wrote nothing would otherwise pass the gates and end as CODER_COMPLETE.
if (( fix_attempts >= max_fix_attempts )); then
  printf '%s' "$state" | jq -c \
    --argjson n "$fix_attempts" \
    --argjson max "$max_fix_attempts" \
    "$quoted_error"'
    | {
        "gate_error": ("implement ended without concluding with the fix budget spent ("
          + ($n | tostring) + " of " + ($max | tostring) + "): " + $err),
        "_next": "end_failure"
      }'
  exit 0
fi

next_attempts=$((fix_attempts + 1))

# The dead pass may have been inside the fix or review loop, so the build
# output or review notes it was working from are carried after the fault.
printf '%s' "$state" | jq -c \
  --argjson n "$next_attempts" \
  --argjson max "$max_fix_attempts" \
  "$quoted_error"'
  | ((.project_dir // "") | tostring | if . == "" then "the project directory" else . end) as $dir
  | ((.fix_instructions // "") | tostring) as $prior
  | {
      "fix_attempts": $n,
      "fix_instructions": ("## Fix loop status (attempt " + ($n | tostring) + " of " + ($max | tostring) + ")\n\n"
        + "The previous implement pass ended without concluding. Error: " + $err + "\n\n"
        + "Its edits are still on disk. Review `git status` and `git diff` in " + $dir
        + " and continue from there."
        + (if $prior == "" then "" else "\n\nThe pass was working on:\n\n" + $prior end)),
      "last_script_error": "",
      "gate_retries": 0,
      "_next": "implement"
    }'
