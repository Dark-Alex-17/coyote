#!/usr/bin/env bash
set -euo pipefail

# Prepares the pinned Python Reticulum/LXMF reference for the mesh interop suite
# (src/mesh/conformance/interop.rs).
#
# Usage:
#   scripts/mesh-interop/setup.sh
#   source "${COYOTE_MESH_INTEROP_DIR:-$HOME/.cache/coyote/mesh-interop}/env.sh"
#   COYOTE_MESH_INTEROP=1 cargo test --all mesh::conformance -- --include-ignored
#
# Env:
#   COYOTE_MESH_INTEROP_DIR   Where the clones and the venv live
#                             (default: ~/.cache/coyote/mesh-interop).
#
# Idempotent: re-running fetches nothing new when the pins are already checked out and
# only re-verifies them. The reference is imported straight from the clones via
# PYTHONPATH; `rns`/`lxmf` are never pip-installed, so the pins are the only version
# in play.

DIR="${COYOTE_MESH_INTEROP_DIR:-$HOME/.cache/coyote/mesh-interop}"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"

RETICULUM_URL="https://github.com/markqvist/Reticulum"
RETICULUM_PIN="ea98db4f53dcf0defc0e71a16e60d28b1229c4e6"
LXMF_URL="https://github.com/markqvist/LXMF"
LXMF_PIN="727830cefda83d9c6e3982b48675425f3f988f9c"

log() {
  echo "[mesh-interop] $*" >&2
}

fail() {
  echo "[mesh-interop] ERROR: $*" >&2
  exit 1
}

# clone-or-fetch `$2` into `$1` and leave it detached at `$3`.
pin_checkout() {
  local dest="$1" url="$2" pin="$3"
  if [ ! -d "$dest/.git" ]; then
    log "cloning $url"
    git clone --quiet "$url" "$dest"
  fi
  if [ "$(git -C "$dest" rev-parse HEAD)" != "$pin" ]; then
    if ! git -C "$dest" cat-file -e "$pin^{commit}" 2>/dev/null; then
      log "fetching $url"
      git -C "$dest" fetch --quiet origin
    fi
    git -C "$dest" checkout --quiet --detach "$pin"
  fi
  local head
  head="$(git -C "$dest" rev-parse HEAD)"
  if [ "$head" != "$pin" ]; then
    fail "$dest is at $head, expected $pin"
  fi
  log "$dest at $pin"
}

pin_checkout "$DIR/reticulum" "$RETICULUM_URL" "$RETICULUM_PIN"
pin_checkout "$DIR/lxmf" "$LXMF_URL" "$LXMF_PIN"

PYTHON=""
if [ -x "$DIR/venv/bin/python" ] && "$DIR/venv/bin/python" -c "import cryptography, serial" 2>/dev/null; then
  PYTHON="$DIR/venv/bin/python"
else
  # A venv that exists but cannot import its packages is a failed earlier attempt.
  rm -rf "$DIR/venv"
fi
if [ -n "$PYTHON" ]; then
  :
elif command -v uv >/dev/null 2>&1; then
  log "creating venv with uv"
  uv venv --quiet "$DIR/venv"
  uv pip install --quiet --python "$DIR/venv/bin/python" cryptography pyserial
  PYTHON="$DIR/venv/bin/python"
elif python3 -m venv "$DIR/venv" 2>/dev/null; then
  log "creating venv with python3 -m venv"
  "$DIR/venv/bin/python" -m pip install --quiet cryptography pyserial
  PYTHON="$DIR/venv/bin/python"
elif python3 -c "import cryptography, serial" 2>/dev/null; then
  # interop.rs prefers `<dir>/venv/bin/python` whenever it exists, so no half-built venv may remain.
  rm -rf "$DIR/venv"
  log "WARNING: no venv could be created; using the system python3, which already has cryptography and pyserial"
  PYTHON="$(command -v python3)"
else
  rm -rf "$DIR/venv"
  fail "no venv could be created (install uv, or a python3 with ensurepip) and the system python3 lacks cryptography and/or pyserial"
fi

PYTHONPATH="$DIR/reticulum:$DIR/lxmf" "$PYTHON" - <<'PY' >&2
import RNS, LXMF
print(f"[mesh-interop] RNS {RNS.__version__} / LXMF {LXMF.__version__}")
PY

# The import above compiles only what loads eagerly; Reticulum pulls its interfaces and
# LXMF its router and stamper in lazily at start. Left cold, five references compiling the
# same modules in parallel start late enough for the suite's 15 s waits to expire on the
# first run, so compile everything now.
"$PYTHON" -m compileall -q "$DIR/reticulum/RNS" "$DIR/lxmf/LXMF" >&2

{
  printf 'export COYOTE_MESH_INTEROP_DIR=%q\n' "$DIR"
  printf 'export COYOTE_MESH_INTEROP_PYTHON=%q\n' "$PYTHON"
  printf 'export PYTHONPATH=%q\n' "$DIR/reticulum:$DIR/lxmf"
} > "$DIR/env.sh"

cat "$DIR/env.sh"
