#!/usr/bin/env bash
# Black-box usage-pattern probe for the coyote hook system.
#
# Verifies, against a REAL built `coyote` binary in a clean/isolated
# COYOTE_CONFIG_DIR (dry_run:true, no network/LLM credentials required):
#   A) exactly one turn.started + one turn.completed + one llm.request.completed
#      fire per headless run, with correctly scoped env vars (no cross-event
#      extras leakage).
#   B) a failing/missing-script hook never changes coyote's stdout, stderr, or
#      exit code.
#   C) `coyote --install-builtins hooks` installs both bundled example scripts
#      0755 into <config_dir>/hooks via the real CLI; a non-interactive re-run
#      keeps locally modified scripts while still installing missing ones.
#   D) `coyote --agent <bundled-agent> --info` (an inspection flag) fires ZERO
#      agent.* hook events.
#   E) `coyote --install-builtins hooks` reconciles manifest-owned leftovers
#      in roles/hooks and each bundled agent's hooks dir, sparing user files.
#   F) top-level hooks-dir manifest reconcile: a stale shipped hook is removed
#      on reinstall; a malformed manifest deletes NOTHING (fail-safe).
#   G) a plain startup sweeps manifest-owned hooks of removed builtin agents
#      while leaving user agent dirs and user files untouched.
#   H) the INSTALLED bundled scripts behave as documented when run directly:
#      log-events.sh defaults under ${XDG_STATE_HOME:-$HOME/.local/state}/coyote/
#      (0600, UTC ISO-8601 header, secrets filtered, symlink at the default
#      path refused) and notify.sh's no-notifier/no-tty fallback strips
#      ANSI/OSC control bytes; the hooks-dir .builtin-manifest is never
#      installed executable.
#   I) `--session <name>` discriminates the session events end-to-end: a
#      fresh named session fires exactly one session.started (and no
#      session.resumed); resuming a persisted session fires exactly one
#      session.resumed (and no session.started).
#   J) pressing ctrl-c in a real terminal during a live headless --agent run
#      fires exactly one agent.interrupted with NO COYOTE_AGENT_ERROR, and
#      never agent.failed or agent.completed (needs python3 for the pty and
#      the hanging LLM endpoint; skipped when python3 is unavailable).
#
# Not covered here (see src/function/mod.rs tests for tool.* + payload-file
# coverage instead): a live tool.started firing requires a real LLM round
# trip that actually decides to call a tool; dry_run's echo_messages() path
# never invokes tools, so it cannot be exercised via this recipe.
#
# Usage: scripts/usage-probe-hooks.sh
# Requires: a `coyote` binary on PATH or at ./target/debug/coyote or
# ./target/release/coyote (build with `cargo build --bin coyote` first).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${COYOTE_BIN:-}"
if [ -z "$BIN" ]; then
  if [ -x "$REPO_ROOT/target/debug/coyote" ]; then
    BIN="$REPO_ROOT/target/debug/coyote"
  elif [ -x "$REPO_ROOT/target/release/coyote" ]; then
    BIN="$REPO_ROOT/target/release/coyote"
  else
    echo "No coyote binary found; run 'cargo build --bin coyote' first." >&2
    exit 2
  fi
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

fail() {
  echo "FAIL: $1" >&2
  exit 1
}

# Bounded poll: retries the given command every 0.1s until it succeeds or the
# ~10s deadline expires, then fails loudly. Hooks are fire-and-forget child
# processes, so every wait on their side effects goes through this helper —
# never a bare sleep.
wait_for() {
  local what="$1"
  shift
  local tries=0
  until "$@"; do
    tries=$((tries + 1))
    [ "$tries" -ge 100 ] && fail "timed out waiting for $what"
    sleep 0.1
  done
}

write_dryrun_config() {
  # $1 = config dir, $2..$n = extra YAML fragment lines appended verbatim
  local dir="$1"
  shift
  cat > "$dir/config.yaml" <<'BASE'
model: dryrun:dry-model
dry_run: true
clients:
  - type: openai
    name: dryrun
    auth: none
    api_key: 'unused'
    models:
      - name: dry-model
        max_input_tokens: 100000
        supports_function_calling: true
save: false
memory: false
stream: false
BASE
  if [ "$#" -gt 0 ]; then
    printf '%s\n' "$@" >> "$dir/config.yaml"
  fi
}

echo "== Scenario A: turn/llm hook cadence and env scoping =="
CFG_A="$WORKDIR/a"
mkdir -p "$CFG_A"
TS_LOG="$WORKDIR/turn-started.log"
TC_LOG="$WORKDIR/turn-completed.log"
LC_LOG="$WORKDIR/llm-completed.log"
write_dryrun_config "$CFG_A" "hooks:" \
  "  turn.started:" \
  "    - name: probe" \
  "      command: \"{ echo \\\"EVENT=\$COYOTE_EVENT NAME=\$COYOTE_HOOK_NAME\\\"; } >> $TS_LOG\"" \
  "  turn.completed:" \
  "    - name: probe" \
  "      command: \"{ echo \\\"EVENT=\$COYOTE_EVENT NAME=\$COYOTE_HOOK_NAME\\\"; } >> $TC_LOG\"" \
  "  llm.request.completed:" \
  "    - name: probe-llm" \
  "      command: \"{ echo \\\"EVENT=\$COYOTE_EVENT PROVIDER=\$COYOTE_LLM_PROVIDER MODEL=\$COYOTE_LLM_MODEL\\\"; } >> $LC_LOG\""

COYOTE_CONFIG_DIR="$CFG_A" "$BIN" --no-stream "say exactly: hi" > /dev/null
scenario_a_logs_ready() { [ -s "$TS_LOG" ] && [ -s "$TC_LOG" ] && [ -s "$LC_LOG" ]; }
wait_for "turn/llm hook logs" scenario_a_logs_ready

[ "$(grep -c '^EVENT=turn.started' "$TS_LOG" 2>/dev/null || echo 0)" = "1" ] \
  || fail "expected exactly one turn.started, got: $(cat "$TS_LOG" 2>/dev/null)"
[ "$(grep -c '^EVENT=turn.completed' "$TC_LOG" 2>/dev/null || echo 0)" = "1" ] \
  || fail "expected exactly one turn.completed, got: $(cat "$TC_LOG" 2>/dev/null)"
[ "$(grep -c '^EVENT=llm.request.completed' "$LC_LOG" 2>/dev/null || echo 0)" = "1" ] \
  || fail "expected exactly one llm.request.completed, got: $(cat "$LC_LOG" 2>/dev/null)"
grep -q 'PROVIDER=dryrun MODEL=dryrun:dry-model' "$LC_LOG" \
  || fail "llm.request.completed missing expected COYOTE_LLM_PROVIDER/MODEL: $(cat "$LC_LOG")"
echo "PASS: exactly 1 turn.started + 1 turn.completed + 1 llm.request.completed, correctly scoped"

echo "== Scenario B: failing/missing-script hooks never affect output or exit code =="
CFG_B="$WORKDIR/b"
mkdir -p "$CFG_B"
write_dryrun_config "$CFG_B" "hooks:" \
  "  turn.completed:" \
  "    - name: failing" \
  "      command: /nonexistent/script/does/not/exist" \
  "    - name: exit1" \
  "      command: \"exit 1\""
CFG_BASE="$WORKDIR/base"
mkdir -p "$CFG_BASE"
write_dryrun_config "$CFG_BASE"

rc_b=0
COYOTE_CONFIG_DIR="$CFG_B" "$BIN" --no-stream "say exactly: hi" > "$WORKDIR/out_b.txt" 2> "$WORKDIR/err_b.txt" || rc_b=$?
rc_base=0
COYOTE_CONFIG_DIR="$CFG_BASE" "$BIN" --no-stream "say exactly: hi" > "$WORKDIR/out_base.txt" 2> "$WORKDIR/err_base.txt" || rc_base=$?

[ "$rc_b" = "$rc_base" ] || fail "exit code differs: with-hooks=$rc_b baseline=$rc_base"
diff -q "$WORKDIR/out_b.txt" "$WORKDIR/out_base.txt" > /dev/null || fail "stdout differs when a hook fails/is missing"
diff -q "$WORKDIR/err_b.txt" "$WORKDIR/err_base.txt" > /dev/null || fail "stderr differs when a hook fails/is missing"
echo "PASS: failing/missing-script hooks are invisible to stdout/stderr/exit code"

echo "== Scenario C: coyote --install-builtins hooks (real CLI, clean config dir) =="
CFG_C="$WORKDIR/c"
mkdir -p "$CFG_C"

out_install1="$WORKDIR/install1.out"
COYOTE_CONFIG_DIR="$CFG_C" "$BIN" --install-builtins hooks > "$out_install1" 2>&1

[ -f "$CFG_C/hooks/notify.sh" ] || fail "notify.sh was not installed: $(cat "$out_install1")"
[ -f "$CFG_C/hooks/log-events.sh" ] || fail "log-events.sh was not installed: $(cat "$out_install1")"
grep -q "Reinstalled bundled hooks" "$out_install1" \
  || fail "expected confirmation message, got: $(cat "$out_install1")"

if command -v stat > /dev/null 2>&1; then
  mode_notify="$(stat -c '%a' "$CFG_C/hooks/notify.sh" 2>/dev/null || stat -f '%Lp' "$CFG_C/hooks/notify.sh")"
  mode_log="$(stat -c '%a' "$CFG_C/hooks/log-events.sh" 2>/dev/null || stat -f '%Lp' "$CFG_C/hooks/log-events.sh")"
  [ "$mode_notify" = "755" ] || fail "notify.sh mode is $mode_notify, expected 755"
  [ "$mode_log" = "755" ] || fail "log-events.sh mode is $mode_log, expected 755"
fi

# Modify one script locally and delete the other, then re-run
# --install-builtins hooks: without a terminal the conflict resolution keeps
# local files, so the edit must survive while the missing script is
# reinstalled from the packaged content.
echo "# LOCALLY MODIFIED" > "$CFG_C/hooks/notify.sh"
rm "$CFG_C/hooks/log-events.sh"
# The refresh also touches the roles and per-agent hook locations: user files
# living there must survive it untouched.
AGENT_C="$(ls "$CFG_C/agents" | head -1)"
mkdir -p "$CFG_C/roles/hooks" "$CFG_C/agents/$AGENT_C/hooks"
echo "mine-role" > "$CFG_C/roles/hooks/user-role.sh"
echo "mine-agent" > "$CFG_C/agents/$AGENT_C/hooks/user-agent.sh"
COYOTE_CONFIG_DIR="$CFG_C" "$BIN" --install-builtins hooks > "$WORKDIR/install2.out" 2>&1
grep -q "LOCALLY MODIFIED" "$CFG_C/hooks/notify.sh" \
  || fail "non-interactive reinstall overwrote the locally modified notify.sh"
[ -f "$CFG_C/hooks/log-events.sh" ] \
  || fail "reinstall did not restore the deleted log-events.sh"
grep -q '^#!/usr/bin/env bash' "$CFG_C/hooks/log-events.sh" \
  || fail "log-events.sh was not restored to the packaged script"
if command -v stat > /dev/null 2>&1; then
  mode_restored="$(stat -c '%a' "$CFG_C/hooks/log-events.sh" 2>/dev/null || stat -f '%Lp' "$CFG_C/hooks/log-events.sh")"
  [ "$mode_restored" = "755" ] || fail "restored log-events.sh mode is $mode_restored, expected 755"
fi
[ "$(cat "$CFG_C/roles/hooks/user-role.sh")" = "mine-role" ] \
  || fail "user file in roles/hooks was touched by the refresh"
[ "$(cat "$CFG_C/agents/$AGENT_C/hooks/user-agent.sh")" = "mine-agent" ] \
  || fail "user file in the agent hooks dir was touched by the refresh"
echo "PASS: --install-builtins hooks installs both scripts 0755; re-runs keep modified files (all hook locations) and restore missing ones 0755"

echo "== Scenario C2: cold-start auto-bootstrap fills the hooks dir but never forces =="
CFG_C2="$WORKDIR/c2"
mkdir -p "$CFG_C2/hooks"
write_dryrun_config "$CFG_C2"

# Every coyote invocation calls install_builtins() (force=false) before
# dispatching any flag, so a plain run on a config dir with no hooks/ yet
# must auto-populate it...
rmdir "$CFG_C2/hooks" 2>/dev/null || true
COYOTE_CONFIG_DIR="$CFG_C2" "$BIN" --no-stream "say exactly: hi" > /dev/null
[ -f "$CFG_C2/hooks/notify.sh" ] || fail "cold-start run did not auto-install notify.sh"

# ...but once a file exists, the non-force path must NEVER touch it again.
echo "# LOCALLY MODIFIED" > "$CFG_C2/hooks/notify.sh"
COYOTE_CONFIG_DIR="$CFG_C2" "$BIN" --no-stream "say exactly: hi" > /dev/null
grep -q "LOCALLY MODIFIED" "$CFG_C2/hooks/notify.sh" \
  || fail "a plain (non --install-builtins) invocation overwrote a locally modified hook script"
echo "PASS: plain invocations auto-bootstrap missing hooks but never force-overwrite existing ones"

echo "== Scenario C3: installed log-events.sh fires end-to-end via the documented relative path =="
# Exercises the exact wiring promised in the script headers:
#   command: ./hooks/log-events.sh   (resolves against the config dir)
# and the script's own contract: COYOTE_HOOK_LOG override honored, log file
# created 0600, COYOTE_SECRET_* values excluded from the snapshot.
CFG_C3="$WORKDIR/c3"
mkdir -p "$CFG_C3"
HOOK_LOG="$WORKDIR/c3-hook.log"
write_dryrun_config "$CFG_C3" "hooks:" \
  "  turn.completed:" \
  "    - name: log-events" \
  "      command: ./hooks/log-events.sh"
COYOTE_CONFIG_DIR="$CFG_C3" "$BIN" --install-builtins hooks > /dev/null 2>&1
COYOTE_CONFIG_DIR="$CFG_C3" COYOTE_HOOK_LOG="$HOOK_LOG" COYOTE_SECRET_PROBE=hunter2 \
  "$BIN" --no-stream "say exactly: hi" > /dev/null
wait_for "installed log-events.sh output" test -s "$HOOK_LOG"

[ -f "$HOOK_LOG" ] || fail "installed log-events.sh did not write to COYOTE_HOOK_LOG via relative-path wiring"
[ "$(grep -c '^=== ' "$HOOK_LOG")" = "1" ] \
  || fail "expected exactly one event block in the hook log, got: $(cat "$HOOK_LOG")"
grep -q 'COYOTE_EVENT=turn.completed' "$HOOK_LOG" \
  || fail "hook log missing COYOTE_EVENT=turn.completed: $(cat "$HOOK_LOG")"
grep -q 'hunter2' "$HOOK_LOG" \
  && fail "COYOTE_SECRET_* value leaked into the hook log"
grep -q 'COYOTE_SECRET_' "$HOOK_LOG" \
  && fail "COYOTE_SECRET_ variable name leaked into the hook log"
if command -v stat > /dev/null 2>&1; then
  mode_hook_log="$(stat -c '%a' "$HOOK_LOG" 2>/dev/null || stat -f '%Lp' "$HOOK_LOG")"
  [ "$mode_hook_log" = "600" ] || fail "hook log mode is $mode_hook_log, expected 600"
fi
echo "PASS: relative-path wiring fires the installed log-events.sh; log is 0600 with secrets excluded"

echo "== Scenario D: --agent <bundled> --info fires zero agent.* events =="
CFG_D="$WORKDIR/d"
mkdir -p "$CFG_D/agents"
AGENT_LOG="$WORKDIR/agent-events.log"
SENTINEL_LOG="$WORKDIR/d-sentinel.log"
write_dryrun_config "$CFG_D" "hooks:" \
  "  agent.started:" \
  "    - name: probe-agent" \
  "      command: \"echo AGENT_STARTED >> $AGENT_LOG\"" \
  "  agent.completed:" \
  "    - name: probe-agent" \
  "      command: \"echo AGENT_DONE >> $AGENT_LOG\"" \
  "  agent.failed:" \
  "    - name: probe-agent" \
  "      command: \"echo AGENT_FAILED >> $AGENT_LOG\"" \
  "  turn.completed:" \
  "    - name: sentinel" \
  "      command: \"echo SENTINEL >> $SENTINEL_LOG\""

AGENT_NAME="$(ls "$REPO_ROOT/assets/agents" | head -1)"
cp -r "$REPO_ROOT/assets/agents/$AGENT_NAME" "$CFG_D/agents/$AGENT_NAME"

COYOTE_CONFIG_DIR="$CFG_D" "$BIN" --agent "$AGENT_NAME" --info > /dev/null 2>&1
# Proving absence needs a positive signal to wait on: a follow-up plain run
# fires turn.completed, and its sentinel landing bounds how long the --info
# run's (nonexistent) agent.* children could have taken to write.
COYOTE_CONFIG_DIR="$CFG_D" "$BIN" --no-stream "say exactly: hi" > /dev/null
wait_for "scenario D sentinel" test -s "$SENTINEL_LOG"
[ -s "$AGENT_LOG" ] && fail "expected zero agent.* events from --agent --info, got: $(cat "$AGENT_LOG")"
echo "PASS: --agent $AGENT_NAME --info fired zero agent.* hook events"

echo "== Scenario E: --install-builtins hooks reconciles roles/hooks and agent hooks dirs =="
CFG_E="$WORKDIR/e"
AGENT_E="$(ls "$REPO_ROOT/assets/agents" | head -1)"
mkdir -p "$CFG_E/roles/hooks" "$CFG_E/agents/$AGENT_E/hooks"
# Manifest-owned leftovers a previous release shipped, plus user files that
# must survive the refresh in both locations.
printf 'stale-role.sh\n' > "$CFG_E/roles/hooks/.builtin-manifest"
echo "stale" > "$CFG_E/roles/hooks/stale-role.sh"
echo "mine" > "$CFG_E/roles/hooks/user-role.sh"
printf 'stale-agent.sh\n' > "$CFG_E/agents/$AGENT_E/hooks/.builtin-manifest"
echo "stale" > "$CFG_E/agents/$AGENT_E/hooks/stale-agent.sh"
echo "mine" > "$CFG_E/agents/$AGENT_E/hooks/user-agent.sh"

COYOTE_CONFIG_DIR="$CFG_E" "$BIN" --install-builtins hooks > "$WORKDIR/install_e.out" 2>&1 \
  || fail "--install-builtins hooks failed: $(cat "$WORKDIR/install_e.out")"

[ -f "$CFG_E/hooks/notify.sh" ] || fail "global hooks were not installed"
[ ! -f "$CFG_E/roles/hooks/stale-role.sh" ] \
  || fail "manifest-owned stale role hook survived the refresh"
[ "$(cat "$CFG_E/roles/hooks/user-role.sh")" = "mine" ] \
  || fail "user file in roles/hooks was touched by the refresh"
[ ! -f "$CFG_E/agents/$AGENT_E/hooks/stale-agent.sh" ] \
  || fail "manifest-owned stale agent hook survived the refresh"
[ "$(cat "$CFG_E/agents/$AGENT_E/hooks/user-agent.sh")" = "mine" ] \
  || fail "user file in the agent hooks dir was touched by the refresh"
echo "PASS: refresh reconciles roles/hooks and bundled-agent hooks, sparing user files"

echo "== Scenario F: top-level manifest reconcile removes stale hooks; malformed manifest deletes nothing =="
CFG_F="$WORKDIR/f"
mkdir -p "$CFG_F"
COYOTE_CONFIG_DIR="$CFG_F" "$BIN" --install-builtins hooks > /dev/null 2>&1
[ -f "$CFG_F/hooks/.builtin-manifest" ] \
  || fail "install did not write a .builtin-manifest to the top-level hooks dir"

# Simulate an upgrade from a release that also shipped old-hook.sh: the
# manifest records it, so reinstall must remove it — while a user script
# absent from the manifest survives.
printf 'log-events.sh\nnotify.sh\nold-hook.sh\n' > "$CFG_F/hooks/.builtin-manifest"
echo "stale" > "$CFG_F/hooks/old-hook.sh"
echo "mine" > "$CFG_F/hooks/user-hook.sh"
COYOTE_CONFIG_DIR="$CFG_F" "$BIN" --install-builtins hooks > /dev/null 2>&1
[ ! -f "$CFG_F/hooks/old-hook.sh" ] \
  || fail "stale shipped hook recorded in the manifest was not removed on reinstall"
[ "$(cat "$CFG_F/hooks/user-hook.sh")" = "mine" ] \
  || fail "user hook script was deleted or modified by manifest reconcile"
grep -q 'old-hook.sh' "$CFG_F/hooks/.builtin-manifest" \
  && fail "manifest still lists the removed stale hook"

# Fail-safe: a malformed (non-UTF8) manifest must delete NOTHING and must
# not fail the command.
printf '\xff\xfe\x00' > "$CFG_F/hooks/.builtin-manifest"
echo "keep" > "$CFG_F/hooks/orphan.sh"
rc_f=0
COYOTE_CONFIG_DIR="$CFG_F" "$BIN" --install-builtins hooks > "$WORKDIR/install_f.out" 2>&1 || rc_f=$?
[ "$rc_f" = "0" ] \
  || fail "reinstall failed on a malformed manifest (exit $rc_f): $(cat "$WORKDIR/install_f.out")"
[ -f "$CFG_F/hooks/orphan.sh" ] \
  || fail "a malformed manifest caused a deletion (fail-safe broken)"
[ -f "$CFG_F/hooks/notify.sh" ] && [ -f "$CFG_F/hooks/log-events.sh" ] \
  || fail "shipped hooks missing after malformed-manifest reinstall"
echo "PASS: stale manifest-owned hook removed; user files survive; malformed manifest deletes nothing"

echo "== Scenario G: plain startup sweeps removed-builtin-agent hooks, spares user agents =="
CFG_G="$WORKDIR/g"
mkdir -p "$CFG_G"
write_dryrun_config "$CFG_G"
# ghost: a removed builtin agent (has a manifest, absent from the embed)
# with a user file beside the manifest-owned hook -> hooks dir survives
# with only the user file.
mkdir -p "$CFG_G/agents/ghost/hooks"
printf 'shipped.sh\n' > "$CFG_G/agents/ghost/hooks/.builtin-manifest"
echo "stale" > "$CFG_G/agents/ghost/hooks/shipped.sh"
echo "mine" > "$CFG_G/agents/ghost/hooks/user.sh"
# ghost2: same, but nothing user-owned -> the emptied hooks dir is removed.
mkdir -p "$CFG_G/agents/ghost2/hooks"
printf 'shipped.sh\n' > "$CFG_G/agents/ghost2/hooks/.builtin-manifest"
echo "stale" > "$CFG_G/agents/ghost2/hooks/shipped.sh"
# mine: a user agent with no manifest -> never touched.
mkdir -p "$CFG_G/agents/mine/hooks"
echo "mine" > "$CFG_G/agents/mine/hooks/mine.sh"

COYOTE_CONFIG_DIR="$CFG_G" "$BIN" --no-stream "say exactly: hi" > /dev/null

[ ! -f "$CFG_G/agents/ghost/hooks/shipped.sh" ] \
  || fail "removed-agent sweep left the manifest-owned hook behind"
[ ! -f "$CFG_G/agents/ghost/hooks/.builtin-manifest" ] \
  || fail "removed-agent sweep left the manifest behind"
[ "$(cat "$CFG_G/agents/ghost/hooks/user.sh")" = "mine" ] \
  || fail "removed-agent sweep deleted a user file"
[ ! -d "$CFG_G/agents/ghost2/hooks" ] \
  || fail "emptied hooks dir of a removed agent was not removed"
[ -d "$CFG_G/agents/ghost2" ] \
  || fail "sweep removed more than the hooks dir of a removed agent"
[ "$(cat "$CFG_G/agents/mine/hooks/mine.sh")" = "mine" ] \
  || fail "sweep touched a user agent dir without a manifest"
echo "PASS: startup sweep removes only manifest-owned hooks of removed agents"

echo "== Scenario H: bundled hook scripts behave as documented when run directly =="
# The two shipped scripts are consumer-facing artifacts in their own right.
# Exercise the INSTALLED copies (real CLI install) the way a hook runner or a
# manual invocation would: clean env, no COYOTE_HOOK_LOG, no notifier
# binaries, no controlling terminal.
CFG_H="$WORKDIR/h"
mkdir -p "$CFG_H"
COYOTE_CONFIG_DIR="$CFG_H" "$BIN" --install-builtins hooks > /dev/null 2>&1
LOG_SCRIPT="$CFG_H/hooks/log-events.sh"
NOTIFY_SCRIPT="$CFG_H/hooks/notify.sh"
[ -x "$LOG_SCRIPT" ] || fail "installed log-events.sh missing or non-executable"
[ -x "$NOTIFY_SCRIPT" ] || fail "installed notify.sh missing or non-executable"

# The manifest the installer drops beside the scripts is NOT a script: the
# unified exec-bit predicate must leave it non-executable.
if [ -f "$CFG_H/hooks/.builtin-manifest" ]; then
  [ ! -x "$CFG_H/hooks/.builtin-manifest" ] \
    || fail ".builtin-manifest in the hooks dir is executable; non-script files must stay non-executable"
fi

# H1: with COYOTE_HOOK_LOG unset the log defaults under XDG_STATE_HOME, is
# created 0600, carries a UTC ISO-8601 header, and never logs COYOTE_SECRET_*.
XDG_H="$WORKDIR/h-xdg"
env -u COYOTE_HOOK_LOG XDG_STATE_HOME="$XDG_H" HOME="$WORKDIR/h-home-unused" \
  COYOTE_EVENT=probe.event COYOTE_HOOK_NAME=probe COYOTE_SECRET_TOKEN=hunter2 \
  bash "$LOG_SCRIPT" || fail "log-events.sh exited non-zero on the XDG default path"
HLOG="$XDG_H/coyote/hooks.log"
[ -f "$HLOG" ] || fail "log-events.sh did not default to \$XDG_STATE_HOME/coyote/hooks.log"
if command -v stat > /dev/null 2>&1; then
  mode_hlog="$(stat -c '%a' "$HLOG" 2>/dev/null || stat -f '%Lp' "$HLOG")"
  [ "$mode_hlog" = "600" ] || fail "default hook log mode is $mode_hlog, expected 600"
fi
grep -Eq '^=== [0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z probe\.event$' "$HLOG" \
  || fail "log header is not a UTC ISO-8601 timestamp: $(head -1 "$HLOG")"
grep -q '^COYOTE_EVENT=probe.event$' "$HLOG" \
  || fail "log snapshot is missing COYOTE_EVENT"
! grep -q 'hunter2' "$HLOG" \
  || fail "COYOTE_SECRET_* value leaked into the hook log"

# H2: with XDG_STATE_HOME also unset, the log falls back to
# $HOME/.local/state/coyote/hooks.log.
HOME_H="$WORKDIR/h-home"
mkdir -p "$HOME_H"
env -u COYOTE_HOOK_LOG -u XDG_STATE_HOME HOME="$HOME_H" COYOTE_EVENT=probe.event \
  bash "$LOG_SCRIPT" || fail "log-events.sh exited non-zero on the HOME default path"
[ -f "$HOME_H/.local/state/coyote/hooks.log" ] \
  || fail "log-events.sh did not fall back to ~/.local/state/coyote/hooks.log"

# H3: a symlink planted at the DEFAULT log path is refused — exit 0, target
# untouched (the hardening must hold on the new default location too).
HOME_S="$WORKDIR/h-sym"
mkdir -p "$HOME_S/.local/state/coyote"
VICTIM_H="$WORKDIR/h-victim.txt"
echo "untouched" > "$VICTIM_H"
ln -s "$VICTIM_H" "$HOME_S/.local/state/coyote/hooks.log"
env -u COYOTE_HOOK_LOG -u XDG_STATE_HOME HOME="$HOME_S" COYOTE_EVENT=probe.event \
  bash "$LOG_SCRIPT" || fail "log-events.sh exited non-zero when refusing a symlink"
[ "$(cat "$VICTIM_H")" = "untouched" ] \
  || fail "log-events.sh wrote through a symlink planted at the default log path"

# H4: notify.sh with no notifier on PATH and no controlling terminal falls
# back to stdout with control bytes (ESC/BEL, i.e. ANSI + OSC) stripped from
# model-influenced values.
BIN_H="$WORKDIR/h-bin"
mkdir -p "$BIN_H"
ln -s "$(command -v tr)" "$BIN_H/tr"
NOTIFY_OUT="$WORKDIR/h-notify.out"
rc_h=0
setsid -w env -i PATH="$BIN_H" \
  COYOTE_EVENT=turn.completed \
  COYOTE_TOOL_NAME="$(printf 'evil\033]0;own\007\033[31mred')" \
  "$(command -v bash)" "$NOTIFY_SCRIPT" > "$NOTIFY_OUT" 2> /dev/null || rc_h=$?
[ "$rc_h" = "0" ] || fail "notify.sh fallback exited $rc_h"
grep -Fq '[coyote] turn.completed tool=evil]0;own[31mred' "$NOTIFY_OUT" \
  || fail "notify.sh fallback output missing/unsanitized: $(cat "$NOTIFY_OUT")"
! LC_ALL=C grep -q "$(printf '\033')" "$NOTIFY_OUT" \
  || fail "ANSI/OSC escape bytes leaked through the notify.sh tty fallback"
echo "PASS: installed hook scripts honor XDG default, 0600, UTC timestamps, symlink refusal, secret filter, and ANSI-stripped notify fallback"

echo "== Scenario I: session.started vs session.resumed via --session =="
CFG_I="$WORKDIR/i"
mkdir -p "$CFG_I"
SESS_LOG="$WORKDIR/i-sess.log"
write_dryrun_config "$CFG_I" "hooks:" \
  "  session.started:" \
  "    - name: probe-sess" \
  "      command: \"echo STARTED >> $SESS_LOG\"" \
  "  session.resumed:" \
  "    - name: probe-sess" \
  "      command: \"echo RESUMED >> $SESS_LOG\""

# A fresh named session (no file on disk yet) fires session.started only.
COYOTE_CONFIG_DIR="$CFG_I" "$BIN" --session probe-fresh --no-stream "say exactly: hi" > /dev/null
wait_for "session.started log" test -s "$SESS_LOG"
[ "$(grep -c '^STARTED$' "$SESS_LOG" 2>/dev/null || echo 0)" = "1" ] \
  || fail "expected exactly one session.started for a fresh session, got: $(cat "$SESS_LOG")"
grep -q '^RESUMED$' "$SESS_LOG" \
  && fail "a fresh named session must not fire session.resumed: $(cat "$SESS_LOG")"

# A persisted session on disk: resuming it fires session.resumed, and never
# session.started (the inverse gate).
mkdir -p "$CFG_I/sessions"
printf 'model: dryrun:dry-model\nmessages: []\n' > "$CFG_I/sessions/probe-resume.yaml"
COYOTE_CONFIG_DIR="$CFG_I" "$BIN" --session probe-resume --no-stream "say exactly: hi" > /dev/null
scenario_i_resumed() { grep -q '^RESUMED$' "$SESS_LOG"; }
wait_for "session.resumed log" scenario_i_resumed
[ "$(grep -c '^RESUMED$' "$SESS_LOG" 2>/dev/null || echo 0)" = "1" ] \
  || fail "expected exactly one session.resumed for a resumed session, got: $(cat "$SESS_LOG")"
[ "$(grep -c '^STARTED$' "$SESS_LOG" 2>/dev/null || echo 0)" = "1" ] \
  || fail "resuming a session must not fire session.started: $(cat "$SESS_LOG")"
echo "PASS: fresh session fires session.started only; resume fires session.resumed only"

echo "== Scenario J: terminal ctrl-c during a live headless --agent run fires agent.interrupted =="
if ! command -v python3 > /dev/null 2>&1; then
  echo "SKIP: python3 not available; cannot allocate a pty or hanging LLM endpoint" >&2
else
  CFG_J="$WORKDIR/j"
  mkdir -p "$CFG_J/agents/probe-int"
  INT_LOG="$WORKDIR/j-int.log"

  # A local "LLM endpoint" that accepts connections and never answers keeps
  # the agent run in flight (spinner active, its ctrl-c watcher polling)
  # while the probe delivers ^C. It writes its port to $1 once listening and
  # touches $2 on the first accepted connection, so the runner below can
  # gate the ^C on the request being demonstrably in flight.
  python3 - "$WORKDIR/j-port" "$WORKDIR/j-conn" <<'PY' &
import socket, sys
s = socket.socket()
s.bind(("127.0.0.1", 0))
s.listen(8)
open(sys.argv[1], "w").write(str(s.getsockname()[1]))
conns = []
while True:
    c, _ = s.accept()
    if not conns:
        open(sys.argv[2], "w").write("1")
    conns.append(c)
PY
  SRV_J=$!
  trap 'kill "$SRV_J" 2>/dev/null || true; rm -rf "$WORKDIR"' EXIT
  wait_for "hang-server port" test -s "$WORKDIR/j-port"
  PORT_J="$(cat "$WORKDIR/j-port")"

  cat > "$CFG_J/config.yaml" <<EOF
model: hangc:hang-model
clients:
  - type: openai
    name: hangc
    auth: none
    api_key: 'unused'
    api_base: http://127.0.0.1:$PORT_J/v1
    models:
      - name: hang-model
        max_input_tokens: 100000
        supports_function_calling: true
save: false
memory: false
stream: false
EOF

  # Agent-level hooks resolve without any global_hooks whitelisting, so the
  # probe agent carries its own markers for every terminal agent event.
  cat > "$CFG_J/agents/probe-int/config.yaml" <<EOF
name: probe-int
description: ctrl-c interruption probe agent
version: 0.1.0
instructions: |
  You are a probe agent. Reply briefly.
hooks:
  agent.started:
    - name: mark
      command: "echo STARTED >> $INT_LOG"
  agent.completed:
    - name: mark
      command: "echo COMPLETED >> $INT_LOG"
  agent.failed:
    - name: mark
      command: "echo \"FAILED err=\${COYOTE_AGENT_ERROR:-unset}\" >> $INT_LOG"
  agent.interrupted:
    - name: mark
      command: "echo \"INTERRUPTED err=\${COYOTE_AGENT_ERROR:-unset}\" >> $INT_LOG"
EOF

  # Spawn coyote on a real pty (the spinner's ctrl-c watcher only runs on a
  # terminal), wait until the agent.started marker AND the first accepted
  # LLM connection prove the run is in flight, then send a literal ^C
  # through the pty line discipline — exactly what a terminal user does.
  cat > "$WORKDIR/j-pty.py" <<'PY'
import os, pty, sys, time

int_log, conn_marker = sys.argv[1], sys.argv[2]
argv = sys.argv[3:]

pid, master = pty.fork()
if pid == 0:
    os.execvp(argv[0], argv)

os.set_blocking(master, False)

def drain():
    try:
        while os.read(master, 4096):
            pass
    except OSError:
        pass

def marker_seen():
    try:
        with open(int_log) as f:
            return "STARTED" in f.read()
    except FileNotFoundError:
        return False

deadline = time.time() + 15
while time.time() < deadline:
    drain()
    if marker_seen() and os.path.exists(conn_marker):
        break
    time.sleep(0.1)
else:
    print("TIMEOUT: agent.started marker / LLM connection never appeared", flush=True)
    os.kill(pid, 9)
    os.waitpid(pid, 0)
    sys.exit(3)

os.write(master, b"\x03")  # ^C via the pty line discipline

deadline = time.time() + 15
while time.time() < deadline:
    drain()
    got, status = os.waitpid(pid, os.WNOHANG)
    if got == pid:
        if os.WIFSIGNALED(status):
            print(f"FATAL: coyote died on signal {os.WTERMSIG(status)} — no ctrl-c handler was installed", flush=True)
            sys.exit(4)
        sys.exit(0)
    time.sleep(0.1)

print("TIMEOUT: coyote did not exit within 15s of ^C", flush=True)
os.kill(pid, 9)
os.waitpid(pid, 0)
sys.exit(3)
PY

  COYOTE_CONFIG_DIR="$CFG_J" python3 "$WORKDIR/j-pty.py" "$INT_LOG" "$WORKDIR/j-conn" \
    "$BIN" --agent probe-int --no-stream "say exactly: hi" \
    || fail "pty ctrl-c run did not exit cleanly (see message above)"

  scenario_j_interrupted() { grep -qs '^INTERRUPTED err=unset$' "$INT_LOG"; }
  wait_for "agent.interrupted log" scenario_j_interrupted
  [ "$(grep -c '^STARTED$' "$INT_LOG" 2>/dev/null || echo 0)" = "1" ] \
    || fail "expected exactly one agent.started, got: $(cat "$INT_LOG")"
  [ "$(grep -c '^INTERRUPTED' "$INT_LOG" 2>/dev/null || echo 0)" = "1" ] \
    || fail "expected exactly one agent.interrupted, got: $(cat "$INT_LOG")"
  grep -q '^FAILED' "$INT_LOG" \
    && fail "ctrl-c must fire agent.interrupted, never agent.failed: $(cat "$INT_LOG")"
  grep -q '^COMPLETED' "$INT_LOG" \
    && fail "an interrupted run must not fire agent.completed: $(cat "$INT_LOG")"
  kill "$SRV_J" 2>/dev/null || true
  echo "PASS: terminal ctrl-c fires exactly one agent.interrupted with no COYOTE_AGENT_ERROR; never failed/completed"
fi

echo
echo "ALL SCENARIOS PASSED"
