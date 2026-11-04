import { useEffect, useState } from "react";
import { Link } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { formatListTime } from "../../lib/format";
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
 *
 * home-glance-d060: the membership, the count and the row's one-line headline
 * all come from the SAME `GET /v1/owner/glance` the ① row reads (home.md
 * §5.3) — never from a second per-row projection. The store row remains the
 * source for the live status dot and the ⋯ menu, so the list still reacts to
 * frames those rows carry.
 */
export function WorkingList() {
  const hub = useHub();
  const nowMs = useNowTick();
  const [menuFor, setMenuFor] = useState<string | null>(null);
  const glance = hub.glance;
  const byId = new Map(hub.instances.map((row) => [row.id as string, row]));
  const rows = (glance?.working ?? []).flatMap((row) =>
    byId.has(row.instanceId) ? [{ glance: row, instance: byId.get(row.instanceId)! }] : [],
  );
  const total = glance ? glance.counts.working : null;
  const stale = hub.connection !== "live";
  // A menu id whose row left the PAINTED slice has no menu to close it: a
  // later re-rank would otherwise pop it open by itself. (Checking all working
  // rows is not enough — the row can stay working behind 「+N」.)
  useEffect(() => {
    if (menuFor === null) return;
    if (rows.slice(0, WORKING_ROWS).some((row) => row.instance.id === menuFor)) return;
    setMenuFor(null);
  }, [menuFor, rows]);
  return (
    <section className={css.section} data-testid="home-working" data-stale={stale ? "1" : "0"} aria-label="工作中">
      <header className={css.head}>工作中 · {total ?? "–"}</header>
      {total === null ? (
        <div className={css.skeleton} aria-hidden="true" />
      ) : total === 0 ? (
        <p className={css.empty}>没有在跑的会话</p>
      ) : rows.length === 0 ? (
        // A non-zero count with no paintable row (the session row has not
        // arrived yet): never claim nothing is running under a number that
        // says otherwise.
        <div className={css.skeleton} aria-hidden="true" data-testid="home-working-pending" />
      ) : (
        <ul className={css.list}>
          {rows.slice(0, WORKING_ROWS).map((row) => (
            <li key={row.instance.id} className={`${css.row} ${ROW_ACTIONS_HOST}`} data-testid="home-working-row">
              <Link className={css.rowLink} to={`/s/${row.instance.id}`} title={row.glance.headline}>
                <StateDot status="working" />
                <span className={css.title}>{hubStore.titleOf(row.instance.id as Id)}</span>
                <span className={css.time}>{formatListTime(row.instance.updatedAt, nowMs)}</span>
              </Link>
              <RowActions
                instance={row.instance}
                open={menuFor === row.instance.id}
                onOpen={() => setMenuFor(row.instance.id)}
                onClose={() => setMenuFor((current) => (current === row.instance.id ? null : current))}
                stale={stale}
              />
            </li>
          ))}
        </ul>
      )}
      {total !== null && total > WORKING_ROWS ? (
        <Link className={css.overflow} to="/sessions?status=working&scope=all" data-testid="home-working-overflow">
          +{total - WORKING_ROWS} ›
        </Link>
      ) : null}
    </section>
  );
}
