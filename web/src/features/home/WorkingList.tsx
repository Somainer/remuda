import { useState } from "react";
import { Link } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { formatListTime } from "../../lib/format";
import { projectStatus } from "../../lib/status";
import { hubStore, useHub } from "../../lib/store";
import { useNowTick } from "../../lib/useNowTick";
import type { Id } from "../../types/wire";
import css from "./workingList.module.css";

/** Rows shown before 「+N ›」 (home.md §9 ⑥). */
const WORKING_ROWS = 5;

/**
 * ⑥ 「工作中」 (home.md §2.2): the desktop right column's list of running
 * sessions at ≥1440, newest first, five rows then 「+N ›」 to the list filtered
 * on 工作中. Each row opens /s/:id; its ⋯ holds 中断 (the turn only — the
 * process keeps running). Stale data greys out while the Hub link is not live.
 */
export function WorkingList() {
  const hub = useHub();
  const nowMs = useNowTick();
  const [menuFor, setMenuFor] = useState<string | null>(null);
  const rows = hub.instances
    .filter((row) => row.parent == null && projectStatus(row) === "working")
    .sort((a, b) => (Date.parse(b.updatedAt) || 0) - (Date.parse(a.updatedAt) || 0));
  const stale = hub.connection !== "live";
  return (
    <section className={css.section} data-testid="home-working" data-stale={stale ? "1" : "0"} aria-label="工作中">
      <header className={css.head}>工作中 · {hub.ready ? rows.length : "–"}</header>
      {!hub.ready ? (
        <div className={css.skeleton} aria-hidden="true" />
      ) : rows.length === 0 ? (
        <p className={css.empty}>没有在跑的会话</p>
      ) : (
        <ul className={css.list}>
          {rows.slice(0, WORKING_ROWS).map((row) => (
            <li key={row.id} className={css.row} data-testid="home-working-row">
              <Link className={css.rowLink} to={`/s/${row.id}`}>
                <StateDot status="working" />
                <span className={css.title}>{hubStore.titleOf(row.id as Id)}</span>
                <span className={css.time}>{formatListTime(row.updatedAt, nowMs)}</span>
              </Link>
              <button
                type="button"
                className={css.more}
                aria-label="更多操作"
                aria-expanded={menuFor === row.id}
                data-testid="home-working-more"
                onClick={() => setMenuFor((current) => (current === row.id ? null : row.id))}
              >
                ⋯
              </button>
              {menuFor === row.id ? (
                <div className={css.menu} role="menu">
                  <button
                    type="button"
                    role="menuitem"
                    className={css.menuItem}
                    data-testid="home-working-interrupt"
                    onClick={() => {
                      setMenuFor(null);
                      void hubStore.cancel(row.id as Id).catch(() => undefined);
                    }}
                  >
                    中断
                  </button>
                </div>
              ) : null}
            </li>
          ))}
        </ul>
      )}
      {rows.length > WORKING_ROWS ? (
        <Link className={css.overflow} to="/sessions?status=working&scope=all" data-testid="home-working-overflow">
          +{rows.length - WORKING_ROWS} ›
        </Link>
      ) : null}
    </section>
  );
}
