import { ownerBlockedTasks, type GlanceSource } from "./glance";
import { deriveInboxQueue } from "../mobile/inboxRows";
import { liveChildParents, rank } from "../mobile/homeRows";

/**
 * ⑨'s 快捷提问 chips (home.md §6.3, task home-main-extras).
 *
 * Candidates are exactly the four §6.3 names. A chip carries a number ONLY
 * when an existing derivation yields a real count: 谁在等我 reads the ①
 * 「需要你」 N (§5.1), and 哪些卡住了 reads the blocked/stalled count (sessions
 * ranked blocked plus owner-blocked tasks with no waiting interaction — the
 * same cards ④ paints). Without a snapshot both are unknown and no number is
 * painted (§0.3: an unknown is never 0).
 *
 * Which candidates Main's grants can answer (the mapping this task states):
 *  - 谁在等我 needs the owner-scope interaction list, which only the
 *    `address-owner` seat reads — every resolved Main holds it by definition.
 *  - 哪些卡住了 and 昨晚跑完了什么 read the task board / instance list inside
 *    Main's own scope; no extra grant is required.
 *  - 今天落地了什么 asks about landings, so it is listed only for a Main that
 *    holds `land` (OA2: a seat's other grants are chosen at seating).
 *
 * A tap fills the input and never sends (Composer's `suggestions` path).
 */

export type QuickAsk = {
  /** Stable id for tests and the chip's data attribute. */
  id: "who-waits" | "last-night" | "stuck" | "landed";
  /** The question as listed; also the text a tap fills in. */
  text: string;
  /** What the chip paints: the question, plus a real count when there is one. */
  label: string;
};

export const QUICK_ASKS: readonly { id: QuickAsk["id"]; text: string; grant: string | null }[] = [
  { id: "who-waits", text: "谁在等我", grant: "address-owner" },
  { id: "last-night", text: "昨晚跑完了什么", grant: null },
  { id: "stuck", text: "哪些卡住了", grant: null },
  { id: "landed", text: "今天落地了什么", grant: "land" },
];

/**
 * The chips to paint for `grants`. A question whose grant the seat does not
 * hold is not listed at all, and its number is then not computed either.
 */
export function quickAskChips(input: {
  grants: readonly string[] | null | undefined;
  needYou: number | null;
  stuck: number | null;
}): QuickAsk[] {
  const grants = input.grants ?? [];
  const chips: QuickAsk[] = [];
  for (const candidate of QUICK_ASKS) {
    if (candidate.grant !== null && !grants.includes(candidate.grant)) continue;
    const count =
      candidate.id === "who-waits" ? input.needYou : candidate.id === "stuck" ? input.stuck : null;
    chips.push({
      id: candidate.id,
      text: candidate.text,
      // §0.3: a known zero gets no number, and neither does an unknown.
      label: count !== null && count > 0 ? `${candidate.text} ${count}` : candidate.text,
    });
  }
  return chips;
}

/**
 * The blocked/stalled count behind 哪些卡住了: sessions whose rank is blocked
 * (they wait on a pending interaction) plus owner-blocked tasks whose sessions
 * have no such interaction — the 「任务待你」 cards. The two sets are disjoint
 * by construction (§4.6), so nothing is counted twice.
 */
export function stuckCount(source: GlanceSource): number {
  const queue = deriveInboxQueue({
    interactions: [...source.interactions],
    instances: [...source.instances],
    hosts: [...source.hosts],
    answering: source.answering,
    deviceId: source.deviceId,
    nowMs: source.nowMs,
  });
  const childLive = liveChildParents(source.instances);
  const blockedSessions = source.instances.filter(
    (instance) => rank(instance, childLive.has(instance.id)) === "blocked",
  ).length;
  return blockedSessions + ownerBlockedTasks(source.tasks, source.instances, queue).length;
}
