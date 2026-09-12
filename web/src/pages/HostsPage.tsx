import { useState } from "react";
import { Link, useParams } from "react-router-dom";
import { AddHostForm, hostRegistry, useHostViews, type HostView } from "../features/hosts";
import { useHub } from "../lib/store";
import ui from "../styles/ui.module.css";
import css from "../features/hosts/hosts.module.css";

export function HostsPage() {
  const hub = useHub();
  const hosts = useHostViews(hub.hosts, hub.instances);
  const [adding, setAdding] = useState(false);
  const online = hosts.filter((h) => h.online).length;

  return (
    <div className={css.board} data-testid="hosts-page">
      <header className={css.head}>
        <h1 className={css.title}>主机</h1>
        <div className={css.count}>
          {hosts.length} 台 · {online} 在线
        </div>
        <Link to="/fleet" className={css.count}>
          Fleet
        </Link>
        <div style={{ flex: 1 }} />
        <button type="button" className={css.add} data-testid="hosts-add" onClick={() => setAdding(true)}>
          添加
        </button>
      </header>
      {hosts.map((host) => (
        <Link
          key={host.id}
          to={`/hosts/${host.id}`}
          className={css.row}
          data-testid="host-row"
          data-transport={host.transport}
          data-online={host.online ? "1" : "0"}
          data-label={host.label}
        >
          <span className={`${css.dot} ${host.online ? css.dotOn : css.dotOff}`} aria-label={host.online ? "在线" : "离线"} />
          <span className={css.identity}>
            <span className={`${css.name} ${host.online ? "" : css.nameOff}`}>{host.label}</span>
            <span className={css.mobileMeta}>
              {host.online
                ? `在线${host.rttMs != null ? ` ${host.rttMs}ms` : ""}`
                : `离线${host.lastSeenAt ? ` · 最后心跳 ${host.lastSeenAt.slice(11, 16)}` : ""}`}
              {host.cli[0] ? ` · ${host.cli[0].kind}${host.cli[0].version ? ` ${host.cli[0].version}` : ""}` : ""}
              {` · 会话 ${host.instanceCount}`}
            </span>
          </span>
          <span className={`${css.cell} ${css.cellRtt}`}>
            {host.online ? `在线${host.rttMs != null ? ` ${host.rttMs}ms` : ""}` : "离线 —"}
          </span>
          <span className={`${css.cell} ${css.cellCli}`}>{host.cli.map((c) => c.kind).join(" · ") || "—"}</span>
          <span className={`${css.cell} ${css.cellTransport}`}>
            {host.transport}
            {!host.online && host.lastSeenAt ? ` · 最后心跳 ${host.lastSeenAt.slice(11, 16)}` : ""}
          </span>
          <span className={`${css.cell} ${css.cellSessions}`}>会话 {host.instanceCount}</span>
          <span className={css.chevron} aria-hidden>
            ›
          </span>
        </Link>
      ))}
      <AddHostForm open={adding} onClose={() => setAdding(false)} />
    </div>
  );
}

export function HostDetailPage() {
  const { hostId = "" } = useParams();
  const hub = useHub();
  const hosts = useHostViews(hub.hosts, hub.instances);
  const host = hosts.find((h) => h.id === hostId);
  if (!host) return <p style={{ padding: 16 }}>主机不存在</p>;
  const workspaces = hub.workspaces.filter((w) => w.hostId === host.id);
  return <HostDetail host={host} workspaces={workspaces.map((w) => ({ label: w.label, path: w.rootPath }))} />;
}

function HostDetail({ host, workspaces }: { host: HostView; workspaces: { label: string; path: string }[] }) {
  const [label, setLabel] = useState(host.label);
  const [labelDraft, setLabelDraft] = useState(host.labels.join(", "));
  const [maxInstances, setMaxInstances] = useState(String(host.maxInstances));

  return (
    <div className={css.board} data-testid="host-detail">
      <div className={css.detail}>
        <div className={css.detailHead}>
          <Link to="/hosts" className={css.back} aria-label="返回主机列表">
            ←
          </Link>
          <h1 className={css.detailTitle}>{host.label}</h1>
          <span className={css.detailMeta}>
            {host.id.slice(0, 10)} · Node Agent {host.agentVersion ?? "—"}
          </span>
          {host.online ? (
            <Link to={`/sessions/new?host=${host.id}`} className={css.newSession}>
              新会话
            </Link>
          ) : (
            <button type="button" className={css.newSession} disabled>
              新会话
            </button>
          )}
        </div>
        <div className={css.metrics}>
          <div className={css.metric}>
            <div className={css.metricLabel}>传输</div>
            <div className={css.metricValue}>{host.transport}</div>
          </div>
          <div className={css.metric}>
            <div className={css.metricLabel}>心跳</div>
            <div className={css.metricValue}>{host.online ? `${host.rttMs ?? "—"}ms` : "离线"}</div>
          </div>
          {host.resources ? (
            <>
              {host.resources.cpuPct != null ? (
                <div className={css.metric}>
                  <div className={css.metricLabel}>cpu</div>
                  <div className={css.metricValue}>{host.resources.cpuPct}%</div>
                </div>
              ) : null}
              {host.resources.memPct != null ? (
                <div className={css.metric}>
                  <div className={css.metricLabel}>mem</div>
                  <div className={css.metricValue}>{host.resources.memPct}%</div>
                </div>
              ) : null}
            </>
          ) : null}
        </div>
        <div>
          <div className={css.sectionLabel}>CLI · 按本机盘点，绝对路径 + 版本</div>
          <div className={css.cliTable}>
            {host.cli.length === 0 ? <div className={css.cliRow}>尚无盘点</div> : null}
            {host.cli.map((cli) => (
              <div key={`${cli.kind}:${cli.path}`} className={css.cliRow} data-testid="host-cli">
                <span className={css.cliKind}>
                  {cli.kind}
                  <span className={css.cliVerInline}>{cli.version ? ` ${cli.version}` : ""}</span>
                </span>
                <span className={css.cliPath}>{cli.path}</span>
                <span className={css.cliVer}>{cli.version}</span>
                <span className={css.cliAuth}>
                  <span className={cli.auth === "logged_in" ? css.dotAuth : css.dotUnknown} />
                  {cli.auth}
                </span>
              </div>
            ))}
          </div>
        </div>
        <div>
          <div className={css.sectionLabel}>Workspace · 最近 cwd</div>
          <div className={css.chips}>
            {workspaces.length ? (
              workspaces.map((row) => (
                <span key={row.path} className={css.chip}>
                  {row.label}
                  <span className={css.chipPath}>{row.path}</span>
                </span>
              ))
            ) : (
              <span className={css.chip}>最近 cwd —</span>
            )}
          </div>
        </div>
        <div className={css.fields}>
          <label className={ui.field}>
            显示名
            <input className={ui.input} value={label} data-testid="host-label-edit" onChange={(e) => setLabel(e.target.value)} />
          </label>
          <label className={ui.field}>
            标签（逗号分隔）
            <input className={ui.input} value={labelDraft} data-testid="host-labels-edit" onChange={(e) => setLabelDraft(e.target.value)} />
          </label>
          <label className={ui.field}>
            maxInstances
            <input
              className={ui.input}
              type="number"
              min={1}
              value={maxInstances}
              data-testid="host-max-instances"
              onChange={(e) => setMaxInstances(e.target.value)}
            />
          </label>
          <button
            type="button"
            className={css.add}
            data-testid="host-save-meta"
            onClick={() => {
              hostRegistry.patch(host.id, {
                label: label.trim() || host.label,
                labels: labelDraft
                  .split(",")
                  .map((item) => item.trim())
                  .filter(Boolean),
                maxInstances: Math.max(1, Number(maxInstances) || host.maxInstances),
              });
            }}
          >
            保存
          </button>
        </div>
        <div className={css.foot}>离线主机仍可看历史会话（journal 在 Hub），但不能 create / send</div>
      </div>
    </div>
  );
}
