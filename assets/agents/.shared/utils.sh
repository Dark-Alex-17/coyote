#!/usr/bin/env bash
# Shared Agent Utilities - Minimal, focused helper functions
set -euo pipefail

#######################
## PROJECT DETECTION ##
#######################

# Cache file name for detected project info
_LOKI_PROJECT_CACHE=".loki-project.json"

# Read cached project detection if valid
# Usage: _read_project_cache "/path/to/project"
# Returns: cached JSON on stdout (exit 0) or nothing (exit 1)
_read_project_cache() {
  local dir="$1"
  local cache_file="${dir}/${_LOKI_PROJECT_CACHE}"

  if [[ -f "${cache_file}" ]]; then
    local cached
    cached=$(cat "${cache_file}" 2>/dev/null) || return 1
    if echo "${cached}" | jq -e '.type and .build != null and .test != null and .check != null' &>/dev/null; then
      echo "${cached}"
      return 0
    fi
  fi
  return 1
}

# Write project detection result to cache
# Usage: _write_project_cache "/path/to/project" '{"type":"rust",...}'
_write_project_cache() {
  local dir="$1"
  local json="$2"
  local cache_file="${dir}/${_LOKI_PROJECT_CACHE}"

  echo "${json}" > "${cache_file}" 2>/dev/null || true
}

_detect_heuristic() {
  local dir="$1"

  # Rust
  if [[ -f "${dir}/Cargo.toml" ]]; then
    echo '{"type":"rust","build":"cargo build","test":"cargo test","check":"cargo check"}'
    return 0
  fi

  # Go
  if [[ -f "${dir}/go.mod" ]]; then
    echo '{"type":"go","build":"go build ./...","test":"go test ./...","check":"go vet ./..."}'
    return 0
  fi

  # Node.JS/Deno/Bun
  if [[ -f "${dir}/deno.json" ]] || [[ -f "${dir}/deno.jsonc" ]]; then
    echo '{"type":"deno","build":"deno task build","test":"deno test","check":"deno lint"}'
    return 0
  fi

  if [[ -f "${dir}/package.json" ]]; then
    local pm="npm"

    [[ -f "${dir}/bun.lockb" ]] || [[ -f "${dir}/bun.lock" ]] && pm="bun"
    [[ -f "${dir}/pnpm-lock.yaml" ]] && pm="pnpm"
    [[ -f "${dir}/yarn.lock" ]] && pm="yarn"

    echo "{\"type\":\"nodejs\",\"build\":\"${pm} run build\",\"test\":\"${pm} test\",\"check\":\"${pm} run lint\"}"
    return 0
  fi

  # Python
  if [[ -f "${dir}/pyproject.toml" ]] || [[ -f "${dir}/setup.py" ]] || [[ -f "${dir}/setup.cfg" ]]; then
    local test_cmd="pytest"
    local check_cmd="ruff check ."

    if [[ -f "${dir}/poetry.lock" ]]; then
      test_cmd="poetry run pytest"
      check_cmd="poetry run ruff check ."
    elif [[ -f "${dir}/uv.lock" ]]; then
      test_cmd="uv run pytest"
      check_cmd="uv run ruff check ."
    fi

    echo "{\"type\":\"python\",\"build\":\"\",\"test\":\"${test_cmd}\",\"check\":\"${check_cmd}\"}"
    return 0
  fi

  # JVM (Maven)
  if [[ -f "${dir}/pom.xml" ]]; then
    echo '{"type":"java","build":"mvn compile","test":"mvn test","check":"mvn verify"}'
    return 0
  fi

  # JVM (Gradle)
  if [[ -f "${dir}/build.gradle" ]] || [[ -f "${dir}/build.gradle.kts" ]]; then
    local gw="gradle"
    [[ -f "${dir}/gradlew" ]] && gw="./gradlew"
    echo "{\"type\":\"java\",\"build\":\"${gw} build\",\"test\":\"${gw} test\",\"check\":\"${gw} check\"}"
    return 0
  fi

  # .NET / C#
  if compgen -G "${dir}/*.sln" &>/dev/null || compgen -G "${dir}/*.csproj" &>/dev/null; then
    echo '{"type":"dotnet","build":"dotnet build","test":"dotnet test","check":"dotnet build --warnaserrors"}'
    return 0
  fi

  # C/C++ (CMake)
  if [[ -f "${dir}/CMakeLists.txt" ]]; then
    echo '{"type":"cmake","build":"cmake --build build","test":"ctest --test-dir build","check":"cmake --build build"}'
    return 0
  fi

  # Ruby
  if [[ -f "${dir}/Gemfile" ]]; then
    local test_cmd="bundle exec rake test"
    [[ -f "${dir}/Rakefile" ]] && grep -q "rspec" "${dir}/Gemfile" 2>/dev/null && test_cmd="bundle exec rspec"
    echo "{\"type\":\"ruby\",\"build\":\"\",\"test\":\"${test_cmd}\",\"check\":\"bundle exec rubocop\"}"
    return 0
  fi

  # Elixir
  if [[ -f "${dir}/mix.exs" ]]; then
    echo '{"type":"elixir","build":"mix compile","test":"mix test","check":"mix credo"}'
    return 0
  fi

  # PHP
  if [[ -f "${dir}/composer.json" ]]; then
    echo '{"type":"php","build":"","test":"./vendor/bin/phpunit","check":"./vendor/bin/phpstan analyse"}'
    return 0
  fi

  # Swift
  if [[ -f "${dir}/Package.swift" ]]; then
    echo '{"type":"swift","build":"swift build","test":"swift test","check":"swift build"}'
    return 0
  fi

  # Zig
  if [[ -f "${dir}/build.zig" ]]; then
    echo '{"type":"zig","build":"zig build","test":"zig build test","check":"zig build"}'
    return 0
  fi

  # Generic build systems (last resort before LLM)
  if [[ -f "${dir}/justfile" ]] || [[ -f "${dir}/Justfile" ]]; then
    echo '{"type":"just","build":"just build","test":"just test","check":"just lint"}'
    return 0
  fi

  if [[ -f "${dir}/Makefile" ]] || [[ -f "${dir}/makefile" ]] || [[ -f "${dir}/GNUmakefile" ]]; then
    echo '{"type":"make","build":"make build","test":"make test","check":"make lint"}'
    return 0
  fi

  return 1
}

# Gather lightweight evidence about a project for LLM analysis
# Usage: _gather_project_evidence "/path/to/project"
# Returns: evidence string on stdout
_gather_project_evidence() {
  local dir="$1"
  local evidence=""

  evidence+="Root files and directories:"$'\n'
  evidence+=$(ls -1 "${dir}" 2>/dev/null | head -50)
  evidence+=$'\n\n'

  evidence+="File extension counts:"$'\n'
  evidence+=$(find "${dir}" -type f \
    -not -path '*/.git/*' \
    -not -path '*/node_modules/*' \
    -not -path '*/target/*' \
    -not -path '*/dist/*' \
    -not -path '*/__pycache__/*' \
    -not -path '*/vendor/*' \
    -not -path '*/.build/*' \
    2>/dev/null \
    | sed 's/.*\.//' | sort | uniq -c | sort -rn | head -10)
  evidence+=$'\n\n'

  local config_patterns=("*.toml" "*.yaml" "*.yml" "*.json" "*.xml" "*.gradle" "*.gradle.kts" "*.cabal" "*.pro" "Makefile" "justfile" "Justfile" "Dockerfile" "Taskfile*" "BUILD" "WORKSPACE" "flake.nix" "shell.nix" "default.nix")
  local found_configs=0

  for pattern in "${config_patterns[@]}"; do
    if [[ ${found_configs} -ge 5 ]]; then
      break
    fi

    local files
    files=$(find "${dir}" -maxdepth 1 -name "${pattern}" -type f 2>/dev/null)

    while IFS= read -r f; do
      if [[ -n "${f}" && ${found_configs} -lt 5 ]]; then
        local basename
        basename=$(basename "${f}")
        evidence+="--- ${basename} (first 30 lines) ---"$'\n'
        evidence+=$(head -30 "${f}" 2>/dev/null)
        evidence+=$'\n\n'
        found_configs=$((found_configs + 1))
      fi
    done <<< "${files}"
  done

  echo "${evidence}"
}

# LLM-based project detection fallback
# Usage: _detect_with_llm "/path/to/project"
# Returns: JSON on stdout or empty (exit 1)
_detect_with_llm() {
  local dir="$1"
  local evidence
  evidence=$(_gather_project_evidence "${dir}")
  local prompt
  prompt=$(cat <<-EOF

		Analyze this project directory and determine the project type, primary language, and the correct shell commands to build, test, and check (lint/typecheck) it.

		EOF
	)
  prompt+=$'\n'"${evidence}"$'\n'
  prompt+=$(cat <<-EOF

		Respond with ONLY a valid JSON object. No markdown fences, no explanation, no extra text.
		The JSON must have exactly these 4 keys:
		{"type":"<language>","build":"<build command>","test":"<test command>","check":"<lint or typecheck command>"}

		Rules:
		- "type" must be a single lowercase word (e.g. rust, go, python, nodejs, java, ruby, elixir, cpp, c, zig, haskell, scala, kotlin, dart, swift, php, dotnet, etc.)
		- If a command doesn't apply to this project, use an empty string, ""
		- Use the most standard/common commands for the detected ecosystem
		- If you detect a package manager lockfile, use that package manager (e.g. pnpm over npm)
		EOF
	)

  local llm_response
  llm_response=$(loki --no-stream "${prompt}" 2>/dev/null) || return 1

  llm_response=$(echo "${llm_response}" | sed 's/^```json//;s/^```//;s/```$//' | tr -d '\n' | sed 's/^[[:space:]]*//')
  llm_response=$(echo "${llm_response}" | grep -o '{[^}]*}' | head -1)

  if echo "${llm_response}" | jq -e '.type and .build != null and .test != null and .check != null' &>/dev/null; then
    echo "${llm_response}" | jq -c '{type: (.type // "unknown"), build: (.build // ""), test: (.test // ""), check: (.check // "")}'
    return 0
  fi

  return 1
}

# Detect project type and return build/test commands
# Uses: cached result -> fast heuristics -> LLM fallback
detect_project() {
  local dir="${1:-.}"

  local cached
  if cached=$(_read_project_cache "${dir}"); then
    echo "${cached}" | jq -c '{type, build, test, check}'
    return 0
  fi

  local result
  if result=$(_detect_heuristic "${dir}"); then
    local enriched
    enriched=$(echo "${result}" | jq -c '. + {"_detected_by":"heuristic","_cached_at":"'"$(date -Iseconds)"'"}')

    _write_project_cache "${dir}" "${enriched}"

    echo "${result}"
    return 0
  fi

  if result=$(_detect_with_llm "${dir}"); then
    local enriched
    enriched=$(echo "${result}" | jq -c '. + {"_detected_by":"llm","_cached_at":"'"$(date -Iseconds)"'"}')

    _write_project_cache "${dir}" "${enriched}"

    echo "${result}"
    return 0
  fi

  echo '{"type":"unknown","build":"","test":"","check":""}'
}

###########################
## FILE SEARCH UTILITIES ##
###########################

search_files() {
  local pattern="$1"
  local dir="${2:-.}"
  
  find "${dir}" -type f -name "${pattern}" \
    -not -path '*/target/*' \
    -not -path '*/node_modules/*' \
    -not -path '*/.git/*' \
    -not -path '*/dist/*' \
    -not -path '*/__pycache__/*' \
    2>/dev/null | head -25
}

get_tree() {
  local dir="${1:-.}"
  local depth="${2:-3}"
  
  if command -v tree &>/dev/null; then
    tree -L "${depth}" --noreport -I 'node_modules|target|dist|.git|__pycache__|*.pyc' "${dir}" 2>/dev/null || find "${dir}" -maxdepth "${depth}" -type f | head -50
  else
    find "${dir}" -maxdepth "${depth}" -type f \
      -not -path '*/target/*' \
      -not -path '*/node_modules/*' \
      -not -path '*/.git/*' \
      2>/dev/null | head -50
  fi
}
