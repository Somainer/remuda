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
import { api } from "./api";
import { HubHttpError } from "./httpError";
import { hubStore } from "./store";

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
