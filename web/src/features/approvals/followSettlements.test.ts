import { afterEach, expect, it, vi } from "vitest";
import { followSettlements, seenViewOf } from "./followSettlements";

afterEach(() => { vi.unstubAllGlobals(); });

it("reads an instance.seen follow event as a seen view and ignores everything else", () => {
  const event = {
    type: "instance.seen", instanceId: "ins_a", unseen: false,
    seenSeq: "12", seenAt: "2026-10-30T10:00:00.000Z", turnEndSeq: "12",
  };
  expect(seenViewOf(event)).toEqual({
    instanceId: "ins_a", unseen: false, seenSeq: "12", seenAt: "2026-10-30T10:00:00.000Z", turnEndSeq: "12",
  });
  // A view before any report or turn end carries only the flag.
  expect(seenViewOf({ type: "instance.seen", instanceId: "ins_a", unseen: false })).toEqual({
    instanceId: "ins_a", unseen: false, seenSeq: undefined, seenAt: undefined, turnEndSeq: undefined,
  });
  // A malformed seq is dropped rather than trusted.
  expect(seenViewOf({ ...event, seenSeq: "-1" })?.seenSeq).toBeUndefined();
  expect(seenViewOf({ ...event, type: "host.updated" })).toBeNull();
  expect(seenViewOf({ ...event, unseen: "no" })).toBeNull();
  expect(seenViewOf({ kind: "message", payload: {} })).toBeNull();
});

it("delivers seen events and settlements from the same global socket", () => {
  const sockets: FakeSocket[] = [];
  class FakeSocket extends EventTarget {
    constructor(_url: string) { super(); sockets.push(this); }
    close() { this.dispatchEvent(new Event("close")); }
    message(value: unknown) { this.dispatchEvent(new MessageEvent("message", { data: JSON.stringify(value) })); }
  }
  vi.stubGlobal("WebSocket", FakeSocket);
  const onSettlement = vi.fn();
  const onSeen = vi.fn();
  const stop = followSettlements("ws://localhost/v1/follow", onSettlement, onSeen);
  const seen = { type: "instance.seen", instanceId: "ins_a", unseen: true, turnEndSeq: "4" };
  sockets[0].message({ type: "event", instanceId: "", seq: "0", event: seen });
  // A journal row on the same unfiltered socket is not a seen event.
  sockets[0].message({ type: "event", instanceId: "ins_a", seq: "4", event: { kind: "lifecycle" } });
  sockets[0].message({ type: "settlement", state: "invalidated", interactionId: "int_a", reason: "generation-ended" });
  expect(onSeen).toHaveBeenCalledOnce();
  expect(onSeen).toHaveBeenCalledWith(expect.objectContaining({ instanceId: "ins_a", unseen: true, turnEndSeq: "4" }));
  expect(onSettlement).toHaveBeenCalledWith("int_a", "generation-ended");
  // Callers that pass no seen handler (older call sites) are unaffected.
  const plain = followSettlements("ws://localhost/v1/follow", onSettlement);
  sockets[1].message({ type: "event", instanceId: "", seq: "0", event: seen });
  stop();
  plain();
});
