#!/usr/bin/env bash
set -euo pipefail

if [[ -n "${GRAPH_STATE_FILE:-}" ]]; then
  state=$(cat "$GRAPH_STATE_FILE")
elif [[ -n "${GRAPH_STATE:-}" ]]; then
  state="$GRAPH_STATE"
else
  state='{}'
fi

findings=$(echo "$state" | jq -r '.findings // ""')

trimmed=$(printf '%s\n' "$findings" | awk '/^##+ [Ff]indings/{found=1} found{print}')

if [[ -z "$trimmed" ]]; then
  trimmed="$findings"
fi

# A regex probe, not "${trimmed//[[:space:]]/}": that expansion is quadratic
# in bash 3.2 (stock macOS) and blows the node timeout on a 10 KB report.
if [[ "$trimmed" =~ [^[:space:]] ]]; then
  next="end_success"
else
  next="end_failure"
fi

# Findings go to jq on stdin, never as an argument: a long report would
# exceed the Win32 command-line limit (32 KiB) and fail the whole node.
printf '%s' "$trimmed" | jq -Rsc \
  --arg next "$next" \
  '{"findings": ., "_next": $next}'
