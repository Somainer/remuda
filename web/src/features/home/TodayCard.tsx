import { useState } from "react";
import { useMainEntry } from "../../lib/store";
import { useHomeFreshness } from "./freshness";
import { WakeSheet } from "./WakeSheet";
import { activeWakes, orderWakes, wakeClock, wakeLine, useMainWakes } from "./wakes";
import css from "./wakes.module.css";

/** Rows the 「今天」 card paints before folding the rest into one entry. */
export const TODAY_ROWS = 5;

/**
 * ⑩ 「今天」 (home.md §2.2, §9): the ≥1440 right-column card, between the
 * 问 Main input and ⑥ 工作中. It lists Main's next wake-ups and opens the same
 * sheet as the Main card's third line. Hidden when there is nothing: no Main,
 * no wake-up, or the first read has not answered yet (never a placeholder,
 * §0.3: an unknown is not a zero). The card greys under a stale read (§8) and
 * one tap opens the sheet, where a stale read also holds the cancel controls.
 */
export function TodayCard() {
  const entry = useMainEntry();
  const freshness = useHomeFreshness();
  const wakes = useMainWakes(entry?.lineageId ?? null);
  const [open, setOpen] = useState(false);
  if (!entry || !wakes.loaded) return null;
  const active = activeWakes(wakes.wakes);
  const line = wakeLine(wakes.wakes);
  if (!line) return null;
  const rows = orderWakes(active);
  const shown = rows.slice(0, TODAY_ROWS);
  const hidden = rows.length - shown.length;
  const stale = freshness.state === "stale";
  return (
    <section className={css.today} data-testid="home-today" data-stale={stale ? "1" : undefined} aria-label="今天">
      <header className={css.todayHead}>
        <h2 className={css.todayTitle} data-testid="home-today-title">
          今天
        </h2>
        <span className={css.todayCount} data-testid="home-today-count">
          共 {active.length} 个
        </span>
      </header>
      {shown.map((wake) => (
        <button
          key={wake.wakeId}
          type="button"
          className={css.todayRow}
          data-testid="home-today-row"
          data-wake-id={wake.wakeId}
          onClick={() => setOpen(true)}
        >
          <span className={css.todayClock} data-testid="home-today-row-clock">
            {wakeClock(wake.nextFireAt) ?? "—"}
          </span>
          <span className={css.todayName} title={wake.name}>
            {wake.name}
          </span>
        </button>
      ))}
      <button type="button" className={css.todayMore} data-testid="home-today-more" onClick={() => setOpen(true)}>
        {hidden > 0 ? `还有 ${hidden} 个 ›` : "全部 ›"}
      </button>
      <WakeSheet open={open} wakes={wakes.wakes} stale={stale} onClose={() => setOpen(false)} />
    </section>
  );
}
