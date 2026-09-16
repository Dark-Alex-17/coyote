#!/usr/bin/env bash
# Black-box usage-pattern probe for inspection flags on a pristine config dir.
#
# Contract under test (first-run wizard bypass for look-don't-touch flags):
#   With COYOTE_CONFIG_DIR pointing at an EMPTY directory, every inspection
#   flag (--info, --list-* , --mcp-list, vault list) must:
#     1) exit without prompting (no tty read, no first-run wizard),
#     2) exit 0 (Config::default() fallback); only --info guarantees output
#        on stdout — list flags legitimately emit an empty listing on a
#        pristine dir,
#     3) write NOTHING into the config dir (no config.yaml, no builtins).
#   The plain interactive path (`coyote` on a tty, no flags) must still reach
#   the first-run wizard prompt; that is covered manually / by the REPL recipe
#   and is intentionally not asserted here (needs a pty).
#
# Usage: scripts/usage-probe-inspection.sh
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

failures=0
fail() {
  echo "FAIL: $1" >&2
  failures=$((failures + 1))
}

# Each flag gets a fresh, empty config dir. stdin is closed so any attempt to
# prompt fails fast; timeout guards against a hang instead of a read error.
probe_flag() {
  # $1 = "require-output" or "allow-empty" (stdout expectation),
  # $2... = the flag (and any args) to pass
  local output_mode="$1"; shift
  local desc="$*"
  local cfg out rc nwritten
  cfg="$(mktemp -d "$WORKDIR/cfg.XXXXXX")"
  out="$WORKDIR/out.txt"
  rc=0
  COYOTE_CONFIG_DIR="$cfg" timeout 30 "$BIN" "$@" </dev/null >"$out" 2>&1 || rc=$?

  if [ "$rc" = "124" ]; then
    fail "$desc: hung (likely waiting on a prompt) on an empty config dir"
    return
  fi
  if [ "$rc" != "0" ]; then
    fail "$desc: expected clean exit 0 on an empty config dir, got rc=$rc, output: $(head -c 200 "$out")"
  fi
  if [ "$output_mode" = "require-output" ] && [ ! -s "$out" ]; then
    fail "$desc: expected some output, got none"
  fi
  nwritten="$(find "$cfg" -mindepth 1 | wc -l | tr -d ' ')"
  if [ "$nwritten" != "0" ]; then
    fail "$desc: expected NOTHING written into the empty config dir, found $nwritten entries: $(find "$cfg" -mindepth 1 -maxdepth 1 | tr '\n' ' ')"
  fi
}

echo "== Inspection flags on a pristine COYOTE_CONFIG_DIR: no wizard, clean exit, zero writes =="
probe_flag require-output --info
probe_flag allow-empty --list-models
probe_flag allow-empty --list-roles
probe_flag allow-empty --list-sessions
probe_flag allow-empty --list-agents
probe_flag allow-empty --list-macros
probe_flag allow-empty --list-skills
probe_flag allow-empty --list-bundles
probe_flag allow-empty --list-secrets
probe_flag allow-empty --mcp-list

# --sync-models writes models-override.yaml, so it must NOT behave like an
# inspection flag: on a fresh empty config dir it takes the bootstrap path
# (builtins written) and, off a terminal, fails on the missing config.
# IS_SANDBOX is stripped: the host path is the one under test, and a leaked
# sandbox flag would reroute the missing config to the wizard prompt.
echo "== --sync-models on a pristine COYOTE_CONFIG_DIR: bootstrap path, strict failure =="
sync_cfg="$(mktemp -d "$WORKDIR/cfg.XXXXXX")"
sync_out="$WORKDIR/sync-out.txt"
sync_rc=0
env -u IS_SANDBOX -u COYOTE_PROVIDER -u COYOTE_PLATFORM COYOTE_CONFIG_DIR="$sync_cfg" \
  timeout 30 "$BIN" --sync-models </dev/null >"$sync_out" 2>&1 || sync_rc=$?
if [ "$sync_rc" = "124" ]; then
  fail "--sync-models: hung (likely waiting on a prompt) on an empty config dir"
else
  if [ "$sync_rc" = "0" ]; then
    fail "--sync-models: expected the strict missing-config failure on an empty config dir, got rc=0"
  fi
  sync_nwritten="$(find "$sync_cfg" -mindepth 1 | wc -l | tr -d ' ')"
  if [ "$sync_nwritten" = "0" ]; then
    fail "--sync-models: expected the builtins bootstrap to populate the empty config dir, found nothing"
  fi
fi

if [ "$failures" != "0" ]; then
  echo
  echo "$failures inspection-flag scenario(s) FAILED" >&2
  exit 1
fi

echo
echo "ALL SCENARIOS PASSED"
