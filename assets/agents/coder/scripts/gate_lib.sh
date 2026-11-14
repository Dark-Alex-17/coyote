#!/usr/bin/env bash
# Helpers for the coder's baseline_tests, verify_build, verify_tests and
# fix_loop_gate scripts; sourced, never run on its own. gate_workdir,
# gate_test_command and run_gate_until call functions from .shared/utils.sh,
# so a script using them sources that file first. The rest, GATE_JQ_CAP
# included, do not: fix_loop_gate sources this file without utils.sh.

# gate_posix_path prints `path` in the form bash's own file tests and dirname
# read. A Windows drive path (C:\x or C:/x), which is what a Git Bash script
# is handed, goes through cygpath when present, else has its separators
# flipped; any other path is printed unchanged.
gate_posix_path() {
  local path=$1 drive_re='^[A-Za-z]:[\\/]'
  if [[ "$path" =~ $drive_re ]]; then
    if command -v cygpath >/dev/null 2>&1 && cygpath -u "$path"; then
      return 0
    fi
    path=${path//\\//}
  fi
  printf '%s\n' "$path"
}

# gate_tree_root prints the root of the git work tree holding directory
# `dir`, resolved through `pwd -P`, or fails when there is none. Both sides
# of every comparison in gate_workdir come from here or from `pwd -P`, so
# they share one spelling (separators, drive form, short names) whatever
# form the input path had.
gate_tree_root() {
  local top
  top=$(git -C "$1" rev-parse --show-toplevel 2>/dev/null) || return 1
  (cd "$top" && pwd -P)
}

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
  local state=$1 dir defaulted path parent next top resolved_dir dir_top
  local -a tops=()
  dir=$(printf '%s' "$state" | jq -r '.project_dir // "" | if . == "" then "." else . end')
  defaulted=$(printf '%s' "$state" | jq -r '.project_dir_defaulted // false')
  if [[ "$defaulted" == "true" ]] && command -v git >/dev/null 2>&1; then
    while IFS= read -r path; do
      path=$(gate_posix_path "$path")
      [[ "$path" == /* || "$path" =~ ^[A-Za-z]:/ ]] || continue
      parent=$path
      while [[ ! -d "$parent" ]]; do
        next=$(dirname "$parent")
        [[ "$next" != "$parent" ]] || break
        parent=$next
      done
      [[ -d "$parent" && "$parent" != "." ]] || continue
      top=$(gate_tree_root "$parent") || continue
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
      dir_top=$(gate_tree_root "$dir") || dir_top=''
      resolved_dir=$(cd "$dir" 2>/dev/null && pwd -P) || resolved_dir=$dir
      if [[ "$dir_top" != "${tops[0]}" && "$resolved_dir" != "${tops[0]}" && "$resolved_dir" != "${tops[0]}"/* ]]; then
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

# gate_test_command prints the test command for directory `dir`: the
# test_command variable, then TEST_CMD, then the detected one. Prints
# nothing when there is none.
gate_test_command() {
  local dir=$1 cmd
  if [[ -n "${LLM_AGENT_VAR_TEST_COMMAND:-}" ]]; then
    cmd="$LLM_AGENT_VAR_TEST_COMMAND"
  elif [[ -n "${TEST_CMD:-}" ]]; then
    cmd="$TEST_CMD"
  else
    cmd=$(detect_project "$dir" | jq -r '.test // ""')
  fi
  [[ "$cmd" == "null" ]] && cmd=''
  printf '%s' "$cmd"
}

# gate_test_command_for_run prints test command `cmd` as the gates run it:
# every `cargo test` or `cargo t` (also after `+toolchain`) lacking
# --no-fail-fast gets it right after the subcommand. Without it cargo stops
# at the first failing target, so the failures of later targets never show
# and cannot be compared with the baseline. Other commands are printed
# unchanged.
gate_test_command_for_run() {
  local rest=$1 out='' match after seg end_re="^[[:space:];&|)'\"]"
  local re="(^|[[:space:];&|('\"])cargo([[:space:]]+\\+[^[:space:];&|]+)?[[:space:]]+t(est)?"
  while [[ "$rest" =~ $re ]]; do
    match=${BASH_REMATCH[0]}
    out+=${rest%%"$match"*}$match
    after=${rest#*"$match"}
    rest=$after
    [[ -z "$after" || "$after" =~ $end_re ]] || continue
    seg=" ${after%%[;&|)\'\"]*} "
    seg=${seg%%" -- "*}
    [[ "$seg " == *" --no-fail-fast "* ]] || out+=" --no-fail-fast"
  done
  printf '%s' "$out$rest"
}

# GATE_JQ_CAP defines the jq filter gate_cap(sep), which joins an array of
# test ids with `sep`, keeping the first 20 and counting the rest as
# "(+K more)". Prepend it to a filter that uses gate_cap.
GATE_JQ_CAP='def gate_cap($sep): if length > 20 then (.[:20] | join($sep)) + $sep + "(+\(length - 20) more)" else join($sep) end;'

# gate_plain_text prints file `file` without carriage returns or ANSI escape
# sequences (colour, cursor and charset switches).
gate_plain_text() {
  local esc
  esc=$(printf '\033')
  tr -d '\r' <"$1" | LC_ALL=C sed -E "s/${esc}\[[0-9;?]*[A-Za-z]//g; s/${esc}[()][A-Za-z0-9]//g"
}

# gate_killed_by_signal succeeds when test output file `file` shows a cargo
# test target aborted by a signal. Its remaining tests never ran, so the
# failures it printed are not the whole story.
gate_killed_by_signal() {
  gate_plain_text "$1" | LC_ALL=C grep -qE "process didn't exit successfully: .*\(signal: [0-9]+"
}

# gate_cargo_stopped_early succeeds when test output file `file` shows cargo
# stopping at its first failing target: a rerun hint with no closing
# `N targets failed:` summary, which only --no-fail-fast prints. The later
# targets never ran, so the failures shown are not the whole story.
gate_cargo_stopped_early() {
  local text
  text=$(gate_plain_text "$1")
  LC_ALL=C grep -q '^error: test failed, to rerun pass' <<<"$text" &&
    ! LC_ALL=C grep -qE '^error: [0-9]+ targets? failed:' <<<"$text"
}

# gate_scan_tests reads test output file `file` for the unittest, pytest,
# cargo, go, jest and vitest formats. With mode `failing` it prints the ids
# of the failing tests, with `passing` those shown passing, and with `reason`
# why the failures cannot be identified (nothing when they can). The
# failures are unidentified when no id was read, or when the runner's own
# failure total exceeds the ids read: some failures were printed in a form
# this does not parse, and `failing` then prints nothing. Cargo ids carry
# their target (`<target>::<name>`) once a Running or Doc-tests header was
# seen, jest ids their file. A jest or vitest file header yields a
# file-level id only when no test of that file failed, so a new failure in
# a file that already failed is still told apart.
gate_scan_tests() {
  gate_plain_text "$2" | LC_ALL=C awk -v mode="$1" '
    function fail(id) { if (id != "" && !(id in failed)) { failed[id] = 1; nfail++ } }
    function pass(id) { if (id != "") passed[id] = 1 }
    function cargo_id(name) { return (target == "") ? name : (target "::" name) }
    function total(n) { if (n > expected) expected = n }
    function count_of(text, word,   n) {
      if (!match(text, "[0-9]+ " word)) return 0
      n = substr(text, RSTART, RLENGTH)
      return n + 0
    }
    function unittest_id(text,   name, paren, tail) {
      name = text; sub(/ .*/, "", name)
      paren = text; sub(/^[^(]*\(/, "", paren); sub(/\).*/, "", paren)
      tail = "." name
      if (length(paren) > length(tail) && substr(paren, length(paren) - length(tail) + 1) == tail) return paren
      return paren tail
    }
    {
      line = $0
      sub(/[ \t]+$/, "", line)
    }
    line ~ /^[ \t]*Running [^(]+ \(/ {
      target = line; sub(/^[ \t]*Running /, "", target)
      target = substr(target, 1, index(target, " (") - 1)
      gsub(/\\/, "/", target)
      next
    }
    line ~ /^[ \t]*Doc-tests [^ ]+$/ {
      target = line; sub(/^[ \t]*/, "", target)
      next
    }
    line ~ /^test result: FAILED\. / {
      cargo_failed += count_of(line, "failed")
      total(cargo_failed)
      next
    }
    line ~ /^test .+ \.\.\. FAILED$/ {
      id = line; sub(/^test /, "", id); sub(/ \.\.\. FAILED$/, "", id)
      fail(cargo_id(id))
      next
    }
    line ~ /^test .+ \.\.\. ok$/ {
      id = line; sub(/^test /, "", id); sub(/ \.\.\. ok$/, "", id)
      pass(cargo_id(id))
      next
    }
    line ~ /^FAILED \((failures|errors)=/ {
      n = 0
      if (match(line, /failures=[0-9]+/)) n += substr(line, RSTART + 9, RLENGTH - 9) + 0
      if (match(line, /errors=[0-9]+/)) n += substr(line, RSTART + 7, RLENGTH - 7) + 0
      total(n)
      next
    }
    line ~ /^(FAIL|ERROR): [^ ]+ \([^)]+\)/ {
      id = line; sub(/^[A-Z]+: /, "", id)
      fail(unittest_id(id))
      next
    }
    line ~ /^[^ ]+ \([^)]+\) \.\.\. ok$/ {
      pass(unittest_id(line))
      next
    }
    line ~ /^(FAILED|ERROR) [^ (]/ {
      id = line; sub(/^[A-Z]+ /, "", id)
      if (index(id, " - ") > 0) id = substr(id, 1, index(id, " - ") - 1)
      if (id ~ /::/ || id ~ /\.py$/) fail(id)
      next
    }
    line ~ /::.* PASSED( |$)/ {
      pass(substr(line, 1, index(line, " PASSED") - 1))
      next
    }
    line ~ / in [0-9.]+s( \([^)]*\))?( =+)?$/ && (line ~ /[0-9]+ failed/ || line ~ /[0-9]+ error/) {
      total(count_of(line, "failed") + count_of(line, "error"))
      next
    }
    line ~ /^[ \t]*--- FAIL: / {
      id = line; sub(/^[ \t]*--- FAIL: /, "", id); sub(/ \([^)]*\)$/, "", id)
      fail(id)
      next
    }
    line ~ /^[ \t]*--- PASS: / {
      id = line; sub(/^[ \t]*--- PASS: /, "", id); sub(/ \([^)]*\)$/, "", id)
      pass(id)
      next
    }
    line ~ /^FAIL[ \t]+[^ \t]+[ \t]+([0-9.]+s|\[build failed\]|\[setup failed\])$/ {
      id = line; sub(/^FAIL[ \t]+/, "", id); sub(/[ \t].*/, "", id)
      fail("pkg:" id)
      next
    }
    line ~ /^(Tests|Test Suites):[ \t]+[0-9]+ failed/ || line ~ /^[ \t]*(Tests|Test Files)[ \t]+[0-9]+ failed/ {
      total(count_of(line, "failed"))
      next
    }
    line ~ /^[ \t]*● / { # debris-exempt
      id = line; sub(/^[ \t]*● /, "", id) # debris-exempt
      if (id != "Console" && id != "Test suite failed to run") fail((file == "") ? id : (file " > " id))
      next
    }
    line ~ /^[ \t]*FAIL[ \t]+.+ > / {
      id = line; sub(/^[ \t]*FAIL[ \t]+/, "", id)
      fail(id)
      next
    }
    line ~ /^[ \t]+FAIL[ \t]+[^ \t]/ {
      file = line; sub(/^[ \t]+FAIL[ \t]+/, "", file)
      sub(/ \[.*\]$/, "", file); sub(/ \([0-9.]+ m?s\)$/, "", file)
      if (!(file in files)) { files[file] = 1; nfiles++; file_at[nfiles] = file }
      next
    }
    END {
      for (i = 1; i <= nfiles; i++) {
        alone = 1
        for (id in failed) if (index(id, file_at[i] " > ") == 1) { alone = 0; break }
        if (alone) fail(file_at[i])
      }
      if (mode == "reason") {
        if (nfail == 0) print "no test ids could be parsed"
        else if (expected > nfail) print "the runner reported " expected " failures but only " nfail " ids were parsed"
      } else if (mode == "failing") {
        if (expected <= nfail) for (id in failed) print id
      } else {
        for (id in passed) if (!(id in failed)) print id
      }
    }
  ' | LC_ALL=C sort -u
}

# gate_failing_tests prints the sorted unique ids of the failing tests named
# in test output file `file`, one per line, or nothing when they cannot all
# be identified (see gate_scan_tests), a test target died of a signal, or
# cargo stopped at the first failing target.
gate_failing_tests() {
  gate_killed_by_signal "$1" && return 0
  gate_cargo_stopped_early "$1" && return 0
  gate_scan_tests failing "$1"
}

# gate_passing_tests prints the sorted unique ids of the tests test output
# file `file` shows passing, in the id forms gate_failing_tests uses.
gate_passing_tests() {
  gate_scan_tests passing "$1"
}

# gate_unidentified_reason prints why the failures in test output file
# `file` cannot be identified, or nothing when gate_failing_tests names them.
gate_unidentified_reason() {
  if gate_killed_by_signal "$1"; then
    printf '%s\n' "a test target was killed by a signal"
    return 0
  fi
  if gate_cargo_stopped_early "$1"; then
    printf '%s\n' "cargo stopped at the first failing target"
    return 0
  fi
  gate_scan_tests reason "$1"
}

# gate_failure_hash prints a digest of the failure lines in test output file
# `file`, with timings, addresses and trailing blanks normalised away so two
# runs failing the same way hash alike. It prints nothing when there is no
# failure line, so two runs with nothing to compare never match.
gate_failure_hash() {
  local lines
  lines=$(gate_plain_text "$1" | { LC_ALL=C grep -iE 'fail|error|panic|assert' || true; } |
    LC_ALL=C sed -E \
      -e 's/0x[0-9A-Fa-f]+/0x?/g' \
      -e 's/[0-9]+(\.[0-9]+)?(ms|s)([^A-Za-z0-9_]|$)/?\3/g' \
      -e 's/[[:space:]]+$//' |
    LC_ALL=C sort -u)
  [[ -n "$lines" ]] || return 0
  if gate_has_sha256; then
    printf '%s' "$lines" | gate_sha256
  else
    printf '%s' "$lines" | cksum | awk '{print $1}'
  fi
}

# gate_has_sha256 succeeds when gate_sha256 has a tool to hash with.
gate_has_sha256() {
  command -v sha256sum >/dev/null 2>&1 || command -v shasum >/dev/null 2>&1
}

# gate_sha256 prints the hex sha256 digest of its stdin, through sha256sum
# or else `shasum -a 256`. Check gate_has_sha256 first.
gate_sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum | awk '{print $1}'
  else
    shasum -a 256 | awk '{print $1}'
  fi
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
