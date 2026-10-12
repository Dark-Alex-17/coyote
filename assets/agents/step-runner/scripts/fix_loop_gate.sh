#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

fix_attempts=$(echo "$state" | jq -r '.fix_attempts // 0')
max_fix_attempts=$(echo "$state" | jq -r '.max_fix_attempts // 2')
lint_ok=$(echo "$state" | jq -r '.lint_ok | if . == null then "true" else (. | tostring) end')
build_ok=$(echo "$state" | jq -r '.build_ok | if . == null then "true" else (. | tostring) end')
tests_ok=$(echo "$state" | jq -r '.tests_ok | if . == null then "true" else (. | tostring) end')
lint_output=$(echo "$state" | jq -r '.lint_output // ""')
build_output=$(echo "$state" | jq -r '.build_output // ""')
tests_output=$(echo "$state" | jq -r '.tests_output // ""')
gate_retries=$(echo "$state" | jq -r '.gate_retries // 0')
last_script_error=$(echo "$state" | jq -r '.last_script_error // ""')

# A verify_* script crashed and the engine took its `fallback` edge, which
# carries no state_updates. The engine records every script fallback in
# last_script_error as "script '<node>' failed: ...", so match on the verify_
# prefix: a stale route_staleness crash must not turn a red run into a crash.
# All three gates still green is the fallback signal if nothing was recorded.
# Nothing in the code is known to be broken, so another implement pass cannot
# help — re-run verification once, then give up.
if [[ "$last_script_error" == "script 'verify_"* || ( "$lint_ok" == "true" && "$build_ok" == "true" && "$tests_ok" == "true" ) ]]; then
  if [[ "$last_script_error" == "script 'verify_"* ]]; then
    gate_error="$last_script_error"
  else
    gate_error="no error recorded in state"
  fi
  if (( gate_retries < 1 )); then
    jq -nc \
      --arg err "$gate_error" \
      '{
        "gate_retries": 1,
        "gate_error": $err,
        "last_script_error": "",
        "_next": "verify_format_lint"
      }'
  else
    jq -nc \
      --arg err "$gate_error" \
      '{
        "gate_error": ("verification gate crashed again after one retry: " + $err),
        "last_script_error": "",
        "_next": "end_failure"
      }'
  fi
  exit 0
fi

if (( fix_attempts >= max_fix_attempts )); then
  jq -nc \
    --argjson n "$fix_attempts" \
    '{
      "fix_attempts": $n,
      "gate_error": "",
      "_next": "end_failure"
    }'
  exit 0
fi

next_attempts=$((fix_attempts + 1))

if [[ "$lint_ok" != "true" ]]; then
  stage="lint"
  output="$lint_output"
elif [[ "$build_ok" != "true" ]]; then
  stage="build"
  output="$build_output"
else
  stage="full test suite"
  output="$tests_output"
fi

fix_instructions=$(printf '## Fix loop status (step-level attempt %d of %d)\n\nThe implementation passed the coder'"'"'s internal checks but failed step-level verification at the %s stage.\n\nOutput:\n```\n%s\n```\n\nIdentify the minimal fix and apply it. Do not refactor. Regressions in untouched code caused by this change are in scope.' \
  "$next_attempts" "$max_fix_attempts" "$stage" "$output")

# fix_instructions embeds a gate transcript; feed it to jq on stdin so it can
# never hit the 128 KiB per-argument cap (see verify_tests.sh). The flags are
# reset so a crash on the next lap is not masked by this lap's `false`, and the
# crash bookkeeping so a recovered crash is neither reported later nor denied
# its own retry on a later lap (each lap still burns a fix attempt).
printf '%s' "$fix_instructions" | jq -Rsc \
  --argjson n "$next_attempts" \
  '{
    "fix_attempts": $n,
    "fix_instructions": .,
    "lint_ok": true,
    "build_ok": true,
    "tests_ok": true,
    "last_script_error": "",
    "gate_error": "",
    "gate_retries": 0,
    "_next": "implement"
  }'
