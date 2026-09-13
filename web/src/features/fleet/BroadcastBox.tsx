import { useState } from "react";
import { Button } from "../../components/Button";
import { api, type FleetBroadcastResult } from "../../lib/api";
import type { Instance } from "../../types/instance";
import ui from "../../styles/ui.module.css";
import type { HostView } from "../hosts/model";
import {
  BROADCAST_KEYS,
  buildBroadcastBody,
  orderResults,
  summarize,
  type BroadcastForm,
} from "./broadcast";
import css from "./fleet.module.css";

const EMPTY: BroadcastForm = {
  mode: "prompt",
  text: "",
  key: "enter",
  filter: { hostId: "", kind: "" },
};

/**
 * Broadcast one prompt or key to every running instance matching the filter
 * (`POST /v1/fleet/broadcast`). Results list every instance, failures first.
 */
export function BroadcastBox({ hosts, instances }: { hosts: HostView[]; instances: Instance[] }) {
  const [form, setForm] = useState<BroadcastForm>(EMPTY);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<FleetBroadcastResult | null>(null);

  const kinds = [...new Set(instances.map((instance) => instance.kind))].sort();

  async function submit() {
    const built = buildBroadcastBody(form);
    if ("error" in built) {
      setError(built.error);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const value = await api.fleetBroadcast({ ...built.body, confirm: true });
      setResult(value);
      if (form.mode === "prompt") setForm((prev) => ({ ...prev, text: "" }));
    } catch (err) {
      setError(err instanceof Error ? err.message : "广播失败");
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className={ui.card} style={{ marginBottom: 16 }} data-testid="fleet-broadcast">
      <strong>群发</strong>
      <p className={ui.listMeta}>POST /v1/fleet/broadcast · 对筛选到的运行中 Instance 发送 prompt 或按键</p>

      <div className={ui.row} style={{ gap: 8, marginTop: 8 }}>
        <label className={ui.field}>
          主机
          <select
            className={ui.select}
            data-testid="broadcast-host"
            value={form.filter.hostId}
            onChange={(e) => setForm({ ...form, filter: { ...form.filter, hostId: e.target.value } })}
          >
            <option value="">全部主机</option>
            {hosts.map((host) => (
              <option key={host.id} value={host.id}>
                {host.label}
              </option>
            ))}
          </select>
        </label>
        <label className={ui.field}>
          kind
          <select
            className={ui.select}
            data-testid="broadcast-kind"
            value={form.filter.kind}
            onChange={(e) => setForm({ ...form, filter: { ...form.filter, kind: e.target.value } })}
          >
            <option value="">全部 kind</option>
            {kinds.map((kind) => (
              <option key={kind} value={kind}>
                {kind}
              </option>
            ))}
          </select>
        </label>
        <label className={ui.field}>
          内容
          <select
            className={ui.select}
            data-testid="broadcast-mode"
            value={form.mode}
            onChange={(e) => setForm({ ...form, mode: e.target.value as BroadcastForm["mode"] })}
          >
            <option value="prompt">prompt</option>
            <option value="key">按键</option>
          </select>
        </label>
      </div>

      {form.mode === "prompt" ? (
        <label className={ui.field} style={{ marginTop: 8 }}>
          <span className={ui.listMeta}>群发内容</span>
          <textarea
            className={ui.textarea}
            data-testid="broadcast-text"
            rows={3}
            value={form.text}
            placeholder="PAUSE git commits ~5 分钟"
            onChange={(e) => setForm({ ...form, text: e.target.value })}
          />
        </label>
      ) : (
        <label className={ui.field} style={{ marginTop: 8 }}>
          <span className={ui.listMeta}>按键</span>
          <select
            className={ui.select}
            data-testid="broadcast-key"
            value={form.key}
            onChange={(e) => setForm({ ...form, key: e.target.value as BroadcastForm["key"] })}
          >
            {BROADCAST_KEYS.map((key) => (
              <option key={key} value={key}>
                {key}
              </option>
            ))}
          </select>
        </label>
      )}

      {error ? (
        <p data-testid="broadcast-error" style={{ color: "var(--dust)" }}>
          {error}
        </p>
      ) : null}

      <div className={ui.row} style={{ marginTop: 8, justifyContent: "flex-end" }}>
        <Button variant="primary" data-testid="broadcast-send" disabled={busy} onClick={submit}>
          {busy ? "发送中…" : "确认群发"}
        </Button>
      </div>

      {result ? (
        <div style={{ marginTop: 8 }}>
          <p className={ui.listMeta} data-testid="broadcast-summary">
            {summarize(result)}
          </p>
          <ul className={css.members} data-testid="broadcast-results">
            {orderResults(result).map((entry) => (
              <li
                key={entry.commandId ?? entry.instanceId}
                className={css.member}
                data-testid="broadcast-result"
                data-ok={String(entry.ok ?? false)}
              >
                <span className={ui.listMeta}>{entry.ok ? "✓" : "×"}</span>
                <span>
                  {entry.instanceId}
                  <div className={ui.listMeta}>
                    {entry.kind} · {entry.hostId}
                    {entry.replayed ? " · 重放" : ""}
                    {entry.error ? ` · ${entry.error}` : entry.state ? ` · ${entry.state}` : ""}
                  </div>
                </span>
              </li>
            ))}
          </ul>
        </div>
      ) : null}
    </section>
  );
}
