#!/usr/bin/env bash
set -u
cd /home/wangruming.nana/Projects/remuda-agents
exec 200>locks/gate-e2e.lock
flock -x 200
pkill -KILL -f 'target/debug/examples/hub_e2e' 2>/dev/null || true
trap 'pkill -KILL -f "target/debug/examples/hub_e2e" 2>/dev/null || true' EXIT
cd wt/c-uo0/web
export PW_CHANNEL=chromium
./node_modules/.bin/playwright test -c playwright.hub.config.ts \
  tests/e2e/effort-sync.hub.spec.ts -g "orthogonal switch" --workers 1 </dev/null
