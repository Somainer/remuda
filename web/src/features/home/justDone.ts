import type { Instance } from "../../types/instance";
import { liveChildParents, rank } from "../mobile/homeRows";
import { showsDoneUnseen } from "./glance";

/** Rows the section paints before 「+N」 (home.md §7, §9). */
export const JUST_DONE_SHOWN = 3;

export type JustDone = {
  /** Every qualifying row, newest first. */
  rows: Instance[];
  /** The first {@link JUST_DONE_SHOWN} of them. */
  shown: Instance[];
  /** How many rows sit behind 「+N」. */
  overflow: number;
  total: number;
};

/**
 * ⑤ 刚完成 (home.md §7). The rows that rank done-unseen and that this surface
 * lists, newest first — the same rank and the same current-chapter rule the
 * ◉ count uses, so the section and the segment cannot disagree.
 *
 * `listed` narrows to the rows the surface shows: desktop passes top-level
 * sessions, the phone ledger keeps the default (every session).
 */
export function justDoneRows(
  instances: readonly Instance[],
  listed: (instance: Instance) => boolean = () => true,
): JustDone {
  const childLive = liveChildParents(instances);
  const rows = instances
    .filter((instance) => listed(instance))
    .filter((instance) => rank(instance, childLive.has(instance.id)) === "done-unseen")
    .filter((instance) => showsDoneUnseen(instance, instances))
    .sort((a, b) => {
      const time = (Date.parse(b.updatedAt) || 0) - (Date.parse(a.updatedAt) || 0);
      return time !== 0 ? time : a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
    });
  return {
    rows,
    shown: rows.slice(0, JUST_DONE_SHOWN),
    overflow: Math.max(0, rows.length - JUST_DONE_SHOWN),
    total: rows.length,
  };
}
