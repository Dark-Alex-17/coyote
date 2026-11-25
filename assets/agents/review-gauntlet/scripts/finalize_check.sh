#!/usr/bin/env bash
# The final tier's full-suite check, reached from signals once it has
# recorded the worktree tree the lanes will review. A finalize record for the
# worktree tree written with the same floor is reused as it stands, green or
# red; on a miss (a record that decides nothing reads as one)
# .shared/finalize_gate.sh runs fmt,
# check, lint and the full suite there, each killed after DEADLINE_SECS, with
# the baseline_floor variable as its floor, and writes one. Prints
# full_suite_record {status: green|red|unavailable|missing, tree, detail},
# where tree is the tree the record is for; verdict_gate.py compares it with
# the recorded one.
# The gate restores every file its fmt step rewrites; a gate that still
# leaves a tree other than the recorded one is reported red with the changed
# paths, since the lanes would then review files the author did not submit.
set -uo pipefail

DEADLINE_SECS=3300
shared=$(cd "$(dirname "$0")/../../.shared" && pwd)

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

project_dir=$(jq -r '.project_dir // "."' <<<"$state")
review_tree=$(jq -r '.review_tree_before // ""' <<<"$state")
floor=$(jq -r '.baseline_floor // "" | strings' <<<"$state")

# emit prints full_suite_record from status $1, tree $2 and detail $3.
emit() {
  SUITE_STATUS=$1 SUITE_TREE=$2 SUITE_DETAIL=$3 jq -nc \
    '{full_suite_record: {status: env.SUITE_STATUS, tree: env.SUITE_TREE, detail: env.SUITE_DETAIL}}'
  exit 0
}

# The results that keep a record from being green; a test that did not run
# counts, since a finalize record needs a test that did.
passing=$(python3 -c 'import json, sys; sys.path.insert(0, sys.argv[1]); import verify_record; print(json.dumps(verify_record.PASSING))' "$shared")
FAILING='[.[] | select((.verdict as $v | $passing | any(. == $v) | not) or (.role == "test" and .verdict == "not_run"))
  | "\(.role) `\(.cmd)` \(.verdict)" + (if (.note // "") != "" then " (\(.note | .[0:300]))" else "" end)] | join("; ")'

lookup=$(python3 "$shared/verify_record.py" lookup --root "$project_dir" --tier finalize 2>/dev/null) || lookup=''
floor_sha=''
if [[ -n "$floor" ]]; then
  floor_sha=$(git -C "$project_dir" rev-parse --verify -q "$floor^{commit}" 2>/dev/null) || floor_sha=$floor
fi
# A record counts only when it was written with the same floor (none for
# none): the floor decides both what fmt's rewrites belong to and which base
# pre-existing failures were judged against.
if [[ $(jq -r '.status // "missing"' <<<"$lookup" 2>/dev/null) =~ ^(green|red)$ ]] &&
  FLOOR_SHA=$floor_sha jq -e '(.record.floor // "") == env.FLOOR_SHA' >/dev/null 2>&1 <<<"$lookup"; then
  status=$(jq -r '.status' <<<"$lookup")
  tree=$(jq -r '.record.tree' <<<"$lookup")
  detail=$(jq -r --argjson passing "$passing" '"finalize record \(.record.tree) @ \(.record.recorded_at), reused"
    + ((.record.results | '"$FAILING"') as $f | if $f != "" then ": " + $f else "" end)' <<<"$lookup")
  emit "$status" "$tree" "$detail"
fi

err_file=$(mktemp)
trap 'rm -f "$err_file"' EXIT
rc=0
gate=$(TEST_TIMEOUT=$DEADLINE_SECS bash "$shared/finalize_gate.sh" "$project_dir" review-gauntlet "$floor" 2>"$err_file") || rc=$?
if (( rc != 0 )) || ! jq -e '.status | strings' >/dev/null 2>&1 <<<"$gate"; then
  emit missing "" "the finalize gate failed (exit $rc): $(tail -c 300 "$err_file" | tr '\n' ' ')"
fi
status=$(jq -r '.status' <<<"$gate")
tree=$(jq -r '.tree // ""' <<<"$gate")
detail=$(jq -r --argjson passing "$passing" '"finalize gate ran on tree \(.tree)"
  + ((.items | '"$FAILING"') as $f | if $f != "" then ": " + $f else "" end)' <<<"$gate")
if [[ -n "$review_tree" && -n "$tree" && "$tree" != "$review_tree" ]]; then
  changed=$(git -C "$project_dir" diff-tree -r --name-only --no-renames "$review_tree" "$tree" 2>/dev/null |
    paste -sd ' ' -) || changed=''
  changed=${changed:0:600}
  emit red "$tree" "$detail; the gate rewrote the reviewed tree $review_tree: ${changed:-paths unknown}; format the change and review again"
fi
emit "$status" "$tree" "$detail"
