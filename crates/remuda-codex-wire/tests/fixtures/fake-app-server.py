#!/usr/bin/env python3
"""Minimal Codex app-server stdio JSONL stub.

Source: authored for remuda-codex-wire process tests. Speaks the same
no-jsonrpc NDJSON framing as `codex app-server --listen stdio://`.
Extra argv (`app-server --listen stdio://`) is ignored.
"""
from __future__ import annotations

import json
import sys

initialized = False
thread = {
    "id": "thread-test-1",
    "sessionId": "thread-test-1",
    "forkedFromId": None,
    "parentThreadId": None,
    "preview": "",
    "ephemeral": False,
    "historyMode": "paginated",
    "modelProvider": "openai",
    "model": "gpt-5.6-sol",
    "reasoningEffort": "low",
    "createdAt": 1,
    "updatedAt": 1,
    "recencyAt": 1,
    "status": {"type": "idle"},
    "path": "/tmp/fake-rollout.jsonl",
    "cwd": "/tmp",
    "cliVersion": "0.0.0-fake",
    "originator": "remuda",
    "source": "vscode",
    "canAcceptDirectInput": True,
    "name": None,
    "turns": [],
    "environments": [
        {
            "environmentId": "local",
            "cwd": "/tmp",
            "runtimeWorkspaceRoots": ["/tmp"],
        }
    ],
}
active_turn_id = None
approval_id = 9000


def send(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def result(rid, payload) -> None:
    send({"id": rid, "result": payload})


def error(rid, code: int, message: str) -> None:
    send({"id": rid, "error": {"code": code, "message": message}})


def notify(method: str, params) -> None:
    send({"method": method, "params": params})


for raw in sys.stdin:
    line = raw.strip()
    if not line or line.startswith("#"):
        continue
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    method = msg.get("method")
    rid = msg.get("id")
    params = msg.get("params") or {}

    if method == "initialized":
        continue

    if method != "initialize" and not initialized:
        if rid is not None:
            error(rid, -32600, "Not initialized")
        continue

    if method == "initialize":
        if initialized:
            error(rid, -32600, "Already initialized")
            continue
        initialized = True
        result(
            rid,
            {
                "userAgent": "remuda/0.0.0-fake",
                "codexHome": "/tmp/fake-codex-home",
                "platformFamily": "unix",
                "platformOs": "macos",
            },
        )
        notify(
            "remoteControl/status/changed",
            {"status": "disabled", "serverName": "fake", "installationId": "fake", "environmentId": None},
        )
        continue

    if method == "model/list":
        result(
            rid,
            {
                "data": [
                    {
                        "id": "gpt-5.6-sol",
                        "model": "gpt-5.6-sol",
                        "displayName": "GPT-5.6-Sol",
                        "description": "fake",
                        "hidden": False,
                        "isDefault": True,
                        "supportedReasoningEfforts": [
                            {"reasoningEffort": "low", "description": "fast"}
                        ],
                        "defaultReasoningEffort": "low",
                        "inputModalities": ["text"],
                        "multiAgentVersion": "v2",
                    }
                ],
                "nextCursor": None,
            },
        )
        continue

    if method == "thread/start":
        thread["model"] = params.get("model") or thread["model"]
        thread["cwd"] = params.get("cwd") or thread["cwd"]
        thread["preview"] = ""
        result(
            rid,
            {
                "thread": thread,
                "model": thread["model"],
                "modelProvider": "openai",
                "serviceTier": "default",
                "cwd": thread["cwd"],
                "runtimeWorkspaceRoots": [thread["cwd"]],
                "instructionSources": [],
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "sandbox": {
                    "type": "workspaceWrite",
                    "writableRoots": [],
                    "networkAccess": False,
                    "excludeTmpdirEnvVar": False,
                    "excludeSlashTmp": False,
                },
                "reasoningEffort": "low",
            },
        )
        notify("thread/started", {"thread": thread})
        continue

    if method == "thread/list":
        result(rid, {"data": [thread], "nextCursor": None, "backwardsCursor": None})
        continue

    if method == "thread/read":
        snapshot = dict(thread)
        if not params.get("includeTurns"):
            snapshot["turns"] = []
        result(rid, {"thread": snapshot})
        continue

    if method == "thread/resume":
        thread["status"] = {"type": "idle"}
        result(
            rid,
            {
                "thread": thread,
                "model": thread["model"],
                "modelProvider": "openai",
                "cwd": thread["cwd"],
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "sandbox": {
                    "type": "workspaceWrite",
                    "writableRoots": [],
                    "networkAccess": False,
                    "excludeTmpdirEnvVar": False,
                    "excludeSlashTmp": False,
                },
            },
        )
        continue

    if method == "turn/start":
        texts = [
            block.get("text", "")
            for block in params.get("input") or []
            if isinstance(block, dict)
        ]
        prompt = texts[0] if texts else ""
        turn_id = "turn-test-1"
        active_turn_id = turn_id
        turn = {
            "id": turn_id,
            "items": [],
            "itemsView": "notLoaded",
            "status": "inProgress",
            "error": None,
            "startedAt": 1,
            "completedAt": None,
            "durationMs": None,
        }
        result(rid, {"turn": turn})
        notify(
            "thread/status/changed",
            {"threadId": thread["id"], "status": {"type": "active", "activeFlags": []}},
        )
        notify("turn/started", {"threadId": thread["id"], "turn": turn})
        user_item = {
            "type": "userMessage",
            "id": "item-user-1",
            "clientId": None,
            "content": [{"type": "text", "text": prompt, "text_elements": []}],
        }
        notify(
            "item/started",
            {
                "item": user_item,
                "threadId": thread["id"],
                "turnId": turn_id,
                "startedAtMs": 1,
            },
        )
        notify(
            "item/completed",
            {
                "item": user_item,
                "threadId": thread["id"],
                "turnId": turn_id,
                "completedAtMs": 1,
            },
        )
        if prompt == "SLOW":
            continue
        if prompt == "NEED_APPROVAL":
            approval_id += 1
            send(
                {
                    "id": approval_id,
                    "method": "item/commandExecution/requestApproval",
                    "params": {
                        "itemId": "item-cmd-1",
                        "threadId": thread["id"],
                        "turnId": turn_id,
                        "kind": "command",
                        "command": "echo hi",
                        "cwd": "/tmp",
                        "startedAtMs": 1,
                    },
                }
            )
            continue
        agent_item = {
            "type": "agentMessage",
            "id": "item-agent-1",
            "text": "OK",
            "phase": "final_answer",
            "memoryCitation": None,
            "delivery": None,
            "questions": None,
        }
        notify(
            "item/started",
            {
                "item": {**agent_item, "text": ""},
                "threadId": thread["id"],
                "turnId": turn_id,
                "startedAtMs": 2,
            },
        )
        notify(
            "item/agentMessage/delta",
            {
                "threadId": thread["id"],
                "turnId": turn_id,
                "itemId": "item-agent-1",
                "delta": "OK",
            },
        )
        notify(
            "item/completed",
            {
                "item": agent_item,
                "threadId": thread["id"],
                "turnId": turn_id,
                "completedAtMs": 3,
            },
        )
        completed = {
            **turn,
            "status": "completed",
            "items": [agent_item],
            "itemsView": "summary",
            "completedAt": 2,
            "durationMs": 10,
        }
        notify("thread/status/changed", {"threadId": thread["id"], "status": {"type": "idle"}})
        notify("turn/completed", {"threadId": thread["id"], "turn": completed})
        active_turn_id = None
        continue

    if method == "turn/steer":
        if not active_turn_id:
            error(rid, -32600, "no active turn to steer")
            continue
        if params.get("expectedTurnId") != active_turn_id:
            error(rid, -32600, "no active turn to steer")
            continue
        result(rid, {"turnId": active_turn_id})
        continue

    if method == "turn/interrupt":
        if not active_turn_id:
            error(rid, -32600, "no active turn")
            continue
        result(rid, {})
        completed = {
            "id": active_turn_id,
            "items": [],
            "itemsView": "notLoaded",
            "status": "interrupted",
            "error": None,
            "startedAt": 1,
            "completedAt": 1,
            "durationMs": 1,
        }
        notify("thread/status/changed", {"threadId": thread["id"], "status": {"type": "idle"}})
        notify("turn/completed", {"threadId": thread["id"], "turn": completed})
        active_turn_id = None
        continue

    if rid is not None and method is None and "result" in msg:
        # client reply to a server request
        turn_id = active_turn_id or "turn-test-1"
        agent_item = {
            "type": "agentMessage",
            "id": "item-agent-1",
            "text": "OK",
            "phase": "final_answer",
        }
        completed = {
            "id": turn_id,
            "items": [agent_item],
            "itemsView": "summary",
            "status": "completed",
            "error": None,
            "startedAt": 1,
            "completedAt": 2,
            "durationMs": 10,
        }
        notify("thread/status/changed", {"threadId": thread["id"], "status": {"type": "idle"}})
        notify("turn/completed", {"threadId": thread["id"], "turn": completed})
        active_turn_id = None
        continue

    if rid is not None:
        error(rid, -32601, "Method not found")
