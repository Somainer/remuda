#!/usr/bin/env bash
# Repeat the SW-restore case under the gate lock.
set -u
ROOT=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace
LOCK=/home/wangruming.nana/Projects/remuda-agents/locks/gate-e2e.lock
SCRATCH="$ROOT/scratch/reconnfu-r2"
mkdir -p "$SCRATCH/tmp"
LOG="$SCRATCH/e2e-swrpt2.log"
: > "$LOG"
exec 200>"$LOCK"
echo "[$(date -Is)] acquired gate-e2e.lock" | tee -a "$LOG"
exec 200>&-
trap 'echo "[$(date -Is)] EXIT $?" | tee -a "$LOG"' EXIT
cd "$ROOT/web"
PW_CHANNEL=chromium TMPDIR="$SCRATCH/tmp" \
  npx playwright test -c playwright.hub.config.ts \
    tests/e2e/offline-outbox.hub.spec.ts \
    -g "STILL offline" --repeat-each 3 >>"$LOG" 2>&1
