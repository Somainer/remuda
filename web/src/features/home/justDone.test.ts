import { describe, expect, it } from "vitest";
import { withoutDoneUnseen } from "../../lib/store";
import type { OwnerGlance } from "../../types/generated";
import { justDoneRows, JUST_DONE_SHOWN } from "./justDone";

/**
 * ⑤ 刚完成 (home.md §7, §9): the rows the section lists and how many it shows.
 * They are the owner glance's ◉ set — the Hub ranks and scopes them (the rank,
 * the current-chapter rule and the owner scope are pinned in owner_glance.rs).
 * Pure — the component only paints this.
 */

function glance(doneUnseen: number, ids: string[]): Pick<OwnerGlance, "counts" | "doneUnseenIds"> {
  return { counts: { needYou: 0, doneUnseen, working: 0, doneToday: 0 }, doneUnseenIds: ids };
}

describe("justDoneRows", () => {
  it("lists the glance's ◉ set in its order, and shows three plus the rest", () => {
    const result = justDoneRows(glance(4, ["ins_new", "ins_mid", "ins_old", "ins_fourth"]));
    expect(result.total).toBe(4);
    expect(result.rows).toEqual(["ins_new", "ins_mid", "ins_old", "ins_fourth"]);
    expect(result.shown).toEqual(["ins_new", "ins_mid", "ins_old"]);
    expect(result.overflow).toBe(1);
    expect(JUST_DONE_SHOWN).toBe(3);
  });

  it("shows nothing before the first glance or when ◉ is empty", () => {
    for (const result of [justDoneRows(null), justDoneRows(glance(0, []))]) {
      expect(result.total).toBe(0);
      expect(result.shown).toEqual([]);
      expect(result.overflow).toBe(0);
    }
  });

  it("keeps the count as the total past the Hub's row cap", () => {
    const ids = Array.from({ length: 50 }, (_, index) => `ins_${index}`);
    const result = justDoneRows(glance(52, ids));
    expect(result.total).toBe(52);
    expect(result.rows).toHaveLength(50);
    expect(result.overflow).toBe(49);
  });

  it("follows a seen mark in the list and the count at once", () => {
    const before = { ...glance(3, ["ins_a", "ins_b", "ins_c"]) } as Pick<OwnerGlance, "counts" | "doneUnseenIds">;
    const after = withoutDoneUnseen(before, new Set(["ins_b", "ins_unlisted"]));
    expect(justDoneRows(after)).toMatchObject({ total: 2, rows: ["ins_a", "ins_c"] });
    // Nothing listed: the same object, so no re-render.
    expect(withoutDoneUnseen(before, new Set(["ins_x"]))).toBe(before);
  });
});
