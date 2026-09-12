#!/usr/bin/env python3
"""Offline consume double: fixture replay, restart accounting, and SIGTERM evidence.

Never calls Feishu, reads credentials, or starts a model. Outbound invocation is
an error: composition tests must select DryRun.
"""
import json
import os
from pathlib import Path
import signal
import sys

directory = Path(os.environ["REMUDA_TEST_LARK_DIR"])
if sys.argv[1:3] != ["event", "consume"]:
    (directory / "unexpected-outbound").write_text("called")
    sys.exit(97)

key = sys.argv[3]
profile = sys.argv[sys.argv.index("--profile") + 1]
identity = sys.argv[sys.argv.index("--as") + 1]
assert profile == "dispatcher-test" and identity == "bot"
counter = directory / (key + ".starts")
attempt = int(counter.read_text()) + 1 if counter.exists() else 1


def stop(_signum, _frame):
    (directory / (key + ".stopped")).write_text("SIGTERM")
    sys.exit(0)


signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
counter.write_text(str(attempt))
if os.environ.get("REMUDA_TEST_LARK_FAIL_ONCE") and attempt == 1:
    sys.exit(1)
if not os.environ.get("REMUDA_TEST_LARK_UNREADY"):
    print("[event] ready " + key, file=sys.stderr, flush=True)
    if key == "im.message.receive_v1":
        recording = Path(__file__).with_name("dispatcher-inbound.jsonl")
        for line in recording.read_text().splitlines():
            if line.startswith("#") or not line.strip():
                continue
            if os.environ.get("REMUDA_TEST_LARK_STATUS_ONLY") and json.loads(line)["content"] != "/status":
                continue
            print(line, flush=True)
    # Written only after every replayed line has been flushed, so a test can
    # wait for delivery rather than for the restart counter, which is bumped
    # before the replay begins.
    (directory / (key + ".replayed")).write_text(str(attempt))

# EOF is distinct from SIGTERM so tests can prove the root retained stdin.
for _line in sys.stdin:
    pass
(directory / (key + ".stopped")).write_text("EOF")
