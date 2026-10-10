#!/bin/sh
# Entrypoint for the coyote Docker image. Runs under tini:
#   ENTRYPOINT ["/usr/bin/tini", "-s", "-g", "--", "/usr/local/bin/coyote-entrypoint"]
#
# Process model: tini (PID 1) -> this script -> two children.
#   rnsd is started in the background in its own session (setsid), so neither a
#   TTY-generated SIGINT under `docker run -it` nor tini's group signal reaches it;
#   it is stopped here once the main command has returned.
#   The main command runs in this script's foreground, so it keeps the default
#   signal dispositions and the container's stdin. A dash background job would
#   get neither: dash hard-ignores SIGINT/SIGQUIT in an `&` child and points its
#   fd 0 at /dev/null (jobs.c forkchild()), which made `docker run -it <img>
#   sh -c ...` impossible to interrupt.
#   tini -g delivers TERM (docker stop) and INT (docker kill -s INT) to the whole
#   process group, i.e. to the main command directly; this script traps them only
#   to stay alive until the main command returns and then exits with its status
#   (143 for a command killed by the TERM, coyote's own code otherwise). tini -s
#   makes PID 1 a subreaper for the grandchildren a dying main command leaves.
# The main command is `coyote "$@"`, or "$@" itself when the first arg is sh, bash
# or a path (the Docker Sandboxes keep-alive; `coyote --sandbox` reaches coyote via
# `sbx exec` and never passes through here). rnsd starts on both paths.
#
# Environment:
#   COYOTE_MESH_RNSD=0           do not start rnsd
#   COYOTE_MESH_RELAY=host:port  add a [[Team Relay]] TCPClientInterface to the config
#   COYOTE_MESH_LAN=1            add the AutoInterface (only useful with --network host)
# The config is rendered from /opt/coyote/reticulum.config.tmpl into
# $HOME/.reticulum/config on first start only; an existing file is never touched.

template=/opt/coyote/reticulum.config.tmpl
rnsd_pid=

case "$1" in
  sh | bash | /*) passthrough=1 ;;
  *) passthrough=0 ;;
esac

warn() {
  echo "coyote-entrypoint: WARNING: $*" >&2
}

# Sets relay_host/relay_port from COYOTE_MESH_RELAY, or leaves them empty with a
# warning. The charset check is what makes the awk gsub below safe: `&` and `\`
# are special in a replacement, and anything outside [A-Za-z0-9._-] would reach
# rnsd's config parser unescaped.
parse_relay() {
  relay_host=
  relay_port=
  [ -n "${COYOTE_MESH_RELAY:-}" ] || return 0
  host="${COYOTE_MESH_RELAY%:*}"
  port="${COYOTE_MESH_RELAY##*:}"
  case "$COYOTE_MESH_RELAY" in
    *:*) ;;
    *) host= ;;
  esac
  case "$host" in
    "" | *[!A-Za-z0-9._-]*) host= ;;
  esac
  case "$port" in
    "" | *[!0-9]* | ??????*) port= ;;
    *) if [ "$port" -lt 1 ] || [ "$port" -gt 65535 ]; then port=; fi ;;
  esac
  if [ -z "$host" ] || [ -z "$port" ]; then
    warn "COYOTE_MESH_RELAY=$COYOTE_MESH_RELAY is not host:port; no relay stanza written"
    return 0
  fi
  relay_host="$host"
  relay_port="$port"
}

# Renders the template into $config_dir/config. The #@if lan / #@if relay / #@end
# marker lines select optional stanzas and never reach the output.
render_config() {
  if [ ! -r "$template" ]; then
    warn "$template is missing or unreadable; rnsd not started"
    return 1
  fi
  parse_relay
  lan=0
  if [ "${COYOTE_MESH_LAN:-}" = 1 ]; then
    lan=1
  fi
  relay=0
  if [ -n "$relay_host" ]; then
    relay=1
  fi
  tmp=$(mktemp "$config_dir/.config.XXXXXX") || {
    warn "cannot write to $config_dir; rnsd not started"
    return 1
  }
  if awk -v lan="$lan" -v relay="$relay" -v host="$relay_host" -v port="$relay_port" '
    /^#@if lan$/ { skip = !lan; next }
    /^#@if relay$/ { skip = !relay; next }
    /^#@end$/ { skip = 0; next }
    skip { next }
    { gsub(/@RELAY_HOST@/, host); gsub(/@RELAY_PORT@/, port); print }
  ' "$template" > "$tmp"; then
    mv "$tmp" "$config_dir/config"
  else
    rm -f "$tmp"
    warn "rendering $template failed; rnsd not started"
    return 1
  fi
}

start_rnsd() {
  config_dir="${HOME:-/home/agent}/.reticulum"
  if ! mkdir -p "$config_dir"; then
    warn "cannot create $config_dir; rnsd not started"
    return 1
  fi
  if [ ! -e "$config_dir/config" ]; then
    render_config || return 1
  fi
  # rnsd reads ~/.reticulum by default, so no --config. `-vv` only takes effect
  # because the template has a [logging] section (Reticulum.py:459-467), and
  # RNS.log is a bare print() (RNS/__init__.py:129-134): without PYTHONUNBUFFERED=1
  # the lines sit in CPython's block buffer while fd 2 is a pipe and `docker logs`
  # never shows them. Output stays raw on the container's stderr; a prefixing pipe
  # would hide the pid needed to stop it. The `setsid` process of this `&` job
  # shares this script's process group and so is not a group leader; util-linux
  # setsid therefore calls setsid() without forking and $! is rnsd's own pid.
  PYTHONUNBUFFERED=1 setsid rnsd -vv </dev/null >&2 &
  rnsd_pid=$!
}

if [ "${COYOTE_MESH_RNSD:-}" = 0 ]; then
  echo "coyote-entrypoint: COYOTE_MESH_RNSD=0, rnsd not started" >&2
else
  start_rnsd
fi

# The main command gets the group signal from tini at the same moment this script
# does and decides for itself; the trap keeps this script alive until the command
# returns. A trap with a command (unlike `trap ''`) leaves the main command's
# disposition at the default. Forwarding INT here would be wrong twice over: the
# command already has it, and rnsd installs a SIGINT handler that exits
# (Reticulum.py:375), so a Ctrl-C that coyote survives would take the daemon down.
trap ':' TERM INT

if [ "$passthrough" = 1 ]; then
  "$@"
else
  coyote "$@"
fi
rc=$?

if [ -n "$rnsd_pid" ]; then
  kill -s TERM "$rnsd_pid" 2>/dev/null
  polls=0
  while [ "$polls" -lt 25 ] && kill -0 "$rnsd_pid" 2>/dev/null; do
    sleep 0.2
    polls=$((polls + 1))
  done
  if kill -0 "$rnsd_pid" 2>/dev/null; then
    kill -s KILL "$rnsd_pid" 2>/dev/null
  fi
  wait "$rnsd_pid"
fi

exit "$rc"
