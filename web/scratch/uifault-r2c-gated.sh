#!/usr/bin/env bash
set -u
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-uo0
exec 200>locks/gate-e2e.lock
flock -x 200
echo "[r2c $(date +%T)] acquired" >>/home/wangruming.nana/Projects/remuda-agents/wt/c-uo0/web/scratch/uifault-r2c.log
pkill -KILL -f 'target/debug/examples/hub_e2e' 2>/dev/null || true
trap 'pkill -KILL -f "target/debug/examples/hub_e2e" 2>/dev/null || true' EXIT
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-uo0/web
export PW_CHANNEL=chromium
# All output goes to a log file; the harness tmp fs stays untouched.
./node_modules/.bin/playwright test -c playwright.hub.config.ts \
  tests/e2e/effort-sync.hub.spec.ts </dev/null >/home/wangruming.nana/Projects/remuda-agents/wt/c-uo0/web/scratch/uifault-r2c.log 2>&1
echo "[r2c $(date +%T)] exit=$?" >>/home/wangruming.nana/Projects/remuda-agents/wt/c-uo0/web/scratch/uifault-r2c.log
