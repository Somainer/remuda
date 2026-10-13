#!/usr/bin/env bash
set -u
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-uo0
exec 200>locks/gate-e2e.lock
echo "[uifault-r2b $(date +%T)] waiting for gate-e2e.lock ..."
flock -x 200
echo "[uifault-r2b $(date +%T)] acquired gate-e2e.lock"
pkill -f 'remuda-hub.*hub_e2e' 2>/dev/null || true
# Kill anything left on the e2e ports from a prior run (servers inside the lock).
pkill -f 'target/debug/examples/hub_e2e' 2>/dev/null || true
cleanup() {
  echo "[uifault-r2b $(date +%T)] cleanup: killing hub_e2e process group"
  pkill -KILL -f 'target/debug/examples/hub_e2e' 2>/dev/null || true
}
trap cleanup EXIT
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-uo0/web
export PW_CHANNEL=chromium
./node_modules/.bin/playwright test -c playwright.hub.config.ts \
  tests/e2e/effort-sync.hub.spec.ts \
  tests/e2e/ux-workflow-card.hub.spec.ts \
  tests/e2e/ux-wfdrill.hub.spec.ts \
  tests/e2e/ux-toolfold.hub.spec.ts \
  tests/e2e/new-session.spec.ts \
  tests/e2e/composer-effort.spec.ts \
  2>&1
code=$?
echo "[uifault-r2b $(date +%T)] playwright exit=$code"
exit $code
