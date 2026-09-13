#!/usr/bin/env python3
"""Synthetic gate executable for merge_cli.rs; no Cargo, pnpm, or network work."""
import json
import os
from pathlib import Path
import subprocess
import sys

step = sys.argv[1]
trace = Path(os.environ["REMUDA_TEST_GATE_TRACE"])
with trace.open("a", encoding="utf-8") as output:
    output.write(json.dumps({
        "step": step,
        "cwd": str(Path.cwd()),
        "target": os.environ.get("CARGO_TARGET_DIR"),
        "incremental": os.environ.get("CARGO_INCREMENTAL"),
    }) + "\n")

if step == "cargo-test" and os.environ.get("REMUDA_TEST_GATE_CAS"):
    subprocess.run(["git", "update-ref", "refs/heads/main",
                    os.environ["REMUDA_TEST_GATE_CAS"]], check=True)
if step == "gen-api-current":
    Path("web/src/lib/api.generated.ts").write_text(
        "generated client current\n", encoding="utf-8")
if step == os.environ.get("REMUDA_TEST_GATE_FAIL"):
    sys.exit(1)
if step == os.environ.get("REMUDA_TEST_GATE_RETRY"):
    marker = trace.with_suffix(".retried")
    if not marker.exists():
        marker.write_text("first attempt failed\n", encoding="utf-8")
        sys.exit(1)
if step == "cargo-test" and os.environ.get("REMUDA_TEST_GATE_MUTATE"):
    Path("shared file.txt").write_text("gate changed tracked content\n", encoding="utf-8")
