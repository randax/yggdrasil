#!/usr/bin/env bash
# Start (or restart) `yg serve --role=all` against the isolated stack.
#   ./serve.sh start [--cold]   --cold wipes the Shard cache dir first
#   ./serve.sh stop
# Server log: $STATE_DIR/serve.log (appended); pid in $STATE_DIR/serve.pid.
set -euo pipefail
. "$(dirname "$0")/env.sh"
mkdir -p "$STATE_DIR"

stop() {
  if [[ -f "$STATE_DIR/serve.pid" ]]; then
    local pid
    pid=$(cat "$STATE_DIR/serve.pid")
    if kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid"
      for _ in $(seq 1 90); do kill -0 "$pid" 2>/dev/null || break; sleep 0.5; done
    fi
    rm -f "$STATE_DIR/serve.pid"
  fi
}

start() {
  if [[ "${1:-}" == "--cold" ]]; then
    rm -rf "$YG_SHARD_CACHE"
  fi
  mkdir -p "$YG_GIT_CACHE" "$YG_SHARD_CACHE"
  echo "=== start $(date -u +%FT%TZ) ${1:-}" >>"$STATE_DIR/serve.log"
  RUST_LOG=${RUST_LOG:-info} nohup "$YG_BIN" serve --role=all >>"$STATE_DIR/serve.log" 2>&1 &
  echo $! >"$STATE_DIR/serve.pid"
  for _ in $(seq 1 120); do
    if curl -fsS "$YG_SERVER/healthz" >/dev/null 2>&1; then return 0; fi
    sleep 0.25
  done
  echo "server did not become healthy; see $STATE_DIR/serve.log" >&2
  exit 1
}

case "${1:-}" in
  start) shift; stop; start "$@" ;;
  stop) stop ;;
  *) echo "usage: $0 start [--cold] | stop" >&2; exit 2 ;;
esac
