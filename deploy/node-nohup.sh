#!/usr/bin/env bash
# Foreground/nohup launcher for hosts without systemd (Docker containers,
# short canary). Not a production supervisor. One Node identity, one PID
# file; never start a second copy. Pair with a same-UID Herdr process.
set -euo pipefail

ROOT="${REMUDA_NODE_ROOT:-${HOME}/remuda-node}"
BIN="${REMUDA_BIN:-${ROOT}/bin/remuda}"
CONFIG="${REMUDA_NODE_CONFIG:-${ROOT}/etc/node.toml}"
TOKEN_FILE="${REMUDA_NODE_TOKEN_FILE:-${ROOT}/secrets/node-token}"
LOG="${REMUDA_NODE_LOG:-${ROOT}/log/node.log}"
PIDFILE="${REMUDA_NODE_PID:-${ROOT}/run/node.pid}"

usage() {
  cat <<'EOF'
Usage: node-nohup.sh start|stop|status

Env:
  REMUDA_NODE_ROOT   default $HOME/remuda-node
  REMUDA_BIN         default $ROOT/bin/remuda
  REMUDA_NODE_CONFIG default $ROOT/etc/node.toml
  REMUDA_NODE_TOKEN_FILE
  REMUDA_NODE_LOG
  REMUDA_NODE_PID

Cleanup after a canary: node-nohup.sh stop && rm -rf "$REMUDA_NODE_ROOT"
EOF
}

digest() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$BIN" | awk '{print $1}'
  else
    shasum -a 256 "$BIN" | awk '{print $1}'
  fi
}

is_running() {
  if [[ ! -f "$PIDFILE" ]]; then
    return 1
  fi
  local pid
  pid="$(cat "$PIDFILE")"
  [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null
}

cmd_start() {
  if is_running; then
    echo "already running pid=$(cat "$PIDFILE")" >&2
    exit 1
  fi
  if [[ ! -x "$BIN" ]]; then
    echo "missing executable $BIN" >&2
    exit 1
  fi
  mkdir -p "$(dirname "$LOG")" "$(dirname "$PIDFILE")"
  umask 077
  echo "starting remuda node bin=$BIN digest=$(digest) config=$CONFIG"
  nohup env REMUDA_NODE_TOKEN_FILE="$TOKEN_FILE" \
    "$BIN" node --config "$CONFIG" >>"$LOG" 2>&1 &
  echo $! >"$PIDFILE"
  echo "pid=$(cat "$PIDFILE") log=$LOG"
}

cmd_stop() {
  if ! is_running; then
    echo "not running"
    rm -f "$PIDFILE"
    return 0
  fi
  local pid
  pid="$(cat "$PIDFILE")"
  kill "$pid"
  local i=0
  while kill -0 "$pid" 2>/dev/null && (( i < 20 )); do
    sleep 0.2
    i=$((i + 1))
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill -9 "$pid" 2>/dev/null || true
  fi
  rm -f "$PIDFILE"
  echo "stopped pid=$pid"
}

cmd_status() {
  if is_running; then
    echo "running pid=$(cat "$PIDFILE") digest=$(digest)"
  else
    echo "stopped"
    exit 1
  fi
}

case "${1:-}" in
  start) cmd_start ;;
  stop) cmd_stop ;;
  status) cmd_status ;;
  -h|--help|help) usage ;;
  *) usage >&2; exit 2 ;;
esac
