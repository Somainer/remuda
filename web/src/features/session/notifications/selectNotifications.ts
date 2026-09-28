/**
 * In-app session notifications.
 *
 * A harness `Notification` hook is an advisory, not a turn transition: the
 * idle prompt ("waiting for your input") fires *after* the turn ends and a
 * permission hint points at a dialog that the blocking `PermissionRequest`
 * hook owns. The Node therefore no longer folds it into the live phase (see
 * `remuda_signal::map`); instead it journals the native `Notification`
 * lifecycle and the web surfaces it here — a toast plus a small dismissible
 * list on the session page — while the existing OS/push path is untouched.
 *
 * Pure selector: observations in, stable notification rows out.
 */
import type { Observation } from "../../../types/generated";

export type SessionNotification = {
  /** Stable id within the session: the journal seq. */
  id: string;
  /** The human-readable line (`message`), falling back to the type. */
  text: string;
  /** `notification_type` when the harness supplied one (e.g. idle_prompt). */
  notificationType: string | null;
  /** A hint that points at a blocking dialog, not just an idle prompt. */
  pointsAtDialog: boolean;
  /** RFC3339-ms the Node observed it. */
  at: string;
};

const DIALOG_TYPES = new Set(["permission_prompt", "permission", "plan_prompt"]);

function isNotification(ev: Observation): boolean {
  return (
    ev.kind === "lifecycle" &&
    ev.payload.type === "native" &&
    ev.payload.nativeName === "Notification"
  );
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function seqNum(id: string): bigint {
  try {
    return BigInt(id);
  } catch {
    return 0n;
  }
}

/**
 * The notification's consumed tags as validated strings. `relatedIds` is
 * untrusted wire data: a `message: 42` tag reached `message.trim()` and threw;
 * malformed tags are dropped, and the row falls back to its type / 「通知」.
 */
function notificationTags(ev: Observation): { message: string | null; notificationType: string | null } {
  if (!isNotification(ev)) return { message: null, notificationType: null };
  const tags = asRecord(ev.payload.type === "native" ? ev.payload.relatedIds : null) ?? {};
  const message = tags.message;
  const notificationType = tags.notificationType;
  return {
    message: typeof message === "string" ? message : null,
    notificationType: typeof notificationType === "string" ? notificationType : null,
  };
}

/** All Notification advisories for one session, seq-ordered oldest first. */
export function selectSessionNotifications(
  events: readonly Observation[],
): SessionNotification[] {
  const rows: SessionNotification[] = [];
  for (const ev of events) {
    if (!isNotification(ev)) continue;
    const { message, notificationType } = notificationTags(ev);
    const text = message?.trim() || notificationType || "通知";
    rows.push({
      id: String(ev.seq),
      text,
      notificationType,
      pointsAtDialog: notificationType !== null && DIALOG_TYPES.has(notificationType),
      at: ev.observedAt,
    });
  }
  rows.sort((a, b) => {
    const sa = seqNum(a.id);
    const sb = seqNum(b.id);
    return sa < sb ? -1 : sa > sb ? 1 : 0;
  });
  return rows;
}

function interactionIdOf(ev: Observation): string | null {
  const payload = asRecord(ev.payload);
  if (!payload) return null;
  if (ev.kind === "interaction.requested") {
    const interaction = asRecord(payload.interaction);
    const id = interaction?.id;
    return typeof id === "string" ? id : null;
  }
  const id = payload.interactionId;
  return typeof id === "string" ? id : null;
}

/**
 * Seqs of dialog-pointing advisories whose dialog has CLOSED. The retirement
 * is keyed to the interactions that were actually open when the advisory
 * landed — never to the global "is any dialog pending" boolean: once
 * permission A is answered its advisory stays retired even when an unrelated
 * interaction B opens later (it used to resurrect and link to B's card). An
 * advisory with no open interaction in the journal (none seen yet, or a tail
 * that truncated the request) is retired immediately, matching the original
 * "no dialog pending" behaviour for that row.
 */
export function selectRetiredDialogAdvisories(events: readonly Observation[]): Set<string> {
  const open = new Set<string>();
  const closed = new Set<string>();
  const snapshots = new Map<string, readonly string[]>();
  const ordered = [...events].sort((a, b) => {
    const sa = seqNum(String(a.seq));
    const sb = seqNum(String(b.seq));
    return sa < sb ? -1 : sa > sb ? 1 : 0;
  });
  for (const ev of ordered) {
    if (ev.kind === "interaction.requested") {
      const id = interactionIdOf(ev);
      if (id) open.add(id);
    } else if (ev.kind === "interaction.answered" || ev.kind === "interaction.expired") {
      const id = interactionIdOf(ev);
      if (id) {
        open.delete(id);
        closed.add(id);
      }
    } else if (ev.kind === "lifecycle") {
      const { notificationType } = notificationTags(ev);
      if (notificationType !== null && DIALOG_TYPES.has(notificationType)) {
        snapshots.set(String(ev.seq), [...open]);
      }
    }
  }
  const retired = new Set<string>();
  for (const [seq, ids] of snapshots) {
    if (ids.every((id) => closed.has(id))) retired.add(seq);
  }
  return retired;
}
