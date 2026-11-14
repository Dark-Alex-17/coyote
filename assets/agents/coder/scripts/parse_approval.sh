#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

# gate_approval routes only the exact options itself; any other answer lands
# here. A reworded yes or no is honoured, and anything unclear is asked again
# rather than read as a rejection.
APPROVAL_ANSWER=$(printf '%s' "$state" | jq -r '.approval_answer // "" | tostring | gsub("\\s+"; " ") | sub("^ "; "") | sub(" $"; "")')
export APPROVAL_ANSWER
words=" $(printf '%s' "$APPROVAL_ANSWER" | tr '[:upper:]' '[:lower:]' | LC_ALL=C sed -E "s/[^a-z0-9']+/ /g") "
read -r first _ <<<"$words" || true

negated=0
case "$words" in
  *" not "* | *" no "* | *" don't "* | *" dont "* | *" never "* | *" cannot "* | *" can't "*) negated=1 ;;
esac

case "${first:-}" in
  yes | y | approve | approved | ok | okay | lgtm | proceed | sure | go)
    if (( negated == 0 )); then
      jq -nc '{"_next": "baseline_tests", "approval_reask": ""}'
      exit 0
    fi
    ;;
  no | n | reject | rejected | stop | abort | cancel | decline)
    jq -nc '{"_next": "end_rejected", "approval_reask": ""}'
    exit 0
    ;;
esac

jq -nc '{
  "_next": "gate_approval",
  "approval_reask": ("Your previous answer \"" + env.APPROVAL_ANSWER + "\" was not understood. Answer exactly yes or no.")
}'
