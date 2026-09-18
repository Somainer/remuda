#!/usr/bin/env python3
"""Bounded stdio metadata probe. Never calls tools or reads app/window content."""

import argparse
import json
import os
from pathlib import Path
import selectors
import subprocess
import sys
import time


class ProbeError(Exception):
    pass


def probe(timeout, include_schema):
    launcher = Path(__file__).resolve().with_name("launch-mcp.sh")
    process = subprocess.Popen(
        ["/bin/sh", str(launcher)],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        bufsize=0,
    )
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ, "stdout")
    selector.register(process.stderr, selectors.EVENT_READ, "stderr")
    pending = bytearray()
    stderr_bytes = 0
    deadline = time.monotonic() + timeout

    def send(message):
        encoded = (json.dumps({"jsonrpc": "2.0", **message}) + "\n").encode()
        process.stdin.write(encoded)
        process.stdin.flush()

    def receive(request_id):
        nonlocal stderr_bytes
        while True:
            while b"\n" in pending:
                line, _, rest = pending.partition(b"\n")
                pending[:] = rest
                if not line.strip():
                    continue
                try:
                    message = json.loads(line)
                except (ValueError, UnicodeError) as exc:
                    raise ProbeError("Server stdout was not newline-delimited JSON-RPC") from exc
                if not isinstance(message, dict):
                    raise ProbeError("Server returned a non-object JSON-RPC message")
                if message.get("jsonrpc") != "2.0":
                    raise ProbeError("Server returned an unsupported JSON-RPC version")
                if message.get("id") == request_id:
                    if "error" in message:
                        error = message["error"]
                        code = error.get("code") if isinstance(error, dict) else None
                        raise ProbeError(f"MCP request {request_id} failed (code {code})")
                    if not isinstance(message.get("result"), dict):
                        raise ProbeError("MCP response has no object result")
                    return message["result"]
                # Ignore notifications; this metadata-only client requests no capabilities.
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ProbeError("MCP metadata probe timed out")
            for key, _ in selector.select(remaining):
                chunk = os.read(key.fd, 65536)
                if not chunk:
                    selector.unregister(key.fileobj)
                    if key.data == "stdout":
                        raise ProbeError("MCP stdout closed before the response")
                elif key.data == "stdout":
                    pending.extend(chunk)
                    if len(pending) > 4 * 1024 * 1024:
                        raise ProbeError("MCP response exceeded the 4 MiB probe limit")
                else:
                    # Drain without exposing arbitrary runtime logs or session data.
                    stderr_bytes += len(chunk)

    try:
        send({
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "codex-computer-use-skill-probe", "version": "1.0"},
            },
        })
        initialization = receive(1)
        if initialization.get("protocolVersion") != "2024-11-05":
            raise ProbeError("Server negotiated an unsupported protocol version")
        capabilities = initialization.get("capabilities")
        if not isinstance(capabilities, dict) or not isinstance(capabilities.get("tools"), dict):
            raise ProbeError("Server did not advertise the tools capability")
        send({"method": "notifications/initialized"})
        send({"id": 2, "method": "tools/list", "params": {}})
        inventory = receive(2)
        tool_list = inventory.get("tools")
        if not isinstance(tool_list, list) or not all(
            isinstance(tool, dict) and isinstance(tool.get("name"), str)
            and isinstance(tool.get("inputSchema"), dict)
            and tool["inputSchema"].get("type") == "object"
            for tool in tool_list
        ):
            raise ProbeError("Invalid tools/list result")
        names = sorted(tool["name"] for tool in tool_list)
        if not {"list_apps", "get_app_state"}.issubset(names):
            raise ProbeError("Server lacks the expected app observation tools")
        if inventory.get("nextCursor"):
            raise ProbeError("Tool list is paginated; this bounded probe needs an update")
        result = {
            "status": "metadata_ok",
            "protocol_version": initialization["protocolVersion"],
            "server": initialization.get("serverInfo", {}),
            "tool_count": len(names),
            "tools": names,
            "ui_verified": False,
            "claude_verified": False,
            "runtime_stderr_bytes": stderr_bytes,
        }
        if include_schema:
            result["schemas"] = {
                tool["name"]: tool.get("inputSchema") for tool in tool_list
            }
        return result
    finally:
        selector.close()
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        for pipe in (process.stdin, process.stdout, process.stderr):
            pipe.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--timeout", type=float, default=15, help="Total metadata deadline, 1-60 seconds")
    parser.add_argument("--schema", action="store_true", help="Include live tool input schemas")
    args = parser.parse_args()
    if not 1 <= args.timeout <= 60:
        parser.error("--timeout must be between 1 and 60")
    try:
        print(json.dumps(probe(args.timeout, args.schema), ensure_ascii=False, indent=2))
        return 0
    except (ProbeError, OSError) as exc:
        print(json.dumps({"status": "failed", "error": str(exc), "ui_verified": False}), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
