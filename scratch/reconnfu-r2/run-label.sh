#!/usr/bin/env bash
# c-reconnfu r2 gated e2e: Playwright owns BOTH servers (as the merge gate
# does) under the shared gate-e2e.lock; servers stay inside the locked section
# and fd 200 is closed before exec so child processes never inherit it.
set -u

ROOT=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace
LOCK=/home/wangruming.nana/Projects/remuda-agents/locks/gate-e2e.lock
SCRATCH="$ROOT/scratch/reconnfu-r2"
mkdir -p "$SCRATCH/tmp"
LOG="$SCRATCH/e2e-label2.log"

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
    -g "Hub-accepted send shows its delivered label" >>"$LOG" 2>&1
RC=$?
echo "[$(date -Is)] playwright exit $RC" | tee -a "$LOG"
exit $RC
