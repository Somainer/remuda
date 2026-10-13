#!/usr/bin/env bash
# c-uifold: run the three touched hub specs under the shared gate-e2e.lock.
# Playwright owns both servers (cargo hub_e2e + vite), both inside the lock.
set -uo pipefail
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-uo0/web
LOCK=/home/wangruming.nana/Projects/remuda-agents/locks/gate-e2e.lock
exec 200>"$LOCK"
echo "[uifold] $(date +%T) waiting for gate-e2e.lock ..."
flock -x 200
echo "[uifold] $(date +%T) lock acquired"
trap 'echo "[uifold] $(date +%T) exit trap, releasing lock"; flock -u 200 2>/dev/null || true' EXIT
export PW_CHANNEL=chromium
# No `--` before the file filters: Playwright treats tokens after `--` as
# non-option args but still ignores them as test filters here.
./node_modules/.bin/playwright test -c playwright.hub.config.ts \
  tests/e2e/ux-wfdrill.hub.spec.ts \
  tests/e2e/ux-workflow-card.hub.spec.ts \
  tests/e2e/ux-toolfold.hub.spec.ts
code=$?
echo "[uifold] $(date +%T) playwright exit: $code"
exit $code
