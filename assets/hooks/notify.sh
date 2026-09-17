#!/usr/bin/env bash
# Desktop-notification hook: posts a notification describing the event that
# fired, falling back to a message on the controlling terminal when no
# notifier is present.
#
# Env vars read (set by coyote when the hook fires):
#   COYOTE_EVENT        dotted event name, e.g. turn.completed
#   COYOTE_AGENT_NAME   agent name, when the event carries one
#   COYOTE_TOOL_NAME    tool name, when the event carries one
#
# Wire it up under `hooks:` in your config.yaml:
#   hooks:
#     turn.completed:
#       - name: notify
#         command: ./hooks/notify.sh
#
# A relative command path like ./hooks/notify.sh resolves against the coyote
# config directory for global hooks, so this works with the default hooks dir.
#
# See https://github.com/Dark-Alex-17/coyote/wiki/Hooks for the full list of
# events and env vars. Hooks are fire-and-forget: this script never fails
# loudly, and coyote ignores its exit code either way.

set -u

title="coyote"
body="${COYOTE_EVENT:-event}"
[ -n "${COYOTE_AGENT_NAME:-}" ] && body="$body agent=$COYOTE_AGENT_NAME"
[ -n "${COYOTE_TOOL_NAME:-}" ] && body="$body tool=$COYOTE_TOOL_NAME"

if command -v notify-send > /dev/null 2>&1; then
  notify-send "$title" "$body" 2> /dev/null || true
elif command -v osascript > /dev/null 2>&1; then
  # $body carries model-influenced values (e.g. tool names); strip quotes and
  # backslashes so it cannot escape the AppleScript string literal.
  safe_body=${body//[\"\\]/}
  osascript -e "display notification \"$safe_body\" with title \"$title\"" 2> /dev/null || true
else
  # coyote discards hook stdout, so aim the fallback at the controlling
  # terminal when one exists; plain stdout keeps direct/manual runs working.
  # $body carries model-influenced values: strip control bytes (including
  # ESC, so ANSI/OSC sequences cannot drive the terminal) before echoing.
  safe_line="$(printf '%s' "[$title] $body" | tr -d '\000-\010\013\014\016-\037\177')"
  { echo "$safe_line" > /dev/tty; } 2> /dev/null || echo "$safe_line"
fi
