#!/usr/bin/env python3
"""Drive bundled fake-claude scripts over stream-json NDJSON.

Used by scripts/acceptance/m0.sh. Speaks the same control frames as
crates/remuda-testing/src/client.rs.
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import subprocess
import sys
import threading
from typing import Any

FIXED_SESSION_ID = "00000000-0000-4000-8000-000000000001"
TIMEOUT_S = 10.0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", required=True, help="Path to fake-claude")
    parser.add_argument(
        "--script",
        required=True,
        choices=("ok", "approval", "askuser", "workflow"),
    )
    parser.add_argument("--script-path", help="FAKE_CLAUDE_SCRIPT path or name")
    parser.add_argument("--transcript-dir", help="FAKE_CLAUDE_TRANSCRIPT_DIR")
    parser.add_argument("--log", help="Write captured NDJSON here")
    parser.add_argument("--timeout", type=float, default=TIMEOUT_S)
    return parser.parse_args()


def send(proc: subprocess.Popen[str], obj: dict[str, Any]) -> None:
    assert proc.stdin is not None
    proc.stdin.write(json.dumps(obj, separators=(",", ":")) + "\n")
    proc.stdin.flush()


def pump_stdout(stream, out: "queue.Queue[str | None]") -> None:
    try:
        for line in stream:
            out.put(line)
    finally:
        out.put(None)


def read_line(lines: "queue.Queue[str | None]", timeout: float) -> dict[str, Any]:
    try:
        item = lines.get(timeout=timeout)
    except queue.Empty as exc:
        raise TimeoutError("timed out waiting for fake-claude stdout") from exc
    if item is None:
        raise RuntimeError("fake-claude stdout closed")
    line = item.strip()
    if not line:
        return read_line(lines, timeout)
    return json.loads(line)


def is_type(frame: dict[str, Any], want: str) -> bool:
    return frame.get("type") == want


def is_system(frame: dict[str, Any], subtype: str) -> bool:
    return is_type(frame, "system") and frame.get("subtype") == subtype


def is_control(frame: dict[str, Any], subtype: str) -> bool:
    req = frame.get("request") or {}
    return is_type(frame, "control_request") and req.get("subtype") == subtype


def allow_tool(frame: dict[str, Any]) -> dict[str, Any]:
    request = frame.get("request") or {}
    tool_input = request.get("input") or {}
    updated: dict[str, Any] = dict(tool_input) if isinstance(tool_input, dict) else {}
    if request.get("tool_name") == "AskUserQuestion":
        questions = updated.get("questions") or []
        answers: dict[str, str] = {}
        if questions and isinstance(questions[0], dict):
            q = questions[0].get("question") or "Tea or coffee?"
            answers[q] = "Tea"
        updated["answers"] = answers
    return {
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": frame.get("request_id"),
            "response": {"behavior": "allow", "updatedInput": updated},
        },
    }


def expected_results(script: str) -> int:
    return 2 if script == "workflow" else 1


def drive(args: argparse.Namespace) -> None:
    env = os.environ.copy()
    env["FAKE_CLAUDE_SCRIPT"] = args.script_path or args.script
    if args.transcript_dir:
        os.makedirs(args.transcript_dir, exist_ok=True)
        env["FAKE_CLAUDE_TRANSCRIPT_DIR"] = args.transcript_dir
    proc = subprocess.Popen(
        [
            args.bin,
            "-p",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "default",
            "--permission-prompts",
            "host",
            "--permission-prompt-tool",
            "stdio",
            "--session-id",
            FIXED_SESSION_ID,
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=None,
        text=True,
        env=env,
        bufsize=1,
    )
    lines: queue.Queue[str | None] = queue.Queue()
    reader = threading.Thread(target=pump_stdout, args=(proc.stdout, lines), daemon=True)
    reader.start()
    log_fp = open(args.log, "w", encoding="utf-8") if args.log else None
    results: list[dict[str, Any]] = []
    try:
        init = read_line(lines, args.timeout)
        if log_fp:
            log_fp.write(json.dumps(init) + "\n")
        if not is_system(init, "init"):
            raise RuntimeError(f"expected system/init, got {init.get('type')}/{init.get('subtype')}")
        send(
            proc,
            {
                "type": "control_request",
                "request_id": f"init-{args.script}",
                "request": {"subtype": "initialize"},
            },
        )
        while True:
            frame = read_line(lines, args.timeout)
            if log_fp:
                log_fp.write(json.dumps(frame) + "\n")
            if is_type(frame, "control_response"):
                echoed = (frame.get("response") or {}).get("request_id")
                if echoed == f"init-{args.script}":
                    break
        send(
            proc,
            {
                "type": "user",
                "message": {"role": "user", "content": f"m0 {args.script}"},
            },
        )
        need = expected_results(args.script)
        while len(results) < need:
            frame = read_line(lines, args.timeout)
            if log_fp:
                log_fp.write(json.dumps(frame) + "\n")
            if is_control(frame, "can_use_tool"):
                send(proc, allow_tool(frame))
                continue
            if is_type(frame, "result"):
                results.append(frame)
        if proc.stdin:
            proc.stdin.close()
            proc.stdin = None
        code = proc.wait(timeout=args.timeout)
        if code != 0:
            raise RuntimeError(f"fake-claude exit {code}")
        check_script(args.script, results)
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        if log_fp:
            log_fp.close()


def check_script(script: str, results: list[dict[str, Any]]) -> None:
    if not results:
        raise RuntimeError("no result frames")
    text = (results[-1].get("result") or "") if results else ""
    if script == "ok":
        if results[0].get("result") != "OK":
            raise RuntimeError(f"ok script expected OK, got {results[0].get('result')!r}")
    elif script == "approval":
        if text != "Done.":
            raise RuntimeError(f"approval script expected Done., got {text!r}")
    elif script == "askuser":
        if "tea" not in text.lower():
            raise RuntimeError(f"askuser script expected tea, got {text!r}")
    elif script == "workflow":
        if len(results) < 2:
            raise RuntimeError("workflow script expected two result frames")
        if results[-1].get("result") != "OK":
            raise RuntimeError(f"workflow script expected final OK, got {results[-1].get('result')!r}")
        if results[-1].get("result_index") != 1:
            raise RuntimeError(
                f"workflow script expected result_index 1, got {results[-1].get('result_index')!r}"
            )


def main() -> int:
    args = parse_args()
    try:
        drive(args)
    except Exception as exc:  # noqa: BLE001 — CLI boundary
        print(f"fake-claude-drive {args.script}: {exc}", file=sys.stderr)
        return 1
    print(f"fake-claude-drive {args.script}: pass", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
