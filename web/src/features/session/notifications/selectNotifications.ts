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

function seqNum(id: string): bigint {
  try {
    return BigInt(id);
  } catch {
    return 0n;
  }
}

/** All Notification advisories for one session, seq-ordered oldest first. */
export function selectSessionNotifications(
  events: readonly Observation[],
): SessionNotification[] {
  const rows: SessionNotification[] = [];
  for (const ev of events) {
    if (!isNotification(ev)) continue;
    const tags = (
      ev.payload.type === "native" ? ev.payload.relatedIds ?? {} : {}
    ) as Record<string, string>;
    const notificationType = tags.notificationType ?? null;
    const text = tags.message?.trim() || notificationType || "通知";
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
