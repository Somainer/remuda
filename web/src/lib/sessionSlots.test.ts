import { describe, expect, it } from "vitest";
import { projectStatus } from "./status";
import { known } from "../types/wire";
import { defaultSpacePrefs, type Space } from "../features/spaces/store";
import { SWITCH_SLOT_COUNT, switchSlotOf, switchSlots } from "./sessionSlots";
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
