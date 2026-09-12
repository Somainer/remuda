#!/usr/bin/env bash
# Source: local test double for lark-cli 1.0.76 event consume / im +messages-* (2026-09-12).
# Does not contact Feishu or change lark-cli config.
set -u

SIGNAL_FILE="${FAKE_LARK_SIGNAL_FILE:-}"
COUNTER_FILE="${FAKE_LARK_COUNTER:-}"
FAIL_TIMES="${FAKE_LARK_FAIL_TIMES:-0}"
EVENTS_FILE="${FAKE_LARK_EVENTS_FILE:-}"

log_signal() {
  if [[ -n "$SIGNAL_FILE" ]]; then
    printf '%s\n' "$1" > "$SIGNAL_FILE"
  fi
}

if [[ "${1:-}" == "im" ]]; then
  printf '%s\n' '{"code":0,"data":{"message_id":"om_fake_outbound"},"msg":"ok"}'
  exit 0
fi

key="unknown"
args=("$@")
for i in "${!args[@]}"; do
  if [[ "${args[$i]}" == "consume" ]]; then
    next=$((i + 1))
    key="${args[$next]:-unknown}"
  fi
done

if [[ -n "$COUNTER_FILE" && "$FAIL_TIMES" -gt 0 ]]; then
  n=0
  if [[ -f "$COUNTER_FILE" ]]; then
    n=$(cat "$COUNTER_FILE")
  fi
  n=$((n + 1))
  printf '%s\n' "$n" > "$COUNTER_FILE"
  if [[ "$n" -le "$FAIL_TIMES" ]]; then
    echo "Error: simulated consume failure" >&2
    exit 1
  fi
fi

cleanup() {
  log_signal "term"
  echo "[event] exited — received 0 event(s) in 0s (reason: signal)" >&2
  exit 0
}
trap cleanup TERM INT

echo "[event] ready event_key=${key}" >&2

if [[ -n "$EVENTS_FILE" && -f "$EVENTS_FILE" ]]; then
  grep -v '^#' "$EVENTS_FILE" | grep -v '^$' || true
fi

# stdin keep-alive: loop until EOF or SIGTERM. Timeouts are not EOF.
while true; do
  if IFS= read -r -t 1 _line; then
    :
  else
    status=$?
    if [[ $status -eq 1 ]]; then
      log_signal "eof"
      echo "[event] exited — received 0 event(s) in 0s (reason: signal)" >&2
      exit 0
    fi
  fi
done
