#!/usr/bin/env bash
set -euo pipefail
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/web
export PW_CHANNEL=chromium TMPDIR=/tmp/ca-tmp
export CARGO_TARGET_DIR=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/target-authrace
exec ./node_modules/.bin/playwright test -c playwright.hub.config.ts offline-outbox.hub.spec.ts --reporter=line
