import { describe, expect, it } from "vitest";
import { projectStatus } from "./status";
import { known } from "../types/wire";
import type { Workspace } from "../types/workspace";
import { defaultSpacePrefs, OTHER_SPACE, spaceKey, type Space, type SpacePrefs } from "../features/spaces/store";
import { resolveTabSet, SWITCH_SLOT_COUNT, switchSlotOf, switchSlots, tabSlots } from "./sessionSlots";
import type { Instance } from "../types/instance";

function instance(id: string, createdAt: string, patch: Partial<Instance> = {}): Instance {
  return {
    id,
    hostId: "hst_a",
    workspaceId: "wsp_a",
    createdAt,
    updatedAt: createdAt,
    lifecycle: "ready",
    connectivity: "connected",
    activity: known("idle"),
    ...patch,
  } as Instance;
}

function space(ids: Array<[string, string]>): Space {
  return {
    id: '["hst_a","wsp_a"]',
    hostId: "hst_a",
    workspaceId: "wsp_a",
    name: "root",
    instances: ids.map(([id, createdAt]) => instance(id, createdAt)),
    liveCount: 0,
    blockedCount: 0,
  };
}

describe("switchSlots — the shared ⌘digit ordering", () => {
  it("returns the visible tab order capped at nine slots", () => {
    const ids = Array.from({ length: 11 }, (_, i) => [`ins_${i}`, `2026-09-1${(i % 9) + 1}T00:00:00Z`] as [string, string]);
    const slots = switchSlots(space(ids), defaultSpacePrefs());
    expect(slots).toHaveLength(SWITCH_SLOT_COUNT);
    expect(slots.map((slot) => slot.id)).toEqual(ids.slice(0, 9).map(([id]) => id));
  });

  it("returns nothing without an active space", () => {
    expect(switchSlots(undefined, defaultSpacePrefs())).toEqual([]);
  });

  it("excludes dismissed tabs so the strip and the digits agree", () => {
    const sp = space([
      ["ins_a", "2026-09-10T00:00:00Z"],
      ["ins_b", "2026-09-11T00:00:00Z"],
      ["ins_c", "2026-09-12T00:00:00Z"],
    ]);
    const prefs = { ...defaultSpacePrefs(), closedTabs: { [sp.id]: [{ id: "ins_b", resurface: true }] } };
    const slots = switchSlots(sp, prefs);
    expect(slots.map((slot) => slot.id)).toEqual(["ins_a", "ins_c"]);
    // The dismissed row is renumbered: pressing 2 must open the second
    // *visible* tab, never the hidden one.
    expect(switchSlotOf(slots, "ins_c")).toBe(2);
    expect(switchSlotOf(slots, "ins_b")).toBe(0);
  });

  it("re-admits a dismissed tab that becomes blocked (visibleTabs resurface rule)", () => {
    const sp: Space = {
      ...space([
        ["ins_a", "2026-09-10T00:00:00Z"],
        ["ins_b", "2026-09-11T00:00:00Z"],
        ["ins_c", "2026-09-12T00:00:00Z"],
      ]),
    };
    sp.instances[1] = instance("ins_b", "2026-09-11T00:00:00Z", { activity: known("waiting-interaction") });
    expect(projectStatus(sp.instances[1])).toBe("blocked");
    const prefs = { ...defaultSpacePrefs(), closedTabs: { [sp.id]: [{ id: "ins_b", resurface: true }] } };

    // A blocked session the user dismissed still claims a digit, in its
    // original tab position — the same order the tab strip shows.
    const slots = switchSlots(sp, prefs);
    expect(slots.map((slot) => slot.id)).toEqual(["ins_a", "ins_b", "ins_c"]);
    expect(switchSlotOf(slots, "ins_b")).toBe(2);
  });
});

function workspace(hostId: string, workspaceId: string, label: string): Workspace {
  return { id: workspaceId, hostId, label, rootPath: `/${label}`, writePolicy: "workspace-write",
    canonicalRoot: { state: "known", value: `/${label}` } } as Workspace;
}

function taskInstance(id: string, hostId: string, workspaceId: string, createdAt: string, taskId?: string | null): Instance {
  return instance(id, createdAt, { hostId, workspaceId, taskId: taskId ?? null });
}

describe("tabSlots — the /s/* digit ordering", () => {
  it("is the rendered strip order capped at nine", () => {
    const tabs = Array.from({ length: 11 }, (_, i) => instance(`ins_${i}`, "2026-09-01T00:00:00Z"));
    expect(tabSlots(tabs)).toHaveLength(SWITCH_SLOT_COUNT);
    expect(tabSlots(tabs).map((tab) => tab.id)).toEqual(tabs.slice(0, 9).map((tab) => tab.id));
  });
});

describe("resolveTabSet — Task sessions vs Space tabs (D-053 §10)", () => {
  const workspaces = [
    workspace("hst_a", "wsp_a", "alpha"),
    workspace("hst_a", "wsp_b", "beta"),
  ];

  it("falls back to the active Space's visible tabs for an unbound session", () => {
    const instances = [
      taskInstance("ins_a1", "hst_a", "wsp_a", "2026-09-10T00:00:00Z", null),
      taskInstance("ins_a2", "hst_a", "wsp_a", "2026-09-11T00:00:00Z", null),
      taskInstance("ins_b1", "hst_a", "wsp_b", "2026-09-12T00:00:00Z", null),
    ];
    const set = resolveTabSet(workspaces, instances, defaultSpacePrefs(), "ins_a2");
    expect(set.kind).toBe("space");
    expect(set.tabs.map((tab) => tab.id)).toEqual(["ins_a1", "ins_a2"]);
    expect(set.ariaLabel).toBe(`空间 alpha 的会话`);
    expect(set.space?.id).toBe(spaceKey("hst_a", "wsp_a"));
  });

  it("collects every snapshot instance of the bound task across Spaces, sorted", () => {
    const instances = [
      taskInstance("ins_b1", "hst_a", "wsp_b", "2026-09-12T00:00:00Z", "tsk_1"),
      taskInstance("ins_a2", "hst_a", "wsp_a", "2026-09-11T00:00:00Z", "tsk_1"),
      taskInstance("ins_a1", "hst_a", "wsp_a", "2026-09-10T00:00:00Z", "tsk_1"),
      taskInstance("ins_a3", "hst_a", "wsp_a", "2026-09-13T00:00:00Z", null),
      taskInstance("ins_b2", "hst_a", "wsp_b", "2026-09-14T00:00:00Z", "tsk_other"),
    ];
    const set = resolveTabSet(workspaces, instances, defaultSpacePrefs(), "ins_b1");
    expect(set.kind).toBe("task");
    expect(set.taskId).toBe("tsk_1");
    expect(set.ariaLabel).toBe("本任务的会话");
    // createdAt order; neither the unbound Space mate nor the other task shows.
    expect(set.tabs.map((tab) => tab.id)).toEqual(["ins_a1", "ins_a2", "ins_b1"]);
    // A deep link selects the tab's owning Space even while showing the task set.
    expect(set.space?.id).toBe(spaceKey("hst_a", "wsp_b"));
  });

  it("filters a task tab by its owning Space dismissal, and records close on that owner", () => {
    const instances = [
      taskInstance("ins_a1", "hst_a", "wsp_a", "2026-09-10T00:00:00Z", "tsk_1"),
      taskInstance("ins_b1", "hst_a", "wsp_b", "2026-09-11T00:00:00Z", "tsk_1"),
    ];
    const prefs: SpacePrefs = {
      ...defaultSpacePrefs(),
      closedTabs: { [spaceKey("hst_a", "wsp_b")]: [{ id: "ins_b1", resurface: true }] },
    };
    const set = resolveTabSet(workspaces, instances, prefs, "ins_a1");
    expect(set.tabs.map((tab) => tab.id)).toEqual(["ins_a1"]);
    expect(set.ownerOf(instances[0])).toBe(spaceKey("hst_a", "wsp_a"));
    expect(set.ownerOf(instances[1])).toBe(spaceKey("hst_a", "wsp_b"));
  });

  it("re-surfaces a dismissed task tab from another Space when it becomes blocked", () => {
    const instances = [
      taskInstance("ins_a1", "hst_a", "wsp_a", "2026-09-10T00:00:00Z", "tsk_1"),
      taskInstance("ins_b1", "hst_a", "wsp_b", "2026-09-11T00:00:00Z", "tsk_1"),
    ];
    instances[1].activity = known("waiting-interaction");
    const prefs: SpacePrefs = {
      ...defaultSpacePrefs(),
      closedTabs: { [spaceKey("hst_a", "wsp_b")]: [{ id: "ins_b1", resurface: true }] },
    };
    const set = resolveTabSet(workspaces, instances, prefs, "ins_a1");
    expect(set.tabs.map((tab) => tab.id)).toEqual(["ins_a1", "ins_b1"]);
  });

  it("owns an unregistered task session through the 其他 Space bucket", () => {
    const instances = [
      taskInstance("ins_a1", "hst_a", "wsp_a", "2026-09-10T00:00:00Z", "tsk_1"),
      taskInstance("ins_x1", "hst_zzz", "wsp_zzz", "2026-09-11T00:00:00Z", "tsk_1"),
    ];
    const set = resolveTabSet(workspaces, instances, defaultSpacePrefs(), "ins_x1");
    expect(set.tabs.map((tab) => tab.id)).toEqual(["ins_a1", "ins_x1"]);
    expect(set.ownerOf(instances[1])).toBe(OTHER_SPACE);
  });

  it("tabSlots numbers the task strip in the same order resolveTabSet renders it", () => {
    const instances = [
      taskInstance("ins_b1", "hst_a", "wsp_b", "2026-09-12T00:00:00Z", "tsk_1"),
      taskInstance("ins_a1", "hst_a", "wsp_a", "2026-09-10T00:00:00Z", "tsk_1"),
    ];
    const set = resolveTabSet(workspaces, instances, defaultSpacePrefs(), "ins_b1");
    expect(tabSlots(set.tabs).map((tab) => tab.id)).toEqual(["ins_a1", "ins_b1"]);
  });
});
