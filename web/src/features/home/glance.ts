import type { Host, Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import type { Task } from "../../types/generated";
import { resolveMain } from "../../lib/store";
import { deriveInboxQueue, type InboxQueueItem } from "../mobile/inboxRows";
import { liveChildParents, rank } from "../mobile/homeRows";
import { dispatchedChild } from "../../lib/dispatched";

/**
 * The ① count row (home.md §5.2). Everything here is pure so the phone head,
 * the desktop PageHeader band and H2's queue/badge can share one derivation
 * later: `glanceCounts()` implements the §5.1 rules over the three existing
 * reads (instances, interactions, tasks), and `glancePieces()` turns counts
 * into the segments the row paints.
 *
 * Numbers never come from decoration: a segment with no source is absent
 * (今日完成 belongs to home-glance-d060), a known zero segment is hidden, an
 * unknown number is 「–」 and anything above 99 is 99+.
 */

export type GlanceSource = {
  instances: readonly Instance[];
  interactions: readonly Interaction[];
  hosts: readonly Host[];
  tasks: readonly Task[];
  answering: Record<string, true>;
  deviceId: string;
  nowMs: number;
};

/**
 * The rows the unfiltered `/sessions` board lists: every session except the
 * ones an Agent dispatched (`dispatchedChild`); a resume stays on the board.
 */
export const TOP_LEVEL_WORKING = (instance: Instance): boolean => !dispatchedChild(instance);

export type GlanceCounts = {
  /** §5.1 N = A (interactions really waiting on the owner) + B (owner-blocked tasks with no such interaction). */
  needYou: number;
  /** §5.2 ◉: instances ranked done-unseen (§3.1 rank 2), current chapter only. */
  doneUnseen: number;
  /** §5.2 ●: instances ranked working (§3.1 rank 3). */
  working: number;
  /**
   * §5.2 今日完成: sessions whose latest settled root turn ended in the owner's
   * local day. Desktop only, and hidden while unknown — the client receives it
   * from `GET /v1/owner/glance` (home-glance-d060), so an older Hub that omits
   * it simply never paints the segment.
   */
  doneToday?: number;
};

/**
 * home.md §5.1: the count the row's first segment and the OS badge share.
 *
 * A is `deriveInboxQueue()` — the one projection the inbox and every in-app
 * badge already read, so a ghost card whose deadline passed is not counted.
 * B is an unarchived task with a blocked reason whose sessions have no
 * interaction in that queue (§4.6: the interaction card and the task card are
 * never both shown, so the task is never counted next to its interaction).
 * Notices and failed turns are deliberately absent: a failed turn is a queue
 * card, never ◉.
 */
export function glanceCounts(
  source: GlanceSource,
  /**
   * The instances the calling surface actually LISTS for 工作中 (default: all
   * of them). Desktop passes top-level sessions; the phone ledger includes
   * child sessions, so it keeps the default.
   */
  workingInstances: readonly Instance[] = source.instances,
): GlanceCounts {
  const queue = deriveInboxQueue({
    interactions: [...source.interactions],
    instances: [...source.instances],
    hosts: [...source.hosts],
    answering: source.answering,
    deviceId: source.deviceId,
    nowMs: source.nowMs,
  });
  const blockedTasks = ownerBlockedTasks(source.tasks, source.instances, queue).length;

  const childLive = liveChildParents(source.instances);
  const seen = new Set<string>();
  let working = 0;
  let doneUnseen = 0;
  for (const instance of workingInstances) {
    if (seen.has(instance.id)) continue;
    seen.add(instance.id);
    const homeRank = rank(instance, childLive.has(instance.id));
    if (homeRank === "working") working += 1;
    else if (homeRank === "done-unseen" && showsDoneUnseen(instance, source.instances)) doneUnseen += 1;
  }
  return { needYou: queue.length + blockedTasks, doneUnseen, working };
}

/**
 * home.md §7: only Main's current chapter shows ◉. A session that holds the
 * address-owner grant, or that shares the Main lineage, is counted only when
 * it IS the chapter `resolveMain` addresses; an older or fenced chapter of
 * that lineage is not. An ordinary session (no grant, another lineage) is
 * counted whenever it ranks done-unseen.
 */
export function showsDoneUnseen(instance: Instance, instances: readonly Instance[]): boolean {
  const main = resolveMain(instances);
  if (main === null) return true;
  const lineage = instance.lineageId ?? instance.id;
  const inMainLineage = lineage === main.lineageId || instance.grants?.includes("address-owner") === true;
  if (!inMainLineage) return true;
  return instance.id === main.instance.id;
}

/**
 * home.md §4.6 / §5.1 B: unarchived tasks with a blocked reason none of whose
 * sessions has an interaction in the 待你处理 queue. The ONE derivation both
 * the count (glanceCounts) and the queue's 「任务待你」 cards read, so the
 * header number and the cards can never disagree. Oldest task first.
 */
export function ownerBlockedTasks(
  tasks: readonly Task[],
  instances: readonly Instance[],
  queue: readonly InboxQueueItem[],
): Task[] {
  const queuedInstanceIds = new Set(queue.map((row) => row.item.instanceId));
  const sessionsByTask = new Map<string, string[]>();
  for (const instance of instances) {
    if (!instance.taskId) continue;
    const list = sessionsByTask.get(instance.taskId) ?? [];
    list.push(instance.id);
    sessionsByTask.set(instance.taskId, list);
  }
  return tasks
    .filter((task) => {
      if (task.archivedAt != null) return false;
      if (!task.blockedReason?.trim()) return false;
      const sessions = sessionsByTask.get(task.id) ?? [];
      return !sessions.some((id) => queuedInstanceIds.has(id));
    })
    .sort((a, b) => (a.createdAt < b.createdAt ? -1 : a.createdAt > b.createdAt ? 1 : a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

/** The largest number the row spells out; above it the segment reads 99+. */
export const GLANCE_COUNT_MAX = 99;

export function formatGlanceCount(count: number): string {
  return count > GLANCE_COUNT_MAX ? `${GLANCE_COUNT_MAX}+` : String(count);
}

/** 「早上好」 05–12, 「下午好」 12–18, 「晚上好」 18–05 (local hour). */
export function greetingForHour(hour: number): string {
  if (hour >= 5 && hour < 12) return "早上好";
  if (hour >= 12 && hour < 18) return "下午好";
  return "晚上好";
}

/** 「更新于 HH:MM」 for the stale row, on the local clock. */
export function formatUpdatedAt(ms: number): string {
  return new Date(ms).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
}

/**
 * One painted piece of the row. `null` counts mean "no snapshot yet" and
 * render 「–」; a known zero is not a piece at all (it is hidden).
 */
export type GlancePiece =
  | { key: "needYou"; count: number | null }
  | { key: "doneUnseen"; count: number | null }
  | { key: "working"; count: number | null }
  | { key: "doneToday"; count: number }
  | { key: "greeting"; text: string }
  | { key: "hosts"; text: string };

export type GlanceLineInput = {
  /**
   * null before the first Hub snapshot. `doneUnseen` is optional so a caller
   * that only knows the older two counts still type-checks; an omitted count
   * reads as 0.
   */
  counts: (Omit<GlanceCounts, "doneUnseen"> & { doneUnseen?: number }) | null;
  /** Local hour, injected so the greeting is testable. */
  hour: number;
  hostsOnline: number;
  hostsTotal: number;
  /**
   * The 工作中 rank filter is applied. A known-zero segment is normally hidden
   * (§5.2), but the active filter needs its control on screen to be cleared —
   * so it stays, at 0, until the second tap.
   */
  workingFilter?: boolean;
  /**
   * The 完成未看 rank filter is applied. Same rule as `workingFilter`: the
   * active filter needs its control on screen to be cleared, so a known-zero
   * ◉ segment stays until the second tap.
   */
  doneUnseenFilter?: boolean;
  /**
   * §5.2: 今日完成 is desktop-only. On the phone the segment is absent
   * whatever the count is.
   */
  showDoneToday?: boolean;
};

/**
 * home.md §5.2 段的取舍:
 *  - before the first snapshot every segment reads 「–」;
 *  - a queue of zero replaces the first segment with the greeting and the
 *    remaining non-zero segments; when those are zero too, the line ends with
 *    「N/M 主机在线」 (skipped when no host is registered at all).
 */
export function glancePieces(input: GlanceLineInput): GlancePiece[] {
  if (input.counts === null) {
    return [
      { key: "needYou", count: null },
      { key: "doneUnseen", count: null },
      { key: "working", count: null },
    ];
  }
  const { needYou, working } = input.counts;
  const doneUnseen = input.counts.doneUnseen ?? 0;
  const pieces: GlancePiece[] = [];
  if (needYou > 0) pieces.push({ key: "needYou", count: needYou });
  else pieces.push({ key: "greeting", text: greetingForHour(input.hour) });
  // §5.2 order: 需要你 · 完成未看 · 工作中.
  if (doneUnseen > 0 || input.doneUnseenFilter === true) pieces.push({ key: "doneUnseen", count: doneUnseen });
  if (working > 0 || input.workingFilter === true) pieces.push({ key: "working", count: working });
  // §5.2 今日完成: desktop only, hidden while unknown (`doneToday` absent), and
  // a known zero is hidden like every other segment.
  const doneToday = input.counts.doneToday ?? null;
  const paintedDoneToday = input.showDoneToday === true && doneToday !== null && doneToday > 0;
  if (paintedDoneToday && doneToday !== null) pieces.push({ key: "doneToday", count: doneToday });
  // The host tail closes a line that otherwise says nothing but the greeting.
  // A zero ◉ segment kept alive only because its filter is active still leaves
  // the line empty of news, so the tail stays; a painted ● segment does not.
  if (
    needYou === 0 &&
    doneUnseen === 0 &&
    !(working > 0 || input.workingFilter === true) &&
    !paintedDoneToday &&
    input.hostsTotal > 0
  ) {
    pieces.push({ key: "hosts", text: `${input.hostsOnline}/${input.hostsTotal} 主机在线` });
  }
  return pieces;
}

/** The row's one-line text, for tests and the accessible name. */
export function glanceText(pieces: readonly GlancePiece[]): string {
  return pieces
    .map((piece) => {
      if (piece.key === "needYou") {
        return `⚠${piece.count === null ? "–" : formatGlanceCount(piece.count)} 需要你`;
      }
      if (piece.key === "doneUnseen") {
        return `◉${piece.count === null ? "–" : formatGlanceCount(piece.count)} 完成未看`;
      }
      if (piece.key === "working") {
        return `●${piece.count === null ? "–" : formatGlanceCount(piece.count)} 工作中`;
      }
      if (piece.key === "doneToday") {
        return `今日完成 ${formatGlanceCount(piece.count)}`;
      }
      if (piece.key === "greeting") return `${piece.text} · 都处理完了`;
      return piece.text;
    })
    .join(" · ");
}
