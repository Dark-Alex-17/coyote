#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

# analyze_request died (iteration cap, timeout, API failure) and the engine
# took its fallback edge, leaving its error in last_node_output. The run goes
# on with an empty plan so implement explores the request itself, and the
# fault stays visible in plan_summary, which every end node renders.
# Interactive runs go to the human checkpoint; headless runs skip it the way
# route_complexity does. The quoted error goes on one line and is capped, so a
# multi-line provider error cannot start a line that reads as a report field.
empty_plan='
  ((.last_node_output // "") | tostring | gsub("\\s*\\n\\s*"; " ") | .[0:400]
    | if . == "" then "no error recorded in state" else . end) as $err
  | {
      "plan_summary": ("Planning ended without a plan (" + $err + "); the implementer plans from the request itself."),
      "files_to_modify": [],
      "files_to_create": [],
      "risks": (((.risks // []) | if type == "array" then . else [] end)
        + ["analyze_request ended without concluding: " + $err]),
      "complexity_score": 7
    }'

if [[ "${CODER_AUTOAPPROVE:-0}" == "1" ]]; then
  printf '%s' "$state" | jq -c "$empty_plan"' + {"_next": "baseline_tests"}'
else
  printf '%s' "$state" | jq -c "$empty_plan"' + {"_next": "gate_approval"}'
fi
