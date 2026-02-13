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

# @cmd Read a file's contents before modifying
# @option --path! Path to the file (relative to project root)
read_file() {
	# shellcheck disable=SC2154
  local file_path="${argc_path}"
  local project_dir
  project_dir=$(_project_dir)
  local full_path="${project_dir}/${file_path}"
  
  if [[ ! -f "${full_path}" ]]; then
    warn "File not found: ${file_path}" >> "$LLM_OUTPUT"
    return 0
  fi
  
  {
  	info "Reading: ${file_path}"
  	echo ""
  	cat "${full_path}"
  } >> "$LLM_OUTPUT"
}

# @cmd Write complete file contents
# @option --path! Path for the file (relative to project root)
# @option --content! Complete file contents to write
write_file() {
  local file_path="${argc_path}"
  # shellcheck disable=SC2154
  local content="${argc_content}"
  local project_dir
  project_dir=$(_project_dir)
  local full_path="${project_dir}/${file_path}"
  
  mkdir -p "$(dirname "${full_path}")"
  echo "${content}" > "${full_path}"
  
  green "Wrote: ${file_path}" >> "$LLM_OUTPUT"
}

# @cmd Find files similar to a given path (for pattern matching)
# @option --path! Path to find similar files for
find_similar_files() {
  local file_path="${argc_path}"
  local project_dir
  project_dir=$(_project_dir)
  
  local ext="${file_path##*.}"
  local dir
  dir=$(dirname "${file_path}")
  
  info "Similar files to: ${file_path}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  local results
  results=$(find "${project_dir}/${dir}" -maxdepth 1 -type f -name "*.${ext}" \
    ! -name "$(basename "${file_path}")" \
    ! -name "*test*" \
    ! -name "*spec*" \
    2>/dev/null | head -3)
  
  if [[ -z "${results}" ]]; then
    results=$(find "${project_dir}/src" -type f -name "*.${ext}" \
      ! -name "*test*" \
      ! -name "*spec*" \
      -not -path '*/target/*' \
      2>/dev/null | head -3)
  fi
  
  if [[ -n "${results}" ]]; then
    echo "${results}" >> "$LLM_OUTPUT"
  else
    warn "No similar files found" >> "$LLM_OUTPUT"
  fi
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

# @cmd Search for content in the codebase
# @option --pattern! Pattern to search for
search_code() {
	# shellcheck disable=SC2154
  local pattern="${argc_pattern}"
  local project_dir
  project_dir=$(_project_dir)
  
  info "Searching: ${pattern}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  local results
  results=$(grep -rn "${pattern}" "${project_dir}" 2>/dev/null | \
    grep -v '/target/' | \
    grep -v '/node_modules/' | \
    grep -v '/.git/' | \
    head -20) || true
  
  if [[ -n "${results}" ]]; then
    echo "${results}" >> "$LLM_OUTPUT"
  else
    warn "No matches" >> "$LLM_OUTPUT"
  fi
}