#!/usr/bin/env python3
"""Synthetic gate executable for merge queue tests.

Beyond the trace of the normal stub it models two queue scenarios:

* ``gate-deny.txt`` at the worktree root fails ``cargo-test`` (a branch that
  breaks the gate itself).
* ``expect-ok.txt`` together with a ``state.txt`` whose content is not
  ``ok`` fails ``cargo-test``: branch 1 adds the expectation ("a test"),
  branch 2 changes the state ("the code the test checks"), and each branch
  passes alone while the merged tree fails.
"""
import json
import os
from pathlib import Path
import sys

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
if step == "gen-api-current":
    Path("web/src/lib/api.generated.ts").write_text(
        "generated client current\n", encoding="utf-8")
