#!/usr/bin/env bash
set -uo pipefail

# shellcheck disable=SC1091
source "$(dirname "$0")/../../.shared/utils.sh"
# shellcheck disable=SC1091
source "$(dirname "$0")/gate_lib.sh"

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

# emit_baseline prints the node's state update from the BASELINE_* variables,
# reading the failing test ids on stdin. Every outcome goes on to implement:
# with no red baseline, verify_tests keeps its plain exit-code verdict.
# BASELINE_EXTRA, when set, is appended to the note as its own paragraph.
emit_baseline() {
  jq -Rsc "$GATE_JQ_CAP"'
    (split("\n") | map(select(. != "")) | unique) as $ids
    | (env.BASELINE_EXIT | tonumber) as $rc
    | {
        "baseline_status": env.BASELINE_STATUS,
        "baseline_exit_code": $rc,
        "baseline_failures": $ids,
        "baseline_failure_hash": env.BASELINE_HASH,
        "baseline_reused_from": env.BASELINE_REUSED_FROM,
        "baseline_note": (
          (if env.BASELINE_STATUS != "red" then env.BASELINE_NOTE
          elif ($ids | length) > 0 then
            "These tests already failed before you changed anything (exit \($rc)):\n"
            + ($ids | gate_cap("\n"))
            + "\n\nDo not fix them unless the request asks you to, and do not write new tests that encode the behaviour a known-failing test rejects."
          else
            "The test suite already failed before you changed anything (exit \($rc)), and its failures could not be identified (\(env.BASELINE_REASON)). Do not fix them unless the request asks you to."
          end)
          + (if env.BASELINE_EXTRA == "" then "" else "\n\n" + env.BASELINE_EXTRA end)),
        "_next": "implement"
      }'
}

# store_baseline writes the BASELINE_* result, with the failing ids from file
# `ids`, to $record: a temp file in the same directory moved into place. An
# existing record is never replaced, so the first writer wins.
store_baseline() {
  local store_dir tmp
  store_dir=$(dirname "$record")
  mkdir -p "$store_dir" 2>/dev/null || return 1
  tmp=$(mktemp "$store_dir/.tests-$key.XXXXXX" 2>/dev/null) || return 1
  if ! jq -Rn '{
      "version": 1,
      "head": env.BASELINE_HEAD,
      "workdir": env.BASELINE_WORKDIR,
      "command": env.BASELINE_CMD,
      "recorded_at": env.BASELINE_RECORDED_AT,
      "status": env.BASELINE_STATUS,
      "exit_code": (env.BASELINE_EXIT | tonumber),
      "failures": ([inputs | select(. != "")] | unique),
      "failure_hash": env.BASELINE_HASH,
      "unidentified_reason": env.BASELINE_REASON
    }' <"$1" >"$tmp"; then
    rm -f "$tmp"
    return 1
  fi
  [[ -e "$record" ]] || mv -n "$tmp" "$record" 2>/dev/null
  rm -f "$tmp"
  [[ -e "$record" ]]
}

# reuse_record emits stored record $1 as this run's baseline, its four state
# fields unchanged; the caller sets BASELINE_EXTRA to say where it came from.
reuse_record() {
  BASELINE_STATUS=$(jq -r '.status' "$1")
  BASELINE_EXIT=$(jq -r '.exit_code // 0' "$1")
  BASELINE_HASH=$(jq -r '.failure_hash // ""' "$1")
  BASELINE_REASON=$(jq -r '.unidentified_reason // ""' "$1")
  BASELINE_NOTE="All tests passed before you changed anything."
  jq -r '.failures // [] | .[]' "$1" | emit_baseline
}

# valid_record succeeds when file $1 is a readable green or red record.
valid_record() {
  jq -e '.version == 1 and (.status == "green" or .status == "red")' "$1" >/dev/null 2>&1
}

export BASELINE_STATUS=none BASELINE_EXIT=0 BASELINE_HASH='' BASELINE_NOTE='' BASELINE_REASON='' BASELINE_EXTRA='' BASELINE_REUSED_FROM=''

project_dir=$(gate_workdir "$state")
cmd=$(gate_test_command_for_run "$(gate_test_command "$project_dir")")

if [[ -z "$cmd" ]]; then
  BASELINE_NOTE="No test command was configured or detected, so no baseline was recorded."
  printf '' | emit_baseline
  exit 0
fi

if ! deadline=$(gate_deadline TEST_TIMEOUT 1800); then
  BASELINE_NOTE="No baseline was recorded: the test_timeout variable is not a whole number of seconds."
  printf '' | emit_baseline
  exit 0
fi

# A baseline is only trusted from a tree known to be pristine, so it is keyed
# to the commit, directory and command it ran with and kept under the git
# dir: a resumed run on an already edited tree reuses the record taken before
# the edits instead of taking its own failures for pre-existing ones.
head=''
top=''
workdir=$(cd "$project_dir" 2>/dev/null && pwd -P) || workdir=''
if [[ -n "$workdir" ]]; then
  top=$(gate_tree_root "$workdir") &&
    head=$(git -C "$workdir" rev-parse --verify -q 'HEAD^{commit}' 2>/dev/null) || head=''
fi
if [[ -z "$head" ]]; then
  BASELINE_NOTE="The project directory is not inside a git work tree with a commit, so a pristine tree cannot be told from an edited one and no baseline was recorded."
  printf '' | emit_baseline
  exit 0
fi
if ! gate_has_sha256; then
  BASELINE_NOTE="Neither sha256sum nor shasum is available to key a stored baseline, so a pristine tree cannot be told from an edited one and no baseline was recorded."
  printf '' | emit_baseline
  exit 0
fi

if [[ "$workdir" == "$top" ]]; then
  rel_dir=.
else
  rel_dir=${workdir#"$top"/}
fi
# record_key prints the record key of commit $1 for this directory and command.
record_key() {
  printf '%s\n%s\n%s' "$1" "$rel_dir" "$cmd" | gate_sha256 | cut -c1-16
}
key=$(record_key "$head")
if ! records_dir=$(git -C "$workdir" rev-parse --git-path coyote-baselines 2>/dev/null) || [[ -z "$records_dir" ]]; then
  BASELINE_NOTE="git could not name a directory to keep the baseline in, so a pristine tree cannot be told from an edited one and no baseline was recorded."
  printf '' | emit_baseline
  exit 0
fi
records_dir=$(gate_posix_path "$records_dir")
[[ "$records_dir" == /* ]] || records_dir="$workdir/$records_dir"
record="$records_dir/tests-$key.json"
short_head=${head:0:7}

if [[ -e "$record" ]]; then
  if ! valid_record "$record"; then
    BASELINE_NOTE="The stored baseline $record cannot be read, so no baseline was recorded and every failing test counts as this run's to fix. Delete that file to take a new baseline on a clean tree."
    printf '' | emit_baseline
    exit 0
  fi
  recorded_at=$(jq -r '.recorded_at // ""' "$record")
  BASELINE_EXTRA="This baseline was recorded on HEAD $short_head before any edit ($recorded_at) and reused; the suite was not run again."
  reuse_record "$record"
  exit 0
fi

# A git status that fails prints nothing, which must not pass for a clean tree.
if ! porcelain=$(git -C "$workdir" status --porcelain --untracked-files=all 2>/dev/null); then
  BASELINE_NOTE="git status failed, so the work tree cannot be shown to be untouched and no baseline was recorded; every failing test counts as this run's to fix."
  printf '' | emit_baseline
  exit 0
fi
if [[ -n "$porcelain" ]]; then
  BASELINE_STATUS=dirty
  BASELINE_NOTE="The work tree already held uncommitted changes at the start of this run and no baseline is stored for HEAD $short_head, so no baseline was recorded and every failing test counts as this run's to fix."
  printf '' | emit_baseline
  exit 0
fi

# When the caller names the commit its run started from (baseline_floor), the
# nearest record on HEAD's first-parent chain is reused only when the floor is
# an ancestor of the record's commit, no merge commit lies between that commit
# and HEAD, and every commit in between has a committer date on or after the
# record's recorded_at. Anything else runs the suite.
floor=${LLM_AGENT_VAR_BASELINE_FLOOR:-}
if [[ -n "$floor" ]] && floor=$(git -C "$workdir" rev-parse --verify -q "$floor^{commit}" 2>/dev/null); then
  ancestor=''
  ancestor_record=''
  while IFS= read -r sha; do
    candidate="$records_dir/tests-$(record_key "$sha").json"
    if [[ -e "$candidate" ]]; then
      ancestor=$sha
      ancestor_record=$candidate
      break
    fi
  done < <(git -C "$workdir" rev-list --first-parent --skip=1 --max-count=50 HEAD 2>/dev/null)
  if [[ -n "$ancestor" ]] && valid_record "$ancestor_record" &&
    git -C "$workdir" merge-base --is-ancestor "$floor" "$ancestor" 2>/dev/null &&
    merges=$(git -C "$workdir" rev-list --merges "$ancestor..HEAD" 2>/dev/null) && [[ -z "$merges" ]]; then
    recorded_at=$(jq -r '.recorded_at // ""' "$ancestor_record")
    # Fixed-width UTC timestamps compare as strings, so the earliest committer
    # date sorts first.
    dates=$(TZ=UTC git -C "$workdir" log --format=%cd --date='format-local:%Y-%m-%dT%H:%M:%SZ' "$ancestor..HEAD" 2>/dev/null) || dates=''
    earliest=$(printf '%s\n' "$dates" | LC_ALL=C sort | head -n 1)
    if [[ "$recorded_at" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] &&
      [[ -n "$earliest" && ! "$earliest" < "$recorded_at" ]]; then
      between=$(git -C "$workdir" rev-list --reverse --abbrev-commit --abbrev=7 "$ancestor..HEAD" | paste -sd ' ' -)
      count=$(printf '%s\n' "$dates" | wc -l | tr -d ' ')
      BASELINE_EXTRA="This baseline was recorded on ${ancestor:0:7} ($recorded_at), $count commit(s) before HEAD $short_head ($between), all made after it, and reused; the suite was not run again."
      BASELINE_REUSED_FROM="${ancestor:0:7}, $count commit(s) before HEAD $short_head"
      reuse_record "$ancestor_record"
      exit 0
    fi
  fi
fi

out_file=$(mktemp)
ids_file=$(mktemp)
trap 'rm -f "$out_file" "$ids_file"' EXIT
exit_code=0
run_gate_until "$project_dir" "$cmd" "$deadline" "$out_file" || exit_code=$?

if (( GATE_TIMED_OUT == 1 )); then
  BASELINE_STATUS=timed_out
  BASELINE_NOTE="The baseline test run timed out after ${deadline} s (test_timeout), so no baseline was recorded."
  printf '' | emit_baseline
  exit 0
fi

if (( exit_code == 0 )); then
  BASELINE_STATUS=green
  BASELINE_NOTE="All tests passed before you changed anything."
else
  BASELINE_STATUS=red
  BASELINE_EXIT=$exit_code
  BASELINE_HASH=$(gate_failure_hash "$out_file")
  BASELINE_REASON=$(gate_unidentified_reason "$out_file")
  gate_failing_tests "$out_file" >"$ids_file"
fi

export BASELINE_HEAD="$head" BASELINE_WORKDIR="$workdir" BASELINE_CMD="$cmd"
BASELINE_RECORDED_AT=$(date -u +%Y-%m-%dT%H:%M:%SZ)
export BASELINE_RECORDED_AT
if ! store_baseline "$ids_file"; then
  BASELINE_EXTRA="This baseline could not be stored under $records_dir, so a later run on this commit will not reuse it."
fi
emit_baseline <"$ids_file"
