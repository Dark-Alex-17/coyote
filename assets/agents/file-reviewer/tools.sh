#!/usr/bin/env bash
set -eo pipefail

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"
source "$LLM_ROOT_DIR/agents/.shared/utils.sh"

# @env LLM_OUTPUT=/dev/stdout
# @env LLM_AGENT_VAR_PROJECT_DIR=.
# @describe File reviewer tools for single-file code review

_project_dir() {
  local dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
  (cd "${dir}" 2>/dev/null && pwd) || echo "${dir}"
}

# @cmd Get project structure to understand codebase layout
get_structure() {
  local project_dir
  project_dir=$(_project_dir)

  info "Project structure:" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"

  local project_info
  project_info=$(detect_project "${project_dir}")

  {
    echo "Type: $(echo "${project_info}" | jq -r '.type')"
    echo ""
    get_tree "${project_dir}" 2
  } >> "$LLM_OUTPUT"
}