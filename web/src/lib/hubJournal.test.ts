import { describe, expect, it } from "vitest";
import type { Id } from "../types/wire";
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
});
