#!/usr/bin/env python3
"""Generate the journal-parity fixtures from one scenario description.

Source note: hand-authored for D-028 §12 step 3 / §13 P7. The three turns
(text, tool call + result, approval) are declared once in SCENARIO; each
emitter renders them the way its carrier actually would. Writing both sides
from one description is the point — a fixture pair that drifted apart would
prove nothing about parity.

Run from the repo root:
    python3 crates/remuda/tests/fixtures/journal-parity/generate.py

`--out <dir>` writes elsewhere, which is how the drift test compares without
rewriting the checked-in files: regenerating in place made every concurrent
reader of those fixtures race a truncated write.
"""

import argparse
import json
import os
import pathlib

HERE = pathlib.Path(__file__).resolve().parent

# Envelope constants. Volatile per side on purpose: the comparator has to
# strip ids and timestamps rather than get lucky with matching ones.
PRINT = {
    "journal": "obj_01993ab0-1111-7000-8000-000000000001",
    "instance": "ins_01993ab0-1111-7000-8000-000000000001",
    "run": "run_01993ab0-1111-7000-8000-000000000001",
    "host": "hst_01993ab0-1111-7000-8000-000000000001",
    "session": "01993ab0-1111-7000-8000-0000000000aa",
    "driver": "claude-print",
    "channel": "stdout",
    "clock": "2026-09-14T09:00:00.000Z",
}
PTY = {
    "journal": "obj_01993ab0-2222-7000-8000-000000000001",
    "instance": "ins_01993ab0-2222-7000-8000-000000000001",
    "run": "run_01993ab0-2222-7000-8000-000000000001",
    "host": "hst_01993ab0-2222-7000-8000-000000000001",
    "session": "01993ab0-2222-7000-8000-0000000000bb",
    "driver": "claude-pty",
    "channel": "transcript",
    "clock": "2026-09-14T11:30:00.000Z",
}

SCENARIO = {
    "prompt": "Read README.md and tell me the project tagline.",
    "turn1": "The tagline is “unified remote agent runtime”.\n",
    "tool": {"name": "Read", "category": "file-read", "input": {"path": "README.md"}},
    "result": "# Remuda\n\nUnified remote agent runtime.\n",
    "approval": {
        "title": "Write file",
        "field": "Apply the edit to README.md?",
        "options": ["Allow", "Deny"],
    },
    "turn3": "Applied the edit.\n",
    "tokens": {"input": 1840, "output": 96, "total": 1936},
}


def unknown():
    return {"state": "unknown", "reason": "not-emitted", "evidenceEventIds": []}


def known(value):
    return {"state": "known", "value": value}


class Journal:
    """Accumulates observations for one carrier."""

    def __init__(self, side):
        self.side = side
        self.events = []

    def add(self, kind, payload, channel=None, completeness="structured"):
        seq = len(self.events) + 1
        self.events.append(
            {
                "schemaVersion": 1,
                "eventId": f"evt_{self.side['journal'][4:-12]}{seq:012d}",
                "journalId": self.side["journal"],
                "instanceId": self.side["instance"],
                "runId": self.side["run"],
                "hostId": self.side["host"],
                "processGeneration": "1",
                "runGeneration": "1",
                "seq": str(seq),
                "observedAt": self.side["clock"],
                "nativeAt": unknown(),
                "source": {
                    "driverKind": self.side["driver"],
                    "driverVersion": "fixture",
                    "adapterVersion": "0.1.0",
                    "channel": channel or self.side["channel"],
                    "delivery": "live",
                    "nativeSessionId": known(self.side["session"]),
                    "nativeTurnId": unknown(),
                    "nativeAgentId": {"state": "not-applicable"},
                    "nativeItemId": unknown(),
                    "nativeEventId": unknown(),
                    "nativeRequestId": {"type": "none"},
                    "sourceCursor": {"type": "runtime", "ledgerRevision": str(seq)},
                },
                "kind": kind,
                "completeness": completeness,
                "rawRef": None,
                "evidenceEventIds": [],
                "payload": payload,
            }
        )

    def node(self, prefix, index):
        return f"obj_{self.side['journal'][4:-4]}{prefix}{index:03d}"

    def dump(self, name, container, out_dir=HERE):
        path = out_dir / name
        if container == "hub":
            body = {
                "instanceId": self.side["instance"],
                "durableSeq": str(len(self.events)),
                "events": self.events,
            }
        else:  # coordinator jdump / `remuda instance read --source journal`
            body = {
                "instanceId": self.side["instance"],
                "source": "journal",
                "asOfSeq": str(len(self.events)),
                "durableSeq": str(len(self.events)),
                "observations": self.events,
            }
        # Write-then-rename. `write_text` truncates in place, so a reader
        # that opens the file mid-write sees a partial document — which is
        # exactly what happened when this ran alongside the tests that read
        # these fixtures. Rename within a directory is atomic on POSIX, so a
        # reader sees either the old file or the new one.
        tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
        tmp.write_text(json.dumps(body, indent=2) + "\n")
        tmp.replace(path)
        return path


def message(node_id, operation, **fields):
    payload = {
        "nodeId": node_id,
        "revision": fields.pop("revision"),
        "operation": operation,
        "baseRevision": fields.pop("baseRevision", None),
        "messageId": node_id,
        "role": fields.pop("role", "assistant"),
        "phase": fields.pop("phase", "final"),
        "blocks": fields.pop("blocks", []),
        "targetBlock": fields.pop("targetBlock", None),
        "parentToolCallId": None,
        "nativeOrigin": unknown(),
        "status": fields.pop("status", "streaming"),
    }
    assert not fields, fields
    return payload


def text(value):
    return [{"type": "text", "text": value}]


def tool_call(node_id, operation, revision, base, state, **fields):
    return {
        "nodeId": node_id,
        "revision": revision,
        "operation": operation,
        "baseRevision": base,
        "toolCallId": node_id,
        "parentToolCallId": None,
        "toolName": known(SCENARIO["tool"]["name"]),
        "displayTitle": unknown(),
        "category": SCENARIO["tool"]["category"],
        "input": fields.get("input", unknown()),
        "inputTextDelta": fields.get("delta"),
        "state": state,
        "executor": unknown(),
    }


def tool_result(node_id, revision, outcome="succeeded"):
    return {
        "nodeId": node_id,
        "revision": revision,
        "operation": "open",
        "baseRevision": None,
        "toolCallId": node_id,
        "stage": "final",
        "outcome": outcome,
        "blocks": text(SCENARIO["result"]),
        "structuredResult": unknown(),
        "exitCode": known(0),
        "changes": [],
    }


def interaction(interaction_id, side, carrier):
    return {
        "interaction": {
            "id": interaction_id,
            "revision": "1",
            "createdAt": side["clock"],
            "updatedAt": side["clock"],
            "instanceId": side["instance"],
            "runId": side["run"],
            "hostId": side["host"],
            "kind": "approval",
            "requestKey": {
                "native": {
                    "type": "rpc",
                    "valueType": "string",
                    "value": f"perm-{side['driver']}",
                },
                "processGeneration": "1",
                "runGeneration": "1",
                "connectionEpoch": f"epoch_{side['journal'][4:]}",
            },
            "requestVersion": "1",
            "state": "pending",
            "blocking": True,
            "answerable": True,
            "carrier": carrier,
            "request": {
                "kind": "approval",
                "title": SCENARIO["approval"]["title"],
                "fields": [
                    {
                        "id": "decision",
                        "title": SCENARIO["approval"]["field"],
                        "description": None,
                        "input": "single-select",
                        "required": True,
                        "options": [
                            {"id": label.lower(), "label": label}
                            for label in SCENARIO["approval"]["options"]
                        ],
                        "allowFreeText": False,
                        "sensitive": False,
                    }
                ],
            },
            "deadline": unknown(),
            "deadlineSource": "none",
            "answer": unknown(),
            "delivery": "not-sent",
            "resolution": unknown(),
        }
    }


def answered(interaction_id, side, delivery):
    return {
        "interactionId": interaction_id,
        "requestVersion": "1",
        "answerCommandId": f"cmd_{side['journal'][4:]}",
        "actor": {
            "principalId": f"prn_{side['journal'][4:]}",
            "type": "human",
            "deviceId": f"dev_{side['journal'][4:]}",
            "instanceId": None,
        },
        "answerRef": f"obj_{side['journal'][4:]}",
        "delivery": delivery,
    }


def usage(usage_id, accounting, cost):
    return {
        "usageId": usage_id,
        "scope": "turn",
        "scopeId": "turn-1",
        "mode": "snapshot",
        "metricRevision": "1",
        "inputTokens": known(str(SCENARIO["tokens"]["input"])),
        "inputAccounting": "total-including-cache",
        "outputTokens": known(str(SCENARIO["tokens"]["output"])),
        "reasoningTokens": unknown(),
        "cacheReadTokens": unknown(),
        "cacheWriteTokens": unknown(),
        "totalTokens": known(str(SCENARIO["tokens"]["total"])),
        "cost": cost,
        "accounting": accounting,
        "nativeFieldsRef": None,
    }


def session_started(side):
    return {
        "type": "native",
        "topic": "session",
        "nativeName": "session-started",
        "nativeId": known(side["session"]),
        "status": known("ready"),
        "relatedIds": {},
        "dataRef": None,
        "severity": "info",
        "affectsCompletion": False,
    }


def hook(name):
    return {
        "type": "native",
        "topic": "hook",
        "nativeName": name,
        "nativeId": unknown(),
        "status": known("ok"),
        "relatedIds": {},
        "dataRef": None,
        "severity": "info",
        "affectsCompletion": False,
    }


def turn_completed():
    return {
        "type": "native",
        "topic": "turn",
        "nativeName": "turn-completed",
        "nativeId": unknown(),
        "status": known("end_turn"),
        "relatedIds": {},
        "dataRef": None,
        "severity": "info",
        "affectsCompletion": True,
    }


def build_print():
    """claude-print: stream-json token deltas, control-channel approval."""
    j = Journal(PRINT)
    user, reply1, call, reply3 = (j.node("a", i) for i in (1, 2, 3, 4))
    int_id = f"int_{PRINT['journal'][4:]}"

    j.add("lifecycle", session_started(PRINT))
    j.add("message", message(user, "open", revision="1", role="user",
                             phase="input", blocks=text(SCENARIO["prompt"]),
                             status="complete"))

    # Turn 1: assistant text arrives as token deltas.
    j.add("message", message(reply1, "open", revision="1", blocks=[]))
    chunks = ["The tagline is ", "“unified remote ", "agent runtime”.\n"]
    for index, chunk in enumerate(chunks):
        j.add("message", message(reply1, "append", revision=str(index + 2),
                                 baseRevision=str(index + 1), targetBlock=0,
                                 blocks=text(chunk)))
    j.add("message", message(reply1, "close", revision=str(len(chunks) + 2),
                             baseRevision=str(len(chunks) + 1),
                             status="complete"))

    # Turn 2: tool arguments stream as input_json_delta, then complete.
    j.add("tool_call", tool_call(call, "open", "1", None, "proposed", delta='{"path":'))
    j.add("tool_call", tool_call(call, "append", "2", "1", "proposed",
                                 delta='"README.md"}'))
    j.add("tool_call", tool_call(call, "replace", "3", "2", "running",
                                 input=known(SCENARIO["tool"]["input"])))
    j.add("tool_result", tool_result(call, "1"))

    # Turn 3: approval over the stream-json control channel.
    j.add("interaction.requested", interaction(int_id, PRINT, "claude-control"))
    j.add("interaction.answered", answered(int_id, PRINT, "written"))
    j.add("message", message(reply3, "open", revision="1", blocks=[]))
    for index, chunk in enumerate(["Applied ", "the edit.\n"]):
        j.add("message", message(reply3, "append", revision=str(index + 2),
                                 baseRevision=str(index + 1), targetBlock=0,
                                 blocks=text(chunk)))
    j.add("message", message(reply3, "close", revision="4", baseRevision="3",
                             status="complete"))

    # Print is the only carrier that reports real cost today (§12.1).
    j.add("usage", usage(j.node("b", 1), "reported",
                         known({"amount": "0.0121", "currency": "USD"})))
    j.add("lifecycle", turn_completed())
    return j


def build_pty():
    """agent-pty: MessageDisplay lines + transcript, hook approval."""
    j = Journal(PTY)
    user, reply1, call, reply3 = (j.node("a", i) for i in (1, 2, 3, 4))
    int_id = f"int_{PTY['journal'][4:]}"

    j.add("lifecycle", session_started(PTY), channel="hook")
    # Hook lifecycle has no print counterpart; the whitelist drops it.
    j.add("lifecycle", hook("SessionStart"), channel="hook")
    j.add("message", message(user, "open", revision="1", role="user",
                             phase="input", blocks=text(SCENARIO["prompt"]),
                             status="complete"))

    # Turn 1: MessageDisplay flushes whole lines, transcript settles the text.
    j.add("message", message(reply1, "open", revision="1", blocks=[]))
    j.add("message", message(reply1, "append", revision="2", baseRevision="1",
                             targetBlock=0, blocks=text(SCENARIO["turn1"])))
    j.add("message", message(reply1, "close", revision="3", baseRevision="2",
                             status="complete"))

    # Turn 2: no argument deltas — the completed transcript item is the first
    # thing the PTY path sees.
    j.add("lifecycle", hook("PreToolUse"), channel="hook")
    j.add("tool_call", tool_call(call, "open", "1", None, "running",
                                 input=known(SCENARIO["tool"]["input"])))
    j.add("tool_result", tool_result(call, "1"))
    j.add("lifecycle", hook("PostToolUse"), channel="hook")

    # Turn 3: approval arrives on the PermissionRequest hook and is answered
    # synchronously, so delivery is `confirmed` rather than `written`.
    j.add("interaction.requested", interaction(int_id, PTY, "claude-hook"),
          channel="hook")
    j.add("interaction.answered", answered(int_id, PTY, "confirmed"),
          channel="hook")
    j.add("message", message(reply3, "open", revision="1", blocks=[]))
    j.add("message", message(reply3, "append", revision="2", baseRevision="1",
                             targetBlock=0, blocks=text(SCENARIO["turn3"])))
    j.add("message", message(reply3, "close", revision="3", baseRevision="2",
                             status="complete"))

    # No provider cost on this path yet: tokens converted locally (§12.1).
    j.add("usage", usage(j.node("b", 1), "estimated",
                         known({"amount": "0.0118", "currency": "USD"})))
    j.add("lifecycle", turn_completed())
    return j


def build_pty_missing_result():
    """Negative case: the tool result never lands. Must fail the gate."""
    j = build_pty()
    j.events = [event for event in j.events if event["kind"] != "tool_result"]
    for index, event in enumerate(j.events, start=1):
        event["seq"] = str(index)
    return j


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--out",
        type=pathlib.Path,
        default=HERE,
        help="directory to write into; defaults to the fixture directory",
    )
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    written = [
        build_print().dump("print-3turn.json", "hub", args.out),
        build_pty().dump("pty-3turn.json", "jdump", args.out),
        build_pty_missing_result().dump(
            "pty-3turn-missing-tool-result.json", "jdump", args.out
        ),
    ]
    for path in written:
        try:
            print(path.relative_to(HERE.parents[3]))
        except ValueError:
            print(path)


if __name__ == "__main__":
    main()
