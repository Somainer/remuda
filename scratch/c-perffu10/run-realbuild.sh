#!/usr/bin/env bash
set -euo pipefail
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/web
export PW_CHANNEL=chromium
export TMPDIR=/tmp/ca-tmp
export HUB_E2E_SERVE_BIN=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/target-authrace/debug/examples/serve
export CARGO_TARGET_DIR=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/target-authrace
# The documented gate override, pointed at a BOGUS origin the test must ignore.
export VITE_HUB_URL=http://127.0.0.1:60180
exec ./node_modules/.bin/playwright test -c playwright.hub.config.ts pwa-realbuild.hub.spec.ts --reporter=line
