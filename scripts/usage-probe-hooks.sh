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
sleep 1

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

exit_b=0
COYOTE_CONFIG_DIR="$CFG_B" "$BIN" --no-stream "say exactly: hi" > "$WORKDIR/out_b.txt" 2> "$WORKDIR/err_b.txt" || exit_b=$?
exit_base=0
COYOTE_CONFIG_DIR="$CFG_BASE" "$BIN" --no-stream "say exactly: hi" > "$WORKDIR/out_base.txt" 2> "$WORKDIR/err_base.txt" || exit_base=$?

[ "$exit_b" = "$exit_base" ] || fail "exit code differs: with-hooks=$exit_b baseline=$exit_base"
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
COYOTE_CONFIG_DIR="$CFG_C" "$BIN" --install-builtins hooks > "$WORKDIR/install2.out" 2>&1
grep -q "LOCALLY MODIFIED" "$CFG_C/hooks/notify.sh" \
  || fail "non-interactive reinstall overwrote the locally modified notify.sh"
[ -f "$CFG_C/hooks/log-events.sh" ] \
  || fail "reinstall did not restore the deleted log-events.sh"
grep -q '^#!/usr/bin/env bash' "$CFG_C/hooks/log-events.sh" \
  || fail "log-events.sh was not restored to the packaged script"
echo "PASS: --install-builtins hooks installs both scripts 0755; re-runs keep modified files and restore missing ones"

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
sleep 1

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
write_dryrun_config "$CFG_D" "hooks:" \
  "  agent.started:" \
  "    - name: probe-agent" \
  "      command: \"echo AGENT_STARTED >> $AGENT_LOG\"" \
  "  agent.completed:" \
  "    - name: probe-agent" \
  "      command: \"echo AGENT_DONE >> $AGENT_LOG\"" \
  "  agent.failed:" \
  "    - name: probe-agent" \
  "      command: \"echo AGENT_FAILED >> $AGENT_LOG\""

AGENT_NAME="$(ls "$REPO_ROOT/assets/agents" | head -1)"
cp -r "$REPO_ROOT/assets/agents/$AGENT_NAME" "$CFG_D/agents/$AGENT_NAME"

COYOTE_CONFIG_DIR="$CFG_D" "$BIN" --agent "$AGENT_NAME" --info > /dev/null 2>&1
sleep 1
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
exit_f=0
COYOTE_CONFIG_DIR="$CFG_F" "$BIN" --install-builtins hooks > "$WORKDIR/install_f.out" 2>&1 || exit_f=$?
[ "$exit_f" = "0" ] \
  || fail "reinstall failed on a malformed manifest (exit $exit_f): $(cat "$WORKDIR/install_f.out")"
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

echo
echo "ALL SCENARIOS PASSED"
