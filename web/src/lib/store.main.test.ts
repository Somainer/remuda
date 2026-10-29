import { describe, expect, it, vi } from "vitest";
import { api, mapInstance, type LineageRecord } from "./api";
import type { components } from "./api.generated";
import { mockDb } from "./mock";
import { ADDRESS_OWNER_GRANT, hubStore, resolveMain, selectMain, type HubState } from "./store";
import type { Instance } from "../types/instance";
import type { Id } from "../types/wire";

function chapter(id: string, patch: Partial<Instance> = {}): Instance {
  return {
    ...mockDb.instances[0],
    id: id as Id,
    lifecycle: "running",
    connectivity: "connected",
    lineageId: id as Id,
    generation: 1,
    chapterCause: null,
    fencedAt: null,
    grants: [ADDRESS_OWNER_GRANT],
    createdAt: "2026-10-30T08:00:00Z",
    updatedAt: "2026-10-30T08:00:00Z",
    ...patch,
  } as Instance;
}

describe("resolveMain (D-057 §9 Main entry)", () => {
  it("renders nothing without an address-owner lineage", () => {
    const worker = chapter("ins_worker", { grants: ["dispatch"] });
    const plain = chapter("ins_plain", { grants: undefined });
    expect(resolveMain([worker, plain])).toBeNull();
  });

  it("resolves the live current chapter, never a fenced predecessor", () => {
    const first = chapter("ins_g1", {
      lifecycle: "exited",
      fencedAt: "2026-10-30T09:00:00Z",
      updatedAt: "2026-10-30T10:30:00Z",
    });
    const second = chapter("ins_g2", {
      lineageId: "ins_g1" as Id,
      generation: 2,
      chapterCause: "owner-resume",
      updatedAt: "2026-10-30T09:05:00Z",
    });
    const main = resolveMain([first, second]);
    expect(main?.instance.id).toBe("ins_g2");
    expect(main?.live).toBe(true);
    expect(main?.generation).toBe(2);
    expect(main?.lineageId).toBe("ins_g1");
    expect(main?.chapterIds).toEqual(["ins_g1", "ins_g2"]);
  });

  it("with no live chapter, resolves the latest chapter of the most recent lineage", () => {
    const oldLineage = chapter("ins_old", { lifecycle: "exited", updatedAt: "2026-10-29T08:00:00Z" });
    const g1 = chapter("ins_new1", {
      lifecycle: "exited",
      fencedAt: "2026-10-30T09:00:00Z",
      updatedAt: "2026-10-30T09:00:00Z",
    });
    const g2 = chapter("ins_new2", {
      lineageId: "ins_new1" as Id,
      generation: 2,
      lifecycle: "exited",
      updatedAt: "2026-10-30T11:00:00Z",
    });
    const main = resolveMain([oldLineage, g1, g2]);
    expect(main?.instance.id).toBe("ins_new2");
    expect(main?.live).toBe(false);
  });

  it("keeps a contact-lost chapter live: the process may still run (§5 host loss)", () => {
    const lost = chapter("ins_lost", { lifecycle: "exited", lastError: "host-contact-lost" });
    const main = resolveMain([lost]);
    expect(main?.instance.id).toBe("ins_lost");
    expect(main?.live).toBe(true);
  });

  it("selectMain is identity-stable while the resolved row is unchanged", () => {
    const row = chapter("ins_stable");
    const a = selectMain({ instances: [row] } as unknown as HubState);
    const b = selectMain({ instances: [row] } as unknown as HubState);
    expect(b).toBe(a);
  });
});

describe("mapInstance lineage fields", () => {
  it("carries lineageId, generation, chapterCause, fencedAt and grants verbatim", () => {
    const rec = {
      instanceId: "ins_map2",
      hostId: "host-a",
      kind: "claude",
      driver: "claude-sdk",
      lifecycle: "running",
      activity: "idle",
      connectivity: "connected",
      journalId: "jrn_map2",
      durableSeq: "4",
      lineageId: "ins_map1",
      generation: 2,
      chapterCause: "owner-resume",
      fencedAt: null,
      grants: ["address-owner"],
    } as unknown as components["schemas"]["InstanceRecord"];
    const mapped = mapInstance(rec);
    expect(mapped.lineageId).toBe("ins_map1");
    expect(mapped.generation).toBe(2);
    expect(mapped.chapterCause).toBe("owner-resume");
    expect(mapped.fencedAt).toBeUndefined();
    expect(mapped.grants).toEqual(["address-owner"]);
  });
});

describe("refreshLineage across a chapter switch (r2 item 3)", () => {
  function deferred<T>() {
    let resolve!: (value: T) => void;
    const promise = new Promise<T>((r) => (resolve = r));
    return { promise, resolve };
  }
  const record = (generation: number, state: string): LineageRecord => ({
    lineageId: "ins_lsw" as Id,
    state,
    generation,
    chapters: Array.from({ length: generation }, (_, i) => ({
      instanceId: `ins_lsw_g${i + 1}` as Id,
      generation: i + 1,
      createdAt: "2026-10-30T08:00:00Z",
    })),
  });

  it("a post-switch read never joins the in-flight pre-switch GET, and a late pre-switch answer never wins", async () => {
    const before = deferred<LineageRecord>();
    const after = deferred<LineageRecord>();
    const get = vi.spyOn(api, "lineageGet").mockReturnValueOnce(before.promise).mockReturnValueOnce(after.promise);
    const pre = hubStore.refreshLineage("ins_lsw" as Id, "ins_lsw_g1");
    const post = hubStore.refreshLineage("ins_lsw" as Id, "ins_lsw_g2");
    expect(get).toHaveBeenCalledTimes(2);
    after.resolve(record(2, "running"));
    await post;
    before.resolve(record(1, "paused"));
    await pre;
    expect(hubStore.getSnapshot().lineages["ins_lsw"]?.generation).toBe(2);
    expect(hubStore.getSnapshot().lineages["ins_lsw"]?.state).toBe("running");
    get.mockRestore();
  });

  it("the same view still shares one read", async () => {
    const one = deferred<LineageRecord>();
    const get = vi.spyOn(api, "lineageGet").mockReturnValueOnce(one.promise);
    const a = hubStore.refreshLineage("ins_lsw" as Id, "ins_lsw_g3");
    const b = hubStore.refreshLineage("ins_lsw" as Id, "ins_lsw_g3");
    expect(get).toHaveBeenCalledTimes(1);
    one.resolve(record(3, "running"));
    await Promise.all([a, b]);
    get.mockRestore();
  });
});
