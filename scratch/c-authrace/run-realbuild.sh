#!/usr/bin/env bash
set -euo pipefail
trap 'echo "[trap] exit $? on line $LINENO"' EXIT
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/web
export PW_CHANNEL=chromium
export HUB_E2E_SERVE_BIN=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/target-authrace/debug/examples/serve
export CARGO_TARGET_DIR=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/target-authrace
exec ./node_modules/.bin/playwright test -c playwright.hub.config.ts pwa-realbuild.hub.spec.ts --reporter=line
