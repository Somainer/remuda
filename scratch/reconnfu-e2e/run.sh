#!/usr/bin/env bash
# c-reconnfu item 8 gated e2e: hub + 4 specs under the shared gate-e2e.lock.
set -u

ROOT=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace
LOCK=/home/wangruming.nana/Projects/remuda-agents/locks/gate-e2e.lock
SCRATCH="$ROOT/scratch/reconnfu-e2e"
mkdir -p "$SCRATCH/tmp"
LOG="$SCRATCH/e2e.log"

exec 200>"$LOCK"
echo "[$(date -Is)] waiting for gate-e2e.lock" | tee "$LOG"
flock -x 200
echo "[$(date -Is)] acquired gate-e2e.lock" | tee -a "$LOG"

HUB_PGID=""
cleanup() {
  if [ -n "$HUB_PGID" ]; then
    kill -9 -"$HUB_PGID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

cd "$ROOT"
# Hub inside the lock, lock fd closed for its process group; SIGKILL on exit
# (hub_e2e exits on ctrl-c; ignore stray TERM/INT).
setsid env HUB_E2E_LISTEN=127.0.0.1:58880 \
  HUB_E2E_ORIGINS="http://127.0.0.1:58889,http://localhost:58889" \
  cargo run -p remuda-hub --example hub_e2e --locked 200>&- \
  >"$SCRATCH/hub.log" 2>&1 &
HUB_PGID=$!
trap '' TERM INT

for _ in $(seq 1 240); do
  if curl -sf -o /dev/null http://127.0.0.1:58880/healthz; then break; fi
  if ! kill -0 "$HUB_PGID" 2>/dev/null; then
    echo "hub exited early; see $SCRATCH/hub.log" | tee -a "$LOG"
    exit 1
  fi
  sleep 2
done
curl -sf http://127.0.0.1:58880/healthz >/dev/null || { echo "hub healthz failed" | tee -a "$LOG"; exit 1; }
echo "[$(date -Is)] hub healthy" | tee -a "$LOG"

cd "$ROOT/web"
PW_CHANNEL=chromium HUB_E2E_EXTERNAL=1 TMPDIR="$SCRATCH/tmp" \
  npx playwright test -c playwright.hub.config.ts \
    offline-outbox hub-live m-inbox journal-window >>"$LOG" 2>&1
RC=$?
echo "[$(date -Is)] playwright exit $RC" | tee -a "$LOG"
exit $RC
