#!/usr/bin/env bash
set -euo pipefail
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/web
export PW_CHANNEL=chromium
export TMPDIR=/tmp/ca-tmp
export HUB_E2E_SERVE_BIN=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/target-authrace/debug/examples/serve
export CARGO_TARGET_DIR=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/target-authrace
# Servers run INSIDE the flock; EXIT trap releases the lock and kills stragglers.
cleanup() {
  pkill -f "target-authrace/debug/examples/serve" 2>/dev/null || true
}
trap 'echo "[trap] gate-pwa exit=$?"; cleanup' EXIT
exec ./node_modules/.bin/playwright test -c playwright.hub.config.ts \
  pwa-shell.hub.spec.ts pwa-realbuild.hub.spec.ts offline-outbox.hub.spec.ts \
  --reporter=line
