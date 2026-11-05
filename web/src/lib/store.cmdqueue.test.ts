/**
 * cb-command-queue store behaviour:
 * - a configure names the generation the last answer produced; a
 *   follow-up posted while this tab's own configure is in flight names none
 *   (a tab's rapid switches never conflict with each other);
 * - a 409 CONFIG_GENERATION_CONFLICT reloads the current values and rolls the
 *   optimistic switch back;
 * - the Hub's `deferred` answer becomes the pending's boundary.
 */
import { afterEach, expect, it, vi } from "vitest";
import type { CommandResult } from "../types/command";
import { api, mapInstance } from "./api";
import type { components } from "./api.generated";
import { HubHttpError } from "./httpError";
import { hubStore } from "./store";
import type { Id } from "../types/wire";

afterEach(() => {
  vi.restoreAllMocks();
});

function result(generation: number, deferred?: "next-turn" | "next-chapter"): CommandResult {
  return {
    command: {
      id: "cmd_cfg",
      revision: "1",
      createdAt: "",
      updatedAt: "",
      commandId: "cmd_cfg",
      actor: { principalId: "", type: "system", deviceId: null, instanceId: "ins_cfg" },
      origin: "ui",
      operation: "instance.configure",
      target: { hostId: "hst_cfg", instanceId: "ins_cfg", runId: null },
      payloadDigest: { algorithm: "sha256", value: "0" },
      state: deferred ? "settled" : "accepted",
      dispatch: "transport-written",
      resolution: "clear",
      ...(deferred ? { deferred: { boundary: deferred } } : {}),
    } as unknown as CommandResult["command"],
    relatedCommandIds: [],
    generation,
  };
}

it("names the generation the last answer produced, and none while its own is in flight", async () => {
  const seen: (number | undefined)[] = [];
  let release!: () => void;
  const gate = new Promise<void>((resolve) => {
    release = resolve;
  });
  let n = 0;
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _mode, extras) => {
    seen.push(extras?.expectedGeneration);
    n += 1;
    if (n === 1) await gate;
    return result(10 + n);
  });
  const first = hubStore.setModel("ins_cfg_order", "opus");
  // A rapid follow-up while the first is in flight: it cannot know the
  // generation the first will produce, so it names none (never a self-409).
  const second = hubStore.setModel("ins_cfg_order", "sonnet");
  await vi.waitFor(() => expect(seen).toHaveLength(2));
  release();
  await Promise.all([first, second]);
  await hubStore.setModel("ins_cfg_order", "haiku");
  expect(seen).toEqual([undefined, undefined, 12]);
});

it("reloads the current values on a generation conflict and rolls back", async () => {
  vi.spyOn(api, "instanceConfigure").mockRejectedValue(
    new HubHttpError(409, "CONFIG_GENERATION_CONFLICT", "runtime config is at generation 4"),
  );
  const refresh = vi.spyOn(hubStore, "refresh").mockResolvedValue(undefined as never);
  await hubStore.setModel("ins_cfg_conflict", "opus");
  expect(refresh).toHaveBeenCalled();
  expect(hubStore.modelPendingOf("ins_cfg_conflict")).toBeNull();
  expect(hubStore.getSnapshot().toast?.text ?? "").toContain("已载入当前值");
});

it("keeps the Hub's deferred boundary on the pending switch", async () => {
  vi.spyOn(api, "instanceConfigure").mockResolvedValue(result(3, "next-turn"));
  await hubStore.setModel("ins_cfg_deferred", "opus");
  expect(hubStore.modelPendingOf("ins_cfg_deferred")?.deferred).toBe("next-turn");
});

/** Boot the store on one Hub instance record, as a fresh page load does. */
async function boot(record: components["schemas"]["InstanceRecord"]) {
  vi.spyOn(api, "hello").mockResolvedValue({} as never);
  vi.spyOn(api, "hasDeviceSession").mockReturnValue(true);
  vi.spyOn(api, "hostList").mockResolvedValue({ items: [], nextCursor: null });
  vi.spyOn(api, "instanceList").mockResolvedValue({ items: [mapInstance(record)], nextCursor: null });
  vi.spyOn(api, "interactionList").mockResolvedValue([]);
  vi.spyOn(api, "deviceList").mockResolvedValue({ items: [] });
  vi.spyOn(hubStore, "startPoll").mockImplementation(() => undefined);
  vi.spyOn(api, "hostWorkspaceSubscribe").mockReturnValue(() => undefined);
  vi.spyOn(api, "settlementSubscribe").mockReturnValue(() => undefined);
  await hubStore.bootstrap();
}

function record(id: string, configureSeq: number): components["schemas"]["InstanceRecord"] {
  return {
    instanceId: id, hostId: "hst_cfg", kind: "claude", driver: "claude-pty", lifecycle: "running",
    activity: "idle", connectivity: "connected", journalId: "obj_cfg", durableSeq: "1",
    lineageId: id, generation: 1, configureSeq,
  };
}

it("maps the Hub's configureSeq, so a fresh page's first configure names it", async () => {
  expect(mapInstance(record("ins_cfg_map", 4)).configureSeq).toBe(4);
  const seen: (number | undefined)[] = [];
  try {
    await boot(record("ins_cfg_fresh", 7));
    vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _mode, extras) => {
      seen.push(extras?.expectedGeneration);
      return result(8);
    });
    await hubStore.setModel("ins_cfg_fresh" as Id, "opus");
    expect(seen).toEqual([7]);
  } finally {
    hubStore.logout();
  }
});

it("re-seeds from the current generation after a conflict", async () => {
  const seen: (number | undefined)[] = [];
  let n = 0;
  vi.spyOn(api, "instanceConfigure").mockImplementation(async (_id, _mode, extras) => {
    seen.push(extras?.expectedGeneration);
    n += 1;
    if (n === 2) {
      throw new HubHttpError(409, "CONFIG_GENERATION_CONFLICT", "runtime config is at generation 9");
    }
    return result(n === 1 ? 5 : 10);
  });
  vi.spyOn(api, "instanceConfig").mockResolvedValue({
    instanceId: "ins_cfg_reseed", generation: 9, model: null, effort: null,
    permissionMode: null, effective: {}, pending: [],
  } as never);
  vi.spyOn(hubStore, "refresh").mockResolvedValue(undefined as never);
  await hubStore.setModel("ins_cfg_reseed", "opus");
  await hubStore.setModel("ins_cfg_reseed", "sonnet");
  await hubStore.setModel("ins_cfg_reseed", "haiku");
  expect(seen).toEqual([undefined, 5, 9]);
});
