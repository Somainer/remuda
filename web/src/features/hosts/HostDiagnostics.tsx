import { useEffect, useState } from "react";
import { fetchHostDoctor, type HostDoctorReport } from "../../lib/api";
import type { Id } from "../../types/wire";
import css from "./hosts.module.css";

export function HostDiagnostics({ hostId, online }: { hostId: Id; online: boolean }) {
  return <HostDiagnosticsRequest key={`${hostId}:${online}`} hostId={hostId} online={online} />;
}

function HostDiagnosticsRequest({ hostId, online }: { hostId: Id; online: boolean }) {
  const [result, setResult] = useState<{ refresh: number; report?: HostDoctorReport; error?: string } | null>(null);
  const [refresh, setRefresh] = useState(0);
  const current = result?.refresh === refresh ? result : null;
  const checking = online && current === null;
  const report = current?.report;
  const error = current?.error;

  useEffect(() => {
    let active = true;
    if (online) {
      void fetchHostDoctor(hostId)
        .then((report) => { if (active) setResult({ refresh, report }); })
        .catch((cause: unknown) => { if (active) setResult({ refresh, error: cause instanceof Error ? cause.message : "主机诊断不可用" }); });
    }
    return () => { active = false; };
  }, [hostId, online, refresh]);

  const findings = report?.checks.filter((check) => check.status !== "ok") ?? [];
  return <section data-testid="host-diagnostics" aria-label="主机诊断">
    <div className={css.sectionLabel}>主机诊断 · 工作目录访问权限</div>
    <button type="button" className={css.add} disabled={!online || checking} onClick={() => setRefresh((value) => value + 1)}>
      {checking ? "检查中…" : "重新检查"}
    </button>
    {!online ? <p role="status">主机离线，无法检查当前权限</p> : null}
    {error ? <p role="alert" className={css.sshError}>{error}</p> : null}
    {report && findings.length === 0 ? <p role="status">主机检查通过</p> : null}
    {findings.map((check) => <p key={check.name} role={check.status === "blocker" ? "alert" : "status"} className={css.sshError}>
      {check.message}
    </p>)}
  </section>;
}
