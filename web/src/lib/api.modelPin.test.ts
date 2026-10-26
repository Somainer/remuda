/**
 * model-pin-1 §5.4 end-to-end wire mapping: the durable launch divergence the
 * Hub projects onto the instance record must survive the public API →
 * `mapInstance` boundary, so run details can render it independently of the
 * bounded journal tail.
 */
import { describe, expect, it } from "vitest";
import { mapInstance } from "./api";
import type { components } from "./api.generated";

type InstanceRecord = components["schemas"]["InstanceRecord"];

/** Minimal wire InstanceRecord; only the fields the mapper branches on. */
function wireRecord(over: Partial<InstanceRecord> = {}): InstanceRecord {
  return {
    instanceId: "ins_wire_1",
    hostId: "hst_wire_1",
    workspaceId: "wsp_wire_1",
    kind: "claude",
    driver: "shell-pty",
    lifecycle: "ready",
    activity: "idle",
    connectivity: "connected",
    createdAt: "2026-09-24T00:00:00Z",
    updatedAt: "2026-09-24T00:00:00Z",
    title: "wire",
    journalId: "obj_wire_1",
    durableSeq: "9",
    ...over,
  } as InstanceRecord;
}

describe("mapInstance modelPinMismatches", () => {
  it("carries the projected launch divergence through the wire mapper", () => {
    const rec = wireRecord({
      modelPinMismatches: [
        {
          requested: "acme_hub/model_x_o50[1m]",
          observed: "acme_hub/model_x_o48[1m]",
          observedAt: "2026-09-24T00:00:00.000Z",
        },
      ],
    });
    const mapped = mapInstance(rec);
    expect(mapped.modelPinMismatches).toEqual([
      {
        requested: "acme_hub/model_x_o50[1m]",
        observed: "acme_hub/model_x_o48[1m]",
        observedAt: "2026-09-24T00:00:00.000Z",
      },
    ]);
  });

  it("preserves a durable effortUltracode flag on reload even without an effort event", () => {
    // Round 2 item 4: the flag rides the instance record; when the journal
    // tail no longer contains the effort event, the mapped instance must
    // still carry effortUltracode so the switch survives reload.
    const mapped = mapInstance(
      wireRecord({ effortName: "xhigh", effortIndex: 3, effortUltracode: true }),
    );
    expect(mapped.effortName).toBe("xhigh");
    expect(mapped.effortIndex).toBe(3);
    expect(mapped.effortUltracode).toBe(true);
    expect(mapped.effortEffective).toBeNull(); // no event in the tail
  });

  it("maps an absent effortUltracode flag to false (never undefined)", () => {
    const mapped = mapInstance(wireRecord());
    expect(mapped.effortUltracode).toBe(false);
  });

  it("maps an absent/null projection to null, and drops malformed rows", () => {
    expect(mapInstance(wireRecord()).modelPinMismatches).toBeNull();
    expect(mapInstance(wireRecord({ modelPinMismatches: null })).modelPinMismatches).toBeNull();
    const mapped = mapInstance(
      wireRecord({
        modelPinMismatches: [
          // @ts-expect-error deliberately malformed (missing observed/observedAt)
          { requested: "A" },
          { requested: "A2", observed: "B2", observedAt: "t" },
        ],
      }),
    );
    expect(mapped.modelPinMismatches).toEqual([
      { requested: "A2", observed: "B2", observedAt: "t" },
    ]);
  });
});
