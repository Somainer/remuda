#!/usr/bin/env bash
# c-reconnfu r2 recheck: journal-window + offline-outbox under gate-e2e.lock.
set -u

ROOT=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace
LOCK=/home/wangruming.nana/Projects/remuda-agents/locks/gate-e2e.lock
SCRATCH="$ROOT/scratch/reconnfu-r2"
mkdir -p "$SCRATCH/tmp"
LOG="$SCRATCH/e2e-recheck2.log"

exec 200>"$LOCK"
echo "[$(date -Is)] waiting for gate-e2e.lock" | tee "$LOG"
flock -x 200
echo "[$(date -Is)] acquired gate-e2e.lock" | tee -a "$LOG"
exec 200>&-

trap 'echo "[$(date -Is)] EXIT $?" | tee -a "$LOG"' EXIT

cd "$ROOT/web"
PW_CHANNEL=chromium TMPDIR="$SCRATCH/tmp" \
  npx playwright test -c playwright.hub.config.ts \
    tests/e2e/offline-outbox.hub.spec.ts \
     >>"$LOG" 2>&1
RC=$?
echo "[$(date -Is)] playwright exit $RC" | tee -a "$LOG"
exit $RC
