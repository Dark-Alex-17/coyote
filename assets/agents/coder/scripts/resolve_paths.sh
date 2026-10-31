#!/usr/bin/env bash
set -euo pipefail

project_dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
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
