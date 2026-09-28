import { describe, expect, it } from "vitest";
import type { Observation } from "../../../types/generated";
import { liveStatus, phraseIsThinking } from "./liveStatus";

function statusEvent(seq: number, tags: Record<string, unknown>, at = "2026-09-16T10:00:00.000Z"): Observation {
  return {
    kind: "lifecycle",
    eventId: `ev_${seq}`,
    journalId: "jrn_1",
    instanceId: "ins_1",
    hostId: "hos_1",
    processGeneration: "1",
    runGeneration: "1",
    runId: "run_1",
    seq: String(seq),
    observedAt: at,
    nativeAt: { state: "not-applicable" },
    source: {
      adapterVersion: "t",
      channel: "pty",
      delivery: "live",
      driverKind: "shell-pty",
      driverVersion: "t",
      nativeAgentId: { state: "not-applicable" },
      nativeEventId: { state: "not-applicable" },
      nativeItemId: { state: "not-applicable" },
      nativeRequestId: { type: "none" },
      nativeSessionId: { state: "known", value: "s" },
      nativeTurnId: { state: "not-applicable" },
      sourceCursor: { type: "runtime", value: { ledgerRevision: String(seq) } },
    },
    completeness: "screen-derived",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
    payload: {
      type: "native",
      topic: "turn",
      nativeName: "live.status",
      nativeId: { state: "not-applicable" },
      status: { state: "not-applicable" },
      relatedIds: { tier: "screen", provision: "emulated", ...tags },
      dataRef: null,
      severity: "info",
      affectsCompletion: false,
    },
  } as unknown as Observation;
}

describe("liveStatus projection", () => {
  it("is null with no screen status events", () => {
    expect(liveStatus([])).toBeNull();
  });

  it("folds the owner screenshot line into every field", () => {
    const status = liveStatus([
      statusEvent(1, {
        liveStatus: "1",
        verb: "Razzmatazzing",
        elapsedScreen: "49m 38s",
        since: "2026-09-16T09:10:22.000Z",
        tokensLabel: "66.0k",
        tokensDown: "66000",
        phrase: "thinking some more with xhigh effort",
      }),
    ])!;
    expect(status.active).toBe(true);
    expect(status.verb).toBe("Razzmatazzing");
    expect(status.elapsedScreen).toBe("49m 38s");
    expect(status.tokensLabel).toBe("66.0k");
    expect(status.tokensDown).toBe(66000);
    expect(status.phrase).toContain("xhigh");
    expect(status.since).toBe("2026-09-16T09:10:22.000Z");
    expect(status.interruptible).toBe(false);
  });

  it("takes the latest reading by seq, not encounter order (gap backfill)", () => {
    const events = [
      statusEvent(3, { liveStatus: "1", verb: "Third", tokensLabel: "9" }),
      statusEvent(1, { liveStatus: "1", verb: "First", tokensLabel: "1" }),
      statusEvent(2, { liveStatus: "1", verb: "Second", tokensLabel: "5" }),
    ];
    expect(liveStatus(events)!.verb).toBe("Third");
  });

  it("a liveStatus:0 clear folds to inactive and drops the fields", () => {
    const events = [
      statusEvent(1, { liveStatus: "1", verb: "Working", tokensLabel: "5" }),
      statusEvent(2, { liveStatus: "0" }),
    ];
    const status = liveStatus(events)!;
    expect(status.active).toBe(false);
    expect(status.verb).toBeNull();
    expect(status.tokensLabel).toBeNull();
  });

  it("survives malformed counts", () => {
    const status = liveStatus([
      statusEvent(1, { liveStatus: "1", verb: "V", tokensDown: "oops" }),
    ])!;
    expect(status.tokensDown).toBeNull();
  });

  it("drops malformed NESTED tags (numbers, objects, null, arrays, booleans) instead of crashing", () => {
    // The r3 incident: relatedIds:{liveStatus:"1", phrase:42} flowed a number
    // into phraseIsThinking -> 42.toLowerCase() and crashed the strip. Every
    // consumed tag is runtime-validated at the projection boundary.
    const malformedValues: unknown[] = [42, { nested: true }, null, ["a", "b"], true];
    const stringTags = ["verb", "phrase", "tokensLabel", "elapsedScreen", "since", "interruptible"];
    for (const value of malformedValues) {
      for (const tag of stringTags) {
        const status = liveStatus([statusEvent(1, { liveStatus: "1", [tag]: value })])!;
        const label = `${tag}=${JSON.stringify(value)}`;
        expect(status.active, label).toBe(true);
        expect(status.verb, label).toBeNull();
        expect(status.phrase, label).toBeNull();
        expect(status.tokensLabel, label).toBeNull();
        expect(status.tokensDown, label).toBeNull();
        expect(status.elapsedScreen, label).toBeNull();
        expect(status.since, label).toBeNull();
        expect(status.interruptible, label).toBe(false);
      }
      // A malformed count never coerces (Number([]) === 0, Number([5]) === 5).
      const count = liveStatus([statusEvent(1, { liveStatus: "1", tokensDown: value })])!;
      expect(count.tokensDown, `tokensDown=${JSON.stringify(value)}`).toBeNull();
    }
  });

  it("treats a malformed relatedIds bag itself as empty, never throws", () => {
    for (const related of [[], "tags", 42, null, true]) {
      const ev = statusEvent(1, {});
      (ev.payload as { relatedIds: unknown }).relatedIds = related;
      const status = liveStatus([ev])!;
      expect(status.active, JSON.stringify(related)).toBe(true);
      expect(status.phrase, JSON.stringify(related)).toBeNull();
      expect(status.tokensDown, JSON.stringify(related)).toBeNull();
    }
  });

  it("a non-string liveStatus tag other than the literal 0 does not clear the spinner", () => {
    expect(liveStatus([statusEvent(1, { liveStatus: 0 })])!.active).toBe(true);
    expect(liveStatus([statusEvent(1, { liveStatus: ["0"] })])!.active).toBe(true);
    expect(liveStatus([statusEvent(1, { liveStatus: "0" })])!.active).toBe(false);
  });

  it("recognises only thinking-class phrases", () => {
    expect(phraseIsThinking("thinking with xhigh effort")).toBe(true);
    expect(phraseIsThinking("thought for 9s")).toBe(true);
    expect(phraseIsThinking("running UserPromptSubmit hook")).toBe(false);
    expect(phraseIsThinking(null)).toBe(false);
    // Untrusted runtime shapes never reach toLowerCase.
    expect(phraseIsThinking(42)).toBe(false);
    expect(phraseIsThinking({ nested: true })).toBe(false);
    expect(phraseIsThinking(["thinking"])).toBe(false);
    expect(phraseIsThinking(undefined)).toBe(false);
  });
});
