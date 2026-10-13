#!/usr/bin/env bash
# c-uifold round 2 gated hub e2e: one Playwright process, one hub, under the
# shared gate-e2e.lock. Servers live inside the lock; fd 200 is closed in the
# child so spawned hubs never hold it. Runs ONLY the specs this round touched.
set -u
cd /home/wangruming.nana/Projects/remuda-agents
exec 200>locks/gate-e2e.lock
echo "[uifold-r2 $(date +%T)] waiting for gate-e2e.lock"
flock -x 200
echo "[uifold-r2 $(date +%T)] acquired gate-e2e.lock"

# Kill anything left on the e2e ports from a crashed prior run (this devbox).
pkill -f 'remuda-hub.*hub_e2e' 2>/dev/null || true

cleanup() {
  echo "[uifold-r2 $(date +%T)] cleanup: killing hub_e2e process group"
  pkill -KILL -f 'target/debug/examples/hub_e2e' 2>/dev/null || true
  pkill -KILL -f 'cargo run -p remuda-hub --example hub_e2e' 2>/dev/null || true
}
trap cleanup EXIT

cd wt/c-uo0/web
export PW_CHANNEL=chromium
# Let Playwright own cargo (hub) + vite (web) as its webServers; close the
# lock fd for the whole process tree.
./node_modules/.bin/playwright test -c playwright.hub.config.ts \
  tests/e2e/ux-toolfold.hub.spec.ts \
  tests/e2e/ux-wfdrill.hub.spec.ts \
  tests/e2e/ux-workflow-card.hub.spec.ts \
  200>&-
rc=$?
echo "[uifold-r2 $(date +%T)] playwright exit=$rc"
exit $rc
