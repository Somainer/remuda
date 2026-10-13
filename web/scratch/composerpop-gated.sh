#!/usr/bin/env bash
set -u
BASE=/home/wangruming.nana/Projects/remuda-agents
exec 200>"$BASE/locks/gate-e2e.lock"
flock -x 200
LOG=$BASE/wt/c-uo0/web/scratch/composerpop-gate.log
echo "[run $(date +%T)] acquired" >>"$LOG"
mkdir -p "$HOME/r-tmp"
trap 'pkill -KILL -f "target-c-uo0/debug/examples/hub_e2e" 2>/dev/null || true' EXIT
cd "$BASE/wt/c-uo0/web"
export PW_CHANNEL=chromium
export TMPDIR="$HOME/r-tmp"
export HUB_E2E_LISTEN=127.0.0.1:59080
export HUB_E2E_WEB_PORT=59089
export HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59081
export PW_TEST_CONNECT_WS_ENDPOINT=ws://127.0.0.1:3177
./node_modules/.bin/playwright test -c playwright.hub.config.ts \
  tests/e2e/composer-popover.hub.spec.ts "$@" </dev/null >>"$LOG" 2>&1
echo "[run $(date +%T)] exit=$?" >>"$LOG"
