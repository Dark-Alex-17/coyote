#!/usr/bin/env bash
# Helpers for the coder's verify_build and verify_tests gates. Sourced after
# .shared/utils.sh, never run on its own.

# gate_workdir prints the directory a verify gate runs in, given the graph
# state. It is project_dir, except when resolve_paths recorded that the
# project_dir variable was left unset: script nodes inherit the engine's
# working directory, which for a spawned coder is the orchestrator's checkout,
# not the tree being edited. Then the planned files decide: when every
# absolute path in files_to_modify / files_to_create lies in one git work
# tree that does not already contain project_dir, the gates run at that
# tree's root. Anything else (relative paths, no git, several trees) keeps
# project_dir. resolve_gate_dir is applied last either way.
gate_workdir() {
  local state=$1 dir defaulted path parent top resolved_dir
  local -a tops=()
  dir=$(printf '%s' "$state" | jq -r '.project_dir // "" | if . == "" then "." else . end')
  defaulted=$(printf '%s' "$state" | jq -r '.project_dir_defaulted // false')
  if [[ "$defaulted" == "true" ]] && command -v git >/dev/null 2>&1; then
    while IFS= read -r path; do
      [[ "$path" == /* ]] || continue
      parent=$path
      while [[ ! -d "$parent" && "$parent" != "/" ]]; do
        parent=$(dirname "$parent")
      done
      top=$(git -C "$parent" rev-parse --show-toplevel 2>/dev/null) || continue
      top=$(cd "$top" && pwd -P) || continue
      if [[ ${#tops[@]} -eq 0 ]]; then
        tops=("$top")
      elif [[ "${tops[0]}" != "$top" ]]; then
        tops+=("$top")
      fi
    done < <(printf '%s' "$state" | jq -r '
      [.files_to_modify, .files_to_create]
      | map(if type == "array" then .[] else empty end)
      | .[] | strings')
    if [[ ${#tops[@]} -eq 1 ]]; then
      resolved_dir=$(cd "$dir" 2>/dev/null && pwd -P) || resolved_dir=$dir
      if [[ "$resolved_dir" != "${tops[0]}" && "$resolved_dir" != "${tops[0]}"/* ]]; then
        dir=${tops[0]}
      fi
    fi
  fi
  resolve_gate_dir "$dir"
}

# gate_deadline prints the deadline in whole seconds named by the agent
# variable `var` (LLM_AGENT_VAR_<VAR>), or `fallback` when it is unset or
# empty. Returns 1 on anything that is not a whole number; 0 means no deadline.
gate_deadline() {
  local name="LLM_AGENT_VAR_$1" fallback=$2 value
  value=${!name:-}
  [[ -n "$value" ]] || value=$fallback
  [[ "$value" =~ ^[0-9]+$ ]] || return 1
  printf '%s' "$((10#$value))"
}

# run_gate_until runs `cmd` through run_gate inside `dir`, writing its
# combined output to `out`, and kills it once `secs` seconds pass (0: no
# deadline). The command runs in its own process group so the kill reaches
# every child (a test runner's workers, a lock wrapper) instead of leaving
# them holding the lock. Returns the command's status; after a kill it sets
# GATE_TIMED_OUT=1 and returns 124.
# Usage: run_gate_until "$dir" "$cmd" "$secs" "$out" || rc=$?
run_gate_until() {
  local dir=$1 cmd=$2 secs=$3 out=$4 pid ticks=0 rc=0
  GATE_TIMED_OUT=0
  if (( secs == 0 )); then
    (cd "$dir" && run_gate "$cmd") >"$out" 2>&1 || rc=$?
    return "$rc"
  fi
  set -m
  (cd "$dir" && run_gate "$cmd") >"$out" 2>&1 &
  pid=$!
  set +m
  while kill -0 "$pid" 2>/dev/null; do
    if (( ticks >= secs * 5 )); then
      GATE_TIMED_OUT=1
      kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null
      ticks=0
      while kill -0 "$pid" 2>/dev/null && (( ticks < 10 )); do
        sleep 0.2
        ticks=$((ticks + 1))
      done
      kill -KILL -- "-$pid" 2>/dev/null || kill -KILL "$pid" 2>/dev/null
      break
    fi
    sleep 0.2
    ticks=$((ticks + 1))
  done
  wait "$pid" 2>/dev/null || rc=$?
  if (( GATE_TIMED_OUT == 1 )); then
    return 124
  fi
  return "$rc"
}
