import { Fragment, useEffect, useMemo } from "react";
import { useHub } from "../../lib/store";
import { thisDeviceId } from "../../lib/interactionStatus";
import { hostIsOnline } from "../hosts/model";
import { useInboxClockNow } from "../mobile/useInboxPendingCount";
import { syncInboxDeadlineClock } from "../mobile/inboxClock";
import { useTaskLedger } from "../tasks/TaskList";
import { useHomeFreshness } from "./freshness";
import {
  formatGlanceCount,
  formatUpdatedAt,
  glanceCounts,
  glancePieces,
  type GlanceCounts,
} from "./glance";
import css from "./homeGlance.module.css";

/**
 * ① 计数行 (home.md §5.1–5.2).
 *
 * Phone: the 28px row under the 52px title row, sticky together with it (80px
 * total). Desktop: the same line inside the PageHeader. The counts are the
 * pure §5.1 derivation in `glance.ts` (H2's queue and the badge will read the
 * same function); this component only wires the store, the 5 s task ledger,
 * the inbox deadline clock and the §8 freshness verdict.
 *
 * Taps: 需要你 scrolls to the queue (today: the ledger's first blocked row —
 * the queue itself lands with H2); 工作中 filters the ledger to that rank and
 * a second tap clears it.
 */
export function HomeGlance({
  placement,
  onNeedYou,
  onWorking,
  workingFilter = false,
}: {
  placement: "phone" | "desktop";
  onNeedYou: () => void;
  onWorking: () => void;
  /** The rank filter this segment currently applies, for aria-pressed. */
  workingFilter?: boolean;
}) {
  const hub = useHub();
  const freshness = useHomeFreshness();
  const { tasks } = useTaskLedger();
  const deviceId = useMemo(() => thisDeviceId(), []);
  const nowMs = useInboxClockNow();
  useEffect(() => {
    syncInboxDeadlineClock(hub.interactions);
  }, [hub.interactions]);

  // The §5.1 derivation lives in glance.ts so H2's queue header and the OS
  // badge can read it later; the in-app badge is still A-only until
  // home-queue adds B. A store re-notification with unchanged counts
  // re-derives the same numbers and paints identically.
  const counts = useMemo<GlanceCounts | null>(
    () =>
      freshness.state === "unknown"
        ? null
        : glanceCounts({
            instances: hub.instances,
            interactions: hub.interactions,
            hosts: hub.hosts,
            tasks,
            answering: hub.answering,
            deviceId,
            nowMs,
          }),
    [freshness.state, hub.instances, hub.interactions, hub.hosts, tasks, hub.answering, deviceId, nowMs],
  );

  const hostsOnline = hub.hosts.filter((host) => hostIsOnline(host)).length;
  const pieces = glancePieces({
    counts,
    hour: new Date().getHours(),
    hostsOnline,
    hostsTotal: hub.hosts.length,
  });
  // §8: stale greys the whole row and names the last successful update. A
  // foreground grace is explicitly NOT stale.
  const stale = freshness.state === "stale";

  return (
    <div
      className={css.row}
      data-testid="home-glance"
      data-placement={placement}
      data-stale={stale ? "1" : undefined}
    >
      {pieces.map((piece, index) => (
        <Fragment key={piece.key}>
          {index > 0 ? (
            <span className={css.sep} aria-hidden="true">
              ·
            </span>
          ) : null}
          {piece.key === "needYou" ? (
            <button
              type="button"
              className={css.seg}
              data-testid="glance-need-you"
              data-key="needYou"
              onClick={onNeedYou}
            >
              <span className={css.glyph} aria-hidden="true">
                ⚠
              </span>
              {piece.count === null ? "–" : formatGlanceCount(piece.count)} 需要你
            </button>
          ) : piece.key === "working" ? (
            <button
              type="button"
              className={css.seg}
              data-testid="glance-working"
              data-key="working"
              aria-pressed={workingFilter}
              onClick={onWorking}
            >
              <span className={css.glyph} aria-hidden="true">
                ●
              </span>
              {piece.count === null ? "–" : formatGlanceCount(piece.count)} 工作中
            </button>
          ) : piece.key === "greeting" ? (
            <span className={css.greeting} data-testid="glance-greeting">
              {piece.text} · 都处理完了
            </span>
          ) : (
            <span className={css.hosts} data-testid="glance-hosts">
              {piece.text}
            </span>
          )}
        </Fragment>
      ))}
      {stale && freshness.lastOkAt !== null ? (
        <span className={css.updated} data-testid="glance-updated">
          更新于 {formatUpdatedAt(freshness.lastOkAt)}
        </span>
      ) : null}
    </div>
  );
}
