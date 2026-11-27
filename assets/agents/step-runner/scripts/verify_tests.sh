#!/usr/bin/env bash
set -uo pipefail

# The step's finalize gate, with verify_format_lint and verify_build before
# it: a finalize record for the worktree tree whose every test exited 0 is
# reused instead of running the suite again, and a green run writes one, so
# a final-tier review of the same tree reuses it too. A record green only
# through pre-existing failures is not reused: this gate fails any suite
# that exits non-zero.

shared="$(dirname "$0")/../../.shared"
# shellcheck disable=SC1091
source "$shared/utils.sh"

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

project_dir=$(echo "$state" | jq -r '.project_dir // "."')
project_dir=$(resolve_gate_dir "$project_dir")

if [[ -n "${TEST_CMD:-}" ]]; then
  cmd="$TEST_CMD"
else
  project_info=$(detect_project "$project_dir")
  cmd=$(echo "$project_info" | jq -r '.test // ""')
fi

if [[ -z "$cmd" || "$cmd" == "null" ]]; then
  jq -nc '{
    "tests_ok": true,
    "tests_output": "(GATE NOT RUN: no test command configured or detected. This is NOT evidence that tests passed — set TEST_CMD, and never report the suite as green.)",
    "_next": "debt_scan"
  }'
  exit 0
fi

tree=$(python3 "$shared/verify_record.py" tree --root "$project_dir" 2>/dev/null) || tree=''
if [[ -n "$tree" ]]; then
  lookup=$(python3 "$shared/verify_record.py" lookup --root "$project_dir" --tier finalize 2>/dev/null) || lookup=''
  if jq -e '.status == "green"
    and ([.record.results[] | select(.role == "test")] | length > 0
      and all(.verdict == "green" and .exit == 0))' >/dev/null 2>&1 <<<"$lookup"; then
    jq -c '{
      "tests_ok": true,
      "tests_output": ("Reused the finalize record " + .path + " (tree " + .record.tree + ", recorded "
        + .record.recorded_at + " by " + .record.writer + "); the suite was not run again.\n\n"
        + ([.record.results[] | "- " + .role + " `" + .cmd + "`: " + .verdict] | join("\n"))),
      "_next": "debt_scan"
    }' <<<"$lookup"
    exit 0
  fi
fi

started=$SECONDS
exit_code=0
output=$(cd "$project_dir" && run_gate "$cmd" 2>&1) || exit_code=$?
test_cmd=$cmd
cmd=$(gate_ran "$cmd")

# write_record stores the finalize record of this green run under the tree
# taken before it, from the gate outputs already in the state.
write_record() {
  printf '%s' "$state" | RECORD_CMD=$test_cmd RECORD_SECS=$((SECONDS - started)) jq -c '
    def gate($role; $text; $ok):
      if ($text // "") | startswith("Ran: ")
      then {role: $role, cmd: ($text | ltrimstr("Ran: ") | split("\n")[0]), exit: (if $ok then 0 else 1 end),
            duration_s: 0, verdict: (if $ok then "green" else "red" end), note: "", tail: ""}
      else {role: $role, cmd: "", exit: 0, duration_s: 0, verdict: "not_run", note: "the step gate did not run it", tail: ""}
      end;
    [gate("check"; .build_output; .build_ok != false),
     gate("lint"; .lint_output; .lint_ok != false),
     {role: "test", cmd: env.RECORD_CMD, exit: 0, duration_s: (env.RECORD_SECS | tonumber), verdict: "green", note: "", tail: ""}]' |
    python3 "$shared/verify_record.py" write --root "$project_dir" --tier finalize --writer step-runner \
      --tree "$tree" >/dev/null 2>&1 ||
    echo "verify_tests.sh: the finalize record could not be written" >&2
}

# Transcript goes to jq on stdin, not as an argument — a single argv string is
# capped at 128 KiB on Linux (MAX_ARG_STRLEN) and a green `cargo test --all`
# run exceeds it. See the coder's verify_tests.sh for the failure mode.
if (( exit_code == 0 )); then
  [[ -n "$tree" ]] && write_record
  trim_output "$output" | jq -Rsc \
    --arg cmd "$cmd" \
    '{
      "tests_ok": true,
      "tests_output": ("Ran: " + $cmd + "\n\n" + .),
      "_next": "debt_scan"
    }'
else
  trim_output "$output" | jq -Rsc \
    --arg cmd "$cmd" \
    --argjson rc "$exit_code" \
    '{
      "tests_ok": false,
      "tests_output": ("Ran: " + $cmd + "\nExit code: " + ($rc | tostring) + "\n\n" + .),
      "_next": "fix_loop_gate"
    }'
fi
