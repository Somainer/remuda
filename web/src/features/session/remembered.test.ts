import { describe, expect, it } from "vitest";
import type { Observation } from "../../types/observation";
import type { MemoryWriteItem } from "../memory/api";
import { rememberedByReply, replyNodesByTurn } from "./remembered";

function lifecycle(nativeName: string, seq: number): Observation {
  return {
    kind: "lifecycle",
    seq: String(seq),
    payload: { type: "native", topic: "turn", nativeName },
  } as unknown as Observation;
}

function message(
  seq: number,
  role: "assistant" | "user",
  nodeId: string,
  messageId = nodeId,
): Observation {
  return {
    kind: "message",
    seq: String(seq),
    payload: { role, nodeId, messageId, operation: "close", revision: "1" },
  } as unknown as Observation;
}

function write(overrides: Partial<MemoryWriteItem>): MemoryWriteItem {
  return {
    slug: "pref",
    title: "Terse",
    revision: 1,
    op: "write",
    turnSeq: 10,
    writtenAt: "2026-10-30T00:00:00.000Z",
    ...overrides,
  };
}

describe("rememberedByReply", () => {
  it("hangs a turn's writes under the LAST assistant reply of that turn", () => {
    const events = [
      lifecycle("turn_started", 10),
      message(11, "user", "u1"),
      message(12, "assistant", "a1"),
      message(13, "assistant", "a2"),
      lifecycle("result", 14),
    ];
    expect(replyNodesByTurn(events).get("10")).toBe("a2");
    const placed = rememberedByReply(events, [write({ turnSeq: 10 })]);
    expect([...placed.keys()]).toEqual(["a2"]);
    expect(placed.get("a2")).toEqual([write({ turnSeq: 10 })]);
  });

  it("places nothing without a known turn", () => {
    const events = [message(12, "assistant", "a1")];
    expect(rememberedByReply(events, [write({ turnSeq: 10 })]).size).toBe(0);
    // A write with no turn (owner write or before the first start) has no row.
    expect(rememberedByReply(events, [write({ turnSeq: null })]).size).toBe(0);
  });

  it("keeps one row per slug: a delete in the same turn replaces the write", () => {
    const events = [lifecycle("turn_started", 10), message(12, "assistant", "a1")];
    const writes = [
      write({ slug: "pref", revision: 1, op: "write", turnSeq: 10 }),
      write({ slug: "pref", revision: 2, op: "delete", turnSeq: 10 }),
      write({ slug: "other", revision: 1, op: "write", turnSeq: 10 }),
    ];
    const rows = rememberedByReply(events, writes).get("a1") ?? [];
    expect(rows.map((row) => [row.slug, row.revision, row.op])).toEqual([
      ["other", 1, "write"],
      ["pref", 2, "delete"],
    ]);
  });

  it("keeps separate turns under their own replies", () => {
    const events = [
      lifecycle("turn_started", 10),
      message(12, "assistant", "a1"),
      lifecycle("result", 13),
      lifecycle("turn_started", 20),
      message(22, "assistant", "a2"),
    ];
    const writes = [
      write({ slug: "pref", revision: 1, turnSeq: 10 }),
      write({ slug: "pref", revision: 2, turnSeq: 20, writtenAt: "2026-10-30T00:01:00.000Z" }),
    ];
    const placed = rememberedByReply(events, writes);
    expect(placed.get("a1")?.map((row) => row.revision)).toEqual([1]);
    expect(placed.get("a2")?.map((row) => row.revision)).toEqual([2]);
  });

  it("uses the group's earliest node id across a messageId rename", () => {
    // Revision 1 opens node n1 under m1; the close renames the messageId but
    // the assembler keys the node on the earliest revision's nodeId.
    const opened = message(12, "assistant", "n1", "m1");
    const renamed = message(13, "assistant", "n2", "m1");
    const events = [lifecycle("turn_started", 10), opened, renamed];
    expect(replyNodesByTurn(events).get("10")).toBe("n1");
  });

  it("does not attribute an assistant message after the turn result", () => {
    const events = [
      lifecycle("turn_started", 10),
      message(12, "assistant", "a1"),
      lifecycle("result", 13),
      message(14, "assistant", "a2"),
    ];
    expect(replyNodesByTurn(events).get("10")).toBe("a1");
  });
});
