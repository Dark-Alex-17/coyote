#!/bin/sh
# Entrypoint for the coyote Docker image. Runs under tini:
#   ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/coyote-entrypoint"]
#
# tini (PID 1) reaps orphaned processes and forwards signals; coyote is not an
# init and does neither.

case "$1" in
  sh | bash | /*) exec "$@" ;;
esac

exec coyote "$@"
