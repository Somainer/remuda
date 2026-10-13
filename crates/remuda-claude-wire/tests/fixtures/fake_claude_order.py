#!/usr/bin/env python3
"""NDJSON peer that records the EXACT order inbound lines were read.

Used by the FIFO writer-queue tests (D-057 OA6 ma-sdk-state r3): controls
and user prompts share one queue on the parent, so the wire order here must
be the enqueue order. Every inbound frame appends one marker line to the file
named by $FAKE_ORDER_FILE:

  control_request:<subtype>   (initialize/interrupt/…)
  control_response
  user

Control requests get a success control_response so a real handshake/control
round-trip completes; the process exits on stdin EOF.
"""

from __future__ import annotations

import json
import os
import sys


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


def marker(msg: dict, path: str) -> None:
    ty = msg.get("type", "unknown")
    if ty == "control_request":
        subtype = (msg.get("request") or {}).get("subtype", "")
        line = f"control_request:{subtype}\n"
    else:
        line = f"{ty}\n"
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(line)


def main() -> int:
    order_file = os.environ.get("FAKE_ORDER_FILE", "")
    init = None
    while True:
        msg = read()
        if msg is None:
            return 1
        if order_file:
            marker(msg, order_file)
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

    while True:
        msg = read()
        if msg is None:
            return 0
        if order_file:
            marker(msg, order_file)
        if msg.get("type") == "control_request":
            subtype = (msg.get("request") or {}).get("subtype", "")
            request_id = msg.get("request_id", "")
            send(
                {
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": request_id,
                        "response": {"still_queued": []}
                        if subtype == "interrupt"
                        else {},
                    },
                }
            )


if __name__ == "__main__":
    raise SystemExit(main())
