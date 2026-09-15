#!/usr/bin/env python3
"""Synthetic gate executable for merge queue tests.

Beyond the trace of the normal stub it models these scenarios:

* ``gate-deny.txt`` at the worktree root fails ``cargo-test`` (a branch that
  breaks the gate itself).
* ``expect-ok.txt`` together with a ``state.txt`` whose content is not
  ``ok`` fails ``cargo-test``: branch 1 adds the expectation ("a test"),
  branch 2 changes the state ("the code the test checks"), and each branch
  passes alone while the merged tree fails.
* ``queue-kill.txt`` makes this gate process send SIGKILL to itself during
  ``cargo-test`` (an externally killed lane: no final report is written).
* ``queue-hang.txt`` parks the gate in ``cargo-test`` forever, so the queue
  watchdog can kill the lane itself.
"""
import json
import os
from pathlib import Path
import signal
import sys
import time

step = sys.argv[1]
trace = Path(os.environ["REMUDA_TEST_GATE_TRACE"])
with trace.open("a", encoding="utf-8") as output:
    output.write(json.dumps({
        "step": step,
        "cwd": str(Path.cwd()),
        "target": os.environ.get("CARGO_TARGET_DIR"),
        "incremental": os.environ.get("CARGO_INCREMENTAL"),
        "hubListen": os.environ.get("HUB_E2E_LISTEN"),
        "webPort": os.environ.get("HUB_E2E_WEB_PORT"),
    }) + "\n")

if step == "cargo-test":
    if Path("gate-deny.txt").is_file():
        sys.exit(1)
    if Path("expect-ok.txt").is_file():
        state = Path("state.txt")
        if not state.is_file() or state.read_text(encoding="utf-8").strip() != "ok":
            sys.exit(1)
    if Path("queue-kill.txt").is_file():
        time.sleep(float(os.environ.get("REMUDA_TEST_KILL_DELAY", "1")))
        os.kill(os.getpid(), signal.SIGKILL)
    if Path("queue-hang.txt").is_file():
        while True:
            time.sleep(3600)
if step == "gen-api-current":
    Path("web/src/lib/api.generated.ts").write_text(
        "generated client current\n", encoding="utf-8")
