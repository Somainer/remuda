import { useEffect, useState } from "react";
import { fetchHostDoctor, type HostDoctorReport } from "../../lib/api";
import type { Id } from "../../types/wire";
import { COMPUTER_USE_KIND, computerUseState, type HostCli } from "./model";
import css from "./hosts.module.css";

export function HostDiagnostics({ hostId, online, cli }: { hostId: Id; online: boolean; cli?: HostCli[] }) {
  return <HostDiagnosticsRequest key={`${hostId}:${online}`} hostId={hostId} online={online} cli={cli} />;
}

/**
 * The `computer-use` row, in the three states the Node can actually report.
 *
 * D-045 §3.4 / ui-spec §2.6: this is an ordinary CLI row, but the state that
 * has no precedent is "not reported" — an older Node omits the row entirely,
 * and that must never be drawn as "this host cannot". So the copy is explicit
 * and the two negatives stay visually distinct.
 */
function ComputerUseRow({ cli }: { cli?: HostCli[] }) {
  const state = computerUseState(cli);
  const label = !state.reported ? "未上报" : state.installed ? "已安装" : "未安装";
  const detail = !state.reported
    ? "该 Node 未回报此行；不代表本机不支持"
    : state.installed
      ? [state.version, state.path].filter(Boolean).join(" · ")
      : "Codex Computer Use 客户端不在该 Node 的 CODEX_HOME 下";
  return <div className={css.cliRow} data-testid="computer-use-row" data-state={
    !state.reported ? "unreported" : state.installed ? "installed" : "absent"
  }>
    <span className={css.cliKind}>{COMPUTER_USE_KIND}</span>
    <span className={css.cliVer}>{label}</span>
    <span className={css.cliVer} data-testid="computer-use-detail">{detail}</span>
  </div>;
}

function HostDiagnosticsRequest({ hostId, online, cli }: { hostId: Id; online: boolean; cli?: HostCli[] }) {
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
    <ComputerUseRow cli={cli} />
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
