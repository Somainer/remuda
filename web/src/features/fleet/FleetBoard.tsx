import { useState } from "react";
import { Link } from "react-router-dom";
import { Button } from "../../components/Button";
import { StateDot } from "../../components/StateDot";
import ui from "../../styles/ui.module.css";
import { PlacementPicker } from "../hosts/PlacementPicker";
import type { HostView, Placement } from "../hosts/model";
import { countFleet, type Fleet } from "./model";
import { fleetStore } from "./store";
import css from "./fleet.module.css";

export function FleetBoard({ fleets, hosts }: { fleets: Fleet[]; hosts: HostView[] }) {
  const [placement, setPlacement] = useState<Placement>({ kind: "any" });
  const [n, setN] = useState(2);
  const [error, setError] = useState<string | null>(null);

  return (
    <div data-testid="fleet-board">
      <section className={ui.card} style={{ marginBottom: 16 }}>
        <strong>创建 fleet</strong>
        <p className={ui.listMeta}>POST /v1/fleet/instances · 同一 spec 在 N 台主机各起一个 Instance</p>
        <PlacementPicker hosts={hosts} value={placement} onChange={setPlacement} />
        <label className={ui.field} style={{ marginTop: 8 }}>
          N
          <input
            className={ui.input}
            type="number"
            min={1}
            max={8}
            value={n}
            data-testid="fleet-n"
            onChange={(e) => setN(Number(e.target.value) || 1)}
          />
        </label>
        {error ? (
          <p data-testid="fleet-error" style={{ color: "var(--dust)" }}>
            {error}
          </p>
        ) : null}
        <div className={ui.row} style={{ marginTop: 8, justifyContent: "flex-end" }}>
          <Button
            variant="primary"
            data-testid="fleet-create"
            onClick={() => {
              setError(null);
              try {
                fleetStore.create(placement, hosts, n, "fleet batch");
              } catch (err) {
                setError(err instanceof Error ? err.message : "create failed");
              }
            }}
          >
            创建
          </Button>
        </div>
      </section>
      {fleets.map((fleet) => {
        const counts = countFleet(fleet.members);
        return (
          <section key={fleet.id} className={ui.card} style={{ marginBottom: 12 }} data-testid="fleet-card">
            <div className={ui.row} style={{ justifyContent: "space-between" }}>
              <strong>{fleet.label}</strong>
              <Button data-testid="fleet-cancel" onClick={() => fleetStore.cancel(fleet.id)}>
                广播 cancel
              </Button>
            </div>
            <p className={ui.listMeta}>
              working {counts.working} · idle {counts.idle} · blocked {counts.blocked} · unknown {counts.unknown}
              {counts.cancelled ? ` · cancelled ${counts.cancelled}` : ""}
            </p>
            {counts.offlineHosts ? (
              <p className={ui.listMeta} data-testid="fleet-offline-host">
                主机离线，任务状态未知
              </p>
            ) : null}
            <ul className={css.members}>
              {fleet.members.map((member) => (
                <li key={member.instanceId} className={css.member} data-testid="fleet-member" data-status={member.status}>
                  {member.status === "cancelled" ? (
                    <span className={ui.listMeta}>×</span>
                  ) : (
                    <StateDot status={member.status} />
                  )}
                  <span>
                    <Link to={`/s/${member.instanceId}`}>{member.title}</Link>
                    <div className={ui.listMeta}>
                      {member.hostLabel} · {member.status}
                      {member.hostOnline ? "" : " · 主机离线"}
                    </div>
                  </span>
                </li>
              ))}
            </ul>
          </section>
        );
      })}
    </div>
  );
}
