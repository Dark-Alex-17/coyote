#!/usr/bin/env bash
set -euo pipefail

project_dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
resolved=$(cd "$project_dir" 2>/dev/null && pwd) || resolved="$project_dir"

jq -nc \
  --arg pd "$resolved" \
  '{
    "project_dir": $pd,
    "_next": "analyze_request"
  }'
