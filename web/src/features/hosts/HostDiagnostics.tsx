import { useEffect, useState } from "react";
import { fetchHostDoctor, type HostDoctorReport } from "../../lib/api";
import type { Id } from "../../types/wire";
import { COMPUTER_USE_KIND, computerUseState, type HostCli } from "./model";
import css from "./hosts.module.css";

export function HostDiagnostics({ hostId, online, cli }: { hostId: Id; online: boolean; cli?: HostCli[] }) {
  return <HostDiagnosticsRequest key={`${hostId}:${online}`} hostId={hostId} online={online} cli={cli} />;
}

/**
 * The `computer-use` row, but **only when the host did not report it at all**.
 *
 * ui-spec §2.6: the capability is an ordinary CLI row, and the host detail's
 * CLI table already draws the two reported states (`installed → 已安装`,
 * `installed: false → 未安装`). The one state that table *cannot* express is
 * absence of the row itself — `installedCli` and `absentCli` both yield
 * nothing there — so an older Node would silently show no capability line at
 * all. That is the only case this renders; drawing the reported states here
 * too would show the same fact twice on one page.
 */
function ComputerUseRow({ cli }: { cli?: HostCli[] }) {
  const state = computerUseState(cli);
  if (state.reported) return null;
  return <div className={css.cliRow} data-testid="computer-use-row" data-state="unreported">
    <span className={css.cliKind}>{COMPUTER_USE_KIND}</span>
    <span className={css.cliVer}>未上报</span>
    <span className={css.cliVer} data-testid="computer-use-detail">
      该 Node 未回报此行；不代表本机不支持
    </span>
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
