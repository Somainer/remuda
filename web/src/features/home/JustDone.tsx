import { useState } from "react";
import { Link } from "react-router-dom";
import { hubStore, useHub } from "../../lib/store";
import type { Id } from "../../types/wire";
import { useHomeFreshness } from "./freshness";
import { TOP_LEVEL_WORKING } from "./glance";
import { justDoneRows } from "./justDone";
import css from "./justDone.module.css";

/**
 * ⑤ 刚完成 (home.md §7, §9).
 *
 * Sits under the queue on both surfaces. Phone: up to 3 rows. Desktop: up to 3
 * inline items, then 「+N」. Hidden while there is no snapshot yet, while the
 * Hub read is stale, and while no row ranks done-unseen — never an empty state.
 *
 * 打开 navigates AND marks the row seen at the click, so it drops out of ◉
 * before the page settles (the session page's own reporter covers it again
 * while it stays visible). 标为已看 marks one row; 全部看过 marks every listed
 * row, including those folded behind 「+N」.
 */
export function JustDone({ placement }: { placement: "phone" | "desktop" }) {
  const hub = useHub();
  const freshness = useHomeFreshness();
  const [expanded, setExpanded] = useState(false);
  const listed = placement === "desktop" ? TOP_LEVEL_WORKING : undefined;
  const done = justDoneRows(hub.instances, listed);

  // §9: hidden while loading, while stale, and while empty. A foreground grace
  // is not stale — the rows stay.
  if (freshness.state === "unknown" || freshness.state === "stale" || done.total === 0) return null;

  const visible = expanded ? done.rows : done.shown;
  return (
    <section className={css.section} data-testid="home-just-done" data-placement={placement} aria-label="刚完成">
      <header className={css.head}>
        <span className={css.heading}>刚完成 · {done.total}</span>
        <button type="button" className={css.markAll} data-testid="just-done-mark-all" onClick={() => markAll(done.rows)}>
          全部看过
        </button>
      </header>
      <ul className={placement === "desktop" ? css.inline : css.list}>
        {visible.map((row) => (
          <li key={row.id} className={css.item} data-testid="just-done-row">
            <Link className={css.open} data-testid="just-done-open" to={`/s/${row.id}`} onClick={() => markOne(row.id)}>
              <span className={css.mark} aria-hidden="true">◉</span>
              <span className={css.title}>{hubStore.titleOf(row.id)}</span>
            </Link>
            <button type="button" className={css.seen} data-testid="just-done-mark" onClick={() => markOne(row.id)}>
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
    </section>
  );
}

/** Mark one row seen. A list action reports without a visibility lease. */
function markOne(id: string) {
  void hubStore.markSeen(id as Id).catch(() => undefined);
}

/** 全部看过: one seen write per listed row, including the rows behind +N. */
function markAll(rows: { id: string }[]) {
  for (const row of rows) markOne(row.id);
}
