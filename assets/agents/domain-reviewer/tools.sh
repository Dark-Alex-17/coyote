#!/usr/bin/env bash
set -eo pipefail

# @env LLM_OUTPUT=/dev/stdout
# @env LLM_AGENT_VAR_PROJECT_DIR=.
# @describe Domain-slice reviewer tools

_project_dir() {
  local dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
  (cd "${dir}" 2>/dev/null && pwd) || echo "${dir}"
}

# @cmd Get the git diff for this domain's file slice. Defaults to staged changes, falls back to unstaged, then HEAD~1. Pass --base for a ref or range (e.g. "main...HEAD").
# @option --base Optional base ref or range to diff against (e.g. "main...HEAD", "HEAD~3", a SHA)
# @option --files Comma-separated list of file paths to limit the diff to (your assignment's files)
get_diff() {
  local project_dir
  project_dir=$(_project_dir)
  # shellcheck disable=SC2154
  local base="${argc_base:-}"
  # shellcheck disable=SC2154
  local files_csv="${argc_files:-}"

  local -a path_args=()
  if [[ -n "${files_csv}" ]]; then
    local IFS=','
    local f
    for f in ${files_csv}; do
      f="$(echo "${f}" | sed -e 's/^ *//' -e 's/ *$//')"
      [[ -n "${f}" ]] && path_args+=("${f}")
    done
  fi

  _diff() {
    if ((${#path_args[@]})); then
      (cd "${project_dir}" && git diff "$@" -- "${path_args[@]}" 2>&1) || true
    else
      (cd "${project_dir}" && git diff "$@" 2>&1) || true
    fi
  }

  local diff_output=""
  if [[ -n "${base}" ]]; then
    diff_output=$(_diff "${base}")
  else
    diff_output=$(_diff --cached)
    [[ -z "${diff_output}" ]] && diff_output=$(_diff)
    [[ -z "${diff_output}" ]] && diff_output=$(_diff HEAD~1)
  fi

  if [[ -z "${diff_output}" ]]; then
    echo "No changes found for the requested files in ${project_dir}." >> "$LLM_OUTPUT"
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
