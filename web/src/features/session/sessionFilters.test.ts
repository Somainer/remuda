import { describe, expect, it } from "vitest";
import {
  applyFilters,
  availableConditions,
  clearedConditions,
  conditionCount,
  describeScope,
  emptyState,
  hasConditions,
  pruneForScope,
  readConditions,
  selectedChips,
  toggleValue,
  withoutChip,
  writeConditions,
  type FilterConditions,
} from "./sessionFilters";
import type { Instance, UiStatus } from "../../types/instance";
import type { Workspace } from "../../types/workspace";
import type { Id } from "../../types/wire";

function conditions(over: Partial<FilterConditions> = {}): FilterConditions {
  return { q: "", status: [], kind: [], host: [], workspace: [], scope: "space", ...over };
}

/** Only the fields the filter model reads; the rest of Instance is irrelevant here. */
function instance(over: { id: string; hostId: string; workspaceId: string; kind?: string; native?: string }): Instance {
  return {
    id: over.id as Id,
    hostId: over.hostId as Id,
    workspaceId: over.workspaceId as Id,
    kind: (over.kind ?? "claude") as Instance["kind"],
    nativeRef: {
      sessionId: over.native ? { state: "known", value: over.native } : { state: "unknown", reason: "none" },
    },
  } as unknown as Instance;
}

function workspace(id: string, hostId: string, label: string, rootPath: string): Workspace {
  return { id, hostId, label, rootPath } as unknown as Workspace;
}

const hostNames: Record<string, string> = { h1: "alpha", h2: "beta", h3: "gamma" };
const hostName = (id: string) => hostNames[id] ?? id;

describe("sessionFilters URL round-trip", () => {
  it("reads every condition out of the query string", () => {
    const params = new URLSearchParams("q=spill&status=blocked,idle&kind=claude&host=h1&workspace=w1&scope=all");
    expect(readConditions(params)).toEqual({
      q: "spill",
      status: ["blocked", "idle"],
      kind: ["claude"],
      host: ["h1"],
      workspace: ["w1"],
      scope: "all",
    });
  });

  it("defaults to Space scope and empty conditions on a bare URL", () => {
    expect(readConditions(new URLSearchParams(""))).toEqual(conditions());
  });

  it("writes conditions back to the same query string it read", () => {
    const source = new URLSearchParams("q=spill&status=blocked,idle&kind=claude&host=h1&workspace=w1&scope=all");
    const round = writeConditions(new URLSearchParams(), readConditions(source));
    expect(readConditions(round)).toEqual(readConditions(source));
  });

  it("drops emptied conditions from the URL instead of leaving blank keys", () => {
    const params = new URLSearchParams("q=spill&status=blocked&host=h1&scope=all");
    const out = writeConditions(params, conditions({ q: "", status: [], host: [], scope: "space" }));
    expect(out.has("q")).toBe(false);
    expect(out.has("status")).toBe(false);
    expect(out.has("host")).toBe(false);
    expect(out.has("scope")).toBe(false);
    expect(out.toString()).toBe("");
  });

  it("preserves unrelated params a deep link carried in", () => {
    const params = new URLSearchParams("tab=terminal&q=old");
    const out = writeConditions(params, conditions({ q: "new" }));
    expect(out.get("tab")).toBe("terminal");
    expect(out.get("q")).toBe("new");
  });

  it("counts each active condition value for the filter badge", () => {
    expect(conditionCount(conditions())).toBe(0);
    expect(conditionCount(conditions({ q: "  " }))).toBe(0);
    expect(conditionCount(conditions({ q: "x", status: ["blocked", "idle"], host: ["h1"] }))).toBe(4);
    expect(hasConditions(conditions())).toBe(false);
    expect(hasConditions(conditions({ status: ["idle"] }))).toBe(true);
  });

  it("toggles a value in and back out", () => {
    expect(toggleValue([], "blocked")).toEqual(["blocked"]);
    expect(toggleValue(["blocked", "idle"], "blocked")).toEqual(["idle"]);
  });
});

describe("scope derivation", () => {
  const space = { name: "sfe-root", hostId: "h1", workspaceId: "w1" };

  it("pins to the Space and names its host, so same-name directories stay distinct", () => {
    const scope = describeScope(conditions(), space, hostName);
    expect(scope.kind).toBe("space");
    expect(scope.hostId).toBe("h1");
    expect(scope.label).toContain("当前 Space 固定范围");
    expect(scope.label).toContain("sfe-root");
    expect(scope.label).toContain("alpha");
  });

  it("goes global on an explicit scope=all, regardless of the selected Space", () => {
    const scope = describeScope(conditions({ scope: "all" }), space, hostName);
    expect(scope.kind).toBe("all");
    expect(scope.label).toContain("所有空间");
  });

  it("falls back to global when there is no Space to pin to", () => {
    expect(describeScope(conditions(), undefined, hostName).kind).toBe("all");
    expect(describeScope(conditions(), { name: "other" }, hostName).kind).toBe("all");
  });

  it("offers host and directory conditions only outside a fixed Space", () => {
    const pinned = availableConditions(describeScope(conditions(), space, hostName));
    expect(pinned).toMatchObject({ host: false, workspace: false, status: true, kind: true });
    const global = availableConditions(describeScope(conditions({ scope: "all" }), space, hostName));
    expect(global).toMatchObject({ host: true, workspace: true });
  });
});

describe("Space switch pruning", () => {
  const space = { name: "sfe-root", hostId: "h1", workspaceId: "w1" };

  it("drops host and directory conditions that the new fixed Space cannot honour", () => {
    const before = conditions({ q: "spill", status: ["blocked"], host: ["h2"], workspace: ["w9"] });
    const { conditions: after, dropped } = pruneForScope(before, describeScope(before, space, hostName));
    expect(after.host).toEqual([]);
    expect(after.workspace).toEqual([]);
    expect(dropped.map((chip) => chip.key)).toEqual(["host", "workspace"]);
  });

  it("keeps text and status conditions, which mean the same thing in any Space", () => {
    const before = conditions({ q: "spill", status: ["blocked", "idle"], kind: ["claude"], host: ["h2"] });
    const { conditions: after } = pruneForScope(before, describeScope(before, space, hostName));
    expect(after.q).toBe("spill");
    expect(after.status).toEqual(["blocked", "idle"]);
    expect(after.kind).toEqual(["claude"]);
  });

  it("reports nothing dropped when there is nothing to drop, so the caller can skip the rewrite", () => {
    const before = conditions({ q: "spill", status: ["blocked"] });
    const result = pruneForScope(before, describeScope(before, space, hostName));
    expect(result.dropped).toEqual([]);
    expect(result.conditions).toBe(before);
  });

  it("leaves host and directory conditions alone in global scope", () => {
    const before = conditions({ host: ["h2"], workspace: ["w9"], scope: "all" });
    const result = pruneForScope(before, describeScope(before, space, hostName));
    expect(result.dropped).toEqual([]);
    expect(result.conditions.host).toEqual(["h2"]);
  });

  it("clears every narrowing condition but keeps the current scope", () => {
    const cleared = clearedConditions(conditions({ q: "x", status: ["idle"], host: ["h1"], scope: "all" }));
    expect(cleared).toEqual(conditions({ scope: "all" }));
  });
});

describe("matching", () => {
  const workspaces = [
    workspace("w1", "h1", "sfe-root", "/home/dev/projects/sfe-root"),
    workspace("w2", "h2", "sfe-root", "/srv/build/sfe-root"),
  ];
  const rows = [
    instance({ id: "ins_a", hostId: "h1", workspaceId: "w1", native: "sess-aaa" }),
    instance({ id: "ins_b", hostId: "h2", workspaceId: "w2", kind: "codex" }),
  ];
  const titles: Record<string, string> = { ins_a: "spill 抖动", ins_b: "codex worker" };
  const context = { titleOf: (id: string) => titles[id] ?? "", workspaces };
  const statusOf = (row: Instance): UiStatus => (row.id === "ins_a" ? "blocked" : "idle");

  it("matches on title, cwd, native id and instance id", () => {
    const run = (q: string) => applyFilters(rows, conditions({ q }), statusOf, context).map((row) => row.id);
    expect(run("spill")).toEqual(["ins_a"]);
    expect(run("/srv/build")).toEqual(["ins_b"]);
    expect(run("sess-aaa")).toEqual(["ins_a"]);
    expect(run("ins_b")).toEqual(["ins_b"]);
    expect(run("SPILL")).toEqual(["ins_a"]);
  });

  it("keeps same-name directories on different hosts separable by host condition", () => {
    const both = applyFilters(rows, conditions({ q: "sfe-root", scope: "all" }), statusOf, context);
    expect(both).toHaveLength(2);
    const one = applyFilters(rows, conditions({ q: "sfe-root", host: ["h2"], scope: "all" }), statusOf, context);
    expect(one.map((row) => row.id)).toEqual(["ins_b"]);
  });

  it("intersects conditions, so a self-excluding pair yields nothing", () => {
    const out = applyFilters(rows, conditions({ status: ["blocked"], kind: ["codex"], scope: "all" }), statusOf, context);
    expect(out).toEqual([]);
  });
});

describe("selected chips", () => {
  const names = {
    hostName,
    workspaceLabel: (id: string) => (id === "w1" || id === "w2" ? "sfe-root" : "valhalla"),
    workspaceAmbiguous: (id: string) => id === "w1" || id === "w2",
    workspaceHost: (id: string) => (id === "w1" ? "alpha" : "beta"),
  };

  it("lists every active condition as its own removable chip", () => {
    const chips = selectedChips(conditions({ q: "spill", status: ["blocked"], kind: ["claude"], host: ["h1"], scope: "all" }), names);
    expect(chips.map((chip) => chip.key)).toEqual(["q", "status", "kind", "host"]);
    expect(chips[0].label).toContain("spill");
    expect(chips[1].label).toBe("待处理");
    expect(chips[3].label).toContain("alpha");
  });

  it("qualifies a directory chip with its host when the name is shared", () => {
    const [shared] = selectedChips(conditions({ workspace: ["w1"], scope: "all" }), names);
    expect(shared.label).toBe("目录：sfe-root · alpha");
    const [unique] = selectedChips(conditions({ workspace: ["w9"], scope: "all" }), names);
    expect(unique.label).toBe("目录：valhalla");
  });

  it("removes only the chip's own value", () => {
    const before = conditions({ status: ["blocked", "idle"], q: "spill" });
    expect(withoutChip(before, { key: "status", value: "blocked", label: "" }).status).toEqual(["idle"]);
    expect(withoutChip(before, { key: "q", value: "spill", label: "" }).q).toBe("");
    expect(withoutChip(before, { key: "q", value: "spill", label: "" }).status).toEqual(["blocked", "idle"]);
  });
});

describe("empty states", () => {
  const base = { hostCount: 2, workspaceCount: 2, sourceCount: 3, matchCount: 1, conditions: conditions() };

  it("distinguishes the four causes of an empty list", () => {
    expect(emptyState({ ...base, hostCount: 0 })).toBe("no-hosts");
    expect(emptyState({ ...base, workspaceCount: 0 })).toBe("no-workspaces");
    expect(emptyState({ ...base, sourceCount: 0, matchCount: 0 })).toBe("no-sessions");
    expect(emptyState({ ...base, matchCount: 0, conditions: conditions({ q: "zzz" }) })).toBe("no-matches");
  });

  it("reports no empty state while something matches", () => {
    expect(emptyState(base)).toBeNull();
    expect(emptyState({ ...base, conditions: conditions({ q: "spill" }) })).toBeNull();
  });

  it("blames the conditions only when there are conditions to blame", () => {
    // Sessions exist and none match, but nothing is filtered: that is an empty
    // Space, not a filtered-out list, and it must not offer 清除筛选.
    expect(emptyState({ ...base, matchCount: 0 })).toBe("no-sessions");
  });

  it("checks hosts before workspaces before sessions", () => {
    expect(emptyState({ hostCount: 0, workspaceCount: 0, sourceCount: 0, matchCount: 0, conditions: conditions() })).toBe("no-hosts");
    expect(emptyState({ hostCount: 1, workspaceCount: 0, sourceCount: 0, matchCount: 0, conditions: conditions() })).toBe("no-workspaces");
  });
});
