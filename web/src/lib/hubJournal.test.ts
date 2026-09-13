import { describe, expect, it } from "vitest";
import { assembleTranscript } from "../features/session/assemble";
import type { MessagePayload } from "../types/observation";
import type { Id } from "../types/wire";
import { known } from "../types/wire";
import { coerceObservation, coerceObservationList } from "./hubJournal";

describe("coerceObservation", () => {
  it("unwraps Hub journal records", () => {
    const obs = coerceObservation(
      {
        instanceId: "ins_1",
        seq: 2,
        eventId: "evt_2",
        event: { kind: "message", payload: { role: "user", text: "hi" }, seq: "2" },
      },
      "obj_j" as Id,
      "ins_1" as Id,
    );
    expect(obs).not.toBeNull();
    expect(obs!.kind).toBe("message");
    expect(obs!.seq).toBe("2");
    expect((obs!.payload as { text?: string }).text).toBe("hi");
  });

  it("accepts a bare observation", () => {
    const obs = coerceObservation(
      { kind: "message", payload: { text: "bare" }, seq: "3", eventId: "evt_3" },
      "obj_j" as Id,
      "ins_1" as Id,
    );
    expect(obs).not.toBeNull();
    expect(obs!.seq).toBe("3");
    expect(coerceObservationList([{ event: { kind: "opaque", payload: {} } }], "obj_j" as Id, "ins_1" as Id)).toHaveLength(1);
  });

  it("renders fake Node user and assistant messages from history and live replay", () => {
    const rows = [
      { seq: "1", eventId: "evt_user", event: { kind: "message", completeness: "structured", payload: { role: "user", text: "hello from web hub" } } },
      { seq: "2", eventId: "evt_assistant", event: { kind: "message", completeness: "structured", payload: { role: "assistant", text: "echo: hello from web hub" } } },
    ];
    const history = coerceObservationList(rows, "obj_j", "ins_1");
    const live = rows.map((row) => coerceObservation({
      type: "event", seq: row.seq,
      event: { ...row.event, eventId: row.eventId, seq: row.seq },
    }, "obj_j", "ins_1")!);
    const expected = [
      { type: "message", id: "evt_user", role: "user", text: "hello from web hub", status: "complete" },
      { type: "message", id: "evt_assistant", role: "assistant", text: "echo: hello from web hub", status: "complete" },
    ];
    expect(assembleTranscript(history)).toEqual(expected);
    expect(assembleTranscript(live)).toEqual(expected);
    expect(assembleTranscript([...history, ...live])).toEqual(expected);
    expect(assembleTranscript([...live, ...history])).toEqual(expected);
  });

  it.each(["open", "append", "replace", "close"] as const)("preserves typed %s payload fields and explicit empty blocks", (operation) => {
    const payload: MessagePayload = {
      messageId: "msg_stream", nodeId: "node_stream", role: "assistant", phase: "commentary",
      revision: "18446744073709551615", baseRevision: "18446744073709551614", operation,
      status: "streaming", blocks: [], targetBlock: 0, parentToolCallId: "call_parent",
      nativeOrigin: known("native-assistant"), text: "legacy text must not replace explicit empty blocks",
    };
    const observation = coerceObservation({ kind: "message", payload, eventId: "evt_typed", seq: "3" }, "obj_j", "ins_1");
    expect(observation!.payload).toEqual(payload);
    expect(assembleTranscript([observation!])).toMatchObject([{ id: "msg_stream", text: "", status: "streaming" }]);
  });

  it("opens unknown operations and recovers missing identities without aliasing unrelated messages", () => {
    const events = coerceObservationList([
      { kind: "message", payload: { nodeId: "node_1", operation: "unknown", text: "first" }, eventId: "evt_1", seq: "1" },
      { kind: "message", payload: { messageId: "message_2", nodeId: "", revision: "invalid", text: "second" }, eventId: "evt_2", seq: "2" },
    ], "obj_j", "ins_1");
    expect(events[0].payload).toMatchObject({ messageId: "node_1", nodeId: "node_1", operation: "open", revision: "1" });
    expect(events[1].payload).toMatchObject({ messageId: "message_2", nodeId: "message_2", revision: "1" });
    expect(assembleTranscript(events)).toMatchObject([{ id: "node_1", text: "first" }, { id: "message_2", text: "second" }]);
  });
});
