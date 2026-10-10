#!/usr/bin/env bash
# Smoke test for a built coyote image: proves the entrypoint contract (rnsd beside
# the main command, TERM/INT delivered to the main command and not to rnsd,
# exit-code passthrough) and that a container reaches a propagation node over
# COYOTE_MESH_RELAY.
#
# Usage: image-smoke.sh <image> [--pn <pn-image>]
# Without --pn, deployment/propagation-node is built as coyote-pn:smoke.
set -euo pipefail

usage() {
  echo "usage: $0 <image> [--pn <pn-image>]" >&2
  exit 2
}

image="${1:-}"
[[ -n "$image" ]] || usage
shift
pn_image=
while [[ $# -gt 0 ]]; do
  case "$1" in
    --pn)
      [[ $# -ge 2 ]] || usage
      pn_image="$2"
      shift 2
      ;;
    *) usage ;;
  esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
suffix="$$-$(date +%s)"
t="coyote-smoke-main-$suffix"
i="coyote-smoke-int-$suffix"
pn="coyote-smoke-pn-$suffix"
c="coyote-smoke-relay-$suffix"
net="coyote-smoke-$suffix"

cleanup() {
  docker rm -f "$t" "$i" "$pn" "$c" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
}
trap cleanup EXIT

ok() {
  echo "ok: $*"
}

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

fail_with_logs() {
  local container="$1"
  shift
  echo "FAIL: $*" >&2
  echo "--- docker logs $container (last 40 lines)" >&2
  docker logs "$container" 2>&1 | tail -n 40 >&2 || true
  exit 1
}

# Polls `docker exec <container> bash -c <probe>` once a second for up to 30 s.
wait_exec() {
  local container="$1" probe="$2"
  for _ in $(seq 1 30); do
    if docker exec "$container" bash -c "$probe" >/dev/null 2>&1; then
      return 0
    fi
    sleep 1
  done
  return 1
}

if [[ -z "$pn_image" ]]; then
  pn_image="coyote-pn:smoke"
  docker build -q -t "$pn_image" "$repo_root/deployment/propagation-node" >/dev/null
  ok "built $pn_image"
fi

# 1. The passthrough branch: the Docker Sandboxes keep-alive shape, where rnsd must start too.
docker run -d --name "$t" "$image" sh -c 'sleep infinity' >/dev/null
ok "started $t"

# 2. The coyote binary is on PATH and runs as uid 1000.
version="$(docker exec "$t" coyote --version)" || fail_with_logs "$t" "coyote --version failed"
[[ -n "$version" ]] || fail_with_logs "$t" "coyote --version printed nothing"
ok "coyote --version: $version"

# 3. rnsd came up on the loopback hop every coyote session dials.
wait_exec "$t" 'exec 3<>/dev/tcp/127.0.0.1/4242' \
  || fail_with_logs "$t" "127.0.0.1:4242 did not answer within 30 s"
ok "rnsd listens on 127.0.0.1:4242"

# 4. Process model: tini is PID 1, rnsd and the main child are uid 1000, rnsd owns its session.
ps_table="$(docker exec "$t" ps -eo pid,user,comm)"
pid1_comm="$(docker exec "$t" ps -o comm= -p 1)"
[[ "$pid1_comm" == "tini" ]] || fail_with_logs "$t" "PID 1 is $pid1_comm, not tini:"$'\n'"$ps_table"
ok "PID 1 is tini"
uid_comm="$(docker exec "$t" ps -eo uid=,comm=)"
grep -Eq '^ *1000 +rnsd$' <<<"$uid_comm" || fail_with_logs "$t" "no rnsd owned by uid 1000:"$'\n'"$ps_table"
grep -Eq '^ *1000 +sleep$' <<<"$uid_comm" || fail_with_logs "$t" "no sleep (main child) owned by uid 1000:"$'\n'"$ps_table"
ok "rnsd and the main child run as uid 1000"
rnsd_row="$(docker exec "$t" ps -eo pid=,sid=,comm= | awk '$3 == "rnsd"')"
[[ -n "$rnsd_row" ]] || fail_with_logs "$t" "no rnsd row in ps:"$'\n'"$ps_table"
read -r rnsd_pid rnsd_sid _ <<<"$rnsd_row"
[[ "$rnsd_pid" == "$rnsd_sid" ]] || fail_with_logs "$t" "rnsd pid $rnsd_pid is not its own session leader (sid $rnsd_sid)"
ok "rnsd is a session leader (pid $rnsd_pid == sid)"
exec_uid="$(docker exec "$t" id -u)"
[[ "$exec_uid" == "1000" ]] || fail "docker exec runs as uid $exec_uid, not 1000"
ok "container user is uid 1000"

# 5. The interpreter rnsd runs under, found via its shebang.
docker exec "$t" bash -c '"$(sed -n "1s/^#!//p" "$(command -v rnsd)")" -c "import ssl, cryptography"' \
  || fail_with_logs "$t" "rnsd's interpreter cannot import ssl and cryptography"
ok "rnsd's python imports ssl and cryptography"

# 6. The rendered config: logging section present, loopback listener only, no opt-in stanzas.
config="$(docker exec "$t" cat /home/agent/.reticulum/config)"
for needle in '[logging]' 'loglevel = 4' 'ingress_control = No' 'listen_ip = 127.0.0.1'; do
  grep -qF -- "$needle" <<<"$config" || fail "rendered config lacks \`$needle\`:"$'\n'"$config"
done
for absent in 'AutoInterface' 'Team Relay' 'share_instance = No'; do
  ! grep -qF -- "$absent" <<<"$config" || fail "rendered config must not contain \`$absent\` by default:"$'\n'"$config"
done
ok "rendered config has the logging section and the loopback listener only"

# 7. docker's default grace is 10 s before SIGKILL; TERM must reach the main child well inside it.
start=$(date +%s)
docker stop "$t" >/dev/null
elapsed=$(( $(date +%s) - start ))
[[ "$elapsed" -lt 10 ]] || fail_with_logs "$t" "docker stop took ${elapsed}s"
exit_code="$(docker inspect --format '{{.State.ExitCode}}' "$t")"
[[ "$exit_code" == "143" ]] || fail_with_logs "$t" "exit code after docker stop is $exit_code, not 143"
ok "docker stop took ${elapsed}s and exited 143"

# 8. INT reaches the main command (tini -g, foreground child with default dispositions)
#    and not rnsd, which exits on SIGINT (RNS Reticulum.py:375): a command that handles
#    INT keeps running with its daemon, one that does not dies of it as under plain docker.
docker run -d --name "$i" "$image" bash -c 'trap "echo main-got-INT" INT; while :; do sleep 1; done' >/dev/null
wait_exec "$i" 'exec 3<>/dev/tcp/127.0.0.1/4242' \
  || fail_with_logs "$i" "127.0.0.1:4242 did not answer within 30 s in the INT container"
docker kill -s INT "$i" >/dev/null
sleep 2
docker logs "$i" 2>&1 | grep -q 'main-got-INT' || fail_with_logs "$i" "the main command's INT trap did not fire"
docker exec "$i" ps -eo comm= | grep -qx rnsd || fail_with_logs "$i" "rnsd did not survive an INT the main command handled"
ok "INT reaches the main command and leaves rnsd running"
docker rm -f "$i" >/dev/null
set +e
docker run --name "$i" "$image" sh -c 'sleep 999' >/dev/null 2>&1 &
sleep 3
docker kill -s INT "$i" >/dev/null 2>&1
wait $!
rc=$?
set -e
[[ "$rc" == "130" ]] || fail_with_logs "$i" "INT on an unhandled main command gave exit $rc, not 130"
ok "INT on an unhandled main command exits 130"
docker rm -f "$i" >/dev/null

# 9. The main child's exit status is the container's exit status.
set +e
docker run --rm "$image" sh -c 'exit 7'
rc=$?
set -e
[[ "$rc" == "7" ]] || fail "exit code passthrough gave $rc, not 7"
ok "exit code 7 passes through"

# 10. The opt-out leaves no rnsd behind.
docker run --rm -e COYOTE_MESH_RNSD=0 "$image" bash -c 'sleep 1; ! pgrep -x rnsd' \
  || fail "rnsd ran despite COYOTE_MESH_RNSD=0"
ok "COYOTE_MESH_RNSD=0 starts no rnsd"

# 11. Only ~/.reticulum and /tmp need to be writable for rnsd to start.
set +e
read_only_err="$(docker run --rm --read-only --tmpfs /home/agent/.reticulum --tmpfs /tmp "$image" \
  bash -c 'for _ in $(seq 1 60); do exec 3<>/dev/tcp/127.0.0.1/4242 && exit 0; sleep 0.5; done; exit 1' 2>&1 >/dev/null)"
rc=$?
set -e
[[ "$rc" == "0" ]] || fail "rnsd did not come up on a read-only root with tmpfs mounts:"$'\n'"$read_only_err"
ok "rnsd comes up on a read-only root"

# 12. A malformed relay value warns and is dropped from the config; the main child is unaffected.
set +e
bad_relay_err="$(docker run --rm -e COYOTE_MESH_RELAY=bad "$image" sh -c 'exit 0' 2>&1 >/dev/null)"
rc=$?
set -e
[[ "$rc" == "0" ]] || fail "COYOTE_MESH_RELAY=bad changed the main child's exit code to $rc"
grep -q 'WARNING:' <<<"$bad_relay_err" || fail "COYOTE_MESH_RELAY=bad produced no WARNING:"$'\n'"$bad_relay_err"
ok "a malformed COYOTE_MESH_RELAY warns and never blocks the main child"

# 13. Two containers on one network: a propagation node and a coyote image dialing it.
docker network create "$net" >/dev/null
docker run -d --name "$pn" --network "$net" "$pn_image" >/dev/null
pn_up=0
for _ in $(seq 1 30); do
  if docker run --rm --network "$net" -e COYOTE_MESH_RNSD=0 "$image" bash -c "exec 3<>/dev/tcp/$pn/4242" >/dev/null 2>&1; then
    pn_up=1
    break
  fi
  sleep 1
done
[[ "$pn_up" == "1" ]] || fail_with_logs "$pn" "propagation node $pn:4242 did not answer within 30 s"
ok "propagation node answers on $pn:4242"

docker run -d --name "$c" --network "$net" -e COYOTE_MESH_RELAY="$pn:4242" "$image" sh -c 'sleep infinity' >/dev/null
# The first alternative is the post-connect line: RNS 1.5.2 TCPInterface.py:233 logs
# `Establishing TCP connection for ...` before connecting and :247 logs
# `TCP connection for ... established` after, hence the `] established` suffix anchor;
# a bare `TCP connection for TCPInterface[Team Relay` would match the pre-connect line
# and could never fail. The second alternative is :290, when the first attempt failed
# and a retry connected. `[^]]` is a POSIX bracket expression matching anything but `]`.
relay_re='TCP connection for TCPInterface\[Team Relay/[^]]*\] established|Reconnected socket for TCPInterface\[Team Relay/'
connected=0
for _ in $(seq 1 30); do
  if docker logs "$c" 2>&1 | grep -Eq "$relay_re"; then
    connected=1
    break
  fi
  sleep 1
done
if [[ "$connected" != "1" ]]; then
  echo "--- docker exec $c rnstatus" >&2
  docker exec "$c" rnstatus >&2 || true
  fail_with_logs "$c" "no established Team Relay connection to $pn within 30 s"
fi
ok "Team Relay connection to the propagation node established"

echo "all image smoke assertions passed for $image"
