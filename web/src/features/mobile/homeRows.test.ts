import { describe, expect, it } from "vitest";
import { mockDb } from "../../lib/mock";
import { known, type Id } from "../../types/wire";
import type { Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import type { Observation } from "../../types/observation";
import type { UsageRollup } from "../session/contextUsage";
import {
  OTHER_SPACE,
  buildSpaces,
  defaultSpacePrefs,
  spaceKey,
  type SpacePrefs,
} from "../spaces/store";
import {
  HOME_ORDER_KEY,
  arrangeHomeGroups,
  buildHomeGroups,
  buildHomeRows,
  homeError,
  homeRowsSignature,
  readHomeOrder,
  writeHomeOrder,
} from "./homeRows";

/**
 * Fixtures: the mock's first instance is a connected claude-print session
 * with resume supported; tests override the fields each rule needs.
 */
function session(id: string, patch: Partial<Instance> = {}): Instance {
  return {
    ...mockDb.instances[0],
    id: id as Id,
    hostId: "host-alpha",
    workspaceId: "wsp-alpha",
    lifecycle: "ready",
    connectivity: "connected",
    activity: known("idle"),
    updatedAt: "2026-09-19T10:00:00.000Z",
    parent: null,
    ...patch,
  } as Instance;
}

function workspace(id: string, hostId: string, root: string) {
  return {
    id,
    hostId,
    label: root.split("/").pop()!,
    rootPath: root,
    revision: "1",
    createdAt: "",
    updatedAt: "",
    writePolicy: "default",
    canonicalRoot: known(root),
  };
}

const workspaces = [
  workspace("wsp-alpha", "host-alpha", "/srv/alpha"),
  workspace("wsp-beta", "host-beta", "/srv/beta"),
];

const hostNames: Record<string, string> = {
  "host-alpha": "alpha-host",
  "host-beta": "beta-host",
};

const titles: Record<string, string> = {};
const rollups: Record<string, UsageRollup | null> = {};

function build(instances: Instance[], order: "clock" | "list" = "clock", query = "", prefs?: SpacePrefs) {
  const spaces = buildSpaces(workspaces, instances, prefs ?? defaultSpacePrefs());
  return buildHomeGroups({
    spaces,
    interactions: [],
    order,
    query,
    titleOf: (instanceId) => titles[instanceId] ?? instanceId,
    hostNameOf: (hostId) => hostNames[hostId ?? ""] ?? hostId ?? "",
    branchOf: (spaceId) => (spaceId === spaceKey("host-alpha", "wsp-alpha") ? "main" : "wt/x"),
    rollupOf: (instanceId) => rollups[instanceId] ?? null,
  });
}

function named(instances: Instance[]) {
  for (const instance of instances) titles[instance.id] = `${instance.id} title`;
  return instances;
}

describe("buildHomeGroups grouping", () => {
  it("groups by project (host + workspace) and annotates the git branch", () => {
    const groups = build(
      named([session("ins-a1"), session("ins-b1", { hostId: "host-beta", workspaceId: "wsp-beta" })]),
    );
    expect(groups).toHaveLength(2);
    expect(groups[0].project).toBe("alpha");
    expect(groups[0].branch).toBe("main");
    expect(groups[1].project).toBe("beta");
    expect(groups[1].branch).toBe("wt/x");
  });

  it("puts instances with no registered workspace into 其他, which sorts last", () => {
    const groups = build(
      named([
        session("ins-known"),
        session("ins-orphan", { hostId: "host-gamma", workspaceId: "wsp-missing" }),
      ]),
    );
    expect(groups.map((group) => group.id)).toEqual([
      spaceKey("host-alpha", "wsp-alpha"),
      OTHER_SPACE,
    ]);
  });

  it("group blocked count equals buildSpaces() blockedCount", () => {
    const instances = [
      session("ins-blocked", { activity: known("waiting-interaction") }),
      session("ins-working", { activity: known("working"), updatedAt: "2026-09-19T12:00:00.000Z" }),
      session("ins-idle", { updatedAt: "2026-09-19T11:00:00.000Z" }),
    ];
    const prefs = defaultSpacePrefs();
    const spaces = buildSpaces(workspaces, instances, prefs);
    const groups = build(instances, "clock", "", prefs);
    expect(spaces[0].blockedCount).toBe(1);
    expect(groups[0].blockedCount).toBe(spaces[0].blockedCount);
    expect(groups[0].blockedCount).toBe(1);
  });
});

describe("buildHomeGroups ordering and blocked pinning", () => {
  const rows = () => [
    session("ins-blocked-old", {
      activity: known("waiting-interaction"),
      updatedAt: "2026-09-19T08:00:00.000Z",
    }),
    session("ins-working-new", {
      activity: known("working"),
      updatedAt: "2026-09-19T12:00:00.000Z",
    }),
    session("ins-idle-mid", { updatedAt: "2026-09-19T10:00:00.000Z" }),
  ];

  it("clock ordering sorts by most recent change but pins blocked rows to the top", () => {
    const groups = build(named(rows()), "clock");
    expect(groups[0].rows.map((row) => row.id)).toEqual([
      "ins-blocked-old",
      "ins-working-new",
      "ins-idle-mid",
    ]);
  });

  it("list ordering sorts groups by project and rows by title, blocked still pinned", () => {
    const instances = [
      session("ins-a", { updatedAt: "2026-09-19T08:00:00.000Z" }),
      session("ins-zblocked", {
        hostId: "host-beta",
        workspaceId: "wsp-beta",
        activity: known("waiting-interaction"),
        updatedAt: "2026-09-19T08:00:00.000Z",
      }),
      session("ins-z", {
        hostId: "host-beta",
        workspaceId: "wsp-beta",
        activity: known("working"),
        updatedAt: "2026-09-19T20:00:00.000Z",
      }),
    ];
    named(instances);
    titles["ins-a"] = "aaa title";
    titles["ins-z"] = "zzz title";
    titles["ins-zblocked"] = "zzz blocked title";
    const groups = build(instances, "list");
    // Groups: alpha (project name) before beta even though beta is fresher.
    expect(groups.map((group) => group.project)).toEqual(["alpha", "beta"]);
    // The blocked row is pinned despite its title sorting last.
    expect(groups[1].rows.map((row) => row.title)).toEqual(["zzz blocked title", "zzz title"]);
  });

  it("clock ordering sorts groups by their newest change", () => {
    const instances = [
      session("ins-a-old", { updatedAt: "2026-09-19T08:00:00.000Z" }),
      session("ins-b-new", {
        hostId: "host-beta",
        workspaceId: "wsp-beta",
        activity: known("working"),
        updatedAt: "2026-09-19T20:00:00.000Z",
      }),
    ];
    const groups = build(named(instances), "clock");
    expect(groups.map((group) => group.project)).toEqual(["beta", "alpha"]);
  });
});

describe("buildHomeGroups body text", () => {
  it("uses the nextStep() sentence verbatim on a plain idle row", () => {
    const groups = build(named([session("ins-idle")]));
    expect(groups[0].rows[0].body).toBe("回合结束、进程仍在 · 可继续发送");
    expect(groups[0].rows[0].bodyIsError).toBe(false);
  });

  it("puts the instance error text in the body slot on an exited row", () => {
    const groups = build(
      named([
        session("ins-failed", {
          lifecycle: "exited",
          activity: known("idle"),
          lastError: "API Error: Request rejected (429)",
        }),
      ]),
    );
    const row = groups[0].rows[0];
    expect(row.status).toBe("exited");
    expect(row.body).toBe("API Error: Request rejected (429)");
    expect(row.bodyIsError).toBe(true);
  });

  it("falls back to the latest native severity=error lifecycle event when lastError is absent", () => {
    const instance = session("ins-event-error", { lifecycle: "running", activity: known("idle") });
    const events = [
      {
        kind: "lifecycle",
        payload: {
          type: "native",
          nativeName: "turn_error",
          severity: "error",
          status: { state: "known", value: "Upstream 500" },
        },
      },
    ] as unknown as Observation[];
    expect(homeError(instance, events)).toBe("Upstream 500");
  });

  it("keeps 状态待确认 for disconnected/unknown rows even when an error text exists", () => {
    const disconnected = session("ins-disconnected", {
      connectivity: "disconnected",
      activity: known("working"),
      lastError: "API Error: Request rejected (429)",
    });
    const reconciling = session("ins-reconciling", {
      lifecycle: "reconciling",
      lastError: "API Error: boom",
    });
    const groups = build(named([disconnected, reconciling]));
    const bodies = groups[0].rows.map((row) => row.body);
    for (const body of bodies) {
      expect(body).toBe("状态待确认 · 不推断成功或结束");
      expect(body).not.toMatch(/运行中|API Error/);
    }
  });

  it("advertises 恢复 only on exited rows whose resume capability is supported", () => {
    const resumable = session("ins-exited", { lifecycle: "exited", activity: known("idle") });
    const noCaps = session("ins-exited-uncap", {
      lifecycle: "exited",
      activity: known("idle"),
      capabilities: {
        ...mockDb.instances[0].capabilities,
        capabilities: {
          ...mockDb.instances[0].capabilities.capabilities,
          resume: { state: "unsupported", scope: [], reasonCode: "no-transcript", prerequisites: [], evidence: [] },
        },
      },
    });
    const groups = build(named([resumable, noCaps]));
    const byId = new Map(groups[0].rows.map((row) => [row.id, row]));
    expect(byId.get("ins-exited")?.canResume).toBe(true);
    expect(byId.get("ins-exited-uncap")?.canResume).toBe(false);
    expect(byId.get("ins-exited")?.body).toMatch(/已退出/);
  });
});

describe("buildHomeGroups context ring", () => {
  it("exposes contextPct 78 from the rollup", () => {
    rollups["ins-with-usage"] = { contextPct: 78 } as UsageRollup;
    const groups = build(named([session("ins-with-usage")]));
    expect(groups[0].rows[0].contextPct).toBe(78);
  });

  it("keeps contextPct null (no ring) when the rollup is absent", () => {
    const groups = build(named([session("ins-no-usage")]));
    expect(groups[0].rows[0].contextPct).toBeNull();
  });
});

describe("buildHomeGroups search", () => {
  function searchable() {
    const instances = [
      session("ins-alpha-one", { updatedAt: "2026-09-19T09:00:00.000Z" }),
      session("ins-beta-one", {
        hostId: "host-beta",
        workspaceId: "wsp-beta",
        updatedAt: "2026-09-19T10:00:00.000Z",
      }),
    ];
    titles["ins-alpha-one"] = "spill 抖动排查";
    titles["ins-beta-one"] = "grok 结构翻译";
    return instances;
  }

  it("matches a title word", () => {
    const groups = build(searchable(), "clock", "spill");
    const ids = groups.flatMap((group) => group.rows.map((row) => row.id));
    expect(ids).toEqual(["ins-alpha-one"]);
  });

  it("matches the project and the host name", () => {
    expect(build(searchable(), "clock", "beta").flatMap((g) => g.rows.map((r) => r.id))).toEqual([
      "ins-beta-one",
    ]);
    expect(build(searchable(), "clock", "alpha-host").flatMap((g) => g.rows.map((r) => r.id))).toEqual([
      "ins-alpha-one",
    ]);
  });

  it("returns nothing for a word that only appears in a transcript body / next-step sentence", () => {
    // "echo e2e" is the approval description the fake harness puts in the
    // body sentence; no title, project or host contains it.
    const groups = build(searchable(), "clock", "echo");
    expect(groups.flatMap((group) => group.rows)).toHaveLength(0);
  });

  it("never returns an id-only match (title / project / host scope only)", () => {
    const groups = build(searchable(), "clock", "ins-alpha-one");
    expect(groups.flatMap((group) => group.rows)).toHaveLength(0);
  });
});

describe("home ordering preference", () => {
  it("defaults to clock and remembers list on this device only", () => {
    const store = new Map<string, string>();
    const storage = {
      getItem: (key: string) => (store.has(key) ? store.get(key)! : null),
      setItem: (key: string, value: string) => void store.set(key, value),
    };
    expect(readHomeOrder(storage)).toBe("clock");
    writeHomeOrder("list", storage);
    expect(store.get(HOME_ORDER_KEY)).toBe("list");
    expect(readHomeOrder(storage)).toBe("list");
    // Unknown/garbage values never silently become list.
    store.set(HOME_ORDER_KEY, "garbage");
    expect(readHomeOrder(storage)).toBe("clock");
  });
});

describe("buildHomeRows / arrangeHomeGroups split (commit:HomeList caching seam)", () => {
  function interaction(instanceId: string, id: string): Interaction {
    return {
      instanceId,
      id,
      state: "pending",
      request: { kind: "approval", title: "t", description: "" },
    } as unknown as Interaction;
  }

  it("derives one row per instance with a 12px-class relative time label", () => {
    const instances = [session("ins-a1"), session("ins-b1", { hostId: "host-beta", workspaceId: "wsp-beta" })];
    const spaces = buildSpaces(workspaces, instances, defaultSpacePrefs());
    const rows = buildHomeRows({
      spaces,
      interactions: [],
      titleOf: (id) => `${id} title`,
      rollupOf: () => null,
      nowMs: Date.parse("2026-09-19T10:00:30.000Z"),
    });
    expect([...rows.keys()].sort()).toEqual(["ins-a1", "ins-b1"]);
    expect(rows.get("ins-a1")?.timeLabel).toBeTruthy();
  });

  it("arrangeHomeGroups filters the pre-derived rows by the query", () => {
    const instances = named([session("ins-find")]);
    const spaces = buildSpaces(workspaces, instances, defaultSpacePrefs());
    const rows = buildHomeRows({
      spaces,
      interactions: [],
      titleOf: (instanceId) => titles[instanceId] ?? instanceId,
      rollupOf: () => null,
    });
    const titleOf = (instanceId: string) => titles[instanceId] ?? instanceId;
    const hostNameOf = (hostId?: string) => hostNames[hostId ?? ""] ?? hostId ?? "";
    expect(
      arrangeHomeGroups({ spaces, rows, needle: "ins-find title", order: "clock", hostNameOf, titleOf })
        .flatMap((group) => group.rows),
    ).toHaveLength(1);
    expect(
      arrangeHomeGroups({ spaces, rows, needle: "nothing-here", order: "clock", hostNameOf, titleOf })
        .flatMap((group) => group.rows),
    ).toHaveLength(0);
  });

  it("the display signature is stable when another pending interaction lands on the same blocked instance", () => {
    const blocked = session("ins-blocked", { activity: known("waiting-interaction") });
    const spaces = buildSpaces(workspaces, [blocked], defaultSpacePrefs());
    const titleOf = () => "blocked title";
    const hostNameOf = () => "alpha-host";
    const derive = (interactions: ReturnType<typeof interaction>[]) => {
      const rows = buildHomeRows({
        spaces,
        interactions,
        titleOf,
        rollupOf: () => null,
      });
      return homeRowsSignature(
        rows,
        interactions.map((item) => item.instanceId as unknown as string),
        spaces,
        hostNameOf,
      );
    };
    const one = derive([interaction("ins-blocked", "int-1")]);
    const two = derive([interaction("ins-blocked", "int-1"), interaction("ins-blocked", "int-2")]);
    const three = derive([
      interaction("ins-blocked", "int-1"),
      interaction("ins-blocked", "int-2"),
      interaction("ins-blocked", "int-3"),
    ]);
    // The badge climbs 1→2→3 but the instance set and every row pixel are
    // unchanged: HomeList must keep its cached slice and bail out.
    expect(two).toBe(one);
    expect(three).toBe(one);
  });

  it("the signature changes when a different instance becomes blocked or the body sentence changes", () => {
    const first = session("ins-blocked", { activity: known("waiting-interaction") });
    const second = session("ins-idle");
    const spaces1 = buildSpaces(workspaces, [first, second], defaultSpacePrefs());
    const titleOf = (instanceId: string) => `${instanceId} title`;
    const hostNameOf = () => "alpha-host";
    const sig = (interactions: ReturnType<typeof interaction>[], instances: Instance[]) => {
      const spaces = buildSpaces(workspaces, instances, defaultSpacePrefs());
      const rows = buildHomeRows({ spaces, interactions, titleOf, rollupOf: () => null });
      return homeRowsSignature(
        rows,
        interactions.map((item) => item.instanceId as unknown as string),
        spaces,
        hostNameOf,
      );
    };
    const baseline = sig([interaction("ins-blocked", "int-1")], [first, second]);
    const secondBlocked = { ...second, activity: known("waiting-interaction") } as Instance;
    expect(sig([interaction("ins-blocked", "int-1"), interaction("ins-idle", "int-2")], [first, secondBlocked])).not.toBe(baseline);
    // A real body change on the same instance changes the signature too.
    const errored = { ...first, lastError: "boom" } as Instance;
    const spaces2 = buildSpaces(workspaces, [errored, second], defaultSpacePrefs());
    const rowsErrored = buildHomeRows({ spaces: spaces2, interactions: [interaction("ins-blocked", "int-1")], titleOf, rollupOf: () => null });
    const rowsClean = buildHomeRows({ spaces: spaces1, interactions: [interaction("ins-blocked", "int-1")], titleOf, rollupOf: () => null });
    expect(homeRowsSignature(rowsErrored, ["ins-blocked"], spaces2, hostNameOf)).not.toBe(
      homeRowsSignature(rowsClean, ["ins-blocked"], spaces1, hostNameOf),
    );
  });
});
