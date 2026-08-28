#!/usr/bin/env bash
set -eo pipefail

# @env LLM_OUTPUT=/dev/stdout
# @env LLM_AGENT_VAR_PROJECT_DIR=.
# @describe Usage-pattern probe tools

_project_dir() {
  local dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
  (cd "${dir}" 2>/dev/null && pwd) || echo "${dir}"
}

# @cmd Get the git diff whose consumer-facing surface is under probe. Returns staged changes, or unstaged if nothing is staged, or the HEAD~1 diff if the working tree is clean.
# @option --base Optional base ref to diff against (e.g., "main", "HEAD~3", a commit SHA, or a task's base SHA)
get_diff() {
  local project_dir
  project_dir=$(_project_dir)
  # shellcheck disable=SC2154
  local base="${argc_base:-}"

  local diff_output=""
  if [[ -n "${base}" ]]; then
    diff_output=$(cd "${project_dir}" && git diff "${base}" 2>&1) || true
  else
    diff_output=$(cd "${project_dir}" && git diff --cached 2>&1) || true
    if [[ -z "${diff_output}" ]]; then
      diff_output=$(cd "${project_dir}" && git diff 2>&1) || true
    fi
    if [[ -z "${diff_output}" ]]; then
      diff_output=$(cd "${project_dir}" && git diff HEAD~1 2>&1) || true
    fi
  fi

  if [[ -z "${diff_output}" ]]; then
    echo "No changes found to probe in ${project_dir}." >> "$LLM_OUTPUT"
    return 0
  fi

  local file_count
  file_count=$(echo "${diff_output}" | grep -c '^diff --git' || true)
  {
    echo "Diff contains changes to ${file_count} file(s):"
    echo ""
    echo "${diff_output}"
  } >> "$LLM_OUTPUT"
}

# @cmd Get the list of changed files with stats (a quick map for locating the changed consumer-facing surface).
# @option --base Optional base ref to diff against
get_changed_files() {
  local project_dir
  project_dir=$(_project_dir)
  local base="${argc_base:-}"

  local stat_output=""
  if [[ -n "${base}" ]]; then
    stat_output=$(cd "${project_dir}" && git diff --stat "${base}" 2>&1) || true
  else
    stat_output=$(cd "${project_dir}" && git diff --cached --stat 2>&1) || true
    if [[ -z "${stat_output}" ]]; then
      stat_output=$(cd "${project_dir}" && git diff --stat 2>&1) || true
    fi
    if [[ -z "${stat_output}" ]]; then
      stat_output=$(cd "${project_dir}" && git diff --stat HEAD~1 2>&1) || true
    fi
  fi

  if [[ -z "${stat_output}" ]]; then
    echo "No changes found in ${project_dir}." >> "$LLM_OUTPUT"
    return 0
  fi

  {
    echo "Changed files:"
    echo ""
    echo "${stat_output}"
  } >> "$LLM_OUTPUT"
}
