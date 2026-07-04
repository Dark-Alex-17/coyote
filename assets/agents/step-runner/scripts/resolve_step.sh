#!/usr/bin/env bash
set -uo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

fail() {
  jq -nc --arg r "$1" '{"blocking_reason": $r, "_next": "end_failure"}'
  exit 0
}

project_dir="${LLM_AGENT_VAR_PROJECT_DIR:-.}"
project_dir=$(cd "$project_dir" 2>/dev/null && pwd) || fail "project_dir does not exist: $project_dir"

plans_dir="${LLM_AGENT_VAR_PLANS_DIR:-plans}"
[[ "$plans_dir" != /* ]] && plans_dir="$project_dir/$plans_dir"
steps_dir="$plans_dir/steps"
handoffs_dir="$plans_dir/handoffs"
notes_path="$plans_dir/NOTES.md"

[[ -d "$steps_dir" ]] || fail "No step plans directory at $steps_dir (expected <plans_dir>/steps/NN-<slug>.md)"

frontmatter() {
  awk '/^---[[:space:]]*$/{n++; next} n==1{print} n>=2{exit}' "$1"
}

fm_value() {
  echo "$1" | grep -E "^$2:" | head -1 | sed -E "s/^$2:[[:space:]]*//" | sed -E 's/^["'"'"']|["'"'"']$//g'
}

step="${LLM_AGENT_VAR_STEP:-next}"
if [[ "$step" == "next" ]]; then
  prompt_step=$(echo "$state" | jq -r '.initial_prompt // ""' | grep -oiE 'step[[:space:]#:]*[0-9]+' | head -1 | grep -oE '[0-9]+' || true)
  [[ -n "$prompt_step" ]] && step="$prompt_step"
fi

plan_file=""
if [[ "$step" == "next" ]]; then
  first_pending=""
  while IFS= read -r f; do
    st=$(fm_value "$(frontmatter "$f")" "status")
    if [[ "$st" == "in-progress" ]]; then
      plan_file="$f"
      break
    fi
    [[ -z "$first_pending" && ( "$st" == "pending" || -z "$st" ) ]] && first_pending="$f"
  done < <(find "$steps_dir" -maxdepth 1 -name '*.md' | sort)
  [[ -z "$plan_file" ]] && plan_file="$first_pending"
  [[ -z "$plan_file" ]] && fail "No in-progress or pending step plans in $steps_dir"
else
  [[ "$step" =~ ^[0-9]+$ ]] || fail "step must be a number or 'next'; got: $step"
  padded=$(printf '%02d' "$((10#$step))")
  plan_file=$(find "$steps_dir" -maxdepth 1 \( -name "${padded}-*.md" -o -name "${step}-*.md" \) | sort | head -1)
  [[ -n "$plan_file" ]] || fail "No step plan matching step $step in $steps_dir"
fi

bn=$(basename "$plan_file" .md)
num_part="${bn%%-*}"
[[ "$num_part" =~ ^[0-9]+$ ]] || fail "Step plan filename must start with a number: $bn"
step_number=$((10#$num_part))
step_slug="${bn#*-}"

fm=$(frontmatter "$plan_file")
step_title=$(fm_value "$fm" "title")
[[ -z "$step_title" ]] && step_title="$step_slug"

deps=$(echo "$fm" | awk '/^depends_on:/{f=1; print; next} f && /^[[:space:]]*-/{print; next} f{exit}' | grep -oE '[0-9]+' || true)
unsatisfied=""
for dep in $deps; do
  dep_padded=$(printf '%02d' "$((10#$dep))")
  dep_handoff=$(find "$handoffs_dir" -maxdepth 1 \( -name "${dep_padded}-*.md" -o -name "${dep}-*.md" \) 2>/dev/null | sort | head -1)
  if [[ -z "$dep_handoff" ]]; then
    unsatisfied+="- step $dep: no handoff found (step not executed?)"$'\n'
    continue
  fi
  dep_result=$(fm_value "$(frontmatter "$dep_handoff")" "result")
  if [[ "$dep_result" != "complete" ]]; then
    unsatisfied+="- step $dep: handoff result is '$dep_result' (not complete): $dep_handoff"$'\n'
  fi
done

prev_handoff_path="(none)"
prev_handoff="(none - this is the first step)"
prev_file=""
prev_num=0
while IFS= read -r h; do
  hn="${h##*/}"
  hn="${hn%%-*}"
  [[ "$hn" =~ ^[0-9]+$ ]] || continue
  n=$((10#$hn))
  if (( n < step_number && n >= prev_num )); then
    prev_num=$n
    prev_file="$h"
  fi
done < <(find "$handoffs_dir" -maxdepth 1 -name '*.md' 2>/dev/null | sort)
if [[ -n "$prev_file" ]]; then
  prev_handoff_path="$prev_file"
  prev_handoff=$(head -c 16000 "$prev_file")
fi

notes="(none)"
[[ -f "$notes_path" ]] && notes=$(head -c 8000 "$notes_path")

step_plan=$(head -c 24000 "$plan_file")
handoff_path="$handoffs_dir/$(basename "$plan_file")"

tmp=$(mktemp)
awk 'BEGIN{n=0} /^---[[:space:]]*$/{n++; print; next} n==1 && /^status:/{print "status: in-progress"; next} {print}' "$plan_file" > "$tmp" && mv "$tmp" "$plan_file"

next_node="orient"
blocking_reason=""
if [[ -n "$unsatisfied" ]]; then
  next_node="gate_blocked"
  blocking_reason="Unsatisfied dependencies:"$'\n'"$unsatisfied"
fi

jq -nc \
  --arg pd "$project_dir" \
  --arg pl "$plans_dir" \
  --argjson sn "$step_number" \
  --arg ss "$step_slug" \
  --arg st "$step_title" \
  --arg spp "$plan_file" \
  --arg sp "$step_plan" \
  --arg php "$prev_handoff_path" \
  --arg ph "$prev_handoff" \
  --arg np "$notes_path" \
  --arg no "$notes" \
  --arg hp "$handoff_path" \
  --arg br "$blocking_reason" \
  --arg nx "$next_node" \
  '{
    "project_dir": $pd,
    "plans_dir": $pl,
    "step_number": $sn,
    "step_slug": $ss,
    "step_title": $st,
    "step_plan_path": $spp,
    "step_plan": $sp,
    "prev_handoff_path": $php,
    "prev_handoff": $ph,
    "notes_path": $np,
    "notes": $no,
    "handoff_path": $hp,
    "blocking_reason": $br,
    "_next": $nx
  }'
