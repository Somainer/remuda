/**
 * Owner alerts for questions (c-question-alert, OWNER-REQUESTED).
 *
 * When an agent asks a question (AskUserQuestion `question`, plus
 * `elicitation` and `plan-review`; approvals keep their existing approval
 * flow), the owner who is NOT looking at that session must get a visible
 * alert. This module is the pure, testable core:
 *
 *  - {@link QUESTION_ALERT_KINDS} decides which interaction kinds alert;
 *  - {@link selectQuestionAlerts} turns the live interaction list into the
 *    alert rows, suppressing ones the owner is already answering / viewing and
 *    anything that is not a live pending question;
 *  - {@link formatQuestionCountdown} renders the deadline countdown.
 *
 * The reactive store that watches the hub state and posts a toast / title
 * badge lives in `questionAlertStore.ts`; this file has no React and no
 * globals, so arrival / suppression / replay rules are unit-testable with
 * plain data.
 */
import type { Host, Instance } from "../../types/instance";
import type { Interaction } from "../../types/interaction";
import { projectInteraction } from "../../lib/interactionStatus";

/** Interaction kinds that raise a question alert. Approvals are excluded:
 * they already have their own approval surfacing. */
export const QUESTION_ALERT_KINDS = new Set<Interaction["kind"]>([
  "question",
  "elicitation",
  "plan-review",
]);

/** A pending question the owner should be alerted about. */
export type QuestionAlert = {
  interactionId: Id;
  instanceId: Id;
  /** Session title for the alert line ("Agent <title> …"). */
  title: string;
  /** RFC3339 deadline; null when the interaction reported none. */
  deadline: string | null;
};

type Id = string;

export type QuestionAlertSource = {
  interactions: readonly Interaction[];
  instances: readonly Instance[];
  /** Host records, keyed by id, for the connectivity projection. */
  hosts?: readonly Host[];
  /**
   * The session the owner is currently viewing, if any. A question on that
   * session is answered in place and raises no global alert. `null` = the
   * owner is not viewing a session.
   */
  activeSessionId: Id | null;
  /** Interactions the owner is submitting an answer for on this device. */
  answering?: ReadonlySet<Id>;
  /** Resolve a session's display title. */
  titleOf: (instanceId: Id) => string;
  /** Clock injection for tests; defaults to wall time. */
  nowMs?: number;
};

/**
 * The pending questions that deserve an alert right now.
 *
 * Suppression rules (owner request):
 *  - only {@link QUESTION_ALERT_KINDS}, and only rows that project to
 *    `pending` (not expired/settled/paused/superseded);
 *  - no alert for a question on the session the owner is actively viewing;
 *  - no alert while the owner is answering it on this device.
 *
 * History replay: this selector is fed the LIVE interaction list by
 * `questionAlertStore`, which diffs against already-seen ids and is fed only
 * fresh arrivals (a page hydration is marked `replay`). Replayed rows never
 * enter the store, so this selector alone never re-alerts for history; the
 * id-seen gate lives in the store, not here.
 */
export function selectQuestionAlerts(source: QuestionAlertSource): QuestionAlert[] {
  const nowMs = source.nowMs ?? Date.now();
  const instanceById = new Map(source.instances.map((instance) => [instance.id, instance]));
  const hostById = new Map((source.hosts ?? []).map((host) => [host.id, host]));
  const alerts: QuestionAlert[] = [];
  for (const item of source.interactions) {
    if (!QUESTION_ALERT_KINDS.has(item.kind)) continue;
    if (item.instanceId === source.activeSessionId) continue;
    if (source.answering?.has(item.id)) continue;
    const instance = instanceById.get(item.instanceId);
    const host = hostById.get(item.hostId);
    const uiState = projectInteraction(item, {
      host,
      connectivity: instance?.connectivity,
      nowMs,
    });
    if (uiState !== "pending") continue;
    alerts.push({
      interactionId: item.id,
      instanceId: item.instanceId,
      title: source.titleOf(item.instanceId),
      deadline: item.deadline.state === "known" ? item.deadline.value : null,
    });
  }
  return alerts;
}

/**
 * Render the remaining-time line for a question deadline.
 *
 * Returns e.g. "还剩 12 分钟，超时将自动拒绝". Returns null when there is no
 * known deadline or it has already passed (the row then renders its expired
 * state, not a countdown).
 */
export function formatQuestionCountdown(
  deadline: string | null | undefined,
  nowMs: number = Date.now(),
): string | null {
  if (!deadline) return null;
  const at = Date.parse(deadline);
  if (!Number.isFinite(at)) return null;
  const remaining = at - nowMs;
  if (remaining <= 0) return null;
  const minutes = Math.floor(remaining / 60_000);
  if (minutes >= 1) return `还剩 ${minutes} 分钟，超时将自动拒绝`;
  const seconds = Math.max(1, Math.ceil(remaining / 1000));
  return `还剩 ${seconds} 秒，超时将自动拒绝`;
}
