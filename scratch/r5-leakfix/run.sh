#!/bin/bash
# c-authrace leak-fix repro: own specs first, then the three gate failures,
# one hub process, serial, under the shared gate-e2e.lock.
set -u

WT=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace
SCRATCH="$WT/scratch/r5-leakfix"
mkdir -p "$SCRATCH" "$HOME/r-tmp"

echo "waiting for lock $(date -Is)" > "$SCRATCH/status"
exec 200>/home/wangruming.nana/Projects/remuda-agents/locks/gate-e2e.lock
flock -x 200
echo "acquired lock $(date -Is)" > "$SCRATCH/status"

PGID=""
cleanup() {
  if [ -n "$PGID" ]; then
    kill -KILL -"$PGID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

export HUB_E2E_LISTEN=127.0.0.1:59380
export HUB_E2E_WEB_PORT=59389
export HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59381
export TMPDIR="$HOME/r-tmp"
mkdir -p "$TMPDIR"

cd "$WT/web" || exit 1

trap '' TERM INT
setsid env \
  PW_CHANNEL=chromium \
  HUB_E2E_LISTEN="$HUB_E2E_LISTEN" \
  HUB_E2E_WEB_PORT="$HUB_E2E_WEB_PORT" \
  HUB_E2E_UPSTREAM_LISTEN="$HUB_E2E_UPSTREAM_LISTEN" \
  TMPDIR="$TMPDIR" \
  200>&- \
  ./node_modules/.bin/playwright test -c playwright.hub.config.ts \
  tests/e2e/offline-outbox.hub.spec.ts \
  tests/e2e/ux-command-id.hub.spec.ts \
  tests/e2e/task-model-list.hub.spec.ts \
  tests/e2e/uo3-evidence.hub.spec.ts \
  tests/e2e/ux-mobile-new.hub.spec.ts \
  >"$SCRATCH/e2e.log" 2>&1 &
PGID=$!

wait "$PGID"
rc=$?
trap - EXIT
kill -KILL -"$PGID" 2>/dev/null || true
echo "rc=$rc finished $(date -Is)" >> "$SCRATCH/status"
exit "$rc"
