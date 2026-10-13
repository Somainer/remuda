#!/usr/bin/env bash
set -u
cd /home/wangruming.nana/Projects/remuda-agents
exec 200>locks/gate-e2e.lock
flock -x 200
LOG=/home/wangruming.nana/Projects/remuda-agents/wt/c-uo0/web/scratch/pop-gated.log
echo "[pop $(date +%T)] acquired" >>"$LOG"
pkill -KILL -f 'target-c-uo0/debug/examples/hub_e2e' 2>/dev/null || true
trap 'pkill -KILL -f "target-c-uo0/debug/examples/hub_e2e" 2>/dev/null || true' EXIT
cd wt/c-uo0/web
export PW_CHANNEL=chromium
./node_modules/.bin/playwright test -c playwright.hub.config.ts tests/e2e/composer-popover.hub.spec.ts </dev/null >>"$LOG" 2>&1
echo "[pop $(date +%T)] exit=$?" >>"$LOG"
