#!/usr/bin/env bash
# Fails-before-fix evidence: run with the 7 c-composerpop UI files swapped to
# base 3bd07311 (test seams in store.ts/SessionPage.tsx stay FIXED).
set -u
BASE=/home/wangruming.nana/Projects/remuda-agents
exec 200>"$BASE/locks/gate-e2e.lock"
flock -x 200
GREP="$1"; LOG="$2"
: > "$LOG"
echo "[baseproof $(date +%T)] $GREP" >>"$LOG"
trap 'pkill -KILL -f "target-c-uo0/debug/examples/hub_e2e" 2>/dev/null || true' EXIT
cd "$BASE/wt/c-uo0/web"
export PW_CHANNEL=chromium TMPDIR="$HOME/r-tmp"
export HUB_E2E_LISTEN=127.0.0.1:59080 HUB_E2E_WEB_PORT=59089 HUB_E2E_UPSTREAM_LISTEN=127.0.0.1:59081
export PW_TEST_CONNECT_WS_ENDPOINT=ws://127.0.0.1:3177
./node_modules/.bin/playwright test -c playwright.hub.config.ts \
  tests/e2e/composer-popover.hub.spec.ts -g "$GREP" </dev/null >>"$LOG" 2>&1
echo "[baseproof $(date +%T)] exit=$?" >>"$LOG"
