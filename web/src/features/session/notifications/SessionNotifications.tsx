/**
 * In-app notifications for one session: a transient toast for each new
 * Notification advisory plus a small dismissible list. A permission_prompt row
 * links to the pending dialog card. This deliberately does not touch the
 * existing push/OS notification path.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { formatClock } from "../../../lib/format";
import type { Observation } from "../../../types/generated";
import { selectSessionNotifications, type SessionNotification } from "./selectNotifications";
import { dismissNotification, useDismissedNotifications } from "./dismissed";
import css from "./notifications.module.css";

const TOAST_MS = 8000;

export function SessionNotifications({
  instanceId,
  events,
}: {
  instanceId: string;
  events: readonly Observation[];
}) {
  const all = useMemo(() => selectSessionNotifications(events), [events]);
  const dismissedIds = useDismissedNotifications(instanceId);
  const visible = all.filter((n) => !dismissedIds.has(n.id));

  // Toast only for a notification that arrived while this page was mounted —
  // backfilling a journal must not replay old rows as popups.
  const seenAtMount = useRef<Set<string>>(new Set(all.map((n) => n.id)));
  const [toastId, setToastId] = useState<string | null>(null);
  useEffect(() => {
    const newest = all.at(-1);
    if (newest && !seenAtMount.current.has(newest.id) && !dismissedIds.has(newest.id)) {
      seenAtMount.current.add(newest.id);
      setToastId(newest.id);
    }
  }, [all, dismissedIds]);
  useEffect(() => {
    if (toastId === null) return;
    const timer = window.setTimeout(() => setToastId(null), TOAST_MS);
    return () => window.clearTimeout(timer);
  }, [toastId]);
  // If the toasted row gets dismissed from the list, drop the toast too.
  useEffect(() => {
    if (toastId !== null && dismissedIds.has(toastId)) setToastId(null);
  }, [toastId, dismissedIds]);

  if (visible.length === 0 && toastId === null) return null;

  const toast = toastId ? all.find((n) => n.id === toastId) ?? null : null;

  const goToDialog = () => {
    document
      .querySelector<HTMLElement>("[data-testid='approval-card'], [data-testid='question-form']")
      ?.scrollIntoView({ behavior: "smooth", block: "center" });
  };

  return (
    <>
      <div className={css.panel} data-testid="session-notifications">
        {visible.map((n: SessionNotification) => (
          <div className={css.row} key={n.id} data-testid="session-notification" data-type={n.notificationType ?? undefined}>
            <span className={css.time}>{formatClock(n.at)}</span>
            <span className={css.text}>{n.text}</span>
            {n.pointsAtDialog ? (
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
