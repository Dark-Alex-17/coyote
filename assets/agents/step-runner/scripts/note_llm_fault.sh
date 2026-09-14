#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

orient_failure=$(echo "$state" | jq -r '.orient_failure // ""')
handoff_failure=$(echo "$state" | jq -r '.handoff_failure // ""')

# Engine failure text is always prefix-anchored "LLM node" (llm.rs); on
# success these keys hold the node's structured JSON output, which never
# starts with that prefix.
if [[ "$orient_failure" == "LLM node"* ]]; then
  fault_note="PIPELINE-FAULT: orient stage failed — ${orient_failure:0:300}"
elif [[ "$handoff_failure" == "LLM node"* ]]; then
  fault_note="PIPELINE-FAULT: handoff stage failed — ${handoff_failure:0:300}"
else
  fault_note=""
fi

jq -nc --arg note "$fault_note" '{"fault_note": $note}'
