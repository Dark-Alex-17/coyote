#!/usr/bin/env bash
set -euo pipefail

# Local Reticulum daemon (rnsd) for Coyote mesh (Linux/macOS)
#
# Installs rnsd, writes ~/.reticulum/config once, and registers a user service
# that keeps the daemon running; every Coyote session on this host then dials
# it at 127.0.0.1:4242.
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
#   --dry-run            Print the plan and write nothing.
#   --allow-root         Permit running as root.
#
# Exit codes: 0 done or nothing to do; 1 usage; 2 unsupported OS or no way to
# install rnsd; 3 service installed but 127.0.0.1:4242 did not come up in 30 s.

# Must track `ARG RNS_VERSION` in deployment/propagation-node/Dockerfile (the interop-verified version).
DEFAULT_RNS_SPEC="rns==1.5.2"

SERVICE_NAME="coyote-rnsd"
LAUNCHD_LABEL="com.coyote.rnsd"
LISTEN_PORT=4242
READY_TIMEOUT=30

usage() {
  echo "coyote mesh relay setup (Linux/macOS): install rnsd, write its config, run it as a user service"
  echo
  echo "Options:"
  echo "  --relay <host:port>     Also dial your team's relay (TCPClientInterface)"
  echo "  --version <X>           rns version to install (default: ${DEFAULT_RNS_SPEC#rns==})"
  echo "  --bin-dir <dir>         Where the rnsd command goes (default: /usr/local/bin or ~/.local/bin)"
  echo "  --no-service            Skip the user service"
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

# Mirrors how coyote itself resolves its config directory.
resolve_paths() {
  if [[ -z "${HOME:-}" ]]; then err "HOME is not set"; exit 2; fi
  RNS_DIR="$HOME/.reticulum"
  RNS_CONFIG="$RNS_DIR/config"

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

describe_rung() {
  case "$INSTALL_RUNG" in
    present) echo "reuse $EXISTING_RNSD (already works)";;
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

install_rnsd() {
  local real=""
  case "$INSTALL_RUNG" in
    present)
      log "rnsd already installed: $EXISTING_RNSD"
      link_rnsd "$EXISTING_RNSD"
      ;;
    uv)
      log "Installing $RNS_SPEC with uv (managed CPython 3.12)"
      if ! uv tool install --python 3.12 "$RNS_SPEC"; then err "uv tool install failed"; exit 2; fi
      real="$(uv tool dir --bin --color never)/rnsd"
      if ! rnsd_works "$real" && command -v rnsd >/dev/null 2>&1; then real="$(command -v rnsd)"; fi
      link_rnsd "$real"
      ;;
    pipx)
      log "Installing $RNS_SPEC with pipx"
      if ! pipx install "$RNS_SPEC"; then err "pipx install failed"; exit 2; fi
      real="$(pipx_bin_dir)/rnsd"
      if ! rnsd_works "$real" && command -v rnsd >/dev/null 2>&1; then real="$(command -v rnsd)"; fi
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

# Shared instance left at the default so rnstatus, rnpath, Sideband and NomadNet on
# this host attach to this daemon instead of binding 4242 / AutoInterface again.

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
  log "Firewall: AutoInterface listens for LAN peers, and with enable_transport this rnsd forwards traffic for any Reticulum peer on the LAN (and on to the Team Relay when one is configured). macOS and Windows will ask whether python/rnsd may accept incoming connections; allow it or discovery of LAN hosts will not work."
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
    if grep -q AutoInterface "$RNS_CONFIG"; then firewall_warning; fi
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

systemd_unit_text() {
  cat <<EOF
[Unit]
Description=Reticulum daemon for Coyote mesh
After=network.target

[Service]
ExecStart=$RNSD
Environment=PYTHONUNBUFFERED=1
Restart=on-failure
RestartSec=5

[Install]
WantedBy=default.target
EOF
}

launchd_plist_text() {
  cat <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$LAUNCHD_LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$RNSD</string>
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
  <string>$LAUNCHD_LOG</string>
  <key>StandardErrorPath</key>
  <string>$LAUNCHD_LOG</string>
</dict>
</plist>
EOF
}

# Writes $2 to $1 unless the file already holds exactly that text; returns 1 when nothing changed.
write_if_changed() {
  local path="$1" text="$2"
  if [[ -f "$path" ]] && [[ "$(cat "$path")" == "$text" ]]; then
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
    if [[ -f "$SYSTEMD_UNIT" ]] && [[ "$(cat "$SYSTEMD_UNIT")" == "$unit" ]]; then
      log "Service: $SYSTEMD_UNIT already present with this content"
    else
      log "Would write $SYSTEMD_UNIT:"
    fi
    echo
    echo "$unit"
    echo
    if user_bus_reachable; then
      log "Would run: systemctl --user daemon-reload && systemctl --user enable --now $SERVICE_NAME (unless already active)"
    else
      log "No user session bus here; would write the unit and print how to enable it"
    fi
    linger_hint
    return 0
  fi

  if write_if_changed "$SYSTEMD_UNIT" "$unit"; then
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
    log "$SERVICE_NAME is already running; not restarted"
  else
    systemctl --user enable --now "$SERVICE_NAME"
    log "Enabled and started $SERVICE_NAME"
  fi
  linger_hint
  wait_ready "journalctl --user -u $SERVICE_NAME"
}

launchd_loaded() {
  launchctl print "gui/$(id -u)/$LAUNCHD_LABEL" >/dev/null 2>&1
}

service_darwin() {
  local plist
  plist="$(launchd_plist_text)"
  if [[ "$DRY_RUN" -eq 1 ]]; then
    if [[ -f "$LAUNCHD_PLIST" ]] && [[ "$(cat "$LAUNCHD_PLIST")" == "$plist" ]]; then
      log "Service: $LAUNCHD_PLIST already present with this content"
    else
      log "Would write $LAUNCHD_PLIST:"
    fi
    echo
    echo "$plist"
    echo
    if launchd_loaded; then
      log "$LAUNCHD_LABEL is already loaded; nothing to do"
    else
      log "Would run: launchctl bootstrap gui/$(id -u) $LAUNCHD_PLIST"
    fi
    log "Logs: $LAUNCHD_LOG"
    return 0
  fi

  if write_if_changed "$LAUNCHD_PLIST" "$plist"; then
    log "Wrote $LAUNCHD_PLIST"
  else
    log "$LAUNCHD_PLIST already present with this content"
  fi
  if command -v plutil >/dev/null 2>&1; then plutil -lint "$LAUNCHD_PLIST"; fi
  mkdir -p "$(dirname "$LAUNCHD_LOG")"

  if launchd_loaded; then
    log "$LAUNCHD_LABEL is already loaded; not restarted"
  else
    launchctl bootstrap "gui/$(id -u)" "$LAUNCHD_PLIST"
    log "Bootstrapped $LAUNCHD_LABEL"
  fi
  log "Logs: $LAUNCHD_LOG"
  wait_ready "$LAUNCHD_LOG"
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
  log "Install: $(describe_rung)"
  if [[ -f "$RNS_CONFIG" ]]; then
    log "Config: $RNS_CONFIG (already there)"
  else
    log "Config: $RNS_CONFIG (to be written)"
  fi
  if [[ "$NO_SERVICE" -eq 1 ]]; then
    log "Service: skipped (--no-service)"
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

  if [[ "$NO_SERVICE" -eq 0 ]]; then
    if [[ "$OS" == "linux" ]]; then service_linux; else service_darwin; fi
  fi

  print_next_steps
}

main "$@"
