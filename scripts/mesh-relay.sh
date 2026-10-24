#!/usr/bin/env bash
set -euo pipefail

# Local Reticulum daemon (rnsd) for Coyote mesh (Linux/macOS)
#
# Installs rnsd, writes the Reticulum config once, in the directory rnsd reads
# (/etc/reticulum, ~/.config/reticulum when it holds a config, else ~/.reticulum),
# and registers a user service that keeps the daemon running; every Coyote
# session on this host then dials it at 127.0.0.1:4242.
#
# Usage examples:
#   bash scripts/mesh-relay.sh
#   bash scripts/mesh-relay.sh --relay relay.example.org:4242
#   BIN_DIR="$HOME/.local/bin" bash scripts/mesh-relay.sh --no-service
#
# Flags / Env:
#   --relay <host:port>  Also dial your team's relay (TCPClientInterface).
#   --version <X>        rns version to install (default: see --help).
#   --bin-dir <dir>      Where the rnsd command goes (default: /usr/local/bin or ~/.local/bin). Or set BIN_DIR.
#   --no-service         Skip the user service (systemd --user / launchd).
#   --keep-rnsd          Keep an older rnsd that is already installed instead of upgrading it to the pinned version.
#   --dry-run            Print the plan and write nothing.
#   --allow-root         Permit running as root.
#   COYOTE_MESH_ETC_RETICULUM=<dir>  Where the system config lives (default /etc/reticulum); for the test harness.
#
# Exit codes: 0 done or nothing to do; 1 usage; 2 unsupported OS or no way to
# install rnsd; 3 service installed but it did not come up: 127.0.0.1:4242 stayed closed
# for 30 s, launchd kept the old agent loaded for 5 s, or the new service failed to start
# after the old one was stopped.

# Must track `ARG RNS_VERSION` in deployment/propagation-node/Dockerfile (the interop-verified version).
DEFAULT_RNS_SPEC="rns==1.5.2"

SERVICE_NAME="coyote-rnsd"
LAUNCHD_LABEL="com.coyote.rnsd"
LISTEN_PORT=4242
# The override exists for the test harness; operators get the documented 30 s.
READY_TIMEOUT="${COYOTE_MESH_READY_TIMEOUT:-30}"
# launchctl bootout returns before the agent is gone; the unload is polled in 0.25 s steps for this long.
LAUNCHD_UNLOAD_SECS=5
LAUNCHD_UNLOAD_STEPS=$((LAUNCHD_UNLOAD_SECS * 4))
# The override exists for the test harness, which cannot write under /etc.
ETC_RNS_DIR="${COYOTE_MESH_ETC_RETICULUM:-/etc/reticulum}"

usage() {
  echo "coyote mesh relay setup (Linux/macOS): install rnsd, write its config, run it as a user service"
  echo
  echo "Options:"
  echo "  --relay <host:port>     Also dial your team's relay (TCPClientInterface)"
  echo "  --version <X>           rns version to install (default: ${DEFAULT_RNS_SPEC#rns==})"
  echo "  --bin-dir <dir>         Where the rnsd command goes (default: /usr/local/bin or ~/.local/bin)"
  echo "  --no-service            Skip the user service"
  echo "  --keep-rnsd             Keep an older rnsd that is already installed instead of upgrading it to the pinned version"
  echo "  --dry-run               Print the plan and write nothing"
  echo "  --allow-root            Permit running as root"
  echo "  -h, --help              Show help"
}

log() {
  echo "[coyote-mesh] $*"
}

err() {
  echo "[coyote-mesh] Error: $*" >&2
}

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    err "required command '$1' not found"
    exit 2
  fi
}

TMP_FILE=""
cleanup() {
  if [[ -n "$TMP_FILE" ]]; then rm -f "$TMP_FILE"; fi
}

parse_args() {
  RELAY_HOST=""
  RELAY_PORT=""
  RNS_SPEC="$DEFAULT_RNS_SPEC"
  BIN_DIR="${BIN_DIR:-}"
  NO_SERVICE=0
  KEEP_RNSD=0
  DRY_RUN=0
  ALLOW_ROOT=0

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --relay)
        if [[ $# -lt 2 ]]; then err "--relay needs host:port"; exit 1; fi
        if [[ "$2" =~ ^([A-Za-z0-9._-]+):([0-9]{1,5})$ ]]; then
          RELAY_HOST="${BASH_REMATCH[1]}"
          RELAY_PORT="${BASH_REMATCH[2]}"
        else
          err "--relay expects host:port, got '$2'"; exit 1
        fi
        if [[ "$RELAY_PORT" -lt 1 || "$RELAY_PORT" -gt 65535 ]]; then err "--relay port out of range: $RELAY_PORT"; exit 1; fi
        shift 2;;
      --version)
        if [[ $# -lt 2 || ! "$2" =~ ^[0-9][0-9A-Za-z.]*$ ]]; then err "--version expects an rns version such as ${DEFAULT_RNS_SPEC#rns==}"; exit 1; fi
        RNS_SPEC="rns==$2"; shift 2;;
      --bin-dir)
        if [[ $# -lt 2 ]]; then err "--bin-dir needs a directory"; exit 1; fi
        BIN_DIR="$2"; shift 2;;
      --no-service) NO_SERVICE=1; shift;;
      --keep-rnsd) KEEP_RNSD=1; shift;;
      --dry-run) DRY_RUN=1; shift;;
      --allow-root) ALLOW_ROOT=1; shift;;
      -h|--help) usage; exit 0;;
      *) err "unknown argument: $1"; usage >&2; exit 1;;
    esac
  done
}

detect_os() {
  case "$(uname -s)" in
    Linux) OS=linux;;
    Darwin) OS=darwin;;
    *) err "unsupported OS '$(uname -s)'; this script covers Linux and macOS"; exit 2;;
  esac
}

refuse_root() {
  if [[ "$(id -u)" -eq 0 && "$ALLOW_ROOT" -eq 0 ]]; then
    err "running as root is a usage error: rnsd, its config and its service belong to your user. Re-run without sudo, or pass --allow-root."
    exit 1
  fi
}

resolve_paths() {
  if [[ -z "${HOME:-}" ]]; then err "HOME is not set"; exit 2; fi
  # rnsd's own order (RNS/Reticulum.py): /etc/reticulum when it holds a config, else
  # ~/.config/reticulum when it holds one, else ~/.reticulum. Reticulum does not read
  # XDG_CONFIG_HOME here, so neither does this.
  RNS_DIR_SYSTEM=0
  if [[ -f "$ETC_RNS_DIR/config" ]]; then
    RNS_DIR="$ETC_RNS_DIR"
    RNS_DIR_REASON="rnsd reads $ETC_RNS_DIR because it holds a config"
    RNS_DIR_SYSTEM=1
  elif [[ -f "$HOME/.config/reticulum/config" ]]; then
    RNS_DIR="$HOME/.config/reticulum"
    RNS_DIR_REASON="rnsd reads $HOME/.config/reticulum because it already holds a config"
  else
    RNS_DIR="$HOME/.reticulum"
    RNS_DIR_REASON="rnsd reads $HOME/.reticulum (no config under $ETC_RNS_DIR or $HOME/.config/reticulum)"
  fi
  RNS_CONFIG="$RNS_DIR/config"

  # The venv follows how coyote itself resolves its config directory.
  if [[ -n "${COYOTE_CONFIG_DIR:-}" ]]; then
    COYOTE_CONFIG="$COYOTE_CONFIG_DIR"
  elif [[ -n "${XDG_CONFIG_HOME:-}" ]]; then
    COYOTE_CONFIG="$XDG_CONFIG_HOME/coyote"
  elif [[ "$OS" == "darwin" ]]; then
    COYOTE_CONFIG="$HOME/Library/Application Support/coyote"
  else
    COYOTE_CONFIG="$HOME/.config/coyote"
  fi
  VENV_DIR="$COYOTE_CONFIG/mesh/rns-venv"

  if [[ -z "$BIN_DIR" ]]; then
    if [[ -w "/usr/local/bin" ]]; then BIN_DIR="/usr/local/bin"; else BIN_DIR="$HOME/.local/bin"; fi
  fi
  case "$BIN_DIR" in
    /*) ;;
    *) BIN_DIR="$PWD/${BIN_DIR#./}";;
  esac
  RNSD="$BIN_DIR/rnsd"

  SYSTEMD_UNIT="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$SERVICE_NAME.service"
  LAUNCHD_PLIST="$HOME/Library/LaunchAgents/$LAUNCHD_LABEL.plist"
  LAUNCHD_LOG="$HOME/Library/Logs/coyote-rnsd.log"
}

rnsd_works() {
  [[ -x "$1" ]] && "$1" --version >/dev/null 2>&1
}

python_ok() {
  command -v "$1" >/dev/null 2>&1 && "$1" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 9) else 1)' >/dev/null 2>&1
}

find_python() {
  local candidate
  for candidate in python3 python; do
    if python_ok "$candidate"; then
      PYTHON="$(command -v "$candidate")"
      return 0
    fi
  done
  PYTHON=""
}

choose_rung() {
  INSTALL_RUNG=""
  EXISTING_RNSD=""
  PYTHON=""
  if rnsd_works "$RNSD"; then
    INSTALL_RUNG="present"
    EXISTING_RNSD="$RNSD"
  elif command -v rnsd >/dev/null 2>&1 && rnsd_works "$(command -v rnsd)"; then
    INSTALL_RUNG="present"
    EXISTING_RNSD="$(command -v rnsd)"
  elif command -v uv >/dev/null 2>&1; then
    INSTALL_RUNG="uv"
  elif command -v pipx >/dev/null 2>&1; then
    INSTALL_RUNG="pipx"
  else
    find_python
    if [[ -n "$PYTHON" ]]; then
      INSTALL_RUNG="venv"
    else
      err "no way to install rnsd: need uv, pipx, or python3 >= 3.9 (e.g. 'curl -LsSf https://astral.sh/uv/install.sh | sh', or apt/brew install python3)"
      exit 2
    fi
  fi
}

# The first X.Y.Z in `$1 --version`; empty when there is none. A pre-release suffix
# (X.Y.Zrc1) is dropped, so such a build compares as its base version.
rnsd_version_of() {
  local line
  line="$("$1" --version 2>/dev/null | head -n 1 || true)"
  if [[ "$line" =~ ([0-9]+)\.([0-9]+)\.([0-9]+) ]]; then
    echo "${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.${BASH_REMATCH[3]}"
  fi
}

# older, equal or newer: the installed X.Y.Z in $1 against the pinned X.Y.Z in $2.
version_verdict() {
  local have="$1" pin="$2" h p
  while [[ -n "$have" && -n "$pin" ]]; do
    h="${have%%.*}"
    p="${pin%%.*}"
    if (( 10#$h < 10#$p )); then echo "older"; return 0; fi
    if (( 10#$h > 10#$p )); then echo "newer"; return 0; fi
    if [[ "$have" == *.* ]]; then have="${have#*.}"; else have=""; fi
    if [[ "$pin" == *.* ]]; then pin="${pin#*.}"; else pin=""; fi
  done
  echo "equal"
}

# $1 followed through symlinks; readlink without -f, which macOS before 12.3 lacks.
resolve_link() {
  local path="$1" target hops=0
  while [[ -L "$path" && "$hops" -lt 10 ]]; do
    target="$(readlink "$path")"
    case "$target" in
      /*) path="$target";;
      *) path="$(dirname "$path")/$target";;
    esac
    hops=$((hops + 1))
  done
  echo "$path"
}

# The rnsd a shim written by write_shim calls (its second line is
# `exec "<venv>/bin/rnsd" "$@"`); $1 itself when it is a symlink or anything else.
shim_target() {
  local line="" pattern='^exec "(.+/bin/rnsd)" "\$@"$'
  if [[ -f "$1" && ! -L "$1" ]]; then
    { read -r _ && read -r line; } < "$1" 2>/dev/null || true
    if [[ "$line" =~ $pattern ]]; then
      echo "${BASH_REMATCH[1]}"
      return 0
    fi
  fi
  echo "$1"
}

# uv, pipx, venv or unknown: the tool that installed the rnsd at $1, by where the
# binary really lives (through symlinks and our own shim), else by asking uv and
# pipx what they manage.
rnsd_owner() {
  local target
  target="$(resolve_link "$1")"
  target="$(shim_target "$target")"
  if [[ "$target" == */uv/tools/* ]] || [[ -n "${UV_TOOL_DIR:-}" && "$target" == "${UV_TOOL_DIR}/"* ]]; then
    echo "uv"
  elif [[ -n "${PIPX_HOME:-}" && "$target" == "${PIPX_HOME}/venvs/"* ]] || [[ "$target" == "$HOME/.local/pipx/venvs/"* || "$target" == "$HOME/.local/share/pipx/venvs/"* ]]; then
    echo "pipx"
  elif [[ "$target" == "$VENV_DIR/"* ]]; then
    echo "venv"
  elif command -v uv >/dev/null 2>&1 && uv tool list 2>/dev/null | grep '^rns ' >/dev/null; then
    echo "uv"
  elif command -v pipx >/dev/null 2>&1 && pipx list 2>/dev/null | grep 'package rns ' >/dev/null; then
    echo "pipx"
  else
    echo "unknown"
  fi
}

upgrade_command() {
  case "$RNSD_OWNER" in
    uv) echo "uv tool install --force --python 3.12 \"$RNS_SPEC\"";;
    pipx) echo "pipx install --force \"$RNS_SPEC\"";;
    venv) echo "\"$VENV_DIR/bin/python\" -m pip install --quiet \"$RNS_SPEC\"";;
  esac
}

run_upgrade() {
  case "$RNSD_OWNER" in
    uv) uv tool install --force --python 3.12 "$RNS_SPEC";;
    pipx) pipx install --force "$RNS_SPEC";;
    venv) "$VENV_DIR/bin/python" -m pip install --quiet "$RNS_SPEC";;
  esac
}

# Sets EXISTING_VERSION, VERSION_VERDICT (older, equal, newer or unknown) and, when an
# upgrade is on the table, RNSD_OWNER, so the Install: line and install_rnsd agree.
inspect_existing_rnsd() {
  EXISTING_VERSION=""
  VERSION_VERDICT="unknown"
  RNSD_OWNER=""
  PIN_VERSION="${RNS_SPEC#rns==}"
  if [[ "$INSTALL_RUNG" != "present" ]]; then return 0; fi
  EXISTING_VERSION="$(rnsd_version_of "$EXISTING_RNSD")"
  if [[ -z "$EXISTING_VERSION" ]]; then return 0; fi
  if [[ ! "$PIN_VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    log "Version: $RNS_SPEC is not X.Y.Z, so the installed rnsd $EXISTING_VERSION is kept as is"
    return 0
  fi
  VERSION_VERDICT="$(version_verdict "$EXISTING_VERSION" "$PIN_VERSION")"
  if [[ "$VERSION_VERDICT" == "older" && "$KEEP_RNSD" -eq 0 ]]; then
    RNSD_OWNER="$(rnsd_owner "$EXISTING_RNSD")"
  fi
}

describe_present() {
  case "$VERSION_VERDICT" in
    older)
      if [[ "$KEEP_RNSD" -eq 1 ]]; then
        echo "reuse $EXISTING_RNSD (already works; rnsd $EXISTING_VERSION kept by --keep-rnsd, the pin is $PIN_VERSION)"
      elif [[ "$RNSD_OWNER" == "unknown" ]]; then
        echo "reuse $EXISTING_RNSD (already works; rnsd $EXISTING_VERSION is older than the pin $PIN_VERSION and its installer is unknown; upgrade it yourself or pass --keep-rnsd)"
      else
        echo "upgrade rnsd $EXISTING_VERSION -> $PIN_VERSION via $(upgrade_command)"
      fi;;
    newer) echo "reuse $EXISTING_RNSD (already works; rnsd $EXISTING_VERSION is newer than the version this script was tested with, $PIN_VERSION)";;
    *) echo "reuse $EXISTING_RNSD (already works)";;
  esac
}

describe_rung() {
  case "$INSTALL_RUNG" in
    present) describe_present;;
    uv) echo "uv tool install --python 3.12 \"$RNS_SPEC\"";;
    pipx) echo "pipx install \"$RNS_SPEC\"";;
    venv) echo "$PYTHON -m venv \"$VENV_DIR\" && pip install \"$RNS_SPEC\"";;
  esac
}

pipx_bin_dir() {
  local dir
  dir="$(pipx environment --value PIPX_BIN_DIR 2>/dev/null || true)"
  if [[ -z "$dir" ]]; then dir="${PIPX_BIN_DIR:-$HOME/.local/bin}"; fi
  echo "$dir"
}

# A working rnsd on PATH other than BIN_DIR's own entry, as an absolute path; empty when none.
rnsd_on_path() {
  local found
  found="$(command -v rnsd 2>/dev/null || true)"
  if [[ "$found" == /* && "$found" != "$RNSD" ]] && rnsd_works "$found"; then echo "$found"; fi
}

link_rnsd() {
  local real="$1"
  if [[ "$real" == "$RNSD" ]]; then return 0; fi
  mkdir -p "$BIN_DIR"
  ln -sfn "$real" "$RNSD"
  log "Linked $RNSD -> $real"
}

write_shim() {
  mkdir -p "$BIN_DIR"
  TMP_FILE="$(mktemp "$BIN_DIR/.rnsd.XXXXXX")"
  printf '#!/bin/sh\nexec "%s/bin/rnsd" "$@"\n' "$VENV_DIR" > "$TMP_FILE"
  chmod 755 "$TMP_FILE"
  mv "$TMP_FILE" "$RNSD"
  TMP_FILE=""
  log "Wrote shim $RNSD -> $VENV_DIR/bin/rnsd"
}

# An older rnsd is upgraded through its installer, or kept with the reason; a newer
# one earns one warning.
upgrade_or_warn() {
  local after
  case "$VERSION_VERDICT" in
    older)
      if [[ "$KEEP_RNSD" -eq 1 ]]; then return 0; fi
      if [[ "$RNSD_OWNER" == "unknown" ]]; then
        log "WARNING: rnsd $EXISTING_VERSION at $EXISTING_RNSD is older than the pinned $PIN_VERSION and this script cannot tell what installed it; upgrade it yourself (for example: uv tool install --force --python 3.12 \"$RNS_SPEC\" or pipx install --force \"$RNS_SPEC\") and re-run, or pass --keep-rnsd"
        return 0
      fi
      log "Upgrading rnsd $EXISTING_VERSION -> $PIN_VERSION with $RNSD_OWNER"
      if ! run_upgrade; then err "$RNSD_OWNER upgrade failed"; exit 2; fi
      after="$(rnsd_version_of "$EXISTING_RNSD")"
      # An empty $after (no X.Y.Z at all) is left to the --version check after install.
      if [[ -n "$after" && "$(version_verdict "$after" "$PIN_VERSION")" == "older" ]]; then
        log "WARNING: rnsd still reports $after after the upgrade; check that $EXISTING_RNSD is the binary $RNSD_OWNER manages"
      fi
      ;;
    newer)
      log "WARNING: rnsd $EXISTING_VERSION is newer than the version this script was tested with ($PIN_VERSION); continuing"
      ;;
  esac
}

install_rnsd() {
  local real="" found=""
  case "$INSTALL_RUNG" in
    present)
      log "rnsd already installed: $EXISTING_RNSD"
      upgrade_or_warn
      link_rnsd "$EXISTING_RNSD"
      ;;
    uv)
      log "Installing $RNS_SPEC with uv (managed CPython 3.12)"
      if ! uv tool install --python 3.12 "$RNS_SPEC"; then err "uv tool install failed"; exit 2; fi
      real="$(uv tool dir --bin --color never)/rnsd"
      if ! rnsd_works "$real"; then
        found="$(rnsd_on_path)"
        if [[ -n "$found" ]]; then real="$found"; fi
      fi
      link_rnsd "$real"
      ;;
    pipx)
      log "Installing $RNS_SPEC with pipx"
      if ! pipx install "$RNS_SPEC"; then err "pipx install failed"; exit 2; fi
      real="$(pipx_bin_dir)/rnsd"
      if ! rnsd_works "$real"; then
        found="$(rnsd_on_path)"
        if [[ -n "$found" ]]; then real="$found"; fi
      fi
      link_rnsd "$real"
      log "Hint: 'pipx ensurepath' adds pipx's bin dir to PATH if it is not there yet"
      ;;
    venv)
      if rnsd_works "$VENV_DIR/bin/rnsd"; then
        log "Reusing venv $VENV_DIR"
      else
        # A venv that exists but cannot run rnsd is a failed earlier attempt.
        rm -rf "$VENV_DIR"
        mkdir -p "$(dirname "$VENV_DIR")"
        log "Creating venv $VENV_DIR with $PYTHON"
        if ! "$PYTHON" -m venv "$VENV_DIR"; then
          rm -rf "$VENV_DIR"
          err "could not create a venv; install your distribution's python3-venv package (Debian/Ubuntu: apt install python3-venv) or uv"
          exit 2
        fi
        log "Installing $RNS_SPEC into the venv"
        if ! "$VENV_DIR/bin/python" -m pip install --quiet "$RNS_SPEC"; then err "pip install into the venv failed"; exit 2; fi
      fi
      write_shim
      ;;
  esac

  if ! rnsd_works "$RNSD"; then
    err "$RNSD --version failed after install"
    exit 2
  fi
  log "rnsd ready: $RNSD ($("$RNSD" --version 2>/dev/null | head -n 1))"

  case ":$PATH:" in
    *":${BIN_DIR}:"*) ;;
    *)
      log "Note: ${BIN_DIR} is not in PATH. Add it, e.g.:"
      log "  export PATH=\"${BIN_DIR}:\$PATH\""
      ;;
  esac
}

interfaces_text() {
  cat <<'EOF'
[interfaces]

  # Link-local discovery of other hosts on the same LAN.
  [[Coyote Local]]
    type = AutoInterface
    enabled = Yes

  # The loopback hop every Coyote session on this host dials.
  [[Coyote Sessions]]
    type = TCPServerInterface
    enabled = Yes
    listen_ip = 127.0.0.1
    listen_port = 4242

    # Reticulum holds announces from unknown peers by default, which would delay a
    # brand-new Coyote identity's first announce. Off here, as on the repo's test relay.
    ingress_control = No
EOF
  if [[ -n "$RELAY_HOST" ]]; then
    cat <<EOF

  # Your team's relay (a propagation node or another rnsd reachable over the network).
  [[Team Relay]]
    type = TCPClientInterface
    enabled = Yes
    target_host = $RELAY_HOST
    target_port = $RELAY_PORT
EOF
  fi
}

settings_text() {
  cat <<'EOF'
[reticulum]

# Relay announces and paths between the Coyote sessions that dial in. A transport
# node also forwards traffic for any other Reticulum peer it hears on its interfaces.
enable_transport = True

# Coyote sessions announce hourly (3600 s plus a minute of slack, so the relay never counts
# a heartbeat as early): a transport node rate-limits announces per destination, Reticulum's
# defaults being announce_rate_target = 3600 s with an announce_rate_grace of 5 (RNS
# Interfaces/Interface.py, DEFAULT_AR_TARGET and DEFAULT_AR_GRACE), and past that grace it
# blocks a destination's rebroadcasts until an hour after its last accepted announce. Do not
# lower announce_rate_target here, nor default_ar_target in the [reticulum] section: team
# relays and community nodes keep the default.

# Shared instance left at the default so rnstatus, rnpath, Sideband and NomadNet on
# this host attach to this daemon instead of binding 4242 / AutoInterface again.
# Any other local account on this host can attach to this daemon too, over
# 127.0.0.1:4242 and the shared instance; that is inherent to Reticulum's shared-instance model.

[logging]

# rnsd applies its -v flags only when this section exists (Reticulum.py:459-467).
# 4 = LOG_INFO, the value RNS's own default config ships.
loglevel = 4

EOF
}

config_text() {
  cat <<'EOF'
# Reticulum configuration written by scripts/mesh-relay.sh for Coyote mesh.
# This daemon owns the interfaces; every Coyote session on this host dials it
# with mesh.interfaces: [{type: private, host: 127.0.0.1, port: 4242}].
# Interface options: https://markqvist.github.io/Reticulum/manual/interfaces.html

EOF
  settings_text
  interfaces_text
}

firewall_warning() {
  local sentence="Firewall: AutoInterface listens for LAN peers, and with enable_transport this rnsd forwards traffic for any Reticulum peer on the LAN (and on to the Team Relay when one is configured)."
  if [[ "$OS" == "darwin" ]]; then
    log "$sentence macOS will ask whether python/rnsd may accept incoming connections; allow it or discovery of LAN hosts will not work."
  else
    log "$sentence"
  fi
}

# True when the existing config holds an AutoInterface stanza that is not switched off
# with `enabled = No` before the next stanza header.
config_enables_autointerface() {
  local line in_auto=0 enabled=1
  while IFS= read -r line || [[ -n "$line" ]]; do
    if [[ "$line" =~ ^[[:space:]]*\[\[ ]]; then
      if [[ "$in_auto" -eq 1 && "$enabled" -eq 1 ]]; then return 0; fi
      in_auto=0; enabled=1
    elif [[ "$line" =~ ^[[:space:]]*type[[:space:]]*=[[:space:]]*AutoInterface ]]; then
      in_auto=1
    elif [[ "$line" =~ ^[[:space:]]*(interface_)?enabled[[:space:]]*=[[:space:]]*([Nn]o|[Ff]alse|0) ]]; then
      enabled=0
    fi
  done < "$RNS_CONFIG"
  [[ "$in_auto" -eq 1 && "$enabled" -eq 1 ]]
}

write_config() {
  if [[ -f "$RNS_CONFIG" ]]; then
    log "$RNS_CONFIG already exists, not touched. Make sure it carries these settings and stanzas:"
    echo
    settings_text
    interfaces_text
    echo
    if ! grep -qiE '^[[:space:]]*enable_transport[[:space:]]*=[[:space:]]*(true|yes)' "$RNS_CONFIG"; then
      log "WARNING: $RNS_CONFIG does not set enable_transport = True; without it this rnsd will not relay between Coyote sessions."
    fi
    if config_enables_autointerface; then firewall_warning; fi
    return 0
  fi
  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "Would write $RNS_CONFIG (mode 600):"
    echo
    config_text
    echo
    firewall_warning
    return 0
  fi
  (
    umask 077
    mkdir -p "$RNS_DIR"
  )
  TMP_FILE="$(umask 077 && mktemp "$RNS_DIR/.config.XXXXXX")"
  config_text > "$TMP_FILE"
  mv "$TMP_FILE" "$RNS_CONFIG"
  TMP_FILE=""
  log "Wrote $RNS_CONFIG"
  firewall_warning
}

firewall_pointer_darwin() {
  log "If macOS asks whether Python may accept incoming connections, allow it, or the LAN interface stays silent (AutoInterface: 'No multicast echoes received'); System Settings > Network > Firewall > Options. Nodes using only the loopback relay are unaffected."
}

systemd_unit_text() {
  cat <<EOF
[Unit]
Description=Reticulum daemon for Coyote mesh
After=network.target

[Service]
ExecStart="$RNSD" --config "$RNS_DIR"
Environment=PYTHONUNBUFFERED=1
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF
}

# sed rather than ${text//x/y}: bash 3.2 keeps quotes inside the replacement literally,
# and bash 5.2 reads an unquoted `&` there as the matched text.
xml_escape() {
  printf '%s' "$1" | sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' -e 's/"/\&quot;/g'
}

launchd_plist_text() {
  local rnsd dir log
  rnsd="$(xml_escape "$RNSD")"
  dir="$(xml_escape "$RNS_DIR")"
  log="$(xml_escape "$LAUNCHD_LOG")"
  cat <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$LAUNCHD_LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$rnsd</string>
    <string>--config</string>
    <string>$dir</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PYTHONUNBUFFERED</key>
    <string>1</string>
  </dict>
  <key>StandardOutPath</key>
  <string>$log</string>
  <key>StandardErrorPath</key>
  <string>$log</string>
</dict>
</plist>
EOF
}

# Returns 1 when $1 already holds exactly the text $2.
definition_differs() {
  local path="$1" text="$2"
  if [[ -f "$path" ]] && [[ "$(cat "$path")" == "$text" ]]; then
    return 1
  fi
  return 0
}

# Writes $2 to $1 unless the file already holds exactly that text; returns 1 when nothing changed.
write_if_changed() {
  local path="$1" text="$2"
  if ! definition_differs "$path" "$text"; then
    return 1
  fi
  mkdir -p "$(dirname "$path")"
  TMP_FILE="$(mktemp "$(dirname "$path")/.$(basename "$path").XXXXXX")"
  printf '%s\n' "$text" > "$TMP_FILE"
  chmod 644 "$TMP_FILE"
  mv "$TMP_FILE" "$path"
  TMP_FILE=""
  return 0
}

port_open() {
  (exec 3<>"/dev/tcp/127.0.0.1/$LISTEN_PORT") 2>/dev/null
}

# Before the exit 3: which config the daemon read and the tail of its log. $1 is the
# log file on macOS; on Linux the journal is the log.
readiness_diagnostics() {
  local source="$1" reported=""
  log "The service starts rnsd with --config $RNS_DIR, so it read $RNS_CONFIG"
  if [[ "$OS" == "linux" ]]; then
    if ! command -v journalctl >/dev/null 2>&1; then
      log "journalctl is not on PATH; read the service log with: $source"
      return 0
    fi
    log "Last 10 lines of $source:"
    journalctl --user -u "$SERVICE_NAME" -n 10 --no-pager || true
    reported="$(journalctl --user -u "$SERVICE_NAME" --no-pager 2>/dev/null | grep 'Configuration loaded from' | tail -n 1 || true)"
  else
    if [[ ! -f "$source" ]]; then return 0; fi
    log "Last 10 lines of $source:"
    tail -n 10 "$source"
    reported="$(grep 'Configuration loaded from' "$source" | tail -n 1 || true)"
  fi
  if [[ -n "$reported" ]]; then
    log "The daemon reported: $reported"
  else
    log "The log does not name the config file at loglevel 4"
  fi
}

wait_ready() {
  local waited=0
  log "Waiting for rnsd on 127.0.0.1:$LISTEN_PORT"
  while [[ "$waited" -lt "$READY_TIMEOUT" ]]; do
    if port_open; then
      log "rnsd is listening on 127.0.0.1:$LISTEN_PORT"
      return 0
    fi
    sleep 1
    waited=$((waited + 1))
  done
  readiness_diagnostics "$1"
  err "rnsd did not open 127.0.0.1:$LISTEN_PORT within ${READY_TIMEOUT}s. Logs: $1"
  exit 3
}

user_bus_reachable() {
  systemctl --user show-environment >/dev/null 2>&1 || systemctl --user is-system-running >/dev/null 2>&1
}

linger_hint() {
  log "Hint: 'loginctl enable-linger ${USER:-$(id -un)}' keeps it running when you are logged out"
  log "Logs: journalctl --user -u $SERVICE_NAME"
}

service_linux() {
  local unit
  unit="$(systemd_unit_text)"
  if [[ "$DRY_RUN" -eq 1 ]]; then
    local differs=0
    if definition_differs "$SYSTEMD_UNIT" "$unit"; then
      differs=1
      log "Would write $SYSTEMD_UNIT:"
    else
      log "Service: $SYSTEMD_UNIT already present with this content"
    fi
    echo
    echo "$unit"
    echo
    if user_bus_reachable; then
      log "Would run: systemctl --user daemon-reload && systemctl --user enable --now $SERVICE_NAME (unless already active)"
      if [[ "$differs" -eq 1 ]] && systemctl --user is-active --quiet "$SERVICE_NAME"; then
        log "Would run: systemctl --user daemon-reload && systemctl --user restart $SERVICE_NAME"
      fi
    else
      log "No user session bus here; would write the unit and print how to enable it"
    fi
    linger_hint
    return 0
  fi

  local changed=0
  if write_if_changed "$SYSTEMD_UNIT" "$unit"; then
    changed=1
    log "Wrote $SYSTEMD_UNIT"
  else
    log "$SYSTEMD_UNIT already present with this content"
  fi

  if ! user_bus_reachable; then
    log "No user session bus (systemctl --user cannot connect). Enable the service from a login session with:"
    log "  systemctl --user daemon-reload && systemctl --user enable --now $SERVICE_NAME"
    linger_hint
    return 0
  fi
  systemctl --user daemon-reload
  if systemctl --user is-active --quiet "$SERVICE_NAME"; then
    if [[ "$changed" -eq 1 ]]; then
      log "Restarting $SERVICE_NAME so the new unit takes effect..."
      if ! systemctl --user restart "$SERVICE_NAME"; then
        err "systemctl --user restart $SERVICE_NAME failed and the service may be stopped; check: systemctl --user status $SERVICE_NAME, then run: systemctl --user restart $SERVICE_NAME"
        exit 3
      fi
    else
      log "$SERVICE_NAME is already running; not restarted"
    fi
  else
    systemctl --user enable --now "$SERVICE_NAME"
    log "Enabled and started $SERVICE_NAME"
  fi
  linger_hint
  wait_ready "journalctl --user -u $SERVICE_NAME"
  if ! systemctl --user is-active --quiet "$SERVICE_NAME"; then
    log "WARNING: 127.0.0.1:$LISTEN_PORT is open but $SERVICE_NAME is not active; something else holds the port"
  fi
}

launchd_loaded() {
  launchctl print "gui/$(id -u)/$LAUNCHD_LABEL" >/dev/null 2>&1
}

# Bootstrapping while the old agent is still loaded fails with "Input/output error".
wait_launchd_unloaded() {
  local waited=0
  while launchd_loaded; do
    if [[ "$waited" -ge "$LAUNCHD_UNLOAD_STEPS" ]]; then
      err "launchd did not unload $LAUNCHD_LABEL within $LAUNCHD_UNLOAD_SECS s; run: launchctl bootout gui/$(id -u)/$LAUNCHD_LABEL, wait a moment, then launchctl bootstrap gui/$(id -u) \"$LAUNCHD_PLIST\""
      exit 3
    fi
    sleep 0.25
    waited=$((waited + 1))
  done
}

service_darwin() {
  local plist
  plist="$(launchd_plist_text)"
  if [[ "$DRY_RUN" -eq 1 ]]; then
    local differs=0
    if definition_differs "$LAUNCHD_PLIST" "$plist"; then
      differs=1
      log "Would write $LAUNCHD_PLIST:"
    else
      log "Service: $LAUNCHD_PLIST already present with this content"
    fi
    echo
    echo "$plist"
    echo
    if launchd_loaded; then
      if [[ "$differs" -eq 1 ]]; then
        log "Would run: launchctl bootout gui/$(id -u)/$LAUNCHD_LABEL"
        log "Would wait for launchd to unload $LAUNCHD_LABEL (up to $LAUNCHD_UNLOAD_SECS s)"
        log "Would run: launchctl bootstrap gui/$(id -u) \"$LAUNCHD_PLIST\""
      else
        log "$LAUNCHD_LABEL is already loaded; nothing to do"
      fi
    else
      log "Would run: launchctl bootstrap gui/$(id -u) \"$LAUNCHD_PLIST\""
    fi
    log "Logs: $LAUNCHD_LOG"
    return 0
  fi

  local changed=0
  if write_if_changed "$LAUNCHD_PLIST" "$plist"; then
    changed=1
    log "Wrote $LAUNCHD_PLIST"
  else
    log "$LAUNCHD_PLIST already present with this content"
  fi
  if command -v plutil >/dev/null 2>&1; then plutil -lint "$LAUNCHD_PLIST"; fi
  mkdir -p "$(dirname "$LAUNCHD_LOG")"

  if launchd_loaded; then
    if [[ "$changed" -eq 1 ]]; then
      log "Restarting the rnsd agent so the new plist takes effect (launchctl bootout, then bootstrap)..."
      # bootout answers "36: Operation now in progress" while the agent is still exiting; the poll below is the real check.
      launchctl bootout "gui/$(id -u)/$LAUNCHD_LABEL" || true
      wait_launchd_unloaded
      if ! launchctl bootstrap "gui/$(id -u)" "$LAUNCHD_PLIST"; then
        err "launchctl bootstrap failed and the rnsd agent is not loaded; run: launchctl bootstrap gui/$(id -u) \"$LAUNCHD_PLIST\""
        exit 3
      fi
      log "Bootstrapped $LAUNCHD_LABEL"
    else
      log "$LAUNCHD_LABEL is already loaded; not restarted"
    fi
  else
    launchctl bootstrap "gui/$(id -u)" "$LAUNCHD_PLIST"
    log "Bootstrapped $LAUNCHD_LABEL"
    firewall_pointer_darwin
  fi
  log "Logs: $LAUNCHD_LOG"
  wait_ready "$LAUNCHD_LOG"
  if ! launchd_loaded; then
    log "WARNING: 127.0.0.1:$LISTEN_PORT is open but $LAUNCHD_LABEL is not loaded; something else holds the port"
  fi
}

print_next_steps() {
  log "Diagnostics: 'rnstatus' attaches to the shared instance and lists its interfaces"
  log "Matching coyote config (this is the shipped default; normally nothing to paste):"
  echo
  cat <<EOF
mesh:
  interfaces:
    - type: private
      host: 127.0.0.1
      port: $LISTEN_PORT
EOF
  echo
  log "Next: start coyote and run \`.mesh on\`"
}

main() {
  trap cleanup EXIT
  trap 'cleanup; exit 130' INT
  trap 'cleanup; exit 143' TERM

  parse_args "$@"
  need_cmd uname
  need_cmd mktemp
  detect_os
  refuse_root
  resolve_paths
  choose_rung

  log "OS: $OS"
  log "BIN_DIR: $BIN_DIR"
  inspect_existing_rnsd
  log "Install: $(describe_rung)"
  if [[ -f "$RNS_CONFIG" ]]; then
    log "Config: $RNS_CONFIG (already there)"
  else
    log "Config: $RNS_CONFIG (to be written)"
  fi
  log "Config dir: $RNS_DIR_REASON"
  if [[ "$NO_SERVICE" -eq 1 ]]; then
    log "Service: skipped (--no-service)"
  elif [[ "$RNS_DIR_SYSTEM" -eq 1 ]]; then
    log "Service: skipped; rnsd reads the system config $RNS_CONFIG, run the daemon the way that config's owner intends (a user service here would start a second rnsd on --config $RNS_DIR)"
  elif [[ "$OS" == "linux" ]]; then
    log "Service: systemd user unit $SYSTEMD_UNIT"
  else
    log "Service: launchd agent $LAUNCHD_PLIST"
  fi

  if [[ "$DRY_RUN" -eq 1 ]]; then
    log "Dry run: nothing will be written"
  else
    install_rnsd
  fi

  write_config

  if [[ "$NO_SERVICE" -eq 0 && "$RNS_DIR_SYSTEM" -eq 0 ]]; then
    if [[ "$OS" == "linux" ]]; then service_linux; else service_darwin; fi
  fi

  print_next_steps
}

main "$@"
