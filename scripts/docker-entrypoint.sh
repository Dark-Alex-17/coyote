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
#   process group, i.e. to the main command directly, as long as it stays in this
#   script's process group (coyote and `sh -c` do; an interactive shell with job
#   control moves itself out and must be stopped on its own terms); this script
#   traps them only to stay alive until the main command returns and then exits
#   with its status (143 for a command killed by the TERM, coyote's own code
#   otherwise). PID 1 reaps the orphans a dying main command leaves regardless;
#   tini -s keeps it reaping (and quiet) when it is not PID 1, e.g. under
#   `docker run --init`.
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
  printf 'coyote-entrypoint: WARNING: %s\n' "$*" >&2
}

note() {
  printf 'coyote-entrypoint: %s\n' "$*" >&2
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
  case "${COYOTE_MESH_LAN:-}" in
    "") ;;
    1)
      lan=1
      note "COYOTE_MESH_LAN=1: with --network host this rnsd listens for LAN peers and, with enable_transport, forwards traffic for any Reticulum peer on the LAN (and on to the Team Relay when one is configured)"
      ;;
    *) warn "COYOTE_MESH_LAN=$COYOTE_MESH_LAN is not 1; AutoInterface not added" ;;
  esac
  relay=0
  if [ -n "$relay_host" ]; then
    relay=1
  fi
  tmp=$(mktemp "$config_dir/.config.XXXXXX") || {
    warn "cannot write to $config_dir; rnsd not started"
    return 1
  }
  # A CRLF checkout of the template would otherwise defeat the marker lines.
  if awk -v lan="$lan" -v relay="$relay" -v host="$relay_host" -v port="$relay_port" '
    { sub(/\r$/, "") }
    /^#@if lan$/ { skip = !lan; next }
    /^#@if relay$/ { skip = !relay; next }
    /^#@end$/ { skip = 0; next }
    skip { next }
    { gsub(/@RELAY_HOST@/, host); gsub(/@RELAY_PORT@/, port); print }
  ' "$template" > "$tmp"; then
    mv "$tmp" "$config_dir/config" || {
      rm -f "$tmp"
      warn "cannot write $config_dir/config; rnsd not started"
      return 1
    }
  else
    rm -f "$tmp"
    warn "rendering $template failed; rnsd not started"
    return 1
  fi
}

# Names the opt-in variables that are set but have no effect on an existing config.
note_existing_config() {
  unapplied=
  if [ -n "${COYOTE_MESH_RELAY:-}" ]; then
    unapplied=COYOTE_MESH_RELAY
  fi
  if [ -n "${COYOTE_MESH_LAN:-}" ]; then
    unapplied="${unapplied:+$unapplied/}COYOTE_MESH_LAN"
  fi
  if [ -n "$unapplied" ]; then
    note "$config_dir/config exists; $unapplied not applied (edit the file or remove it)"
  fi
}

start_rnsd() {
  home="${HOME:-/home/agent}"
  config_dir="$home/.reticulum"
  if ! mkdir -p "$config_dir"; then
    warn "cannot create $config_dir; rnsd not started"
    return 1
  fi
  if [ ! -e "$config_dir/config" ]; then
    render_config || return 1
  else
    note_existing_config
  fi
  # rnsd reads ~/.reticulum by default, so no --config. `-vv` only takes effect
  # because the template has a [logging] section (Reticulum.py:459-467), and
  # RNS.log is a bare print() (RNS/__init__.py:129-134): without PYTHONUNBUFFERED=1
  # the lines sit in CPython's block buffer while fd 2 is a pipe and `docker logs`
  # never shows them. Output stays raw on the container's stderr; a prefixing pipe
  # would hide the pid needed to stop it. `env -i` keeps the container's
  # environment (LLM provider keys among it) away from the one network-facing
  # process. `env` and `setsid` both exec in place: the `&` child shares this
  # script's process group and so is not a group leader, util-linux setsid
  # therefore calls setsid() without forking, and $! is rnsd's own pid.
  env -i HOME="$home" PATH="$PATH" PYTHONUNBUFFERED=1 setsid rnsd -vv </dev/null >&2 &
  rnsd_pid=$!
}

# "Ours" means still our child: the parent pid survives the env/setsid/rnsd execs
# and setsid(), where comm only becomes rnsd after the last exec; a main command
# that returns before then must still stop the daemon it started. dash reaps an
# early-dead rnsd while waiting on the foreground main command, after which the
# pid is free, and a reused pid has another parent. Without /proc, kill -0 is all
# there is. The stat line is `pid (comm) S ppid ...` and comm may itself contain
# spaces or `)`, hence the strip through the last `) `.
rnsd_alive() {
  [ -n "$rnsd_pid" ] || return 1
  kill -0 "$rnsd_pid" 2>/dev/null || return 1
  [ -r /proc/"$rnsd_pid"/stat ] || return 0
  ppid=$(awk '{ s = $0; sub(/^.*\) /, "", s); split(s, f, " "); print f[2] }' /proc/"$rnsd_pid"/stat 2>/dev/null)
  [ "$ppid" = "$$" ]
}

# tini -g forwards every signal it receives to the group, so the main command gets
# each one at the same moment this script does and decides for itself; the trap
# keeps this script alive until the command returns. With only TERM and INT
# trapped, a HUP (or a Ctrl-\ QUIT under -it) would kill this script, tini would
# exit, and the container would be torn down around a live main command with rnsd
# never stopped. A trap with a command (unlike `trap ''`) is reset to the default
# in every child, so neither rnsd nor the main command inherits it. It is set
# before rnsd is spawned: a TERM landing between the spawn and the trap would kill
# this script and leave rnsd running. Forwarding INT here would be wrong twice
# over: the command already has it, and rnsd installs a SIGINT handler that exits
# (Reticulum.py:375), so a Ctrl-C that coyote survives would take the daemon down.
# rnsd handles INT and TERM itself and sits in its own session, so widening the
# trap changes nothing for it.
trap ':' HUP INT QUIT TERM USR1 USR2

case "${COYOTE_MESH_RNSD:-}" in
  0) note "COYOTE_MESH_RNSD=0, rnsd not started" ;;
  "" | 1) start_rnsd ;;
  *)
    warn "COYOTE_MESH_RNSD=$COYOTE_MESH_RNSD is not 0; rnsd started"
    start_rnsd
    ;;
esac

if [ "$passthrough" = 1 ]; then
  "$@"
else
  coyote "$@"
fi
rc=$?

# A TERM that lands before the exec chain reaches rnsd kills the `env`/`setsid`
# stage with its default disposition: after an instant-exit main command the
# daemon simply never starts. Only a child we signalled is waited on.
if rnsd_alive; then
  kill -s TERM "$rnsd_pid" 2>/dev/null
  # kill -0 succeeds on an exited-but-unreaped child; the loop ends early only
  # because the shell reaps the background rnsd while waiting on the foreground
  # sleep (dash, bash and ash all do). Do not replace it with a fixed sleep.
  polls=0
  while [ "$polls" -lt 25 ] && rnsd_alive; do
    sleep 0.2
    polls=$((polls + 1))
  done
  if rnsd_alive; then
    kill -s KILL "$rnsd_pid" 2>/dev/null
  fi
  wait "$rnsd_pid"
fi

exit "$rc"
