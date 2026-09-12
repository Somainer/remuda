import { describe, expect, it } from "vitest";
import type { Id } from "../../types/wire";
import { countFleet, type FleetMember } from "./model";

function member(status: FleetMember["status"], online: boolean): FleetMember {
  return {
    instanceId: "ins_1" as Id,
    hostId: (online ? "hst_on" : "hst_off") as Id,
    hostLabel: "x",
    title: "t",
    status,
    hostOnline: online,
  };
}

describe("fleet aggregate", () => {
  it("counts status cards and offline hosts", () => {
    const counts = countFleet([
      member("working", true),
      member("idle", true),
      member("blocked", true),
      member("unknown", false),
      member("cancelled", false),
    ]);
    expect(counts.working).toBe(1);
    expect(counts.idle).toBe(1);
    expect(counts.blocked).toBe(1);
    expect(counts.unknown).toBe(1);
    expect(counts.cancelled).toBe(1);
    expect(counts.offlineHosts).toBe(1);
  });
});
