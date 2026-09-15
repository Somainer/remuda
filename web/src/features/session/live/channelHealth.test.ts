import { describe, expect, it } from "vitest";
import type { Observation } from "../../../types/generated";
import type { NativeRef } from "../../../types/nativeRef";
import { channelHealth, expectedTiersFor, isUnfresh } from "./channelHealth";

function eventAt(channel: Observation["source"]["channel"], at: string, seq = 1): Observation {
  return {
    kind: "opaque",
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
      channel,
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
    completeness: "opaque",
    rawRef: null,
    evidenceEventIds: [],
    schemaVersion: 1,
    payload: { kind: "opaque", nativeType: "x" },
  } as unknown as Observation;
}

const nativeRef = (tier: NativeRef["signalTier"], caps: NativeRef["capabilities"] = []): NativeRef => ({
  hostId: "hos_1",
  nativeStoreId: "obj_1",
  kind: "claude",
  sessionId: { state: "known", value: "s1" },
  transcript: { state: "unknown", reason: "none", evidenceEventIds: [] },
  signalTier: tier,
  capabilities: caps,
});

const NOW = Date.parse("2026-09-16T00:01:00.000Z");

describe("expectedTiersFor", () => {
  it("reads signalTier and capability tiers, skipping none", () => {
    expect(
      expectedTiersFor(
        nativeRef("hook", [
          { name: "completion-native-turn", state: "supported", tier: "file", reasonCode: "" },
          { name: "interactive-approval", state: "supported", tier: "hook", reasonCode: "" },
        ]),
      ).sort(),
    ).toEqual(["file", "hook"]);
    expect(expectedTiersFor(nativeRef("none"))).toEqual([]);
    expect(expectedTiersFor(null)).toEqual([]);
  });
});

describe("channelHealth", () => {
  it("is ok when a record from the tier is fresh", () => {
    const health = channelHealth(
      [eventAt("hook", "2026-09-16T00:00:59.500Z")],
      ["hook"],
      NOW,
    );
    expect(health.get("hook")?.reason).toBe("ok");
  });

  it("is stalled when the tier goes quiet past 3x its cadence", () => {
    // hook cadence is 2 s; the last record is 10 s old.
    const health = channelHealth(
      [eventAt("hook", "2026-09-16T00:00:50.000Z")],
      ["hook"],
      NOW,
    );
    expect(health.get("hook")?.reason).toBe("stalled");
    expect(isUnfresh(health.get("hook"))).toBe(true);
  });

  it("is never-materialised when an expected tier has zero records — not silence", () => {
    // D-4: transcript persistence silently disabled looks exactly like an
    // agent with nothing to say. The health must say so explicitly.
    const health = channelHealth([eventAt("hook", "2026-09-16T00:00:59.000Z")], ["hook", "file"], NOW);
    expect(health.get("hook")?.reason).toBe("ok");
    expect(health.get("file")?.reason).toBe("never-materialised");
    expect(isUnfresh(health.get("file"))).toBe(true);
  });

  it("counts the transcript channel as file-tier evidence", () => {
    const health = channelHealth([eventAt("transcript", "2026-09-16T00:00:59.000Z")], ["file"], NOW);
    expect(health.get("file")?.reason).toBe("ok");
  });

  it("omits tiers that were not expected rather than reporting them disabled", () => {
    const health = channelHealth([], ["hook"], NOW);
    expect(health.has("file")).toBe(false);
  });

  it("uses the newest observation as the last-record anchor", () => {
    const health = channelHealth(
      [
        eventAt("hook", "2026-09-16T00:00:58.000Z", 1),
        eventAt("hook", "2026-09-16T00:00:59.900Z", 2),
      ],
      ["hook"],
      NOW,
    );
    expect(health.get("hook")?.lastRecordAt).toBe("2026-09-16T00:00:59.900Z");
  });
});
