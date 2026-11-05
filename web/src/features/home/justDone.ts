import type { OwnerGlance } from "../../types/generated";
import type { Id } from "../../types/wire";

/** Rows the section paints before 「+N」 (home.md §7, §9). */
export const JUST_DONE_SHOWN = 3;

export type JustDone = {
  /** Every listed session, newest first: the glance's ◉ set. */
  rows: Id[];
  /** The first {@link JUST_DONE_SHOWN} of them. */
  shown: Id[];
  /** How many of the ◉ count sit behind 「+N」 while the section is folded. */
  overflow: number;
  /** The ◉ count itself, which the heading names. */
  total: number;
};

/**
 * ⑤ 刚完成 (home.md §7). EXACTLY the sessions the ◉ segment counted: the owner
 * glance's `doneUnseenIds` (newest first) under its `counts.doneUnseen`. Both
 * placements read the same set, so the section and the segment agree at every
 * width. It is never a second derivation over the instance list, which spans
 * more than the owner scope.
 *
 * The list stops at the Hub's row cap while the count stays the total, so the
 * total is never smaller than the list.
 */
export function justDoneRows(glance: Pick<OwnerGlance, "counts" | "doneUnseenIds"> | null): JustDone {
  const rows = (Array.isArray(glance?.doneUnseenIds) ? glance.doneUnseenIds : []) as Id[];
  const total = Math.max(glance?.counts.doneUnseen ?? 0, rows.length);
  const shown = rows.slice(0, JUST_DONE_SHOWN);
  return { rows, shown, overflow: Math.max(0, total - shown.length), total };
}
