import { Link } from "react-router-dom";
import { StateDot } from "../../components/StateDot";
import { formatListTime } from "../../lib/format";
import { hubStore, useHub, useMainEntry } from "../../lib/store";
import { useNowTick } from "../../lib/useNowTick";
import { useLineageSync } from "./useMain";
import { mainStatus } from "./mainStatus";
import css from "./mainRow.module.css";

/**
 * D-057 §9 / ui-spec §2.1: the Main row pinned at the top of the desktop
 * session list — status dot + 「Main」 + one status sentence + time, opening
 * the resolved chapter's shared /s/:id. Same resolution as the home card
 * (store.resolveMain); nothing renders without an address-owner lineage.
 */
export function MainRow() {
  const entry = useMainEntry();
  const hub = useHub();
  const nowMs = useNowTick();
  useLineageSync(entry);
  if (!entry) return null;
  const instance = entry.instance;
  const status = mainStatus({
    entry,
    lineage: hub.lineages[entry.lineageId] ?? null,
    host: hub.hosts.find((row) => row.id === instance.hostId),
    hostName: hubStore.hostName(instance.hostId),
    asking: hub.interactions.some((row) => row.instanceId === instance.id && row.state === "pending"),
    summary: hub.summaries[instance.id],
    nowMs,
  });
  return (
    <Link
      to={`/s/${instance.id}`}
      className={css.row}
      data-testid="main-row"
      data-phase={status.phase}
      data-tone={status.tone}
      data-main-id={instance.id}
    >
      <StateDot status={status.dot} />
      <span className={css.name}>Main</span>
      <span className={css.phrase} data-testid="main-row-status" title={status.phrase}>
        {status.phrase}
      </span>
      {entry.generation > 1 ? <span className={css.meta}>第 {entry.generation} 章</span> : null}
      <span className={css.time}>{formatListTime(instance.updatedAt, nowMs)}</span>
    </Link>
  );
}
