import type { Host, Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import type { Task } from "../../types/generated";
import { deriveInboxQueue } from "../mobile/inboxRows";
import { liveChildParents, rank } from "../mobile/homeRows";

/**
 * The ① count row (home.md §5.2). Everything here is pure so the phone head,
 * the desktop PageHeader band and H2's queue/badge can share one derivation
 * later: `glanceCounts()` implements the §5.1 rules over the three existing
 * reads (instances, interactions, tasks), and `glancePieces()` turns counts
 * into the segments the row paints.
 *
 * Numbers never come from decoration: a segment with no source is absent
 * (◉ 完成未看 belongs to seen-web, 今日完成 to home-glance-d060), a known zero
 * segment is hidden, an unknown number is 「–」 and anything above 99 is 99+.
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

export type GlanceCounts = {
  /** §5.1 N = A (interactions really waiting on the owner) + B (owner-blocked tasks with no such interaction). */
  needYou: number;
  /** §5.2 ●: instances ranked working (§3.1 rank 3). */
  working: number;
};

/**
 * home.md §5.1: the count the row's first segment and the OS badge share.
 *
 * A is `deriveInboxQueue()` — the one projection the inbox and every in-app
 * badge already read, so a ghost card whose deadline passed is not counted.
 * B is an unarchived task with a blocked reason whose sessions have no
 * interaction in that queue (§4.6: the interaction card and the task card are
 * never both shown, so the task is never counted next to its interaction).
 * 完成未看, notices and failed turns are deliberately absent.
 */
export function glanceCounts(source: GlanceSource): GlanceCounts {
  const queue = deriveInboxQueue({
    interactions: [...source.interactions],
    instances: [...source.instances],
    hosts: [...source.hosts],
    answering: source.answering,
    deviceId: source.deviceId,
    nowMs: source.nowMs,
  });
  const queuedInstanceIds = new Set(queue.map((row) => row.item.instanceId));
  const sessionsByTask = new Map<string, string[]>();
  for (const instance of source.instances) {
    if (!instance.taskId) continue;
    const list = sessionsByTask.get(instance.taskId) ?? [];
    list.push(instance.id);
    sessionsByTask.set(instance.taskId, list);
  }
  let blockedTasks = 0;
  for (const task of source.tasks) {
    if (task.archivedAt != null) continue;
    if (!task.blockedReason?.trim()) continue;
    const sessions = sessionsByTask.get(task.id) ?? [];
    if (sessions.some((id) => queuedInstanceIds.has(id))) continue;
    blockedTasks += 1;
  }

  const childLive = liveChildParents(source.instances);
  const seen = new Set<string>();
  let working = 0;
  for (const instance of source.instances) {
    if (seen.has(instance.id)) continue;
    seen.add(instance.id);
    if (rank(instance, childLive.has(instance.id)) === "working") working += 1;
  }
  return { needYou: queue.length + blockedTasks, working };
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
  | { key: "working"; count: number | null }
  | { key: "greeting"; text: string }
  | { key: "hosts"; text: string };

export type GlanceLineInput = {
  /** null before the first Hub snapshot. */
  counts: GlanceCounts | null;
  /** Local hour, injected so the greeting is testable. */
  hour: number;
  hostsOnline: number;
  hostsTotal: number;
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
      { key: "working", count: null },
    ];
  }
  const { needYou, working } = input.counts;
  const pieces: GlancePiece[] = [];
  if (needYou > 0) pieces.push({ key: "needYou", count: needYou });
  else pieces.push({ key: "greeting", text: greetingForHour(input.hour) });
  if (working > 0) pieces.push({ key: "working", count: working });
  else if (needYou === 0 && input.hostsTotal > 0) {
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
      if (piece.key === "working") {
        return `●${piece.count === null ? "–" : formatGlanceCount(piece.count)} 工作中`;
      }
      if (piece.key === "greeting") return `${piece.text} · 都处理完了`;
      return piece.text;
    })
    .join(" · ");
}
