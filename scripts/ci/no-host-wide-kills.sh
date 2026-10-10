#!/usr/bin/env bash
# Forbid host-wide process tools in gate and test code.
#
# A gate or a test may only signal the exact pids it started: lsof/fuser
# scan machine-wide state, and pattern/name kill tools can reap another
# worker's processes on a shared build host (a host-wide pattern kill of
# "time.sleep(300)" in a gate supervisor test once tore down unrelated
# processes on the box). Walk /proc and signal a recorded pid instead.
#
# This file is excluded from its own scan because it defines the pattern.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

pattern='\b(lsof|fuser|pkill|killall)\b'

if grep -RInE --exclude='no-host-wide-kills.sh' --exclude-dir='target' \
    --exclude-dir='target-gate' "$pattern" crates/ scripts/ci/; then
    cat >&2 <<'EOF'
no-host-wide-kills: found a host-wide process tool in gate/test code.
Signal only pids your own process spawned (record them at spawn, walk
/proc for descendants); see scripts/tests/test_gate_supervisor.py.
EOF
    exit 1
fi
echo "no-host-wide-kills: passed"
