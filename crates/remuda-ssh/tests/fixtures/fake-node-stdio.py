#!/usr/bin/env python3
"""NDJSON peer used by remuda-ssh transport tests. Matches remuda-node StdioCarrier."""

from __future__ import annotations

import json
import os
import sys


def read_frame():
    line = sys.stdin.readline()
    if not line:
        return None
    return json.loads(line)


def write_frame(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


HELLO = {
    "jsonrpc": "2.0",
    "method": "node.hello",
    "params": {
        "nodeVersion": "0.1.0",
        "protocol": {"major": 1, "minor": 0, "framing": "ndjson"},
    },
}


def main() -> int:
    mode = os.environ.get("REMUDA_FAKE_NODE", "hello-exit")
    if mode == "hello-exit":
        write_frame(HELLO)
        return 0
    if mode == "echo":
        write_frame(HELLO)
        incoming = read_frame()
        if incoming is not None:
            write_frame(incoming)
        return 0
    sys.stderr.write(f"unknown REMUDA_FAKE_NODE={mode}\n")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
