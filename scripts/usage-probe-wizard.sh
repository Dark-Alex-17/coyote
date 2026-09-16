#!/usr/bin/env bash
# Black-box usage-pattern probe for the first-run wizard on a real PTY.
#
# Contract under test (companion to scripts/usage-probe-inspection.sh, which
# intentionally cannot assert this because it needs a pty):
#   Plain `coyote` (no flags, no message) on a TTY with an EMPTY
#   COYOTE_CONFIG_DIR must still reach the first-run wizard — the
#   look-don't-touch bypass for inspection flags must NOT swallow the
#   interactive setup path. We assert the wizard's first prompt
#   ("No config file, create a new one?") appears within the timeout; the
#   run is then killed, so the exit code is not asserted.
#
# Usage: scripts/usage-probe-wizard.sh
# Requires: a `coyote` binary on PATH or at ./target/debug/coyote or
# ./target/release/coyote, and the `script` utility (util-linux or BSD).

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

if ! command -v script > /dev/null 2>&1; then
  echo "SKIP: 'script' utility not available; cannot allocate a pty" >&2
  exit 0
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

CFG="$WORKDIR/cfg"
mkdir -p "$CFG"
OUT="$WORKDIR/out.txt"

echo "== Plain \`coyote\` on a pty with an empty COYOTE_CONFIG_DIR reaches the wizard =="

# `script` allocates a real pty for the child, so IS_STDOUT_TERMINAL is true
# inside even in headless CI. IS_SANDBOX is stripped to exercise the plain
# host path from the recipe. The wizard blocks on its prompt forever, so a
# timeout kill (rc 124) is the expected way out.
rc=0
if script --version 2>/dev/null | grep -q util-linux; then
  env -u IS_SANDBOX COYOTE_CONFIG_DIR="$CFG" \
    timeout 15 script -qec "$BIN" /dev/null > "$OUT" 2>&1 < /dev/null || rc=$?
else
  # BSD/macOS script: command goes after the typescript file.
  env -u IS_SANDBOX COYOTE_CONFIG_DIR="$CFG" \
    timeout 15 script -q "$OUT" "$BIN" < /dev/null > /dev/null 2>&1 || rc=$?
fi

if ! grep -aq "No config file, create a new one?" "$OUT"; then
  echo "FAIL: wizard prompt never appeared (rc=$rc); output was:" >&2
  head -c 500 "$OUT" >&2
  exit 1
fi

echo "PASS: first-run wizard prompt appeared on a pty with an empty config dir"
echo
echo "ALL SCENARIOS PASSED"
