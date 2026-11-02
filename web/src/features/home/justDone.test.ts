import { describe, expect, it } from "vitest";
import type { Instance } from "../../types/instance";
import { known, na, unknownKnowledge, type Id } from "../../types/wire";
import { justDoneRows, JUST_DONE_SHOWN } from "./justDone";

/**
 * ⑤ 刚完成 (home.md §7, §9): the rows the section lists, how many it shows,
 * and the current-chapter rule. Pure — the component only paints this.
 */

const T0 = "2026-10-31T08:00:00.000Z";

function inst(overrides: Partial<Instance> & { id: Id }): Instance {
  return {
    revision: "1",
    createdAt: T0,
    updatedAt: T0,
    hostId: "hst_1",
    workspaceId: "wsp_1",
    kind: "claude",
    driver: "claude-cli",
    lifecycle: "running",
    activity: known("idle"),
    connectivity: "connected",
    ownership: "managed",
    nativeRef: {} as Instance["nativeRef"],
    processRef: {} as Instance["processRef"],
    specRevision: "1",
    launchId: unknownKnowledge("none"),
    capabilities: {} as Instance["capabilities"],
    ownerFence: "1",
    activeRunIds: [],
    parent: null,
    journalId: "jrn_1",
    durableSeq: "1",
    exit: na(),
    unseen: true,
    ...overrides,
  } as Instance;
}

describe("justDoneRows", () => {
  it("lists only done-unseen rows, newest first, and shows three plus the rest", () => {
    const rows = [
      inst({ id: "ins_old", updatedAt: "2026-10-31T07:00:00.000Z" }),
      inst({ id: "ins_new", updatedAt: "2026-10-31T11:00:00.000Z" }),
      inst({ id: "ins_mid", updatedAt: "2026-10-31T09:00:00.000Z" }),
      inst({ id: "ins_seen", unseen: false }),
      inst({ id: "ins_working", activity: known("working") }),
      inst({ id: "ins_failed", lastTurnError: { at: T0, text: "boom" } }),
      inst({ id: "ins_fourth", updatedAt: "2026-10-31T06:00:00.000Z" }),
    ];
    const result = justDoneRows(rows);
    expect(result.total).toBe(4);
    expect(result.shown.map((row) => row.id)).toEqual(["ins_new", "ins_mid", "ins_old"]);
    expect(result.overflow).toBe(1);
    expect(JUST_DONE_SHOWN).toBe(3);
  });

  it("shows nothing when no row is ◉", () => {
    const result = justDoneRows([inst({ id: "ins_seen", unseen: false })]);
    expect(result.total).toBe(0);
    expect(result.shown).toEqual([]);
    expect(result.overflow).toBe(0);
  });

  it("lists only Main's current chapter, never an older or fenced one", () => {
    const rows = [
      inst({ id: "ins_current", grants: ["address-owner"], lineageId: "lin_main" as Id, generation: 2, updatedAt: "2026-10-31T09:00:00.000Z" }),
      inst({ id: "ins_old", grants: ["address-owner"], lineageId: "lin_main" as Id, generation: 1, fencedAt: T0, updatedAt: "2026-10-31T11:00:00.000Z" }),
      inst({ id: "ins_other", updatedAt: "2026-10-31T10:00:00.000Z" }),
    ];
    const result = justDoneRows(rows);
    expect(result.shown.map((row) => row.id)).toEqual(["ins_other", "ins_current"]);
  });

  it("drops child sessions when the surface lists top-level sessions only", () => {
    const rows = [
      inst({ id: "ins_parent" }),
      inst({ id: "ins_child", parent: { instanceId: "ins_parent", runId: "run_1", commandId: "cmd_1" } }),
    ];
    expect(justDoneRows(rows, (row) => row.parent == null).total).toBe(1);
  });
});
