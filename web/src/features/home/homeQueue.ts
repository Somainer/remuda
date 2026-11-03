import type { Instance } from "../../types/instance";
import type { DecisionOption, Interaction, InteractionAnswer } from "../../types/interaction";
import type { Task } from "../../types/generated";
import { HubHttpError } from "../../lib/httpError";
import { deriveInboxQueue, type InboxQueueItem } from "../mobile/inboxRows";
import { ownerBlockedTasks, type GlanceSource } from "./glance";
import type { OwnerNotice } from "./notices";

/**
 * The ④ 「需要你」 queue (home.md §4), as pure derivations: which cards it
 * holds, in which order, and what a home card may do with each interaction.
 *
 * Membership is shared, never re-derived: the interaction cards are the
 * inbox's own `deriveInboxQueue()` (so a ghost card past its deadline is not
 * here either), and the 「任务待你」 cards are glance.ts's `ownerBlockedTasks()`
 * — the same B the ① count adds. The header number is `glanceCounts()`, so the
 * queue, the count row and the OS badge read one §5.1 N.
 */

/**
 * home.md §4.2 card kinds. A home card grants less than the inbox (§4.3):
 *  - approval: 拒绝 and 允许一次 only, NEVER allow-session;
 *  - simple-question: one chip per option plus 「其他…」;
 *  - question: 去回答 (the session page form);
 *  - plan: review-only — 审阅计划, no approve anywhere on the home;
 *  - tty: the screen's last 3 lines and 去终端;
 *  - sensitive (a sensitive field, or an elicitation): 去会话 only.
 */
export type HomeCardKind = "approval" | "simple-question" | "question" | "plan" | "tty" | "sensitive";

/** §4.2 简单问题: at most this many options become chips. */
export const HOME_CHIP_MAX = 4;

/**
 * §4.2 简单问题 (owner ruling 6): one field, single-select, 1–4 options, not
 * sensitive. The ONLY case a question is answered in a list (D-061); the
 * inbox keeps its own rules.
 */
export function isSimpleQuestion(item: Interaction): boolean {
  if (item.request.kind !== "question") return false;
  const fields = item.request.fields;
  if (fields.length !== 1) return false;
  const field = fields[0];
  return (
    field.input === "single-select"
    && field.options.length > 0
    && field.options.length <= HOME_CHIP_MAX
    && !field.sensitive
  );
}

function isSensitive(item: Interaction): boolean {
  if (item.request.kind === "elicitation") return true;
  return item.request.kind === "question" && item.request.fields.some((field) => field.sensitive);
}

/**
 * Classify one queued interaction for the home card. Sensitive wins over every
 * other rule (a sensitive field never gets a chip); a native-tty prompt the
 * Hub cannot answer structurally, or one with no options at all, is a
 * terminal prompt whatever its kind.
 */
export function homeCardKind(item: Interaction): HomeCardKind {
  if (isSensitive(item)) return "sensitive";
  if (item.carrier === "native-tty" && !item.answerable) return "tty";
  if (item.request.kind === "approval") return "approval";
  if (item.request.kind === "plan-review") return "plan";
  if (item.request.kind === "question") {
    const hasOptions = item.request.fields.some((field) => field.options.length > 0);
    if (item.carrier === "native-tty" && !hasOptions) return "tty";
    if (item.answerable && isSimpleQuestion(item)) return "simple-question";
    return "question";
  }
  return "sensitive";
}

/**
 * §4.2 权限: the two options a home approval card may render — the `deny`
 * one and the `allow-once` one, verbatim. Null when either is missing (the
 * card then offers only 去收件箱). allow-session is never returned.
 */
export function homeApprovalOptions(
  item: Interaction,
): { deny: DecisionOption; allowOnce: DecisionOption } | null {
  if (item.request.kind !== "approval") return null;
  const deny = item.request.options.find((option) => option.effect === "deny");
  const allowOnce = item.request.options.find((option) => option.effect === "allow-once");
  return deny && allowOnce ? { deny, allowOnce } : null;
}

/** The answer one chip of a simple question submits (null for anything else). */
export function chipAnswer(item: Interaction, optionId: string): InteractionAnswer | null {
  if (!isSimpleQuestion(item) || item.request.kind !== "question") return null;
  const field = item.request.fields[0];
  if (!field.options.some((option) => option.id === optionId)) return null;
  return { kind: "question", answers: { [field.id]: { optionIds: [optionId], text: null } } };
}

/** At most the last `count` non-blank lines, for the tty card (§4.2). */
export function lastLines(lines: readonly string[], count = 3): string[] {
  return lines.filter((line) => line.trim().length > 0).slice(-count);
}

export type HomeQueueCard =
  | { type: "interaction"; id: string; entry: InboxQueueItem; kind: HomeCardKind }
  | {
      type: "task";
      id: string;
      task: Task;
      /** Where 「打开任务」 lands: its primary session, else the project board. */
      primarySessionId: string | null;
    }
  | { type: "notice"; id: string; notice: OwnerNotice }
  | { type: "failed"; id: string; instance: Instance; text: string; at: string };

export type HomeQueue = {
  /** §4.1 order: interactions (inbox order), 任务待你, 告诉你, then 回合失败. */
  cards: HomeQueueCard[];
  /** Cards that count toward N (§4.1 kinds 1–3); equals glanceCounts().needYou. */
  counted: number;
  /** 去收件箱 targets the earliest interaction; null = no interaction card. */
  earliestInteractionId: string | null;
};

/** The inbox 待你处理 order (inboxRows byRecency): newest first, id tiebreak. */
function byRecency(a: Interaction, b: Interaction): number {
  const ta = Date.parse(a.createdAt);
  const tb = Date.parse(b.createdAt);
  if (ta !== tb) return tb - ta;
  return a.id < b.id ? 1 : a.id > b.id ? -1 : 0;
}

function primarySessionOf(task: Task, instances: readonly Instance[]): string | null {
  const sessions = instances.filter((instance) => instance.taskId === task.id);
  const placed = task.placement?.instanceId;
  if (placed && sessions.some((session) => session.id === placed)) return placed;
  const live = sessions.find((session) => session.lifecycle !== "exited" && session.lifecycle !== "failed");
  return (live ?? sessions[0])?.id ?? null;
}

/**
 * home.md §4.5 after seen-hub: a failed turn is a card until the owner's seen
 * position reaches its end. `lastTurnError` clears when the next turn starts,
 * so the latest settled turn end IS the failed one, and `unseen` says whether
 * the owner has seen it. Not counted in N.
 */
export function failedTurnCards(instances: readonly Instance[]): HomeQueueCard[] {
  return instances
    .flatMap((instance): HomeQueueCard[] => {
      const text = instance.lastTurnError?.text?.trim();
      if (!text || instance.unseen !== true) return [];
      return [{ type: "failed", id: instance.id, instance, text, at: instance.lastTurnError?.at ?? "" }];
    })
    .sort((a, b) => {
      const at = a.type === "failed" ? a.at : "";
      const bt = b.type === "failed" ? b.at : "";
      if (at !== bt) return at < bt ? 1 : -1;
      return a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
    });
}

/**
 * §4.1 kind 4 「告诉你」 (D-060 §11.5): the unacknowledged `owner notify`
 * notices, newest first. Not counted in N (§5.1).
 */
export function noticeCards(notices: readonly OwnerNotice[]): HomeQueueCard[] {
  return notices
    .filter((notice) => !notice.ackedAt)
    .map((notice): HomeQueueCard => ({ type: "notice", id: notice.noticeId, notice }))
    .sort((a, b) => {
      const at = a.type === "notice" ? a.notice.createdAt : "";
      const bt = b.type === "notice" ? b.notice.createdAt : "";
      if (at !== bt) return at < bt ? 1 : -1;
      return a.id < b.id ? 1 : a.id > b.id ? -1 : 0;
    });
}

/**
 * Build the ④ queue (home.md §4.1). Main's question pin (home-main-extras)
 * is just an interaction here; 「告诉你」 cards come from `notices` and sit
 * after 任务待你, uncounted.
 */
export function deriveHomeQueue(source: GlanceSource, notices: readonly OwnerNotice[] = []): HomeQueue {
  const queue = deriveInboxQueue({
    interactions: [...source.interactions],
    instances: [...source.instances],
    hosts: [...source.hosts],
    answering: source.answering,
    deviceId: source.deviceId,
    nowMs: source.nowMs,
  });
  const interactionCards: HomeQueueCard[] = [...queue]
    .sort((a, b) => byRecency(a.item, b.item))
    .map((entry) => ({ type: "interaction", id: entry.item.id, entry, kind: homeCardKind(entry.item) }));
  const taskCards: HomeQueueCard[] = ownerBlockedTasks(source.tasks, source.instances, queue).map((task) => ({
    type: "task",
    id: task.id,
    task,
    primarySessionId: primarySessionOf(task, source.instances),
  }));
  let earliest: Interaction | null = null;
  for (const { item } of queue) {
    if (!earliest || byRecency(item, earliest) > 0) earliest = item;
  }
  return {
    cards: [...interactionCards, ...taskCards, ...noticeCards(notices), ...failedTurnCards(source.instances)],
    counted: interactionCards.length + taskCards.length,
    earliestInteractionId: earliest?.id ?? null,
  };
}

/** §4.1 collapsed sizes: 3 cards on the phone, 4 (a 2×2 grid) on desktop. */
export const HOME_QUEUE_LIMIT = { phone: 3, desktop: 4 } as const;

/** Inbox deep link for 去收件箱 (phone `/m/inbox`, desktop `/approvals`). */
export function inboxHref(placement: "phone" | "desktop", interactionId: string | null): string {
  const base = placement === "phone" ? "/m/inbox" : "/approvals";
  return interactionId ? `${base}?focus=${encodeURIComponent(interactionId)}` : base;
}

/** §4.4: the answer receipt a card shows for 2 s before it leaves. */
export type Receipt = "submitted" | "elsewhere";

/** §4.4 receipt wording. */
export const HOME_RECEIPT_TEXT: Record<Receipt, string> = {
  submitted: "已提交",
  elsewhere: "已在其他设备处理",
};

/** §8: what a stale queue says next to its greyed buttons. */
export const HOME_STALE_TEXT = "数据可能过期";

/**
 * §4.4: 409 or 410 means another device (or another answer) got there first,
 * or the interaction already left the queue. A 409 HOST_OFFLINE is not that —
 * the interaction is still waiting — so it stays an inline error.
 */
export function answeredElsewhere(error: unknown): boolean {
  if (!(error instanceof HubHttpError)) return false;
  if (error.status === 410) return true;
  return error.status === 409 && error.code !== "HOST_OFFLINE";
}
