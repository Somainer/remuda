#!/usr/bin/env python3
"""NDJSON peer that closes its STDIN after the handshake and stays alive.

D-057 OA6 ma-sdk-state r3 item 7: the parent's next write must fail
(BrokenPipe) while THIS PROCESS IS STILL RUNNING — a shutdown/EOF the parent
itself sends is a different path. The peer:

  1. completes the initialize handshake,
  2. closes fd 0 (sys.stdin.close() drops the read end of the pipe),
  3. writes $FAKE_READY_FILE so the test knows the pipe is broken,
  4. sleeps with stdout open.

Nothing more is emitted on stdout; the test proves the process survives.
"""

from __future__ import annotations

import json
import os
import sys
import time


def send(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def read() -> dict | None:
    while True:
        line = sys.stdin.readline()
        if line == "":
            return None
        line = line.strip()
        if not line:
            continue
        return json.loads(line)


def main() -> int:
    while True:
        msg = read()
        if msg is None:
            return 1
        if msg.get("type") == "control_request" and (msg.get("request") or {}).get(
            "subtype"
        ) == "initialize":
            init = msg
            break

    send({"type": "system", "subtype": "init", "cwd": "/tmp/fake", "session_id": "s-1"})
    send(
        {
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": init["request_id"],
                "response": {"commands": []},
            },
        }
    )
    sys.stdout.flush()

    # Drop the read end of the parent's stdin pipe, then prove it on disk.
    # os.close(0) is deliberate: sys.stdin.close() only closes the wrapper and
    # buffered readers can leave the underlying fd mapped, so the parent's next
    # write does not see EPIPE until the interpreter exits.
    os.close(0)
    ready = os.environ.get("FAKE_READY_FILE", "")
    if ready:
        with open(ready, "w", encoding="utf-8") as fh:
            fh.write("ready\n")

    # Keep the process (and stdout) alive; the parent's write must error
    # without the child having exited.
    for _ in range(600):
        time.sleep(0.1)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
