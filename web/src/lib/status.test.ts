import { describe, expect, it } from "vitest";
import type { Instance } from "../types/instance";
import { known, unknownKnowledge, type Id } from "../types/wire";
import { printCapabilities } from "./capabilities";
import { nativeShort, projectStatus, uiMode } from "./status";

function instance(patch: Partial<Instance> & Pick<Instance, "lifecycle" | "activity" | "connectivity">): Instance {
  return {
    id: "ins_test" as Id,
    revision: "1",
    createdAt: "2026-09-12T00:00:00.000Z",
    updatedAt: "2026-09-12T00:00:00.000Z",
    hostId: "hst" as Id,
    workspaceId: "wsp" as Id,
    kind: "claude",
    driver: "claude-print",
    ownership: "managed",
    nativeRef: {
      hostId: "hst" as Id,
      nativeStoreId: "obj" as Id,
      kind: "claude",
      sessionId: known("a324ee05-e077-483b-aaae-8ac5b2075d82"),
      transcript: unknownKnowledge("none"),
      claude: { sessionId: "a324ee05-e077-483b-aaae-8ac5b2075d82" },
    },
    processRef: { processGeneration: "1", processIdentity: unknownKnowledge("none"), connectionEpoch: "epoch" as Id },
    specRevision: "1",
    launchId: unknownKnowledge("none"),
    capabilities: printCapabilities(),
    ownerFence: "1",
    activeRunIds: [],
    parent: null,
    journalId: "obj_j" as Id,
    durableSeq: "1",
    exit: { state: "not-applicable" },
    ...patch,
  };
}

describe("projectStatus lifecycle × activity × connectivity", () => {
  it("blocked wins over working when waiting-interaction", () => {
    expect(
      projectStatus(instance({ lifecycle: "ready", activity: known("waiting-interaction"), connectivity: "connected" })),
    ).toBe("blocked");
  });

  it("working from activity=working", () => {
    expect(projectStatus(instance({ lifecycle: "ready", activity: known("working"), connectivity: "connected" }))).toBe("working");
  });

  it("starting from requested/preparing/starting even if activity idle", () => {
    expect(projectStatus(instance({ lifecycle: "requested", activity: known("idle"), connectivity: "connected" }))).toBe("starting");
    expect(projectStatus(instance({ lifecycle: "preparing", activity: known("idle"), connectivity: "connected" }))).toBe("starting");
    expect(projectStatus(instance({ lifecycle: "starting", activity: known("idle"), connectivity: "connected" }))).toBe("starting");
  });

  it("idle is ready/running + idle, including --bg done-but-alive", () => {
    expect(projectStatus(instance({ lifecycle: "ready", activity: known("idle"), connectivity: "connected" }))).toBe("idle");
    expect(projectStatus(instance({ lifecycle: "running", activity: known("idle"), connectivity: "connected" }))).toBe("idle");
  });

  it("running with unknown activity is working, not idle", () => {
    expect(
      projectStatus(instance({ lifecycle: "running", activity: unknownKnowledge("unknown"), connectivity: "connected" })),
    ).toBe("working");
  });

  it("exited from exited/failed/closing", () => {
    expect(projectStatus(instance({ lifecycle: "exited", activity: known("idle"), connectivity: "connected" }))).toBe("exited");
    expect(projectStatus(instance({ lifecycle: "failed", activity: known("idle"), connectivity: "connected" }))).toBe("exited");
    expect(projectStatus(instance({ lifecycle: "closing", activity: known("idle"), connectivity: "connected" }))).toBe("exited");
  });

  it("unknown when lifecycle unknown/reconciling or connectivity is not connected", () => {
    expect(projectStatus(instance({ lifecycle: "unknown", activity: known("idle"), connectivity: "connected" }))).toBe("unknown");
    expect(projectStatus(instance({ lifecycle: "reconciling", activity: known("idle"), connectivity: "connected" }))).toBe("unknown");
    expect(projectStatus(instance({ lifecycle: "ready", activity: known("idle"), connectivity: "disconnected" }))).toBe("unknown");
    expect(projectStatus(instance({ lifecycle: "ready", activity: known("working"), connectivity: "reconciling" }))).toBe("unknown");
  });

  it("does not paint disconnected as idle or exited", () => {
    const status = projectStatus(instance({ lifecycle: "ready", activity: known("idle"), connectivity: "disconnected" }));
    expect(status).not.toBe("idle");
    expect(status).not.toBe("exited");
    expect(status).toBe("unknown");
  });
});

describe("uiMode / nativeShort", () => {
  it("print is structured-only", () => {
    expect(uiMode(instance({ lifecycle: "ready", activity: known("idle"), connectivity: "connected" }))).toBe("structured-only");
  });

  it("shortens known native session ids", () => {
    expect(nativeShort(instance({ lifecycle: "ready", activity: known("idle"), connectivity: "connected" }))).toBe("a324ee05");
  });
});
