import { useState } from "react";
import { Button } from "../../components/Button";
import { Modal } from "../../components/Modal";
import ui from "../../styles/ui.module.css";
import { SSH_ALIASES } from "./fixtures";
import css from "./hosts.module.css";
import { hostRegistry } from "./registry";
import { bootstrapPlan, probeSshAlias, runBootstrap, type BootstrapStep, type ProbeResult } from "./ssh";

export function AddHostForm({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [alias, setAlias] = useState(SSH_ALIASES[0]?.alias ?? "");
  const [probe, setProbe] = useState<ProbeResult | null>(null);
  const [steps, setSteps] = useState<BootstrapStep[]>(bootstrapPlan(alias));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  return (
    <Modal open={open} onClose={onClose}>
      <h2 style={{ marginTop: 0, fontSize: 16 }}>添加主机</h2>
      <p className={css.meta}>从 ~/.ssh/config 别名选择，走 ssh-stdio（proposal §4.6）。无 tailcat。</p>
      <label className={ui.field}>
        SSH 别名
        <select
          className={`${ui.select} ${ui.touchSelect}`}
          data-testid="add-host-alias"
          value={alias}
          onChange={(e) => {
            setAlias(e.target.value);
            setProbe(null);
            setSteps(bootstrapPlan(e.target.value));
            setError(null);
          }}
        >
          {SSH_ALIASES.map((row) => (
            <option key={row.alias} value={row.alias}>
              {row.alias}
            </option>
          ))}
        </select>
      </label>
      <div className={ui.row} style={{ marginTop: 12 }}>
        <Button
          disabled={busy}
          data-testid="add-host-probe"
          onClick={() => {
            setBusy(true);
            setError(null);
            void probeSshAlias(alias)
              .then((result) => setProbe(result))
              .finally(() => setBusy(false));
          }}
        >
          Probe
        </Button>
        <Button
          variant="primary"
          disabled={busy || !probe?.ok}
          data-testid="add-host-bootstrap"
          onClick={() => {
            setBusy(true);
            setError(null);
            void runBootstrap(alias, setSteps)
              .then((host) => {
                hostRegistry.enroll(host);
                onClose();
              })
              .catch((err: unknown) => setError(err instanceof Error ? err.message : "bootstrap failed"))
              .finally(() => setBusy(false));
          }}
        >
          Bootstrap
        </Button>
      </div>
      {probe && !probe.ok ? (
        <p data-testid="add-host-probe-error" style={{ color: "var(--dust)" }}>
          {probe.error}
        </p>
      ) : null}
      {probe?.ok ? (
        <p className={css.meta} data-testid="add-host-probe-ok">
          {probe.hostname} · {probe.rttMs}ms · {probe.cli.map((c) => `${c.kind} ${c.version}`).join(" · ")}
        </p>
      ) : null}
      <div className={css.progress} data-testid="add-host-progress">
        {steps.map((step) => (
          <div key={step.id} className={step.state === "pending" ? css.step : css.stepOn}>
            {step.state === "done" ? "✓" : step.state === "running" ? "…" : "○"} {step.label}
          </div>
        ))}
      </div>
      {error ? <p style={{ color: "var(--dust)" }}>{error}</p> : null}
      <div className={ui.row} style={{ marginTop: 12, justifyContent: "flex-end" }}>
        <Button onClick={onClose}>取消</Button>
      </div>
    </Modal>
  );
}
