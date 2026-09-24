/**
 * In-app notifications for one session (ui-spec §2.2 dock item 3): exactly
 * ONE quiet 24px row — the newest advisory — with the rest collapsed behind
 * an inline 「+N」 expander. A permission_prompt row links to the pending
 * dialog card; once that dialog is answered/expired (or the session itself
 * ends) the row collapses: stale 「waiting for your input」 rows from hours
 * ago never stack above the composer again (UO-6b owner defect).
 *
 * The toast is a transient emphasis of a notification arriving WHILE the
 * page is mounted; backfilling a journal never replays it. It anchors under
 * the top chrome, so on a 390px phone it can never cover the composer. This
 * deliberately does not touch the existing push/OS notification path.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { formatClock } from "../../../lib/format";
import type { Observation } from "../../../types/generated";
import { selectSessionNotifications, type SessionNotification } from "./selectNotifications";
import { dismissNotification, useDismissedNotifications } from "./dismissed";
import { sessionSettlement } from "../live/LiveStatusStrip";
import css from "./notifications.module.css";

const TOAST_MS = 8000;

function seqNum(id: string): bigint {
  try {
    return BigInt(id);
  } catch {
    return 0n;
  }
}

/**
 * Whether a blocking dialog is currently open anywhere on the session, folded
 * from the same journal the notifications come from: a requested interaction
 * stays open until its own answered/expired observation closes it. A
 * permission_prompt notification whose dialog has closed is a stale advisory.
 */
function dialogIsOpen(events: readonly Observation[]): boolean {
  const open = new Set<string>();
  const ordered = [...events].sort((a, b) => {
    const sa = seqNum(String(a.seq));
    const sb = seqNum(String(b.seq));
    return sa < sb ? -1 : sa > sb ? 1 : 0;
  });
  for (const ev of ordered) {
    if (ev.kind === "interaction.requested") {
      const id = (ev.payload as { interaction?: { id?: string } }).interaction?.id;
      if (id) open.add(id);
    } else if (ev.kind === "interaction.answered" || ev.kind === "interaction.expired") {
      const id = (ev.payload as { interactionId?: string }).interactionId;
      if (id) open.delete(id);
    }
  }
  return open.size > 0;
}

export function SessionNotifications({
  instanceId,
  events,
}: {
  instanceId: string;
  events: readonly Observation[];
}) {
  const all = useMemo(() => selectSessionNotifications(events), [events]);
  const settlement = useMemo(() => sessionSettlement(events), [events]);
  const dialogOpen = useMemo(() => dialogIsOpen(events), [events]);
  const dismissedIds = useDismissedNotifications(instanceId);
  const undismissed = all.filter((n) => !dismissedIds.has(n.id));

  // A row is moot when the whole session has ended, or it pointed at a dialog
  // that has since been answered/expired. On an ended session the single
  // quiet row remains (as history), just muted and without its dialog link.
  const isMoot = (n: SessionNotification) =>
    !settlement.ended && n.pointsAtDialog && !dialogOpen;
  const rows = settlement.ended ? undismissed : undismissed.filter((n) => !isMoot(n));
  const newest = rows.at(-1) ?? null;
  const older = newest ? rows.slice(0, -1) : [];

  const [expanded, setExpanded] = useState(false);
  // Session end always re-collapses the stack to the one quiet row.
  useEffect(() => {
    if (settlement.ended) setExpanded(false);
  }, [settlement.ended]);
  const shown = newest ? (expanded ? rows : [newest]) : [];

  // Toast only for a notification that arrived while this page was mounted —
  // backfilling a journal must not replay old rows as popups — and never for
  // a session that has already ended.
  const seenAtMount = useRef<Set<string>>(new Set(all.map((n) => n.id)));
  const [toastId, setToastId] = useState<string | null>(null);
  useEffect(() => {
    const candidate = all.at(-1);
    if (
      candidate &&
      !settlement.ended &&
      !seenAtMount.current.has(candidate.id) &&
      !dismissedIds.has(candidate.id)
    ) {
      seenAtMount.current.add(candidate.id);
      setToastId(candidate.id);
    }
  }, [all, dismissedIds, settlement.ended]);
  useEffect(() => {
    if (toastId === null) return;
    const timer = window.setTimeout(() => setToastId(null), TOAST_MS);
    return () => window.clearTimeout(timer);
  }, [toastId]);
  // The toast disappears with the state that produced it: the toasted row
  // gets dismissed, its dialog answered, or the session ends.
  const toastRow = toastId ? all.find((n) => n.id === toastId) ?? null : null;
  useEffect(() => {
    if (toastId === null) return;
    const moot = toastRow ? !settlement.ended && toastRow.pointsAtDialog && !dialogOpen : false;
    if (dismissedIds.has(toastId) || settlement.ended || moot) setToastId(null);
  }, [toastId, toastRow, dismissedIds, settlement.ended, dialogOpen]);

  if (shown.length === 0 && toastId === null) return null;

  const toast = toastId ? all.find((n) => n.id === toastId) ?? null : null;

  const goToDialog = () => {
    document
      .querySelector<HTMLElement>("[data-testid='approval-card'], [data-testid='question-form']")
      ?.scrollIntoView({ behavior: "smooth", block: "center" });
  };

  return (
    <>
      <div className={css.panel} data-testid="session-notifications" data-settled={settlement.ended ? "1" : undefined}>
        {shown.map((n: SessionNotification) => (
          <div
            className={settlement.ended ? `${css.row} ${css.rowSettled}` : css.row}
            key={n.id}
            data-testid="session-notification"
            data-type={n.notificationType ?? undefined}
            data-settled={settlement.ended ? "1" : undefined}
          >
            <span className={css.time}>{formatClock(n.at)}</span>
            <span className={css.text}>{n.text}</span>
            {n.pointsAtDialog && !settlement.ended ? (
              <button type="button" className={css.link} onClick={goToDialog} data-testid="notification-goto-dialog">
                查看待处理对话
              </button>
            ) : null}
            <button
              type="button"
              className={css.dismiss}
              aria-label="忽略通知"
              title="忽略"
              data-testid="notification-dismiss"
              onClick={() => dismissNotification(instanceId, n.id)}
            >
              ×
            </button>
          </div>
        ))}
        {!expanded && older.length > 0 ? (
          <div className={css.row} data-testid="session-notification-more-row">
            <button
              type="button"
              className={css.expand}
              data-testid="notification-older"
              aria-expanded={false}
              onClick={() => setExpanded(true)}
            >
              +{older.length}
            </button>
          </div>
        ) : null}
        {expanded && older.length > 0 ? (
          <button
            type="button"
            className={css.expand}
            data-testid="notification-collapse"
            aria-expanded={true}
            onClick={() => setExpanded(false)}
          >
            收起
          </button>
        ) : null}
      </div>
      {toast ? (
        <div className={css.toast} data-testid="notification-toast" role="status">
          <span className={css.toastText}>{toast.text}</span>
          <button
            type="button"
            className={css.dismiss}
            aria-label="关闭通知"
            onClick={() => {
              dismissNotification(instanceId, toast.id);
              setToastId(null);
            }}
          >
            ×
          </button>
        </div>
      ) : null}
    </>
  );
}
