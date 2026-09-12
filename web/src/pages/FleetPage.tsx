import { useEffect } from "react";
import { Link } from "react-router-dom";
import { FleetBoard, fleetStore, useFleets } from "../features/fleet";
import { useHostViews } from "../features/hosts";
import { hubStore, useHub } from "../lib/store";
import css from "../features/hosts/hosts.module.css";

export function FleetPage() {
  const hub = useHub();
  const hosts = useHostViews(hub.hosts, hub.instances);
  useEffect(() => {
    fleetStore.seed(hub.instances, hosts, (id) => hubStore.titleOf(id));
  }, [hub.instances, hosts]);
  const fleets = useFleets();

  return (
    <div className={css.page} data-testid="fleet-page">
      <p>
        <Link to="/hosts">← 主机</Link>
      </p>
      <h1 style={{ fontSize: 18 }}>Fleet</h1>
      <p className={css.meta}>跨主机聚合。各 Instance seq 独立，不假设跨主机因果序。</p>
      <FleetBoard fleets={fleets} hosts={hosts} />
    </div>
  );
}
