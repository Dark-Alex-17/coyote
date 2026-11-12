#!/usr/bin/env bash
set -eo pipefail

# @env LLM_OUTPUT=/dev/stdout
# @env LLM_AGENT_VAR_PROJECT_DIR=.
# @describe Domain-slice reviewer tools

_project_dir() {
  local dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
  (cd "${dir}" 2>/dev/null && pwd) || echo "${dir}"
}

# Bare refs are diffed from their merge-base with HEAD (three-dot semantics) so
# commits that landed on the base after branching never surface as changes.
# Explicit `..` ranges pass through, and so does a checkpoint commit on top of
# HEAD that no ref holds, whose merge-base would be HEAD. Prints the base to
# diff from. Returns 1 when there is no merge-base (printing the base as given)
# and 2, printing nothing, for an option-shaped base that git would read as a
# flag.
_resolve_base() {
  local project_dir="$1" base="$2" mb head commit held
  case "${base}" in
    -*) echo "error: --base '${base}' starts with '-'; pass a ref, a commit SHA or an a..b range" >&2; return 2 ;;
    *..*) printf '%s' "${base}"; return ;;
  esac
  if mb=$(cd "${project_dir}" && git merge-base "${base}" HEAD 2>/dev/null) && [[ -n "${mb}" ]]; then
    head=$(cd "${project_dir}" && git rev-parse HEAD 2>/dev/null) || head=""
    commit=$(cd "${project_dir}" && git rev-parse --verify --quiet "${base}^{commit}" 2>/dev/null) || commit=""
    if [[ -n "${head}" && -n "${commit}" && "${mb}" == "${head}" && "${commit}" != "${head}" ]]; then
      # a commit some ref holds is a real base ahead of HEAD, never a checkpoint
      if held=$(cd "${project_dir}" && git for-each-ref --count=1 --contains "${commit}" 2>/dev/null) && [[ -z "${held}" ]]; then
        printf '%s' "${base}"
        return
      fi
    fi
    printf '%s' "${mb}"
  else
    printf '%s' "${base}"
    return 1
  fi
}

# @cmd Get the git diff for this domain's file slice. Defaults to staged changes, falls back to unstaged, then HEAD~1. Pass --base for a ref or range (e.g. "main...HEAD").
# @option --base Optional base ref or range to diff against (e.g. "main...HEAD", "HEAD~3", a SHA). Bare refs are diffed from their merge-base with HEAD; explicit ranges are used as given.
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
      (cd "${project_dir}" && git diff "$@" -- 2>&1) || true
    fi
  }

  local diff_output=""
  if [[ -n "${base}" ]]; then
    local resolved rc=0
    resolved=$(_resolve_base "${project_dir}" "${base}") || rc=$?
    if ((rc == 2)); then
      return 1
    elif ((rc)); then
      echo "No merge-base between '${base}' and HEAD; diffing against '${base}' directly." >> "$LLM_OUTPUT"
    fi
    diff_output=$(_diff --no-ext-diff --no-textconv "${resolved}")
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
