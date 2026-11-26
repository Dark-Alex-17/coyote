#!/usr/bin/env bash
set -eo pipefail
# shellcheck disable=SC1090
source "$LLM_ROOT_DIR/agents/.shared/utils.sh"

# @env LLM_OUTPUT=/dev/stdout
# @env LLM_AGENT_VAR_PROJECT_DIR=.
# @describe Design retrospective tools: read-only views of the change under review

_project_dir() {
  local dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
  (cd "${dir}" 2>/dev/null && pwd) || echo "${dir}"
}

# Sets REPLY to the base to diff from, noting a missing merge-base in the
# output. Runs in the caller's shell, not a $(...), so that note reaches
# LLM_OUTPUT even when it is /dev/stdout. Returns 1 for a refused base.
_diff_base() {
  local rc=0
  REPLY=$(resolve_diff_base "$1" "$2") || rc=$?
  if ((rc == 2)); then
    return 1
  elif ((rc)); then
    echo "No merge-base between '$2' and HEAD; diffing against '$2' directly." >> "$LLM_OUTPUT"
  fi
}

# @cmd Internal dynamic-instructions hook - renders instructions.md against the agent variables. Not for model use.
_instructions() {
  render_instructions "$LLM_AGENT_ROOT_DIR/instructions.md" >> "$LLM_OUTPUT"
}

# @cmd List the files the change touched, with line stats and their add/modify/delete/rename status.
# @option --base! Base of the change: a ref, a commit SHA, or an explicit "a..b" range. Bare refs are diffed from their merge-base with HEAD. Pass HEAD for uncommitted changes.
get_changed_files() {
  local project_dir
  project_dir=$(_project_dir)
  # shellcheck disable=SC2154
  _diff_base "${project_dir}" "${argc_base}" || return 1
  local stat_output status_output
  stat_output=$(cd "${project_dir}" && git diff --stat --no-ext-diff --no-textconv "${REPLY}" -- 2>&1) || true
  status_output=$(cd "${project_dir}" && git diff --name-status --no-ext-diff --no-textconv "${REPLY}" -- 2>&1) || true
  if [[ -z "${stat_output}" ]]; then
    echo "No changes found in ${project_dir} against '${argc_base}'." >> "$LLM_OUTPUT"
    return 0
  fi
  {
    echo "Changed files:"
    echo ""
    echo "${stat_output}"
    echo ""
    echo "Status:"
    echo "${status_output}"
  } >> "$LLM_OUTPUT"
}

# @cmd Show the diff of the change, whole or for one file, to locate hunks before reading files in full.
# @option --base! Base of the change: a ref, a commit SHA, or an explicit "a..b" range. Bare refs are diffed from their merge-base with HEAD. Pass HEAD for uncommitted changes.
# @option --path Optional file path, relative to the project, to limit the diff to that one file.
get_diff() {
  local project_dir
  project_dir=$(_project_dir)
  _diff_base "${project_dir}" "${argc_base}" || return 1
  local paths=()
  # shellcheck disable=SC2154
  if [[ -n "${argc_path:-}" ]]; then
    paths=("${argc_path}")
  fi
  local diff_output
  if ((${#paths[@]})); then
    diff_output=$(cd "${project_dir}" && git diff --no-ext-diff --no-textconv "${REPLY}" -- "${paths[@]}" 2>&1) || true
  else
    diff_output=$(cd "${project_dir}" && git diff --no-ext-diff --no-textconv "${REPLY}" -- 2>&1) || true
  fi
  if [[ -z "${diff_output}" ]]; then
    echo "No changes found in ${project_dir} against '${argc_base}'." >> "$LLM_OUTPUT"
    return 0
  fi
  echo "${diff_output}" >> "$LLM_OUTPUT"
}

# @cmd List the commits of the change, oldest last, to see how the work was sliced.
# @option --base! Base of the change: a ref, a commit SHA, or an explicit "a..b" range. Bare refs start at their merge-base with HEAD.
get_log() {
  local project_dir
  project_dir=$(_project_dir)
  _diff_base "${project_dir}" "${argc_base}" || return 1
  local range="${REPLY}"
  [[ "${range}" == *..* ]] || range="${range}..HEAD"
  local log_output
  log_output=$(cd "${project_dir}" && git log --oneline --no-decorate "${range}" -- 2>&1) || true
  if [[ -z "${log_output}" ]]; then
    echo "No commits in ${range}." >> "$LLM_OUTPUT"
    return 0
  fi
  echo "${log_output}" >> "$LLM_OUTPUT"
}
