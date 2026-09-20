import { afterEach, describe, expect, it } from "vitest";
import type { Id } from "../../../types/wire";
import type { Observation } from "../../../types/observation";
import { promptHistory } from "./promptHistory";

function userMessage(seq: number, messageId: string, text: string, origin?: string): Observation {
  return {
    seq: String(seq),
    eventId: `evt_${seq}` as Id,
    kind: "message",
    payload: {
      nodeId: `node_${messageId}` as Id,
      messageId: messageId as Id,
      revision: "1",
      operation: "open",
      baseRevision: null,
      role: "user",
      phase: "input",
      blocks: [{ type: "text", text }],
      targetBlock: null,
      parentToolCallId: null,
      status: "complete",
      ...(origin ? { origin } : {}),
    },
    source: { channel: "stdout" },
  } as unknown as Observation;
}

describe("promptHistory", () => {
  afterEach(() => localStorage.clear());

  it("returns human user prompts newest first", () => {
    const events = [
      userMessage(1, "m1", "first prompt"),
      userMessage(2, "m2", "second prompt"),
    ];
    expect(promptHistory("ins_a", events)).toEqual(["second prompt", "first prompt"]);
  });

  it("excludes skill bodies and hook-context filed under role=user", () => {
    const events = [
      userMessage(1, "m1", "real prompt"),
      userMessage(2, "m2", "<skill>instructions</skill>", "injected-skill"),
      userMessage(3, "m3", "hook context line", "hook-context"),
    ];
    expect(promptHistory("ins_a", events)).toEqual(["real prompt"]);
  });

  it("dedupes repeated prompts and trims whitespace", () => {
    const events = [
      userMessage(1, "m1", "repeat"),
      userMessage(2, "m2", "  repeat  "),
    ];
    expect(promptHistory("ins_a", events)).toEqual(["repeat"]);
  });

  it("puts the unsent composer draft first without touching or sending it", () => {
    localStorage.setItem("runtime.draft.ins_a", "half typed");
    const events = [userMessage(1, "m1", "sent already")];
    expect(promptHistory("ins_a", events)).toEqual(["half typed", "sent already"]);
    // Reading the draft is non-destructive: the composer still owns it.
    expect(localStorage.getItem("runtime.draft.ins_a")).toBe("half typed");
  });

  it("has no entries for an instance with no history", () => {
    expect(promptHistory("ins_empty", [])).toEqual([]);
  });
});
