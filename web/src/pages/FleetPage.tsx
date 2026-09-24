import { useEffect } from "react";
import { BroadcastBox, FleetBoard, fleetStore, useFleets } from "../features/fleet";
import { useHostViews } from "../features/hosts";
import { PageHeader } from "../components/PageHeader";
import { hubStore, useHub } from "../lib/store";
import css from "../features/fleet/fleet.module.css";

export function FleetPage() {
  const hub = useHub();
  const hosts = useHostViews(hub.hosts, hub.instances);
  useEffect(() => {
    fleetStore.seed(hub.instances, hosts, (id) => hubStore.titleOf(id));
  }, [hub.instances, hosts]);
  const fleets = useFleets();

  return (
    <div className={css.page} data-testid="fleet-page">
      <PageHeader crumbs={[{ label: "主机", to: "/hosts" }]} title="集群" />
      <div className={css.body}>
        <div className={css.inner}>
          <p className={css.intro}>跨主机聚合。各 Instance seq 独立，不假设跨主机因果序。</p>
          <BroadcastBox hosts={hosts} instances={hub.instances} />
          <FleetBoard fleets={fleets} hosts={hosts} />
        </div>
      </div>
    </div>
  );
}
