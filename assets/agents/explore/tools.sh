#!/usr/bin/env bash
set -eo pipefail

# shellcheck disable=SC1090
source "$LLM_PROMPT_UTILS_FILE"
source "$LLM_ROOT_DIR/agents/.shared/utils.sh"

# @env LLM_OUTPUT=/dev/stdout
# @env LLM_AGENT_VAR_PROJECT_DIR=.
# @describe Explore agent tools for codebase search and analysis

_project_dir() {
  local dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
  (cd "${dir}" 2>/dev/null && pwd) || echo "${dir}"
}

# @cmd Get project structure and layout
get_structure() {
  local project_dir
  project_dir=$(_project_dir)
  
  info "Project structure:" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  local project_info
  project_info=$(detect_project "${project_dir}")

  {
  	echo "Type: $(echo "${project_info}" | jq -r '.type')"
  	echo ""

  	get_tree "${project_dir}" 3
  } >> "$LLM_OUTPUT"
}

# @cmd Search for files by name pattern
# @option --pattern! File name pattern (e.g., "*.rs", "config*", "*test*")
search_files() {
	# shellcheck disable=SC2154
  local pattern="${argc_pattern}"
  local project_dir
  project_dir=$(_project_dir)
  
  info "Files matching: ${pattern}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  local results
  results=$(search_files "${pattern}" "${project_dir}")
  
  if [[ -n "${results}" ]]; then
    echo "${results}" >> "$LLM_OUTPUT"
  else
    warn "No files found" >> "$LLM_OUTPUT"
  fi
}

# @cmd Search for content in files
# @option --pattern! Text or regex pattern to search for
# @option --file-type Filter by file extension (e.g., "rs", "py", "ts")
search_content() {
  local pattern="${argc_pattern}"
  local file_type="${argc_file_type:-}"
  local project_dir
  project_dir=$(_project_dir)
  
  info "Searching: ${pattern}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  local include_arg=""
  if [[ -n "${file_type}" ]]; then
    include_arg="--include=*.${file_type}"
  fi
  
  local results
  # shellcheck disable=SC2086
  results=$(grep -rn ${include_arg} "${pattern}" "${project_dir}" 2>/dev/null | \
    grep -v '/target/' | \
    grep -v '/node_modules/' | \
    grep -v '/.git/' | \
    grep -v '/dist/' | \
    head -30) || true
  
  if [[ -n "${results}" ]]; then
    echo "${results}" >> "$LLM_OUTPUT"
  else
    warn "No matches found" >> "$LLM_OUTPUT"
  fi
}

# @cmd Read a file's contents
# @option --path! Path to the file (relative to project root)
# @option --lines Maximum lines to read (default: 200)
read_file() {
	# shellcheck disable=SC2154
  local file_path="${argc_path}"
  local max_lines="${argc_lines:-200}"
  local project_dir
  project_dir=$(_project_dir)
  
  local full_path="${project_dir}/${file_path}"
  
  if [[ ! -f "${full_path}" ]]; then
    error "File not found: ${file_path}" >> "$LLM_OUTPUT"
    return 1
  fi

  {
  	info "File: ${file_path}"
  	echo ""
  } >> "$LLM_OUTPUT"
  
  head -n "${max_lines}" "${full_path}" >> "$LLM_OUTPUT"
  
  local total_lines
  total_lines=$(wc -l < "${full_path}")
  if [[ "${total_lines}" -gt "${max_lines}" ]]; then
    echo "" >> "$LLM_OUTPUT"
    warn "... truncated (${total_lines} total lines)" >> "$LLM_OUTPUT"
  fi
}

# @cmd Find similar files to a given file (for pattern matching)
# @option --path! Path to the reference file
find_similar() {
  local file_path="${argc_path}"
  local project_dir
  project_dir=$(_project_dir)
  
  local ext="${file_path##*.}"
  local dir
  dir=$(dirname "${file_path}")
  
  info "Files similar to: ${file_path}" >> "$LLM_OUTPUT"
  echo "" >> "$LLM_OUTPUT"
  
  local results
  results=$(find "${project_dir}/${dir}" -maxdepth 1 -type f -name "*.${ext}" \
    ! -name "$(basename "${file_path}")" \
    ! -name "*test*" \
    ! -name "*spec*" \
    2>/dev/null | head -5)
  
  if [[ -n "${results}" ]]; then
    echo "${results}" >> "$LLM_OUTPUT"
  else
    results=$(find "${project_dir}" -type f -name "*.${ext}" \
      ! -name "$(basename "${file_path}")" \
      ! -name "*test*" \
      -not -path '*/target/*' \
      2>/dev/null | head -5)
    if [[ -n "${results}" ]]; then
      echo "${results}" >> "$LLM_OUTPUT"
    else
      warn "No similar files found" >> "$LLM_OUTPUT"
    fi
  fi
}