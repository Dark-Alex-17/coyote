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
# here. An answer counts as a yes or a no only when every word in it belongs
# to that side (an approve or reject word plus filler such as "please" or "go
# ahead"). A condition, a change request, a negation or a mix of both sides
# is asked again rather than guessed: "yes, but rename it first" must not
# start the work, and "no problem, go ahead" must not reject it.
YES_WORDS=" yes y yeah yep approve approved ok okay lgtm proceed sure ahead good fine great "
YES_FILLER=" please thanks thank you go looks look sounds all right alright the plan this it that i do "
NO_WORDS=" no n nope reject rejected stop abort cancel decline declined "
NO_FILLER=" please thanks thank you sorry the plan this it that i "

APPROVAL_ANSWER=$(printf '%s' "$state" | jq -r '.approval_answer // "" | tostring | gsub("\\s+"; " ") | sub("^ "; "") | sub(" $"; "")')
export APPROVAL_ANSWER
# Typographic apostrophes become ASCII ones before everything but letters,
# digits and apostrophes turns into spaces.
words=$(printf '%s' "$APPROVAL_ANSWER" | sed "s/$(printf '\342\200\231')/'/g" | tr '[:upper:]' '[:lower:]' | LC_ALL=C sed -E "s/[^a-z0-9']+/ /g")

# side_of prints yes or no when every word belongs to one side and at least
# one is that side's own word, and prints nothing otherwise.
side_of() {
  local side own filler word hit
  for side in yes no; do
    if [[ $side == yes ]]; then
      own=$YES_WORDS filler=$YES_FILLER
    else
      own=$NO_WORDS filler=$NO_FILLER
    fi
    hit=0
    for word in $words; do
      case "$own" in
        *" $word "*) hit=1; continue ;;
      esac
      case "$filler" in
        *" $word "*) continue ;;
      esac
      hit=-1
      break
    done
    if (( hit == 1 )); then
      printf '%s' "$side"
      return 0
    fi
  done
}

set -f
side=$(side_of)
set +f

case "$side" in
  yes)
    jq -nc '{"_next": "baseline_tests", "approval_reask": ""}'
    ;;
  no)
    jq -nc '{"_next": "end_rejected", "approval_reask": ""}'
    ;;
  *)
    jq -nc '{
      "_next": "gate_approval",
      "approval_reask": ("Your previous answer \"" + env.APPROVAL_ANSWER + "\" was not understood. Answer exactly yes or no.")
    }'
    ;;
esac
