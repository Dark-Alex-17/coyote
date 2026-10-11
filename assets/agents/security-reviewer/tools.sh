#!/usr/bin/env bash
set -eo pipefail

# @env LLM_OUTPUT=/dev/stdout
# @env LLM_AGENT_VAR_PROJECT_DIR=.
# @describe Security reviewer tools

_project_dir() {
  local dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
  (cd "${dir}" 2>/dev/null && pwd) || echo "${dir}"
}

# Bare refs are diffed from their merge-base with HEAD (three-dot semantics) so
# commits that landed on the base after branching never surface as changes.
# Explicit `..` ranges pass through. Returns nonzero when no merge-base exists.
_resolve_base() {
  local project_dir="$1" base="$2" mb
  case "${base}" in *..*) printf '%s' "${base}"; return ;; esac
  if mb=$(cd "${project_dir}" && git merge-base "${base}" HEAD 2>/dev/null) && [[ -n "${mb}" ]]; then
    printf '%s' "${mb}"
  else
    printf '%s' "${base}"
    return 1
  fi
}

# @cmd Get the git diff to review for security flaws. Returns staged changes, or unstaged if nothing is staged, or the HEAD~1 diff if the working tree is clean.
# @option --base Optional base ref to diff against (e.g., "main", "HEAD~3", a commit SHA, or a PR base branch). Bare refs are diffed from their merge-base with HEAD; pass an explicit "a..b" range to override.
get_diff() {
  local project_dir
  project_dir=$(_project_dir)
  # shellcheck disable=SC2154
  local base="${argc_base:-}"

  local diff_output=""
  if [[ -n "${base}" ]]; then
    local resolved
    resolved=$(_resolve_base "${project_dir}" "${base}") ||
      echo "No merge-base between '${base}' and HEAD; diffing against '${base}' directly." >> "$LLM_OUTPUT"
    diff_output=$(cd "${project_dir}" && git diff "${resolved}" 2>&1) || true
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
    echo "No changes found to review in ${project_dir}." >> "$LLM_OUTPUT"
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

# @cmd Get the list of changed files with stats (a quick map of the attack surface under review).
# @option --base Optional base ref to diff against. Bare refs are diffed from their merge-base with HEAD; pass an explicit "a..b" range to override.
get_changed_files() {
  local project_dir
  project_dir=$(_project_dir)
  local base="${argc_base:-}"

  local stat_output=""
  if [[ -n "${base}" ]]; then
    local resolved
    resolved=$(_resolve_base "${project_dir}" "${base}") ||
      echo "No merge-base between '${base}' and HEAD; diffing against '${base}' directly." >> "$LLM_OUTPUT"
    stat_output=$(cd "${project_dir}" && git diff --stat "${resolved}" 2>&1) || true
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
