#!/usr/bin/env bash
# c-reconnfu gated e2e: let Playwright own BOTH servers (as the merge gate
# does) under the shared gate-e2e.lock. With no HUB_E2E_EXTERNAL the operator
# real-Node tests skip themselves.
set -u

ROOT=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace
LOCK=/home/wangruming.nana/Projects/remuda-agents/locks/gate-e2e.lock
SCRATCH="$ROOT/scratch/reconnfu-e2e"
mkdir -p "$SCRATCH/tmp"
LOG="$SCRATCH/e2e2.log"

exec 200>"$LOCK"
echo "[$(date -Is)] waiting for gate-e2e.lock" | tee "$LOG"
flock -x 200
echo "[$(date -Is)] acquired gate-e2e.lock" | tee -a "$LOG"

cd "$ROOT/web"
PW_CHANNEL=chromium TMPDIR="$SCRATCH/tmp" \
  npx playwright test -c playwright.hub.config.ts \
    tests/e2e/offline-outbox.hub.spec.ts     tests/e2e/hub-live.spec.ts     tests/e2e/m-inbox.hub.spec.ts     tests/e2e/journal-window.hub.spec.ts >>"$LOG" 2>&1
RC=$?
echo "[$(date -Is)] playwright exit $RC" | tee -a "$LOG"
exit $RC
