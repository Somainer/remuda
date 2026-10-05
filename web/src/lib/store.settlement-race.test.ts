import { describe, expect, it } from "vitest";
import type { Interaction } from "../types/interaction";
import { mergeInteractionSnapshots } from "./store";

/**
 * c-cardsettle r2 item 4: an older in-flight interaction poll resolving AFTER
 * the settlement refresh must not resurrect a settled card. The settlement pin
 * is installed against the seq captured at frame receipt (before the newer
 * refresh); mergeInteractionSnapshots must suppress the stale pending copy.
 */

function interaction(id: string, state: Interaction["state"]): Interaction {
  return {
    id: id as Interaction["id"],
    revision: "1",
    createdAt: "2026-10-01T00:00:00.000Z",
    updatedAt: "2026-10-01T00:00:00.000Z",
    instanceId: "ins_1" as Interaction["instanceId"],
    runId: null,
    hostId: "hst_1" as Interaction["hostId"],
    kind: "approval",
    requestKey: {
      native: { type: "none" },
      processGeneration: "1",
      runGeneration: null,
      connectionEpoch: "epoch" as Interaction["requestKey"]["connectionEpoch"],
    },
    requestVersion: "1",
    state,
    blocking: state === "pending",
    answerable: state === "pending",
    carrier: "harness-hook",
    request: {
      kind: "approval",
      title: "Bash",
      description: "ls",
      toolCallId: null,
      actionRef: "obj_1" as never,
      options: [],
      requestedPermissionsRef: null,
      inputDigest: "sha256:00",
    },
    deadline: { state: "not-applicable" },
    deadlineSource: "none",
    answer: { state: "not-applicable" },
    delivery: "not-sent",
    resolution:
      state === "invalidated"
        ? { state: "known", value: { reason: "generation-ended", eventIds: [] } }
        : { state: "not-applicable" },
  } as Interaction;
}

describe("settlement pin vs an older in-flight poll", () => {
  it("suppresses a stale pending copy from a poll older than the settlement pin", () => {
    // reqSeq 1 (old poll) started first and is still in flight.
    // reqSeq 2 is the settlement refresh; the frame pinned the id at seq 2.
    const pin = new Map([["int_1" as never, { seq: 2, confirmedByNewer: false }]]) as never;

    // The OLD poll resolves last, still carrying pending. Outstanding set is
    // empty by then (the newer settlement refresh already finished). The
    // locally known terminal (invalidated) projection survives — the card is
    // dropped from the actionable queue, never flipped back to pending.
    const merged = mergeInteractionSnapshots(
      [interaction("int_1", "pending")],
      [interaction("int_1", "invalidated")],
      pin,
      1,
      new Set<number>(),
    );
    expect(merged.map((row) => row.state)).toEqual(["invalidated"]);
  });

  it("a newer poll reporting invalidated confirms the settlement", () => {
    const pin = new Map([["int_1" as never, { seq: 2, confirmedByNewer: false }]]) as never;
    const merged = mergeInteractionSnapshots(
      [interaction("int_1", "invalidated")],
      [interaction("int_1", "pending")],
      pin,
      3,
      new Set<number>(),
    );
    expect(merged.map((row) => row.state)).toEqual(["invalidated"]);
  });
});
