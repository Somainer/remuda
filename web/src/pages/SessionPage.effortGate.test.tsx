/**
 * c-effortread r10 item 8: selectSession must include the composer's effort
 * gate inputs (version gate from the HOST's pinned CLI, refusal, withdrawal,
 * default tier). A host inventory refresh supplying the CLI version then
 * enables the ultracode switch with no instance/observation update.
 */
import { afterEach, expect, it, vi } from "vitest";
import { api } from "../lib/api";
import { mockDb } from "../lib/mock";
import { hubStore } from "../lib/store";
import { selectSession } from "./SessionPage";

afterEach(() => {
  hubStore.logout();
  vi.restoreAllMocks();
});

async function followingInstance() {
  const instance = {
    ...mockDb.instances[0],
    id: "ins_gate_slice",
    journalId: "obj_gate_slice",
    kind: "claude" as const,
    driver: "claude-pty",
    effortName: null,
    effortIndex: null,
    effortUltracode: null,
    effortEffective: null,
    capabilities: { ...mockDb.instances[0].capabilities, binaryVersion: null },
  };
  vi.spyOn(api, "instanceGet").mockResolvedValue(instance as never);
  vi.spyOn(api, "eventsRead").mockResolvedValue({
    events: [],
    durableSeq: "1",
    windowFromSeq: "1",
    reachedAfterSeq: true,
  } as never);
  vi.spyOn(api, "eventsSubscribe").mockImplementation(async () => ({
    subscriptionId: "sub_gate_slice",
    journalId: instance.journalId,
    durableSeq: "1",
    windowFromSeq: "1",
    windowAfterSeq: "",
    reachedAfterSeq: true,
    getReadyState: () => 1,
    snapshot: {
      projectionVersion: "v1",
      projectionEpoch: "epoch_gate",
      asOfSeq: "1",
      instance,
      runs: [],
      commands: [],
      pendingInteractions: [],
      nodes: [],
      history: { complete: true, earliestRetainedSeq: "1" },
    },
    history: [],
  }) as never);
  await hubStore.follow(instance.id);
  return instance;
}

it("a host inventory refresh supplying the CLI version enables the ultracode switch via the selected slice", async () => {
  const instance = await followingInstance();
  const host = mockDb.hosts.find((h) => h.id === instance.hostId)!;
  // Before the inventory refresh the host reports no pinned claude CLI: the
  // gate is unknown and the switch must render disabled.
  vi.spyOn(api, "hostList").mockResolvedValue({
    items: [{ ...host, cli: [] }],
  } as never);
  await hubStore.refreshHosts();
  const before = selectSession(hubStore.getSnapshot(), instance.id);
  expect(before.effortVersionGate).toBe("unknown");

  // The inventory refresh lands with the pinned 2.1.289 CLI — the instance
  // row is untouched.
  vi.spyOn(api, "hostList").mockResolvedValue({
    items: [{ ...host, cli: [{ kind: "claude", installed: true, version: "2.1.289" }] }],
  } as never);
  await hubStore.refreshHosts();
  const after = selectSession(hubStore.getSnapshot(), instance.id);
  expect(after.effortVersionGate).toBe("decoupled");

  // The slice also carries the other composer gate inputs the render used to
  // read off hubStore directly (they now update on their own changes).
  expect(after.effortReadbackWithdrawn).toBe(false);
  expect(after.effortRefusal).toBeNull();
  expect(after.effortDefaultIndex === null || typeof after.effortDefaultIndex === "number").toBe(true);
});
