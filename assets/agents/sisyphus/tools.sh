#!/usr/bin/env bash
set -eo pipefail
# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"
source "$LLM_ROOT_DIR/agents/.shared/utils.sh"
export AUTO_CONFIRM=true

# @env LLM_OUTPUT=/dev/stdout
# @env LLM_AGENT_VAR_PROJECT_DIR=.
# @describe Sisyphus orchestrator tools (project info, build, test)

_project_dir() {
  local dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
  (cd "${dir}" 2>/dev/null && pwd) || echo "${dir}"
}

# @cmd Internal dynamic-instructions hook - renders instructions.md against the agent variables. Not for model use.
_instructions() {
  render_instructions "$LLM_AGENT_ROOT_DIR/instructions.md" >> "$LLM_OUTPUT"
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

  info "Running: $(gate_ran "${build_cmd}")" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"

  local output
  if output=$(cd "${project_dir}" && run_gate "${build_cmd}" 2>&1); then
    green "BUILD SUCCESS" >> "$LLM_OUTPUT"
    echo "${output}" >> "$LLM_OUTPUT"
    return 0
  else
    error "BUILD FAILED" >> "$LLM_OUTPUT"
    echo "${output}" >> "$LLM_OUTPUT"
    return 1
  fi
}

# @cmd Deterministic debt-marker scan over the working-tree diff - replaces the manual ls-files/add -N/diff/grep pipeline. Prints one `<file>:<line> -- <line text>` per ADDED debt marker with real positions already resolved (bare `cfg_attr(` without allow|expect already dropped; "no hits (clean)" = no debt), or a DEBT_SCAN_FAILED report quoting the manual fallback.
# @option --base Diff base commit/ref (default HEAD)
# @option --dir Project directory override (default: the agent's project_dir)
debt_scan() {
  local dir
  dir="${argc_dir:-$(_project_dir)}"
  python3 "$LLM_ROOT_DIR/agents/.shared/debt_scan.py" --dir "${dir}" --base "${argc_base:-HEAD}" >> "$LLM_OUTPUT"
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

  info "Running: $(gate_ran "${test_cmd}")" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"

  local output
  if output=$(cd "${project_dir}" && run_gate "${test_cmd}" 2>&1); then
    green "TESTS PASSED" >> "$LLM_OUTPUT"
    echo "${output}" >> "$LLM_OUTPUT"
    return 0
  else
    error "TESTS FAILED" >> "$LLM_OUTPUT"
    echo "${output}" >> "$LLM_OUTPUT"
    return 1
  fi
}

