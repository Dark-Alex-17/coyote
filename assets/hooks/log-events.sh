#!/usr/bin/env bash
# Event-logging hook: appends a timestamped snapshot of every COYOTE_* env var
# to a log file, so you can see exactly which events fire and what context
# each one carries.
#
# Env vars read:
#   COYOTE_HOOK_LOG   log file to append to
#                     (default: ${XDG_STATE_HOME:-$HOME/.local/state}/coyote/hooks.log)
#   COYOTE_*          everything coyote sets for the event is dumped verbatim
#
# Secrets (COYOTE_SECRET_*) are excluded from the snapshot; widening the grep
# filter below opts into logging them.
#
# Wire it up under `hooks:` in your config.yaml (any event works):
#   hooks:
#     tool.completed:
#       - name: log-events
#         command: ./hooks/log-events.sh
#
# A relative command path like ./hooks/log-events.sh resolves against the
# coyote config directory for global hooks, so this works with the default
# hooks dir.
#
# See https://github.com/Dark-Alex-17/coyote/wiki/Hooks for the full list of
# events and env vars. Hooks are fire-and-forget: this script never fails
# loudly, and coyote ignores its exit code either way.

set -u

log_file="${COYOTE_HOOK_LOG:-${XDG_STATE_HOME:-$HOME/.local/state}/coyote/hooks.log}"

# COYOTE_HOOK_LOG may point into a shared directory: refuse to append through
# a symlink someone else planted there.
[ -L "$log_file" ] && exit 0
umask 077
mkdir -p "$(dirname "$log_file")" 2> /dev/null || true
{
  echo "=== $(date -u '+%Y-%m-%dT%H:%M:%SZ') ${COYOTE_EVENT:-unknown}"
  env | grep '^COYOTE_' | grep -v '^COYOTE_SECRET_' | sort
  echo
} >> "$log_file" 2> /dev/null || true
