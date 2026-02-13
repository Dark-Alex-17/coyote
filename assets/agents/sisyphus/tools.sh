#!/usr/bin/env bash
set -eo pipefail
# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"
source "$LLM_ROOT_DIR/agents/.shared/utils.sh"
export AUTO_CONFIRM=true

# @env LLM_OUTPUT=/dev/stdout
# @env LLM_AGENT_VAR_PROJECT_DIR=.
# @describe Sisyphus orchestrator tools for delegating to specialized agents

_project_dir() {
  local dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
  (cd "${dir}" 2>/dev/null && pwd) || echo "${dir}"
}

# @cmd Initialize context for a new task (call at the start of multi-step work)
# @option --goal! Description of the overall task/goal
start_task() {
  local project_dir
  project_dir=$(_project_dir)
  
  export LLM_AGENT_VAR_PROJECT_DIR="${project_dir}"
  # shellcheck disable=SC2154
  init_context "${argc_goal}"

  cat <<-EOF >> "$LLM_OUTPUT"
  	$(green "Context initialized for task: ${argc_goal}")
  	Context file: ${project_dir}/.loki-context
	EOF
}

# @cmd Add a finding to the shared context (useful for recording discoveries)
# @option --source! Source of the finding (e.g., "manual", "explore", "coder")
# @option --finding! The finding to record
record_finding() {
  local project_dir
  project_dir=$(_project_dir)
  
  export LLM_AGENT_VAR_PROJECT_DIR="${project_dir}"
  # shellcheck disable=SC2154
  append_context "${argc_source}" "${argc_finding}"
  
  green "Recorded finding from ${argc_source}" >> "$LLM_OUTPUT"
}

# @cmd Show current accumulated context
show_context() {
  local project_dir
  project_dir=$(_project_dir)
  
  export LLM_AGENT_VAR_PROJECT_DIR="${project_dir}"
  local context
  context=$(read_context)
  
  if [[ -n "${context}" ]]; then
    cat <<-EOF >> "$LLM_OUTPUT"
    	$(info "Current Context:")

    	${context}
		EOF
  else
    warn "No context file found. Use start_task to initialize." >> "$LLM_OUTPUT"
  fi
}

# @cmd Clear the context file (call when task is complete)
end_task() {
  local project_dir
  project_dir=$(_project_dir)
  
  export LLM_AGENT_VAR_PROJECT_DIR="${project_dir}"
  clear_context
  
  green "Context cleared. Task complete." >> "$LLM_OUTPUT"
}

# @cmd Delegate a task to a specialized agent
# @option --agent! Agent to delegate to: explore, coder, or oracle
# @option --task! Specific task description for the agent
# @option --context Additional context (file paths, patterns, constraints)
delegate_to_agent() {
  local extra_context="${argc_context:-}"
  local project_dir
  project_dir=$(_project_dir)

  # shellcheck disable=SC2154
  if [[ ! "${argc_agent}" =~ ^(explore|coder|oracle)$ ]]; then
    error "Invalid agent: ${argc_agent}. Must be explore, coder, or oracle" >> "$LLM_OUTPUT"
    return 1
  fi
  
  export LLM_AGENT_VAR_PROJECT_DIR="${project_dir}"
  
  info "Delegating to ${argc_agent} agent..." >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"

	# shellcheck disable=SC2154
  local prompt="${argc_task}"
  if [[ -n "${extra_context}" ]]; then
    prompt="$(printf "%s\n\nAdditional Context:\n%s\n" "${argc_task}" "${extra_context}")"
  fi

  cat <<-EOF >> "$LLM_OUTPUT"
  $(cyan "------------------------------------------")
  DELEGATING TO: ${argc_agent}
  TASK: ${argc_task}
  $(cyan "------------------------------------------")

	EOF
  
  local output
  output=$(invoke_agent_with_summary "${argc_agent}" "${prompt}" \
    --agent-variable project_dir "${project_dir}" 2>&1) || true

  cat <<-EOF >> "$LLM_OUTPUT"
  ${output}

  $(cyan "------------------------------------------")
  DELEGATION COMPLETE: ${argc_agent}
  $(cyan "------------------------------------------")
	EOF
}

# @cmd Get project information and structure
get_project_info() {
  local project_dir
  project_dir=$(_project_dir)
  
  info "Project: ${project_dir}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"

  local project_info
  project_info=$(detect_project "${project_dir}")

  cat <<-EOF >> "$LLM_OUTPUT"
		Type: $(echo "${project_info}" | jq -r '.type')
		Build: $(echo "${project_info}" | jq -r '.build')
		Test: $(echo "${project_info}" | jq -r '.test')

		$(info "Directory structure:")
		$(get_tree "${project_dir}" 2)
	EOF
}

# @cmd Run build command for the project
run_build() {
  local project_dir
  project_dir=$(_project_dir)
  
  local project_info
  project_info=$(detect_project "${project_dir}")
  local build_cmd
  build_cmd=$(echo "${project_info}" | jq -r '.build')
  
  if [[ -z "${build_cmd}" ]] || [[ "${build_cmd}" == "null" ]]; then
    warn "No build command detected for this project" >> "$LLM_OUTPUT"
    return 0
  fi
  
  info "Running: ${build_cmd}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"

  local output
  if output=$(cd "${project_dir}" && eval "${build_cmd}" 2>&1); then
    green "BUILD SUCCESS" >> "$LLM_OUTPUT"
    echo "${output}" >> "$LLM_OUTPUT"
    return 0
  else
    error "BUILD FAILED" >> "$LLM_OUTPUT"
    echo "${output}" >> "$LLM_OUTPUT"
    return 1
  fi
}

# @cmd Run tests for the project
run_tests() {
  local project_dir
  project_dir=$(_project_dir)
  
  local project_info
  project_info=$(detect_project "${project_dir}")
  local test_cmd
  test_cmd=$(echo "${project_info}" | jq -r '.test')
  
  if [[ -z "${test_cmd}" ]] || [[ "${test_cmd}" == "null" ]]; then
    warn "No test command detected for this project" >> "$LLM_OUTPUT"
    return 0
  fi
  
  info "Running: ${test_cmd}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"

  local output
  if output=$(cd "${project_dir}" && eval "${test_cmd}" 2>&1); then
    green "TESTS PASSED" >> "$LLM_OUTPUT"
    echo "${output}" >> "$LLM_OUTPUT"
    return 0
  else
    error "TESTS FAILED" >> "$LLM_OUTPUT"
    echo "${output}" >> "$LLM_OUTPUT"
    return 1
  fi
}

# @cmd Quick file search in the project
# @option --pattern! File name pattern to search for (e.g., "*.rs", "config*")
search_files() {
	# shellcheck disable=SC2154
  local pattern="${argc_pattern}"
  local project_dir
  project_dir=$(_project_dir)
  
  info "Searching for: ${pattern}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"

  local results
  results=$(search_files "${pattern}" "${project_dir}")
  
  if [[ -n "${results}" ]]; then
    echo "${results}" >> "$LLM_OUTPUT"
  else
    warn "No files found matching: ${pattern}" >> "$LLM_OUTPUT"
  fi
}

# @cmd Search for content in files
# @option --pattern! Text pattern to search for
# @option --file-type File extension to search in (e.g., "rs", "py")
search_content() {
  local pattern="${argc_pattern}"
  local file_type="${argc_file_type:-}"
  local project_dir
  project_dir=$(_project_dir)
  
  info "Searching for: ${pattern}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  local grep_args="-rn"
  if [[ -n "${file_type}" ]]; then
    grep_args="${grep_args} --include=*.${file_type}"
  fi
  
  local results
  # shellcheck disable=SC2086
  results=$(grep ${grep_args} "${pattern}" "${project_dir}" 2>/dev/null | \
    grep -v '/target/' | grep -v '/node_modules/' | grep -v '/.git/' | \
    head -30) || true
  
  if [[ -n "${results}" ]]; then
    echo "${results}" >> "$LLM_OUTPUT"
  else
    warn "No matches found for: ${pattern}" >> "$LLM_OUTPUT"
  fi
}