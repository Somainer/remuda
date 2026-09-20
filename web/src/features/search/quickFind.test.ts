import { describe, expect, it } from "vitest";
import { mockDb } from "../../lib/mock";
import { known } from "../../types/wire";
import type { Instance } from "../../types/instance";
import type { Workspace } from "../../types/workspace";
import { OTHER_SPACE, spaceKey, type Space } from "../spaces/store";
import {
  groupQuickFind,
  QUICKFIND_ORDER_KEY,
  rankQuickFind,
  readQuickFindOrder,
  writeQuickFindOrder,
  type QuickFindGroupSpace,
  type QuickFindInput,
  type QuickFindSpace,
} from "./quickFindSearch";

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

/**
 * groupQuickFind fixtures. Hits always come out of rankQuickFind: the grouper
 * is a view over the ranked list, never a second candidate source, so these
 * tests exercise that exact hand-off.
 */
function blocked(id: string, hostId: string, workspaceId: string, updatedAt: string): Instance {
  return instance(id, {
    hostId,
    workspaceId,
    activity: known("waiting-interaction"),
    updatedAt,
  });
}

function idleAt(id: string, hostId: string, workspaceId: string, updatedAt: string): Instance {
  return instance(id, { hostId, workspaceId, updatedAt });
}

function workspace(id: string, hostId: string, branch?: string): Workspace {
  return { id, hostId, branch } as Workspace;
}

type GroupFixture = {
  spaces: QuickFindSpace[];
  groupSpaces: QuickFindGroupSpace[];
  workspaces: Workspace[];
  titles: Record<string, string>;
};

function groupFixture(): GroupFixture {
  const a1 = blocked("ins_group-a1-0000-7000-8000-000000000001", "host-a", "wsp-a", "2026-09-01T00:00:00Z");
  const a2 = idleAt("ins_group-a2-0000-7000-8000-000000000002", "host-a", "wsp-a", "2026-09-20T00:00:00Z");
  const b1 = idleAt("ins_group-b1-0000-7000-8000-000000000003", "host-b", "wsp-b", "2026-09-21T00:00:00Z");
  const alpha = space(spaceKey("host-a", "wsp-a"), "alpha", "host-a", "wsp-a", [a1, a2]);
  const beta = space(spaceKey("host-b", "wsp-b"), "beta", "host-b", "wsp-b", [b1]);
  const titles: Record<string, string> = {
    [a1.id]: "blocked gate",
    [a2.id]: "zebra work",
    [b1.id]: "apple task",
  };
  return {
    spaces: [alpha, beta],
    // buildSpaces() would count exactly the one blocked row in alpha.
    groupSpaces: [
      { ...alpha, blockedCount: 1 },
      { ...beta, blockedCount: 0 },
    ],
    workspaces: [workspace("wsp-a", "host-a", "feat/workbench-g2")],
    titles,
  };
}

function groupedHits(fixture: GroupFixture, query = "") {
  return rankQuickFind({
    spaces: fixture.spaces,
    query,
    titleOf: (id) => fixture.titles[id] ?? "会话",
    hostNameOf: (hostId) => (hostId ? (HOSTS[hostId] ?? hostId) : ""),
    connection: "live",
  }).hits;
}

describe("groupQuickFind grouping and counts", () => {
  it("groups ranked hits by (host, workspace) with project, host, branch and the buildSpaces blocked count", () => {
    const fixture = groupFixture();
    const groups = groupQuickFind(groupedHits(fixture), fixture.groupSpaces, fixture.workspaces, "clock");

    expect(groups).toHaveLength(2);
    const alpha = groups.find((group) => group.project === "alpha")!;
    const beta = groups.find((group) => group.project === "beta")!;
    expect(alpha.id).toBe(spaceKey("host-a", "wsp-a"));
    expect(alpha!.project).toBe("alpha");
    expect(alpha!.hostName).toBe("demo-node-1");
    expect(alpha!.branch).toBe("feat/workbench-g2");
    expect(alpha!.blockedCount).toBe(1);
    expect(alpha!.hits.map((hit) => hit.instance.id)).toEqual([
      "ins_group-a1-0000-7000-8000-000000000001",
      "ins_group-a2-0000-7000-8000-000000000002",
    ]);

    // wsp-b registers without a branch: the header gets null, not an empty chip.
    expect(beta!.project).toBe("beta");
    expect(beta!.hostName).toBe("zzz-host-machine");
    expect(beta!.branch).toBeNull();
    expect(beta!.blockedCount).toBe(0);
  });

  it("keeps the whole-Space blocked count while a query hides leaves", () => {
    const a1 = blocked("ins_hide-a1-0000-7000-8000-000000000001", "host-a", "wsp-a", "2026-09-01T00:00:00Z");
    const a2 = blocked("ins_hide-a2-0000-7000-8000-000000000002", "host-a", "wsp-a", "2026-09-02T00:00:00Z");
    const a3 = idleAt("ins_hide-a3-0000-7000-8000-000000000003", "host-a", "wsp-a", "2026-09-03T00:00:00Z");
    const alpha = space(spaceKey("host-a", "wsp-a"), "alpha", "host-a", "wsp-a", [a1, a2, a3]);
    const fixture: GroupFixture = {
      spaces: [alpha],
      groupSpaces: [{ ...alpha, blockedCount: 2 }],
      workspaces: [],
      titles: { [a1.id]: "one gate", [a2.id]: "two gate", [a3.id]: "UNIQUE-VISIBLE-TITLE" },
    };

    const hits = groupedHits(fixture, "UNIQUE-VISIBLE-TITLE");
    expect(hits).toHaveLength(1);
    const groups = groupQuickFind(hits, fixture.groupSpaces, fixture.workspaces, "clock");
    expect(groups).toHaveLength(1);
    // The header advertises both blocked sessions even though only one leaf
    // matches the query (homeRows' same "whole Space count" rule).
    expect(groups[0]!.blockedCount).toBe(2);
    expect(groups[0]!.hits).toHaveLength(1);
  });

  it("returns no groups for no hits (empty state)", () => {
    const fixture = groupFixture();
    expect(groupQuickFind([], fixture.groupSpaces, fixture.workspaces, "clock")).toEqual([]);
  });
});

describe("groupQuickFind clock ordering", () => {
  it("orders groups by latest leaf change and pins blocked leaves above newer ones", () => {
    const fixture = groupFixture();
    const groups = groupQuickFind(groupedHits(fixture), fixture.groupSpaces, fixture.workspaces, "clock");

    // beta's only leaf (09-21) is newer than anything in alpha (09-20), so
    // beta opens the sheet even though alpha carries the blocked row.
    expect(groups.map((group) => group.project)).toEqual(["beta", "alpha"]);

    // Inside alpha the blocked leaf stays pinned despite being the oldest.
    const alpha = groups.find((group) => group.project === "alpha")!;
    expect(alpha.hits.map((hit) => hit.instance.id)).toEqual([
      "ins_group-a1-0000-7000-8000-000000000001",
      "ins_group-a2-0000-7000-8000-000000000002",
    ]);
  });

  it("sorts the Other Space last regardless of recency", () => {
    const orphan = idleAt("ins_other-0000-0000-7000-8000-000000000009", "host-x", "wsp-x", "2026-12-01T00:00:00Z");
    const otherSpace: Space = {
      id: OTHER_SPACE,
      name: "其他",
      instances: [orphan],
      liveCount: 1,
      blockedCount: 0,
    };
    const fixture = groupFixture();
    const spaces = [...fixture.spaces, otherSpace];
    const titles = { ...fixture.titles, [orphan.id]: "orphan from the future" };
    const hits = rankQuickFind({
      spaces,
      query: "",
      titleOf: (id) => titles[id] ?? "会话",
      hostNameOf: (id) => (id ? (HOSTS[id] ?? id) : ""),
      connection: "live",
    }).hits;
    const groupSpaces: QuickFindGroupSpace[] = [
      ...fixture.groupSpaces,
      { id: OTHER_SPACE, blockedCount: 0 },
    ];

    const clock = groupQuickFind(hits, groupSpaces, fixture.workspaces, "clock");
    expect(clock.at(-1)!.id).toBe(OTHER_SPACE);
    const list = groupQuickFind(hits, groupSpaces, fixture.workspaces, "list");
    expect(list.at(-1)!.id).toBe(OTHER_SPACE);
  });
});

describe("groupQuickFind list ordering", () => {
  it("orders groups by project name and leaves by title, ties by id", () => {
    const fixture = groupFixture();
    const groups = groupQuickFind(groupedHits(fixture), fixture.groupSpaces, fixture.workspaces, "list");

    expect(groups.map((group) => group.project)).toEqual(["alpha", "beta"]);
    const alpha = groups[0]!;
    // Blocked still pins; among the rest, title order ("blocked gate" < "zebra work").
    expect(alpha.hits.map((hit) => hit.title)).toEqual(["blocked gate", "zebra work"]);
    const beta = groups[1]!;
    expect(beta.hits.map((hit) => hit.title)).toEqual(["apple task"]);
  });

  it("orders non-blocked leaves purely by title", () => {
    const z = idleAt("ins_t-z00000-0000-7000-8000-000000000001", "h", "w", "2026-09-20T00:00:00Z");
    const a = idleAt("ins_t-a00000-0000-7000-8000-000000000002", "h", "w", "2026-09-01T00:00:00Z");
    const sole = space(spaceKey("h", "w"), "sole", "h", "w", [z, a]);
    const fixture: GroupFixture = {
      spaces: [sole],
      groupSpaces: [{ ...sole, blockedCount: 0 }],
      workspaces: [],
      titles: { [z.id]: "z title", [a.id]: "a title" },
    };
    const groups = groupQuickFind(groupedHits(fixture), fixture.groupSpaces, [], "list");
    expect(groups[0]!.hits.map((hit) => hit.instance.id)).toEqual([a.id, z.id]);
  });
});

describe("Jump To order persistence", () => {
  it("defaults to clock and only stores list explicitly", () => {
    expect(readQuickFindOrder(undefined)).toBe("clock");
    const values = new Map<string, string>();
    const storage = {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => void values.set(key, value),
    };
    expect(readQuickFindOrder(storage)).toBe("clock");
    writeQuickFindOrder("list", storage);
    expect(values.get(QUICKFIND_ORDER_KEY)).toBe("list");
    expect(readQuickFindOrder(storage)).toBe("list");
    writeQuickFindOrder("clock", storage);
    expect(readQuickFindOrder(storage)).toBe("clock");
  });

  it("survives a throwing storage without breaking", () => {
    const broken = {
      getItem: () => {
        throw new Error("denied");
      },
      setItem: () => {
        throw new Error("denied");
      },
    };
    expect(readQuickFindOrder(broken)).toBe("clock");
    expect(() => writeQuickFindOrder("list", broken)).not.toThrow();
  });
});
