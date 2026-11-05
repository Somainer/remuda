import { useState } from "react";
import { Link } from "react-router-dom";
import { hubStore, useHub } from "../../lib/store";
import { useNowTick } from "../../lib/useNowTick";
import type { Id } from "../../types/wire";
import { HOME_FRESHNESS, useHomeFreshness } from "./freshness";
import { justDoneRows } from "./justDone";
import { useOwnerGlancePoll } from "./useOwnerGlance";
import css from "./justDone.module.css";

/**
 * ⑤ 刚完成 (home.md §7, §9).
 *
 * Sits under the queue on both surfaces. Phone: up to 3 rows. Desktop: up to 3
 * inline items, then 「+N」. Both list EXACTLY the sessions ◉ counted — the owner
 * glance's set (`justDoneRows`), so the heading number is the segment's.
 * Hidden while there is no snapshot or glance yet, while either Hub read is
 * stale, and while the set is empty — never an empty state.
 *
 * 打开 navigates AND marks the row seen at the click, so it drops out of ◉
 * before the page settles (the session page's own reporter covers it again
 * while it stays visible). 标为已看 marks one row; 全部看过 marks every listed
 * row, including those folded behind 「+N」.
 */
export function JustDone({ placement }: { placement: "phone" | "desktop" }) {
  const hub = useHub();
  const freshness = useHomeFreshness();
  const nowMs = useNowTick(5_000);
  const [expanded, setExpanded] = useState(false);
  // Refcounted: the section reads the same one poll as ①.
  useOwnerGlancePoll();
  const done = justDoneRows(hub.glance);

  // §9: hidden while loading, while stale, and while empty. A foreground grace
  // is not stale — the rows stay. A failed or aged glance is stale too: its
  // rows are as frozen as ①'s greyed numbers.
  const glanceStale =
    hub.glanceFailed || (hub.glanceAt !== null && nowMs - hub.glanceAt > HOME_FRESHNESS.staleAfterMs);
  if (freshness.state === "unknown" || freshness.state === "stale" || glanceStale || done.rows.length === 0) {
    return null;
  }

  const visible = expanded ? done.rows : done.shown;
  // Past the Hub's row cap the count runs on without ids: name the rest.
  const unlisted = expanded ? done.total - done.rows.length : 0;
  return (
    <section className={css.section} data-testid="home-just-done" data-placement={placement} aria-label="刚完成">
      <header className={css.head}>
        <span className={css.heading}>刚完成 · {done.total}</span>
        <button type="button" className={css.markAll} data-testid="just-done-mark-all" onClick={() => markAll(done.rows)}>
          全部看过
        </button>
      </header>
      <ul className={placement === "desktop" ? css.inline : css.list}>
        {visible.map((id) => (
          <li key={id} className={css.item} data-testid="just-done-row" data-instance-id={id}>
            <Link className={css.open} data-testid="just-done-open" to={`/s/${id}`} onClick={() => markOne(id)}>
              <span className={css.mark} aria-hidden="true">◉</span>
              <span className={css.title}>{hubStore.titleOf(id)}</span>
            </Link>
            <button type="button" className={css.seen} data-testid="just-done-mark" onClick={() => markOne(id)}>
              标为已看
            </button>
          </li>
        ))}
      </ul>
      {!expanded && done.overflow > 0 ? (
        <button type="button" className={css.more} data-testid="just-done-more" onClick={() => setExpanded(true)}>
          +{done.overflow}
        </button>
      ) : null}
      {unlisted > 0 ? (
        <span className={css.more} data-testid="just-done-unlisted">
          +{unlisted}
        </span>
      ) : null}
    </section>
  );
}

/** Mark one row seen. A list action reports without a visibility lease. */
function markOne(id: string) {
  void hubStore.markSeen(id as Id).catch(() => undefined);
}

/** 全部看过: one seen write per listed row, including the rows behind +N. */
function markAll(ids: readonly string[]) {
  for (const id of ids) markOne(id);
}
