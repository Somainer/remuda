import { Fragment, useMemo } from "react";
import { useHub } from "../../lib/store";
import { useNowTick } from "../../lib/useNowTick";
import { hostIsOnline } from "../hosts/model";
import { HOME_FRESHNESS, useHomeFreshness } from "./freshness";
import { useOwnerGlancePoll } from "./useOwnerGlance";
import {
  formatGlanceCount,
  formatUpdatedAt,
  glancePieces,
  type GlanceCounts,
} from "./glance";
import css from "./homeGlance.module.css";

/**
 * ① 计数行 (home.md §5.1–5.2, §5.3).
 *
 * Phone: the 28px row under the 52px title row, sticky together with it (80px
 * total). Desktop: the same line inside the PageHeader, plus the desktop-only
 * 「今日完成 N」 segment.
 *
 * home-glance-d060: every count reads ONE `GET /v1/owner/glance` per poll —
 * the projection home.md §5.3 defines over Main's subtree plus the sessions
 * the owner opened by hand. Nothing here derives a number from the instance
 * list any more, so the row cannot disagree with the Hub (or with
 * `remuda glance`); before the first glance the segments read 「–」 and
 * 今日完成 stays hidden, exactly as §5.2 asks.
 *
 * Taps: 需要你 scrolls to the queue; 完成未看 and 工作中 each filter the ledger
 * to that rank, and a second tap clears it.
 */
export function HomeGlance({
  placement,
  onNeedYou,
  onWorking,
  onDoneUnseen,
  workingFilter = false,
  doneUnseenFilter = false,
}: {
  placement: "phone" | "desktop";
  onNeedYou: () => void;
  onWorking: () => void;
  /** Narrow the ledger to the done-unseen rank; a second tap clears it. */
  onDoneUnseen?: () => void;
  /** The rank filter this segment currently applies, for aria-pressed. */
  workingFilter?: boolean;
  /** The done-unseen rank filter this segment currently applies. */
  doneUnseenFilter?: boolean;
}) {
  const hub = useHub();
  const freshness = useHomeFreshness();
  // A local clock for the glance-age check: the store emits on every poll, but
  // a hung poll would otherwise never re-render this row.
  const nowMs = useNowTick(5_000);
  // §5.3: the glance is polled only while this home surface is mounted, and
  // once per cycle; the mount read closes the cold-bootstrap 「–」 window.
  useOwnerGlancePoll();
  const counts = useMemo<GlanceCounts | null>(() => {
    const glance = hub.glance;
    // A store that somehow holds a partial projection paints 「–」 rather than
    // crashing the home (refreshGlance rejects malformed bodies at the
    // boundary; this is the belt and braces).
    if (!glance || !glance.counts || typeof glance.counts.needYou !== "number") return null;
    return {
      needYou: glance.counts.needYou,
      doneUnseen: glance.counts.doneUnseen,
      working: glance.counts.working,
      doneToday: glance.counts.doneToday,
    };
  }, [hub.glance]);

  const hostsOnline = hub.hosts.filter((host) => hostIsOnline(host)).length;
  const pieces = glancePieces({
    counts,
    hour: new Date().getHours(),
    hostsOnline,
    hostsTotal: hub.hosts.length,
    // Keep the segment on screen while it is the only way to clear the filter.
    workingFilter,
    doneUnseenFilter,
    // §5.2: 今日完成 is desktop-only.
    showDoneToday: placement === "desktop",
  });
  // §8: stale greys the whole row and names the last successful update. A
  // foreground grace is explicitly NOT stale. A glance-only failure (the main
  // refresh still healthy) freezes these numbers, so it greys the row and
  // names the last GOOD glance — frozen numbers are never shown as fresh.
  const glanceStale =
    hub.glanceFailed ||
    (hub.glanceAt !== null && nowMs - hub.glanceAt > HOME_FRESHNESS.staleAfterMs);
  const stale = freshness.state === "stale" || (counts !== null && glanceStale);
  const updatedAt = hub.glanceAt ?? freshness.lastOkAt;

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
          ) : piece.key === "doneUnseen" ? (
            <button
              type="button"
              className={css.seg}
              data-testid="glance-done-unseen"
              data-key="doneUnseen"
              aria-pressed={doneUnseenFilter}
              onClick={onDoneUnseen}
            >
              <span className={css.glyph} aria-hidden="true">
                ◉
              </span>
              {piece.count === null ? "–" : formatGlanceCount(piece.count)} 完成未看
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
          ) : piece.key === "doneToday" ? (
            <span className={css.doneToday} data-testid="glance-done-today" data-key="doneToday">
              今日完成 {formatGlanceCount(piece.count)}
            </span>
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
      {stale && updatedAt !== null ? (
        <span className={css.updated} data-testid="glance-updated">
          更新于 {formatUpdatedAt(updatedAt)}
        </span>
      ) : null}
    </div>
  );
}
