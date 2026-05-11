#!/usr/bin/env bash
set -eo pipefail

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"
source "$LLM_ROOT_DIR/agents/.shared/utils.sh"

# @env LLM_OUTPUT=/dev/stdout
# @env LLM_AGENT_VAR_PROJECT_DIR=.
# @describe Coder agent tools for implementing code changes

_project_dir() {
  local dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
  (cd "${dir}" 2>/dev/null && pwd) || echo "${dir}"
}

# @cmd Verify the project builds successfully
verify_build() {
  local project_dir
  project_dir=$(_project_dir)
  
  local project_info
  project_info=$(detect_project "${project_dir}")
  local build_cmd
  build_cmd=$(echo "${project_info}" | jq -r '.check // .build')
  
  if [[ -z "${build_cmd}" ]] || [[ "${build_cmd}" == "null" ]]; then
    warn "No build command detected" >> "$LLM_OUTPUT"
    return 0
  fi
  
  info "Running: ${build_cmd}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  local output exit_code=0
  output=$(cd "${project_dir}" && eval "${build_cmd}" 2>&1) || exit_code=$?
  
  echo "${output}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  if [[ ${exit_code} -eq 0 ]]; then
    green "BUILD SUCCESS" >> "$LLM_OUTPUT"
    return 0
  else
    error "BUILD FAILED (exit code: ${exit_code})" >> "$LLM_OUTPUT"
    return 1
  fi
}

# @cmd Run project tests
run_tests() {
  local project_dir
  project_dir=$(_project_dir)
  
  local project_info
  project_info=$(detect_project "${project_dir}")
  local test_cmd
  test_cmd=$(echo "${project_info}" | jq -r '.test')
  
  if [[ -z "${test_cmd}" ]] || [[ "${test_cmd}" == "null" ]]; then
    warn "No test command detected" >> "$LLM_OUTPUT"
    return 0
  fi
  
  info "Running: ${test_cmd}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  local output exit_code=0
  output=$(cd "${project_dir}" && eval "${test_cmd}" 2>&1) || exit_code=$?
  
  echo "${output}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  if [[ ${exit_code} -eq 0 ]]; then
    green "TESTS PASSED" >> "$LLM_OUTPUT"
    return 0
  else
    error "TESTS FAILED (exit code: ${exit_code})" >> "$LLM_OUTPUT"
    return 1
  fi
}

# @cmd Get project structure for context
get_project_structure() {
  local project_dir
  project_dir=$(_project_dir)
  
  local project_info
  project_info=$(detect_project "${project_dir}")

  {
  	info "Project: $(echo "${project_info}" | jq -r '.type')"
  	echo ""
  
  	get_tree "${project_dir}" 2
  } >> "$LLM_OUTPUT"
}

