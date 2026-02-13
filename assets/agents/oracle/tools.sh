#!/usr/bin/env bash
set -eo pipefail

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"
source "$LLM_ROOT_DIR/agents/.shared/utils.sh"

# @env LLM_OUTPUT=/dev/stdout
# @env LLM_AGENT_VAR_PROJECT_DIR=.
# @describe Oracle agent tools for analysis and consultation (read-only)

_project_dir() {
  local dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
  (cd "${dir}" 2>/dev/null && pwd) || echo "${dir}"
}

# @cmd Read a file for analysis
# @option --path! Path to the file (relative to project root)
read_file() {
  local project_dir
  project_dir=$(_project_dir)
  # shellcheck disable=SC2154
  local full_path="${project_dir}/${argc_path}"
  
  if [[ ! -f "${full_path}" ]]; then
    error "File not found: ${argc_path}" >> "$LLM_OUTPUT"
    return 1
  fi

  {
  	info "Reading: ${argc_path}"
  	echo ""
  	cat "${full_path}"
  } >> "$LLM_OUTPUT"
}

# @cmd Get project structure and type
get_project_info() {
  local project_dir
  project_dir=$(_project_dir)
  
  local project_info
  project_info=$(detect_project "${project_dir}")

  {
		info "Project Analysis" >> "$LLM_OUTPUT"
		cat <<-EOF

			Type: $(echo "${project_info}" | jq -r '.type')
			Build: $(echo "${project_info}" | jq -r '.build')
			Test: $(echo "${project_info}" | jq -r '.test')

		EOF

		info "Structure:" >> "$LLM_OUTPUT"
		get_tree "${project_dir}" 3
  } >> "$LLM_OUTPUT"
}

# @cmd Search for patterns in the codebase
# @option --pattern! Pattern to search for
# @option --file-type Filter by extension (e.g., "rs", "py")
search_code() {
  local file_type="${argc_file_type:-}"
  local project_dir
  project_dir=$(_project_dir)

  # shellcheck disable=SC2154
  info "Searching: ${argc_pattern}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  local include_arg=""
  if [[ -n "${file_type}" ]]; then
    include_arg="--include=*.${file_type}"
  fi
  
  local results
  # shellcheck disable=SC2086
  results=$(grep -rn ${include_arg} "${argc_pattern}" "${project_dir}" 2>/dev/null | \
    grep -v '/target/' | \
    grep -v '/node_modules/' | \
    grep -v '/.git/' | \
    head -30) || true
  
  if [[ -n "${results}" ]]; then
    echo "${results}" >> "$LLM_OUTPUT"
  else
    warn "No matches found" >> "$LLM_OUTPUT"
  fi
}

# @cmd Run a read-only command for analysis (e.g., git log, cargo tree)
# @option --command! Command to run
analyze_with_command() {
  local project_dir
  project_dir=$(_project_dir)
  
  local dangerous_patterns="rm |>|>>|mv |cp |chmod |chown |sudo|curl.*\\||wget.*\\|"
  # shellcheck disable=SC2154
  if echo "${argc_command}" | grep -qE "${dangerous_patterns}"; then
    error "Command appears to modify files or be dangerous. Oracle is read-only." >> "$LLM_OUTPUT"
    return 1
  fi
  
  info "Running: ${argc_command}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  local output
  output=$(cd "${project_dir}" && eval "${argc_command}" 2>&1) || true
  echo "${output}" >> "$LLM_OUTPUT"
}

# @cmd List directory contents
# @option --path Path to list (default: project root)
list_directory() {
  local dir_path="${argc_path:-.}"
  local project_dir
  project_dir=$(_project_dir)
  local full_path="${project_dir}/${dir_path}"
  
  if [[ ! -d "${full_path}" ]]; then
    error "Directory not found: ${dir_path}" >> "$LLM_OUTPUT"
    return 1
  fi

  {
		info "Contents of: ${dir_path}"
		echo ""
		ls -la "${full_path}"
  } >> "$LLM_OUTPUT"
}
