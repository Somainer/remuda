import { describe, expect, it } from "vitest";
import { mockDb } from "../../lib/mock";
import { known } from "../../types/wire";
import type { Instance } from "../../types/instance";
import { spaceKey, type Space } from "../spaces/store";
import { rankQuickFind, type QuickFindInput, type QuickFindSpace } from "./quickFindSearch";

/**
 * Fixture notes:
 * - hosts are deliberately distinct so a host-name query can hit only the host.
 * - `updatedAt` is set explicitly so tie-breaks are deterministic.
 */
function instance(id: string, patch: Partial<Instance> = {}): Instance {
  return {
    ...mockDb.instances[0],
    id,
    hostId: "host-a",
    workspaceId: "wsp-a",
    lifecycle: "ready",
    connectivity: "connected",
    activity: known("idle"),
    createdAt: "2026-09-01T00:00:00Z",
    updatedAt: "2026-09-10T00:00:00Z",
    ...patch,
  };
}

function space(
  id: string,
  name: string,
  hostId: string,
  workspaceId: string,
  instances: Instance[],
): Space {
  return { id, name, hostId, workspaceId, instances, liveCount: instances.length, blockedCount: 0 };
}

const zebraHostA = space(spaceKey("host-a", "wsp-a"), "alpha", "host-a", "wsp-a", [
  instance("ins_aaaa1111-0000-7000-8000-000000000001", { updatedAt: "2026-09-10T00:00:00Z" }),
]);
const betaHostB = space(spaceKey("host-b", "wsp-b"), "beta", "host-b", "wsp-b", [
  instance("ins_bbbb2222-0000-7000-8000-000000000002", {
    hostId: "host-b",
    workspaceId: "wsp-b",
    updatedAt: "2026-09-09T00:00:00Z",
  }),
]);
const gammaHostC = space(spaceKey("host-c", "wsp-c"), "gamma", "host-c", "wsp-c", [
  instance("ins_cccc3333-0000-7000-8000-000000000003", {
    hostId: "host-c",
    workspaceId: "wsp-c",
    updatedAt: "2026-09-08T00:00:00Z",
  }),
]);

const TITLES: Record<string, string> = {
  [zebraHostA.instances[0].id]: "zzz-title-match launch review",
  [betaHostB.instances[0].id]: "ordinary session",
  [gammaHostC.instances[0].id]: "another ordinary session",
};

const HOSTS: Record<string, string> = { "host-a": "demo-node-1", "host-b": "zzz-host-machine", "host-c": "gamma-host" };

function run(query: string, patch: Partial<QuickFindInput> = {}) {
  const spaces: QuickFindSpace[] = patch.spaces ?? [zebraHostA, betaHostB, gammaHostC];
  return rankQuickFind({
    spaces,
    query,
    titleOf: (id) => TITLES[id] ?? "会话",
    hostNameOf: (hostId) => (hostId ? (HOSTS[hostId] ?? hostId) : ""),
    connection: "live",
    ...patch,
  });
}

function ids(query: string, patch?: Partial<QuickFindInput>): string[] {
  return run(query, patch).hits.map((hit) => hit.instance.id);
}

describe("rankQuickFind priority", () => {
  it("matches title before Space before host", () => {
    // Each token lives in exactly one field, on a different instance.
    expect(ids("zzz-title")).toEqual([zebraHostA.instances[0].id]);
    expect(ids("beta")).toEqual([betaHostB.instances[0].id]);
    expect(ids("zzz-host")).toEqual([betaHostB.instances[0].id]);

    // One query that hits both title and host at once must order title first,
    // regardless of recency (which here points the opposite direction).
    const mixed = run("zzz");
    expect(mixed.hits.map((hit) => hit.field)).toEqual(["title", "host"]);
    expect(mixed.hits.map((hit) => hit.instance.id)).toEqual([
      zebraHostA.instances[0].id,
      betaHostB.instances[0].id,
    ]);
  });

  it("ranks exact over prefix over word-start over substring within a field", () => {
    const exact = instance("ins_exact000-0000-7000-8000-000000000001", { updatedAt: "2026-09-01T00:00:00Z" });
    const prefix = instance("ins_prefix00-0000-7000-8000-000000000002", { updatedAt: "2026-09-02T00:00:00Z" });
    const word = instance("ins_word0000-0000-7000-8000-000000000003", { updatedAt: "2026-09-03T00:00:00Z" });
    const middle = instance("ins_middle00-0000-7000-8000-000000000004", { updatedAt: "2026-09-04T00:00:00Z" });
    const spaces = [
      space(spaceKey("h", "w"), "s", "h", "w", [
        // Deliberately shuffled and with recency pointing the wrong way.
        middle,
        word,
        prefix,
        exact,
      ]),
    ];
    const titleFor = (id: string) =>
      id === exact.id
        ? "deploy"
        : id === prefix.id
          ? "deploy queue"
          : id === word.id
            ? "auto deploy plan"
            : "redeploy now";
    const order = run("deploy", {
      spaces,
      titleOf: titleFor,
    }).hits.map((hit) => hit.instance.id);
    expect(order).toEqual([exact.id, prefix.id, word.id, middle.id]);
  });

  it("breaks ties by most recent activity, then id", () => {
    const older = instance("ins_older0000-0000-7000-8000-000000000001", { updatedAt: "2026-09-01T00:00:00Z" });
    const newer = instance("ins_newer0000-0000-7000-8000-000000000002", { updatedAt: "2026-09-11T00:00:00Z" });
    const spaces = [space(spaceKey("h", "w"), "s", "h", "w", [older, newer])];
    const order = run("session", {
      spaces,
      titleOf: () => "same session title",
    }).hits.map((hit) => hit.instance.id);
    expect(order).toEqual([newer.id, older.id]);
  });
});

describe("rankQuickFind instance id as a secondary hit", () => {
  it("includes an id-only hit but never ahead of a title/Space/host hit", () => {
    // The fragment "aaaa1111" exists only inside the first instance's id.
    const byId = ids("aaaa1111");
    expect(byId).toEqual([zebraHostA.instances[0].id]);
    expect(run("aaaa1111").hits[0]!.field).toBe("id");

    // "zzz" matches a title on instance 1 and an id nowhere; an id-only
    // candidate sharing the query token would still lose to the title.
    const idAlsoMatches = space(spaceKey("host-d", "wsp-d"), "delta", "host-d", "wsp-d", [
      instance("ins_zzz99999-0000-7000-8000-000000000009", {
        hostId: "host-d",
        workspaceId: "wsp-d",
      }),
    ]);
    const order = run("zzz", { spaces: [zebraHostA, idAlsoMatches] });
    expect(order.hits[0]!.field).toBe("title");
    expect(order.hits.map((h) => h.field)).toEqual(["title", "id"]);
  });
});

describe("rankQuickFind same-title disambiguation", () => {
  it("flags every same-titled row with ambiguousTitle and keeps Space/host distinct", () => {
    const shared = "payments";
    const first = instance("ins_payments1-0000-7000-8000-000000000001", { hostId: "h1", workspaceId: "w1" });
    const second = instance("ins_payments2-0000-7000-8000-000000000002", { hostId: "h2", workspaceId: "w2" });
    const spaces = [
      space(spaceKey("h1", "w1"), "payments", "h1", "w1", [first]),
      space(spaceKey("h2", "w2"), "payments", "h2", "w2", [second]),
    ];
    const hits = run("payments", {
      spaces,
      titleOf: () => shared,
      hostNameOf: (hostId) => (hostId === "h1" ? "demo-node-1" : "demo-node-2"),
    }).hits;
    expect(hits).toHaveLength(2);
    expect(hits.every((hit) => hit.ambiguousTitle)).toBe(true);
    expect(hits[0]!.spaceName).toBe("payments");
    expect(hits[1]!.spaceName).toBe("payments");
    expect(new Set(hits.map((hit) => hit.hostName))).toEqual(new Set(["demo-node-1", "demo-node-2"]));
  });

  it("does not flag a title that only appears once", () => {
    const hits = run("zzz-title").hits;
    expect(hits).toHaveLength(1);
    expect(hits[0]!.ambiguousTitle).toBe(false);
  });
});

describe("rankQuickFind cache-only scope", () => {
  it("reports cacheOnly when the Hub is offline or reconnecting", () => {
    expect(run("anything", { connection: "offline" }).cacheOnly).toBe(true);
    expect(run("anything", { connection: "reconnecting" }).cacheOnly).toBe(true);
    expect(run("anything", { connection: "live" }).cacheOnly).toBe(false);
    expect(run("anything").cacheOnly).toBe(false);
  });

  it("still ranks the full cache offline and counts before the limit", () => {
    const many = Array.from({ length: 7 }, (_, i) =>
      instance(`ins_cache000-0000-7000-8000-00000000000${i}`, {
        hostId: "h",
        workspaceId: "w",
        updatedAt: `2026-09-0${i + 1}T00:00:00Z`,
      }),
    );
    const spaces = [space(spaceKey("h", "w"), "s", "h", "w", many)];
    const result = run("", { spaces, connection: "offline", limit: 3, titleOf: () => "会话" });
    expect(result.cacheOnly).toBe(true);
    expect(result.total).toBe(7);
    expect(result.hits).toHaveLength(3);
    // Empty query: recency order, newest first.
    expect(result.hits[0]!.instance.id).toBe(many[6]!.id);
    expect(result.hits.every((hit) => hit.field === null)).toBe(true);
  });
});
