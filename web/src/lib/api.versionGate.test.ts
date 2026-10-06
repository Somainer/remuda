/**
 * D-056 round 2: the version gate must run on the REPORTED Claude Code
 * binaryVersion, never a fabricated one. The live mapper stamps the
 * per-instance snapshot's version; an absent/blank version parses to the
 * "unknown" gate so the switch is disabled with a named reason, rather than
 * the static matrix silently claiming decoupled 2.1.289.
 */
import { describe, expect, it } from "vitest";
import { mapInstance } from "./api";
import { claudeVersionGate } from "../features/session/effort";
import type { components } from "./api.generated";

type InstanceRecord = components["schemas"]["InstanceRecord"];

function wireRecord(over: Partial<InstanceRecord> = {}): InstanceRecord {
  return {
    instanceId: "ins_ver_1",
    hostId: "hst_ver_1",
    workspaceId: "wsp_ver_1",
    kind: "claude",
    driver: "shell-pty",
    lifecycle: "ready",
    activity: "idle",
    connectivity: "connected",
    createdAt: "2026-10-06T00:00:00Z",
    updatedAt: "2026-10-06T00:00:00Z",
    title: "ver",
    journalId: "obj_ver_1",
    durableSeq: "1",
    ...over,
  } as InstanceRecord;
}

function gateFor(rec: InstanceRecord) {
  return claudeVersionGate(mapInstance(rec).capabilities?.binaryVersion);
}

describe("mapInstance version gate uses the reported binaryVersion", () => {
  it("couples 2.1.277", () => {
    expect(gateFor(wireRecord({ capabilities: { binaryVersion: "2.1.277" } } as never))).toBe("coupled");
  });

  it("legacys 2.1.150 (switch disabled)", () => {
    expect(gateFor(wireRecord({ capabilities: { binaryVersion: "2.1.150" } } as never))).toBe("legacy");
  });

  it("decouples 2.1.289", () => {
    expect(gateFor(wireRecord({ capabilities: { binaryVersion: "2.1.289" } } as never))).toBe("decoupled");
  });

  it("is unknown (not decoupled) when no snapshot version is reported", () => {
    expect(gateFor(wireRecord())).toBe("unknown");
    expect(gateFor(wireRecord({ capabilities: {} } as never))).toBe("unknown");
    expect(gateFor(wireRecord({ capabilities: { binaryVersion: "   " } } as never))).toBe("unknown");
  });

  it("is unknown for an unparsable version", () => {
    expect(gateFor(wireRecord({ capabilities: { binaryVersion: "banana" } } as never))).toBe("unknown");
  });

  it("maps the reported version through verbatim without inventing 2.1.289", () => {
    const mapped = mapInstance(wireRecord({ capabilities: { binaryVersion: "2.1.277" } } as never));
    expect(mapped.capabilities?.binaryVersion).toBe("2.1.277");
  });
});
