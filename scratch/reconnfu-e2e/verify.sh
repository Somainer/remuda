#!/usr/bin/env bash
set -u
ROOT=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace
LOCK=/home/wangruming.nana/Projects/remuda-agents/locks/gate-e2e.lock
SCRATCH="$ROOT/scratch/reconnfu-e2e"; mkdir -p "$SCRATCH/tmp"
LOG="$SCRATCH/e2e-verify.log"
exec 200>"$LOCK"; echo "[$(date -Is)] waiting" | tee "$LOG"; flock -x 200; echo "[$(date -Is)] go" | tee -a "$LOG"
cd "$ROOT/web"
PW_CHANNEL=chromium TMPDIR="$SCRATCH/tmp" npx playwright test -c playwright.hub.config.ts \
  -g "browser context STILL offline" tests/e2e/offline-outbox.hub.spec.ts >>"$LOG" 2>&1
echo "[$(date -Is)] exit $?" | tee -a "$LOG"
