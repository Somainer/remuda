#!/usr/bin/env python3
"""NDJSON peer used by remuda-claude-wire process tests. Not a real model."""

from __future__ import annotations

import json
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


def main() -> int:
    init = None
    while True:
        msg = read()
        if msg is None:
            return 1
        if msg.get("type") == "control_request" and (msg.get("request") or {}).get(
            "subtype"
        ) == "initialize":
            init = msg
            break

    send(
        {
            "type": "system",
            "subtype": "init",
            "cwd": "/tmp/fake",
            "session_id": "sess-1",
            "tools": ["Bash", "Workflow"],
        }
    )
    send({"type": "keep_alive"})
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
        if msg.get("type") == "user":
            break

    req_id = "perm-1"
    send(
        {
            "type": "control_request",
            "request_id": req_id,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Bash",
                "input": {"command": "touch x"},
                "tool_use_id": "toolu_1",
                "blocked_paths": ["/tmp/x"],
            },
        }
    )

    while True:
        msg = read()
        if msg is None:
            return 1
        if msg.get("type") == "control_response":
            echoed = (msg.get("response") or {}).get("request_id")
            send(
                {
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [
                            {
                                "type": "tool_result",
                                "tool_use_id": "toolu_1",
                                "content": f"echo:{echoed}",
                            }
                        ],
                    },
                }
            )
            send(
                {
                    "type": "result",
                    "subtype": "success",
                    "is_error": False,
                    "result": "Workflow launched. Waiting for completion...",
                    "num_turns": 2,
                    "result_index": 0,
                    "queued_turn_count": 0,
                }
            )
            send(
                {
                    "type": "system",
                    "subtype": "task_started",
                    "task_id": "t1",
                    "task_type": "local_workflow",
                    "description": "demo",
                }
            )
            send(
                {
                    "type": "result",
                    "subtype": "success",
                    "is_error": False,
                    "result": "OK",
                    "num_turns": 1,
                    "result_index": 1,
                    "queued_turn_count": 0,
                }
            )
            send({"type": "brand_new_future_event", "hello": True})
            send({"type": "system", "subtype": "brand_new_subtype", "x": 1})
            break

    while read() is not None:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
