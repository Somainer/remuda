import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { formatListTime } from "../../lib/format";
import { projectStatus } from "../../lib/status";
import { hubStore, useHub } from "../../lib/store";
import { useNowTick } from "../../lib/useNowTick";
import type { Id } from "../../types/wire";
import { RowActions, ROW_ACTIONS_HOST } from "./RowActions";
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
  // A menu id whose row left the PAINTED slice has no menu to close it: a
  // later re-rank would otherwise pop it open by itself. (Checking all working
  // rows is not enough — the row can stay working behind 「+N」.)
  useEffect(() => {
    if (menuFor === null) return;
    if (rows.slice(0, WORKING_ROWS).some((row) => row.id === menuFor)) return;
    setMenuFor(null);
  }, [menuFor, rows]);
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
            <li key={row.id} className={`${css.row} ${ROW_ACTIONS_HOST}`} data-testid="home-working-row">
              <Link className={css.rowLink} to={`/s/${row.id}`}>
                <StateDot status="working" />
                <span className={css.title}>{hubStore.titleOf(row.id as Id)}</span>
                <span className={css.time}>{formatListTime(row.updatedAt, nowMs)}</span>
              </Link>
              <RowActions
                instance={row}
                open={menuFor === row.id}
                onOpen={() => setMenuFor(row.id)}
                onClose={() => setMenuFor((current) => (current === row.id ? null : current))}
                stale={stale}
              />
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
