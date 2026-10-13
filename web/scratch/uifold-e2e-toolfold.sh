#!/usr/bin/env bash
# c-uifold: isolated rerun of the compact-fold collapse spec after a
# navigation/bootstrap login flake (servers were up; login-page never painted).
set -uo pipefail
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-uo0/web
LOCK=/home/wangruming.nana/Projects/remuda-agents/locks/gate-e2e.lock
exec 200>"$LOCK"
echo "[uifold-tf] $(date +%T) waiting for gate-e2e.lock ..."
flock -x 200
echo "[uifold-tf] $(date +%T) lock acquired"
trap 'flock -u 200 2>/dev/null || true' EXIT
export PW_CHANNEL=chromium
./node_modules/.bin/playwright test -c playwright.hub.config.ts \
  tests/e2e/ux-toolfold.hub.spec.ts
code=$?
echo "[uifold-tf] $(date +%T) playwright exit: $code"
exit $code
