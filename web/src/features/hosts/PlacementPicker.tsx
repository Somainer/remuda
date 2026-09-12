import type { Id } from "../../types/wire";
import ui from "../../styles/ui.module.css";
import { hostsMatching, type HostView, type Placement } from "./model";

/** New-session placement control. Import this instead of editing NewSessionPage. */
export function PlacementPicker({
  hosts,
  value,
  onChange,
}: {
  hosts: HostView[];
  value: Placement;
  onChange: (next: Placement) => void;
}) {
  const labels = [...new Set(hosts.flatMap((h) => h.labels))].sort();
  const matched = hostsMatching(hosts, value);
  const selectedLabels = value.kind === "labels" ? value.labels : [];

  return (
    <fieldset data-testid="placement-picker" style={{ border: 0, padding: 0 }}>
      <legend className={ui.listMeta}>Placement</legend>
      <div className={ui.row}>
        {(
          [
            ["host", "指定主机"],
            ["labels", "标签"],
            ["any", "任意"],
          ] as const
        ).map(([kind, label]) => (
          <button
            key={kind}
            type="button"
            className={`${ui.chip} ${value.kind === kind ? ui.chipOn : ""}`}
            data-testid={`placement-kind-${kind}`}
            onClick={() => {
              if (kind === "host") onChange({ kind: "host", hostId: (value.kind === "host" ? value.hostId : hosts[0]?.id) ?? ("" as Id) });
              else if (kind === "labels") onChange({ kind: "labels", labels: selectedLabels });
              else onChange({ kind: "any" });
            }}
          >
            {label}
          </button>
        ))}
      </div>
      {value.kind === "host" ? (
        <label className={ui.field} style={{ marginTop: 8 }}>
          主机
          <select
            className={`${ui.select} ${ui.touchSelect}`}
            data-testid="placement-host"
            value={value.hostId}
            onChange={(e) => onChange({ kind: "host", hostId: e.target.value as Id })}
          >
            {hosts.map((host) => (
              <option key={host.id} value={host.id}>
                {host.label} · {host.state} · {host.transport}
              </option>
            ))}
          </select>
        </label>
      ) : null}
      {value.kind === "labels" ? (
        <div className={ui.row} style={{ marginTop: 8 }}>
          {labels.map((label) => {
            const on = selectedLabels.includes(label);
            return (
              <button
                key={label}
                type="button"
                className={`${ui.chip} ${on ? ui.chipOn : ""}`}
                data-testid={`placement-label-${label}`}
                onClick={() => {
                  const next = on ? selectedLabels.filter((item) => item !== label) : selectedLabels.concat(label);
                  onChange({ kind: "labels", labels: next });
                }}
              >
                {label}
              </button>
            );
          })}
        </div>
      ) : null}
      {value.kind === "any" ? <p className={ui.listMeta}>Hub 按能力与负载选择，不可满足时返回明确错误。</p> : null}
      {matched.length === 0 ? (
        <p className={ui.listMeta} data-testid="placement-unsatisfied" style={{ color: "var(--dust)" }}>
          没有满足 placement 的在线主机，不会静默降级。
        </p>
      ) : (
        <p className={ui.listMeta} data-testid="placement-match-count">
          {matched.length} 台可调度
        </p>
      )}
    </fieldset>
  );
}
