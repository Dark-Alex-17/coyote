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

# impact_facts or select_checks died, so this lap's tests were never chosen:
# the full test command is the selection and the reason says why. A crash of
# impact_facts leaves its error in last_script_error, named after the node;
# select_checks leaves its error in selection_output. The error goes on one
# capped line, so it cannot start a line that reads as a report field.
project_dir=$(gate_workdir "$state")
cmd=$(gate_test_command "$project_dir")
export SELECT_CMD="$cmd"

printf '%s' "$state" | jq -c '
  ((.last_script_error // "") | tostring) as $script
  | (if ($script | startswith("script '"'"'impact_facts'"'"'")) then $script
     else (.selection_output // "") | tostring end
     | gsub("\\s*\\n\\s*"; " ") | .[0:400]
     | if . == "" then "no error recorded in state" else . end) as $err
  | {
      "targeted": (if env.SELECT_CMD == "" then [] else [env.SELECT_CMD] end),
      "impacted": [],
      "reason": ("test selection failed (" + $err + "), so "
        + (if env.SELECT_CMD == "" then "nothing was selected: no full test command is configured or detected"
           else "the full test command runs instead" end)),
      "last_script_error": "",
      "_next": "verify_tests"
    }'
