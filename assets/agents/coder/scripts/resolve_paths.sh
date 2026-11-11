#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

# A parent graph that passes `inputs: project_dir` seeds it into this graph's
# state; the agent variable it inherited may still name the parent's tree, so
# the seeded value wins. The graph's own initial_state seeds an empty string,
# so a caller that passes no input falls through to the variable.
seeded=$(printf '%s' "$state" | jq -r '.project_dir // "" | strings')
if [[ -n "$seeded" ]]; then
  project_dir="$seeded"
else
  project_dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
fi
resolved=$(cd "$project_dir" 2>/dev/null && pwd) || resolved="$project_dir"

# An unset project_dir resolves against the engine's working directory, which
# need not be the tree being edited; the verify gates then look at the planned
# files to find it.
if [[ "$project_dir" == "." ]]; then
  export PROJECT_DIR_DEFAULTED=true
else
  export PROJECT_DIR_DEFAULTED=false
fi

jq -nc \
  --arg pd "$resolved" \
  '{
    "project_dir": $pd,
    "project_dir_defaulted": (env.PROJECT_DIR_DEFAULTED == "true"),
    "_next": "analyze_request"
  }'
