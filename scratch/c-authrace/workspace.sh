#!/usr/bin/env bash
set -euo pipefail
cd /home/wangruming.nana/Projects/remuda-agents/wt/c-authrace
export TMPDIR=/tmp
export CARGO_TARGET_DIR=/home/wangruming.nana/Projects/remuda-agents/wt/c-authrace/target-authrace
nice -n 10 cargo test --workspace --locked
