#!/usr/bin/env bash
# Start or stop an isolated `herdr --session <name>` headless server.
# Socket path is the only stdout line. Never touches the user default session.
set -euo pipefail

SESSION="${HERDR_SESSION:-remuda-test}"
COMMAND=""
FIXTURE_ONLY=0
HERDR_BIN="${HERDR_BINARY:-herdr}"

usage() {
  cat <<'EOF' >&2
Usage: herdr-isolated.sh start|stop|status [--session remuda-test] [--fixture-only]

  start          spawn `herdr --session <name> server` if the socket is down
  stop           `herdr --session <name> session stop` only (never default)
  status         print the socket path if the named session looks alive
  --fixture-only print the socket path; do not spawn or stop herdr

Stdout: absolute API socket path.
Stderr: human logs.
EOF
}

log() { printf '%s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

config_dir() {
  if [[ -n "${XDG_CONFIG_HOME:-}" ]]; then
    printf '%s/herdr' "$XDG_CONFIG_HOME"
  else
    printf '%s/.config/herdr' "${HOME:-.}"
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    start|stop|status)
      COMMAND="$1"
      shift
      ;;
    --session)
      [[ $# -ge 2 ]] || die "--session needs a value"
      SESSION="$2"
      shift 2
      ;;
    --session=*)
      SESSION="${1#--session=}"
      shift
      ;;
    --fixture-only)
      FIXTURE_ONLY=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage
      exit 2
      ;;
  esac
done

[[ -n "$COMMAND" ]] || { usage; exit 2; }
[[ -n "$SESSION" ]] || die "session name is empty"
if [[ "$SESSION" == "default" ]]; then
  die "refusing the user default herdr session; pass a dedicated name such as remuda-test"
fi

CFG="$(config_dir)"
SOCKET="$CFG/sessions/$SESSION/herdr.sock"
DEFAULT_SOCKET="$CFG/herdr.sock"
PIDFILE="/tmp/remuda-herdr-${SESSION}.pid"
LOGFILE="/tmp/remuda-herdr-${SESSION}.log"

if [[ "$SOCKET" == "$DEFAULT_SOCKET" ]]; then
  die "named session socket resolved to the default socket ($DEFAULT_SOCKET)"
fi

emit_socket() {
  printf '%s\n' "$SOCKET"
}

pid_is_our_session() {
  local pid="$1"
  local args
  args="$(ps -p "$pid" -o args= 2>/dev/null || true)"
  [[ -n "$args" ]] || return 1
  [[ "$args" == *"$DEFAULT_SOCKET"* ]] && return 1
  [[ "$args" == *" --session ${SESSION} "* ]] || [[ "$args" == *" --session ${SESSION}" ]] || [[ "$args" == *"--session=${SESSION}"* ]]
}

cmd_start() {
  if [[ "$FIXTURE_ONLY" -eq 1 ]]; then
    log "fixture-only: not starting herdr session=$SESSION"
    emit_socket
    return 0
  fi
  if [[ -S "$SOCKET" ]]; then
    log "herdr session=$SESSION already has a socket"
    emit_socket
    return 0
  fi
  command -v "$HERDR_BIN" >/dev/null 2>&1 || die "herdr binary not found (HERDR_BINARY=$HERDR_BIN)"
  mkdir -p "$(dirname "$SOCKET")"
  : >"$LOGFILE"
  log "starting herdr --session $SESSION server"
  (
    unset HERDR_SOCKET_PATH HERDR_ENV HERDR_PANE_ID
    export HERDR_SESSION="$SESSION"
    nohup "$HERDR_BIN" --session "$SESSION" server >>"$LOGFILE" 2>&1 &
    echo $! >"$PIDFILE"
  )
  local i=0
  while [[ $i -lt 50 ]]; do
    if [[ -S "$SOCKET" ]]; then
      emit_socket
      return 0
    fi
    if [[ -f "$PIDFILE" ]]; then
      local pid
      pid="$(cat "$PIDFILE" 2>/dev/null || true)"
      if [[ -n "$pid" ]] && ! kill -0 "$pid" 2>/dev/null; then
        die "herdr session=$SESSION exited before the socket appeared; see $LOGFILE"
      fi
    fi
    sleep 0.1
    i=$((i + 1))
  done
  die "timed out waiting for $SOCKET"
}

cmd_stop() {
  if [[ "$FIXTURE_ONLY" -eq 1 ]]; then
    log "fixture-only: not stopping herdr session=$SESSION"
    emit_socket
    return 0
  fi
  if command -v "$HERDR_BIN" >/dev/null 2>&1; then
    log "stopping herdr session=$SESSION"
    (
      unset HERDR_SOCKET_PATH HERDR_ENV HERDR_PANE_ID
      export HERDR_SESSION="$SESSION"
      "$HERDR_BIN" --session "$SESSION" session stop
    ) >/dev/null 2>&1 || true
  else
    log "herdr binary missing; will only reap pidfile if present"
  fi
  if [[ -f "$PIDFILE" ]]; then
    local pid
    pid="$(cat "$PIDFILE" 2>/dev/null || true)"
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      if pid_is_our_session "$pid"; then
        kill "$pid" 2>/dev/null || true
        local j=0
        while kill -0 "$pid" 2>/dev/null && [[ $j -lt 20 ]]; do
          sleep 0.1
          j=$((j + 1))
        done
        if kill -0 "$pid" 2>/dev/null; then
          kill -9 "$pid" 2>/dev/null || true
        fi
      else
        log "pid $pid in $PIDFILE is not session=$SESSION; not killing"
      fi
    fi
    rm -f "$PIDFILE"
  fi
  if [[ -S "$DEFAULT_SOCKET" ]]; then
    log "default herdr socket left untouched: $DEFAULT_SOCKET"
  fi
  emit_socket
}

cmd_status() {
  if [[ -S "$SOCKET" ]]; then
    log "session=$SESSION socket exists"
    emit_socket
    return 0
  fi
  log "session=$SESSION socket missing"
  emit_socket
  return 1
}

case "$COMMAND" in
  start) cmd_start ;;
  stop) cmd_stop ;;
  status) cmd_status ;;
  *) usage; exit 2 ;;
esac
