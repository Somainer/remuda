import { useState } from "react";
import { Button } from "../../components/Button";
import { api, type FleetBroadcastResult } from "../../lib/api";
import type { Instance } from "../../types/instance";
import type { Id } from "../../types/wire";
import ui from "../../styles/ui.module.css";
import type { HostView } from "../hosts/model";
import {
  BROADCAST_KEYS,
  DELIVERY_LABEL,
  buildBroadcastBody,
  orderResults,
  provisionalState,
  resolveEntry,
  summarize,
  type BroadcastForm,
  type DeliveryState,
} from "./broadcast";
import css from "./fleet.module.css";

const EMPTY: BroadcastForm = {
  mode: "prompt",
  text: "",
  key: "enter",
  filter: { hostId: "", kind: "" },
};

type RowState = {
  state: DeliveryState;
  reason?: string;
  /** True while the authoritative command read is still being polled. */
  resolving?: boolean;
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
  const [rowStates, setRowStates] = useState<Record<string, RowState>>({});

  const kinds = [...new Set(instances.map((instance) => instance.kind))].sort();

  /**
   * Read the authoritative settlement for each accepted row. The fleet
   * response never carries `settlement.outcome`, so green confirmation is
   * only allowed after the command resource reports completed. Reads are
   * bounded: one immediate read per row plus follow-ups inside resolveEntry
   * for up to ~10s; still-open rows keep their neutral provisional label.
   */
  async function resolveRows(value: FleetBroadcastResult) {
    const entries = value.results ?? [];
    const initial: Record<string, RowState> = {};
    for (const entry of entries) {
      const key = entry.commandId ?? entry.instanceId;
      if (key) initial[key] = { state: provisionalState(entry) };
    }
    setRowStates(initial);

    await Promise.all(
      entries.map(async (entry) => {
        const key = entry.commandId ?? entry.instanceId;
        if (!key || !entry.ok || entry.replayed || !entry.commandId || !entry.instanceId) return;
        setRowStates((prev) => ({ ...prev, [key]: { state: provisionalState(entry), resolving: true } }));
        const settled = await resolveEntry(entry, (instanceId, commandId) =>
          api.instanceCommandStatus(instanceId as Id, commandId as Id),
        );
        setRowStates((prev) => ({ ...prev, [key]: settled }));
      }),
    );
  }

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
      setRowStates({});
      if (form.mode === "prompt") setForm((prev) => ({ ...prev, text: "" }));
      // Do not block the send button on the bounded settlement follow-up:
      // rows update in place as authoritative outcomes arrive.
      void resolveRows(value);
    } catch (err) {
      setError(err instanceof Error ? err.message : "广播失败");
    } finally {
      setBusy(false);
    }
  }

  return (
    <section className={css.section} data-testid="fleet-broadcast">
      <div className={css.sectionHead}>
        <h2 className={css.sectionTitle}>群发</h2>
        <p className={css.sectionSub}>
          POST /v1/fleet/broadcast · 对筛选到的运行中 Instance 发送 prompt 或按键
        </p>
      </div>

      <div className={css.formRow}>
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
        <label className={ui.field}>
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
        <label className={ui.field}>
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
        <p data-testid="broadcast-error" className={css.error}>
          {error}
        </p>
      ) : null}

      <div className={css.actions}>
        <Button variant="primary" data-testid="broadcast-send" disabled={busy} onClick={submit}>
          {busy ? "发送中…" : "确认群发"}
        </Button>
      </div>

      {result ? (
        <div className={css.resultBlock}>
          <p className={css.sectionSub} data-testid="broadcast-summary">
            {summarize(result)}
          </p>
          <ul className={css.members} data-testid="broadcast-results">
            {orderResults(result).map((entry) => {
              const key = entry.commandId ?? entry.instanceId;
              const row = (key ? rowStates[key] : undefined) ?? { state: provisionalState(entry) };
              const state: DeliveryState = row.state;
              const markClass = `mark${state[0].toUpperCase()}${state.slice(1)}`;
              return (
                <li
                  key={key}
                  className={css.member}
                  data-testid="broadcast-result"
                  data-ok={String(entry.ok ?? false)}
                  data-delivery={state}
                  data-resolving={row.resolving ? "1" : undefined}
                >
                  <span className={`${css.mark} ${css[markClass]}`}>
                    {DELIVERY_LABEL[state]}
                    {row.resolving ? "…" : ""}
                  </span>
                  <span className={css.memberBody}>
                    {entry.instanceId}
                    <div className={css.memberMeta}>
                      {entry.kind} · {entry.hostId}
                      {entry.replayed ? " · 重放" : ""}
                      {row.reason
                        ? ` · ${row.reason}`
                        : entry.error
                          ? ` · ${entry.error}`
                          : entry.state
                            ? ` · ${entry.state}`
                            : ""}
                    </div>
                  </span>
                </li>
              );
            })}
          </ul>
        </div>
      ) : null}
    </section>
  );
}
